//! ACP v1 agents behind cookie's v2 client (`docs/site/specs/acp-v1-agents.md`).
//!
//! Cookie speaks ACP v2 only. An agent that answers `initialize` with
//! protocol version 1, such as Antigravity's `agy acp`, is spoken to through
//! [`V1Transport`]: it sits between the engine's peer and the agent's frames
//! and turns each side's messages into the other's version. Every agent goes
//! through it; once the agent answers v2 it passes frames on untouched.
//!
//! What v2 has and v1 does not is made up here:
//!
//! - **Turns by state.** `session/prompt` is answered at once with a
//!   message ID, echoed as a `user_message`, and reported `running`; the
//!   agent's v1 answer becomes the `idle` that ends the work. A prompt sent
//!   while the agent works steers it: the work is `idle` only once every
//!   prompt sent is answered.
//! - **Work the agent starts itself** (a v1 agent reports it between
//!   prompts) is reported `running` at its first update, and `idle` once
//!   the agent has been quiet for [`WAKE_QUIET`] with no request waiting
//!   on the user ([`WAKE_QUIET_OPEN_CALL`] while one of its calls runs):
//!   v1 says nothing when such work ends.
//! - **Message IDs** for streamed chunks, one per run of chunks of a kind.
//! - **Shapes:** `tool_call` becomes a `tool_call_update`, `plan` a
//!   `plan_update`, a diff's old and new text a git patch, a permission
//!   request gets its `title` and `subject`, the `initialize` answer its v2
//!   names.
//! - **Tool names:** a call of an MCP server cookie handed the session,
//!   named `mcp_<server>_<tool>` (Antigravity), is named as Claude Code
//!   names it, `mcp__<server>__<tool>`, which cookie reads.
//! - **Subagents** (the subagents RFD, which v2 made every client's): a
//!   `subagent_update`'s state is also reported on the subagent's own
//!   session, as v2 does, and a subagent's updates never count as work the
//!   agent started itself.
//! - **Command output** that Claude Code's and Codex's adapters send as
//!   `_meta` keys of a tool call becomes v2's agent terminals.
//! - **The appended system prompt** goes as Antigravity's
//!   `_meta.antigravity.systemPrompt.append` to an agent that offers that.
//!
//! v1's modes and models are left out: the v1 agents cookie knows offer the
//! same as config options, which cookie uses.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use async_trait::async_trait;
use cookie_agent_protocol::{MessageFrame, Transport, TransportError};
use serde_json::{Map, Value, json};
use tokio::time::Instant;

/// How long work a v1 agent started on its own may go without an update
/// before it counts as ended.
const WAKE_QUIET: Duration = Duration::from_secs(10);

/// The same while one of the agent's tool calls is still running.
const WAKE_QUIET_OPEN_CALL: Duration = Duration::from_secs(120);

/// The prefix of the request IDs the translator gives the prompts it sends
/// the agent: their answers are its own, never the peer's.
const PROMPT_ID_PREFIX: &str = "cookie-v1-prompt-";

/// The engine's peer's frames to and from an agent, translated when the
/// agent speaks ACP v1.
pub(crate) struct V1Transport<T> {
    inner: T,
    translator: Translator,
    /// Frames for the peer the translator made, ahead of the agent's next.
    to_client: VecDeque<Value>,
}

impl<T> V1Transport<T> {
    pub(crate) fn new(inner: T) -> Self {
        Self {
            inner,
            translator: Translator::default(),
            to_client: VecDeque::new(),
        }
    }
}

fn frame_value(frame: MessageFrame) -> Result<Value, TransportError> {
    Ok(match frame {
        MessageFrame::Value(value) => value,
        MessageFrame::Text(text) => serde_json::from_str(&text)?,
        MessageFrame::Bytes(bytes) => serde_json::from_slice(&bytes)?,
    })
}

#[async_trait]
impl<T: Transport> Transport for V1Transport<T> {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        if self.translator.mode == Mode::V2 {
            return self.inner.send(frame).await;
        }
        let message = frame_value(frame)?;
        let out = self.translator.outgoing(message, Instant::now());
        self.to_client.extend(out.to_client);
        for message in out.to_agent {
            self.inner.send(MessageFrame::Value(message)).await?;
        }
        Ok(())
    }

    /// Cancel-safe, as the peer needs: what an awaited frame turns into is
    /// queued before the next await.
    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        loop {
            if let Some(message) = self.to_client.pop_front() {
                return Ok(Some(MessageFrame::Value(message)));
            }
            if self.translator.mode == Mode::V2 {
                return self.inner.recv().await;
            }
            let deadline = self.translator.deadline();
            tokio::select! {
                frame = self.inner.recv() => match frame? {
                    None => return Ok(None),
                    Some(frame) => {
                        let message = frame_value(frame)?;
                        let out = self.translator.incoming(message, Instant::now());
                        self.to_client.extend(out);
                    }
                },
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let out = self.translator.timer(Instant::now());
                    self.to_client.extend(out);
                }
            }
        }
    }

    async fn close(&mut self) {
        self.inner.close().await;
    }
}

/// Which version the agent speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    /// Not known before its `initialize` answer.
    #[default]
    Unknown,
    V1,
    V2,
}

/// What one translated message becomes.
#[derive(Debug, Default)]
struct Out {
    to_agent: Vec<Value>,
    to_client: Vec<Value>,
}

/// A request of cookie's whose answer the translator reads.
#[derive(Debug)]
enum Pending {
    Initialize,
    /// `session/new`, `resume`, or `fork`: the MCP servers it named
    /// for the session it opens; `session` for one that names its own.
    Open {
        session: Option<String>,
        servers: Vec<String>,
    },
    /// A prompt the translator sent under its own ID.
    Prompt {
        session: String,
    },
}

/// What the translator knows of one ACP session.
#[derive(Debug, Default)]
struct Session {
    /// The MCP servers cookie handed it.
    servers: Vec<String>,
    /// The session that announced it, for one of the agent's subagents
    /// (`subagent_update`): its state comes in those announcements.
    parent: Option<String>,
    /// Prompts sent and not answered by the agent yet.
    in_flight: usize,
    /// Reported `running`, with no `idle` since.
    working: bool,
    /// When work with no prompt in flight last showed (a wake).
    last_seen: Option<Instant>,
    /// The agent's tool calls not ended yet.
    open_calls: HashSet<String>,
    /// The stop of the last prompt the agent answered, and its error.
    stop_reason: Option<String>,
    error: Option<String>,
    /// The usage the agent answered the work's prompts with, summed.
    usage: Option<Map<String, Value>>,
    /// The kind and ID of the message whose chunks stream now.
    message: Option<(String, String)>,
}

#[derive(Debug, Default)]
struct Translator {
    mode: Mode,
    /// The agent takes Antigravity's `_meta.antigravity.systemPrompt.append`.
    meta_system_prompt: bool,
    /// The agent takes v1's `systemPrompt`.
    system_prompt: bool,
    /// Cookie's requests whose answers are read, by their ID's JSON.
    pending: HashMap<String, Pending>,
    /// The agent's requests waiting on cookie, by ID's JSON, with their
    /// session.
    agent_requests: HashMap<String, String>,
    sessions: HashMap<String, Session>,
    next_id: u64,
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn session_update(session: &str, update: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": session, "update": update},
    })
}

/// Update kinds that show the agent working, rather than describing the
/// session.
fn is_work(kind: &str) -> bool {
    matches!(
        kind,
        "agent_message_chunk" | "agent_thought_chunk" | "tool_call" | "tool_call_update" | "plan"
    )
}

impl Translator {
    fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    /// What a message of cookie's becomes.
    fn outgoing(&mut self, mut message: Value, now: Instant) -> Out {
        let mut out = Out::default();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        match (method.as_deref(), id) {
            (Some("initialize"), Some(id)) => {
                self.pending.insert(id_key(&id), Pending::Initialize);
            }
            _ if self.mode != Mode::V1 => {}
            // An answer to one of the agent's requests.
            (None, Some(id)) => {
                if let Some(session) = self.agent_requests.remove(&id_key(&id))
                    && let Some(state) = self.sessions.get_mut(&session)
                    && state.working
                    && state.in_flight == 0
                {
                    state.last_seen = Some(now);
                }
            }
            (Some("session/prompt"), Some(id)) => return self.prompt(message, id),
            (Some("session/cancel"), None) => {
                if let Some(session) = param_str(&message, "sessionId")
                    && let Some(state) = self.sessions.get_mut(&session)
                    && state.working
                    && state.in_flight == 0
                {
                    out.to_client.push(self.idle(&session, Some("cancelled")));
                }
            }
            (Some("session/close"), _) => {
                if let Some(session) = param_str(&message, "sessionId") {
                    self.sessions.remove(&session);
                }
            }
            (Some(method @ ("session/new" | "session/resume" | "session/fork")), Some(id)) => {
                let servers = self.open_params(&mut message);
                let session = (method == "session/resume")
                    .then(|| param_str(&message, "sessionId"))
                    .flatten();
                self.pending
                    .insert(id_key(&id), Pending::Open { session, servers });
            }
            (Some("session/set_config_option"), Some(_)) => {
                if let Some(params) = message.get_mut("params").and_then(Value::as_object_mut)
                    && params.get("type").and_then(Value::as_str) == Some("id")
                {
                    params.remove("type");
                }
            }
            _ => {}
        }
        out.to_agent.push(message);
        out
    }

    /// The params of a request that opens a session, in v1's shape; the
    /// names of the MCP servers it hands the agent.
    fn open_params(&self, message: &mut Value) -> Vec<String> {
        let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else {
            return Vec::new();
        };
        let mut servers = Vec::new();
        if let Some(list) = params.get_mut("mcpServers").and_then(Value::as_array_mut) {
            for server in list {
                if let Some(name) = server.get("name").and_then(Value::as_str) {
                    servers.push(name.to_owned());
                }
                // A v1 stdio server has no `type`.
                if let Some(server) = server.as_object_mut()
                    && server.get("type").and_then(Value::as_str) == Some("stdio")
                {
                    server.remove("type");
                }
            }
        }
        if let Some(prompt) = params.remove("systemPrompt") {
            if self.meta_system_prompt {
                let text = prompt
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(super::wire::content_text)
                    .filter(|text| !text.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if !text.is_empty() {
                    let meta = params.entry("_meta").or_insert_with(|| json!({}));
                    if !meta.is_object() {
                        *meta = json!({});
                    }
                    meta["antigravity"]["systemPrompt"]["append"] = Value::from(text);
                }
            } else if self.system_prompt {
                params.insert("systemPrompt".to_owned(), prompt);
            }
        }
        servers
    }

    /// Cookie's `session/prompt`: sent under the translator's own ID, and
    /// answered at once as taken in.
    fn prompt(&mut self, mut message: Value, id: Value) -> Out {
        let mut out = Out::default();
        let Some(session) = param_str(&message, "sessionId") else {
            out.to_agent.push(message);
            return out;
        };
        let own = self.fresh_id(PROMPT_ID_PREFIX);
        let message_id = self.fresh_id("cookie-v1-user-");
        message["id"] = Value::from(own.clone());
        let prompt = message
            .pointer("/params/prompt")
            .cloned()
            .unwrap_or_else(|| json!([]));
        self.pending.insert(
            id_key(&Value::from(own)),
            Pending::Prompt {
                session: session.clone(),
            },
        );
        let state = self.sessions.entry(session.clone()).or_default();
        state.in_flight += 1;
        state.last_seen = None;
        state.message = None;
        out.to_agent.push(message);
        out.to_client.push(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"messageId": message_id},
        }));
        if !state.working {
            state.working = true;
            state.stop_reason = None;
            state.error = None;
            state.usage = None;
            out.to_client.push(session_update(
                &session,
                json!({"sessionUpdate": "state_update", "state": "running"}),
            ));
        }
        out.to_client.push(session_update(
            &session,
            json!({"sessionUpdate": "user_message", "messageId": message_id, "content": prompt}),
        ));
        out
    }

    /// The `idle` that ends `session`'s work.
    fn idle(&mut self, session: &str, stop: Option<&str>) -> Value {
        let state = self.sessions.entry(session.to_owned()).or_default();
        state.working = false;
        state.last_seen = None;
        state.message = None;
        state.open_calls.clear();
        let mut update = json!({"sessionUpdate": "state_update", "state": "idle"});
        let stop = stop.map(str::to_owned).or_else(|| state.stop_reason.take());
        if let Some(stop) = stop {
            update["stopReason"] = Value::from(stop);
        }
        if let Some(error) = state.error.take() {
            update["error"] = json!({"message": error});
        }
        if let Some(usage) = state.usage.take() {
            update["usage"] = Value::Object(usage);
        }
        session_update(session, update)
    }

    /// What a message of the agent's becomes.
    fn incoming(&mut self, mut message: Value, now: Instant) -> Vec<Value> {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        match (method.as_deref(), id) {
            // An answer to a request of cookie's.
            (None, Some(id)) => {
                let Some(pending) = self.pending.remove(&id_key(&id)) else {
                    return vec![message];
                };
                match pending {
                    Pending::Initialize => {
                        self.initialized(&mut message);
                        vec![message]
                    }
                    Pending::Open { session, servers } => {
                        if let Some(result) =
                            message.get_mut("result").and_then(Value::as_object_mut)
                        {
                            result.remove("modes");
                            result.remove("models");
                            let opened = result
                                .get("sessionId")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .or(session);
                            if let Some(opened) = opened {
                                self.sessions.entry(opened).or_default().servers = servers;
                            }
                        }
                        vec![message]
                    }
                    Pending::Prompt { session } => self.answered(&session, &message),
                }
            }
            _ if self.mode != Mode::V1 => vec![message],
            (Some("session/update"), None) => self.update(message, now),
            (Some("session/request_permission"), Some(id)) => {
                let mut out = self.agent_request(&message, &id, now);
                if let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) {
                    let mut call = params.get("toolCall").cloned().unwrap_or(Value::Null);
                    let session = params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if let Some(call) = call.as_object_mut() {
                        call.remove("sessionUpdate");
                        self.tool_call(&session, call);
                    }
                    if !params.contains_key("title") {
                        let title = call
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("The agent asks for permission")
                            .to_owned();
                        params.insert("title".to_owned(), Value::from(title));
                    }
                    if !call.is_null() {
                        params.insert(
                            "subject".to_owned(),
                            json!({"type": "tool_call", "toolCall": call}),
                        );
                    }
                }
                out.push(message);
                out
            }
            (Some(_), Some(id)) => {
                let mut out = self.agent_request(&message, &id, now);
                out.push(message);
                out
            }
            _ => vec![message],
        }
    }

    /// The agent's `initialize` answer: a v1 one is put in v2's names.
    fn initialized(&mut self, message: &mut Value) {
        let Some(result) = message.get_mut("result").and_then(Value::as_object_mut) else {
            return;
        };
        if result.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
            self.mode = Mode::V2;
            return;
        }
        self.mode = Mode::V1;
        let agent = result
            .remove("agentCapabilities")
            .unwrap_or_else(|| json!({}));
        let mut session = agent
            .get("sessionCapabilities")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        // v2 reads every capability as an object; v1 marks some `true`.
        let markers = |value: Option<&Value>| -> Value {
            let mut map = Map::new();
            for (name, on) in value.and_then(Value::as_object).into_iter().flatten() {
                if on.as_bool() == Some(true) || on.is_object() {
                    map.insert(name.clone(), json!({}));
                }
            }
            Value::Object(map)
        };
        session.insert(
            "prompt".to_owned(),
            markers(agent.get("promptCapabilities")),
        );
        let mut mcp = markers(agent.get("mcpCapabilities"));
        mcp["stdio"] = json!({});
        session.insert("mcp".to_owned(), mcp);
        self.system_prompt = session.contains_key("systemPrompt");
        self.meta_system_prompt = agent
            .pointer("/_meta/antigravity/systemPrompt/append")
            .is_some_and(|append| append.as_bool() == Some(true) || append.is_object());
        if self.meta_system_prompt {
            session.insert("systemPrompt".to_owned(), json!({}));
        }
        let mut capabilities = json!({"session": session});
        if let Some(auth) = agent.get("auth") {
            capabilities["auth"] = auth.clone();
        }
        if let Some(meta) = agent.get("_meta") {
            capabilities["_meta"] = meta.clone();
        }
        result.insert("capabilities".to_owned(), capabilities);
        if let Some(info) = result.remove("agentInfo") {
            result.insert("info".to_owned(), info);
        }
        if let Some(methods) = result.get_mut("authMethods").and_then(Value::as_array_mut) {
            for method in methods.iter_mut().filter_map(Value::as_object_mut) {
                if let Some(id) = method.remove("id") {
                    method.entry("methodId").or_insert(id);
                }
            }
        }
    }

    /// The agent answered a prompt the translator sent: the work is `idle`
    /// once it answered every prompt sent.
    fn answered(&mut self, session: &str, answer: &Value) -> Vec<Value> {
        let state = self.sessions.entry(session.to_owned()).or_default();
        state.in_flight = state.in_flight.saturating_sub(1);
        if let Some(error) = answer.get("error") {
            state.stop_reason = Some("error".to_owned());
            state.error = Some(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| error.to_string(), str::to_owned),
            );
        } else if let Some(result) = answer.get("result") {
            state.stop_reason = result
                .get("stopReason")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(usage) = result.get("usage").and_then(Value::as_object) {
                let sum = state.usage.get_or_insert_with(Map::new);
                for (name, value) in usage {
                    match (sum.get(name).and_then(Value::as_u64), value.as_u64()) {
                        (Some(known), Some(more)) => {
                            sum.insert(name.clone(), Value::from(known.saturating_add(more)));
                        }
                        _ => {
                            sum.insert(name.clone(), value.clone());
                        }
                    }
                }
            }
        }
        if state.in_flight > 0 || !state.working {
            return Vec::new();
        }
        vec![self.idle(session, None)]
    }

    /// A request of the agent's (a permission, a question): it starts work
    /// when none runs, and holds a wake's end until cookie answers.
    fn agent_request(&mut self, message: &Value, id: &Value, now: Instant) -> Vec<Value> {
        let Some(session) = param_str(message, "sessionId") else {
            return Vec::new();
        };
        self.agent_requests.insert(id_key(id), session.clone());
        self.work_shows(&session, now)
    }

    /// The agent shows work in `session`: reported `running` when none
    /// was, as work it started itself.
    fn work_shows(&mut self, session: &str, now: Instant) -> Vec<Value> {
        let state = self.sessions.entry(session.to_owned()).or_default();
        if state.in_flight > 0 || state.parent.is_some() {
            return Vec::new();
        }
        state.last_seen = Some(now);
        if state.working {
            return Vec::new();
        }
        state.working = true;
        state.stop_reason = None;
        state.error = None;
        state.usage = None;
        vec![session_update(
            session,
            json!({"sessionUpdate": "state_update", "state": "running"}),
        )]
    }

    /// A `session/update` in v2's shape, after a `running` when it starts
    /// the agent's own work.
    fn update(&mut self, mut message: Value, now: Instant) -> Vec<Value> {
        let session = param_str(&message, "sessionId").unwrap_or_default();
        let Some(update) = message
            .pointer_mut("/params/update")
            .and_then(Value::as_object_mut)
        else {
            return vec![message];
        };
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        // A call's end that comes after the work it was part of ended
        // (such as one cancelled with its turn) starts no work.
        let stray_end = kind == "tool_call_update"
            && matches!(
                update.get("status").and_then(Value::as_str),
                Some("completed" | "failed" | "cancelled")
            )
            && update
                .get("toolCallId")
                .and_then(Value::as_str)
                .is_none_or(|call| {
                    self.sessions
                        .get(&session)
                        .is_none_or(|state| !state.working && !state.open_calls.contains(call))
                });
        let mut out = if is_work(&kind) && !stray_end {
            self.work_shows(&session, now)
        } else {
            Vec::new()
        };
        let state = self.sessions.entry(session.clone()).or_default();
        match kind.as_str() {
            "agent_message_chunk" | "agent_thought_chunk" => {
                if !update.contains_key("messageId") {
                    let id = match &state.message {
                        Some((streaming, id)) if *streaming == kind => id.clone(),
                        _ => {
                            self.next_id += 1;
                            let id = format!("cookie-v1-agent-{}", self.next_id);
                            state.message = Some((kind.clone(), id.clone()));
                            id
                        }
                    };
                    update.insert("messageId".to_owned(), Value::from(id));
                }
            }
            "tool_call" | "tool_call_update" => {
                state.message = None;
                update.insert("sessionUpdate".to_owned(), Value::from("tool_call_update"));
                if let Some(call) = update.get("toolCallId").and_then(Value::as_str) {
                    match update.get("status").and_then(Value::as_str) {
                        Some("completed" | "failed" | "cancelled") => {
                            state.open_calls.remove(call);
                        }
                        Some(_) => {
                            state.open_calls.insert(call.to_owned());
                        }
                        None if kind == "tool_call" => {
                            state.open_calls.insert(call.to_owned());
                        }
                        None => {}
                    }
                }
                let terminal = terminal_updates(update);
                self.tool_call(&session, update);
                out.push(message);
                out.extend(
                    terminal
                        .into_iter()
                        .map(|update| session_update(&session, update)),
                );
                return out;
            }
            // A subagent's state comes in its announcements; v2 reports it
            // on the subagent's own session too, where cookie reads it.
            "subagent_update" => {
                state.message = None;
                let child = update
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let child_state = update.get("state").and_then(Value::as_object).cloned();
                if let Some(child) = child {
                    self.sessions.entry(child.clone()).or_default().parent = Some(session.clone());
                    out.push(message);
                    if let Some(mut child_state) = child_state {
                        child_state.insert("sessionUpdate".to_owned(), Value::from("state_update"));
                        out.push(session_update(&child, Value::Object(child_state)));
                    }
                    return out;
                }
            }
            "plan" => {
                state.message = None;
                let entries = update.remove("entries").unwrap_or_else(|| json!([]));
                update.insert("sessionUpdate".to_owned(), Value::from("plan_update"));
                update.insert("planId".to_owned(), Value::from("plan"));
                update.insert(
                    "plan".to_owned(),
                    json!({"type": "items", "entries": entries}),
                );
            }
            // Config options say the same, which cookie reads.
            "current_mode_update" => return out,
            "usage_update"
            | "available_commands_update"
            | "config_option_update"
            | "session_info_update" => {}
            _ => state.message = None,
        }
        out.push(message);
        out
    }

    /// A tool call (an update's, or a permission request's) in v2's
    /// shape: its MCP tool named as cookie reads it, its diffs as patches.
    fn tool_call(&self, session: &str, call: &mut Map<String, Value>) {
        if let Some(name) = call.get("name").and_then(Value::as_str)
            && let Some(renamed) = self.mcp_name(session, name)
        {
            call.insert("name".to_owned(), Value::from(renamed));
        }
        if let Some(content) = call.get_mut("content").and_then(Value::as_array_mut) {
            for item in content.iter_mut() {
                if item.get("type").and_then(Value::as_str) == Some("diff")
                    && item.get("changes").is_none()
                {
                    *item = diff(item);
                }
            }
        }
    }

    /// `mcp__<server>__<tool>` for an MCP tool named `mcp_<server>_<tool>`
    /// of a server cookie handed `session`; the longest name wins.
    fn mcp_name(&self, session: &str, name: &str) -> Option<String> {
        let rest = name.strip_prefix("mcp_")?;
        if rest.starts_with('_') {
            return None;
        }
        let mut state = self.sessions.get(session)?;
        // A subagent has its root session's servers.
        for _ in 0..16 {
            match state
                .parent
                .as_deref()
                .and_then(|parent| self.sessions.get(parent))
            {
                Some(parent) if state.servers.is_empty() => state = parent,
                _ => break,
            }
        }
        state
            .servers
            .iter()
            .filter_map(|server| {
                let tool = rest.strip_prefix(server.as_str())?.strip_prefix('_')?;
                (!tool.is_empty()).then_some((server, tool))
            })
            .max_by_key(|(server, _)| server.len())
            .map(|(server, tool)| format!("mcp__{server}__{tool}"))
    }

    /// When the agent's own work counts as ended, if any runs.
    fn deadline(&self) -> Option<Instant> {
        self.sessions
            .iter()
            .filter_map(|(session, state)| self.wake_deadline(session, state))
            .min()
    }

    fn wake_deadline(&self, session: &str, state: &Session) -> Option<Instant> {
        if !state.working || state.in_flight > 0 {
            return None;
        }
        let last = state.last_seen?;
        if self
            .agent_requests
            .values()
            .any(|waiting| waiting == session)
        {
            return None;
        }
        let quiet = if state.open_calls.is_empty() {
            WAKE_QUIET
        } else {
            WAKE_QUIET_OPEN_CALL
        };
        Some(last + quiet)
    }

    /// The `idle`s of the agent's own work that went quiet by `now`.
    fn timer(&mut self, now: Instant) -> Vec<Value> {
        let ended = self
            .sessions
            .iter()
            .filter(|(session, state)| {
                self.wake_deadline(session, state)
                    .is_some_and(|deadline| deadline <= now)
            })
            .map(|(session, _)| session.clone())
            .collect::<Vec<_>>();
        ended
            .into_iter()
            .map(|session| self.idle(&session, Some("end_turn")))
            .collect()
    }
}

/// The v2 terminal updates for the terminal extension's `_meta` keys of a
/// tool call (Zed's, which Claude Code's and Codex's adapters follow once
/// the client declares `_meta.terminal_output`): `terminal_info` names the
/// call's terminal, which the call's content then names as v2 does;
/// `terminal_output` and `terminal_output_delta` carry text to append, and
/// `terminal_exit` how the command ended. The keys are taken out of the
/// call.
fn terminal_updates(call: &mut Map<String, Value>) -> Vec<Value> {
    use base64::Engine as _;
    let Some(meta) = call.get_mut("_meta").and_then(Value::as_object_mut) else {
        return Vec::new();
    };
    let id_of = |value: &Value| {
        value
            .get("terminal_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    };
    let named = meta.remove("terminal_info").as_ref().and_then(id_of);
    let mut updates = Vec::new();
    for key in ["terminal_output", "terminal_output_delta"] {
        if let Some(output) = meta.remove(key)
            && let Some(id) = id_of(&output).or_else(|| named.clone())
            && let Some(data) = output.get("data").and_then(Value::as_str)
            && !data.is_empty()
        {
            updates.push(json!({
                "sessionUpdate": "terminal_output_chunk",
                "terminalId": id,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            }));
        }
    }
    if let Some(exit) = meta.remove("terminal_exit")
        && let Some(id) = id_of(&exit).or_else(|| named.clone())
    {
        updates.push(json!({
            "sessionUpdate": "terminal_update",
            "terminalId": id,
            "exitStatus": {
                "exitCode": exit.get("exit_code").cloned().unwrap_or(Value::Null),
                "signal": exit.get("signal").cloned().unwrap_or(Value::Null),
            },
        }));
    }
    if let Some(id) = named {
        let item = json!({"type": "terminal", "terminalId": id});
        match call.get_mut("content").and_then(Value::as_array_mut) {
            Some(content) => {
                if !content.contains(&item) {
                    content.insert(0, item);
                }
            }
            None => {
                call.insert("content".to_owned(), json!([item]));
            }
        }
    }
    updates
}

fn param_str(message: &Value, name: &str) -> Option<String> {
    message
        .get("params")?
        .get(name)?
        .as_str()
        .map(str::to_owned)
}

/// A v1 diff (`path`, `oldText`, `newText`) as a v2 one: the file's change,
/// with the text's difference as a git patch.
fn diff(item: &Value) -> Value {
    let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
    let old = item.get("oldText").and_then(Value::as_str);
    let new = item
        .get("newText")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let unified = similar::TextDiff::from_lines(old.unwrap_or_default(), new)
        .unified_diff()
        .context_radius(3)
        .to_string();
    let mut patch = format!("diff --git {path} {path}\n");
    if old.is_none() {
        patch.push_str("new file mode 100644\n");
    }
    patch.push_str(&unified);
    json!({
        "type": "diff",
        "changes": [{"path": path, "operation": if old.is_none() { "add" } else { "modify" }}],
        "patch": {"format": "git_patch", "text": patch},
    })
}

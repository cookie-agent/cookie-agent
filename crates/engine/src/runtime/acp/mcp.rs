//! Cookie's tools for an ACP agent (`docs/site/specs/acp-agents.md`
//! section 9): the MCP server the daemon serves behind the bridge.
//!
//! Each ACP session gets a bridge on the agent's runtime (`agent/mcp_open`),
//! which the agent reaches as the stdio MCP server `cookie`; the runtime
//! relays every message of each connection to it as `agent/mcp`. The
//! daemon answers them here: `initialize`, `ping`, `tools/list` with the
//! session's tools as cookie composes them for a model request (the merged
//! rules hide what they deny) minus the agent document's `acp.hide_tools`,
//! and `tools/call` through the same preparation, permission decision,
//! approval, and execution as a model's call, inside the agent's turn.
//!
//! A call through the bridge runs as a call of the agent's turn: the ACP
//! runner records it as a cookie tool call of the turn, next to the call
//! the agent reports for it.
//!
//! The server advertises `tools.listChanged`. The daemon keeps what each
//! open connection last fetched from `tools/list` and, at its check points
//! (after each bridged call, before its result; at the start of each turn;
//! when a client attaches or detaches a runtime), compares the session's
//! list with it. A connection whose list differs is told
//! `notifications/tools/list_changed`, once until it fetches again.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use cookie_agent_protocol::SessionId;
use cookie_agent_runtime_api::protocol::{AgentMcpClosedParams, AgentMcpParams, method};
use serde_json::{Value, json};

use super::super::{Engine, Inner};
use super::process::AgentProcess;
use super::runner::LiveSession;
use crate::runtime_snapshot::RuntimeAgent;
use crate::runtimes::RuntimeClient;

/// The name the agent knows cookie's MCP server by, in `session/new` and
/// in the names it gives the server's tools (`mcp__cookie__<tool>`).
pub(crate) const BRIDGE_SERVER: &str = "cookie";

/// The MCP versions the server answers with, newest first; a client asking
/// for another gets the first.
const MCP_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// How long a bridge call waits for the agent's report that says which
/// session made it, before it is refused. The report travels on the
/// agent's ACP stream and is read as it is routed, never behind a runner's
/// writes, so only the two streams' skew delays it; the bound is for an
/// agent that does not report the call at all.
const REPORT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// How often a bridge call waiting for a report looks again even without
/// a new one: the session of a report can be routed later than it came.
const REPORT_RECHECK: std::time::Duration = std::time::Duration::from_millis(50);

/// How deep the agent's subagents of a bridge's session are looked for.
const MAX_SUBAGENT_DEPTH: usize = 16;

/// The most reports a process keeps; the oldest go first.
const MAX_SIGHTINGS: usize = 1024;

/// The `_meta` keys of a `tools/call` that name the agent's ID of the
/// call, as it reports the call in its ACP stream: Claude Code's.
const CALL_ID_META: [&str; 1] = ["claudecode/toolUseId"];

/// Where an agent's report of its call of a cookie tool stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SightingState {
    /// No bridge call stands for it yet.
    Open,
    /// The agent ended the call without a bridge call claiming it.
    Ended,
    /// A bridge call runs it in its own session.
    Claimed,
    /// A bridge call runs it in the bridge's session, since the subagent
    /// that made it is not offered the tool: the subagent does not show it.
    ClaimedElsewhere,
}

/// The agent's report of its call `call` of cookie's tool `tool` in its
/// ACP session `acp_session`, read by the process as it routed it.
#[derive(Clone, Debug)]
pub(crate) struct BridgeSighting {
    pub acp_session: String,
    pub call: String,
    pub tool: String,
    pub state: SightingState,
}

/// What a `tool_call_update` says of a call of cookie's tools: its ID, the
/// tool when it names one, and whether the call ended.
pub(crate) struct Sighted {
    call: String,
    tool: Option<String>,
    ended: bool,
}

/// The call of cookie's tools `update` reports or ends, if it is one.
pub(crate) fn sighting_of(update: &Value) -> Option<Sighted> {
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("tool_call_update") {
        return None;
    }
    let call = update
        .get("toolCallId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_owned();
    let text = |key: &str| update.get(key).and_then(Value::as_str).map(str::to_owned);
    let fields = super::wire::ToolCallFields {
        tool_call_id: call.clone(),
        name: text("name"),
        title: text("title"),
        raw_input: update.get("rawInput").cloned(),
        meta: update.get("_meta").cloned(),
        ..Default::default()
    };
    Some(Sighted {
        call,
        tool: super::permissions::bridge_tool_of(&fields),
        ended: matches!(
            update.get("status").and_then(Value::as_str),
            Some("completed" | "failed")
        ),
    })
}

/// Records what the process routed of a call of cookie's tools in
/// `acp_session`: a new report, or the end of one.
pub(crate) fn sight(process: &AgentProcess, acp_session: &str, sighted: Sighted) {
    let mut sightings = super::lock(&process.bridge_sightings);
    if let Some(known) = sightings
        .iter_mut()
        .find(|known| known.acp_session == acp_session && known.call == sighted.call)
    {
        if sighted.ended && known.state == SightingState::Open {
            known.state = SightingState::Ended;
        }
        return;
    }
    let Some(tool) = sighted.tool.filter(|_| !sighted.ended) else {
        return;
    };
    if sightings.len() >= MAX_SIGHTINGS {
        sightings.remove(0);
    }
    sightings.push(BridgeSighting {
        acp_session: acp_session.to_owned(),
        call: sighted.call,
        tool,
        state: SightingState::Open,
    });
    drop(sightings);
    process.sighted.notify_waiters();
}

/// Forgets the report of call `call` in `acp_session`: how it stood, when
/// there was one.
pub(crate) fn forget_sighting(
    process: &AgentProcess,
    acp_session: &str,
    call: &str,
) -> Option<SightingState> {
    let mut sightings = super::lock(&process.bridge_sightings);
    let index = sightings
        .iter()
        .position(|known| known.acp_session == acp_session && known.call == call)?;
    Some(sightings.remove(index).state)
}

/// The agent's ID of the call a `tools/call` with `params` stands for, when
/// the agent names it.
fn named_call(params: &Value) -> Option<&str> {
    let meta = params.get("_meta")?;
    CALL_ID_META
        .iter()
        .find_map(|key| meta.get(*key).and_then(Value::as_str))
        .filter(|id| !id.is_empty())
}

/// Which session a bridge call runs in, and for whom.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Attribution {
    pub session: SessionId,
    /// The agent's ID of the call it stands for, when known.
    pub call: Option<String>,
    /// It runs in the bridge's session for a subagent not offered the tool.
    pub for_subagent: bool,
}

/// The cookie session and agent a bridge serves.
#[derive(Clone, Debug)]
pub(crate) struct BridgeBinding {
    pub session: SessionId,
    pub agent: RuntimeAgent,
}

/// One MCP bridge: the session it serves and its open connections, by the
/// number the runtime gives each.
#[derive(Debug)]
pub(crate) struct Bridge {
    pub binding: BridgeBinding,
    connections: std::collections::HashMap<u64, BridgeConnection>,
}

/// An open connection to a bridge.
struct BridgeConnection {
    /// The runtime connection the bridge's messages travel over.
    client: RuntimeClient,
    /// The fingerprint of the list the connection last fetched from
    /// `tools/list`; `None` before its first fetch.
    fetched: Option<u64>,
    /// Told the list changed since it last fetched it.
    notified: bool,
}

impl std::fmt::Debug for BridgeConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BridgeConnection")
            .field("fetched", &self.fetched)
            .field("notified", &self.notified)
            .finish_non_exhaustive()
    }
}

/// The fingerprint of a tool list: its names, descriptions, and input
/// schemas, in order.
fn fingerprint(tools: &[Value]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(tools)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// Binds bridge `bridge` of runtime `runtime` to `session`.
pub(crate) fn bind(
    inner: &Arc<Inner>,
    agent: &RuntimeAgent,
    runtime: &str,
    bridge: &str,
    session: SessionId,
) {
    inner
        .acp
        .processes
        .bridges
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            (runtime.to_owned(), bridge.to_owned()),
            Bridge {
                binding: BridgeBinding {
                    session,
                    agent: agent.clone(),
                },
                connections: Default::default(),
            },
        );
}

/// A bridge connection ended: forgets it.
pub(crate) fn connection_closed(
    inner: &Arc<Inner>,
    client: &RuntimeClient,
    params: &AgentMcpClosedParams,
) {
    if let Some(bridge) = super::lock(&inner.acp.processes.bridges)
        .get_mut(&(client.runtime_id().to_owned(), params.bridge.clone()))
    {
        bridge.connections.remove(&params.connection);
    }
}

/// Records that `connection` of bridge `key` fetched `tools`.
fn fetched(inner: &Inner, key: &(String, String), connection: u64, tools: &[Value]) {
    if let Some(open) = super::lock(&inner.acp.processes.bridges)
        .get_mut(key)
        .and_then(|bridge| bridge.connections.get_mut(&connection))
    {
        open.fetched = Some(fingerprint(tools));
        open.notified = false;
    }
}

/// Answers one MCP message a bridge connection sent.
pub(crate) fn message(engine: Engine, client: RuntimeClient, params: AgentMcpParams) {
    let key = (client.runtime_id().to_owned(), params.bridge.clone());
    let binding = super::lock(&engine.inner.acp.processes.bridges)
        .get_mut(&key)
        .map(|bridge| {
            bridge
                .connections
                .entry(params.connection)
                .or_insert_with(|| BridgeConnection {
                    client: client.clone(),
                    fetched: None,
                    notified: false,
                });
            bridge.binding.clone()
        });
    let Some(id) = params.message.get("id").cloned() else {
        // Notifications (`notifications/initialized`, cancellations) need
        // no answer.
        return;
    };
    let method_name = params
        .message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if method_name.is_empty() {
        // A reply to a request this server never sends.
        return;
    }
    tokio::spawn(async move {
        let request = params.message.get("params").cloned().unwrap_or(Value::Null);
        let answer = match binding {
            None => Err((-32002, "this bridge serves no session".to_owned())),
            Some(binding) => {
                engine
                    .mcp_answer(&binding, (&key, params.connection), &method_name, &request)
                    .await
            }
        };
        let message = match answer {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        let _ = client
            .agent_notify(
                method::AGENT_MCP,
                &AgentMcpParams {
                    bridge: params.bridge,
                    connection: params.connection,
                    message,
                },
            )
            .await;
    });
}

/// Whether `tool` matches one of the `hidden` patterns (`*`, `?`).
fn hidden(tool: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| cookie_agent_config::simple_wildcard_match(pattern, tool))
}

impl Engine {
    async fn mcp_answer(
        &self,
        binding: &BridgeBinding,
        (key, connection): (&(String, String), u64),
        method: &str,
        params: &Value,
    ) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let asked = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let version = MCP_VERSIONS
                    .iter()
                    .find(|version| **version == asked)
                    .unwrap_or(&MCP_VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": BRIDGE_SERVER, "version": env!("CARGO_PKG_VERSION")},
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                let tools = self
                    .bridge_tools(binding)
                    .map_err(|error| (-32603, error.to_string()))?;
                fetched(&self.inner, key, connection, &tools);
                Ok(json!({"tools": tools}))
            }
            "tools/call" => {
                let result = self.bridge_call(binding, params).await;
                // Before the result, so the agent can fetch the new list
                // before its next step.
                self.announce_tool_changes(binding.session).await;
                Ok(result)
            }
            other => Err((
                -32601,
                format!("cookie's MCP server has no method `{other}`"),
            )),
        }
    }

    /// The patterns of the tools the agent of `binding` does not get.
    fn hidden_tools(&self, binding: &BridgeBinding) -> Vec<String> {
        self.current_runtime()
            .agents
            .get(&binding.agent.id)
            .and_then(|agent| agent.document.frontmatter.acp.clone())
            .unwrap_or_default()
            .hidden_tools()
    }

    /// The active run of `binding`'s session, if one runs.
    fn bridge_run(
        &self,
        binding: &BridgeBinding,
    ) -> Option<(cookie_agent_protocol::RunId, Arc<super::super::ActiveRun>)> {
        self.inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(_, active)| active.session == binding.session)
            .map(|(run, active)| (*run, Arc::clone(active)))
    }

    /// The tools of `session` a run of `policy` offers its ACP agent over
    /// the bridge, `hide` left out, by the provider that publishes them.
    pub(crate) fn offered_tools(
        &self,
        session: SessionId,
        policy: &super::super::FrozenRunPolicy,
        hide: &[String],
    ) -> Result<super::super::tool_prompts::OfferedTools, crate::EngineError> {
        let composition = self.compose_for_policy(session, policy)?;
        let mut offered = super::super::tool_prompts::OfferedTools::default();
        for tool in composition
            .tools
            .iter()
            .filter(|tool| !hidden(&tool.name, hide))
        {
            let provider = tool.source.provider.provider_id();
            offered.skill |= tool.spec.permission_name.as_deref() == Some("skill");
            if provider == crate::memory_tools::PROVIDER_ID {
                offered.memory = true;
                offered.memory_write |=
                    tool.spec.permission_name.as_deref() == Some("memory_write");
            }
            offered.providers.insert((
                provider.to_owned(),
                tool.source
                    .attachment
                    .as_ref()
                    .map(|name| name.as_str().to_owned()),
            ));
        }
        Ok(offered)
    }

    /// The tools of `session` a run of `policy` offers its ACP agent over
    /// the bridge: its tool definitions without `hide`.
    pub(crate) fn bridged_tool_definitions(
        &self,
        session: SessionId,
        policy: &super::super::FrozenRunPolicy,
        hide: &[String],
    ) -> Result<Vec<oven_sdk::ToolDefinition>, crate::EngineError> {
        let mut tools = self.tool_definitions(session, policy)?;
        tools.retain(|tool| !hidden(&tool.name, hide));
        Ok(tools)
    }

    /// The session's tools as the agent gets them.
    fn bridge_tools(&self, binding: &BridgeBinding) -> Result<Vec<Value>, crate::EngineError> {
        let policy = match self.bridge_run(binding) {
            Some((_, active)) => Arc::clone(&active.policy),
            None => {
                let projection = self.inner.store.get(binding.session)?;
                let selection = cookie_agent_protocol::RunSelection {
                    agent: binding.agent.id.clone(),
                    model: cookie_agent_protocol::acp_model_selection(&binding.agent.id),
                    preset: projection.meta.creation_selection.preset.clone(),
                };
                let memory = self.memory_variables(binding.session);
                Arc::new(self.freeze_root_selection(&selection, true, &memory)?.0)
            }
        };
        let hide = self.hidden_tools(binding);
        Ok(self
            .bridged_tool_definitions(binding.session, &policy, &hide)?
            .into_iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema.as_value(),
                })
            })
            .collect())
    }

    /// Tells each connection to a bridge of `session` that the tool list
    /// changed when the list it last fetched differs from the session's
    /// now, once until it fetches again. A subagent's calls go over its
    /// parent's bridge, so the parent's session is the one to name.
    pub(crate) async fn announce_tool_changes(&self, session: SessionId) {
        let candidates = super::lock(&self.inner.acp.processes.bridges)
            .iter()
            .filter(|(_, bridge)| {
                bridge.binding.session == session
                    && bridge
                        .connections
                        .values()
                        .any(|open| open.fetched.is_some() && !open.notified)
            })
            .map(|(key, bridge)| (key.clone(), bridge.binding.clone()))
            .collect::<Vec<_>>();
        for (key, binding) in candidates {
            let Ok(tools) = self.bridge_tools(&binding) else {
                continue;
            };
            let now = fingerprint(&tools);
            let due = super::lock(&self.inner.acp.processes.bridges)
                .get_mut(&key)
                .map(|bridge| {
                    bridge
                        .connections
                        .iter_mut()
                        .filter(|(_, open)| {
                            !open.notified && open.fetched.is_some_and(|last| last != now)
                        })
                        .map(|(connection, open)| {
                            open.notified = true;
                            (*connection, open.client.clone())
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for (connection, client) in due {
                let _ = client
                    .agent_notify(
                        method::AGENT_MCP,
                        &AgentMcpParams {
                            bridge: key.1.clone(),
                            connection,
                            message: json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/tools/list_changed",
                            }),
                        },
                    )
                    .await;
            }
        }
    }

    /// The cookie session of the agent's ACP session `acp` when it is
    /// `bridge`'s own or one of its subagents' (at any depth), which reach
    /// cookie over its bridge.
    fn bridge_member(&self, bridge: &LiveSession, acp: &str) -> Option<SessionId> {
        if acp == bridge.acp_session_id {
            return Some(bridge.session);
        }
        let child = super::lock(&self.inner.acp.subagents).get(acp).copied()?;
        let live = self.live_session(child)?;
        if !Arc::ptr_eq(&live.process, &bridge.process) {
            return None;
        }
        let mut session = child;
        for _ in 0..MAX_SUBAGENT_DEPTH {
            let parent = self.acp_subagent_parent(session)?;
            if parent == bridge.session {
                return Some(child);
            }
            session = parent;
        }
        None
    }

    /// Whether another session than `bridge`'s may call over its bridge: a
    /// subagent of it routed, or one the agent announced that is not.
    fn bridge_shared(&self, bridge: &LiveSession) -> bool {
        if bridge.process.expects_subagents() {
            return true;
        }
        let others = super::lock(&self.inner.acp.processes.sessions)
            .values()
            .filter(|live| {
                live.session != bridge.session && Arc::ptr_eq(&live.process, &bridge.process)
            })
            .map(|live| live.acp_session_id.clone())
            .collect::<Vec<_>>();
        others
            .iter()
            .any(|acp| self.bridge_member(bridge, acp).is_some())
    }

    /// The session a call of `tool` over `binding`'s bridge (`bridge`, its
    /// live session) belongs to, by the agent's report of the call, which
    /// names the session that made it: the bridge's own or one of its
    /// subagents', which share the bridge. A call that names the agent's
    /// ID for it (`named`) waits for that report; one that does not takes
    /// the only session with a report of the tool no bridge call claimed,
    /// and runs at once in the bridge's session when no other could have
    /// made it. A subagent whose rules do not offer the tool has its call
    /// run in the bridge's session, and does not show it again. A call
    /// whose session cannot be told is refused, never run in another.
    async fn bridge_call_session(
        &self,
        binding: &BridgeBinding,
        bridge: &LiveSession,
        tool: &str,
        named: Option<&str>,
    ) -> Result<Attribution, String> {
        let deadline = tokio::time::Instant::now() + REPORT_WAIT;
        loop {
            let sighted = bridge.process.sighted.notified();
            tokio::pin!(sighted);
            sighted.as_mut().enable();
            if let Some(found) = self.claim_sighting(bridge, tool, named)? {
                return Ok(self.offer_to_subagent(binding, bridge, tool, found));
            }
            if named.is_none() && !self.bridge_shared(bridge) {
                return Ok(Attribution {
                    session: bridge.session,
                    call: None,
                    for_subagent: false,
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "cookie could not tell which of the agent's sessions called `{tool}`: the \
                     agent did not report the call, so it did not run"
                ));
            }
            tokio::select! {
                () = &mut sighted => {}
                () = tokio::time::sleep(REPORT_RECHECK) => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    /// Claims the agent's report a call of `tool` over `bridge`'s bridge
    /// stands for (the one `named`, else the oldest of the only session
    /// with one open): the session that made it and the call's ID; `None`
    /// while none fits yet.
    fn claim_sighting(
        &self,
        bridge: &LiveSession,
        tool: &str,
        named: Option<&str>,
    ) -> Result<Option<(SessionId, BridgeSighting)>, String> {
        let candidates = super::lock(&bridge.process.bridge_sightings)
            .iter()
            .filter(|known| match named {
                Some(call) => known.call == call,
                None => known.tool == tool && known.state == SightingState::Open,
            })
            .cloned()
            .collect::<Vec<_>>();
        let members = candidates
            .into_iter()
            .filter_map(|known| Some((self.bridge_member(bridge, &known.acp_session)?, known)))
            .collect::<Vec<_>>();
        let Some((session, chosen)) = members.first().cloned() else {
            return Ok(None);
        };
        if named.is_some() {
            if chosen.tool != tool {
                return Err(format!(
                    "the agent's call `{}` is of `{}`, not `{tool}`, so it did not run",
                    chosen.call, chosen.tool
                ));
            }
            if chosen.state != SightingState::Open {
                return Err(format!(
                    "the agent's call `{}` already ran or ended, so it did not run again",
                    chosen.call
                ));
            }
        } else if members.iter().any(|(other, _)| *other != session) {
            return Err(format!(
                "more than one of the agent's sessions is calling `{tool}` and the call does \
                 not say whose it is, so it did not run"
            ));
        }
        let mut sightings = super::lock(&bridge.process.bridge_sightings);
        match sightings.iter_mut().find(|known| {
            known.acp_session == chosen.acp_session
                && known.call == chosen.call
                && known.state == SightingState::Open
        }) {
            Some(known) => {
                known.state = SightingState::Claimed;
                Ok(Some((session, chosen)))
            }
            // It changed meanwhile: looked at again.
            None => Ok(None),
        }
    }

    /// Where the call the agent's session `session` reported (`report`)
    /// runs: there, unless it is a subagent whose rules do not offer
    /// `tool`, whose call runs in the bridge's session.
    fn offer_to_subagent(
        &self,
        binding: &BridgeBinding,
        bridge: &LiveSession,
        tool: &str,
        (session, report): (SessionId, BridgeSighting),
    ) -> Attribution {
        if session == bridge.session {
            return Attribution {
                session,
                call: Some(report.call),
                for_subagent: false,
            };
        }
        let subagent = BridgeBinding {
            session,
            agent: binding.agent.clone(),
        };
        let offered = self.bridge_tools(&subagent).is_ok_and(|tools| {
            tools
                .iter()
                .any(|offered| offered.get("name").and_then(Value::as_str) == Some(tool))
        });
        if offered {
            return Attribution {
                session,
                call: Some(report.call),
                for_subagent: false,
            };
        }
        if let Some(known) = super::lock(&bridge.process.bridge_sightings)
            .iter_mut()
            .find(|known| known.acp_session == report.acp_session && known.call == report.call)
        {
            known.state = SightingState::ClaimedElsewhere;
        }
        Attribution {
            session: bridge.session,
            call: None,
            for_subagent: true,
        }
    }

    /// Runs one `tools/call` as a call of the agent's turn (the runner of
    /// the session's turn records and runs it): a result for the agent, an
    /// error one when the call failed or was refused.
    async fn bridge_call(&self, binding: &BridgeBinding, params: &Value) -> Value {
        let failed = |message: String| json!({"content": [{"type": "text", "text": message}], "isError": true});
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return failed("the call names no tool".to_owned());
        };
        if hidden(name, &self.hidden_tools(binding)) {
            return failed(format!("tool `{name}` is not offered to this agent"));
        }
        let outside_turns =
            || failed("cookie's tools can be called only during the agent's turns".to_owned());
        let Some(bridge) = self.live_session(binding.session) else {
            return outside_turns();
        };
        let attribution = match self
            .bridge_call_session(binding, &bridge, name, named_call(params))
            .await
        {
            Ok(attribution) => attribution,
            Err(refusal) => return failed(refusal),
        };
        #[cfg(test)]
        self.inner.test_hooks.bridge_attributed(attribution.session);
        let Some(live) = self.live_session(attribution.session) else {
            return outside_turns();
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        let event = super::process::SessionEvent::BridgeCall {
            name: name.to_owned(),
            arguments: params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
            call: attribution.call,
            for_subagent: attribution.for_subagent,
            reply,
        };
        if live.process.send_to(&live.acp_session_id, event).is_err() {
            return outside_turns();
        }
        answer.await.unwrap_or_else(|_| {
            failed("the agent's turn ended before the call finished".to_owned())
        })
    }
}

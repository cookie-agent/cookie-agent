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
use crate::runtime_snapshot::RuntimeAgent;
use crate::runtimes::RuntimeClient;

/// The name the agent knows cookie's MCP server by, in `session/new` and
/// in the names it gives the server's tools (`mcp__cookie__<tool>`).
pub(crate) const BRIDGE_SERVER: &str = "cookie";

/// The MCP versions the server answers with, newest first; a client asking
/// for another gets the first.
const MCP_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// How long a bridge call waits for the agent's report of it, which names
/// the session that made it, before it goes to the bridge's own session.
const REPORT_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// An agent's report of its call of cookie's tool `tool` (its tool call
/// `call`) in `session`, which no bridge call has claimed yet.
#[derive(Clone, Debug)]
pub(crate) struct BridgeReport {
    pub session: SessionId,
    /// The parent session, when `session` is the agent's own subagent,
    /// whose calls reach cookie over its parent's bridge.
    pub parent: Option<SessionId>,
    pub call: String,
    pub tool: String,
    /// Whether `session`'s own rules offer the tool, so the call runs
    /// there; else it runs in the parent.
    pub offered: bool,
    /// A bridge call ran it in the parent: the subagent does not show it
    /// again.
    pub claimed_elsewhere: bool,
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

    /// The session a call of `tool` over `bridge_session`'s bridge belongs
    /// to: the one whose oldest unclaimed report of a call of the tool the
    /// agent sent, the bridge's own or one of its subagents' (which share
    /// the bridge), waited for a moment since the report and the call
    /// travel apart; the bridge's own without one. A subagent whose rules
    /// do not offer the tool has its call run in the parent, and does not
    /// show it again; the second value says so.
    async fn bridge_call_session(
        &self,
        bridge_session: SessionId,
        tool: &str,
    ) -> (SessionId, bool) {
        let deadline = tokio::time::Instant::now() + REPORT_WAIT;
        loop {
            {
                let mut reports = super::lock(&self.inner.acp.bridge_reports);
                if let Some(index) = reports.iter().position(|report| {
                    report.tool == tool
                        && !report.claimed_elsewhere
                        && (report.session == bridge_session
                            || report.parent == Some(bridge_session))
                }) {
                    let report = &mut reports[index];
                    if report.session == bridge_session || report.offered {
                        return (reports.remove(index).session, false);
                    }
                    report.claimed_elsewhere = true;
                    return (bridge_session, true);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return (bridge_session, false);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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
        let (session, for_subagent) = self.bridge_call_session(binding.session, name).await;
        let Some(live) = self.live_session(session) else {
            return failed("cookie's tools can be called only during the agent's turns".to_owned());
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        let event = super::process::SessionEvent::BridgeCall {
            name: name.to_owned(),
            arguments: params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
            for_subagent,
            reply,
        };
        if live.process.send_to(&live.acp_session_id, event).is_err() {
            return failed("cookie's tools can be called only during the agent's turns".to_owned());
        }
        answer.await.unwrap_or_else(|_| {
            failed("the agent's turn ended before the call finished".to_owned())
        })
    }
}

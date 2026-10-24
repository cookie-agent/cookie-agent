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

/// The cookie session and agent a bridge serves.
#[derive(Clone, Debug)]
pub(crate) struct BridgeBinding {
    pub session: SessionId,
    pub agent: RuntimeAgent,
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
            BridgeBinding {
                session,
                agent: agent.clone(),
            },
        );
}

/// A bridge connection ended; nothing is kept per connection.
pub(crate) fn connection_closed(
    _inner: &Arc<Inner>,
    _client: &RuntimeClient,
    _params: &AgentMcpClosedParams,
) {
}

/// Answers one MCP message a bridge connection sent.
pub(crate) fn message(engine: Engine, client: RuntimeClient, params: AgentMcpParams) {
    let binding = engine
        .inner
        .acp
        .processes
        .bridges
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&(client.runtime_id().to_owned(), params.bridge.clone()))
        .cloned();
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
            Some(binding) => engine.mcp_answer(&binding, &method_name, &request).await,
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
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": BRIDGE_SERVER, "version": env!("CARGO_PKG_VERSION")},
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => self
                .bridge_tools(binding)
                .map(|tools| json!({"tools": tools}))
                .map_err(|error| (-32603, error.to_string())),
            "tools/call" => Ok(self.bridge_call(binding, params).await),
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
            .tool_definitions(binding.session, &policy)?
            .into_iter()
            .filter(|tool| !hidden(&tool.name, &hide))
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema.as_value(),
                })
            })
            .collect())
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
        let Some(live) = self.live_session(binding.session) else {
            return failed("cookie's tools can be called only during the agent's turns".to_owned());
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        let event = super::process::SessionEvent::BridgeCall {
            name: name.to_owned(),
            arguments: params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
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

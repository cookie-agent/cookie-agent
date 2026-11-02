//! `acp.check` (`docs/site/specs/acp-onboarding.md` section 5): starts a
//! declared ACP agent on a runtime, opens a session in the runtime's home,
//! remembers what the agent offered, and closes the session again, saying
//! how far it got.

use std::time::Duration;

use cookie_agent_protocol::{AcpCheckResult, AcpCheckStage, AgentId, RuntimeId};

use super::{lifecycle::end_agent_session, wire};
use crate::Engine;

/// How long the agent gets to answer `session/new`; a first start through
/// `npx` downloads the adapter before `initialize`, which the process start
/// waits for on its own.
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);

fn failed(stage: AcpCheckStage, error: String) -> AcpCheckResult {
    AcpCheckResult {
        ok: false,
        stage,
        error: Some(error),
        agent_name: None,
        agent_version: None,
        options: Vec::new(),
    }
}

impl Engine {
    /// Checks the ACP agent `agent` declared on runtime `runtime`.
    pub async fn check_acp_agent(&self, runtime: &RuntimeId, agent: &AgentId) -> AcpCheckResult {
        let Some(declared) = self
            .inner
            .acp
            .runtime_agents()
            .into_iter()
            .find(|candidate| candidate.runtime == *runtime && candidate.agent == *agent)
        else {
            return failed(
                AcpCheckStage::Declared,
                format!("runtime `{runtime}` does not declare ACP agent `{agent}`"),
            );
        };
        if !declared.enabled {
            return failed(
                AcpCheckStage::Declared,
                format!("ACP agent `{agent}` is turned off on runtime `{runtime}`"),
            );
        }
        if !declared.connected {
            return failed(
                AcpCheckStage::Declared,
                format!("runtime `{runtime}` is not connected"),
            );
        }
        let (process, _use) = match self
            .inner
            .acp
            .processes
            .acquire(&self.inner, &declared)
            .await
        {
            Ok(acquired) => acquired,
            Err(error) => return failed(AcpCheckStage::Start, error),
        };
        let info = &process.init().info;
        let text = |key: &str| {
            info.get(key)
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let agent_name = text("title").or_else(|| text("name"));
        let agent_version = text("version");
        let cwd = process.client.home().unwrap_or_else(|| "/".to_owned());
        let response = tokio::time::timeout(
            SESSION_TIMEOUT,
            process.peer.request::<_, wire::SessionResponse>(
                wire::method::SESSION_NEW,
                &wire::NewSessionRequest {
                    cwd: &cwd,
                    additional_directories: &[],
                    mcp_servers: &[],
                    system_prompt: None,
                    meta: wire::session_meta(),
                },
            ),
        )
        .await;
        let response = match response {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                let reason = process
                    .exit_reason()
                    .unwrap_or_else(|| super::process::agent_error_message(&error));
                return AcpCheckResult {
                    agent_name,
                    agent_version,
                    ..failed(
                        AcpCheckStage::Session,
                        format!(
                            "the agent did not open a session: {reason}; if it needs a login, \
                             log in to it on that machine"
                        ),
                    )
                };
            }
            Err(_) => {
                return AcpCheckResult {
                    agent_name,
                    agent_version,
                    ..failed(
                        AcpCheckStage::Session,
                        format!(
                            "the agent did not answer `session/new` within {} s",
                            SESSION_TIMEOUT.as_secs()
                        ),
                    )
                };
            }
        };
        let options = super::runner::options_of(response.config_options.as_ref());
        self.remember_acp_options(&declared, &options, true);
        if let Some(session) = response.session_id.as_deref().filter(|id| !id.is_empty()) {
            end_agent_session(&process, session, true, true).await;
        }
        AcpCheckResult {
            ok: true,
            stage: AcpCheckStage::Ready,
            error: None,
            agent_name,
            agent_version,
            options,
        }
    }
}

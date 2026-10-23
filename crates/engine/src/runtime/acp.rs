//! ACP agents (`docs/site/specs/acp-agents.md`): external coding agents a
//! runtime hosts, which take a session's or a subagent's turns.
//!
//! Runtimes declare their agents and report them; the engine registers
//! each as `<runtime>-<agent>` ([`cookie_agent_protocol::runtime_agent_id`])
//! next to the daemon's own agents, available while its runtime is
//! connected (or started on demand). A run of such an agent goes to the
//! ACP runner ([`runner`]) instead of the model loop: the engine speaks
//! ACP to the agent's process, which the runtime starts and relays
//! ([`process`]), renders what the agent reports as model turns and tool
//! calls, decides its permission requests by the agent's rules
//! ([`permissions`]), and serves it the session's tools over the MCP
//! bridge ([`mcp`]).

use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{AgentId, RuntimeId};

use super::Engine;
use crate::runtime_snapshot::RuntimeAgent;

pub(crate) mod mcp;
pub(crate) mod permissions;
pub(crate) mod process;
pub(crate) mod runner;
pub(crate) mod wire;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The engine's ACP agents and their processes.
pub(crate) struct AcpState {
    /// The agents runtimes host, as the agent registry was last resolved
    /// with.
    agents: Mutex<Vec<RuntimeAgent>>,
    /// Refused declarations already reported, so each is reported once.
    reported: Mutex<HashSet<String>>,
    /// The agents' processes and sessions.
    pub(crate) processes: process::Processes,
}

impl AcpState {
    /// The state of an engine opening with `config`: the agents its
    /// runtimes declare, until they connect and report.
    pub(crate) fn new(config: &cookie_agent_config::LoadedConfiguration) -> Self {
        let declared = config
            .runtimes
            .iter()
            .map(|(runtime, template)| {
                (
                    runtime.to_string(),
                    crate::runtimes::RuntimeAcpAgents {
                        agents: template
                            .acp_agents
                            .iter()
                            .map(
                                |(id, agent)| cookie_agent_runtime_api::protocol::AcpAgentReport {
                                    id: id.to_string(),
                                    description: agent.description.clone(),
                                    enabled: agent.enabled,
                                },
                            )
                            .collect(),
                        available: true,
                    },
                )
            })
            .collect();
        let reported = Mutex::default();
        let agents = runtime_agents(declared, &reported);
        Self {
            agents: Mutex::new(agents),
            reported,
            processes: process::Processes::default(),
        }
    }

    /// The agents runtimes host now.
    pub(crate) fn runtime_agents(&self) -> Vec<RuntimeAgent> {
        lock(&self.agents).clone()
    }
}

/// Reports a refused declaration once.
fn report_once(reported: &Mutex<HashSet<String>>, message: String) {
    if lock(reported).insert(message.clone()) {
        eprintln!("cookie-agent: {message}");
    }
}

/// The agents of every runtime, each under its daemon ID. A declaration
/// whose ID is not an agent ID, or whose daemon ID another runtime's agent
/// has, is refused and reported: the first runtime in ID order keeps it.
fn runtime_agents(
    by_runtime: BTreeMap<String, crate::runtimes::RuntimeAcpAgents>,
    reported: &Mutex<HashSet<String>>,
) -> Vec<RuntimeAgent> {
    let mut agents = BTreeMap::<AgentId, RuntimeAgent>::new();
    for (runtime, declared) in by_runtime {
        let Ok(runtime_id) = RuntimeId::new(runtime.as_str()) else {
            continue;
        };
        for report in declared.agents {
            let Ok(agent) = AgentId::new(report.id.as_str()) else {
                report_once(
                    reported,
                    format!(
                        "runtime `{runtime}` declares ACP agent `{}`, which is not an agent ID",
                        report.id
                    ),
                );
                continue;
            };
            let id = match cookie_agent_protocol::runtime_agent_id(&runtime_id, &agent) {
                Ok(id) => id,
                Err(reason) => {
                    report_once(reported, reason);
                    continue;
                }
            };
            if let Some(other) = agents.get(&id) {
                report_once(
                    reported,
                    format!(
                        "ACP agent `{agent}` of runtime `{runtime}` would be agent `{id}`, as ACP \
                         agent `{}` of runtime `{}` is; it is not registered",
                        other.agent, other.runtime
                    ),
                );
                continue;
            }
            agents.insert(
                id.clone(),
                RuntimeAgent {
                    id,
                    runtime: runtime_id.clone(),
                    agent,
                    description: report.description,
                    enabled: report.enabled,
                    connected: declared.available,
                },
            );
        }
    }
    agents.into_values().collect()
}

impl Engine {
    /// Starts following the runtimes' ACP agents: the relay handler for
    /// their processes, and a task that re-registers them whenever a
    /// runtime reports a change or connects or disconnects.
    pub(super) fn install_acp(&self) {
        let handler: Arc<dyn crate::runtimes::AcpRelayHandler> =
            Arc::new(process::RelayHandler::new(Arc::downgrade(&self.inner)));
        self.inner.mcp_hub.acp().set_handler(handler);
        let Some(runtime) = self.inner.runtime.clone() else {
            return;
        };
        let mut changes = self.inner.mcp_hub.acp().subscribe();
        let mut registry_changes = self.inner.runtimes.subscribe();
        let weak = Arc::downgrade(&self.inner);
        runtime.spawn(async move {
            loop {
                {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    Engine { inner }.refresh_runtime_agents();
                }
                tokio::select! {
                    changed = changes.changed() => if changed.is_err() {
                        return;
                    },
                    changed = registry_changes.changed() => if changed.is_err() {
                        return;
                    },
                }
            }
        });
    }

    /// Re-registers the runtimes' ACP agents, and publishes the agent
    /// registry again when they changed.
    pub(crate) fn refresh_runtime_agents(&self) {
        let agents = runtime_agents(self.inner.runtimes.acp_agents(), &self.inner.acp.reported);
        let config = self.inner.config.load();
        for agent in &agents {
            if config
                .agents
                .get(&agent.id)
                .is_some_and(|document| document.frontmatter.acp.is_none())
            {
                report_once(
                    &self.inner.acp.reported,
                    format!(
                        "ACP agent `{}` of runtime `{}` would be agent `{}`, which names a daemon \
                         agent; it is not registered",
                        agent.agent, agent.runtime, agent.id
                    ),
                );
            }
        }
        {
            let mut current = lock(&self.inner.acp.agents);
            if *current == agents {
                return;
            }
            *current = agents;
        }
        if let Err(error) = self.republish_agents() {
            eprintln!(
                "cookie-agent: ACP agents changed but the agent registry was not republished: \
                 {error}"
            );
        }
    }

    /// Publishes the runtime again for an agent registry change.
    fn republish_agents(&self) -> Result<(), crate::EngineError> {
        let _mutation = self
            .inner
            .runtime_mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = self.current_runtime();
        let models = Arc::clone(&previous.models);
        let publication = self.prepare_publication(
            &self.inner.config.load(),
            &models,
            &previous,
            vec![cookie_agent_protocol::RuntimeChangeReason::AgentReloaded],
        )?;
        self.publish(publication);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cookie_agent_runtime_api::protocol::AcpAgentReport;

    use super::*;

    fn declared(ids: &[&str], available: bool) -> crate::runtimes::RuntimeAcpAgents {
        crate::runtimes::RuntimeAcpAgents {
            agents: ids
                .iter()
                .map(|id| AcpAgentReport {
                    id: (*id).to_owned(),
                    description: String::new(),
                    enabled: *id != "off",
                })
                .collect(),
            available,
        }
    }

    #[test]
    fn colliding_and_malformed_declarations_are_refused_once_each() {
        let reported = Mutex::default();
        let agents = runtime_agents(
            BTreeMap::from([
                ("lab".to_owned(), declared(&["pc-codex", "off"], true)),
                ("lab_pc".to_owned(), declared(&["codex", "Bad"], false)),
                ("lab__x".to_owned(), declared(&["codex"], true)),
            ]),
            &reported,
        );
        let listed = agents
            .iter()
            .map(|agent| (agent.id.as_str(), agent.runtime.as_str(), agent.available()))
            .collect::<Vec<_>>();
        assert_eq!(
            listed,
            [("lab-off", "lab", false), ("lab-pc-codex", "lab", true)]
        );
        let reported = lock(&reported);
        assert_eq!(reported.len(), 3, "{reported:?}");
        assert!(reported.iter().any(|message| {
            message.contains("`lab-pc-codex`, as ACP agent `pc-codex` of runtime `lab`")
        }));
        assert!(reported.iter().any(|message| message.contains("`Bad`")));
        assert!(
            reported
                .iter()
                .any(|message| message.contains("`lab--x-codex`"))
        );
    }
}

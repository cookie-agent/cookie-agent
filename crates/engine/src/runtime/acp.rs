//! ACP agents (`docs/site/specs/acp-agents.md`): external coding agents a
//! runtime hosts, which take a session's or a subagent's turns.
//!
//! Runtimes declare their agents and report them; the engine registers
//! each declared ID as one agent next to the daemon's own, offered by
//! every runtime that declares it. A session runs it on the one of its
//! attached runtimes that offers it ([`crate::runtime_snapshot::AcpOffers`]).
//! A run of such an agent goes to the
//! ACP runner ([`runner`]) instead of the model loop: the engine speaks
//! ACP to the agent's process, which the runtime starts and relays
//! ([`process`]), renders what the agent reports as model turns and tool
//! calls, decides its permission requests by the agent's rules
//! ([`permissions`]), and serves it the session's tools over the MCP
//! bridge ([`mcp`]).

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{AgentId, RuntimeId, SessionId};

use super::Engine;
use crate::runtime_snapshot::RuntimeAgent;

mod background;
#[cfg(test)]
pub(crate) use background::WAKE_GRACE;
pub(crate) mod compaction;
mod diff;
pub(crate) mod elicitation;
pub(crate) mod lifecycle;
pub(crate) mod mcp;
pub(crate) mod options;
pub(crate) mod permissions;
pub(crate) mod process;
pub(crate) mod prompt;
pub(crate) mod runner;
mod subagents;
mod terminal;
pub(crate) use subagents::is_acp_subagent;
mod wakes;
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
    /// What each agent offered on each runtime when it last started there.
    pub(crate) known: options::KnownAgents,
    /// The agents' processes and sessions.
    pub(crate) processes: process::Processes,
    /// The agents' questions waiting on the user.
    pub(crate) questions: elicitation::Questions,
    /// The user inputs on their way into an agent's running turn
    /// (`_session/steering`), by session: they stay waiting, and no client
    /// can take them back, until the agent answers.
    pub(crate) steering: Mutex<HashMap<SessionId, Vec<u64>>>,
    /// The cookie sessions of the agents' own subagents, by the
    /// subagent's ACP session ID.
    pub(crate) subagents: Mutex<HashMap<String, SessionId>>,
    /// The agents' commands still running after their calls ended.
    pub(crate) background: Mutex<Vec<background::AgentCommand>>,
    /// When a command of each session's agent last ended between runs, for
    /// the grace in which the agent may still start work about it.
    pub(crate) background_ended: Mutex<HashMap<SessionId, std::time::Instant>>,
}

impl AcpState {
    /// The state of an engine opening with `config`: the agents its
    /// runtimes declare, until they connect and report.
    pub(crate) fn new(
        config: &cookie_agent_config::LoadedConfiguration,
        data_dir: &std::path::Path,
    ) -> Self {
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
        let known = options::KnownAgents::open(data_dir);
        let agents = runtime_agents(declared, &reported, &known);
        Self {
            agents: Mutex::new(agents),
            reported,
            known,
            processes: process::Processes::default(),
            questions: elicitation::Questions::default(),
            steering: Mutex::new(HashMap::new()),
            subagents: Mutex::new(HashMap::new()),
            background: Mutex::new(Vec::new()),
            background_ended: Mutex::new(HashMap::new()),
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

/// The agents of every runtime, one entry per runtime and agent, in
/// agent and runtime order, with what `known` remembers of each. A
/// declaration whose ID is not an agent ID is refused and reported.
fn runtime_agents(
    by_runtime: BTreeMap<String, crate::runtimes::RuntimeAcpAgents>,
    reported: &Mutex<HashSet<String>>,
    known: &options::KnownAgents,
) -> Vec<RuntimeAgent> {
    let mut agents = BTreeMap::<(AgentId, RuntimeId), RuntimeAgent>::new();
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
            agents.insert(
                (agent.clone(), runtime_id.clone()),
                RuntimeAgent {
                    known: known.get(&agent, &runtime_id),
                    id: agent.clone(),
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
        // A daemon document without an `acp` block keeps its ID; the
        // configuration status names the declarations it shadows.
        let agents = runtime_agents(
            self.inner.runtimes.acp_agents(),
            &self.inner.acp.reported,
            &self.inner.acp.known,
        );
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
    fn runtimes_declaring_one_id_offer_one_agent_and_malformed_ids_are_refused_once() {
        let reported = Mutex::default();
        let agents = runtime_agents(
            BTreeMap::from([
                ("lab".to_owned(), declared(&["codex", "off"], true)),
                ("lab_pc".to_owned(), declared(&["codex", "Bad"], false)),
            ]),
            &reported,
            &options::KnownAgents::default(),
        );
        let listed = agents
            .iter()
            .map(|agent| (agent.id.as_str(), agent.runtime.as_str(), agent.available()))
            .collect::<Vec<_>>();
        assert_eq!(
            listed,
            [
                ("codex", "lab", true),
                ("codex", "lab_pc", false),
                ("off", "lab", false)
            ]
        );
        let reported = lock(&reported);
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert!(reported.iter().any(|message| message.contains("`Bad`")));
    }
}

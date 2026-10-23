//! ACP agents in the clients (`docs/site/specs/acp-agents.md` section 13,
//! phase 4): the agents a runtime hosts in the agent pickers, with their
//! runtime and availability, and the synthetic model they run under.

use cookie_agent_protocol::{
    ACP_MODEL_PROVIDER, AgentDescriptor, AgentMode, AgentRuntime, ModelSelection, RuntimeId,
    ToolSource,
};

/// Whether `agent` is an ACP agent: some runtime offers it.
pub fn is_acp(agent: &AgentDescriptor) -> bool {
    agent.runtime.is_some() || !agent.runtimes.is_empty()
}

/// The runtimes that offer ACP agent `agent`: `runtimes`, else the one
/// `runtime` a daemon older than client protocol 1.21.0 sends.
pub fn offering_runtimes(agent: &AgentDescriptor) -> Vec<&AgentRuntime> {
    if agent.runtimes.is_empty() {
        agent.runtime.iter().collect()
    } else {
        agent.runtimes.iter().collect()
    }
}

/// Where a session that attaches `attached` would run an ACP agent
/// (`docs/site/specs/acp-agents.md` section 15, naming of 2026-10-23).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcpPlacement {
    /// On this runtime, the one attached that offers it.
    Runs(RuntimeId),
    /// The one attached runtime that offers it is away, or has it
    /// disabled.
    Unavailable(RuntimeId),
    /// No attached runtime offers it; these do.
    NeedsOneOf(Vec<RuntimeId>),
    /// Several attached runtimes offer it, and a session runs it on one.
    Ambiguous(Vec<RuntimeId>),
}

/// Where a session or draft that attaches `attached` would run `agent`;
/// `None` for the daemon's own agents.
pub fn placement(agent: &AgentDescriptor, attached: &[RuntimeId]) -> Option<AcpPlacement> {
    let offering = offering_runtimes(agent);
    if offering.is_empty() {
        return None;
    }
    let here = offering
        .iter()
        .filter(|offer| attached.contains(&offer.runtime_id))
        .collect::<Vec<_>>();
    Some(match here[..] {
        [] => AcpPlacement::NeedsOneOf(
            offering
                .iter()
                .map(|offer| offer.runtime_id.clone())
                .collect(),
        ),
        [offer] if offer.available => AcpPlacement::Runs(offer.runtime_id.clone()),
        [offer] => AcpPlacement::Unavailable(offer.runtime_id.clone()),
        _ => AcpPlacement::Ambiguous(here.iter().map(|offer| offer.runtime_id.clone()).collect()),
    })
}

fn joined(runtimes: &[RuntimeId]) -> String {
    runtimes
        .iter()
        .map(RuntimeId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether an agent picker for root sessions of `preset` lists `agent`:
/// every agent that can run as a root, and an ACP agent that could but no
/// runtime can run now, which the picker shows dimmed and does not let the
/// user pick.
pub fn listed_in_root_picker(agent: &AgentDescriptor, preset: Option<&str>) -> bool {
    agent.mode != AgentMode::Internal
        && agent.preset.as_deref() == preset
        && (agent.runnable_as_root
            || (is_acp(agent) && matches!(agent.mode, AgentMode::Primary | AgentMode::All)))
}

/// Whether the user can pick `agent` for a session or draft that attaches
/// `attached`: it runs as a root, and an ACP agent has exactly one of
/// those runtimes offering it, available.
pub fn pickable(agent: &AgentDescriptor, attached: &[RuntimeId]) -> bool {
    agent.runnable_as_root
        && placement(agent, attached)
            .is_none_or(|placement| matches!(placement, AcpPlacement::Runs(_)))
}

/// What a picker shows after an ACP agent's description, for a session or
/// draft that attaches `attached`: where it runs, or why it cannot. `None`
/// for the daemon's own agents.
pub fn runtime_note(agent: &AgentDescriptor, attached: &[RuntimeId]) -> Option<String> {
    Some(match placement(agent, attached)? {
        AcpPlacement::Runs(runtime) if agent.runnable_as_root => format!("on {runtime}"),
        AcpPlacement::Runs(runtime) => format!("on {runtime} · disabled"),
        AcpPlacement::Unavailable(runtime) => {
            let away = offering_runtimes(agent)
                .iter()
                .any(|offer| offer.runtime_id == runtime && !offer.available);
            if away && agent.enabled {
                format!("on {runtime} · unavailable")
            } else {
                format!("on {runtime} · disabled")
            }
        }
        AcpPlacement::NeedsOneOf(runtimes) => format!("needs {} attached", joined(&runtimes)),
        AcpPlacement::Ambiguous(runtimes) => {
            format!("offered by {}: attach one", joined(&runtimes))
        }
    })
}

/// Why picking `agent` for a session or draft that attaches `attached` is
/// refused, for a status line.
pub fn unpickable_reason(agent: &AgentDescriptor, attached: &[RuntimeId]) -> String {
    match placement(agent, attached) {
        Some(AcpPlacement::NeedsOneOf(runtimes)) => format!(
            "`{}` needs a runtime that offers it attached: {}",
            agent.id,
            joined(&runtimes)
        ),
        Some(AcpPlacement::Ambiguous(runtimes)) => format!(
            "`{}` is offered by more than one attached runtime ({}); attach only one of them",
            agent.id,
            joined(&runtimes)
        ),
        Some(AcpPlacement::Unavailable(runtime)) => format!(
            "`{}` is unavailable on runtime `{runtime}`: the runtime is not connected, or the \
             agent is disabled there",
            agent.id
        ),
        Some(AcpPlacement::Runs(runtime)) => {
            format!("`{}` is disabled on runtime `{runtime}`", agent.id)
        }
        None => format!("`{}` cannot run as a root agent", agent.id),
    }
}

/// The model a session of ACP agent `agent` records, `acp/<agent>`; `None`
/// for the daemon's own agents.
pub fn acp_model(agent: &AgentDescriptor) -> Option<ModelSelection> {
    if !is_acp(agent) {
        return None;
    }
    agent
        .resolved_fallback
        .first()
        .cloned()
        .or_else(|| Some(cookie_agent_protocol::acp_model_selection(&agent.id)))
}

/// Whether `selection` is an ACP agent's synthetic model, which no model
/// picker lists.
pub fn is_acp_model(selection: &ModelSelection) -> bool {
    selection.model.provider_id().as_str() == ACP_MODEL_PROVIDER
}

/// The label of a tool call's source when it is an ACP agent's own tool:
/// the agent and its runtime.
pub fn acp_source_label(source: &ToolSource) -> Option<String> {
    match source {
        ToolSource::Acp { agent, runtime_id } => Some(format!("{agent} on {runtime_id}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{AgentDescriptor, AgentId, AgentMode, AgentRuntime, RuntimeId};

    use super::*;

    fn runtime(id: &str) -> RuntimeId {
        RuntimeId::new(id).unwrap()
    }

    fn agent(
        id: &str,
        offers: &[(&str, bool)],
        runnable: bool,
        mode: AgentMode,
    ) -> AgentDescriptor {
        let id = AgentId::new(id).unwrap();
        let runtimes = offers
            .iter()
            .map(|(runtime_id, available)| AgentRuntime {
                runtime_id: runtime(runtime_id),
                agent: id.clone(),
                available: *available,
            })
            .collect::<Vec<_>>();
        AgentDescriptor {
            resolved_fallback: if offers.is_empty() {
                Vec::new()
            } else {
                vec![cookie_agent_protocol::acp_model_selection(&id)]
            },
            runtime: match &runtimes[..] {
                [only] => Some(only.clone()),
                _ => None,
            },
            runtimes,
            id,
            preset: None,
            description: "An agent".into(),
            mode,
            enabled: true,
            runnable_as_root: runnable,
            delegation_targets: Vec::new(),
            unset_models: Vec::new(),
        }
    }

    #[test]
    fn an_acp_agent_runs_on_the_one_attached_runtime_that_offers_it() {
        let claude = agent(
            "claude",
            &[("labpc", true), ("local", true)],
            true,
            AgentMode::All,
        );
        let local = [runtime("global"), runtime("local")];
        assert_eq!(
            placement(&claude, &local),
            Some(AcpPlacement::Runs(runtime("local")))
        );
        assert!(pickable(&claude, &local));
        assert_eq!(runtime_note(&claude, &local).as_deref(), Some("on local"));
        // No attached runtime offers it.
        let elsewhere = [runtime("global"), runtime("gpu")];
        assert!(!pickable(&claude, &elsewhere));
        assert_eq!(
            runtime_note(&claude, &elsewhere).as_deref(),
            Some("needs labpc, local attached")
        );
        assert_eq!(
            unpickable_reason(&claude, &elsewhere),
            "`claude` needs a runtime that offers it attached: labpc, local"
        );
        // Two attached runtimes offer it.
        let both = [runtime("labpc"), runtime("local")];
        assert!(!pickable(&claude, &both));
        assert_eq!(
            runtime_note(&claude, &both).as_deref(),
            Some("offered by labpc, local: attach one")
        );
        assert!(unpickable_reason(&claude, &both).contains("(labpc, local)"));
    }

    #[test]
    fn an_unavailable_acp_agent_is_listed_but_not_pickable() {
        let away = agent("codex", &[("lab", false)], false, AgentMode::All);
        let lab = [runtime("lab")];
        assert!(listed_in_root_picker(&away, None));
        assert!(!pickable(&away, &lab));
        assert_eq!(
            runtime_note(&away, &lab).as_deref(),
            Some("on lab · unavailable")
        );
        // A subagent-only ACP agent is never a root choice.
        let sub = agent("reviewer", &[("lab", true)], false, AgentMode::Subagent);
        assert!(!listed_in_root_picker(&sub, None));
        // A daemon agent that cannot run as a root is not listed.
        let daemon = agent("helper", &[], false, AgentMode::All);
        assert!(!listed_in_root_picker(&daemon, None));
        assert_eq!(runtime_note(&daemon, &lab), None);
        assert!(!listed_in_root_picker(&away, Some("python")));
    }

    #[test]
    fn an_acp_agent_runs_its_synthetic_model() {
        let claude = agent("claude", &[("local", true)], true, AgentMode::All);
        let model = acp_model(&claude).unwrap();
        assert_eq!(model.model.as_str(), "acp/claude");
        assert!(is_acp_model(&model));
        assert_eq!(
            acp_model(&agent("primary", &[], true, AgentMode::Primary)),
            None
        );
    }
}

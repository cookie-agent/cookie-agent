//! ACP agents in the clients (`docs/site/specs/acp-agents.md` section 13,
//! phase 4): the agents a runtime hosts in the agent pickers, with their
//! runtime and availability, and the synthetic model they run under.

use cookie_agent_protocol::{
    ACP_MODEL_PROVIDER, AgentDescriptor, AgentMode, ModelSelection, ToolSource,
};

/// Whether an agent picker for root sessions of `preset` lists `agent`:
/// every agent that can run as a root, and an ACP agent that could but is
/// unavailable now (its runtime is away, or it is disabled), which the
/// picker shows dimmed and does not let the user pick.
pub fn listed_in_root_picker(agent: &AgentDescriptor, preset: Option<&str>) -> bool {
    agent.mode != AgentMode::Internal
        && agent.preset.as_deref() == preset
        && (agent.runnable_as_root
            || (agent.runtime.is_some()
                && matches!(agent.mode, AgentMode::Primary | AgentMode::All)))
}

/// Whether the user can pick `agent` in a root picker.
pub fn pickable(agent: &AgentDescriptor) -> bool {
    agent.runnable_as_root
}

/// The model a session of ACP agent `agent` records, `acp/<agent>`; `None`
/// for the daemon's own agents.
pub fn acp_model(agent: &AgentDescriptor) -> Option<ModelSelection> {
    agent.runtime.as_ref()?;
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

/// What a picker shows after an ACP agent's description: its runtime, and
/// why it cannot be picked when it cannot. `None` for the daemon's own
/// agents.
pub fn runtime_note(agent: &AgentDescriptor) -> Option<String> {
    let runtime = agent.runtime.as_ref()?;
    Some(if !runtime.available {
        format!("on {} · unavailable", runtime.runtime_id)
    } else if !agent.runnable_as_root {
        format!("on {} · disabled", runtime.runtime_id)
    } else {
        format!("on {}", runtime.runtime_id)
    })
}

/// Why picking `agent` is refused, for a status line.
pub fn unpickable_reason(agent: &AgentDescriptor) -> String {
    match &agent.runtime {
        Some(runtime) if !runtime.available => format!(
            "`{}` is unavailable: runtime `{}` is not connected",
            agent.id, runtime.runtime_id
        ),
        Some(runtime) => format!(
            "`{}` is disabled on runtime `{}`",
            agent.id, runtime.runtime_id
        ),
        None => format!("`{}` cannot run as a root agent", agent.id),
    }
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

    fn agent(
        id: &str,
        runtime: Option<(&str, bool)>,
        runnable: bool,
        mode: AgentMode,
    ) -> AgentDescriptor {
        let id = AgentId::new(id).unwrap();
        AgentDescriptor {
            resolved_fallback: if runtime.is_some() {
                vec![cookie_agent_protocol::acp_model_selection(&id)]
            } else {
                Vec::new()
            },
            runtime: runtime.map(|(runtime_id, available)| AgentRuntime {
                runtime_id: RuntimeId::new(runtime_id).unwrap(),
                agent: AgentId::new("claude").unwrap(),
                available,
            }),
            id,
            preset: None,
            description: "An agent".into(),
            mode,
            enabled: true,
            runnable_as_root: runnable,
            delegation_targets: Vec::new(),
        }
    }

    #[test]
    fn an_unavailable_acp_agent_is_listed_but_not_pickable() {
        let away = agent("lab-claude", Some(("lab", false)), false, AgentMode::All);
        assert!(listed_in_root_picker(&away, None));
        assert!(!pickable(&away));
        assert_eq!(runtime_note(&away).as_deref(), Some("on lab · unavailable"));
        assert!(unpickable_reason(&away).contains("runtime `lab` is not connected"));
        // A subagent-only ACP agent is never a root choice.
        let sub = agent("lab-codex", Some(("lab", true)), false, AgentMode::Subagent);
        assert!(!listed_in_root_picker(&sub, None));
        // A daemon agent that cannot run as a root is not listed.
        let daemon = agent("reviewer", None, false, AgentMode::All);
        assert!(!listed_in_root_picker(&daemon, None));
        assert_eq!(runtime_note(&daemon), None);
        // Another preset's agents are not listed.
        assert!(!listed_in_root_picker(&away, Some("python")));
    }

    #[test]
    fn an_acp_agent_runs_its_synthetic_model() {
        let local = agent("local-claude", Some(("local", true)), true, AgentMode::All);
        let model = acp_model(&local).unwrap();
        assert_eq!(model.model.as_str(), "acp/local-claude");
        assert!(is_acp_model(&model));
        assert_eq!(runtime_note(&local).as_deref(), Some("on local"));
        assert_eq!(
            acp_model(&agent("primary", None, true, AgentMode::Primary)),
            None
        );
    }
}

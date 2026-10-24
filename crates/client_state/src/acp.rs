//! ACP agents in the clients (`docs/site/specs/acp-agents.md` section 13,
//! phase 4): the agents a runtime hosts in the agent pickers, with their
//! runtime and availability, and the synthetic model they run under; and
//! the agent's own model and effort, which the clients show and pick as the
//! model and variant (section 15, input and pickers).

use cookie_agent_protocol::{
    ACP_MODEL_PROVIDER, AcpConfigOption, AcpOptionChoice, AgentDescriptor, AgentId, AgentMode,
    AgentRuntime, ModelSelection, RuntimeId, ToolSource,
};

use crate::AcpSessionView;

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

/// The option that is the agent's model: the one of category `model`,
/// else the one with ID `model`.
pub fn model_option(options: &[AcpConfigOption]) -> Option<&AcpConfigOption> {
    options
        .iter()
        .find(|option| option.category.as_deref() == Some("model"))
        .or_else(|| options.iter().find(|option| option.id == "model"))
}

/// The option that is the agent's reasoning effort: the one of category
/// `thought_level`.
pub fn effort_option(options: &[AcpConfigOption]) -> Option<&AcpConfigOption> {
    options
        .iter()
        .find(|option| option.category.as_deref() == Some("thought_level"))
}

/// The name of `option`'s current value, else the value itself.
pub fn current_name(option: &AcpConfigOption) -> String {
    option
        .values
        .iter()
        .find(|value| value.value == option.current_value)
        .map_or_else(|| option.current_value.clone(), |value| value.name.clone())
}

/// What the model chip shows for an agent with `options`: its model's
/// current value and, when it has one, its effort's, as the model and the
/// variant of cookie's own models. `None` without a model option.
pub fn model_and_effort(options: &[AcpConfigOption]) -> Option<(String, Option<String>)> {
    let model = model_option(options)?;
    Some((
        current_name(model),
        effort_option(options).map(current_name),
    ))
}

/// The model chip's label for an agent with `options`, in the form cookie
/// labels its own model and variant: `Large • High`.
pub fn model_label(options: &[AcpConfigOption]) -> Option<String> {
    let (model, effort) = model_and_effort(options)?;
    Some(match effort {
        Some(effort) => format!("{model} • {effort}"),
        None => model,
    })
}

/// The runtime a draft or session that attaches `attached` runs ACP agent
/// `agent` on, whatever its availability: the one attached runtime that
/// offers it.
pub fn placed_runtime<'a>(
    agent: &'a AgentDescriptor,
    attached: &[RuntimeId],
) -> Option<&'a AgentRuntime> {
    match placement(agent, attached)? {
        AcpPlacement::Runs(runtime) | AcpPlacement::Unavailable(runtime) => {
            offering_runtimes(agent)
                .into_iter()
                .find(|offer| offer.runtime_id == runtime)
        }
        AcpPlacement::NeedsOneOf(_) | AcpPlacement::Ambiguous(_) => None,
    }
}

/// `options` with `choices` made their current values, where an option
/// takes the value chosen.
pub fn with_choices(
    options: &[AcpConfigOption],
    choices: &[AcpOptionChoice],
) -> Vec<AcpConfigOption> {
    let mut options = options.to_vec();
    for choice in choices {
        if let Some(option) = options.iter_mut().find(|option| {
            option.id == choice.option_id
                && option
                    .values
                    .iter()
                    .any(|value| value.value == choice.value)
        }) {
            option.current_value.clone_from(&choice.value);
        }
    }
    options
}

/// The options a draft of `agent` that attaches `attached` offers: the
/// ones a new session of it starts with on the runtime it would run on,
/// with the draft's `choices` applied. Empty until the agent has started a
/// session there.
pub fn draft_options(
    agent: &AgentDescriptor,
    attached: &[RuntimeId],
    choices: &[AcpOptionChoice],
) -> Vec<AcpConfigOption> {
    placed_runtime(agent, attached)
        .map(|runtime| with_choices(&runtime.options, choices))
        .unwrap_or_default()
}

/// The options of a session whose ACP view is `view`: the agent's, once
/// its ACP session started; before that, what a draft of `agent` would
/// offer with the values the session was created with.
pub fn session_options(
    view: &AcpSessionView,
    agent: Option<&AgentDescriptor>,
    attached: &[RuntimeId],
) -> Vec<AcpConfigOption> {
    if !view.options.is_empty() {
        return view.options.clone();
    }
    agent
        .map(|agent| draft_options(agent, attached, &view.starting_options))
        .unwrap_or_default()
}

/// The choices a draft makes for its agent's options, which it sends as
/// `session.create`'s `agent_options`. Kept with the agent they were made
/// for, so choosing another agent drops them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DraftAgentOptions {
    agent: Option<AgentId>,
    choices: Vec<AcpOptionChoice>,
}

impl DraftAgentOptions {
    /// The choices made for `agent`.
    pub fn for_agent(&self, agent: &AgentId) -> &[AcpOptionChoice] {
        if self.agent.as_ref() == Some(agent) {
            &self.choices
        } else {
            &[]
        }
    }

    /// Chooses `value` for option `option_id` of `agent`, replacing an
    /// earlier choice of that option; choices made for another agent are
    /// dropped.
    pub fn choose(&mut self, agent: &AgentId, option_id: &str, value: &str) {
        if self.agent.as_ref() != Some(agent) {
            self.agent = Some(agent.clone());
            self.choices.clear();
        }
        match self
            .choices
            .iter_mut()
            .find(|choice| choice.option_id == option_id)
        {
            Some(choice) => value.clone_into(&mut choice.value),
            None => self.choices.push(AcpOptionChoice {
                option_id: option_id.to_owned(),
                value: value.to_owned(),
            }),
        }
    }

    /// Forgets every choice, as a draft becomes a session or is dropped.
    pub fn clear(&mut self) {
        *self = Self::default();
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
                options: Vec::new(),
                input_media: Vec::new(),
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

    fn select(
        id: &str,
        category: Option<&str>,
        current: &str,
        values: &[(&str, &str)],
    ) -> AcpConfigOption {
        AcpConfigOption {
            kind: Default::default(),
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            category: category.map(str::to_owned),
            current_value: current.to_owned(),
            values: values
                .iter()
                .map(|(value, name)| cookie_agent_protocol::AcpConfigValue {
                    value: (*value).to_owned(),
                    name: (*name).to_owned(),
                    description: None,
                })
                .collect(),
        }
    }

    fn claude_options() -> Vec<AcpConfigOption> {
        vec![
            select(
                "mode",
                Some("mode"),
                "auto",
                &[("default", "Default"), ("auto", "Auto")],
            ),
            select(
                "model",
                Some("model"),
                "sonnet",
                &[("sonnet", "Sonnet"), ("opus", "Opus")],
            ),
            select(
                "effort",
                Some("thought_level"),
                "low",
                &[("low", "Low"), ("high", "High")],
            ),
        ]
    }

    #[test]
    fn the_model_chip_shows_the_agents_model_and_effort_options() {
        let options = claude_options();
        assert_eq!(model_label(&options).as_deref(), Some("Sonnet • Low"));
        // Without an effort option, the model alone; without a model
        // option, nothing to show.
        assert_eq!(model_label(&options[..2]).as_deref(), Some("Sonnet"));
        assert_eq!(model_label(&[options[0].clone(), options[2].clone()]), None);
        // An option is the model by its ID when no option has the category.
        let by_id = [select("model", None, "gpt", &[("gpt", "GPT")])];
        assert_eq!(model_label(&by_id).as_deref(), Some("GPT"));
        // A current value the agent does not list shows as itself.
        let unlisted = [select("model", Some("model"), "custom", &[("gpt", "GPT")])];
        assert_eq!(model_label(&unlisted).as_deref(), Some("custom"));
    }

    #[test]
    fn a_draft_offers_the_placed_runtimes_options_with_its_choices() {
        let mut claude = agent(
            "claude",
            &[("labpc", true), ("local", true)],
            true,
            AgentMode::All,
        );
        claude.runtimes[1].options = claude_options();
        let local = [runtime("global"), runtime("local")];
        let mut draft = DraftAgentOptions::default();
        draft.choose(&claude.id, "model", "opus");
        draft.choose(&claude.id, "effort", "high");
        draft.choose(&claude.id, "effort", "extreme");
        // The last choice of an option wins; a value the agent does not
        // take leaves the agent's.
        assert_eq!(
            draft.for_agent(&claude.id),
            [
                AcpOptionChoice {
                    option_id: "model".into(),
                    value: "opus".into()
                },
                AcpOptionChoice {
                    option_id: "effort".into(),
                    value: "extreme".into()
                },
            ]
        );
        let options = draft_options(&claude, &local, draft.for_agent(&claude.id));
        assert_eq!(model_label(&options).as_deref(), Some("Opus • Low"));
        // labpc has never started the agent: nothing to offer there.
        assert!(draft_options(&claude, &[runtime("labpc")], &[]).is_empty());
        // Choosing for another agent drops the first agent's choices.
        let codex = AgentId::new("codex").unwrap();
        draft.choose(&codex, "model", "gpt");
        assert!(draft.for_agent(&claude.id).is_empty());
        assert_eq!(draft.for_agent(&codex).len(), 1);
    }

    #[test]
    fn a_session_shows_its_starting_options_until_its_agent_reports_its_own() {
        let mut claude = agent("claude", &[("local", true)], true, AgentMode::All);
        claude.runtimes[0].options = claude_options();
        let local = [runtime("local")];
        let mut view = AcpSessionView {
            starting_options: vec![AcpOptionChoice {
                option_id: "model".into(),
                value: "opus".into(),
            }],
            ..AcpSessionView::default()
        };
        let options = session_options(&view, Some(&claude), &local);
        assert_eq!(model_label(&options).as_deref(), Some("Opus • Low"));
        view.options = vec![select(
            "model",
            Some("model"),
            "haiku",
            &[("haiku", "Haiku")],
        )];
        let options = session_options(&view, Some(&claude), &local);
        assert_eq!(model_label(&options).as_deref(), Some("Haiku"));
    }
}

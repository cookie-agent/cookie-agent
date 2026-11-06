//! ACP agents in the clients (`docs/site/specs/acp-agents.md` section 13,
//! phase 4): the agents a runtime hosts in the agent pickers, with their
//! runtime and availability, and the synthetic model they run under; and
//! the agent's own model and effort, which the clients show and pick as the
//! model and variant (section 15, input and pickers).

use cookie_agent_protocol::{
    ACP_MODEL_PROVIDER, AcpConfigOption, AcpOptionChoice, AgentDescriptor, AgentId, AgentMode,
    AgentRuntime, InputMedia, ModelSelection, RuntimeId,
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
/// (`docs/site/specs/acp-several-runtimes.md` section 3.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcpPlacement {
    /// On this runtime.
    Runs(RuntimeId),
    /// The runtime it must run on is away, or has it disabled.
    Unavailable(RuntimeId),
    /// No attached runtime offers it, and none was chosen; these do.
    NeedsOneOf(Vec<RuntimeId>),
}

/// The declaration ACP agent `agent` runs, when its document names
/// another one than its own ID (`acp.agent`, addendum of 2026-10-25).
pub fn declaration(agent: &AgentDescriptor) -> Option<&AgentId> {
    offering_runtimes(agent)
        .first()
        .map(|offer| &offer.agent)
        .filter(|declaration| **declaration != agent.id)
}

/// Where a session or draft that attaches `attached` (in attach order)
/// would run `agent`; `None` for the daemon's own agents. `host` is the
/// runtime it must run on: the one its session started it on, else the one
/// the user chose, attached or not (the daemon attaches a chosen one).
/// Without one, the first attached runtime that offers it and can run it.
pub fn placement(
    agent: &AgentDescriptor,
    attached: &[RuntimeId],
    host: Option<&RuntimeId>,
) -> Option<AcpPlacement> {
    let offering = offering_runtimes(agent);
    if offering.is_empty() {
        return None;
    }
    let on = |offer: &AgentRuntime| {
        if offer.available {
            AcpPlacement::Runs(offer.runtime_id.clone())
        } else {
            AcpPlacement::Unavailable(offer.runtime_id.clone())
        }
    };
    if let Some(host) = host {
        return Some(
            offering
                .iter()
                .find(|offer| &offer.runtime_id == host)
                .map_or_else(
                    || AcpPlacement::Unavailable(host.clone()),
                    |offer| on(offer),
                ),
        );
    }
    let here = attached
        .iter()
        .filter_map(|runtime| offering.iter().find(|offer| &offer.runtime_id == runtime))
        .collect::<Vec<_>>();
    Some(
        match here
            .iter()
            .find(|offer| offer.available)
            .or_else(|| here.first())
        {
            Some(offer) => on(offer),
            None => AcpPlacement::NeedsOneOf(
                offering
                    .iter()
                    .map(|offer| offer.runtime_id.clone())
                    .collect(),
            ),
        },
    )
}

/// The runtime a draft that attaches `attached` runs `agent` on when the
/// user chooses none: the first attached runtime that offers it and can
/// run it, else the first that offers it and can run it at all.
pub fn default_runtime(agent: &AgentDescriptor, attached: &[RuntimeId]) -> Option<RuntimeId> {
    let offering = offering_runtimes(agent);
    attached
        .iter()
        .find(|runtime| {
            offering
                .iter()
                .any(|offer| &offer.runtime_id == *runtime && offer.available)
        })
        .cloned()
        .or_else(|| {
            offering
                .iter()
                .find(|offer| offer.available)
                .map(|offer| offer.runtime_id.clone())
        })
}

/// The runtimes that offer `agent`, for the Runs on choice: each with
/// whether it can run the agent now. Empty for the daemon's own agents.
pub fn runs_on_choices(agent: &AgentDescriptor) -> Vec<(RuntimeId, bool)> {
    offering_runtimes(agent)
        .into_iter()
        .map(|offer| (offer.runtime_id.clone(), offer.available))
        .collect()
}

fn joined(runtimes: &[RuntimeId]) -> String {
    runtimes
        .iter()
        .map(RuntimeId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether an agent picker for root sessions of `preset` lists `agent`:
/// every agent that can run as a root, and a root agent that cannot run
/// now (no model, an ACP agent no runtime can run), which the picker shows
/// dimmed, below the others, with the reason, and does not let the user
/// pick. An agent the user turned off is not listed
/// (`docs/site/specs/acp-onboarding.md` section 8.2, D6).
pub fn listed_in_root_picker(agent: &AgentDescriptor, preset: Option<&str>) -> bool {
    agent.mode != AgentMode::Internal
        && agent.preset.as_deref() == preset
        && !turned_off(agent)
        && (agent.runnable_as_root
            || (matches!(agent.mode, AgentMode::Primary | AgentMode::All)
                && (is_acp(agent) || agent.unavailable.is_some())))
}

/// Whether the user turned `agent` off: its document is disabled, or its
/// ACP agent is turned off on every runtime.
#[must_use]
pub fn turned_off(agent: &AgentDescriptor) -> bool {
    !agent.enabled
        || agent.unavailable.as_ref().is_some_and(|unavailable| {
            matches!(
                unavailable.reason,
                cookie_agent_protocol::AgentUnavailableReason::Disabled
                    | cookie_agent_protocol::AgentUnavailableReason::AcpDeclarationDisabled
            )
        })
}

/// `agents` in picker order: the ones that can run as a root, then the
/// others, each part in the order given; and where the second part starts,
/// where pickers draw their divider.
#[must_use]
pub fn picker_order(agents: Vec<&AgentDescriptor>) -> (Vec<&AgentDescriptor>, usize) {
    let (mut runnable, unavailable): (Vec<_>, Vec<_>) =
        agents.into_iter().partition(|agent| agent.runnable_as_root);
    let divider = runnable.len();
    runnable.extend(unavailable);
    (runnable, divider)
}

/// Why `agent` cannot run, as one line, when the daemon says.
#[must_use]
pub fn unavailable_note(agent: &AgentDescriptor) -> Option<String> {
    agent
        .unavailable
        .as_ref()
        .map(cookie_agent_protocol::AgentUnavailable::summary)
}

/// Whether the user can pick `agent` for a session or draft that attaches
/// `attached`, with `host` as [`placement`] takes it: it runs as a root,
/// and an ACP agent can run where it would, or, with no runtime chosen and
/// none attached offering it, on [`default_runtime`].
pub fn pickable(agent: &AgentDescriptor, attached: &[RuntimeId], host: Option<&RuntimeId>) -> bool {
    agent.runnable_as_root
        && match placement(agent, attached, host) {
            None | Some(AcpPlacement::Runs(_)) => true,
            Some(AcpPlacement::NeedsOneOf(_)) => default_runtime(agent, attached).is_some(),
            Some(AcpPlacement::Unavailable(_)) => false,
        }
}

/// What a picker shows after an ACP agent's description, for a session or
/// draft that attaches `attached`: where it runs, or why it cannot, after
/// the declaration it runs when that is not its own ID. `None` for the
/// daemon's own agents.
pub fn runtime_note(
    agent: &AgentDescriptor,
    attached: &[RuntimeId],
    host: Option<&RuntimeId>,
) -> Option<String> {
    // An agent the daemon says cannot run shows why, ACP or not.
    if !agent.runnable_as_root
        && let Some(note) = unavailable_note(agent)
    {
        return Some(note);
    }
    let note = placement_note(agent, attached, host)?;
    Some(match declaration(agent) {
        Some(declaration) if note.starts_with("on ") => format!("{declaration} {note}"),
        Some(declaration) => format!("{declaration} · {note}"),
        None => note,
    })
}

fn placement_note(
    agent: &AgentDescriptor,
    attached: &[RuntimeId],
    host: Option<&RuntimeId>,
) -> Option<String> {
    Some(match placement(agent, attached, host)? {
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
        AcpPlacement::NeedsOneOf(runtimes) => match default_runtime(agent, attached) {
            Some(runtime) => format!("on {runtime}"),
            None => format!("needs one of {} connected", joined(&runtimes)),
        },
    })
}

/// Why picking `agent` for a session or draft that attaches `attached` is
/// refused, for a status line.
pub fn unpickable_reason(
    agent: &AgentDescriptor,
    attached: &[RuntimeId],
    host: Option<&RuntimeId>,
) -> String {
    if !agent.runnable_as_root
        && let Some(note) = unavailable_note(agent)
    {
        return format!("`{}` cannot run: {note}", agent.id);
    }
    match placement(agent, attached, host) {
        Some(AcpPlacement::NeedsOneOf(runtimes)) => format!(
            "`{}` needs one of these runtimes connected: {}",
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

/// Whether agent command `name` is one of the agent's skills, which Codex
/// lists as commands named `$<skill>` and runs from a `$<skill>` mention.
pub fn is_skill_command(name: &str) -> bool {
    name.starts_with('$')
}

/// Whether agent command `name` is listed for the search `query` (without
/// its leading `/`) once the query matches it: the agent's skills only for
/// a query that starts with `$`, as Codex offers them, so a few dozen of
/// them do not bury the agent's commands.
pub fn command_listed(name: &str, query: &str) -> bool {
    !is_skill_command(name) || query.trim_start().starts_with('$')
}

/// How agent command `name` is written: `/<name>`, or the skill `$<skill>`
/// as it is.
pub fn command_label(name: &str) -> String {
    if is_skill_command(name) {
        name.to_owned()
    } else {
        format!("/{name}")
    }
}

/// The prompt that runs agent command `name` with `input`: `/<name>
/// <input>`, as ACP specifies, or for a skill the mention `$<skill>
/// <input>`, as the agent takes it.
pub fn command_prompt(name: &str, input: &str) -> String {
    let input = input.trim();
    let label = command_label(name);
    if input.is_empty() {
        label
    } else {
        format!("{label} {input}")
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

/// The option that is the agent's mode: the one of category `mode`, else
/// the one with ID `mode`. In an ACP session it is the session's
/// permission mode (`docs/site/specs/acp-permissions.md` section 4): the
/// agent's harness decides what it asks the user.
pub fn mode_option(options: &[AcpConfigOption]) -> Option<&AcpConfigOption> {
    options
        .iter()
        .find(|option| option.category.as_deref() == Some("mode"))
        .or_else(|| options.iter().find(|option| option.id == "mode"))
}

/// The value after `option`'s current one, wrapping, for a key that cycles
/// the agent's mode; `None` when it has fewer than two values.
pub fn next_value(option: &AcpConfigOption) -> Option<&cookie_agent_protocol::AcpConfigValue> {
    if option.values.len() < 2 {
        return None;
    }
    let current = option
        .values
        .iter()
        .position(|value| value.value == option.current_value);
    let next = current.map_or(0, |index| (index + 1) % option.values.len());
    option.values.get(next)
}

/// The name of `option`'s current value, else the value itself.
/// The names of `option`'s values, joined: "Opus, Sonnet, Haiku".
#[must_use]
pub fn option_values_label(option: &AcpConfigOption) -> String {
    option
        .values
        .iter()
        .map(|value| value.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

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
/// `agent` on, whatever its availability ([`placement`], else
/// [`default_runtime`]).
pub fn placed_runtime<'a>(
    agent: &'a AgentDescriptor,
    attached: &[RuntimeId],
    host: Option<&RuntimeId>,
) -> Option<&'a AgentRuntime> {
    let runtime = match placement(agent, attached, host)? {
        AcpPlacement::Runs(runtime) | AcpPlacement::Unavailable(runtime) => runtime,
        AcpPlacement::NeedsOneOf(_) => default_runtime(agent, attached)?,
    };
    offering_runtimes(agent)
        .into_iter()
        .find(|offer| offer.runtime_id == runtime)
}

/// The files ACP agent `agent` takes in a prompt on the runtime a draft or
/// session that attaches `attached` runs it on, as the daemon last saw it
/// start there: what the clients check attachments against, its synthetic
/// model being in no catalog.
pub fn input_media(
    agent: &AgentDescriptor,
    attached: &[RuntimeId],
    host: Option<&RuntimeId>,
) -> Vec<InputMedia> {
    placed_runtime(agent, attached, host)
        .map(|runtime| runtime.input_media.clone())
        .unwrap_or_default()
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
    host: Option<&RuntimeId>,
    choices: &[AcpOptionChoice],
) -> Vec<AcpConfigOption> {
    placed_runtime(agent, attached, host)
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
    host: Option<&RuntimeId>,
) -> Vec<AcpConfigOption> {
    if !view.options.is_empty() {
        return view.options.clone();
    }
    agent
        .map(|agent| draft_options(agent, attached, host, &view.starting_options))
        .unwrap_or_default()
}

/// The runtime a session's ACP agent must run on, as [`placement`] takes
/// `host`: the one its ACP session started on (`view`), while it runs the
/// same agent, else the one its `selection` chose.
pub fn session_host<'a>(
    view: &'a AcpSessionView,
    selection: Option<&'a cookie_agent_protocol::RunSelection>,
) -> Option<&'a RuntimeId> {
    let selection = selection?;
    if view.agent.as_ref() == Some(&selection.agent)
        && let Some(runtime) = &view.runtime_id
    {
        return Some(runtime);
    }
    selection.acp_runtime.as_ref()
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
    #[test]
    fn the_agents_skills_are_listed_for_a_dollar_and_run_as_mentions() {
        assert!(super::command_listed("review", ""));
        assert!(super::command_listed("review", "rev"));
        assert!(!super::command_listed("$imagegen", ""));
        assert!(!super::command_listed("$imagegen", "image"));
        assert!(super::command_listed("$imagegen", "$ima"));
        assert_eq!(
            super::command_prompt("review", " the parser "),
            "/review the parser"
        );
        assert_eq!(
            super::command_prompt("$imagegen", "a cat"),
            "$imagegen a cat"
        );
        assert_eq!(super::command_label("$imagegen"), "$imagegen");
        assert_eq!(super::command_label("status"), "/status");
    }

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
            official: false,
            official_version: None,
            unavailable: None,
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
    fn an_acp_agent_runs_on_its_host_else_the_first_attached_runtime_that_offers_it() {
        let claude = agent(
            "claude",
            &[("labpc", true), ("local", true), ("gpu", false)],
            true,
            AgentMode::All,
        );
        let local = [runtime("global"), runtime("local")];
        assert_eq!(
            placement(&claude, &local, None),
            Some(AcpPlacement::Runs(runtime("local")))
        );
        assert!(pickable(&claude, &local, None));
        // Two attached runtimes offer it: the first in attach order runs it,
        // unless a host says otherwise.
        let both = [runtime("labpc"), runtime("local")];
        assert_eq!(
            placement(&claude, &both, None),
            Some(AcpPlacement::Runs(runtime("labpc")))
        );
        assert_eq!(
            placement(&claude, &both, Some(&runtime("local"))),
            Some(AcpPlacement::Runs(runtime("local")))
        );
        // A chosen runtime runs it even before it is attached.
        assert_eq!(
            placement(&claude, &local, Some(&runtime("labpc"))),
            Some(AcpPlacement::Runs(runtime("labpc")))
        );
        // A host that cannot run it does not fall back to another.
        assert_eq!(
            placement(&claude, &both, Some(&runtime("gpu"))),
            Some(AcpPlacement::Unavailable(runtime("gpu")))
        );
        assert!(!pickable(&claude, &both, Some(&runtime("gpu"))));
        // Without a host, an attached runtime that cannot run it is passed
        // over for one that can.
        let gpu_first = [runtime("gpu"), runtime("local")];
        assert_eq!(
            placement(&claude, &gpu_first, None),
            Some(AcpPlacement::Runs(runtime("local")))
        );
        // No attached runtime offers it: it can still be picked, and runs on
        // the first that can run it, which the daemon attaches.
        let elsewhere = [runtime("global")];
        assert!(matches!(
            placement(&claude, &elsewhere, None),
            Some(AcpPlacement::NeedsOneOf(_))
        ));
        assert!(pickable(&claude, &elsewhere, None));
        assert_eq!(default_runtime(&claude, &elsewhere), Some(runtime("labpc")));
        assert_eq!(default_runtime(&claude, &local), Some(runtime("local")));
        assert_eq!(
            placed_runtime(&claude, &elsewhere, None).map(|offer| offer.runtime_id.clone()),
            Some(runtime("labpc"))
        );
        assert_eq!(runs_on_choices(&claude).len(), 3);
    }

    #[test]
    fn a_session_host_is_where_its_agent_started_else_its_choice() {
        let mut selection = cookie_agent_protocol::RunSelection {
            agent: AgentId::new("claude").unwrap(),
            model: cookie_agent_protocol::acp_model_selection(&AgentId::new("claude").unwrap()),
            preset: None,
            acp_runtime: Some(runtime("labpc")),
        };
        let mut view = AcpSessionView::default();
        assert_eq!(
            session_host(&view, Some(&selection)),
            Some(&runtime("labpc"))
        );
        view.agent = Some(AgentId::new("claude").unwrap());
        view.runtime_id = Some(runtime("local"));
        assert_eq!(
            session_host(&view, Some(&selection)),
            Some(&runtime("local"))
        );
        // Another agent's session says nothing of this one's.
        selection.agent = AgentId::new("codex").unwrap();
        selection.acp_runtime = None;
        assert_eq!(session_host(&view, Some(&selection)), None);
    }

    #[test]
    fn a_document_over_another_declaration_names_it_in_the_note() {
        let mut review = agent("claude-review", &[("local", true)], true, AgentMode::All);
        for offer in review.runtimes.iter_mut().chain(review.runtime.as_mut()) {
            offer.agent = AgentId::new("claude").unwrap();
        }
        assert_eq!(declaration(&review).map(AgentId::as_str), Some("claude"));
        assert_eq!(
            runtime_note(&review, &[runtime("local")], None).as_deref(),
            Some("claude on local")
        );
        // An agent of the declaration's own ID names none.
        let claude = agent("claude", &[("local", true)], true, AgentMode::All);
        assert_eq!(declaration(&claude), None);
    }

    #[test]
    fn an_unavailable_acp_agent_is_listed_but_not_pickable() {
        let away = agent("codex", &[("lab", false)], false, AgentMode::All);
        let lab = [runtime("lab")];
        assert!(listed_in_root_picker(&away, None));
        assert!(!pickable(&away, &lab, None));
        assert_eq!(
            runtime_note(&away, &lab, None).as_deref(),
            Some("on lab · unavailable")
        );
        // A subagent-only ACP agent is never a root choice.
        let sub = agent("reviewer", &[("lab", true)], false, AgentMode::Subagent);
        assert!(!listed_in_root_picker(&sub, None));
        // A daemon agent that cannot run as a root is not listed.
        let daemon = agent("helper", &[], false, AgentMode::All);
        assert!(!listed_in_root_picker(&daemon, None));
        assert_eq!(runtime_note(&daemon, &lab, None), None);
        assert!(!listed_in_root_picker(&away, Some("python")));
    }

    #[test]
    fn an_agent_the_daemon_says_cannot_run_is_listed_last_with_why_unless_turned_off() {
        use cookie_agent_protocol::{
            AgentModelEntryStatus, AgentModelProblem, AgentUnavailable, AgentUnavailableReason,
        };
        let lab = [runtime("lab")];
        let mut no_model = agent("coder", &[], false, AgentMode::All);
        no_model.unavailable = Some(AgentUnavailable {
            reason: AgentUnavailableReason::NoModel,
            target: None,
            entries: vec![AgentModelEntryStatus {
                entry: "${default_model}".into(),
                model: None,
                problem: AgentModelProblem::FieldUnset,
                variant: None,
            }],
        });
        assert!(listed_in_root_picker(&no_model, None));
        assert!(!pickable(&no_model, &lab, None));
        let note = runtime_note(&no_model, &lab, None).unwrap();
        assert!(note.contains("default_model"), "{note}");
        assert!(unpickable_reason(&no_model, &lab, None).contains("cannot run"));

        let mut off = agent("primary", &[], false, AgentMode::Primary);
        off.unavailable = Some(AgentUnavailable {
            reason: AgentUnavailableReason::Disabled,
            target: None,
            entries: Vec::new(),
        });
        assert!(!listed_in_root_picker(&off, None), "turned off: panel only");

        let runs = agent("claude", &[("lab", true)], true, AgentMode::All);
        let (ordered, divider) = picker_order(vec![&no_model, &runs]);
        assert_eq!(ordered[0].id.as_str(), "claude");
        assert_eq!(divider, 1);
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
        let options = draft_options(&claude, &local, None, draft.for_agent(&claude.id));
        assert_eq!(model_label(&options).as_deref(), Some("Opus • Low"));
        // labpc has never started the agent: nothing to offer there.
        assert!(
            draft_options(&claude, &[runtime("labpc")], Some(&runtime("labpc")), &[]).is_empty()
        );
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
        let options = session_options(&view, Some(&claude), &local, None);
        assert_eq!(model_label(&options).as_deref(), Some("Opus • Low"));
        view.options = vec![select(
            "model",
            Some("model"),
            "haiku",
            &[("haiku", "Haiku")],
        )];
        let options = session_options(&view, Some(&claude), &local, None);
        assert_eq!(model_label(&options).as_deref(), Some("Haiku"));
    }
}

/// The server and tool of an ACP agent's call titled as Claude Code titles
/// an MCP tool, `mcp__<server>__<tool>`.
fn mcp_title(title: &str) -> Option<(&str, &str)> {
    title
        .strip_prefix("mcp__")?
        .split_once("__")
        .filter(|(server, tool)| !server.is_empty() && !tool.is_empty())
}

/// What a tool call of `session` belongs to, shown beside it where the
/// user answers for it (`docs/site/specs/acp-permissions.md`, section 4):
/// in a session an ACP agent runs, the server of an MCP tool the agent
/// connected itself, whose name its title leaves out. `None` for anything else.
#[must_use]
pub fn tool_source_label(
    session: &crate::SessionState,
    tool: &crate::ToolCallState,
) -> Option<String> {
    session.acp.agent.as_ref()?;
    match tool
        .presentation
        .tool
        .source
        .as_ref()
        .and_then(cookie_agent_protocol::Open::known)
    {
        Some(cookie_agent_protocol::ToolSource::Acp { .. }) | None => {
            mcp_title(tool.presentation.title.as_str()).map(|(server, _)| server.to_owned())
        }
        Some(_) => None,
    }
}

/// The title a tool call shows where the user answers for it: an ACP
/// agent's call of an MCP tool by the tool's own name, without the
/// `mcp__<server>__` prefix its harness gives it (its server is shown
/// beside it, [`tool_source_label`]).
#[must_use]
pub fn approval_title(title: &str) -> &str {
    mcp_title(title).map_or(title, |(_, tool)| tool)
}

#[cfg(test)]
mod mode_option_tests {
    use cookie_agent_protocol::{AcpConfigOption, AcpConfigOptionKind, AcpConfigValue};

    fn mode(current: &str, values: &[&str]) -> AcpConfigOption {
        AcpConfigOption {
            id: "mode".to_owned(),
            name: "Mode".to_owned(),
            kind: AcpConfigOptionKind::default(),
            description: None,
            category: Some("mode".to_owned()),
            current_value: current.to_owned(),
            values: values
                .iter()
                .map(|value| AcpConfigValue {
                    value: (*value).to_owned(),
                    name: (*value).to_owned(),
                    description: None,
                })
                .collect(),
        }
    }

    #[test]
    fn the_next_mode_wraps_and_needs_two_values() {
        let option = mode("auto", &["default", "auto"]);
        assert_eq!(
            super::next_value(&option).map(|value| value.value.as_str()),
            Some("default")
        );
        let option = mode("default", &["default", "auto"]);
        assert_eq!(
            super::next_value(&option).map(|value| value.value.as_str()),
            Some("auto")
        );
        // An unknown current value starts at the first.
        let option = mode("gone", &["default", "auto"]);
        assert_eq!(
            super::next_value(&option).map(|value| value.value.as_str()),
            Some("default")
        );
        assert!(super::next_value(&mode("auto", &["auto"])).is_none());
        let options = [mode("auto", &["auto", "default"])];
        assert_eq!(
            super::mode_option(&options).map(|option| option.id.as_str()),
            Some("mode")
        );
    }
}

#[cfg(test)]
mod approval_label_tests {
    #[test]
    fn an_mcp_title_drops_its_prefix() {
        assert_eq!(super::approval_title("mcp__exa__web_search"), "web_search");
        assert_eq!(super::approval_title("Run tests"), "Run tests");
        assert_eq!(super::approval_title("mcp____x"), "mcp____x");
        assert_eq!(
            super::mcp_title("mcp__exa__web_search"),
            Some(("exa", "web_search"))
        );
    }
}

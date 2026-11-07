use std::sync::Arc;

use cookie_agent_protocol::{
    InternalAgentKind, RunId, SessionId, SessionOrigin, SessionTitle, SessionTitleChange,
    StoredEvent,
};
use tokio_util::sync::CancellationToken;

use super::{
    Engine, EngineError, Event, FrozenInternalAgentPolicy, InternalAgentExecution,
    InternalAgentHistoryInput,
    helpers::truncate_utf8,
    internal_agents::{expand_internal_prompt, internal_text_history},
};
use crate::{
    policy::{FrozenRunPolicy, policy_from_snapshot},
    runtime_snapshot::PublishedRuntime,
};

impl Engine {
    pub(crate) fn historical_run_runtime(
        &self,
        _run: RunId,
    ) -> Result<Arc<PublishedRuntime>, EngineError> {
        Ok(self.current_runtime())
    }

    pub(crate) fn historical_title_policy(
        &self,
        log: &crate::events::EventLog,
        run: RunId,
    ) -> Result<FrozenRunPolicy, EngineError> {
        let (agent, suffix, internal_agents, preset) = latest_run_policy(log, run)?;
        let runtime = self.historical_run_runtime(run)?;
        let agents = Arc::clone(&runtime.agents);
        let mut policy = policy_from_snapshot(
            agent,
            suffix,
            agents,
            runtime,
            crate::policy::ResultLimits::from_config(&self.inner.config.load().runtime),
            self.inner.config.load().runtime.model_retry,
            // Titles are generated for root sessions only.
            cookie_agent_config::CacheLayer::Root,
        )?;
        policy.preset = preset;
        policy.internal_agents = internal_agents;
        policy.historical_delegation = true;
        Ok(policy)
    }

    pub(super) async fn maybe_generate_session_title(
        &self,
        session: SessionId,
        run: RunId,
        input_through_seq: u64,
        cancellation: &CancellationToken,
        internal_policy: &FrozenInternalAgentPolicy,
    ) -> Result<(), EngineError> {
        let policy = &self.inner.config.load().runtime.session_title;
        if !policy.generate_on_first_turn {
            return Ok(());
        }
        let projection = self.inner.store.get(session)?;
        if matches!(projection.meta.origin, SessionOrigin::Delegated { .. })
            || !projection.automatic_title_eligible
        {
            return Ok(());
        }
        let state = projection.log.state();
        let text = |event: &cookie_agent_protocol::StoredEvent| match &event.payload {
            Event::UserInputSubmitted { content } => content.as_plain_text().into_owned(),
            _ => String::new(),
        };
        let opening = state.opening_inputs();
        let held = opening.len() < crate::events::state::OPENING_INPUTS
            || policy.max_input_messages <= opening.len();
        let (inputs, input) = match state.first_input_of_run(run) {
            // Until a title commits this comes around every turn, so it reads
            // the fold. Asked of an older run, or for more than it keeps, it
            // takes a pass.
            Some(first) if held => (
                opening
                    .iter()
                    .filter(|event| event.seq <= input_through_seq)
                    .take(policy.max_input_messages)
                    .map(|event| text(event))
                    .collect::<Vec<_>>(),
                first.map(|event| text(event)),
            ),
            _ => {
                // The session's opening messages, and the first of the run: a
                // pass that stops once it has them.
                let mut inputs = Vec::new();
                let mut input = None;
                for event in projection.log.history()? {
                    let event = event?;
                    let through = event.seq >= input_through_seq;
                    if matches!(event.payload, Event::UserInputSubmitted { .. }) {
                        if event.seq <= input_through_seq
                            && inputs.len() < policy.max_input_messages
                        {
                            inputs.push(text(&event));
                        }
                        if input.is_none() && event.run_id == Some(run) {
                            input = Some(text(&event));
                        }
                    }
                    if input.is_some() && (through || inputs.len() >= policy.max_input_messages) {
                        break;
                    }
                }
                (inputs, input)
            }
        };
        let input = input.unwrap_or_default();
        let prompt = title_prompt(inputs.iter().map(String::as_str), policy.max_input_messages);
        let generated = self
            .run_internal_history_agent(
                session,
                Some(run),
                InternalAgentKind::SessionTitle,
                internal_policy,
                InternalAgentHistoryInput {
                    history: internal_text_history(
                        // The official `title` document's body is the
                        // whole prompt; the output contract stays in the
                        // request itself.
                        expand_internal_prompt(internal_policy, ""),
                        prompt.clone(),
                    ),
                    summary_source: prompt,
                    tools: Vec::new(),
                    reject_non_text: false,
                },
                InternalAgentExecution {
                    cancellation,
                    actor_direct: false,
                },
            )
            .await;
        let commit = match generated {
            Ok(result) => validate_generated_title(&result.text, policy.max_chars)
                .map(|title| SessionTitleChange::InternalAgentSet {
                    title,
                    invocation_id: result.invocation_id,
                })
                .or_else(|| {
                    policy
                        .fallback_to_input_excerpt
                        .then(|| fallback_title(&input, policy.max_chars))
                        .flatten()
                        .map(|title| SessionTitleChange::FallbackSet { title })
                }),
            Err(_) => policy
                .fallback_to_input_excerpt
                .then(|| fallback_title(&input, policy.max_chars))
                .flatten()
                .map(|title| SessionTitleChange::FallbackSet { title }),
        };
        if let Some(commit) = commit {
            self.append(
                session,
                Some(run),
                super::event_origin("engine:session-title"),
                Event::SessionTitleCommitted {
                    input_through_seq,
                    change: commit,
                },
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn generate_title_after_reset(
        &self,
        session: SessionId,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(session)?;
        if matches!(projection.meta.origin, SessionOrigin::Delegated { .. })
            || !projection.automatic_title_eligible
        {
            return Ok(());
        }
        let Some((run, input_through_seq)) = projection.log.state().latest_turn() else {
            return Ok(());
        };
        let frozen = self.historical_title_policy(&projection.log, run)?;
        let active_index = active_fallback_index(&projection.log, run)?;
        let internal = self.internal_agent_policy(
            InternalAgentKind::SessionTitle,
            &frozen,
            frozen.active_suffix(active_index).first(),
        )?;
        self.maybe_generate_session_title(
            session,
            run,
            input_through_seq,
            &CancellationToken::new(),
            &internal,
        )
        .await
    }
}

fn title_prompt<'a>(
    inputs: impl IntoIterator<Item = &'a str>,
    max_input_messages: usize,
) -> String {
    let inputs = inputs
        .into_iter()
        .take(max_input_messages)
        .enumerate()
        .map(|(index, input)| {
            format!(
                "User message {}: {}",
                index + 1,
                truncate_utf8(input, 8 * 1024)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Return only a short plain-text session title for these opening user messages. No quotes, markup, or explanation.\n{inputs}"
    )
}

type PersistedRunPolicy = (
    cookie_agent_protocol::AgentSnapshot,
    Vec<cookie_agent_protocol::FrozenModelBinding>,
    Vec<cookie_agent_protocol::FrozenInternalAgentDefinition>,
    Option<String>,
);

pub(super) fn latest_run_policy(
    log: &crate::events::EventLog,
    run_id: RunId,
) -> Result<PersistedRunPolicy, EngineError> {
    let policy_of = |event: &StoredEvent| match &event.payload {
        Event::RunStarted {
            agent,
            internal_agents,
            selection,
            ..
        } if event.run_id == Some(run_id) => Some((
            agent.as_ref().clone(),
            internal_agents.clone(),
            selection.preset.clone(),
        )),
        _ => None,
    };
    // The run asked about is nearly always the latest.
    let found = match log.state().run_started(run_id) {
        Some(started) => policy_of(started),
        None => log.history()?.find_map_ok(policy_of)?,
    };
    let (agent, internal_agents, preset) = found.ok_or(EngineError::MissingRun(run_id))?;
    let suffix = crate::policy::decode_bindings(agent.selected_suffix())?;
    Ok((agent, suffix, internal_agents, preset))
}

/// The fallback `run_id` is on: its latest attempt or fallback. A pass over the
/// log only for a run another has since tried a model after.
pub(crate) fn active_fallback_index(
    log: &crate::events::EventLog,
    run_id: RunId,
) -> Result<usize, crate::events::EventLogError> {
    match log.state().latest_fallback() {
        None => Ok(0),
        Some((run, index)) if run == run_id => Ok(index),
        Some(_) => Ok(log
            .history()?
            .last_map_ok(|event| {
                if event.run_id != Some(run_id) {
                    return None;
                }
                match &event.payload {
                    Event::ModelAttemptStarted { fallback_index, .. } => {
                        Some(*fallback_index as usize)
                    }
                    Event::ModelFallback {
                        to_fallback_index, ..
                    } => Some(*to_fallback_index as usize),
                    _ => None,
                }
            })?
            .unwrap_or(0)),
    }
}

pub(super) fn validate_generated_title(value: &str, max_chars: usize) -> Option<SessionTitle> {
    let value = value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .trim_matches(['"', '\'', '`'])
        .trim();
    if value.is_empty() {
        return None;
    }
    let bounded = value.chars().take(max_chars).collect::<String>();
    SessionTitle::new(bounded).ok()
}

pub(super) fn fallback_title(input: &str, max_chars: usize) -> Option<SessionTitle> {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded = normalized.chars().take(max_chars).collect::<String>();
    SessionTitle::new(bounded).ok()
}

pub(super) fn delegated_title(
    description: &str,
    max_chars: usize,
) -> Result<SessionTitle, EngineError> {
    if description.chars().any(char::is_control) {
        return Err(EngineError::ToolFailed(
            "delegate description must be a nonblank control-free session title".into(),
        ));
    }
    let bounded = description.chars().take(max_chars).collect::<String>();
    SessionTitle::new(bounded).map_err(|_| {
        EngineError::ToolFailed(
            "delegate description must be a nonblank control-free session title".into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{delegated_title, title_prompt, validate_generated_title};

    #[test]
    fn validate_generated_title_skips_leading_blank_lines_and_strips_quotes() {
        assert_eq!(
            validate_generated_title("\n\nPineapple Instruction", 80)
                .expect("leading blank lines")
                .as_str(),
            "Pineapple Instruction"
        );
        assert_eq!(
            validate_generated_title("  \n  \"Quoted Title\"  ", 80)
                .expect("quoted title after blanks")
                .as_str(),
            "Quoted Title"
        );
        assert_eq!(
            validate_generated_title("First line\nignored second line", 80)
                .expect("first content line wins")
                .as_str(),
            "First line"
        );
        assert!(validate_generated_title("\n\n  \n", 80).is_none());
        assert!(validate_generated_title("\"\"", 80).is_none());
    }

    #[test]
    fn title_prompt_limits_opening_user_messages() {
        let prompt = title_prompt(
            [
                "first user message",
                "second user message",
                "excluded user message",
            ],
            2,
        );
        assert!(prompt.contains("first user message"));
        assert!(prompt.contains("second user message"));
        assert!(!prompt.contains("excluded user message"));
    }

    #[test]
    fn delegated_title_preserves_text_truncates_by_chars_and_rejects_controls() {
        let title = delegated_title("  Direct title  ", 80).expect("delegated title");
        assert_eq!(title.as_str(), "  Direct title  ");
        assert_eq!(
            delegated_title("éééé", 3)
                .expect("truncated delegated title")
                .as_str(),
            "ééé"
        );
        assert!(delegated_title("invalid\ntitle", 80).is_err());
        assert!(delegated_title("valid prefix\n", 5).is_err());
    }
}

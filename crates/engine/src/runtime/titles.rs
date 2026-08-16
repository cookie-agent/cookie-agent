use std::sync::Arc;

use cookie_agent_protocol::{
    InternalAgentKind, RunId, SessionId, SessionOrigin, SessionTitle, SessionTitleChange,
    StoredEvent,
};
use tokio_util::sync::CancellationToken;

use super::{
    Engine, EngineError, Event, FrozenInternalAgentPolicy, InternalAgentExecution,
    helpers::truncate_utf8,
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
        events: &[StoredEvent],
        run: RunId,
    ) -> Result<FrozenRunPolicy, EngineError> {
        let (agent, suffix) = latest_run_policy(events, run)?;
        let runtime = self.historical_run_runtime(run)?;
        let agents = Arc::clone(&runtime.agents);
        policy_from_snapshot(
            agent,
            suffix,
            agents,
            runtime,
            self.inner.config.runtime.tool_output.max_lines,
            self.inner.config.runtime.tool_output.max_bytes,
            self.inner.config.runtime.prompt_caching.strategy(),
        )
    }

    pub(super) async fn maybe_generate_session_title(
        &self,
        session: SessionId,
        run: RunId,
        input_through_seq: u64,
        cancellation: &CancellationToken,
        internal_policy: &FrozenInternalAgentPolicy,
    ) -> Result<(), EngineError> {
        let policy = &self.inner.config.runtime.session_title;
        if !policy.generate_on_first_turn
            || matches!(
                self.inner.store.get(session)?.meta.origin,
                SessionOrigin::Delegated { .. }
            )
        {
            return Ok(());
        }
        let events = self.inner.store.get(session)?.log.events();
        if !automatic_title_eligible(&events) {
            return Ok(());
        }
        let inputs = events
            .iter()
            .filter(|event| event.seq <= input_through_seq)
            .filter_map(|event| match &event.payload {
                Event::UserInputSubmitted { input } => Some(input),
                _ => None,
            })
            .collect::<Vec<_>>();
        let input = events
            .iter()
            .find_map(|event| match &event.payload {
                Event::UserInputSubmitted { input } if event.run_id == Some(run) => {
                    Some(input.as_str())
                }
                _ => None,
            })
            .unwrap_or_default();
        let prompt = title_prompt(
            inputs.iter().map(|input| input.as_str()),
            policy.max_input_messages,
        );
        let generated = self
            .run_internal_text_agent(
                session,
                Some(run),
                InternalAgentKind::SessionTitle,
                internal_policy,
                prompt,
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
                        .then(|| fallback_title(input, policy.max_chars))
                        .flatten()
                        .map(|title| SessionTitleChange::FallbackSet { title })
                }),
            Err(_) => policy
                .fallback_to_input_excerpt
                .then(|| fallback_title(input, policy.max_chars))
                .flatten()
                .map(|title| SessionTitleChange::FallbackSet { title }),
        };
        if let Some(commit) = commit {
            self.append(
                session,
                Some(run),
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
        if matches!(projection.meta.origin, SessionOrigin::Delegated { .. }) {
            return Ok(());
        }
        let events = projection.log.events();
        if !automatic_title_eligible(&events) {
            return Ok(());
        }
        let Some((run, input_through_seq)) = title_regeneration_target(&events) else {
            return Ok(());
        };
        let frozen = self.historical_title_policy(&events, run)?;
        let active_index = active_fallback_index(&events, run);
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

pub(super) fn latest_run_policy(
    events: &[StoredEvent],
    run_id: RunId,
) -> Result<
    (
        cookie_agent_protocol::AgentSnapshot,
        Vec<cookie_agent_protocol::FrozenModelBinding>,
    ),
    EngineError,
> {
    events
        .iter()
        .find_map(|event| match &event.payload {
            Event::RunStarted {
                agent,
                selected_suffix,
                ..
            } if event.run_id == Some(run_id) => {
                Some((agent.as_ref().clone(), selected_suffix.clone()))
            }
            _ => None,
        })
        .ok_or(EngineError::MissingRun(run_id))
}

pub(crate) fn title_regeneration_target(events: &[StoredEvent]) -> Option<(RunId, u64)> {
    events.iter().rev().find_map(|event| match &event.payload {
        Event::ModelTurnCommitted {
            input_through_seq, ..
        } => event.run_id.map(|run| (run, *input_through_seq)),
        _ => None,
    })
}

pub(crate) fn active_fallback_index(events: &[StoredEvent], run_id: RunId) -> usize {
    events
        .iter()
        .rev()
        .find_map(|event| {
            if event.run_id != Some(run_id) {
                return None;
            }
            match &event.payload {
                Event::ModelAttemptStarted { fallback_index, .. } => Some(*fallback_index as usize),
                Event::ModelFallback {
                    to_fallback_index, ..
                } => Some(*to_fallback_index as usize),
                _ => None,
            }
        })
        .unwrap_or(0)
}

pub(super) fn validate_generated_title(value: &str, max_chars: usize) -> Option<SessionTitle> {
    let value = value
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim();
    if value.is_empty() {
        return None;
    }
    let bounded = value.chars().take(max_chars).collect::<String>();
    SessionTitle::new(bounded).ok()
}

pub(super) fn automatic_title_eligible(events: &[StoredEvent]) -> bool {
    let mut latest_automatic = None;
    let mut latest_user = None;
    for event in events {
        if let Event::SessionTitleCommitted { change, .. } = &event.payload {
            match change {
                SessionTitleChange::InternalAgentSet { .. }
                | SessionTitleChange::FallbackSet { .. } => latest_automatic = Some(event.seq),
                SessionTitleChange::DelegatedSet { .. } => {
                    latest_user = Some((event.seq, false));
                }
                SessionTitleChange::UserSet { .. } | SessionTitleChange::UserClear { .. } => {
                    latest_user = Some((event.seq, false));
                }
                SessionTitleChange::UserReset { .. } => latest_user = Some((event.seq, true)),
            }
        }
    }
    match latest_user {
        Some((_, false)) => false,
        Some((reset_seq, true)) => latest_automatic.is_none_or(|seq| seq < reset_seq),
        None => latest_automatic.is_none(),
    }
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
        return Err(EngineError::MissingTool(
            "delegate description must be a nonblank control-free session title".into(),
        ));
    }
    let bounded = description.chars().take(max_chars).collect::<String>();
    SessionTitle::new(bounded).map_err(|_| {
        EngineError::MissingTool(
            "delegate description must be a nonblank control-free session title".into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{delegated_title, title_prompt};

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

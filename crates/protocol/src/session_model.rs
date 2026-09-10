//! Session-local model continuation derived from existing durable events.
use crate::{EventPayload, ModelFinishReason, ModelSelection, RunId, RunSelection, StoredEvent};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionModelState {
    pub selection: Option<RunSelection>,
    pub successful_fallback: bool,
    run: Option<RunId>,
    chain: Vec<ModelSelection>,
}

impl SessionModelState {
    pub fn from_events(events: &[StoredEvent]) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event.run_id, &event.payload);
        }
        state
    }

    pub fn apply(&mut self, run: Option<RunId>, event: &EventPayload) {
        match event {
            EventPayload::SessionCreated {
                creation_selection, ..
            } => {
                self.selection = Some(creation_selection.clone());
            }
            EventPayload::RunStarted {
                selection,
                selected_suffix,
                ..
            } => {
                self.successful_fallback &= self.selection.as_ref() == Some(selection);
                self.selection = Some(selection.clone());
                self.chain = selected_suffix
                    .iter()
                    .map(|binding| binding.selection.clone())
                    .collect();
                self.run = run;
            }
            EventPayload::ModelTurnCommitted {
                resolved_model,
                turn,
                ..
            } if run.is_some() && run == self.run && successful_finish(&turn.finish_reason) => {
                // Temporary skill overrides outside the frozen chain do not change
                // the session default. ModelFallback alone is only an attempted move.
                if self
                    .chain
                    .iter()
                    .position(|model| *model == resolved_model.selection)
                    .is_some_and(|index| index > 0)
                    && let Some(selection) = &mut self.selection
                {
                    selection.model = resolved_model.selection.clone();
                    self.successful_fallback = true;
                }
            }
            _ => {}
        }
    }

    /// Resolve a stale/default selection against a freshly frozen chain. Exact
    /// model+variant identities, not an index from an earlier configuration, match.
    pub fn continuation(
        &self,
        requested: &RunSelection,
        chain: &[ModelSelection],
        reset: bool,
    ) -> RunSelection {
        if !reset
            && self.successful_fallback
            && let Some(selected) = &self.selection
            && selected.agent == requested.agent
            && selected.preset == requested.preset
            && let Some(requested_index) = chain.iter().position(|model| *model == requested.model)
            && let Some(selected_index) = chain.iter().position(|model| *model == selected.model)
            && requested_index < selected_index
        {
            return selected.clone();
        }
        requested.clone()
    }
}

fn successful_finish(reason: &ModelFinishReason) -> bool {
    match reason {
        // A tool-calling or output-limited turn still establishes a working model.
        ModelFinishReason::Stop | ModelFinishReason::ToolCalls | ModelFinishReason::Length => true,
        ModelFinishReason::ContentFilter | ModelFinishReason::Cancelled | ModelFinishReason::Error
        | ModelFinishReason::Aborted | ModelFinishReason::Timeout | ModelFinishReason::Refused
        // Unknown and vendor-specific reasons are not positive success evidence.
        | ModelFinishReason::Unknown | ModelFinishReason::Other(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(model: &str) -> RunSelection {
        RunSelection {
            agent: "primary".parse().unwrap(),
            preset: None,
            model: ModelSelection {
                model: format!("test/{model}").parse().unwrap(),
                variant: None,
            },
        }
    }

    fn committed(model: &RunSelection, finish_reason: ModelFinishReason) -> EventPayload {
        EventPayload::ModelTurnCommitted {
            attempt_id: crate::AttemptId::new_v7(),
            model_turn_seq: 2,
            input_through_seq: 1,
            resolved_model: crate::ResolvedModelRef {
                selection: model.model.clone(),
                provider_id: "test".parse().unwrap(),
                model_id: model.model.model.model_id().clone(),
                adapter_id: crate::AdaptorId::OpenaiCompatible,
                selection_fingerprint: crate::Sha256Digest::of_bytes(b"fixture"),
            },
            turn: crate::PersistedModelTurn {
                content: vec![],
                provider_options: Default::default(),
                finish_reason,
                usage: Default::default(),
                response_metadata: Default::default(),
                provider_metadata: Default::default(),
                native_replay: None,
            },
            warnings: vec![],
        }
    }

    #[test]
    fn only_successful_finish_reasons_advance_fallback_and_later_failure_keeps_progress() {
        let a = selection("a");
        let b = selection("b");
        let c = selection("c");
        let run = Some(RunId::new_v7());
        let initial = SessionModelState {
            selection: Some(a.clone()),
            run,
            chain: vec![a.model.clone(), b.model.clone(), c.model.clone()],
            successful_fallback: false,
        };
        for (reason, success) in [
            (ModelFinishReason::Stop, true),
            (ModelFinishReason::ToolCalls, true),
            (ModelFinishReason::Length, true),
            (ModelFinishReason::Cancelled, false),
            (ModelFinishReason::Aborted, false),
            (ModelFinishReason::Error, false),
            (ModelFinishReason::Timeout, false),
            (ModelFinishReason::ContentFilter, false),
            (ModelFinishReason::Refused, false),
            (ModelFinishReason::Unknown, false),
            (ModelFinishReason::Other("vendor-specific".into()), false),
        ] {
            let mut state = initial.clone();
            state.apply(run, &committed(&b, reason.clone()));
            assert_eq!(
                state.selection,
                Some(if success { b.clone() } else { a.clone() }),
                "{reason:?}"
            );
            assert_eq!(state.successful_fallback, success, "{reason:?}");
            if !success {
                state.apply(run, &committed(&b, ModelFinishReason::ToolCalls));
                state.apply(run, &committed(&c, reason));
                state.apply(
                    run,
                    &EventPayload::RunFailed {
                        error: crate::SafeErrorMessage::new("later run failure").unwrap(),
                        model_error: None,
                        resolved_model: None,
                    },
                );
                state.apply(run, &EventPayload::RunCancelled { reason: None });
                assert_eq!(state.selection, Some(b.clone()));
                assert!(state.successful_fallback);
                assert_eq!(state.continuation(&a, &initial.chain, false), b);
            }
        }
    }

    #[test]
    fn continuation_uses_exact_identity_and_respects_scope_and_reset() {
        let a = selection("a");
        let b = selection("b");
        let c = selection("c");
        let state = SessionModelState {
            selection: Some(b.clone()),
            successful_fallback: true,
            ..Default::default()
        };
        let chain = vec![a.model.clone(), b.model.clone(), c.model.clone()];
        assert_eq!(state.continuation(&a, &chain, false), b);
        assert_eq!(state.continuation(&a, &chain, true), a);
        assert_eq!(state.continuation(&c, &chain, false), c);
        assert_eq!(
            state.continuation(
                &a,
                &[a.model.clone(), c.model.clone(), b.model.clone()],
                false
            ),
            b
        );
        assert_eq!(
            state.continuation(&a, &[a.model.clone(), c.model], false),
            a
        );
        let mut other = a.clone();
        other.agent = "other".parse().unwrap();
        assert_eq!(state.continuation(&other, &chain, false), other);
        other = a.clone();
        other.preset = Some("python".into());
        assert_eq!(state.continuation(&other, &chain, false), other);
        let mut variant_b = b;
        variant_b.model.variant = Some("fast".parse().unwrap());
        let state = SessionModelState {
            selection: Some(variant_b.clone()),
            successful_fallback: true,
            ..Default::default()
        };
        assert_eq!(state.continuation(&a, &chain, false), a);
        assert_eq!(
            state.continuation(&a, &[a.model.clone(), variant_b.model.clone()], false),
            variant_b
        );
    }

    #[test]
    fn reset_flag_is_optional_and_round_trips_without_changing_events() {
        let wire = serde_json::json!({"session_id":crate::SessionId::new_v7(),"client_run_id":"test","selection":selection("a"),"input":"hello"});
        let mut params: crate::RunStartParams = serde_json::from_value(wire.clone()).unwrap();
        assert!(!params.reset_fallback);
        assert_eq!(serde_json::to_value(&params).unwrap(), wire);
        params.reset_fallback = true;
        assert_eq!(
            serde_json::to_value(params).unwrap()["reset_fallback"],
            true
        );
    }
}

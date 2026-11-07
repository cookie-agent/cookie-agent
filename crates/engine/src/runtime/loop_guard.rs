//! Warns a model that keeps repeating the same tool calls with the same results.
//!
//! A run is looping when its most recent tool calls are consecutive copies of
//! one block of calls (same operations, same results). The guard only adds a
//! warning to the latest result; it never blocks or stops the run.

use std::collections::VecDeque;

use cookie_agent_protocol::{ToolCallId, model_text::LOOP_WARNING_MARKER};

use crate::events::state::{LOOP_WINDOW_CALLS as WINDOW_CALLS, LoopCall, LoopObserved};

/// Longest repeating block, in tool calls, the guard looks for.
const MAX_BLOCK_CALLS: usize = 8;
/// Consecutive copies of one block that count as a loop.
const MIN_REPETITIONS: usize = 3;

/// One call in the window, and what the model saw of it.
struct Call<'a> {
    fingerprint: &'a cookie_agent_protocol::OperationFingerprint,
    observed: LoopObserved,
}

/// Returns the warning to append to `current`'s successful output, or `None`
/// when the run's latest calls are not a loop. `calls` are the run's calls
/// the log folded (`LogState::loop_calls`): those since the latest user input,
/// which resets the history, or the run's start. `current` must be one of
/// them; the window is it and the calls started before it, up to
/// [`WINDOW_CALLS`].
pub(super) fn loop_warning(
    calls: &VecDeque<LoopCall>,
    current: ToolCallId,
    current_output: &str,
) -> Option<String> {
    let at = calls.iter().rposition(|call| call.id == current)?;
    let calls = calls
        .range(at.saturating_sub(WINDOW_CALLS - 1)..=at)
        .map(|call| Call {
            fingerprint: &call.fingerprint,
            observed: if call.id == current {
                LoopObserved::Output(LoopObserved::output_digest(current_output))
            } else {
                call.observed.clone().unwrap_or(LoopObserved::Missing)
            },
        })
        .collect::<Vec<_>>();
    let (block, repetitions) = repeated_block(&calls)?;
    Some(warning_text(block, repetitions))
}

/// Finds the shortest block of calls that the tail of `calls` repeats at least
/// [`MIN_REPETITIONS`] times in a row; returns its length and copy count.
fn repeated_block(calls: &[Call<'_>]) -> Option<(usize, usize)> {
    let last = calls.len().checked_sub(1)?;
    (1..=MAX_BLOCK_CALLS)
        .take_while(|block| calls.len() >= block * MIN_REPETITIONS)
        .find_map(|block| {
            let matching = (0..=last - block)
                .take_while(|offset| {
                    let (call, earlier) = (&calls[last - offset], &calls[last - offset - block]);
                    call.fingerprint == earlier.fingerprint
                        && call.observed != LoopObserved::Missing
                        && call.observed == earlier.observed
                })
                .count();
            let repetitions = matching / block + 1;
            (repetitions >= MIN_REPETITIONS).then_some((block, repetitions))
        })
}

fn warning_text(block: usize, repetitions: usize) -> String {
    let calls = if block == 1 {
        "the same tool call".to_owned()
    } else {
        format!("the same sequence of {block} tool calls")
    };
    format!(
        "{LOOP_WARNING_MARKER}: you have made {calls} {repetitions} times in a row, and every \
         repetition returned identical results. Repeating it again will not produce new \
         information. Work from the results you already have: take a different approach, or \
         finish and report what you found, including anything you could not resolve.\n\
         </system-reminder>"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cookie_agent_protocol::{
        AssistantToolCallRef, EventPayload as Event, OperationFingerprint, PersistedToolResult,
        RunId, SafeCode, SafeDisplayText, SafeToolError, SessionId, Sha256Digest, StoredEvent,
        ToolCallStart, ToolCallTermination, ToolTerminationOutcome,
    };

    use super::*;

    struct Log {
        run: RunId,
        state: crate::events::state::LogState,
        seq: u64,
    }

    impl Log {
        fn new() -> Self {
            let mut log = Self {
                run: RunId::new_v7(),
                state: crate::events::state::LogState::default(),
                seq: 0,
            };
            log.push(Event::RunStarted {
                client_run_id: cookie_agent_protocol::ClientRunId::new("loop").unwrap(),
                selection: crate::test_support::run_selection("test"),
                revisions: None,
                internal_agents: crate::test_support::internal_agent_definitions(),
                agent: Box::new(crate::test_support::agent_snapshot(
                    "test",
                    cookie_agent_protocol::AgentMode::Primary,
                )),
                input_through_seq: 0,
            });
            log
        }

        fn push(&mut self, payload: Event) {
            self.seq += 1;
            self.state.observe(&Arc::new(StoredEvent {
                engine_version: None,
                origin: None,
                session_id: SessionId::new_v7(),
                run_id: Some(self.run),
                seq: self.seq,
                timestamp: jiff::Timestamp::from_second(i64::try_from(self.seq).unwrap()).unwrap(),
                payload,
            }));
        }

        fn warning(&self, current: ToolCallId, output: &str) -> Option<String> {
            loop_warning(self.state.loop_calls(self.run)?, current, output)
        }

        fn start(&mut self, operation: &str) -> ToolCallId {
            let id = ToolCallId::new_v7();
            self.push(Event::ToolCallStarted {
                start: ToolCallStart {
                    background: false,
                    tool_call_id: id,
                    output: Default::default(),
                    owner: owner(),
                    presentation: crate::test_support::base_presentation(
                        cookie_agent_runtime_api::tool_title_only("bash"),
                    ),
                    operation_fingerprint: fingerprint(operation),
                },
            });
            id
        }

        fn complete(&mut self, id: ToolCallId, output: &str) {
            self.push(Event::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id: id,
                    owner: owner(),
                    outcome: ToolTerminationOutcome::Completed,
                    result: Some(result(output)),
                    error: None,
                    presentation: None,
                },
            });
        }

        /// Runs one call through the guard as the engine does: start it,
        /// check it, then commit its output with any warning appended.
        fn call(&mut self, operation: &str, output: &str) -> Option<String> {
            let id = self.start(operation);
            let warning = self.warning(id, output);
            self.complete(id, &format!("{output}{}", warning.as_deref().unwrap_or("")));
            warning
        }
    }

    fn fingerprint(operation: &str) -> OperationFingerprint {
        serde_json::from_value(serde_json::json!({
            "digest": Sha256Digest::of_bytes(operation.as_bytes()),
        }))
        .unwrap()
    }

    fn owner() -> AssistantToolCallRef {
        AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: cookie_agent_protocol::ModelCallId::new("call").unwrap(),
            provider_item_id: None,
        }
    }

    fn result(output: &str) -> PersistedToolResult {
        PersistedToolResult {
            title: SafeDisplayText::new("Bash").unwrap(),
            output: output.to_owned(),
            display: Vec::new(),
            background: None,
            retained_output: None,
            metadata: serde_json::Value::Null,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
            loaded_tools: Vec::new(),
        }
    }

    #[test]
    fn warns_on_the_third_identical_call() {
        let mut log = Log::new();
        assert!(log.call("ls", "a b").is_none());
        assert!(log.call("ls", "a b").is_none());
        let warning = log.call("ls", "a b").expect("third repeat warns");
        assert!(warning.contains("the same tool call 3 times"));
        let warning = log.call("ls", "a b").expect("warnings keep counting");
        assert!(warning.contains("the same tool call 4 times"));
    }

    #[test]
    fn warns_on_an_alternating_pair() {
        let mut log = Log::new();
        for _ in 0..2 {
            assert!(log.call("a", "1").is_none());
            assert!(log.call("b", "2").is_none());
        }
        assert!(log.call("a", "1").is_none());
        let warning = log.call("b", "2").expect("third pair warns");
        assert!(warning.contains("the same sequence of 2 tool calls 3 times"));
    }

    #[test]
    fn changing_output_is_progress() {
        let mut log = Log::new();
        for attempt in 0..6 {
            assert!(
                log.call("cargo test", &format!("{attempt} failed"))
                    .is_none()
            );
        }
    }

    #[test]
    fn a_different_call_breaks_the_repetition() {
        let mut log = Log::new();
        log.call("ls", "a");
        log.call("ls", "a");
        log.call("cat a", "text");
        assert!(log.call("ls", "a").is_none());
    }

    #[test]
    fn user_input_resets_the_history() {
        let mut log = Log::new();
        log.call("ls", "a");
        log.call("ls", "a");
        log.push(Event::UserInputApplied { user_input_seq: 1 });
        assert!(log.call("ls", "a").is_none());
    }

    #[test]
    fn repeated_failures_count_as_repetitions() {
        let mut log = Log::new();
        for _ in 0..2 {
            let id = log.start("ls");
            log.push(Event::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id: id,
                    owner: owner(),
                    outcome: ToolTerminationOutcome::Failed,
                    result: None,
                    error: Some(SafeToolError {
                        code: SafeCode::new("execution_failed").unwrap(),
                        message: cookie_agent_protocol::diagnostics::headline("no such file"),
                    }),
                    presentation: None,
                },
            });
        }
        assert!(
            log.call("ls", "a").is_none(),
            "a success differs from failures"
        );
    }

    #[test]
    fn the_count_is_bounded_by_the_window_and_the_fold_stays_bounded() {
        let mut log = Log::new();
        let mut last = None;
        for _ in 0..WINDOW_CALLS + 6 {
            last = log.call("ls", "a b");
        }
        assert!(
            last.expect("loop warns")
                .contains(&format!("the same tool call {WINDOW_CALLS} times"))
        );
        assert!(log.state.loop_calls(log.run).unwrap().len() < WINDOW_CALLS);
    }

    #[test]
    fn calls_started_after_the_current_one_are_not_compared() {
        let mut log = Log::new();
        log.call("ls", "a");
        log.call("ls", "a");
        let current = log.start("ls");
        let later = log.start("cat a");
        log.complete(later, "text");
        let warning = log
            .warning(current, "a")
            .expect("the current call closes the loop");
        assert!(warning.contains("the same tool call 3 times"));
    }
}

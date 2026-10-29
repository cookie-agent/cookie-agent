//! Warns a model that keeps repeating the same tool calls with the same results.
//!
//! A run is looping when its most recent tool calls are consecutive copies of
//! one block of calls (same operations, same results). The guard only adds a
//! warning to the latest result; it never blocks or stops the run.

use std::{
    cell::OnceCell,
    collections::HashMap,
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    OperationFingerprint, RunId, SafeToolError, Sha256Digest, StoredEvent, ToolCallId,
    ToolCallTermination, ToolTerminationOutcome, model_text::LOOP_WARNING_MARKER,
};

use super::Event;

/// Longest repeating block, in tool calls, the guard looks for.
const MAX_BLOCK_CALLS: usize = 8;
/// Consecutive copies of one block that count as a loop.
const MIN_REPETITIONS: usize = 3;
/// Most recent calls compared, which bounds the hashing done per call.
const WINDOW_CALLS: usize = 64;
/// Output digests of a run's recent calls, so each output is hashed once
/// however many later checks compare it. Holds at most one window of calls.
#[derive(Debug, Default)]
pub(super) struct OutputDigests(Mutex<HashMap<ToolCallId, Sha256Digest>>);

/// What the model saw from one call.
#[derive(PartialEq)]
enum Observed {
    Output(Sha256Digest),
    Error(SafeToolError),
    Missing,
}

/// One call in the window; what it observed is resolved only when a
/// comparison reaches it.
struct Call<'a> {
    id: ToolCallId,
    fingerprint: &'a OperationFingerprint,
    source: Source<'a>,
    observed: OnceCell<Observed>,
}

enum Source<'a> {
    Current(&'a str),
    Terminated(Option<&'a ToolCallTermination>),
}

impl Call<'_> {
    fn observed(&self, digests: &mut HashMap<ToolCallId, Sha256Digest>) -> &Observed {
        self.observed.get_or_init(|| {
            let output = match self.source {
                Source::Current(output) => output,
                Source::Terminated(None) => return Observed::Missing,
                Source::Terminated(Some(termination)) => match (
                    &termination.outcome,
                    &termination.result,
                    &termination.error,
                ) {
                    (ToolTerminationOutcome::Completed, Some(result), _) => &result.output,
                    (_, _, Some(error)) => return Observed::Error(error.clone()),
                    _ => return Observed::Missing,
                },
            };
            Observed::Output(
                digests
                    .entry(self.id)
                    .or_insert_with(|| own_output_digest(output))
                    .clone(),
            )
        })
    }
}

/// Returns the warning to append to `current`'s successful output, or `None`
/// when the run's latest calls are not a loop. `current` must already have a
/// `ToolCallStarted` event in `events`; user input resets the history.
pub(super) fn loop_warning(
    events: &[Arc<StoredEvent>],
    run: RunId,
    current: ToolCallId,
    current_output: &str,
    digests: &OutputDigests,
) -> Option<String> {
    // Walk back from the tip only as far as the window reaches: to the
    // current call and the calls started before it, stopping at the user
    // input that resets the history or at the run's start. A termination
    // always follows its start, so every window call's result is seen first.
    let mut window = Vec::<(ToolCallId, &OperationFingerprint)>::new();
    let mut terminated = HashMap::<ToolCallId, &ToolCallTermination>::new();
    let mut found_current = false;
    for event in events
        .iter()
        .rev()
        .filter(|event| event.run_id == Some(run))
    {
        match &event.payload {
            Event::UserInputSubmitted { .. }
            | Event::UserInputApplied { .. }
            | Event::RunStarted { .. } => break,
            Event::ToolCallStarted { start } => {
                found_current |= start.tool_call_id == current;
                if found_current {
                    window.push((start.tool_call_id, &start.operation_fingerprint));
                    if window.len() == WINDOW_CALLS {
                        break;
                    }
                }
            }
            Event::ToolCallTerminated { termination } => {
                terminated
                    .entry(termination.tool_call_id)
                    .or_insert(termination);
            }
            _ => {}
        }
    }
    if !found_current {
        return None;
    }
    window.reverse();
    let calls = window
        .into_iter()
        .map(|(id, fingerprint)| Call {
            id,
            fingerprint,
            source: if id == current {
                Source::Current(current_output)
            } else {
                Source::Terminated(terminated.get(&id).copied())
            },
            observed: OnceCell::new(),
        })
        .collect::<Vec<_>>();
    let mut digests = digests.0.lock().unwrap_or_else(|e| e.into_inner());
    let found = repeated_block(&calls, &mut digests);
    // Only calls still inside the window can be compared again.
    digests.retain(|id, _| calls.iter().any(|call| call.id == *id));
    let (block, repetitions) = found?;
    Some(warning_text(block, repetitions))
}

fn own_output_digest(output: &str) -> Sha256Digest {
    let own_output = cookie_agent_protocol::model_text::without_loop_warning(output);
    Sha256Digest::of_bytes(own_output.as_bytes())
}

/// Finds the shortest block of calls that the tail of `calls` repeats at least
/// [`MIN_REPETITIONS`] times in a row; returns its length and copy count.
fn repeated_block(
    calls: &[Call<'_>],
    digests: &mut HashMap<ToolCallId, Sha256Digest>,
) -> Option<(usize, usize)> {
    let last = calls.len().checked_sub(1)?;
    (1..=MAX_BLOCK_CALLS)
        .take_while(|block| calls.len() >= block * MIN_REPETITIONS)
        .find_map(|block| {
            let matching = (0..=last - block)
                .take_while(|offset| {
                    let (call, earlier) = (&calls[last - offset], &calls[last - offset - block]);
                    call.fingerprint == earlier.fingerprint && {
                        let observed = call.observed(digests);
                        *observed != Observed::Missing && observed == earlier.observed(digests)
                    }
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
    use cookie_agent_protocol::{
        AssistantToolCallRef, PersistedToolResult, SafeCode, SafeDisplayText, SessionId,
        ToolCallStart,
    };

    use super::*;

    struct Log {
        run: RunId,
        events: Vec<Arc<StoredEvent>>,
        digests: OutputDigests,
    }

    impl Log {
        fn new() -> Self {
            Self {
                run: RunId::new_v7(),
                events: Vec::new(),
                digests: OutputDigests::default(),
            }
        }

        fn push(&mut self, payload: Event) {
            let seq = self.events.len() as u64 + 1;
            self.events.push(Arc::new(StoredEvent {
                engine_version: None,
                origin: None,
                session_id: SessionId::new_v7(),
                run_id: Some(self.run),
                seq,
                timestamp: jiff::Timestamp::from_second(i64::try_from(seq).unwrap()).unwrap(),
                payload,
            }));
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
                },
            });
        }

        /// Runs one call through the guard as the engine does: start it,
        /// check it, then commit its output with any warning appended.
        fn call(&mut self, operation: &str, output: &str) -> Option<String> {
            let id = self.start(operation);
            let warning = loop_warning(&self.events, self.run, id, output, &self.digests);
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
                },
            });
        }
        assert!(
            log.call("ls", "a").is_none(),
            "a success differs from failures"
        );
    }

    #[test]
    fn the_count_is_bounded_by_the_window_and_digests_stay_bounded() {
        let mut log = Log::new();
        let mut last = None;
        for _ in 0..WINDOW_CALLS + 6 {
            last = log.call("ls", "a b");
        }
        assert!(
            last.expect("loop warns")
                .contains(&format!("the same tool call {WINDOW_CALLS} times"))
        );
        assert!(log.digests.0.lock().unwrap().len() <= WINDOW_CALLS);
    }

    #[test]
    fn calls_started_after_the_current_one_are_not_compared() {
        let mut log = Log::new();
        log.call("ls", "a");
        log.call("ls", "a");
        let current = log.start("ls");
        let later = log.start("cat a");
        log.complete(later, "text");
        let warning = loop_warning(&log.events, log.run, current, "a", &log.digests)
            .expect("the current call closes the loop");
        assert!(warning.contains("the same tool call 3 times"));
    }
}

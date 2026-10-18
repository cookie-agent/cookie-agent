//! Live-only output still in flight, kept per session so a tail that
//! registers mid-stream begins with what was already streamed rather than
//! a fragment. Each piece leaves when the durable event that supersedes it
//! is published: a committed or abandoned attempt, a terminated tool call,
//! a finished internal agent, or the run's end.

use cookie_agent_protocol::{
    AttemptId, DisplayDelta, EventPayload, InternalAgentInvocationId, LiveDisplay, Open, RunId,
    SafeDisplayText, ToolCallId,
};

#[derive(Debug, Default)]
pub(super) struct InFlightOutput {
    attempts: Vec<InFlightAttempt>,
    tools: Vec<InFlightTool>,
    internal_agents: Vec<InFlightInternalAgent>,
    /// The `native_compaction_started` of a provider call still running.
    native_compaction: Option<(Option<RunId>, EventPayload)>,
}

#[derive(Debug)]
struct InFlightAttempt {
    run_id: Option<RunId>,
    attempt_id: AttemptId,
    /// The attempt's output in arrival order, consecutive deltas of one kind
    /// merged: `true` marks reasoning. Replaying the runs in order rebuilds
    /// the same parts a live reader built.
    chunks: Vec<(bool, String)>,
}

#[derive(Debug)]
struct InFlightTool {
    run_id: Option<RunId>,
    tool_call_id: ToolCallId,
    message: SafeDisplayText,
    /// The live display so far, bounded like a reader bounds it.
    display: LiveDisplay,
}

#[derive(Debug)]
struct InFlightInternalAgent {
    run_id: Option<RunId>,
    invocation_id: InternalAgentInvocationId,
    text: String,
}

impl InFlightOutput {
    /// Records one live-only output.
    pub(super) fn record(&mut self, run_id: Option<RunId>, payload: &EventPayload) {
        match payload {
            EventPayload::TextDelta { attempt_id, text } => {
                self.append_chunk(run_id, *attempt_id, false, text);
            }
            EventPayload::ReasoningDelta { attempt_id, text } => {
                self.append_chunk(run_id, *attempt_id, true, text);
            }
            EventPayload::ToolCallProgress {
                tool_call_id,
                message,
                display,
            } => {
                let tool = match self
                    .tools
                    .iter_mut()
                    .position(|tool| tool.tool_call_id == *tool_call_id)
                {
                    Some(index) => &mut self.tools[index],
                    None => {
                        self.tools.push(InFlightTool {
                            run_id,
                            tool_call_id: *tool_call_id,
                            message: message.clone(),
                            display: LiveDisplay::default(),
                        });
                        self.tools.last_mut().expect("just pushed")
                    }
                };
                tool.message = message.clone();
                // A delta that does not apply, or of a kind this build does
                // not know, changes nothing, as for any reader.
                if let Some(Open::Known(delta)) = display {
                    let _ = tool.display.admit(delta.clone());
                }
            }
            EventPayload::InternalAgentTextDelta {
                invocation_id,
                text,
            } => match self
                .internal_agents
                .iter_mut()
                .find(|agent| agent.invocation_id == *invocation_id)
            {
                Some(agent) => agent.text.push_str(text),
                None => self.internal_agents.push(InFlightInternalAgent {
                    run_id,
                    invocation_id: *invocation_id,
                    text: text.clone(),
                }),
            },
            EventPayload::NativeCompactionStarted { .. } => {
                self.native_compaction = Some((run_id, payload.clone()));
            }
            EventPayload::NativeCompactionFinished { .. } => self.native_compaction = None,
            _ => {}
        }
    }

    /// Drops what a durable event supersedes.
    pub(super) fn settle(&mut self, run_id: Option<RunId>, payload: &EventPayload) {
        match payload {
            EventPayload::ModelTurnCommitted { attempt_id, .. }
            | EventPayload::AttemptAbandoned { attempt_id, .. } => {
                self.attempts
                    .retain(|attempt| attempt.attempt_id != *attempt_id);
            }
            EventPayload::ToolCallTerminated { termination } => {
                self.tools
                    .retain(|tool| tool.tool_call_id != termination.tool_call_id);
            }
            EventPayload::InternalAgentFinished { invocation_id, .. } => {
                self.internal_agents
                    .retain(|agent| agent.invocation_id != *invocation_id);
            }
            EventPayload::RunFinished { .. } => {
                if let Some(run_id) = run_id {
                    self.end_run(run_id);
                }
            }
            _ => {}
        }
    }

    /// Everything in flight as live-only output, in the order a live reader
    /// received it, each attempt's runs coalesced.
    pub(super) fn replay(&self) -> Vec<(Option<RunId>, EventPayload)> {
        let mut replay = Vec::new();
        for attempt in &self.attempts {
            for (reasoning, text) in &attempt.chunks {
                let payload = if *reasoning {
                    EventPayload::ReasoningDelta {
                        attempt_id: attempt.attempt_id,
                        text: text.clone(),
                    }
                } else {
                    EventPayload::TextDelta {
                        attempt_id: attempt.attempt_id,
                        text: text.clone(),
                    }
                };
                replay.push((attempt.run_id, payload));
            }
        }
        for tool in &self.tools {
            replay.push((
                tool.run_id,
                EventPayload::ToolCallProgress {
                    tool_call_id: tool.tool_call_id,
                    message: tool.message.clone(),
                    // The whole live display so far, as one replacement.
                    display: (!tool.display.is_empty()).then(|| {
                        Open::Known(DisplayDelta::Replace {
                            blocks: tool.display.blocks().to_vec(),
                        })
                    }),
                },
            ));
        }
        if let Some((run_id, started)) = &self.native_compaction {
            replay.push((*run_id, started.clone()));
        }
        for agent in &self.internal_agents {
            replay.push((
                agent.run_id,
                EventPayload::InternalAgentTextDelta {
                    invocation_id: agent.invocation_id,
                    text: agent.text.clone(),
                },
            ));
        }
        replay
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    fn end_run(&mut self, run_id: RunId) {
        let other_run = |owner: &Option<RunId>| *owner != Some(run_id);
        self.attempts.retain(|attempt| other_run(&attempt.run_id));
        self.tools.retain(|tool| other_run(&tool.run_id));
        self.internal_agents
            .retain(|agent| other_run(&agent.run_id));
        if self
            .native_compaction
            .as_ref()
            .is_some_and(|(owner, _)| !other_run(owner))
        {
            self.native_compaction = None;
        }
    }

    fn append_chunk(
        &mut self,
        run_id: Option<RunId>,
        attempt_id: AttemptId,
        reasoning: bool,
        text: &str,
    ) {
        let attempt = match self
            .attempts
            .iter_mut()
            .position(|attempt| attempt.attempt_id == attempt_id)
        {
            Some(index) => &mut self.attempts[index],
            None => {
                self.attempts.push(InFlightAttempt {
                    run_id,
                    attempt_id,
                    chunks: Vec::new(),
                });
                self.attempts.last_mut().expect("just pushed")
            }
        };
        match attempt.chunks.last_mut() {
            Some((kind, merged)) if *kind == reasoning => merged.push_str(text),
            _ => attempt.chunks.push((reasoning, text.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AttemptId, DisplayDelta, EventPayload, Open, RunId, SafeDisplayText, ToolCallId,
    };

    use super::InFlightOutput;

    #[test]
    fn replay_coalesces_each_kind_run_and_settles_on_durable_events() {
        let run = RunId::new_v7();
        let attempt = AttemptId::new_v7();
        let mut in_flight = InFlightOutput::default();
        for (reasoning, text) in [
            (true, "think "),
            (true, "more"),
            (false, "Hel"),
            (false, "lo"),
        ] {
            let payload = if reasoning {
                EventPayload::ReasoningDelta {
                    attempt_id: attempt,
                    text: text.into(),
                }
            } else {
                EventPayload::TextDelta {
                    attempt_id: attempt,
                    text: text.into(),
                }
            };
            in_flight.record(Some(run), &payload);
        }
        let tool = ToolCallId::new_v7();
        for chunk in ["line 1\n", "line 2\n"] {
            in_flight.record(
                Some(run),
                &EventPayload::ToolCallProgress {
                    tool_call_id: tool,
                    message: SafeDisplayText::new("running").unwrap(),
                    display: Some(Open::Known(DisplayDelta::Append {
                        index: 0,
                        text: chunk.into(),
                        stream: None,
                    })),
                },
            );
        }
        assert_eq!(
            in_flight.replay(),
            [
                (
                    Some(run),
                    EventPayload::ReasoningDelta {
                        attempt_id: attempt,
                        text: "think more".into(),
                    }
                ),
                (
                    Some(run),
                    EventPayload::TextDelta {
                        attempt_id: attempt,
                        text: "Hello".into(),
                    }
                ),
                (
                    Some(run),
                    EventPayload::ToolCallProgress {
                        tool_call_id: tool,
                        message: SafeDisplayText::new("running").unwrap(),
                        display: Some(Open::Known(DisplayDelta::Replace {
                            blocks: vec![Open::Known(cookie_agent_protocol::DisplayBlock::text(
                                "line 1\nline 2\n"
                            ))],
                        })),
                    }
                ),
            ]
        );
        in_flight.settle(
            Some(run),
            &EventPayload::AttemptAbandoned {
                attempt_id: attempt,
                model_error: None,
            },
        );
        assert_eq!(in_flight.replay().len(), 1);
        in_flight.settle(
            Some(run),
            &EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Cancelled {
                reason: None,
            }),
        );
        assert!(in_flight.replay().is_empty());
    }
}

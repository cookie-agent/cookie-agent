//! One run as a client that watches it to its end sees it: which events
//! are the run's own, how many model turns it committed, the tool calls it
//! started and ended, what went wrong on the way, and how it ended.
//!
//! The headless `cookie run` renders its output from a [`RunWatch`]; the
//! reducer's run-end row reads the same [`RunEnd`], so every client tells a
//! run's end alike.

use std::collections::HashMap;

use cookie_agent_protocol::{
    ApprovalFinalization, ApprovalReasonCode, DiagnosticKind, DiagnosticSource, EventPayload,
    InternalAgentBackend, InternalAgentFailure, InternalAgentKind, InternalAgentOutcome,
    ModelErrorSummary, Open, ResolvedModelRef, RunId, RunOutcome, SessionId, StoredEvent,
    ToolCallId, ToolCallStart, ToolCallTermination,
};

/// How a run ended. An outcome this build does not know reads as
/// interrupted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEndStatus {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

/// A run's end as its `run_finished` outcome tells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunEnd {
    pub status: RunEndStatus,
    /// The reply a completed run left, if any.
    pub final_text: Option<String>,
    /// Why the run did not complete: a failed run's error with the model
    /// failure behind it, else the cancel or interrupt reason it gave.
    pub reason: Option<String>,
}

impl RunEnd {
    /// The end `outcome` describes.
    pub fn of(outcome: &Open<RunOutcome>) -> Self {
        let (status, final_text, reason) = match outcome.known() {
            Some(RunOutcome::Completed { final_text }) => {
                (RunEndStatus::Completed, final_text.clone(), None)
            }
            Some(RunOutcome::Failed {
                error,
                model_error,
                resolved_model,
            }) => (
                RunEndStatus::Failed,
                None,
                Some(cookie_agent_protocol::diagnostics::run_error(
                    error,
                    model_error.as_ref(),
                    resolved_model.as_ref(),
                )),
            ),
            Some(RunOutcome::Cancelled { reason }) => (
                RunEndStatus::Cancelled,
                None,
                reason.as_ref().map(ToString::to_string),
            ),
            Some(RunOutcome::Interrupted { reason }) => (
                RunEndStatus::Interrupted,
                None,
                reason.as_ref().map(ToString::to_string),
            ),
            None => (RunEndStatus::Interrupted, None, None),
        };
        Self {
            status,
            final_text,
            reason,
        }
    }
}

/// What one event means to the watcher of a run ([`RunWatch::observe`]).
#[derive(Debug)]
pub enum RunNote<'a> {
    /// Another session's or another run's event: the watcher shows nothing
    /// of it.
    Foreign,
    /// The run started a tool call.
    ToolStarted(&'a ToolCallStart),
    /// The run committed a model turn, which [`RunWatch::turns`] counts.
    TurnCommitted,
    /// An `auto_approve(N)` budget ran out and rejected one of the run's
    /// approvals: nothing approves the rest either.
    AutoApproveRejected,
    /// A tool call ended; `title` is the one it started under, when the
    /// watch saw it start.
    ToolTerminated {
        termination: &'a ToolCallTermination,
        title: Option<String>,
    },
    /// An internal agent the run started failed.
    InternalAgentFailed {
        kind: InternalAgentKind,
        failure: &'a InternalAgentFailure,
    },
    /// An internal agent moved to its next backend after `attempts` on
    /// `from` failed.
    InternalAgentFallback {
        kind: InternalAgentKind,
        from: &'a InternalAgentBackend,
        to: &'a InternalAgentBackend,
        attempts: u32,
        failure: &'a InternalAgentFailure,
    },
    /// The run moved to its next model after `from` failed.
    ModelFallback {
        from: &'a ResolvedModelRef,
        error: &'a ModelErrorSummary,
    },
    /// A diagnostic of the run, or of the session outside any run.
    Diagnostic {
        source: &'a DiagnosticSource,
        kind: &'a DiagnosticKind,
        message: &'a str,
    },
    /// The run ended. Its later events are its own still, but none comes.
    Finished(RunEnd),
    /// One of the run's events that changes nothing a watcher shows.
    Other,
}

/// The watch over one run of one session: feed it each durable event in
/// order with [`Self::observe`].
#[derive(Clone, Debug)]
pub struct RunWatch {
    session_id: SessionId,
    run_id: RunId,
    turns: u32,
    tool_titles: HashMap<ToolCallId, String>,
}

impl RunWatch {
    pub fn new(session_id: SessionId, run_id: RunId) -> Self {
        Self {
            session_id,
            run_id,
            turns: 0,
            tool_titles: HashMap::new(),
        }
    }

    /// The model turns the run committed so far.
    pub fn turns(&self) -> u32 {
        self.turns
    }

    /// Whether `event` is the run's own: the run's events, and the
    /// diagnostics its session reports outside any run.
    pub fn is_own(&self, event: &StoredEvent) -> bool {
        let session_diagnostic =
            event.run_id.is_none() && matches!(&event.payload, EventPayload::Diagnostic { .. });
        event.session_id == self.session_id
            && (event.run_id == Some(self.run_id) || session_diagnostic)
    }

    /// What `event` means to the run's watcher, recording what later events
    /// need: the turn count and each running call's title.
    pub fn observe<'a>(&mut self, event: &'a StoredEvent) -> RunNote<'a> {
        if !self.is_own(event) {
            return RunNote::Foreign;
        }
        match &event.payload {
            EventPayload::ToolCallStarted { start } => {
                self.tool_titles
                    .insert(start.tool_call_id, start.presentation.title.to_string());
                RunNote::ToolStarted(start)
            }
            EventPayload::ModelTurnCommitted { .. } => {
                self.turns = self.turns.saturating_add(1);
                RunNote::TurnCommitted
            }
            EventPayload::ApprovalFinalized {
                outcome: Open::Known(ApprovalFinalization::Decided { decision }),
                ..
            } if decision.reason_code == ApprovalReasonCode::AutoApproveNRejected => {
                RunNote::AutoApproveRejected
            }
            EventPayload::RunFinished { outcome } => RunNote::Finished(RunEnd::of(outcome)),
            EventPayload::ToolCallTerminated { termination } => RunNote::ToolTerminated {
                termination,
                title: self.tool_titles.remove(&termination.tool_call_id),
            },
            EventPayload::InternalAgentFinished {
                kind,
                outcome: Open::Known(InternalAgentOutcome::Failed { failure, .. }),
                ..
            } => RunNote::InternalAgentFailed {
                kind: *kind,
                failure,
            },
            EventPayload::InternalAgentFallback {
                kind,
                from,
                to,
                attempts,
                failure,
                ..
            } => RunNote::InternalAgentFallback {
                kind: *kind,
                from,
                to,
                attempts: *attempts,
                failure,
            },
            EventPayload::ModelFallback { from, error, .. } => {
                RunNote::ModelFallback { from, error }
            }
            EventPayload::Diagnostic {
                source,
                kind,
                message,
                ..
            } => RunNote::Diagnostic {
                source,
                kind,
                message,
            },
            _ => RunNote::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        DiagnosticSource, EventPayload, Open, PluginDiagnosticKind, RunId, RunOutcome, SessionId,
        StoredEvent,
    };

    use super::{RunEnd, RunEndStatus, RunNote, RunWatch};

    fn event(session_id: SessionId, run_id: Option<RunId>, payload: EventPayload) -> StoredEvent {
        StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id,
            seq: 1,
            timestamp: "2026-01-01T00:00:00Z".parse().expect("timestamp"),
            payload,
        }
    }

    fn diagnostic() -> EventPayload {
        EventPayload::Diagnostic {
            source: DiagnosticSource::plugin("review").expect("source"),
            kind: PluginDiagnosticKind::HookBlocked.label(),
            message: "useful diagnostic".into(),
            count: None,
            detail: None,
        }
    }

    #[test]
    fn session_diagnostics_do_not_admit_other_sessions_or_runs() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let watch = RunWatch::new(session_id, run_id);
        assert!(watch.is_own(&event(session_id, None, diagnostic())));
        assert!(!watch.is_own(&event(SessionId::new_v7(), None, diagnostic())));
        assert!(!watch.is_own(&event(session_id, Some(RunId::new_v7()), diagnostic())));
        assert!(watch.is_own(&event(session_id, Some(run_id), diagnostic())));
        // Only a diagnostic belongs to the run from outside every run.
        let finished = EventPayload::run_finished(RunOutcome::Completed { final_text: None });
        assert!(!watch.is_own(&event(session_id, None, finished)));
    }

    #[test]
    fn only_the_runs_own_turns_count() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let mut watch = RunWatch::new(session_id, run_id);
        let committed = || crate::tests::committed_turn(1);
        let other = event(session_id, Some(RunId::new_v7()), committed());
        assert!(matches!(watch.observe(&other), RunNote::Foreign));
        let own = event(session_id, Some(run_id), committed());
        assert!(matches!(watch.observe(&own), RunNote::TurnCommitted));
        assert!(matches!(watch.observe(&own), RunNote::TurnCommitted));
        assert_eq!(watch.turns(), 2);
    }

    #[test]
    fn a_run_end_keeps_its_reply_or_reason_and_an_unknown_one_is_an_interruption() {
        let completed = RunEnd::of(&Open::Known(RunOutcome::Completed {
            final_text: Some("done".into()),
        }));
        assert_eq!(completed.status, RunEndStatus::Completed);
        assert_eq!(completed.final_text.as_deref(), Some("done"));
        assert_eq!(completed.reason, None);

        let cancelled = RunEnd::of(&Open::Known(RunOutcome::Cancelled { reason: None }));
        assert_eq!(cancelled.status, RunEndStatus::Cancelled);
        assert_eq!(cancelled.reason, None);

        let unknown = RunEnd::of(&Open::Unknown {
            tag: "later".into(),
            raw: serde_json::json!({ "type": "later" }),
        });
        assert_eq!(unknown.status, RunEndStatus::Interrupted);
        assert_eq!(unknown.reason, None);
    }
}

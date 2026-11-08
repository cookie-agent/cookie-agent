//! The reviewer conversation as its own log keeps it
//! (`docs/site/specs/forever-sessions.md` section 5.5).
//!
//! The log is the conversation's turns since its latest epoch, each holding
//! the `user` turn and the verdict as they were sent. A review renders only
//! its delta: the entries the session's log positions after the previous
//! turn's cut and up to this review's, read from the session's log from
//! there with what the previous turn's cursor says of what came before. When
//! that turn would not fit the window, an epoch starts: a fresh turn rendered
//! from what the log kept, within the same budgets, with no model call.
//!
//! What it renders is what [`super::render`], the render of the whole log the
//! reviewer used before, gives for the same reviews, with these exceptions,
//! each by design:
//!
//! - **`AGENTS.md` is read afresh at each epoch** of a root session: the
//!   newest run's records before the epoch's cut, where the render kept those
//!   before the first review for every epoch. A delegated session keeps what
//!   it inherited of its root when its log was created.
//! - **A session with no reviewer log**, as every session an earlier build
//!   reviewed, starts one with a fresh turn at its next review, carrying the
//!   last verdicts its own log records; the render would have sent the
//!   conversation from its first review.
//! - **What a turn sent stays as sent.** A reviewer model with another window
//!   moves the next epoch, not the earlier ones, and a fresh turn renders from
//!   what the log kept, where the render would have reached back to entries
//!   the earlier bounds had left out.
//! - **A run's number counts the runs of the session's log**, of which the
//!   cursor keeps the latest [`RECENT_RUNS`]: a record naming an older run
//!   would number it anew. A call still open holds its entries for at most
//!   [`OPEN_CALLS`] calls, a message waiting to be consumed for at most
//!   [`WAITING_MESSAGES`] messages.
//! - **A verdict the reviewer's log did not record**, as when the daemon
//!   stopped between the evaluation and the record, is no turn: its entries
//!   fall into the next review's, as a failed review's do.

use std::{collections::HashMap, sync::Arc};

use cookie_agent_protocol::{
    ApprovalDecisionSource, ApprovalFinalOutcome, ApprovalId, ApprovalInternalDecisionKind,
    ApprovalReasonCode, ApprovalRequest, AssistantToolCallRef, InternalAgentKind, Open,
    ReviewerAgentsMd, ReviewerAttachment, ReviewerCursor, ReviewerEntry, ReviewerEpoch,
    ReviewerInherited, ReviewerMessage, ReviewerOmitted, ReviewerOpenCall, ReviewerRun,
    ReviewerTurn, ReviewerVerdict, RunId, StoredEvent, ToolCallId,
};
use serde_json::Value;

use super::{
    Attachments, Bounds, CARRIED_VERDICTS, CallFacts, Conversation, Inherited, KEPT_FIRST_MESSAGES,
    MESSAGE_TOKEN_CAP, READ_ONLY_TOOLS, Since, TRAJECTORY_TOKEN_CAP, Verdict, agents_md_before,
    agents_md_text, cap_tokens, estimate_tokens, has_authority, input_label, producer_label,
    request_block, termination_outcome, timestamp_label, trajectory_text,
};
use crate::runtime::{Event, approval::flow::escape_untrusted};

/// Runs a cursor numbers by their id.
pub(in crate::runtime) const RECENT_RUNS: usize = 64;
/// Open calls a cursor holds the entries of.
pub(in crate::runtime) const OPEN_CALLS: usize = 256;
/// Producer messages a cursor holds until they are consumed.
pub(in crate::runtime) const WAITING_MESSAGES: usize = 256;

/// Where the committed turn of a started call is, when the pass did not read
/// it: the tool's name and the model's arguments.
pub(in crate::runtime) type TurnLookup<'a> = &'a dyn Fn(&AssistantToolCallRef) -> Option<Value>;

/// The turns of a reviewer log since its latest epoch, from what the log
/// holds of its window: empty when it holds none, or when the first of them
/// did not start an epoch, which no log this build writes holds.
#[must_use]
pub(in crate::runtime) fn thread_of(events: &[Arc<StoredEvent>]) -> Vec<ReviewerTurn> {
    let turns = events
        .iter()
        .filter_map(|event| match &event.payload {
            Event::ReviewerTurnRecorded { turn } => Some((**turn).clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let Some(start) = turns.iter().rposition(|turn| turn.epoch.is_some()) else {
        return Vec::new();
    };
    turns[start..].to_vec()
}

/// The reviews a pass from the log's start finds, as [`super::render`]
/// recognises them: a completed approval agent between the call's request and
/// its evaluated step, with that step's decision.
#[derive(Default)]
struct Reviews {
    requests: HashMap<ApprovalId, (u64, ToolCallId)>,
    names: HashMap<ToolCallId, String>,
    pending: Option<ApprovalId>,
    reviewed: std::collections::HashSet<ApprovalId>,
    verdicts: Vec<ReviewerVerdict>,
}

/// One pass over a session's log from a cut: the entries it positions there,
/// and the cursor that reading on from where it stops needs.
pub(in crate::runtime) struct Gatherer<'a> {
    cursor: ReviewerCursor,
    entries: Vec<ReviewerEntry>,
    turn_calls: HashMap<(u64, u32), Value>,
    turns: Option<TurnLookup<'a>>,
    reviews: Option<Reviews>,
    current: Option<ApprovalId>,
}

impl<'a> Gatherer<'a> {
    /// A pass from the log's start, finding the reviews it records before
    /// `current`'s.
    pub(in crate::runtime) fn from_start(
        inherited: Option<&Inherited>,
        current: Option<ApprovalId>,
        turns: Option<TurnLookup<'a>>,
    ) -> Self {
        Self {
            cursor: ReviewerCursor {
                inherited: inherited.map(|inherited| ReviewerInherited {
                    agents_md: inherited.agents_md.clone(),
                }),
                ..ReviewerCursor::default()
            },
            entries: Vec::new(),
            turn_calls: HashMap::new(),
            turns,
            reviews: Some(Reviews::default()),
            current,
        }
    }

    /// A pass from the cut `cursor` was taken at.
    pub(in crate::runtime) fn from_cursor(
        cursor: &ReviewerCursor,
        turns: Option<TurnLookup<'a>>,
    ) -> Self {
        Self {
            cursor: cursor.clone(),
            entries: Vec::new(),
            turn_calls: HashMap::new(),
            turns,
            reviews: None,
            current: None,
        }
    }

    /// The label `, starts run N` or `, during run N` of an input in `run`.
    fn run_label(&mut self, run: Option<RunId>) -> String {
        let Some(run) = run else {
            return String::new();
        };
        let Some(known) = self
            .cursor
            .recent_runs
            .iter_mut()
            .find(|known| known.run_id == run)
        else {
            return ", during run 0".to_owned();
        };
        if known.has_input {
            format!(", during run {}", known.ordinal)
        } else {
            known.has_input = true;
            format!(", starts run {}", known.ordinal)
        }
    }

    /// Numbers `run` when the pass sees it first, as runs count in the order
    /// they first appear.
    fn note_run(&mut self, run: RunId) {
        if self
            .cursor
            .recent_runs
            .iter()
            .any(|known| known.run_id == run)
        {
            return;
        }
        self.cursor.runs += 1;
        self.cursor.recent_runs.push(ReviewerRun {
            run_id: run,
            ordinal: self.cursor.runs,
            has_input: false,
        });
        if self.cursor.recent_runs.len() > RECENT_RUNS {
            self.cursor.recent_runs.remove(0);
        }
    }

    fn message(&mut self, seq: u64, text: String) {
        self.entries.push(ReviewerEntry {
            seq,
            text,
            message: true,
        });
    }

    fn trajectory(&mut self, seq: u64, text: String) {
        self.entries.push(ReviewerEntry {
            seq,
            text,
            message: false,
        });
    }

    pub(in crate::runtime) fn observe(&mut self, event: &StoredEvent) {
        if let Some(run) = event.run_id {
            self.note_run(run);
        }
        match &event.payload {
            Event::RuntimeAttached {
                name,
                runtime_id,
                kind,
                workspace,
                ..
            } => {
                let attachment = ReviewerAttachment {
                    name: name.as_str().to_owned(),
                    runtime_id: runtime_id.as_str().to_owned(),
                    kind: kind.as_str().to_owned(),
                    workspace: workspace.clone(),
                };
                match self
                    .cursor
                    .attachments
                    .iter_mut()
                    .find(|known| known.name == attachment.name)
                {
                    Some(known) => *known = attachment,
                    None => self.cursor.attachments.push(attachment),
                }
            }
            Event::AgentMdLoaded { .. } if self.cursor.inherited.is_none() => {
                if let Some(text) = agents_md_text(event) {
                    if self
                        .cursor
                        .agents_md
                        .last()
                        .is_some_and(|last| last.run_id != event.run_id)
                    {
                        self.cursor.agents_md.clear();
                    }
                    self.cursor.agents_md.push(ReviewerAgentsMd {
                        seq: event.seq,
                        run_id: event.run_id,
                        text,
                    });
                }
            }
            Event::UserInputSubmitted { content } => {
                let origin = event.origin.as_ref().map(|origin| origin.as_str());
                let when = self.run_label(event.run_id);
                let text = format!(
                    "{} {}{when}\n{}",
                    input_label(origin),
                    timestamp_label(event.timestamp),
                    escape_untrusted(&cap_tokens(&content.as_plain_text(), MESSAGE_TOKEN_CAP))
                );
                self.message(event.seq, text);
            }
            Event::ProducerMessageAccepted {
                message_id,
                producer_owner,
                body,
                ..
            } => {
                self.cursor
                    .messages
                    .retain(|message| message.message_id != *message_id);
                if let Some(label) = producer_label(producer_owner) {
                    self.cursor.messages.push(ReviewerMessage {
                        message_id: *message_id,
                        label,
                        body: escape_untrusted(&cap_tokens(
                            &body.as_plain_text(),
                            MESSAGE_TOKEN_CAP,
                        ))
                        .into_owned(),
                    });
                    if self.cursor.messages.len() > WAITING_MESSAGES {
                        self.cursor.messages.remove(0);
                    }
                }
            }
            Event::ProducerMessageDiscarded { message_id, .. } => {
                self.cursor
                    .messages
                    .retain(|message| message.message_id != *message_id);
            }
            Event::ProducerMessageConsumed { message_id, run_id } => {
                if let Some(position) = self
                    .cursor
                    .messages
                    .iter()
                    .position(|message| message.message_id == *message_id)
                {
                    let message = self.cursor.messages.remove(position);
                    let when = self.run_label(Some(*run_id));
                    let text = format!(
                        "{} {}{when}\n{}",
                        message.label,
                        timestamp_label(event.timestamp),
                        message.body
                    );
                    self.message(event.seq, text);
                }
            }
            Event::ModelTurnCommitted {
                model_turn_seq,
                turn,
                ..
            } => {
                for (index, part) in turn.content.iter().enumerate() {
                    if let cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                        input, ..
                    } = part
                    {
                        self.turn_calls.insert(
                            (*model_turn_seq, u32::try_from(index).unwrap_or(u32::MAX)),
                            input.clone(),
                        );
                    }
                }
            }
            Event::ToolCallStarted { start } => {
                let name = start.presentation.tool.name.clone();
                if let Some(reviews) = &mut self.reviews {
                    reviews.names.insert(start.tool_call_id, name.clone());
                }
                if READ_ONLY_TOOLS.contains(&name.as_str()) {
                    return;
                }
                let arguments = self
                    .turn_calls
                    .get(&(start.owner.model_turn_seq, start.owner.content_index))
                    .cloned()
                    .or_else(|| self.turns.and_then(|turns| turns(&start.owner)));
                let call = CallFacts {
                    name: Some(name),
                    source: start
                        .presentation
                        .tool
                        .source
                        .as_ref()
                        .and_then(|source| source.known().cloned()),
                    arguments,
                };
                let end = |outcome| trajectory_text(&call, outcome).unwrap_or_default();
                self.cursor
                    .open_calls
                    .retain(|open| open.tool_call_id != start.tool_call_id);
                self.cursor.open_calls.push(ReviewerOpenCall {
                    tool_call_id: start.tool_call_id,
                    completed: end("completed"),
                    failed: end("failed"),
                    denied: end("denied"),
                    cancelled: end("cancelled"),
                });
                if self.cursor.open_calls.len() > OPEN_CALLS {
                    self.cursor.open_calls.remove(0);
                }
            }
            Event::ToolCallTerminated { termination } => {
                if let Some(position) = self
                    .cursor
                    .open_calls
                    .iter()
                    .position(|open| open.tool_call_id == termination.tool_call_id)
                {
                    let open = self.cursor.open_calls.remove(position);
                    let text = match termination_outcome(termination) {
                        "denied" => open.denied,
                        "failed" => open.failed,
                        "cancelled" => open.cancelled,
                        _ => open.completed,
                    };
                    self.trajectory(event.seq, text);
                }
            }
            Event::ApprovalRequested { request } => {
                if let Some(reviews) = &mut self.reviews {
                    reviews
                        .requests
                        .insert(request.approval_id(), (event.seq, request.tool_call_id()));
                    if Some(request.approval_id()) != self.current {
                        reviews.pending = Some(request.approval_id());
                    }
                }
            }
            Event::InternalAgentFinished { kind, outcome, .. } => {
                if let Some(reviews) = &mut self.reviews
                    && *kind == InternalAgentKind::Approval
                    && matches!(
                        outcome.known(),
                        Some(cookie_agent_protocol::InternalAgentOutcome::Completed { .. })
                    )
                    && let Some(approval_id) = reviews.pending.take()
                {
                    reviews.reviewed.insert(approval_id);
                }
            }
            Event::ApprovalStep {
                approval_id,
                step: Open::Known(cookie_agent_protocol::ApprovalStep::Evaluated { decision }),
            } => {
                if let Some(reviews) = &mut self.reviews {
                    if reviews.pending == Some(*approval_id) {
                        reviews.pending = None;
                    }
                    let decided = match decision.reason_code {
                        ApprovalReasonCode::InternalAgentAllowed => {
                            Some(ApprovalInternalDecisionKind::Allow)
                        }
                        ApprovalReasonCode::InternalAgentDenied => {
                            Some(ApprovalInternalDecisionKind::Deny)
                        }
                        ApprovalReasonCode::Escalated => Some(ApprovalInternalDecisionKind::Ask),
                        _ => None,
                    };
                    if let Some(decision_kind) = decided.filter(|_| {
                        reviews.reviewed.contains(approval_id)
                            && decision.source == ApprovalDecisionSource::InternalAgent
                    }) && let Some((cut, call)) = reviews.requests.get(approval_id)
                    {
                        reviews.verdicts.push(ReviewerVerdict {
                            cut_seq: *cut,
                            tool: reviews
                                .names
                                .get(call)
                                .cloned()
                                .unwrap_or_else(|| "a tool".to_owned()),
                            verdict: Verdict {
                                decision: decision_kind,
                                reason: decision.rationale.clone(),
                            }
                            .render(),
                        });
                    }
                }
            }
            Event::ApprovalFinalized {
                outcome:
                    Open::Known(cookie_agent_protocol::ApprovalFinalization::Decided { decision }),
                ..
            } if decision.source == ApprovalDecisionSource::User => {
                let text = match decision.outcome {
                    ApprovalFinalOutcome::Approved => Some("[user decision] approved".to_owned()),
                    ApprovalFinalOutcome::Rejected => Some(match &decision.feedback {
                        Some(feedback) => format!(
                            "[user decision] rejected: {}",
                            escape_untrusted(&cap_tokens(
                                feedback.message.as_str(),
                                TRAJECTORY_TOKEN_CAP
                            ))
                        ),
                        None => "[user decision] rejected".to_owned(),
                    }),
                    ApprovalFinalOutcome::Cancelled | ApprovalFinalOutcome::Expired => None,
                };
                if let Some(text) = text {
                    self.trajectory(event.seq, text);
                }
            }
            _ => {}
        }
    }

    /// The user messages the pass rendered, in order.
    pub(in crate::runtime) fn into_messages(self) -> Vec<String> {
        self.entries
            .into_iter()
            .filter(|entry| entry.message)
            .map(|entry| entry.text)
            .collect()
    }
}

/// What a review sends, and the turn the reviewer's log records once the
/// verdict answers it.
#[derive(Debug)]
pub(in crate::runtime) struct Prepared {
    pub(in crate::runtime) conversation: Conversation,
    turn: ReviewerTurn,
}

impl Prepared {
    /// A log's first turn as sent, when its review gave no verdict to
    /// record: what the next review reads on from.
    #[must_use]
    pub(in crate::runtime) fn into_unrecorded(self) -> Option<ReviewerTurn> {
        self.turn.epoch.is_some().then_some(self.turn)
    }

    /// The turn the reviewer's log records for `verdict`, which the session's
    /// log evaluated at `evaluated_seq`.
    #[must_use]
    pub(in crate::runtime) fn record(self, verdict: &Verdict, evaluated_seq: u64) -> ReviewerTurn {
        ReviewerTurn {
            evaluated_seq,
            verdict: verdict.render(),
            ..self.turn
        }
    }
}

/// The review of `request`, cut at `cut` in the session's log, after
/// `thread`, the reviewer log's turns since its latest epoch.
///
/// `events` is the session's log after the last turn's cut through `cut`, or
/// from its start when `thread` is empty; `inherited` is a delegated
/// session's opening, read only then. `call` is what the log records of the
/// call under review, `turns` where a started call's arguments are when the
/// pass did not read its turn.
pub(in crate::runtime) struct Review<'a> {
    pub(in crate::runtime) thread: &'a [ReviewerTurn],
    /// A log's first turn that was sent and never recorded, as its review
    /// failed: with no thread, the next review reads on from its cut instead
    /// of from the log's start, as it holds what that pass kept.
    pub(in crate::runtime) unrecorded: Option<&'a ReviewerTurn>,
    pub(in crate::runtime) inherited: Option<&'a Inherited>,
    pub(in crate::runtime) request: &'a ApprovalRequest,
    pub(in crate::runtime) cut: u64,
    pub(in crate::runtime) call: Option<&'a CallFacts>,
    pub(in crate::runtime) turns: Option<TurnLookup<'a>>,
    pub(in crate::runtime) bounds: Bounds,
}

impl Review<'_> {
    /// What the review sends, from `events`, the session's log as
    /// [`Review`] says.
    pub(in crate::runtime) fn prepare(
        &self,
        events: impl IntoIterator<Item = Arc<StoredEvent>>,
    ) -> Prepared {
        let Some(last) = self.thread.last() else {
            return match self.unrecorded {
                Some(unrecorded) => self.after_unrecorded(unrecorded, events),
                None => self.first(events),
            };
        };
        let mut gatherer = Gatherer::from_cursor(&last.cursor, self.turns);
        for event in events {
            if event.seq > last.cut_seq && event.seq <= self.cut {
                gatherer.observe(&event);
            }
        }
        let attachments = attachments_of(&gatherer.cursor);
        let messages = gatherer
            .entries
            .iter()
            .filter(|entry| entry.message)
            .count();
        let mut text = String::from("<context_delta>\n");
        for entry in &gatherer.entries {
            text.push_str(&entry.text);
            text.push_str("\n\n");
        }
        if gatherer.entries.is_empty() {
            text.push_str("[nothing new]\n");
        } else {
            text.pop();
        }
        text.push_str("</context_delta>\n\n");
        text.push_str(&request_block(
            Some(self.request),
            self.call,
            &attachments,
            Since::Delta(messages),
        ));
        let used = self.bounds.system_tokens
            + self
                .thread
                .iter()
                .map(|turn| estimate_tokens(&turn.request) + estimate_tokens(&turn.verdict))
                .sum::<usize>();
        if used + estimate_tokens(&text) <= self.bounds.window {
            return Prepared {
                conversation: Conversation {
                    reviews: self
                        .thread
                        .iter()
                        .map(|turn| (turn.request.clone(), turn.verdict.clone()))
                        .collect(),
                    request: text.clone(),
                },
                turn: self.turn(text, gatherer.entries, None, gatherer.cursor),
            };
        }
        // An epoch: the kept entries of the epoch's first turn, those of each
        // turn since, and this one's.
        let epoch = self
            .thread
            .first()
            .and_then(|first| first.epoch.clone())
            .unwrap_or_default();
        let mut messages = epoch.messages;
        let mut trajectory = epoch.trajectory;
        for entry in self
            .thread
            .iter()
            .flat_map(|turn| turn.entries.iter().cloned())
            .chain(gatherer.entries)
        {
            if entry.message {
                messages.push(entry);
            } else {
                trajectory.push(entry);
            }
        }
        let verdicts = epoch
            .verdicts
            .into_iter()
            .chain(self.thread.iter().map(|turn| ReviewerVerdict {
                cut_seq: turn.cut_seq,
                tool: turn.tool.clone(),
                verdict: turn.verdict.clone(),
            }))
            .collect::<Vec<_>>();
        let kept = Kept {
            messages,
            messages_omitted: epoch.messages_omitted,
            trajectory,
            trajectory_omitted: epoch.trajectory_omitted,
        };
        self.fresh(kept, &verdicts, gatherer.cursor, Since::StartedOver)
    }

    /// A log's first turn after `unrecorded`, one that was sent and not
    /// recorded: a fresh turn from what it kept and the entries since its cut,
    /// as [`Self::first`] gives from the whole log, carrying the same verdicts,
    /// as no verdict came since.
    fn after_unrecorded(
        &self,
        unrecorded: &ReviewerTurn,
        events: impl IntoIterator<Item = Arc<StoredEvent>>,
    ) -> Prepared {
        let mut gatherer = Gatherer::from_cursor(&unrecorded.cursor, self.turns);
        for event in events {
            if event.seq > unrecorded.cut_seq && event.seq <= self.cut {
                gatherer.observe(&event);
            }
        }
        let epoch = unrecorded.epoch.clone().unwrap_or_default();
        let mut messages = epoch.messages;
        let mut trajectory = epoch.trajectory;
        for entry in gatherer.entries.drain(..) {
            if entry.message {
                messages.push(entry);
            } else {
                trajectory.push(entry);
            }
        }
        let kept = Kept {
            messages,
            messages_omitted: epoch.messages_omitted,
            trajectory,
            trajectory_omitted: epoch.trajectory_omitted,
        };
        let since = if epoch.verdicts.is_empty() {
            Since::FirstReview
        } else {
            Since::StartedOver
        };
        self.fresh(kept, &epoch.verdicts, gatherer.cursor, since)
    }

    /// The first review of a log: a fresh turn from one pass over the
    /// session's log, carrying the last verdicts it records.
    fn first(&self, events: impl IntoIterator<Item = Arc<StoredEvent>>) -> Prepared {
        let mut gatherer =
            Gatherer::from_start(self.inherited, Some(self.request.approval_id()), self.turns);
        for event in events {
            if event.seq <= self.cut {
                gatherer.observe(&event);
            }
        }
        let verdicts = gatherer
            .reviews
            .take()
            .map(|reviews| reviews.verdicts)
            .unwrap_or_default()
            .into_iter()
            .filter(|verdict| verdict.cut_seq < self.cut)
            .collect::<Vec<_>>();
        let mut messages = self
            .inherited
            .map(|inherited| {
                inherited
                    .entries
                    .iter()
                    .map(|text| ReviewerEntry {
                        seq: 0,
                        text: text.clone(),
                        message: true,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut trajectory = Vec::new();
        for entry in std::mem::take(&mut gatherer.entries) {
            if entry.message {
                messages.push(entry);
            } else {
                trajectory.push(entry);
            }
        }
        let kept = Kept {
            messages,
            messages_omitted: None,
            trajectory,
            trajectory_omitted: 0,
        };
        let since = if verdicts.is_empty() {
            Since::FirstReview
        } else {
            Since::StartedOver
        };
        self.fresh(kept, &verdicts, gatherer.cursor, since)
    }

    fn turn(
        &self,
        request: String,
        entries: Vec<ReviewerEntry>,
        epoch: Option<ReviewerEpoch>,
        cursor: ReviewerCursor,
    ) -> ReviewerTurn {
        ReviewerTurn {
            approval_id: self.request.approval_id(),
            cut_seq: self.cut,
            evaluated_seq: 0,
            tool: self
                .call
                .and_then(|call| call.name.clone())
                .unwrap_or_else(|| "a tool".to_owned()),
            request,
            verdict: String::new(),
            entries,
            epoch,
            cursor,
        }
    }

    /// A fresh turn from `kept`, within the budgets, carrying the last of
    /// `verdicts`: the first turn of an epoch.
    fn fresh(
        &self,
        kept: Kept,
        verdicts: &[ReviewerVerdict],
        cursor: ReviewerCursor,
        since: Since,
    ) -> Prepared {
        let carried = &verdicts[verdicts.len().saturating_sub(CARRIED_VERDICTS)..];
        let carried_entries = carried
            .iter()
            .map(|verdict| {
                (
                    verdict.cut_seq,
                    escape_untrusted(&cap_tokens(
                        &format!(
                            "[your earlier verdict] on {}: {}",
                            verdict.tool, verdict.verdict
                        ),
                        TRAJECTORY_TOKEN_CAP,
                    ))
                    .into_owned(),
                )
            })
            .collect::<Vec<_>>();
        let (messages, messages_marker, messages_omitted) = budget_messages(
            &kept.messages,
            kept.messages_omitted,
            self.bounds.messages(),
        );
        let mut trajectory = kept
            .trajectory
            .iter()
            .map(|entry| (entry.seq, entry.text.as_str()))
            .chain(
                carried_entries
                    .iter()
                    .map(|(seq, text)| (*seq, text.as_str())),
            )
            .collect::<Vec<_>>();
        trajectory.sort_by_key(|(seq, _)| *seq);
        let unknown_before = (kept.trajectory_omitted > 0)
            .then(|| kept.trajectory.first().map(|entry| entry.seq))
            .flatten();
        let (trajectory_start, trajectory_marker) = budget_trajectory(
            &trajectory,
            usize::try_from(kept.trajectory_omitted).unwrap_or(usize::MAX),
            unknown_before,
            self.bounds.trajectory(),
        );

        let agents_md = match &cursor.inherited {
            Some(inherited) => inherited.agents_md.clone(),
            None => agents_md_before(
                &cursor
                    .agents_md
                    .iter()
                    .map(|loaded| (loaded.seq, loaded.run_id, loaded.text.clone()))
                    .collect::<Vec<_>>(),
                self.cut,
            ),
        };
        let mut text = String::from("<context>\n");
        let mut wrote = false;
        let mut push = |text: &mut String, entry: &str| {
            text.push_str(entry);
            text.push_str("\n\n");
            wrote = true;
        };
        if let Some(agents_md) = &agents_md {
            push(&mut text, &format!("[project instructions]\n{agents_md}"));
        }
        let mut merged = messages
            .iter()
            .map(|entry| (entry.seq, entry.text.clone()))
            .chain(
                trajectory[trajectory_start..]
                    .iter()
                    .map(|(seq, entry)| (*seq, (*entry).to_owned())),
            )
            .collect::<Vec<_>>();
        if let Some((at, count)) = messages_marker {
            merged.push((at, format!("[… {count} earlier messages omitted …]")));
        }
        if let Some((at, count)) = trajectory_marker {
            merged.push((
                at,
                format!("[… {count} earlier trajectory entries omitted …]"),
            ));
        }
        merged.sort_by_key(|(seq, _)| *seq);
        for (_, entry) in &merged {
            push(&mut text, entry);
        }
        if wrote {
            text.pop();
        } else {
            text.push_str("[nothing new]\n");
        }
        text.push_str("</context>\n\n");
        let attachments = attachments_of(&cursor);
        text.push_str(&request_block(
            Some(self.request),
            self.call,
            &attachments,
            since,
        ));
        let (trajectory, trajectory_omitted) = hold_trajectory(
            kept.trajectory,
            kept.trajectory_omitted,
            self.bounds.trajectory(),
        );
        let epoch = ReviewerEpoch {
            messages,
            messages_omitted,
            trajectory,
            trajectory_omitted,
            verdicts: carried.to_vec(),
        };
        Prepared {
            conversation: Conversation {
                reviews: Vec::new(),
                request: text.clone(),
            },
            turn: self.turn(text, Vec::new(), Some(epoch), cursor),
        }
    }
}

/// What a fresh turn renders from: the entries a log kept, and what was left
/// out before them.
struct Kept {
    messages: Vec<ReviewerEntry>,
    messages_omitted: Option<ReviewerOmitted>,
    trajectory: Vec<ReviewerEntry>,
    trajectory_omitted: u64,
}

fn attachments_of(cursor: &ReviewerCursor) -> Attachments {
    cursor
        .attachments
        .iter()
        .map(|attachment| {
            (
                attachment.name.clone(),
                (
                    attachment.runtime_id.clone(),
                    attachment.kind.clone(),
                    attachment.workspace.clone(),
                ),
            )
        })
        .collect()
}

fn tokens(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// The messages a fresh turn keeps of `messages`, the ones a log kept, with
/// `omitted` left out before (section 6): within `budget`, the first
/// [`KEPT_FIRST_MESSAGES`] with the user's authority, the first instructions
/// from a parent agent, and the newest down to half the budget, as the render
/// of the whole log keeps them. An earlier epoch left out only messages no
/// later one keeps: never a task, and none newer than what it kept. Returns
/// what it keeps, where the omission marker goes with how many it stands for,
/// and what is left out from here on.
fn budget_messages(
    messages: &[ReviewerEntry],
    omitted: Option<ReviewerOmitted>,
    budget: usize,
) -> (
    Vec<ReviewerEntry>,
    Option<(u64, usize)>,
    Option<ReviewerOmitted>,
) {
    let omitted_tokens = omitted.map_or(0, |omitted| {
        usize::try_from(omitted.tokens).unwrap_or(usize::MAX)
    });
    let total = messages
        .iter()
        .map(|entry| estimate_tokens(&entry.text))
        .sum::<usize>()
        .saturating_add(omitted_tokens);
    let marker = |omitted: Option<ReviewerOmitted>| {
        omitted.map(|omitted| {
            (
                omitted.first_seq,
                usize::try_from(omitted.count).unwrap_or(usize::MAX),
            )
        })
    };
    if total <= budget {
        return (messages.to_vec(), marker(omitted), omitted);
    }
    let target = budget / 2;
    let mut keep = vec![false; messages.len()];
    let mut used = 0;
    let mut first = 0;
    let mut instructions = false;
    for (index, entry) in messages.iter().enumerate() {
        let task = if has_authority(&entry.text) && first < KEPT_FIRST_MESSAGES {
            first += 1;
            true
        } else if !instructions
            && entry
                .text
                .starts_with("[instructions from the parent agent]")
        {
            instructions = true;
            true
        } else {
            false
        };
        if task {
            keep[index] = true;
            used += estimate_tokens(&entry.text);
        }
    }
    let last_first = keep.iter().rposition(|kept| *kept);
    for index in (0..messages.len()).rev() {
        if keep[index] || last_first.is_some_and(|last| index < last) {
            continue;
        }
        let cost = estimate_tokens(&messages[index].text);
        if used + cost > target {
            break;
        }
        keep[index] = true;
        used += cost;
    }
    let mut left = omitted.unwrap_or(ReviewerOmitted {
        count: 0,
        tokens: 0,
        first_seq: u64::MAX,
    });
    let mut kept = Vec::new();
    for (entry, keep) in messages.iter().zip(keep) {
        if keep {
            kept.push(entry.clone());
        } else {
            left.count += 1;
            left.tokens = left
                .tokens
                .saturating_add(tokens(estimate_tokens(&entry.text)));
            left.first_seq = left.first_seq.min(entry.seq);
        }
    }
    let left = (left.count > 0).then_some(left);
    (kept, marker(left), left)
}

/// Where the trajectory a fresh turn keeps starts in `trajectory`, the
/// entries a log kept with the carried verdicts, in order, and the omission
/// marker (section 6): it drops its oldest half, as often as it takes to fit
/// `budget`, as the render of the whole log drops them. `unknown` older
/// entries were let go of, all before `unknown_before`, the first kept, from
/// which on what was kept is over the budget: a half that reaches them is
/// over it too.
fn budget_trajectory(
    trajectory: &[(u64, &str)],
    unknown: usize,
    unknown_before: Option<u64>,
    budget: usize,
) -> (usize, Option<(u64, usize)>) {
    let length = trajectory.len();
    let total = unknown + length;
    // The newest entries whose order is known: all of them, or those from the
    // first kept on.
    let known = match unknown_before {
        Some(first) if unknown > 0 => trajectory.iter().filter(|(seq, _)| *seq >= first).count(),
        _ => length,
    };
    let over = |remaining: usize| {
        remaining > known
            || trajectory[length - remaining..]
                .iter()
                .map(|(_, text)| estimate_tokens(text))
                .sum::<usize>()
                > budget
    };
    let mut remaining = total;
    let mut dropped = 0;
    let mut last_at = None;
    while remaining > 0 && over(remaining) {
        let half = remaining.div_ceil(2);
        last_at = Some(total - remaining + half - 1);
        remaining -= half;
        dropped += half;
    }
    let start = length - remaining.min(length);
    let marker = last_at.and_then(|at| {
        length
            .checked_sub(total - at)
            .map(|index| (trajectory[index].0, dropped))
    });
    (start, marker)
}

/// What a log keeps of `trajectory` for later epochs: the newest entries, up
/// to the first that puts them over `budget`, as no later half that fits
/// reaches past it. Returns them, and how many were let go of in all.
fn hold_trajectory(
    trajectory: Vec<ReviewerEntry>,
    unknown: u64,
    budget: usize,
) -> (Vec<ReviewerEntry>, u64) {
    let mut used = 0;
    let mut start = 0;
    for (index, entry) in trajectory.iter().enumerate().rev() {
        used += estimate_tokens(&entry.text);
        if used > budget {
            start = index;
            break;
        }
    }
    let let_go = tokens(start);
    (
        trajectory.into_iter().skip(start).collect(),
        unknown + let_go,
    )
}

/// What the log records of a call: its name, where it runs, and the model's
/// arguments, from its `tool_call_started` and the committed turn it names.
#[must_use]
pub(in crate::runtime) fn call_facts(start: &StoredEvent, arguments: Option<Value>) -> CallFacts {
    match &start.payload {
        Event::ToolCallStarted { start } => CallFacts {
            name: Some(start.presentation.tool.name.clone()),
            source: start
                .presentation
                .tool
                .source
                .as_ref()
                .and_then(|source| source.known().cloned()),
            arguments,
        },
        _ => CallFacts::default(),
    }
}

/// The arguments of the tool call `owner` names, from its committed turn in
/// `events`: the latest turn of that sequence before it.
#[must_use]
pub(in crate::runtime) fn turn_arguments(
    events: &[Arc<StoredEvent>],
    owner: &AssistantToolCallRef,
) -> Option<Value> {
    events.iter().rev().find_map(|event| match &event.payload {
        Event::ModelTurnCommitted {
            model_turn_seq,
            turn,
            ..
        } if *model_turn_seq == owner.model_turn_seq => {
            match turn.content.get(usize::try_from(owner.content_index).ok()?) {
                Some(cookie_agent_protocol::PersistedAssistantPart::ToolCall { input, .. }) => {
                    Some(input.clone())
                }
                _ => None,
            }
        }
        _ => None,
    })
}

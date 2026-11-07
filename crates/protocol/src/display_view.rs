//! The display view of a session's history and its newest-first windows
//! (`docs/site/specs/session-storage-v3.md` section 3, client protocol
//! 1.59.0).
//!
//! The display view is what a transcript shows of a session's events:
//!
//! - **Bookkeeping is left out**: `model_request_prepared`, which no
//!   client reads, and a `model_replay_evaluated` whose decisions discard
//!   nothing (its rows are below the warning level).
//! - **Superseded settings are left out of a page**: a
//!   `model_tools_published` a later one in the page replaces, and in an
//!   `acp_session_updated` a command list or starting options a later one
//!   replaces, or a tool presentation a later one of the same call
//!   replaces. An update left empty is dropped.
//! - **Long tool results are cut**: a result whose output, display, and
//!   metadata take more than [`CUT_ABOVE_BYTES`] keeps the first
//!   [`CUT_KEEP_BYTES`] of its output, a display fit to the same budget,
//!   and no metadata; a [`CutOutput`] names it and the whole result's size,
//!   and `event.read` returns it whole.
//! - **A tool result's metadata is left out**: no transcript reads it, and
//!   an ACP agent's results repeat their whole output there.
//!
//! Every other event is sent as stored. `model_attempt_started`,
//! `model_output_started`, and `model_usage_recorded` stay: a transcript
//! builds its replies, their thinking times, and their costs from them.
//!
//! A newest-first window ([`tail_window`]) holds the newest display events
//! within a byte budget starting at a run, the events before it its state
//! depends on ([`state_events`]), and, when the newest run alone exceeds the
//! budget, the run's start, a [`HistoryGap`], and its newest model
//! attempts. [`older_page`] reads the history before a window the same way.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::{
    ApprovalId, CutOutput, DisplayBlock, ElicitationId, EventPayload, EventsView, HistoryGap, Open,
    OutputSpan, QueuedInputs, ReplayDisposition, StoredEvent, ToolCallId,
};

/// A tool result larger than this, output, display, and metadata
/// together, is cut in the display view.
pub const CUT_ABOVE_BYTES: usize = 4 * 1024;
/// How much of a cut result's output, and of its display, the view keeps.
pub const CUT_KEEP_BYTES: usize = 2 * 1024;

/// Whether the display view leaves out an event of this payload wherever it
/// stands: bookkeeping no transcript shows.
#[must_use]
pub fn leaves_out(payload: &EventPayload) -> bool {
    match payload {
        EventPayload::ModelRequestPrepared { .. } => true,
        payload if payload.is_omitted_live_state() => true,
        // Its replayed and reconstructed entries are debug rows, and an
        // empty list an info row; only a discard warns.
        EventPayload::ModelReplayEvaluated {
            ordered_decisions, ..
        } => !ordered_decisions.iter().any(|decision| {
            matches!(
                decision.disposition,
                ReplayDisposition::DiscardedForeignAdapter { .. }
                    | ReplayDisposition::DiscardedForeignModelSelection { .. }
                    | ReplayDisposition::DiscardedForeignVariant { .. }
                    | ReplayDisposition::DiscardedInvalidPayload { .. }
            )
        }),
        _ => false,
    }
}

/// One event as the display view sends it: `None` when the view leaves it
/// out, else the event, cut when its tool result is long, with what the
/// cut left out. The same event is shared when nothing changes.
#[must_use]
pub fn shown(event: &Arc<StoredEvent>) -> Option<(Arc<StoredEvent>, Option<CutOutput>)> {
    if leaves_out(&event.payload) {
        return None;
    }
    Some(match cut_result(event) {
        Some((event, cut)) => (Arc::new(event), Some(cut)),
        None => match without_metadata(event) {
            Some(event) => (Arc::new(event), None),
            None => (Arc::clone(event), None),
        },
    })
}

/// A tool result's event without its `metadata`; `None` when it has none.
fn without_metadata(event: &StoredEvent) -> Option<StoredEvent> {
    let EventPayload::ToolCallTerminated { termination } = &event.payload else {
        return None;
    };
    if termination
        .result
        .as_ref()
        .is_none_or(|result| result.metadata.is_null())
    {
        return None;
    }
    let mut termination = termination.clone();
    if let Some(result) = termination.result.as_mut() {
        result.metadata = serde_json::Value::Null;
    }
    Some(with_payload(
        event,
        EventPayload::ToolCallTerminated { termination },
    ))
}

/// A `tool_call_terminated` event whose result is long, cut as the display
/// view sends it.
#[must_use]
pub fn cut_result(event: &StoredEvent) -> Option<(StoredEvent, CutOutput)> {
    let EventPayload::ToolCallTerminated { termination } = &event.payload else {
        return None;
    };
    let result = termination.result.as_ref()?;
    // The output alone decides most of them without measuring the rest.
    if result.output.len() <= CUT_ABOVE_BYTES {
        let display = serialized_len(&result.display);
        let metadata = serialized_len(&result.metadata);
        if result.output.len() + display + metadata <= CUT_ABOVE_BYTES {
            return None;
        }
    }
    let bytes = serialized_len(result) as u64;
    let mut termination = termination.clone();
    let result = termination.result.as_mut().expect("checked above");
    result
        .output
        .truncate(floor_char_boundary(&result.output, CUT_KEEP_BYTES));
    let blocks = std::mem::take(&mut result.display)
        .into_iter()
        .filter_map(Open::into_known)
        .collect::<Vec<DisplayBlock>>();
    result.display = crate::fit_display_budget(blocks, CUT_KEEP_BYTES)
        .into_iter()
        .map(Open::Known)
        .collect();
    result.metadata = serde_json::Value::Null;
    Some((
        StoredEvent {
            engine_version: event.engine_version.clone(),
            origin: event.origin.clone(),
            session_id: event.session_id,
            run_id: event.run_id,
            seq: event.seq,
            timestamp: event.timestamp,
            payload: EventPayload::ToolCallTerminated { termination },
        },
        CutOutput {
            seq: event.seq,
            bytes,
        },
    ))
}

/// Events in `view`: [`display_events`] for the display view, else the
/// events as stored.
#[must_use]
pub fn view_events(
    events: &[Arc<StoredEvent>],
    view: EventsView,
    keep_last: bool,
) -> (Vec<Arc<StoredEvent>>, Vec<CutOutput>) {
    match view {
        EventsView::Display => display_events(events, keep_last),
        EventsView::Full => (events.to_vec(), Vec::new()),
    }
}

/// A contiguous run of a session's events in the display view, in order,
/// with the results it cut. With `keep_last`, the last event is sent even
/// when the view would leave it out, so a reader that pages forward from
/// it continues after the right sequence.
#[must_use]
pub fn display_events(
    events: &[Arc<StoredEvent>],
    keep_last: bool,
) -> (Vec<Arc<StoredEvent>>, Vec<CutOutput>) {
    let superseded = Superseded::of(events);
    let mut shown_events = Vec::with_capacity(events.len());
    let mut cuts = Vec::new();
    for (index, event) in events.iter().enumerate() {
        let last = keep_last && index + 1 == events.len();
        let event = match superseded.apply(index, event) {
            Slim::Whole => Arc::clone(event),
            Slim::Changed(event) => Arc::new(*event),
            Slim::Dropped if last => Arc::clone(event),
            Slim::Dropped => continue,
        };
        match shown(&event) {
            Some((event, cut)) => {
                cuts.extend(cut);
                shown_events.push(event);
            }
            None if last => shown_events.push(event),
            None => {}
        }
    }
    (shown_events, cuts)
}

/// What [`Superseded::apply`] does to one event.
enum Slim {
    Whole,
    Changed(Box<StoredEvent>),
    Dropped,
}

/// The settings in a run of events that a later event of the same run
/// replaces.
#[derive(Default)]
struct Superseded {
    /// The last `model_tools_published`.
    tools: Option<usize>,
    /// The last `acp_session_updated` with commands, and with starting
    /// options.
    commands: Option<usize>,
    starting_options: Option<usize>,
    /// The last tool presentation of each call.
    presentations: HashMap<ToolCallId, usize>,
}

impl Superseded {
    fn of(events: &[Arc<StoredEvent>]) -> Self {
        let mut superseded = Self::default();
        for (index, event) in events.iter().enumerate() {
            superseded.observe(index, event);
        }
        superseded
    }

    fn observe(&mut self, index: usize, event: &StoredEvent) {
        match &event.payload {
            EventPayload::ModelToolsPublished { .. } => self.tools = Some(index),
            EventPayload::AcpSessionUpdated { update } => {
                if update.commands.is_some() {
                    self.commands = Some(index);
                }
                if update.starting_options.is_some() {
                    self.starting_options = Some(index);
                }
                if let Some(presented) = &update.tool_presentation {
                    self.presentations.insert(presented.tool_call_id, index);
                }
            }
            _ => {}
        }
    }

    fn apply(&self, index: usize, event: &StoredEvent) -> Slim {
        match &event.payload {
            EventPayload::ModelToolsPublished { .. } if self.tools != Some(index) => Slim::Dropped,
            EventPayload::AcpSessionUpdated { update } => {
                let commands = update.commands.is_some() && self.commands != Some(index);
                let starting =
                    update.starting_options.is_some() && self.starting_options != Some(index);
                let presentation = update.tool_presentation.as_ref().is_some_and(|presented| {
                    self.presentations.get(&presented.tool_call_id) != Some(&index)
                });
                if !(commands || starting || presentation) {
                    return Slim::Whole;
                }
                let mut update = update.clone();
                if commands {
                    update.commands = None;
                }
                if starting {
                    update.starting_options = None;
                }
                if presentation {
                    update.tool_presentation = None;
                }
                if update.is_empty() {
                    Slim::Dropped
                } else {
                    Slim::Changed(Box::new(with_payload(
                        event,
                        EventPayload::AcpSessionUpdated { update },
                    )))
                }
            }
            _ => Slim::Whole,
        }
    }
}

/// The events of `prefix`, a log's events from its start, that the
/// session's state after them depends on, in sequence order: what a client
/// folds before a window that starts after `prefix` so that its state ends
/// where a full replay's would.
///
/// They are the latest `run_started` and its `run_finished`, the latest
/// `memory_loaded`,
/// `model_tools_published`, `acp_session_started`, and
/// `session_model_selected`; the session's `session_created`; the latest ACP
/// commands, options, starting options, and context use, each event holding
/// only those of them it is the latest for; the latest
/// `goal_activated` and the goal's changes after it; each runtime
/// attachment still attached; a hold no run or input cleared since; the
/// tools announced since the latest compaction; and each input still
/// queued.
#[must_use]
pub fn state_events(prefix: &[Arc<StoredEvent>]) -> Vec<Arc<StoredEvent>> {
    let mut picked: Vec<Arc<StoredEvent>> = Vec::new();
    let mut take = |event: &Arc<StoredEvent>| picked.push(Arc::clone(event));
    if let Some(first) = prefix.first()
        && matches!(first.payload, EventPayload::SessionCreated { .. })
    {
        take(first);
    }
    let mut latest = HashSet::<&'static str>::new();
    let mut acp_members = HashSet::<&'static str>::new();
    let mut attachments = HashSet::new();
    let mut goal_found = false;
    let mut checkpoint_found = false;
    let mut hold_settled = false;
    let mut acp_updates = Vec::new();
    for event in prefix.iter().rev() {
        match &event.payload {
            EventPayload::RunStarted { .. } if latest.insert("run_started") => take(event),
            // The latest run's end, when it ended: its run no longer runs.
            EventPayload::RunFinished { .. } if !latest.contains("run_started") => {
                if latest.insert("run_finished") {
                    take(event);
                }
            }
            EventPayload::MemoryLoaded { .. } if latest.insert("memory_loaded") => take(event),
            EventPayload::ModelToolsPublished { .. } if latest.insert("model_tools") => take(event),
            EventPayload::AcpSessionStarted { .. } if latest.insert("acp_session_started") => {
                take(event);
            }
            EventPayload::SessionModelSelected { .. } if latest.insert("model_selected") => {
                take(event)
            }
            EventPayload::AcpSessionUpdated { update } => {
                let mut members = Vec::new();
                for (member, present) in [
                    ("commands", update.commands.is_some()),
                    ("options", update.options.is_some()),
                    ("starting_options", update.starting_options.is_some()),
                    ("context", update.context.is_some()),
                ] {
                    if present && acp_members.insert(member) {
                        members.push(member);
                    }
                }
                if !members.is_empty() {
                    acp_updates.push((event, members));
                }
            }
            EventPayload::GoalChecklistRevised { .. }
            | EventPayload::GoalLifecycleChanged { .. }
                if !goal_found =>
            {
                take(event);
            }
            EventPayload::GoalActivated { .. } if !goal_found => {
                goal_found = true;
                take(event);
            }
            EventPayload::RuntimeAttached { name, .. } => {
                if attachments.insert(name.clone()) {
                    take(event);
                }
            }
            EventPayload::RuntimeDetached { name } => {
                attachments.insert(name.clone());
            }
            EventPayload::AutomaticRunsHeld { .. } if !hold_settled => {
                hold_settled = true;
                take(event);
            }
            EventPayload::AutomaticRunsResumed {} | EventPayload::UserInputSubmitted { .. } => {
                hold_settled = true;
            }
            EventPayload::ContextCheckpointCommitted { .. } => checkpoint_found = true,
            EventPayload::DeferredToolsAnnounced { .. } if !checkpoint_found => take(event),
            _ => {}
        }
    }
    for (event, members) in acp_updates {
        let EventPayload::AcpSessionUpdated { update } = &event.payload else {
            continue;
        };
        let mut kept = crate::AcpSessionUpdate::default();
        for member in members {
            match member {
                "commands" => kept.commands.clone_from(&update.commands),
                "options" => kept.options.clone_from(&update.options),
                "starting_options" => kept.starting_options.clone_from(&update.starting_options),
                _ => kept.context.clone_from(&update.context),
            }
        }
        picked.push(Arc::new(with_payload(
            event,
            EventPayload::AcpSessionUpdated { update: kept },
        )));
    }
    let queued = QueuedInputs::from_events(prefix);
    for entry in queued.entries() {
        if let Some(event) = event_at(prefix, entry.queued_seq) {
            picked.push(Arc::clone(event));
        }
    }
    picked.sort_by_key(|event| event.seq);
    picked.dedup_by_key(|event| event.seq);
    picked
}

/// Whether [`state_events`] reads an event of this payload. The rest need not
/// be held to fold it.
fn concerns_state(payload: &EventPayload) -> bool {
    match payload {
        EventPayload::AcpSessionUpdated { update } => {
            update.commands.is_some()
                || update.options.is_some()
                || update.starting_options.is_some()
                || update.context.is_some()
        }
        EventPayload::SessionCreated { .. }
        | EventPayload::RunStarted { .. }
        | EventPayload::RunFinished { .. }
        | EventPayload::MemoryLoaded { .. }
        | EventPayload::ModelToolsPublished { .. }
        | EventPayload::AcpSessionStarted { .. }
        | EventPayload::SessionModelSelected { .. }
        | EventPayload::GoalActivated { .. }
        | EventPayload::GoalChecklistRevised { .. }
        | EventPayload::GoalLifecycleChanged { .. }
        | EventPayload::RuntimeAttached { .. }
        | EventPayload::RuntimeDetached { .. }
        | EventPayload::AutomaticRunsHeld { .. }
        | EventPayload::AutomaticRunsResumed {}
        | EventPayload::UserInputSubmitted { .. }
        | EventPayload::UserInputQueued { .. }
        | EventPayload::UserInputQueueRecalled { .. }
        | EventPayload::UserInputQueueVoided {}
        | EventPayload::ContextCheckpointCommitted { .. }
        | EventPayload::DeferredToolsAnnounced { .. } => true,
        _ => false,
    }
}

/// [`state_events`] of a log read one event at a time, holding the header so
/// far and the few events since it was last worked out: [`state_events`] of
/// that header followed by later events is [`state_events`] of the log through
/// them.
#[derive(Clone, Debug, Default)]
pub struct StateEventsFold {
    held: Vec<Arc<StoredEvent>>,
    unsettled: usize,
}

impl StateEventsFold {
    /// How many events it reads, and so holds, before it works the header out.
    const SETTLES_EVERY: usize = 256;

    /// Folds the next event of the log.
    pub fn observe(&mut self, event: &Arc<StoredEvent>) {
        if concerns_state(&event.payload) {
            self.held.push(Arc::clone(event));
            self.unsettled += 1;
            if self.unsettled >= Self::SETTLES_EVERY {
                self.settle();
            }
        }
    }

    fn settle(&mut self) {
        if self.unsettled > 0 {
            self.held = state_events(&self.held);
            self.unsettled = 0;
        }
    }

    /// [`state_events`] of the events it read.
    #[must_use]
    pub fn finish(mut self) -> Vec<Arc<StoredEvent>> {
        self.settle();
        self.held
    }
}

/// The events of `skipped` a reader that holds what comes before and after
/// it still needs: the settings ([`state_events`]) and what is still
/// waiting when the log ends with `rest`: an approval or a question not
/// answered, an input admitted but not applied, and a tool call still
/// running.
fn kept_in_gap(skipped: &[Arc<StoredEvent>], rest: &[Arc<StoredEvent>]) -> Vec<Arc<StoredEvent>> {
    let mut answered_approvals = HashSet::<ApprovalId>::new();
    let mut answered_questions = HashSet::<ElicitationId>::new();
    let mut applied_inputs = HashSet::<u64>::new();
    let mut ended_calls = HashSet::<ToolCallId>::new();
    for event in skipped.iter().chain(rest) {
        match &event.payload {
            EventPayload::ApprovalFinalized { approval_id, .. } => {
                answered_approvals.insert(*approval_id);
            }
            EventPayload::ElicitationResolved { resolution } => {
                answered_questions.insert(resolution.elicitation_id);
            }
            EventPayload::UserInputApplied { user_input_seq }
            | EventPayload::UserInputRecalled { user_input_seq, .. } => {
                applied_inputs.insert(*user_input_seq);
            }
            EventPayload::ToolCallTerminated { termination } => {
                ended_calls.insert(termination.tool_call_id);
            }
            _ => {}
        }
    }
    // The gap's settings, without what the session's start holds: the
    // window's header has it.
    let mut kept = state_events(skipped)
        .into_iter()
        .filter(|event| !matches!(event.payload, EventPayload::SessionCreated { .. }))
        .collect::<Vec<_>>();
    for event in skipped {
        let waiting = match &event.payload {
            EventPayload::ApprovalRequested { request } => {
                !answered_approvals.contains(&request.approval_id())
            }
            EventPayload::ApprovalStep { approval_id, .. } => {
                !answered_approvals.contains(approval_id)
            }
            EventPayload::ElicitationRequested { request } => {
                !answered_questions.contains(&request.elicitation_id)
            }
            EventPayload::UserInputAdmitted { .. } => !applied_inputs.contains(&event.seq),
            EventPayload::ToolCallStarted { start } => !ended_calls.contains(&start.tool_call_id),
            _ => false,
        };
        if waiting {
            kept.push(Arc::clone(event));
        }
    }
    kept.sort_by_key(|event| event.seq);
    kept.dedup_by_key(|event| event.seq);
    kept
}

/// A newest-first answer: [`tail_window`] or [`older_page`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Window {
    /// The display events, in sequence order.
    pub events: Vec<Arc<StoredEvent>>,
    /// The results among them the view cut.
    pub cut: Vec<CutOutput>,
    /// [`state_events`] of everything before the first event.
    pub header: Vec<Arc<StoredEvent>>,
    /// The sequence of the first event, from which older history is read
    /// (`events.page`'s `before`); `None` when it is the log's first.
    pub older_cursor: Option<u64>,
    /// Display events left out inside the window.
    pub gap: Option<HistoryGap>,
}

/// The newest display events of a log, `events`, within `budget` bytes of
/// JSON: the newest runs that fit whole, starting at a `run_started` or
/// `user_input_submitted`. When the newest run alone does not fit, its
/// start (through its first input), the settings and waiting events of
/// what it leaves out, a gap, and the run's newest model attempts that fit.
/// The log's last event is always sent, as a forward page sends it.
///
/// [`TailWindowReader`] gives the same reading a log one event at a time. This
/// walks back from the end and measures only what fits, for a log held in
/// memory.
#[must_use]
pub fn tail_window(events: &[Arc<StoredEvent>], view: EventsView, budget: usize) -> Window {
    newest(events, None, view, budget).unwrap_or_default()
}

/// [`tail_window`] of a log read from a run boundary on: `suffix` holds its
/// events from a `run_started` or `user_input_submitted` to its end, and
/// `header` is [`state_events`] of every event before that one. The answer
/// is the one [`tail_window`] gives for the whole log, or `None` when that
/// window would reach before `suffix`, so the whole log has to be read.
///
/// It needs no more than `header`: [`state_events`] of `header` followed by
/// later events is [`state_events`] of the log through them.
#[must_use]
pub fn tail_window_after(
    header: &[Arc<StoredEvent>],
    suffix: &[Arc<StoredEvent>],
    view: EventsView,
    budget: usize,
) -> Option<Window> {
    if suffix.first().is_none_or(|first| !is_run_boundary(first)) {
        return None;
    }
    newest(suffix, Some(header), view, budget)
}

/// Where a cache of a log's newest-first view starts: the index of the run
/// boundary from which [`tail_window_after`] answers every request of up to
/// `budget` bytes, or `None` when the whole log fits in it.
#[must_use]
pub fn cache_start(events: &[Arc<StoredEvent>], budget: usize) -> Option<usize> {
    let sizes = Sizes::new(events, EventsView::Display);
    let fit = sizes.fit_before(events.len(), budget);
    if fit == 0 {
        return None;
    }
    // The window starts at a boundary at or after `fit`, or at the last one
    // before it when the newest run does not fit: either way after the last
    // boundary before `fit`, whose events from there on do not fit. A
    // window in the full view, a smaller one, and a later log's window
    // start later still.
    (0..fit)
        .rev()
        .find(|&index| is_run_boundary(&events[index]))
}

/// [`tail_window`] of `events`, the whole log when `before` is `None`, else
/// a suffix starting at a run boundary with `before` the header of what
/// precedes it. `None` when the window would reach before such a suffix.
fn newest(
    events: &[Arc<StoredEvent>],
    before: Option<&[Arc<StoredEvent>]>,
    view: EventsView,
    budget: usize,
) -> Option<Window> {
    let Some(last) = events.len().checked_sub(1) else {
        return before.is_none().then(Window::default);
    };
    // The settings before `events[end]`, and where older history starts.
    let header = |end: usize| match before {
        None => state_events(&events[..end]),
        Some(before) => state_events(
            &before
                .iter()
                .chain(&events[..end])
                .cloned()
                .collect::<Vec<_>>(),
        ),
    };
    let older_cursor = |start: usize| (start > 0 || before.is_some()).then(|| events[start].seq);
    let sizes = Sizes::new(events, view);
    let fit = sizes.fit_before(events.len(), budget);
    if fit == 0 && before.is_some() {
        // Everything held fits: older runs may too.
        return None;
    }
    let run_start = (fit..=last).find(|&index| is_run_boundary(&events[index]));
    if fit == 0 || run_start.is_some() {
        let start = if fit == 0 {
            0
        } else {
            run_start.unwrap_or(fit)
        };
        return Some(whole(
            events,
            view,
            start,
            events.len(),
            true,
            header(start),
            older_cursor(start),
        ));
    }
    // The newest run does not fit: its head, a gap, and its newest
    // attempts.
    let run = (0..=last)
        .rev()
        .find(|&index| is_run_boundary(&events[index]))
        .unwrap_or(0);
    let head_end = (run..=last)
        .take_while(|&index| {
            index == run
                || !matches!(
                    events[index].payload,
                    EventPayload::ModelAttemptStarted { .. }
                )
        })
        .find(|&index| {
            matches!(
                events[index].payload,
                EventPayload::UserInputSubmitted { .. }
            )
        })
        .unwrap_or(run)
        + 1;
    let head_bytes = sizes.sum(run, head_end);
    let tail_fit = sizes
        .fit_before(events.len(), budget.saturating_sub(head_bytes))
        .max(head_end);
    let attempts = |range: std::ops::RangeInclusive<usize>| {
        range
            .into_iter()
            .filter(|&index| {
                matches!(
                    events[index].payload,
                    EventPayload::ModelAttemptStarted { .. }
                )
            })
            .collect::<Vec<_>>()
    };
    let tail_start = attempts(tail_fit..=last)
        .first()
        .copied()
        .or_else(|| attempts(head_end..=last).last().copied())
        .unwrap_or(tail_fit)
        .min(last);
    if tail_start <= head_end {
        return Some(whole(
            events,
            view,
            run,
            events.len(),
            true,
            header(run),
            older_cursor(run),
        ));
    }
    let skipped = &events[head_end..tail_start];
    let kept = kept_in_gap(skipped, &events[tail_start..]);
    let sent = events[run..head_end]
        .iter()
        .chain(&kept)
        .chain(&events[tail_start..])
        .cloned()
        .collect::<Vec<_>>();
    let (window_events, cut) = view_events(&sent, view, true);
    let skipped_count = skipped
        .iter()
        .filter(|event| view.is_full() || !leaves_out(&event.payload))
        .count()
        .saturating_sub(kept.len());
    Some(Window {
        events: window_events,
        cut,
        header: header(run),
        older_cursor: older_cursor(run),
        gap: Some(HistoryGap {
            after_seq: events[head_end - 1].seq,
            before_seq: events[tail_start].seq,
            skipped: skipped_count as u64,
        }),
    })
}

/// The newest display events of `events`, a log, before sequence `before`,
/// within `budget` bytes: starting at the earliest run or model attempt
/// that fits, else at the earliest event that does, and at least one event
/// while any precede `before`.
///
/// [`OlderPageReader`] gives the same reading a log one event at a time. This
/// walks back from the end and measures only what fits, for a log held in
/// memory.
#[must_use]
pub fn older_page(
    events: &[Arc<StoredEvent>],
    before: u64,
    view: EventsView,
    budget: usize,
) -> Window {
    let end = events.partition_point(|event| event.seq < before);
    if end == 0 {
        return Window::default();
    }
    let sizes = Sizes::new(&events[..end], view);
    let fit = sizes.fit_before(end, budget).min(end - 1);
    let start = (fit..end)
        .find(|&index| {
            is_run_boundary(&events[index])
                || matches!(
                    events[index].payload,
                    EventPayload::ModelAttemptStarted { .. }
                )
        })
        .unwrap_or(fit);
    whole(
        events,
        view,
        start,
        end,
        false,
        state_events(&events[..start]),
        (start > 0).then(|| events[start].seq),
    )
}

/// [`tail_window`] of a log read one event at a time. It holds the events from
/// the last run boundary whose events on do not fit the budget, which the
/// window can reach back to, and folds what came before: so at least the newest
/// run, whatever its size, while the window is worked out.
#[derive(Clone, Debug)]
pub struct TailWindowReader {
    view: EventsView,
    budget: usize,
    events: std::collections::VecDeque<Arc<StoredEvent>>,
    /// The position in the log of the first event held.
    first: usize,
    /// How many events it read.
    read: usize,
    /// The bytes of the events it read.
    total: usize,
    /// The run boundaries held whose events on still fit: each one's position,
    /// and the bytes of the events before it.
    boundaries: std::collections::VecDeque<(usize, usize)>,
    before: StateEventsFold,
}

impl TailWindowReader {
    #[must_use]
    pub fn new(view: EventsView, budget: usize) -> Self {
        Self {
            view,
            budget,
            events: std::collections::VecDeque::new(),
            first: 0,
            read: 0,
            total: 0,
            boundaries: std::collections::VecDeque::new(),
            before: StateEventsFold::default(),
        }
    }

    /// Reads the next event of the log.
    pub fn observe(&mut self, event: Arc<StoredEvent>) {
        if is_run_boundary(&event) {
            self.boundaries.push_back((self.read, self.total));
        }
        self.total = self.total.saturating_add(view_size(&event, self.view));
        self.read += 1;
        self.events.push_back(event);
        let mut last_over = None;
        while let Some(&(at, before)) = self.boundaries.front()
            && self.total - before > self.budget
        {
            last_over = Some(at);
            self.boundaries.pop_front();
        }
        if let Some(at) = last_over {
            for event in self.events.drain(..at - self.first) {
                self.before.observe(&event);
            }
            self.first = at;
        }
    }

    /// The window of the events it read.
    #[must_use]
    pub fn window(self) -> Window {
        let events = Vec::from(self.events);
        if self.first == 0 {
            return newest(&events, None, self.view, self.budget).unwrap_or_default();
        }
        // It lets go of events only from a boundary whose events on do not
        // fit, and those are the ones it still holds.
        let header = self.before.finish();
        newest(&events, Some(&header), self.view, self.budget)
            .expect("the events held do not fit the budget")
    }

    /// [`cache_start`] of the events it read in the display view: the event a
    /// cache starts at, with [`state_events`] of everything before it. `None`
    /// when the whole log fits.
    #[must_use]
    pub fn cache_start(mut self) -> Option<(Arc<StoredEvent>, Vec<Arc<StoredEvent>>)> {
        let events = Vec::from(self.events);
        let start = cache_start(&events, self.budget)?;
        for event in &events[..start] {
            self.before.observe(event);
        }
        Some((Arc::clone(&events[start]), self.before.finish()))
    }
}

/// Where a cache of a log's newest-first view starts, and the header of what
/// precedes it, kept up to date one event at a time for a log that no longer
/// holds its events (`docs/site/specs/forever-sessions.md` section 12).
///
/// The start is a run boundary whose events on do not fit the budget, as
/// [`cache_start`]'s is, so [`tail_window_after`] answers from it. It is not
/// the latest such boundary: the fold notes the header at a boundary every
/// quarter of the budget, and those are the starts it can move to. So it holds a
/// few headers, whose events they mostly share, and the events it read since it
/// last worked its header out, whatever the number of runs a budget's bytes
/// span, and reads nothing to move.
#[derive(Clone, Debug)]
pub struct CacheStartFold {
    budget: usize,
    /// The bytes of the events it measured.
    total: usize,
    /// The boundaries it can move to, each with the bytes measured before it.
    noted: std::collections::VecDeque<(NotedStart, usize)>,
    /// Where the bytes of the last boundary noted reached.
    noted_at: usize,
    start: Option<NotedStart>,
    /// [`state_events`] of every event it read.
    header: StateEventsFold,
}

/// A boundary of a log, with the header of what precedes it.
#[derive(Clone, Debug)]
struct NotedStart {
    boundary: CacheBoundary,
    header: Vec<Arc<StoredEvent>>,
}

/// A run boundary of a log: where a cache of its view can start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheBoundary {
    pub seq: u64,
    pub timestamp: jiff::Timestamp,
}

impl CacheBoundary {
    fn of(event: &StoredEvent) -> Self {
        Self {
            seq: event.seq,
            timestamp: event.timestamp,
        }
    }
}

impl CacheStartFold {
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            total: 0,
            noted: std::collections::VecDeque::new(),
            noted_at: 0,
            start: None,
            header: StateEventsFold::default(),
        }
    }

    /// The fold of `events`, a log's events from its start, held in memory,
    /// starting at [`cache_start`] of them. It measures only what fits, walking
    /// back from the end, and reads on from there.
    #[must_use]
    pub fn of(events: &[Arc<StoredEvent>], budget: usize) -> Self {
        let mut fold = Self::new(budget);
        let sizes = Sizes::new(events, EventsView::Display);
        if sizes.fit_before(events.len(), budget) == 0 {
            for event in events {
                fold.observe(event);
            }
            return fold;
        }
        let start = cache_start(events, budget);
        if let Some(start) = start {
            fold.start = Some(NotedStart {
                boundary: CacheBoundary::of(&events[start]),
                header: state_events(&events[..start]),
            });
        }
        for event in events {
            fold.header.observe(event);
        }
        fold
    }

    /// Reads the next event of the log.
    pub fn observe(&mut self, event: &Arc<StoredEvent>) {
        if is_run_boundary(event)
            && (self.noted.is_empty() || self.total - self.noted_at >= self.budget / 4)
        {
            self.header.settle();
            self.noted.push_back((
                NotedStart {
                    boundary: CacheBoundary::of(event),
                    header: self.header.held.clone(),
                },
                self.total,
            ));
            self.noted_at = self.total;
        }
        self.header.observe(event);
        self.total = self
            .total
            .saturating_add(view_size(event, EventsView::Display));
        while let Some((_, before)) = self.noted.front()
            && self.total - before > self.budget
        {
            let (noted, _) = self.noted.pop_front().expect("just looked at it");
            self.start = Some(noted);
        }
    }

    /// The boundary a cache starts at. `None` when no boundary is yet known
    /// whose events on do not fit.
    #[must_use]
    pub fn start(&self) -> Option<CacheBoundary> {
        self.start.as_ref().map(|start| start.boundary)
    }

    /// [`state_events`] of every event before [`Self::start`].
    #[must_use]
    pub fn header(&self) -> &[Arc<StoredEvent>] {
        self.start.as_ref().map_or(&[], |start| &start.header)
    }
}

/// [`older_page`] of a log's events before the page, read one event at a time.
/// It holds what fits the budget, and folds what no longer does.
#[derive(Clone, Debug)]
pub struct OlderPageReader {
    view: EventsView,
    budget: usize,
    /// The newest events that fit, each with its size.
    fits: std::collections::VecDeque<(Arc<StoredEvent>, usize)>,
    /// The bytes of `fits`.
    used: usize,
    /// Whether anything no longer fits.
    dropped: bool,
    before: StateEventsFold,
}

impl OlderPageReader {
    #[must_use]
    pub fn new(view: EventsView, budget: usize) -> Self {
        Self {
            view,
            budget,
            fits: std::collections::VecDeque::new(),
            used: 0,
            dropped: false,
            before: StateEventsFold::default(),
        }
    }

    /// Reads the next event before the page's end.
    pub fn observe(&mut self, event: Arc<StoredEvent>) {
        let size = view_size(&event, self.view);
        self.used = self.used.saturating_add(size);
        self.fits.push_back((event, size));
        // At least one event, however large.
        while self.used > self.budget && self.fits.len() > 1 {
            let (event, size) = self.fits.pop_front().expect("more than one event");
            self.used -= size;
            self.dropped = true;
            self.before.observe(&event);
        }
    }

    /// [`state_events`] of every event it read: the header of what starts at
    /// the page's end.
    #[must_use]
    pub fn header_at_end(&self) -> Vec<Arc<StoredEvent>> {
        let mut fold = self.before.clone();
        for (event, _) in &self.fits {
            fold.observe(event);
        }
        fold.finish()
    }

    /// The page.
    #[must_use]
    pub fn finish(mut self) -> Window {
        let events = self
            .fits
            .into_iter()
            .map(|(event, _)| event)
            .collect::<Vec<_>>();
        if events.is_empty() {
            return Window::default();
        }
        let start = events
            .iter()
            .position(|event| {
                is_run_boundary(event)
                    || matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
            })
            .unwrap_or(0);
        for event in &events[..start] {
            self.before.observe(event);
        }
        whole(
            &events,
            self.view,
            start,
            events.len(),
            false,
            self.before.finish(),
            (self.dropped || start > 0).then(|| events[start].seq),
        )
    }
}

/// How many events [`display_events`] shows of a log, read once, one event at a
/// time. It holds what later events can still replace, and not the events.
#[derive(Default)]
pub struct DisplayCount {
    read: usize,
    /// Events shown whatever follows them.
    settled: u64,
    superseded: Superseded,
    /// The events shown only while a setting of theirs is the latest: nothing
    /// else of them would be left once a later event replaces it.
    while_latest: HashSet<usize>,
}

impl DisplayCount {
    /// Reads the next event of the log.
    pub fn observe(&mut self, event: &StoredEvent) {
        let index = self.read;
        self.read += 1;
        self.superseded.observe(index, event);
        if leaves_out(&event.payload) {
            return;
        }
        let replaceable = match &event.payload {
            EventPayload::ModelToolsPublished { .. } => true,
            EventPayload::AcpSessionUpdated { update } => {
                let mut rest = update.clone();
                rest.commands = None;
                rest.starting_options = None;
                rest.tool_presentation = None;
                rest.is_empty()
            }
            _ => false,
        };
        if replaceable {
            self.while_latest.insert(index);
        } else {
            self.settled += 1;
        }
    }

    /// How many the display view shows of the events it read.
    #[must_use]
    pub fn shown(&self) -> u64 {
        let superseded = &self.superseded;
        let held = superseded
            .tools
            .iter()
            .chain(&superseded.commands)
            .chain(&superseded.starting_options)
            .chain(superseded.presentations.values())
            .copied()
            .filter(|index| self.while_latest.contains(index))
            .collect::<HashSet<_>>();
        self.settled + held.len() as u64
    }
}

/// What a client that holds `held`, the header of a window or page, lacks
/// of `header`, the header of an older page: its events that `held` holds
/// no identical event of the same sequence for (`events.page` with
/// `header: "delta"`, client protocol 1.64.0). [`merge_header`] puts them
/// together again.
#[must_use]
pub fn header_delta(
    held: &[Arc<StoredEvent>],
    header: Vec<Arc<StoredEvent>>,
) -> Vec<Arc<StoredEvent>> {
    header
        .into_iter()
        .filter(|event| {
            event_at(held, event.seq)
                .is_none_or(|held| !Arc::ptr_eq(held, event) && **held != **event)
        })
        .collect()
}

/// The header of an older page from `held`, the header the client held,
/// and `delta`, what the page's answer sent of the new one
/// ([`header_delta`]): the held events before `older_cursor`, the page's
/// first sequence, that `delta` holds no event of the same sequence for,
/// with `delta`'s, in sequence order. Every event of the older header
/// before `older_cursor` is in the newer one as it stands, or is sent, so
/// this is the page's whole header.
#[must_use]
pub fn merge_header<E: std::borrow::Borrow<StoredEvent>>(
    held: Vec<E>,
    delta: Vec<E>,
    older_cursor: Option<u64>,
) -> Vec<E> {
    let Some(start) = older_cursor else {
        return Vec::new();
    };
    let sent = delta
        .iter()
        .map(|event| event.borrow().seq)
        .collect::<HashSet<_>>();
    let mut merged = held
        .into_iter()
        .filter(|event| {
            let seq = event.borrow().seq;
            seq < start && !sent.contains(&seq)
        })
        .chain(delta)
        .collect::<Vec<_>>();
    merged.sort_by_key(|event| event.borrow().seq);
    merged
}

/// Display texts shorter than this stay in the event: a span would not be
/// much smaller.
const MIN_SPAN_BYTES: usize = 64;

/// A tool result's event with the display texts that repeat its output
/// left empty, and the [`OutputSpan`]s that name them (client protocol
/// 1.64.0); `None` when no text repeats it.
#[must_use]
pub fn output_spans(event: &StoredEvent) -> Option<(StoredEvent, Vec<OutputSpan>)> {
    let EventPayload::ToolCallTerminated { termination } = &event.payload else {
        return None;
    };
    let result = termination.result.as_ref()?;
    let output = result.output.as_str();
    let found = |text: &str, from: usize| -> Option<(usize, usize)> {
        if text.len() < MIN_SPAN_BYTES {
            return None;
        }
        let start = from + output.get(from..)?.find(text)?;
        Some((start, start + text.len()))
    };
    let span = |block: usize, segment: Option<usize>, (start, end): (usize, usize)| OutputSpan {
        seq: event.seq,
        block: block as u32,
        segment: segment.map(|segment| segment as u32),
        start: start as u64,
        end: end as u64,
    };
    let mut display = result.display.clone();
    let mut spans = Vec::new();
    // A stream's segments occur in `output` in order.
    let mut cursors = HashMap::<String, usize>::new();
    for (index, block) in display.iter_mut().enumerate() {
        let Open::Known(block) = block else {
            continue;
        };
        match block {
            DisplayBlock::Text { text, .. }
            | DisplayBlock::Markdown { text, .. }
            | DisplayBlock::Code { text, .. } => {
                if let Some(range) = found(text, 0) {
                    spans.push(span(index, None, range));
                    text.clear();
                }
            }
            DisplayBlock::Terminal { segments, .. } => {
                for (number, segment) in segments.iter_mut().enumerate() {
                    let cursor = cursors.entry(format!("{:?}", segment.stream)).or_default();
                    if let Some(range) = found(&segment.text, *cursor) {
                        *cursor = range.1;
                        spans.push(span(index, Some(number), range));
                        segment.text.clear();
                    }
                }
            }
            DisplayBlock::FileExcerpt {
                start_line, lines, ..
            } => {
                if lines.is_empty() || lines.iter().any(|line| line.contains('\n')) {
                    continue;
                }
                let numbered = crate::stored_display::numbered_lines(*start_line, lines);
                if let Some(range) = found(&numbered, 0) {
                    spans.push(span(index, None, range));
                    lines.clear();
                }
            }
            _ => {}
        }
    }
    if spans.is_empty() {
        return None;
    }
    let mut termination = termination.clone();
    if let Some(result) = termination.result.as_mut() {
        result.display = display;
    }
    Some((
        with_payload(event, EventPayload::ToolCallTerminated { termination }),
        spans,
    ))
}

/// Puts back the display texts [`output_spans`] left empty in `event`, a
/// `tool_call_terminated` event as JSON, from `spans`, those of its
/// sequence. Fails when a span is not a range of the output or names a
/// block or segment the display does not hold.
pub fn restore_output_spans(
    event: &mut serde_json::Value,
    spans: &[OutputSpan],
) -> Result<(), &'static str> {
    const NOT_A_RESULT: &str = "an output span names no tool result's display";
    let result = event
        .get_mut("payload")
        .and_then(|payload| payload.get_mut("result"))
        .and_then(serde_json::Value::as_object_mut)
        .ok_or(NOT_A_RESULT)?;
    let output = result
        .get("output")
        .and_then(serde_json::Value::as_str)
        .ok_or(NOT_A_RESULT)?
        .to_owned();
    let display = result
        .get_mut("display")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or(NOT_A_RESULT)?;
    // Each span becomes the session log's encoding of the same text, which
    // decoding expands.
    for span in spans {
        let block = display
            .get_mut(span.block as usize)
            .and_then(serde_json::Value::as_object_mut)
            .ok_or(NOT_A_RESULT)?;
        let file_excerpt =
            block.get("type").and_then(serde_json::Value::as_str) == Some("file_excerpt");
        let target = match span.segment {
            Some(segment) => block
                .get_mut("segments")
                .and_then(|segments| segments.get_mut(segment as usize))
                .and_then(serde_json::Value::as_object_mut)
                .ok_or(NOT_A_RESULT)?,
            None => block,
        };
        let (field, member) = if file_excerpt {
            ("lines", "lines_span")
        } else {
            ("text", "text_span")
        };
        target.remove(field);
        target.insert(member.to_owned(), serde_json::json!([span.start, span.end]));
    }
    crate::stored_display::decode(&output, display)
        .map_err(|_| "an output span is not a range of its result's output")
}

/// The window of `events[start..end]` without a gap, after `header`.
fn whole(
    events: &[Arc<StoredEvent>],
    view: EventsView,
    start: usize,
    end: usize,
    keep_last: bool,
    header: Vec<Arc<StoredEvent>>,
    older_cursor: Option<u64>,
) -> Window {
    let (window_events, cut) = view_events(&events[start..end], view, keep_last);
    Window {
        events: window_events,
        cut,
        header,
        older_cursor,
        gap: None,
    }
}

/// Where a window may start: a run, or an input the user submitted.
fn is_run_boundary(event: &StoredEvent) -> bool {
    matches!(
        event.payload,
        EventPayload::RunStarted { .. } | EventPayload::UserInputSubmitted { .. }
    )
}

/// An upper bound of `event`'s size in `view`: what [`display_events`] leaves
/// out of a superseded setting is counted.
fn view_size(event: &Arc<StoredEvent>, view: EventsView) -> usize {
    match view {
        EventsView::Full => serialized_len(&**event) + 1,
        EventsView::Display => shown(event).map_or(0, |(event, _)| serialized_len(&*event) + 1),
    }
}

/// The size of each event as the display view sends it, measured when a
/// window first needs it.
struct Sizes<'a> {
    events: &'a [Arc<StoredEvent>],
    view: EventsView,
    measured: Vec<std::cell::Cell<Option<usize>>>,
}

impl<'a> Sizes<'a> {
    fn new(events: &'a [Arc<StoredEvent>], view: EventsView) -> Self {
        Self {
            events,
            view,
            measured: vec![std::cell::Cell::new(None); events.len()],
        }
    }

    fn of(&self, index: usize) -> usize {
        if let Some(size) = self.measured[index].get() {
            return size;
        }
        let size = view_size(&self.events[index], self.view);
        self.measured[index].set(Some(size));
        size
    }

    fn sum(&self, start: usize, end: usize) -> usize {
        (start..end).map(|index| self.of(index)).sum()
    }

    /// The smallest index from which the events before `end` take at most
    /// `budget` bytes; `end` when not even the one before it fits.
    fn fit_before(&self, end: usize, budget: usize) -> usize {
        let mut used = 0_usize;
        let mut start = end;
        while start > 0 {
            used = used.saturating_add(self.of(start - 1));
            if used > budget {
                break;
            }
            start -= 1;
        }
        start
    }
}

/// The event at `seq` in a log's events, which are in sequence order.
#[must_use]
pub fn event_at(events: &[Arc<StoredEvent>], seq: u64) -> Option<&Arc<StoredEvent>> {
    events
        .binary_search_by_key(&seq, |event| event.seq)
        .ok()
        .map(|index| &events[index])
}

fn with_payload(event: &StoredEvent, payload: EventPayload) -> StoredEvent {
    StoredEvent {
        engine_version: event.engine_version.clone(),
        origin: event.origin.clone(),
        session_id: event.session_id,
        run_id: event.run_id,
        seq: event.seq,
        timestamp: event.timestamp,
        payload,
    }
}

/// The largest index at most `index` that is a char boundary of `text`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    (0..=index)
        .rev()
        .find(|&boundary| text.is_char_boundary(boundary))
        .unwrap_or(0)
}

/// A value's serialized size, counted without building its JSON.
fn serialized_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        Err(_) => usize::MAX / 4,
    }
}

#[cfg(test)]
#[path = "display_view_tests.rs"]
mod tests;

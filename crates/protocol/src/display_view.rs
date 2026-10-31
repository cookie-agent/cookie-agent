//! The display view of a session's history and its newest-first windows
//! (`docs/site/specs/session-storage-v3.md` section 3, client protocol
//! 1.57.0).
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
    QueuedInputs, ReplayDisposition, StoredEvent, ToolCallId,
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
        let mut superseded = Self {
            tools: None,
            commands: None,
            starting_options: None,
            presentations: HashMap::new(),
        };
        for (index, event) in events.iter().enumerate() {
            match &event.payload {
                EventPayload::ModelToolsPublished { .. } => superseded.tools = Some(index),
                EventPayload::AcpSessionUpdated { update } => {
                    if update.commands.is_some() {
                        superseded.commands = Some(index);
                    }
                    if update.starting_options.is_some() {
                        superseded.starting_options = Some(index);
                    }
                    if let Some(presented) = &update.tool_presentation {
                        superseded
                            .presentations
                            .insert(presented.tool_call_id, index);
                    }
                }
                _ => {}
            }
        }
        superseded
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
#[must_use]
pub fn tail_window(events: &[Arc<StoredEvent>], view: EventsView, budget: usize) -> Window {
    let Some(last) = events.len().checked_sub(1) else {
        return Window::default();
    };
    let sizes = Sizes::new(events, view);
    let fit = sizes.fit_before(events.len(), budget);
    let run_start = (fit..=last).find(|&index| is_run_boundary(&events[index]));
    if fit == 0 || run_start.is_some() {
        let start = if fit == 0 {
            0
        } else {
            run_start.unwrap_or(fit)
        };
        return whole(events, view, start, events.len(), true);
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
        return whole(events, view, run, events.len(), true);
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
    Window {
        events: window_events,
        cut,
        header: state_events(&events[..run]),
        older_cursor: (run > 0).then(|| events[run].seq),
        gap: Some(HistoryGap {
            after_seq: events[head_end - 1].seq,
            before_seq: events[tail_start].seq,
            skipped: skipped_count as u64,
        }),
    }
}

/// The newest display events of `events`, a log, before sequence `before`,
/// within `budget` bytes: starting at the earliest run or model attempt
/// that fits, else at the earliest event that does, and at least one event
/// while any precede `before`.
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
    whole(events, view, start, end, false)
}

/// The window of `events[start..end]` without a gap.
fn whole(
    events: &[Arc<StoredEvent>],
    view: EventsView,
    start: usize,
    end: usize,
    keep_last: bool,
) -> Window {
    let (window_events, cut) = view_events(&events[start..end], view, keep_last);
    Window {
        events: window_events,
        cut,
        header: state_events(&events[..start]),
        older_cursor: (start > 0).then(|| events[start].seq),
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

    /// An upper bound of event `index`'s size in the display view: what
    /// [`display_events`] leaves out of a superseded setting is counted.
    fn of(&self, index: usize) -> usize {
        if let Some(size) = self.measured[index].get() {
            return size;
        }
        let event = &self.events[index];
        let size = match self.view {
            EventsView::Full => serialized_len(&**event) + 1,
            EventsView::Display => shown(event).map_or(0, |(event, _)| serialized_len(&*event) + 1),
        };
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

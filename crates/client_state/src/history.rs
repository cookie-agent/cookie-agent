//! A session's history as the display view holds it (client protocol
//! 1.59.0, `docs/site/specs/session-storage-v3.md` section 3).
//!
//! A client that asks for the display view opens a session at its newest
//! messages: the daemon answers with the newest runs, the header events the
//! session's state depends on, and what it left out (older history, and a
//! gap inside a run larger than its budget). The store keeps those events,
//! the live events after them in the display view, and the live-only output
//! still in flight, and folds them again whenever older history arrives or
//! a cut tool result is read whole: the display events are small, so a
//! fold is cheaper than a reducer that can prepend.
//!
//! The fold starts from the header: its events are reduced into a scratch
//! projection whose latest-wins state (agent, model, ACP settings, tools,
//! goal, attachments, hold, queued inputs) seeds the projection the held
//! events then reduce onto, so no row of an older event shows and no older
//! setting overrides a newer one.

use std::collections::BTreeMap;

use cookie_agent_protocol::{
    CutOutput, EventPayload, EventReadParams, EventsPageParams, EventsView, HistoryGap, PageHeader,
    SessionId, StoredEvent, ToolCallId, TransientEvent,
};
use cookie_agent_protocol_transport::{EventsPage, ReplayWindow};

use crate::{
    Replayed, SessionState, StateStore, TranscriptItem, push_unknown_event_row,
    reduce::{push_item, reduce_event_from, reduce_transient},
};

/// A tool result the display view cut: the call shows the result's first
/// bytes until it is read whole with `event.read`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CutResult {
    /// The `tool_call_terminated` event's sequence.
    pub seq: u64,
    /// The whole result's size in bytes.
    pub bytes: u64,
    pub state: CutState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CutState {
    /// Only its first bytes are here.
    Cut,
    /// Its read is in flight.
    Loading,
    /// Its read failed, for this reason; it can be tried again.
    Failed(String),
}

/// What of a session's history the display view has not loaded: the
/// transcript shows a [`TranscriptItem::History`] row for each. Default
/// when the projection holds the session's whole history.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryState {
    /// History before this sequence is not loaded.
    pub older_cursor: Option<u64>,
    /// Display events left out inside the newest run.
    pub gap: Option<HistoryGap>,
    /// The `before` of the page being read.
    pub loading: Option<u64>,
    /// Why the last page could not be read.
    pub failed: Option<String>,
}

/// A session's held history (see the module docs).
#[derive(Clone, Debug, Default)]
pub(crate) struct HeldHistory {
    /// The settings before the oldest held event.
    header: Vec<StoredEvent>,
    /// Every held event by sequence, in the display view.
    events: BTreeMap<u64, Replayed>,
    /// Live-only output whose durable successor has not arrived.
    in_flight: Vec<TransientEvent>,
    /// The held results the view cut, by sequence: the call and the whole
    /// result's size.
    cut: BTreeMap<u64, (ToolCallId, u64)>,
    older_cursor: Option<u64>,
    gap: Option<HistoryGap>,
    /// The ids the fold numbers its rows and parts from.
    base: Ids,
    /// The ids the last fold had reached at the events older history joins
    /// at: its first held event, and the gap's newer side.
    anchors: Vec<(u64, Ids)>,
    /// Where the page just added joins the held history: the next fold
    /// numbers from there as the last did, so the rows a client shows keep
    /// their ids, and with them what it expanded.
    joined_at: Option<u64>,
}

/// The next transcript row and assistant part ids of a projection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Ids {
    rows: u64,
    parts: u64,
}

impl Ids {
    /// Where a first fold starts: far enough up that the rows of every
    /// older page fit below.
    const FIRST: Self = Self {
        rows: 1 << 40,
        parts: 1 << 40,
    };

    fn of(state: &SessionState) -> Self {
        Self {
            rows: state.next_transcript_id,
            parts: state.next_live_part_id,
        }
    }
}

impl HeldHistory {
    /// The history a newest-first replay delivered.
    pub(crate) fn from_window(
        window: ReplayWindow,
        cut: &[CutOutput],
        events: Vec<Replayed>,
    ) -> Self {
        let mut held = Self {
            header: window.header,
            older_cursor: window.older_cursor,
            gap: window.gap,
            base: Ids::FIRST,
            ..Self::default()
        };
        held.extend(events, cut);
        held
    }

    /// Holds display events a replay or a page delivered, with the results
    /// among them the view cut. An event already held stays as it is.
    pub(crate) fn extend(&mut self, events: Vec<Replayed>, cut: &[CutOutput]) {
        for event in events {
            let seq = event.seq();
            if let Some(output) = cut.iter().find(|output| output.seq == seq)
                && let Replayed::Event(event) = &event
                && let EventPayload::ToolCallTerminated { termination } = &event.payload
            {
                self.cut
                    .entry(seq)
                    .or_insert((termination.tool_call_id, output.bytes));
            }
            self.events.entry(seq).or_insert(event);
        }
    }

    /// Holds a live event, its tool result cut as the display view cuts
    /// it, and drops the live-only output it supersedes. It is otherwise
    /// held whole: a fold then builds the rows the live projection built.
    pub(crate) fn hold_live(&mut self, event: &StoredEvent) {
        self.settle(event.run_id, &event.payload);
        let (event, cut) = match cookie_agent_protocol::display_view::cut_result(event) {
            Some((event, cut)) => (event, Some(cut)),
            None => (event.clone(), None),
        };
        self.extend(vec![Replayed::Event(Box::new(event))], cut.as_slice());
    }

    pub(crate) fn hold_unknown(&mut self, event: cookie_agent_protocol::UnknownEvent) {
        self.events
            .entry(event.seq)
            .or_insert(Replayed::Unknown(Box::new(event)));
    }

    /// Holds live-only output the projection applied, until the durable
    /// event that supersedes it.
    pub(crate) fn hold_transient(&mut self, event: TransientEvent) {
        self.in_flight.push(event);
    }

    /// Drops the live-only output a durable event supersedes: a committed
    /// or abandoned attempt's, a terminated call's, a finished internal
    /// agent's, a finished compaction's, and a finished run's.
    fn settle(&mut self, run_id: Option<cookie_agent_protocol::RunId>, payload: &EventPayload) {
        self.in_flight
            .retain(|held| match (payload, &held.payload) {
                (
                    EventPayload::ModelTurnCommitted { attempt_id, .. }
                    | EventPayload::AttemptAbandoned { attempt_id, .. },
                    EventPayload::TextDelta {
                        attempt_id: held, ..
                    }
                    | EventPayload::ReasoningDelta {
                        attempt_id: held, ..
                    },
                ) => attempt_id != held,
                (
                    EventPayload::ToolCallTerminated { termination },
                    EventPayload::ToolCallProgress { tool_call_id, .. },
                ) => termination.tool_call_id != *tool_call_id,
                (
                    EventPayload::ToolCallTerminated { termination },
                    EventPayload::AcpLiveUpdate { update },
                ) => {
                    update.context.is_some()
                        || update
                            .tool_presentation
                            .as_ref()
                            .is_none_or(|held| held.tool_call_id != termination.tool_call_id)
                }
                (
                    EventPayload::InternalAgentFinished { invocation_id, .. },
                    EventPayload::InternalAgentTextDelta {
                        invocation_id: held,
                        ..
                    },
                ) => invocation_id != held,
                (EventPayload::RunFinished { .. }, _) => run_id.is_none() || held.run_id != run_id,
                _ => true,
            });
    }

    /// The output a live-only event ends: a finished compaction drops the
    /// started one it follows, and an ACP agent's live state the values it
    /// replaces.
    pub(crate) fn settle_transient(&mut self, payload: &EventPayload) {
        match payload {
            EventPayload::NativeCompactionFinished { .. } => self.in_flight.retain(|held| {
                !matches!(held.payload, EventPayload::NativeCompactionStarted { .. })
            }),
            EventPayload::AcpLiveUpdate { update } => self.in_flight.retain(|held| {
                let EventPayload::AcpLiveUpdate { update: held } = &held.payload else {
                    return true;
                };
                let context = held.context.is_some() && update.context.is_none();
                let presentation = held.tool_presentation.as_ref().is_some_and(|held| {
                    update
                        .tool_presentation
                        .as_ref()
                        .is_none_or(|new| new.tool_call_id != held.tool_call_id)
                });
                context || presentation
            }),
            _ => {}
        }
    }

    /// The sequence the next page of history ends before, if `before` names
    /// a part of it not loaded: the gap's or the older history's.
    fn wants(&self, before: u64) -> bool {
        self.gap.is_some_and(|gap| gap.before_seq == before) || self.older_cursor == Some(before)
    }

    /// Holds a page of history read before `before`.
    fn add_page(&mut self, before: u64, page: EventsPage) {
        let first = page.events.first().map(|event| event.seq);
        let events = page
            .events
            .into_iter()
            .map(|event| match event.into_known() {
                Ok(event) => Replayed::Event(Box::new(event)),
                Err(unknown) => Replayed::Unknown(Box::new(unknown)),
            })
            .collect::<Vec<_>>();
        let shown = events.len() as u64;
        self.extend(events, &page.cut);
        self.joined_at = Some(before);
        if self.older_cursor == Some(before) {
            self.older_cursor = page.older_cursor;
            self.header = match page.header_kind {
                PageHeader::Delta => cookie_agent_protocol::display_view::merge_header(
                    std::mem::take(&mut self.header),
                    page.header,
                    page.older_cursor,
                ),
                PageHeader::Full | PageHeader::None => page.header,
            };
        } else if let Some(gap) = self.gap.filter(|gap| gap.before_seq == before) {
            // The page reached the run's start, or the log's: nothing is
            // left out between them any more.
            self.gap = match (page.older_cursor, first) {
                (Some(_), Some(first)) if first > gap.after_seq + 1 => Some(HistoryGap {
                    after_seq: gap.after_seq,
                    before_seq: first,
                    skipped: gap.skipped.saturating_sub(shown),
                }),
                _ => None,
            };
        }
    }

    /// The projection the held history folds to (see the module docs).
    /// After a page joined, its rows are numbered so the rows already shown
    /// keep their ids.
    pub(crate) fn fold(&mut self, session_id: SessionId, generation: u64) -> SessionState {
        if let Some(joined) = self.joined_at.take()
            && let Some((_, before)) = self.anchors.iter().find(|(seq, _)| *seq == joined)
        {
            let before = *before;
            let (_, dry) = self.fold_from(session_id, generation, Ids::default(), Some(joined));
            if let Some((_, reached)) = dry.iter().find(|(seq, _)| *seq == joined) {
                self.base = Ids {
                    rows: before.rows.wrapping_sub(reached.rows),
                    parts: before.parts.wrapping_sub(reached.parts),
                };
            }
        }
        let (state, anchors) = self.fold_from(session_id, generation, self.base, None);
        self.anchors = anchors;
        state
    }

    /// [`Self::fold`] numbering from `base`, with the ids reached at each
    /// event older history can join at, and at `joined`.
    fn fold_from(
        &self,
        session_id: SessionId,
        generation: u64,
        base: Ids,
        joined: Option<u64>,
    ) -> (SessionState, Vec<(u64, Ids)>) {
        let first = self.events.keys().next().copied();
        let mut seed = SessionState {
            generation,
            ..SessionState::default()
        };
        for event in self
            .header
            .iter()
            .filter(|event| first.is_none_or(|first| event.seq < first))
        {
            reduce_event_from(
                &mut seed,
                session_id,
                event.run_id,
                event.seq,
                event.timestamp,
                event.origin.clone(),
                event.payload.clone(),
            );
        }
        let mut state = SessionState::seeded(seed);
        state.next_transcript_id = base.rows;
        state.next_live_part_id = base.parts;
        let first_time = self
            .events
            .values()
            .next()
            .map_or_else(jiff::Timestamp::now, Replayed::timestamp);
        if let Some(before) = self.older_cursor {
            push_item(&mut state, first_time, |id| TranscriptItem::History {
                id,
                version: 0,
                before,
                skipped: None,
                loading: false,
                failed: None,
            });
        }
        let mut anchors = Vec::new();
        let mut gap = self.gap;
        for held in self.events.values() {
            if let Some(open) = gap
                && held.seq() >= open.before_seq
            {
                push_item(&mut state, held.timestamp(), |id| TranscriptItem::History {
                    id,
                    version: 0,
                    before: open.before_seq,
                    skipped: Some(open.skipped),
                    loading: false,
                    failed: None,
                });
                gap = None;
                anchors.push((open.before_seq, Ids::of(&state)));
            }
            if Some(held.seq()) == first || Some(held.seq()) == joined {
                anchors.push((held.seq(), Ids::of(&state)));
            }
            state.last_seq = held.seq();
            match held {
                Replayed::Event(event) => reduce_event_from(
                    &mut state,
                    session_id,
                    event.run_id,
                    event.seq,
                    event.timestamp,
                    event.origin.clone(),
                    event.payload.clone(),
                ),
                Replayed::Unknown(event) => push_unknown_event_row(&mut state, event),
            }
        }
        for (seq, (call, bytes)) in &self.cut {
            if let Some(tool) = state.tools.get_mut(call) {
                tool.cut = Some(CutResult {
                    seq: *seq,
                    bytes: *bytes,
                    state: CutState::Cut,
                });
            }
        }
        for transient in &self.in_flight {
            reduce_transient(&mut state, transient.timestamp, transient.payload.clone());
        }
        // A fold is durable history: it restores no unsent input.
        state.voided_inputs.clear();
        state.history = HistoryState {
            older_cursor: self.older_cursor,
            gap: self.gap,
            loading: None,
            failed: None,
        };
        (state, anchors)
    }

    /// Replaces the cut result at `seq` with the whole event.
    fn uncut(&mut self, event: StoredEvent) {
        self.cut.remove(&event.seq);
        self.events
            .insert(event.seq, Replayed::Event(Box::new(event)));
    }
}

impl SessionState {
    /// A fresh projection with the latest-wins state of `seed`, a scratch
    /// projection of the header events: what a fold of later events
    /// starts from.
    pub(crate) fn seeded(seed: SessionState) -> SessionState {
        SessionState {
            generation: seed.generation,
            created_at: seed.created_at,
            last_agent_activity: seed.last_agent_activity,
            active_run: seed.active_run,
            cwd_identity: seed.cwd_identity,
            context_tokens: seed.context_tokens,
            context_window: seed.context_window,
            run_agent: seed.run_agent,
            creation_agent: seed.creation_agent,
            run_snapshot: seed.run_snapshot,
            model_tools: seed.model_tools,
            model_deferred_tools: seed.model_deferred_tools,
            deferred_announcements: seed.deferred_announcements,
            deferred_tool_groups: seed.deferred_tool_groups,
            model_selection: seed.model_selection,
            runtime_attachments: seed.runtime_attachments,
            goal: seed.goal,
            goal_revisions: seed.goal_revisions,
            acp: seed.acp,
            held: seed.held,
            queued_inputs: seed.queued_inputs,
            memory_turn: seed.memory_turn,
            agent_md_latest_run: seed.agent_md_latest_run,
            terminal_runs: seed.terminal_runs,
            ..SessionState::default()
        }
    }
}

impl StateStore {
    /// What the transcript of `session_id` has not loaded, when the display
    /// view holds it.
    #[must_use]
    pub fn holds_history(&self, session_id: SessionId) -> bool {
        self.histories.contains_key(&session_id)
    }

    /// Starts reading the history a [`TranscriptItem::History`] row with
    /// `before` stands for: the `events.page` request to send, whose answer
    /// goes to [`Self::apply_history_page`]. `None` when that history is
    /// loaded, or a page is being read.
    pub fn request_history(
        &mut self,
        session_id: SessionId,
        before: u64,
    ) -> Option<EventsPageParams> {
        let held = self.histories.get(&session_id)?;
        let state = self.sessions.get_mut(&session_id)?;
        if !held.wants(before) || state.history.loading.is_some() {
            return None;
        }
        state.history.loading = Some(before);
        state.history.failed = None;
        bump_history_row(state, before);
        // Older history replaces the header this holds, which the answer
        // needs to send only what changes of; a gap's page leaves it.
        let header = if held.older_cursor == Some(before) {
            PageHeader::Delta
        } else {
            PageHeader::None
        };
        Some(EventsPageParams {
            session_id,
            before,
            view: EventsView::Display,
            max_bytes: None,
            header,
        })
    }

    /// The newest history of `session_id` not loaded: the gap inside its
    /// newest run first, then older history. For a client that loads
    /// history as the user scrolls up.
    #[must_use]
    pub fn next_history(&self, session_id: SessionId) -> Option<u64> {
        let held = self.histories.get(&session_id)?;
        held.gap.map(|gap| gap.before_seq).or(held.older_cursor)
    }

    /// The answer to [`Self::request_history`]'s request for `before`:
    /// the page is held and everything held folds again, or the failure
    /// shows on the row. An answer nothing waits for any more is dropped.
    pub fn apply_history_page(
        &mut self,
        session_id: SessionId,
        before: u64,
        page: Result<EventsPage, String>,
    ) {
        let waiting = self
            .sessions
            .get(&session_id)
            .is_some_and(|state| state.history.loading == Some(before));
        let Some(held) = self.histories.get_mut(&session_id).filter(|_| waiting) else {
            return;
        };
        match page {
            Ok(page) => {
                held.add_page(before, page);
                self.refold(session_id);
            }
            Err(error) => {
                if let Some(state) = self.sessions.get_mut(&session_id) {
                    state.history.loading = None;
                    state.history.failed = Some(error);
                    bump_history_row(state, before);
                }
            }
        }
    }

    /// Starts reading the whole result of `call_id`, which the display view
    /// cut: the `event.read` request to send, whose answer goes to
    /// [`Self::apply_cut_result`]. `None` when the call is not cut or its
    /// read is in flight.
    pub fn request_cut_result(
        &mut self,
        session_id: SessionId,
        call_id: ToolCallId,
    ) -> Option<EventReadParams> {
        let state = self.sessions.get_mut(&session_id)?;
        let tool = state.tools.get(&call_id)?;
        let cut = tool.cut.as_ref()?;
        if cut.state == CutState::Loading {
            return None;
        }
        let seq = cut.seq;
        state.update_tool(call_id, |tool| {
            if let Some(cut) = &mut tool.cut {
                cut.state = CutState::Loading;
            }
        });
        Some(EventReadParams { session_id, seq })
    }

    /// The answer to [`Self::request_cut_result`]: the whole result
    /// replaces the cut one and the history folds again, or the failure
    /// shows on the call.
    pub fn apply_cut_result(
        &mut self,
        session_id: SessionId,
        call_id: ToolCallId,
        read: Result<Option<StoredEvent>, String>,
    ) {
        let whole = match read {
            Ok(Some(event))
                if matches!(
                    &event.payload,
                    EventPayload::ToolCallTerminated { termination }
                        if termination.tool_call_id == call_id
                ) =>
            {
                Ok(event)
            }
            Ok(_) => Err("the session no longer holds this result".to_owned()),
            Err(error) => Err(error),
        };
        match whole {
            Ok(event) => {
                if let Some(held) = self.histories.get_mut(&session_id) {
                    held.uncut(event);
                    self.refold(session_id);
                }
            }
            Err(error) => {
                if let Some(state) = self.sessions.get_mut(&session_id) {
                    state.update_tool(call_id, |tool| {
                        if let Some(cut) = &mut tool.cut {
                            cut.state = CutState::Failed(error);
                        }
                    });
                }
            }
        }
    }

    /// Folds `session_id`'s held history again and puts the result in place
    /// of its projection, keeping what no event holds: the background
    /// calls, the input owed to the composer, approvals a list added, and
    /// the cursor.
    pub(crate) fn refold(&mut self, session_id: SessionId) {
        let (Some(held), Some(previous)) = (
            self.histories.get_mut(&session_id),
            self.sessions.get_mut(&session_id),
        ) else {
            return;
        };
        let mut state = held.fold(session_id, previous.generation);
        state.last_seq = state.last_seq.max(previous.last_seq);
        state.background_calls = std::mem::take(&mut previous.background_calls);
        state.voided_inputs = std::mem::take(&mut previous.voided_inputs);
        for approval in &previous.approvals {
            if approval.escalated
                && !state
                    .approvals
                    .iter()
                    .any(|held| held.approval_id == approval.approval_id)
            {
                state.approvals.push(approval.clone());
            }
        }
        state.version = previous.version.max(state.version).wrapping_add(1);
        *previous = state;
    }
}

/// Shows on the [`TranscriptItem::History`] row for `before` whether its
/// read is in flight or failed, marked changed for a client that caches
/// rows.
fn bump_history_row(state: &mut SessionState, before: u64) {
    for item in &mut state.transcript {
        if let TranscriptItem::History {
            before: row,
            version,
            loading,
            failed,
            ..
        } = item
            && *row == before
        {
            *version = version.wrapping_add(1);
            *loading = state.history.loading == Some(before);
            failed.clone_from(&state.history.failed);
        }
    }
    state.version = state.version.wrapping_add(1);
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;

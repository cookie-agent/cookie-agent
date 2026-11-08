//! What a native model's context is assembled from: the records since the
//! window's start, and the records before it that what follows still names
//! (`docs/site/specs/forever-sessions.md` section 5.1).
//!
//! The window starts where the latest checkpoint's kept range does. Before
//! it, a log keeps only:
//!
//! - **back-references:** an applied input's submission, a started call's
//!   turn, an ended call's start and its link to a child session, and, at any
//!   moment, the records of every call still open and every input not yet
//!   applied, so a record appended later names nothing the log let go of;
//! - **producer messages not yet consumed:** each one's acceptance and
//!   admission, and the acceptance of a message admitted in the window;
//! - **what assembly reads beside what it renders:** the latest run's
//!   `run_started`, which every record of the run names, its latest
//!   `system_prompt_composed`, the system prompt its requests carry, and the
//!   replay cut that reaches furthest;
//! - **under a checkpoint without `context_fresh`**, which an earlier build
//!   wrote, every loaded skill and the latest `AGENTS.md` and memory records,
//!   as that build pinned them. A checkpoint with it recorded its own just
//!   before itself, in the window.
//!
//! [`model_context`] applied to what it returned, with what was appended
//! since, gives what it gives for the whole log: the log can let go of the
//! rest, and a test of every log that asks for this checks it.

use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    sync::Arc,
};

use cookie_agent_protocol::{
    ContextCheckpoint, ContextCheckpointCommit, EventPayload, PersistedAssistantPart, RunId,
    StoredEvent, ToolCallId,
};

use super::state::LogState;

/// The latest checkpoint of `events`, with its record.
pub fn latest_checkpoint<E: Borrow<StoredEvent>>(
    events: &[E],
) -> Option<(&StoredEvent, &ContextCheckpointCommit)> {
    events.iter().rev().find_map(|event| {
        let event = event.borrow();
        match &event.payload {
            EventPayload::ContextCheckpointCommitted { commit } => Some((event, commit)),
            _ => None,
        }
    })
}

/// The latest checkpoint that carries a summary: the one a summary view
/// builds on. A provider window alone, which builds before 1.29.0 wrote, is
/// passed over.
pub fn latest_summary_checkpoint<E: Borrow<StoredEvent>>(
    events: &[E],
) -> Option<(&StoredEvent, &ContextCheckpointCommit)> {
    events.iter().rev().find_map(|event| {
        let event = event.borrow();
        match &event.payload {
            EventPayload::ContextCheckpointCommitted { commit }
                if matches!(commit.checkpoint, ContextCheckpoint::InternalSummary { .. }) =>
            {
                Some((event, commit))
            }
            _ => None,
        }
    })
}

/// The first record a request can read after `commit`: its summary's kept
/// range, or what follows it, and what follows its provider window.
fn commit_window_start(commit: &ContextCheckpointCommit) -> u64 {
    let after_source = commit.boundaries.source_through_seq.saturating_add(1);
    match &commit.checkpoint {
        ContextCheckpoint::InternalSummary { native, .. } => commit
            .boundaries
            .recent_from_seq
            .unwrap_or(after_source)
            .min(native.as_ref().map_or(u64::MAX, |native| {
                native.source_through_seq.saturating_add(1)
            })),
        ContextCheckpoint::NativeWindow { .. } => after_source,
    }
}

/// Where a native model's context window starts: the first record a request
/// on any model reads verbatim since the latest checkpoint, the earlier of
/// its summary's kept range and what follows its provider window. Under a
/// provider window alone, the latest summary's range too, or the whole log
/// when there is none, as a model that cannot use the window reads that.
/// `None` without a checkpoint.
pub fn context_window_start<E: Borrow<StoredEvent>>(events: &[E]) -> Option<u64> {
    let (_, latest) = latest_checkpoint(events)?;
    let start = commit_window_start(latest);
    Some(match latest.checkpoint {
        ContextCheckpoint::InternalSummary { .. } => start,
        ContextCheckpoint::NativeWindow { .. } => start.min(
            latest_summary_checkpoint(events)
                .map_or(0, |(_, summary)| commit_window_start(summary)),
        ),
    })
}

/// Where in `events` the `AGENTS.md` records the model reads before the
/// conversation are: the latest run's, the latest per attachment and the
/// project's, in log order. After a checkpoint with `context_fresh` in that
/// run, those it recorded just before itself.
pub fn current_agent_md_positions<E: Borrow<StoredEvent>>(events: &[E]) -> Vec<usize> {
    let latest_run = events.iter().rev().find_map(|event| {
        let event = event.borrow();
        matches!(event.payload, EventPayload::RunStarted { .. })
            .then_some((event.seq, event.run_id))
    });
    let run_start = latest_run.map_or(0, |(seq, _)| seq);
    let floor = match latest_checkpoint(events) {
        Some((checkpoint, commit)) if commit.context_fresh && checkpoint.seq > run_start => {
            commit.boundaries.input_through_seq
        }
        _ => 0,
    };
    let mut seen = HashSet::new();
    let mut latest = events
        .iter()
        .enumerate()
        .rev()
        .map(|(position, event)| (position, event.borrow()))
        .take_while(|(_, event)| event.seq > floor)
        .filter(|(_, event)| latest_run.is_none_or(|(_, run)| event.run_id == run))
        .filter(|(_, event)| match &event.payload {
            EventPayload::AgentMdLoaded {
                attachment,
                project,
                in_place: false,
                ..
            } => seen.insert((project.is_some(), attachment.as_str())),
            _ => false,
        })
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    latest.reverse();
    latest
}

/// The latest run's latest `system_prompt_composed` in `events`: the system
/// prompt its requests carry, composed afresh at a checkpoint inside it,
/// instead of its `run_started` one (`docs/site/specs/forever-sessions.md`
/// section 5.2). `None` when the run recorded none.
pub fn latest_composed_prompt<E: Borrow<StoredEvent>>(events: &[E]) -> Option<&E> {
    let mut composed = None::<&E>;
    for event in events.iter().rev() {
        match &event.borrow().payload {
            EventPayload::SystemPromptComposed { .. } if composed.is_none() => {
                composed = Some(event);
            }
            EventPayload::RunStarted { .. } => {
                let run = event.borrow().run_id;
                return composed
                    .filter(|composed| Borrow::<StoredEvent>::borrow(*composed).run_id == run);
            }
            _ => {}
        }
    }
    None
}

/// Where in `events` the latest `memory_loaded` before the conversation is,
/// empty or not.
pub fn latest_memory_position<E: Borrow<StoredEvent>>(events: &[E]) -> Option<usize> {
    events.iter().rposition(|event| {
        matches!(
            event.borrow().payload,
            EventPayload::MemoryLoaded {
                in_place: false,
                ..
            }
        )
    })
}

/// What a native model's context is assembled from, of `held`, a log's
/// records in order, or a selection of them this returned, with the records
/// appended since: the window and what is pinned before it, in order.
/// `state` is the log's own fold at `held`'s tip. `None` when that is all of
/// `held`.
#[must_use]
pub fn model_context(held: &[Arc<StoredEvent>], state: &LogState) -> Option<Vec<Arc<StoredEvent>>> {
    let start = context_window_start(held)?;
    let first = held.partition_point(|event| event.seq < start);
    if first == 0 {
        return None;
    }
    let fresh = latest_checkpoint(held).is_some_and(|(_, commit)| commit.context_fresh);
    let before = &held[..first];
    let mut kept = vec![false; first];
    let mut queue = Vec::new();
    let pin = |seq: u64, kept: &mut [bool], queue: &mut Vec<usize>| {
        if let Ok(index) = before.binary_search_by_key(&seq, |event| event.seq)
            && !kept[index]
        {
            kept[index] = true;
            queue.push(index);
        }
    };

    // What assembly reads beside what it renders.
    if let Some(run) = held
        .iter()
        .rev()
        .find(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
    {
        pin(run.seq, &mut kept, &mut queue);
    }
    if let Some(prompt) = latest_composed_prompt(held) {
        pin(prompt.seq, &mut kept, &mut queue);
    }
    if let Some(cut) = held
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::NativeReplayCut { through_seq, .. } => Some((through_seq, event.seq)),
            _ => None,
        })
        .max()
    {
        pin(cut.1, &mut kept, &mut queue);
    }
    // What an earlier build's checkpoint pinned.
    if !fresh {
        for event in before {
            if matches!(event.payload, EventPayload::SkillLoaded { .. }) {
                pin(event.seq, &mut kept, &mut queue);
            }
        }
        for position in current_agent_md_positions(held) {
            pin(held[position].seq, &mut kept, &mut queue);
        }
        if let Some(position) = latest_memory_position(held) {
            pin(held[position].seq, &mut kept, &mut queue);
        }
    }
    // Producer messages not yet consumed, and what is still open.
    for message in &state.producers().messages {
        if !message.consumed && !message.discarded {
            pin(message.accepted_seq, &mut kept, &mut queue);
            if let Some((_, admission)) = message.admission {
                pin(admission, &mut kept, &mut queue);
            }
        }
    }
    for call in state.open_calls() {
        pin(call.seq, &mut kept, &mut queue);
    }
    for submission in state.all_unapplied_submissions() {
        pin(submission, &mut kept, &mut queue);
    }

    // Back-references, from the window and from what is pinned, until
    // nothing more is reached.
    let references = References::of(held);
    queue.extend(first..held.len());
    while let Some(index) = queue.pop() {
        let event = &held[index];
        for seq in references.named_by(event) {
            pin(seq, &mut kept, &mut queue);
        }
    }

    Some(
        held.iter()
            .enumerate()
            .filter(|(index, _)| *index >= first || kept[*index])
            .map(|(_, event)| Arc::clone(event))
            .collect(),
    )
}

/// Where the records a record can name are, by what names them.
struct References<'a> {
    held: &'a [Arc<StoredEvent>],
    /// `model_turn_committed` by run and turn sequence.
    turns: HashMap<(Option<RunId>, u64), Vec<usize>>,
    /// `tool_call_started` by run and call.
    starts: HashMap<(Option<RunId>, ToolCallId), Vec<usize>>,
    /// `tool_call_linked` by run and call.
    links: HashMap<(Option<RunId>, ToolCallId), Vec<u64>>,
    /// The first `producer_message_accepted` of each message.
    accepted: HashMap<cookie_agent_protocol::ProducerMessageId, u64>,
}

impl<'a> References<'a> {
    fn of(held: &'a [Arc<StoredEvent>]) -> Self {
        let mut references = Self {
            held,
            turns: HashMap::new(),
            starts: HashMap::new(),
            links: HashMap::new(),
            accepted: HashMap::new(),
        };
        for (index, event) in held.iter().enumerate() {
            match &event.payload {
                EventPayload::ModelTurnCommitted { model_turn_seq, .. } => references
                    .turns
                    .entry((event.run_id, *model_turn_seq))
                    .or_default()
                    .push(index),
                EventPayload::ToolCallStarted { start } => references
                    .starts
                    .entry((event.run_id, start.tool_call_id))
                    .or_default()
                    .push(index),
                EventPayload::ToolCallLinked { tool_call_id, .. } => references
                    .links
                    .entry((event.run_id, *tool_call_id))
                    .or_default()
                    .push(event.seq),
                EventPayload::ProducerMessageAccepted { message_id, .. } => {
                    references.accepted.entry(*message_id).or_insert(event.seq);
                }
                _ => {}
            }
        }
        references
    }

    /// The records `event` names, as `close_event_dependencies` in the
    /// engine's history assembly follows them, and what assembly reads of
    /// them: a call's link, a message's acceptance.
    fn named_by(&self, event: &StoredEvent) -> Vec<u64> {
        let latest_before = |indexes: Option<&Vec<usize>>,
                             accept: &dyn Fn(&StoredEvent) -> bool| {
            indexes.and_then(|indexes| {
                indexes
                    .iter()
                    .rev()
                    .map(|index| &*self.held[*index])
                    .find(|candidate| candidate.seq <= event.seq && accept(candidate))
                    .map(|candidate| candidate.seq)
            })
        };
        match &event.payload {
            EventPayload::UserInputApplied { user_input_seq } => vec![*user_input_seq],
            EventPayload::ToolCallStarted { start } => {
                let turn = latest_before(
                    self.turns.get(&(event.run_id, start.owner.model_turn_seq)),
                    &|candidate| match &candidate.payload {
                        EventPayload::ModelTurnCommitted { turn, .. } => {
                            turn.content.iter().any(|part| {
                                matches!(part, PersistedAssistantPart::ToolCall { id, .. }
                                    if id == &start.owner.model_call_id)
                            })
                        }
                        _ => false,
                    },
                );
                turn.into_iter()
                    .chain(
                        self.links
                            .get(&(event.run_id, start.tool_call_id))
                            .into_iter()
                            .flatten()
                            .copied(),
                    )
                    .collect()
            }
            EventPayload::ToolCallTerminated { termination } => latest_before(
                self.starts.get(&(event.run_id, termination.tool_call_id)),
                &|_| true,
            )
            .into_iter()
            .collect(),
            EventPayload::ProducerMessageAdmitted { message_id } => {
                self.accepted.get(message_id).copied().into_iter().collect()
            }
            _ => Vec::new(),
        }
    }
}

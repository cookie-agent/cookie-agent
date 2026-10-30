//! How much of an agent's work the transcript shows in every client
//! (`docs/site/specs/reduced-transcript.md`): every thinking block and tool
//! call, or each assistant block's work folded into one `Working…` /
//! `Worked for` row above its reply.

use std::{collections::HashMap, time::Duration};

use cookie_agent_protocol::{ProducerOwner, ToolCallId};

use crate::{
    AssistantChild, EventLevel, ProducerMessageStatus, SessionState, ToolStatus, TranscriptItem,
    reply::format_elapsed,
};

/// How the transcript shows agent work.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum TranscriptMode {
    /// Each block's work folds into one row above its reply.
    #[default]
    Reduced,
    /// Every thinking block and tool call shows.
    Full,
}

impl TranscriptMode {
    /// The mode's name in configuration and storage.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Reduced => "reduced",
            Self::Full => "full",
        }
    }

    /// The mode a stored name names; `None` for any other text.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "reduced" => Some(Self::Reduced),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// The other mode.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Reduced => Self::Full,
            Self::Full => Self::Reduced,
        }
    }
}

/// Where a block's reply starts among its `children`: after its last
/// thinking part or tool call. Everything before is its work, the text
/// written between calls included. `None` when the block has no work, so
/// it shows as in the full mode.
#[must_use]
pub fn reply_start(children: &[AssistantChild]) -> Option<usize> {
    children
        .iter()
        .rposition(|child| {
            matches!(
                child,
                AssistantChild::Thinking { .. }
                    | AssistantChild::Tool { .. }
                    | AssistantChild::CommittedTool { .. }
            )
        })
        .map(|index| index + 1)
}

/// Whether the call `call_id` waits on the user's approval, so it shows
/// outside its block's fold until answered: an approval escalated to the
/// user that has not expired.
#[must_use]
pub fn needs_user(state: &SessionState, call_id: &ToolCallId) -> bool {
    state
        .approvals
        .iter()
        .any(|approval| &approval.tool_call_id == call_id && approval.is_visible_user_escalation())
}

/// What a block's work row says.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkSummary {
    /// The block still takes output: `Working…`, its time ticking.
    pub live: bool,
    /// Its run was cancelled or interrupted: `Stopped after`.
    pub stopped: bool,
    pub elapsed: Duration,
    pub tool_calls: usize,
    pub failed: usize,
}

impl WorkSummary {
    /// The row's label: `Working… 42s · 3 tool calls`, `Worked for 1m 23s`,
    /// or `Stopped after 12s`, the count left out at none.
    #[must_use]
    pub fn label(&self) -> String {
        let elapsed = format_elapsed(self.elapsed);
        let mut label = if self.live {
            format!("Working… {elapsed}")
        } else if self.stopped {
            format!("Stopped after {elapsed}")
        } else {
            format!("Worked for {elapsed}")
        };
        match self.tool_calls {
            0 => {}
            1 => label.push_str(" · 1 tool call"),
            count => label.push_str(&format!(" · {count} tool calls")),
        }
        label
    }

    /// The failed calls, shown after the label in the warning colour:
    /// `· 1 failed`; `None` at none.
    #[must_use]
    pub fn failed_label(&self) -> Option<String> {
        (self.failed > 0).then(|| format!("· {} failed", self.failed))
    }
}

/// The work row of the assistant block `item_id`, whose work is `work`
/// (its children before [`reply_start`]), at `now`. The time runs from the
/// block's durable start to its durable end, or to `now` while it is live.
#[must_use]
pub fn work_summary(
    state: &SessionState,
    item_id: u64,
    work: &[AssistantChild],
    now: jiff::Timestamp,
) -> WorkSummary {
    fold_summary(state, state.item_time(item_id), item_id, work.iter(), now)
}

/// The work row of work `work` that started at `started` and ends with the
/// block `item_id`: live while that block takes output.
fn fold_summary<'a>(
    state: &SessionState,
    started: Option<jiff::Timestamp>,
    item_id: u64,
    work: impl IntoIterator<Item = &'a AssistantChild>,
    now: jiff::Timestamp,
) -> WorkSummary {
    let end = state.assistant_ends.get(&item_id).copied();
    let live = end.is_none() && state.assistant_live(item_id);
    let ended = match end {
        Some(end) => Some(end.at),
        None if live => Some(now),
        // A block that never sealed and is not open (a run with no end in
        // the log): its last part's arrival.
        None => state.assistant_last_child_time(item_id),
    };
    tally(
        state,
        work,
        WorkSummary {
            live,
            stopped: end.is_some_and(|end| end.stopped),
            elapsed: span(started, ended),
            tool_calls: 0,
            failed: 0,
        },
    )
}

/// The time from `started` to `ended`; none when either is unknown or the
/// clock went back.
fn span(started: Option<jiff::Timestamp>, ended: Option<jiff::Timestamp>) -> Duration {
    started
        .zip(ended)
        .and_then(|(started, ended)| Duration::try_from(ended.duration_since(started)).ok())
        .unwrap_or_default()
}

/// `summary` with the tool calls of `work` and the failed ones counted.
fn tally<'a>(
    state: &SessionState,
    work: impl IntoIterator<Item = &'a AssistantChild>,
    mut summary: WorkSummary,
) -> WorkSummary {
    for child in work {
        match child {
            AssistantChild::Tool { call_id } => {
                summary.tool_calls += 1;
                if state
                    .tools
                    .get(call_id)
                    .is_some_and(|tool| tool.status == ToolStatus::Failed)
                {
                    summary.failed += 1;
                }
            }
            AssistantChild::CommittedTool { .. } => summary.tool_calls += 1,
            _ => {}
        }
    }
    summary
}

/// What a live work row says under its label: the latest text it wrote or
/// thought, and its latest tool call.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct WorkLines {
    /// The newest settled line of its newest thinking or text: a thinking
    /// part's step (its heading or a line bold alone) where it has one.
    /// `Thinking…` while a thinking part has nothing settled yet, and
    /// `Starting…` before the work says or does anything.
    pub text: Option<String>,
    /// Its newest tool call's header, `Waiting for approval: …` while the
    /// call waits on the user.
    pub tool: Option<String>,
}

/// The lines under a live work row whose `work` is these children, each
/// with its block's ID. A line still streaming is held back until it ends,
/// or until it is `room` characters long, past which a row cuts it anyway.
#[must_use]
pub fn work_lines<'a>(
    state: &SessionState,
    work: impl DoubleEndedIterator<Item = (u64, &'a AssistantChild)> + Clone,
    room: usize,
) -> WorkLines {
    let text = work.clone().rev().find_map(|(item_id, child)| match child {
        AssistantChild::Thinking { id, text, .. } => {
            settled_line(text, state.is_open_thinking(item_id, *id), room, true)
        }
        AssistantChild::Text { markdown, .. } if filler(markdown.as_str()) => None,
        AssistantChild::Text { id, markdown, .. } => settled_line(
            markdown.as_str(),
            state.is_open_text(item_id, *id),
            room,
            false,
        ),
        _ => None,
    });
    let tool = work.clone().rev().find_map(|(_, child)| match child {
        AssistantChild::Tool { call_id } => {
            let header = state.tools.get(call_id)?.header_text();
            Some(if needs_user(state, call_id) {
                format!("Waiting for approval: {header}")
            } else {
                header
            })
        }
        AssistantChild::CommittedTool { name, .. } => Some(name.as_str().to_owned()),
        _ => None,
    });
    let thinking = work.rev().find_map(|(_, child)| match child {
        AssistantChild::Thinking { .. } => Some(true),
        AssistantChild::Text { .. } => Some(false),
        _ => None,
    });
    let text = text.or_else(|| match (thinking, &tool) {
        (Some(true), _) => Some("Thinking…".to_owned()),
        (None, None) => Some("Starting…".to_owned()),
        _ => None,
    });
    WorkLines { text, tool }
}

/// The newest line of `text` a work row may show: with `streaming`, a line
/// that has ended, or the unfinished last line once it is `room`
/// characters long. With `thinking`, its newest step where it has one.
fn settled_line(text: &str, streaming: bool, room: usize, thinking: bool) -> Option<String> {
    let (ended, open) = match text.rfind('\n') {
        Some(at) if streaming => (&text[..at], text[at + 1..].trim()),
        None if streaming => ("", text.trim()),
        _ => (text, ""),
    };
    let lines = || {
        (open.chars().count() >= room)
            .then_some(open)
            .into_iter()
            .chain(ended.lines().rev())
    };
    thinking
        .then(|| lines().find_map(step))
        .flatten()
        .or_else(|| {
            lines()
                .map(crate::tool_header::flatten_header_argument)
                .find(|line| !line.is_empty())
        })
}

/// The step `line` names: a Markdown heading or a line bold alone
/// (`**Inspecting the reducer**`, as OpenAI's reasoning summaries start
/// each step), without its markers.
fn step(line: &str) -> Option<String> {
    let line = line.trim();
    let heading = line.trim_start_matches('#');
    let step = if heading.len() < line.len() && heading.starts_with(' ') {
        Some(heading.trim())
    } else {
        line.strip_prefix("**")
            .and_then(|rest| rest.strip_suffix("**"))
            .filter(|inner| !inner.is_empty() && !inner.contains("**"))
            .map(str::trim)
    };
    step.filter(|step| !step.is_empty())
        .map(crate::tool_header::flatten_header_argument)
}

/// A fold's key: where its work starts, the block and the child.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkKey {
    pub item: u64,
    pub child: u32,
}

/// Where an item stands in the reduced mode's folds.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ItemFold {
    /// An assistant block of a folded run.
    Block(BlockFold),
    /// A row inside the work of the fold `key`.
    Row { key: WorkKey },
}

/// An assistant block of a folded run: whether it is the run's first block,
/// which keeps its header, and its last, which keeps its footer, and each
/// child's place.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BlockFold {
    pub first: bool,
    pub last: bool,
    pub children: Vec<ChildFold>,
}

/// Where an assistant block's child stands in a folded run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChildFold {
    /// Shown as in the full mode: the run's reply, or a text that breaks
    /// its work.
    Shown,
    /// Part of the work of the fold `key`; `starts` on the fold's first
    /// child, above which its work row draws.
    Work { key: WorkKey, starts: bool },
}

/// A work row: what it says, and while its fold works, what it does now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkRow {
    pub summary: WorkSummary,
    /// What it says under its label while it works.
    pub lines: Option<WorkLines>,
}

/// Whether `text`, written between calls, says nothing, so it folds into the
/// work where any other text breaks it: blank, or only ellipses (`…`,
/// `...`), which Kimi models write between calls.
#[must_use]
pub fn filler(text: &str) -> bool {
    text.chars()
        .all(|char| char.is_whitespace() || matches!(char, '…' | '.'))
}

/// The reduced mode's folds of a transcript. A run's blocks that only
/// background rows came between (an ACP agent's background command ending,
/// a background call's report, a diagnostic below the warning level, a
/// producer message not shown yet) are one stretch of work, those rows
/// included; any other row ends it, and blocks of different runs never
/// join. Once the run's work ends, the stretch's last block's trailing text
/// is its reply. Before it, any text but [`filler`] shows as
/// the reply does and splits the work around it into folds of their own,
/// the one before it done. A fold with no thinking or tool call shows as in
/// the full mode, and media always shows.
#[derive(Clone, Debug, Default)]
pub struct FoldPlan {
    items: HashMap<u64, ItemFold>,
    folds: HashMap<WorkKey, Fold>,
    /// Each block's position in the transcript.
    positions: HashMap<u64, usize>,
}

/// One fold: its work's children in order, and where its work ends.
#[derive(Clone, Debug)]
struct Fold {
    children: Vec<(u64, usize)>,
    end: FoldEnd,
}

#[derive(Clone, Copy, Debug)]
enum FoldEnd {
    /// A text broke the work: it ends where that child entered.
    Text { item: u64, child: usize },
    /// The work ends with the run's block `item`.
    Run { item: u64 },
}

/// One step of a run's stretch: a block's child, or a row between blocks.
#[derive(Clone, Copy)]
enum Unit<'a> {
    Child(u64, usize, &'a AssistantChild),
    Row(u64),
}

impl FoldPlan {
    /// The folds of `state`'s transcript.
    #[must_use]
    pub fn of(state: &SessionState) -> Self {
        let mut plan = Self::default();
        let mut stretch: Option<Building<'_>> = None;
        for (position, item) in state.transcript.iter().enumerate() {
            if let TranscriptItem::Assistant { id, children, .. } = item {
                plan.positions.insert(*id, position);
                let run = state.assistant_runs.get(id).copied();
                if let Some(building) = stretch
                    .as_mut()
                    .filter(|building| run.is_some() && building.run == run)
                {
                    building.rows.append(&mut building.pending);
                    building.blocks.push((*id, children));
                    continue;
                }
                if let Some(done) = stretch.take() {
                    plan.add(state, done);
                }
                stretch = Some(Building {
                    run,
                    blocks: vec![(*id, children)],
                    rows: Vec::new(),
                    pending: Vec::new(),
                });
            } else if background_row(item) {
                if let Some(building) = stretch.as_mut() {
                    building.pending.push((item.id(), building.blocks.len()));
                }
            } else if let Some(done) = stretch.take() {
                plan.add(state, done);
            }
        }
        if let Some(done) = stretch {
            plan.add(state, done);
        }
        plan
    }

    fn add(&mut self, state: &SessionState, stretch: Building<'_>) {
        // The stretch in order: each block's children, and the rows that
        // came before each block after the first.
        let mut units = Vec::new();
        for (index, (id, children)) in stretch.blocks.iter().enumerate() {
            units.extend(
                stretch
                    .rows
                    .iter()
                    .filter(|(_, before)| *before == index)
                    .map(|(row, _)| Unit::Row(*row)),
            );
            units.extend(
                children
                    .iter()
                    .enumerate()
                    .map(|(child, part)| Unit::Child(*id, child, part)),
            );
        }
        let (last, last_children) = *stretch.blocks.last().expect("a stretch has a block");
        // The reply: once the run's work ends, the last block's trailing
        // text, all of it when the block holds no work. While it works, text
        // after its last call breaks the work as any text between calls
        // does.
        let reply = if state.assistant_live(last) {
            units.len()
        } else {
            units.len() - (last_children.len() - reply_start(last_children).unwrap_or(0))
        };
        let mut places: HashMap<(u64, usize), ChildFold> = HashMap::new();
        let mut rows = Vec::new();
        let mut start = 0;
        let mut folded = false;
        for at in 0..=reply {
            let breaks = at == reply
                || matches!(units[at], Unit::Child(_, _, AssistantChild::Text { markdown, .. }) if !filler(markdown.as_str()));
            if !breaks {
                continue;
            }
            let end = match units.get(at) {
                Some(Unit::Child(item, child, _)) if at < reply => FoldEnd::Text {
                    item: *item,
                    child: *child,
                },
                _ => FoldEnd::Run { item: last },
            };
            folded |= self.fold(&units[start..at], end, &mut places, &mut rows);
            start = at + 1;
        }
        if !folded {
            return;
        }
        let count = stretch.blocks.len();
        for (index, (id, children)) in stretch.blocks.iter().enumerate() {
            let children = (0..children.len())
                .map(|child| {
                    places
                        .get(&(*id, child))
                        .copied()
                        .unwrap_or(ChildFold::Shown)
                })
                .collect();
            self.items.insert(
                *id,
                ItemFold::Block(BlockFold {
                    first: index == 0,
                    last: index + 1 == count,
                    children,
                }),
            );
        }
        for (row, key) in rows {
            self.items.insert(row, ItemFold::Row { key });
        }
    }

    /// Folds `units`, one stretch of work ending as `end`, when it holds a
    /// thinking part or a tool call; whether it did.
    fn fold(
        &mut self,
        units: &[Unit<'_>],
        end: FoldEnd,
        places: &mut HashMap<(u64, usize), ChildFold>,
        rows: &mut Vec<(u64, WorkKey)>,
    ) -> bool {
        let worked = units.iter().any(|unit| {
            matches!(
                unit,
                Unit::Child(
                    _,
                    _,
                    AssistantChild::Thinking { .. }
                        | AssistantChild::Tool { .. }
                        | AssistantChild::CommittedTool { .. }
                )
            )
        });
        let first = units.iter().find_map(|unit| match unit {
            Unit::Child(item, child, _) => Some((*item, *child)),
            Unit::Row(_) => None,
        });
        let (true, Some((item, child))) = (worked, first) else {
            return false;
        };
        let key = WorkKey {
            item,
            child: u32::try_from(child).unwrap_or(u32::MAX),
        };
        let mut children = Vec::new();
        for unit in units {
            match unit {
                // What the model sends as media always shows.
                Unit::Child(_, _, AssistantChild::MediaFile { .. }) => {}
                Unit::Child(id, index, _) => {
                    places.insert(
                        (*id, *index),
                        ChildFold::Work {
                            key,
                            starts: (*id, *index) == (item, child),
                        },
                    );
                    children.push((*id, *index));
                }
                Unit::Row(row) => rows.push((*row, key)),
            }
        }
        self.folds.insert(key, Fold { children, end });
        true
    }

    /// Every fold's key.
    pub fn keys(&self) -> impl Iterator<Item = WorkKey> + '_ {
        self.folds.keys().copied()
    }

    /// Where the item `item_id` stands, when it is in a folded run.
    #[must_use]
    pub fn get(&self, item_id: u64) -> Option<&ItemFold> {
        self.items.get(&item_id)
    }

    /// The work row of the fold `key` at `now`, its lines cut at `room`.
    #[must_use]
    pub fn work_row(
        &self,
        state: &SessionState,
        key: WorkKey,
        now: jiff::Timestamp,
        room: usize,
    ) -> Option<WorkRow> {
        let fold = self.folds.get(&key)?;
        let child =
            |item: u64, index: usize| match state.transcript.get(*self.positions.get(&item)?) {
                Some(TranscriptItem::Assistant {
                    children,
                    child_times,
                    ..
                }) => Some((children.get(index)?, child_times.get(index).copied())),
                _ => None,
            };
        let work = fold
            .children
            .iter()
            .filter_map(|(item, index)| Some((*item, child(*item, *index)?.0)))
            .collect::<Vec<_>>();
        // A fold that starts a block starts with the block; a later one
        // when its first child entered.
        let started = (key.child > 0)
            .then(|| child(key.item, key.child as usize).and_then(|(_, time)| time))
            .flatten()
            .or_else(|| state.item_time(key.item));
        let summary = match fold.end {
            FoldEnd::Text { item, child: index } => {
                let ended = child(item, index)
                    .and_then(|(_, time)| time)
                    .or_else(|| state.item_time(item));
                tally(
                    state,
                    work.iter().map(|(_, child)| *child),
                    WorkSummary {
                        live: false,
                        stopped: false,
                        elapsed: span(started, ended),
                        tool_calls: 0,
                        failed: 0,
                    },
                )
            }
            FoldEnd::Run { item } => fold_summary(
                state,
                started,
                item,
                work.iter().map(|(_, child)| *child),
                now,
            ),
        };
        let lines = summary
            .live
            .then(|| work_lines(state, work.iter().copied(), room));
        Some(WorkRow { summary, lines })
    }
}

/// A run's stretch being gathered: its run, its blocks so far, the rows
/// between them (each with how many blocks came before it), and the rows
/// since its last block, which join it only if another of its blocks
/// follows.
struct Building<'a> {
    run: Option<cookie_agent_protocol::RunId>,
    blocks: Vec<(u64, &'a [AssistantChild])>,
    rows: Vec<(u64, usize)>,
    pending: Vec<(u64, usize)>,
}

/// Whether `item` is a background row a run's stretch takes in when the run
/// goes on past it: an ACP agent's background command that ended, a
/// background call's report, a diagnostic below the warning level, or a
/// producer message not shown yet.
fn background_row(item: &TranscriptItem) -> bool {
    match item {
        TranscriptItem::AgentBackgroundEnded { .. } => true,
        TranscriptItem::Event { level, .. } => *level < EventLevel::Warning,
        TranscriptItem::ProducerMessage {
            producer_owner,
            status,
            ..
        } => {
            matches!(producer_owner, ProducerOwner::Tool { .. })
                || matches!(
                    status,
                    ProducerMessageStatus::Pending | ProducerMessageStatus::Admitted
                )
        }
        _ => false,
    }
}

/// The step a thinking trace is on: its newest Markdown heading or line
/// that is bold alone (`**Inspecting the reducer**`, as OpenAI's reasoning
/// summaries start each step), without its markers; else its newest line
/// with text. `None` while it has none.
#[must_use]
pub fn thinking_step(text: &str) -> Option<String> {
    settled_line(text, false, usize::MAX, true)
}

#[cfg(test)]
#[path = "transcript_mode_tests.rs"]
mod tests;

//! How much of an agent's work the transcript shows in every client
//! (`docs/site/specs/reduced-transcript.md`): every thinking block and tool
//! call, or each assistant block's work folded into one `Working…` /
//! `Worked for` row above its reply.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
    time::Duration,
};

use cookie_agent_protocol::{ProducerOwner, ToolCallId};

use crate::{
    AssistantChild, EventLevel, ProducerMessageStatus, SessionState, ToolStatus, TranscriptItem,
    format::format_elapsed,
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
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
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
    /// The row's label: `Working… 42s`, `Worked for 1m 23s · 3 tool calls`,
    /// or `Stopped after 12s`, the count left out at none. A live row
    /// counts its calls on its call line instead ([`Self::calls_label`]).
    #[must_use]
    pub fn label(&self) -> String {
        let elapsed = format_elapsed(self.elapsed);
        if self.live {
            return format!("Working… {elapsed}");
        }
        let mut label = if self.stopped {
            format!("Stopped after {elapsed}")
        } else {
            format!("Worked for {elapsed}")
        };
        if self.tool_calls > 0 {
            label.push_str(&format!(" · {}", self.calls_label()));
        }
        label
    }

    /// The calls counted: `1 tool call`, `3 tool calls`, and `No tool
    /// calls yet` at none, which a live row's call line says while no call
    /// runs.
    #[must_use]
    pub fn calls_label(&self) -> String {
        match self.tool_calls {
            0 => "No tool calls yet".to_owned(),
            1 => "1 tool call".to_owned(),
            count => format!("{count} tool calls"),
        }
    }

    /// The failed calls, shown after the calls in the warning colour:
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
    /// The header of its newest tool call still running (not moved to the
    /// background), `Waiting for approval: …` while the call waits on the
    /// user; none while no call runs, when the line counts the calls
    /// ([`WorkSummary::calls_label`]).
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
    // The newest call the work still waits on: once every call ended, or
    // went on in the background, the line goes.
    let tool = work.clone().rev().find_map(|(_, child)| match child {
        AssistantChild::Tool { call_id } => {
            let call = state.tools.get(call_id)?;
            let going = call.status == crate::ToolStatus::Running && !call.backgrounded;
            if !going && !needs_user(state, call_id) {
                return None;
            }
            let header = call.header_text();
            // Cut like the text: arguments streaming on past the row's
            // room leave the line as it is.
            Some(cut_line(
                if needs_user(state, call_id) {
                    format!("Waiting for approval: {header}")
                } else {
                    header
                },
                room,
            ))
        }
        _ => None,
    });
    let called = work.clone().any(|(_, child)| {
        matches!(
            child,
            AssistantChild::Tool { .. } | AssistantChild::CommittedTool { .. }
        )
    });
    let thinking = work.rev().find_map(|(_, child)| match child {
        AssistantChild::Thinking { .. } => Some(true),
        AssistantChild::Text { .. } => Some(false),
        _ => None,
    });
    let text = text.or_else(|| match (thinking, called) {
        (Some(true), _) => Some("Thinking…".to_owned()),
        (None, false) => Some("Starting…".to_owned()),
        _ => None,
    });
    WorkLines { text, tool }
}

/// The newest line of `text` a work row may show: with `streaming`, a line
/// that has ended, or the unfinished last line once it is `room`
/// characters long. With `thinking`, its newest step where it has one.
/// A line past `room` is cut there and ends in `…`, so an open line that
/// streams on past what the row shows stays the same line, and the row
/// does not take it for a new one.
fn settled_line(text: &str, streaming: bool, room: usize, thinking: bool) -> Option<String> {
    settled_line_whole(text, streaming, room, thinking).map(|line| cut_line(line, room))
}

/// `line` cut to its first `room` characters and `…` when it is longer.
fn cut_line(line: String, room: usize) -> String {
    match line.char_indices().nth(room) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line,
    }
}

fn settled_line_whole(text: &str, streaming: bool, room: usize, thinking: bool) -> Option<String> {
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

impl WorkRow {
    /// Hashes what the row shows, for a client's row cache: a live row's
    /// time as its label counts it, so the row draws again as its label
    /// ticks and not at every frame.
    pub fn hash_shown(&self, hasher: &mut impl Hasher) {
        self.summary.live.hash(hasher);
        if self.summary.live {
            self.summary.label().hash(hasher);
            self.summary.tool_calls.hash(hasher);
            self.summary.failed.hash(hasher);
        } else {
            self.summary.hash(hasher);
        }
        self.lines.hash(hasher);
    }
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FoldPlan {
    /// Where each item stands, and [`Self::placed`]'s hash of it.
    items: Map<u64, (ItemFold, u64)>,
    folds: Map<WorkKey, Fold>,
    /// Each block's position in the transcript.
    positions: Map<u64, usize>,
    /// The work rows of the folds whose work ended, which no clock moves.
    settled: Map<WorkKey, WorkRow>,
    /// The folds still working, whose rows count to the time they draw at.
    live: Vec<WorkKey>,
    /// The stretches planned, in transcript order.
    stretches: Vec<Stretch>,
}

/// The plan's maps, under a fast fixed hash: their keys are IDs.
type Map<K, V> = HashMap<K, V, foldhash::fast::FixedState>;

/// The hash [`FoldPlan::placed`] gives `fold`: the place, and the settled
/// row of each fold it starts.
fn fold_hash(fold: &ItemFold, settled: &Map<WorkKey, WorkRow>) -> u64 {
    use std::hash::BuildHasher as _;
    let mut hasher = foldhash::fast::FixedState::default().build_hasher();
    fold.hash(&mut hasher);
    if let ItemFold::Block(block) = fold {
        for child in &block.children {
            if let ChildFold::Work { key, starts: true } = child
                && let Some(row) = settled.get(key)
            {
                row.hash_shown(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// One planned stretch: where it lies in the transcript, what it read
/// there, and what it planned.
#[derive(Clone, Debug, PartialEq)]
struct Stretch {
    /// The position of its first block.
    start: usize,
    /// The position of the item that ended it, or the transcript's length.
    end: usize,
    /// [`signature`] of what it read.
    signature: u64,
    blocks: Vec<u64>,
    /// The rows it planned into its folds.
    rows: Vec<u64>,
    folds: Vec<WorkKey>,
}

/// What the plan of the stretch from `start` to `end` read: its items, its
/// blocks' children, runs, times, and ends, whether its last block is live,
/// and how the item at `end` ended it. A stretch whose signature still
/// matches plans the same, so it stays.
fn signature(state: &SessionState, start: usize, end: usize) -> u64 {
    use std::hash::BuildHasher as _;
    let mut hasher = foldhash::fast::FixedState::default().build_hasher();
    let Some(items) = state.transcript.get(start..end) else {
        // The transcript is shorter than it was.
        return 0;
    };
    let mut last = None;
    for item in items {
        item.id().hash(&mut hasher);
        std::mem::discriminant(item).hash(&mut hasher);
        if let TranscriptItem::Assistant {
            id,
            version,
            children,
            child_times,
            ..
        } = item
        {
            last = Some(*id);
            version.hash(&mut hasher);
            child_times.hash(&mut hasher);
            children.len().hash(&mut hasher);
            for child in children {
                std::mem::discriminant(child).hash(&mut hasher);
                child.version().hash(&mut hasher);
                match child {
                    AssistantChild::Tool { call_id } => call_id.hash(&mut hasher),
                    AssistantChild::CommittedTool {
                        turn_seq,
                        content_index,
                        ..
                    } => (turn_seq, content_index).hash(&mut hasher),
                    _ => {}
                }
            }
            state.assistant_runs.get(id).hash(&mut hasher);
            state
                .assistant_ends
                .get(id)
                .map(|end| (end.at, end.stopped))
                .hash(&mut hasher);
            state.item_time(*id).hash(&mut hasher);
        } else {
            background_row(item).hash(&mut hasher);
        }
    }
    last.is_some_and(|last| state.assistant_live(last))
        .hash(&mut hasher);
    match state.transcript.get(end) {
        None => 0u8.hash(&mut hasher),
        Some(TranscriptItem::Assistant { id, .. }) => {
            1u8.hash(&mut hasher);
            state.assistant_runs.get(id).hash(&mut hasher);
        }
        Some(item) => {
            2u8.hash(&mut hasher);
            background_row(item).hash(&mut hasher);
        }
    }
    hasher.finish().max(1)
}

/// A session's reduced-mode [`FoldPlan`], worked out once per
/// [`SessionState::version`] however many frames and passes ask for it
/// ([`SessionState::fold_plan`]).
#[derive(Default)]
pub struct FoldCache(Mutex<Option<(u64, Arc<FoldPlan>)>>);

impl Clone for FoldCache {
    fn clone(&self) -> Self {
        Self(Mutex::new(self.lock().clone()))
    }
}

impl std::fmt::Debug for FoldCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FoldCache")
    }
}

impl FoldCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(u64, Arc<FoldPlan>)>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SessionState {
    /// The reduced mode's folds of this projection: [`FoldPlan::of`],
    /// built again only once [`Self::version`] moved.
    #[must_use]
    pub fn fold_plan(&self) -> Arc<FoldPlan> {
        let mut cached = self.folds.lock();
        if let Some((version, plan)) = cached.as_ref()
            && *version == self.version
        {
            return Arc::clone(plan);
        }
        let plan = Arc::new(match cached.take() {
            Some((_, plan)) => {
                // The frames that drew it are done with it: refreshed in
                // place, else copied.
                let mut plan = Arc::try_unwrap(plan).unwrap_or_else(|plan| (*plan).clone());
                plan.refresh(self);
                plan
            }
            None => FoldPlan::of(self),
        });
        *cached = Some((self.version, Arc::clone(&plan)));
        plan
    }
}

/// One fold: its work's children in order, and where its work ends.
#[derive(Clone, Debug, PartialEq)]
struct Fold {
    children: Vec<(u64, usize)>,
    end: FoldEnd,
}

#[derive(Clone, Copy, Debug, PartialEq)]
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
        plan.build_from(state, 0);
        plan
    }

    /// Brings the plan of an earlier version of `state` up to date: the
    /// stretches that read the same as when they were planned stay, and the
    /// transcript is planned again from the first that does not. A run's
    /// earlier stretches never change, so a delta plans only the last.
    fn refresh(&mut self, state: &SessionState) {
        let stale = self
            .stretches
            .iter()
            .position(|stretch| stretch.signature != signature(state, stretch.start, stretch.end));
        if let Some(index) = stale {
            for stretch in self.stretches.drain(index..) {
                for id in &stretch.blocks {
                    self.positions.remove(id);
                    self.items.remove(id);
                }
                for id in &stretch.rows {
                    self.items.remove(id);
                }
                for key in &stretch.folds {
                    self.folds.remove(key);
                    self.settled.remove(key);
                }
            }
        }
        // Plan again from where the last stretch that stays ended, not from
        // where the stale one began: a row removed before it moved it back.
        let from = self.stretches.last().map_or(0, |stretch| stretch.end);
        self.live.retain(|key| self.folds.contains_key(key));
        self.build_from(state, from);
    }

    /// Plans the transcript from `from`, where no stretch is open.
    fn build_from(&mut self, state: &SessionState, from: usize) {
        let mut stretch: Option<Building<'_>> = None;
        for (position, item) in state.transcript.iter().enumerate().skip(from) {
            if let TranscriptItem::Assistant { id, children, .. } = item {
                self.positions.insert(*id, position);
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
                    self.add(state, done, position);
                }
                stretch = Some(Building {
                    start: position,
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
                self.add(state, done, position);
            }
        }
        if let Some(done) = stretch {
            self.add(state, done, state.transcript.len());
        }
        self.live.sort_unstable();
    }

    /// Plans the stretch `stretch`, which the item at `end` ended (or the
    /// transcript's end).
    fn add(&mut self, state: &SessionState, stretch: Building<'_>, end: usize) {
        let mut planned = Stretch {
            start: stretch.start,
            end,
            signature: signature(state, stretch.start, end),
            blocks: stretch.blocks.iter().map(|(id, _)| *id).collect(),
            rows: Vec::new(),
            folds: Vec::new(),
        };
        self.plan(state, &stretch, &mut planned);
        // A row whose work ended says the same at any time: worked out once
        // here, not at every frame.
        for key in &planned.folds {
            match self.row(state, *key, jiff::Timestamp::UNIX_EPOCH, 0) {
                Some(row) if !row.summary.live => {
                    self.settled.insert(*key, row);
                }
                Some(_) => self.live.push(*key),
                None => {}
            }
        }
        let Self { items, settled, .. } = self;
        for id in planned.blocks.iter().chain(&planned.rows) {
            if let Some((fold, hash)) = items.get_mut(id) {
                *hash = fold_hash(fold, settled);
            }
        }
        self.stretches.push(planned);
    }

    fn plan(&mut self, state: &SessionState, stretch: &Building<'_>, planned: &mut Stretch) {
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
        let mut places: Map<(u64, usize), ChildFold> = Map::default();
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
            if let Some(key) = self.fold(&units[start..at], end, &mut places, &mut rows) {
                planned.folds.push(key);
                folded = true;
            }
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
                (
                    ItemFold::Block(BlockFold {
                        first: index == 0,
                        last: index + 1 == count,
                        children,
                    }),
                    0,
                ),
            );
        }
        for (row, key) in rows {
            self.items.insert(row, (ItemFold::Row { key }, 0));
            planned.rows.push(row);
        }
    }

    /// Folds `units`, one stretch of work ending as `end`, when it holds a
    /// thinking part or a tool call; its key when it did.
    fn fold(
        &mut self,
        units: &[Unit<'_>],
        end: FoldEnd,
        places: &mut Map<(u64, usize), ChildFold>,
        rows: &mut Vec<(u64, WorkKey)>,
    ) -> Option<WorkKey> {
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
            return None;
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
        Some(key)
    }

    /// Every fold's key.
    pub fn keys(&self) -> impl Iterator<Item = WorkKey> + '_ {
        self.folds.keys().copied()
    }

    /// Where the item `item_id` stands, when it is in a folded run.
    #[must_use]
    pub fn get(&self, item_id: u64) -> Option<&ItemFold> {
        self.items.get(&item_id).map(|(fold, _)| fold)
    }

    /// Where the item `item_id` stands, with a hash of that place and of
    /// the settled work rows it draws, which changes when either does. What
    /// a client adds (which folds are open, the live rows) it hashes itself.
    #[must_use]
    pub fn placed(&self, item_id: u64) -> Option<(&ItemFold, u64)> {
        self.items.get(&item_id).map(|(fold, hash)| (fold, *hash))
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
        match self.settled.get(&key) {
            Some(row) => Some(row.clone()),
            None => self.row(state, key, now, room),
        }
    }

    /// The work row of the fold `key` once its work ended; `None` while it
    /// works, when [`Self::work_row`] counts it to a time.
    #[must_use]
    pub fn settled_row(&self, key: WorkKey) -> Option<&WorkRow> {
        self.settled.get(&key)
    }

    /// The folds still working, in key order: those whose rows tick.
    pub fn live_keys(&self) -> impl Iterator<Item = WorkKey> + '_ {
        self.live.iter().copied()
    }

    fn row(
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
    /// The position of its first block.
    start: usize,
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

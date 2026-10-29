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
    fold_summary(state, item_id, item_id, work.iter(), now)
}

/// The work row of a fold from the block `first` to the block `last`, whose
/// work is `work`: from `first`'s start to `last`'s end, live while `last`
/// takes output.
fn fold_summary<'a>(
    state: &SessionState,
    first: u64,
    item_id: u64,
    work: impl IntoIterator<Item = &'a AssistantChild>,
    now: jiff::Timestamp,
) -> WorkSummary {
    let end = state.assistant_ends.get(&item_id).copied();
    let live = end.is_none() && state.assistant_live(item_id);
    let started = state.item_time(first);
    let ended = match end {
        Some(end) => Some(end.at),
        None if live => Some(now),
        // A block that never sealed and is not open (a run with no end in
        // the log): its last part's arrival.
        None => state.assistant_last_child_time(item_id),
    };
    let elapsed = started
        .zip(ended)
        .and_then(|(started, ended)| Duration::try_from(ended.duration_since(started)).ok())
        .unwrap_or_default();
    let mut tool_calls = 0;
    let mut failed = 0;
    for child in work {
        match child {
            AssistantChild::Tool { call_id } => {
                tool_calls += 1;
                if state
                    .tools
                    .get(call_id)
                    .is_some_and(|tool| tool.status == ToolStatus::Failed)
                {
                    failed += 1;
                }
            }
            AssistantChild::CommittedTool { .. } => tool_calls += 1,
            _ => {}
        }
    }
    WorkSummary {
        live,
        stopped: end.is_some_and(|end| end.stopped),
        elapsed,
        tool_calls,
        failed,
    }
}

/// The line under a live work row: what its newest part is doing. A
/// thinking part's newest step or line, a tool call's header (`Waiting for
/// approval: …` while it waits on the user), or the newest line of text
/// between calls; `Starting…` before any says anything.
#[must_use]
pub fn working_subtext(state: &SessionState, work: &[AssistantChild]) -> String {
    subtext_of(state, work.iter())
}

/// [`working_subtext`] of work held across blocks.
fn subtext_of<'a>(
    state: &SessionState,
    work: impl DoubleEndedIterator<Item = &'a AssistantChild>,
) -> String {
    work.rev()
        .find_map(|child| match child {
            AssistantChild::Thinking { text, .. } => {
                Some(thinking_step(text).unwrap_or_else(|| "Thinking…".to_owned()))
            }
            AssistantChild::Tool { call_id } => {
                let tool = state.tools.get(call_id)?;
                let header = tool.header_text();
                Some(if needs_user(state, call_id) {
                    format!("Waiting for approval: {header}")
                } else {
                    header
                })
            }
            AssistantChild::CommittedTool { name, .. } => Some(name.as_str().to_owned()),
            AssistantChild::Text { markdown, .. } => newest_line(markdown.as_str()),
            AssistantChild::Attribution { .. }
            | AssistantChild::MediaFile { .. }
            | AssistantChild::Notice { .. } => None,
        })
        .unwrap_or_else(|| "Starting…".to_owned())
}

/// Where an item stands in the reduced mode's folds.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ItemFold {
    /// An assistant block of the fold whose first block is `lead`: the
    /// lead's work row stands for the whole fold. The block's own reply
    /// starts at `reply_start`, its children's length unless it is the
    /// fold's `last` block.
    Block {
        lead: u64,
        reply_start: usize,
        last: bool,
    },
    /// A row between two blocks of the fold `lead`, part of its work.
    Row { lead: u64 },
}

/// A work row: what it says, and while its fold works, what it does now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkRow {
    pub summary: WorkSummary,
    pub subtext: Option<String>,
}

/// The reduced mode's folds of a transcript. A fold is one run's work from
/// its first block to its last: the blocks of one run that only background
/// rows came between (an ACP agent's background command ending, a
/// background call's report, a diagnostic below the warning level), and
/// those rows. Any other row ends a fold, and a fold with no thinking or
/// tool call is not one.
#[derive(Clone, Debug, Default)]
pub struct FoldPlan {
    items: HashMap<u64, ItemFold>,
    /// Each fold's blocks, in order, as their IDs and their positions in
    /// the transcript, by its lead.
    blocks: HashMap<u64, Vec<(u64, usize)>>,
}

impl FoldPlan {
    /// The folds of `state`'s transcript.
    #[must_use]
    pub fn of(state: &SessionState) -> Self {
        let mut plan = Self::default();
        let mut fold: Option<Building<'_>> = None;
        for (position, item) in state.transcript.iter().enumerate() {
            if let TranscriptItem::Assistant { id, children, .. } = item {
                let run = state.assistant_runs.get(id).copied();
                if let Some(building) = fold
                    .as_mut()
                    .filter(|building| run.is_some() && building.run == run)
                {
                    building.rows.append(&mut building.pending);
                    building.blocks.push((*id, position, children));
                    continue;
                }
                if let Some(done) = fold.take() {
                    plan.add(done);
                }
                fold = Some(Building {
                    run,
                    blocks: vec![(*id, position, children)],
                    rows: Vec::new(),
                    pending: Vec::new(),
                });
            } else if background_row(item) {
                if let Some(building) = fold.as_mut() {
                    building.pending.push(item.id());
                }
            } else if let Some(done) = fold.take() {
                plan.add(done);
            }
        }
        if let Some(done) = fold {
            plan.add(done);
        }
        plan
    }

    fn add(&mut self, fold: Building<'_>) {
        let worked = fold
            .blocks
            .iter()
            .any(|(_, _, children)| reply_start(children).is_some());
        if !worked {
            return;
        }
        let lead = fold.blocks[0].0;
        let count = fold.blocks.len();
        for (index, (id, _, children)) in fold.blocks.iter().enumerate() {
            let last = index + 1 == count;
            let reply_start = if last {
                reply_start(children).unwrap_or(0)
            } else {
                children.len()
            };
            self.items.insert(
                *id,
                ItemFold::Block {
                    lead,
                    reply_start,
                    last,
                },
            );
        }
        for row in fold.rows {
            self.items.insert(row, ItemFold::Row { lead });
        }
        self.blocks.insert(
            lead,
            fold.blocks
                .iter()
                .map(|(id, position, _)| (*id, *position))
                .collect(),
        );
    }

    /// Where the item `item_id` stands, when it is in a fold.
    #[must_use]
    pub fn get(&self, item_id: u64) -> Option<ItemFold> {
        self.items.get(&item_id).copied()
    }

    /// The work row of the fold led by `lead`, at `now`.
    #[must_use]
    pub fn work_row(
        &self,
        state: &SessionState,
        lead: u64,
        now: jiff::Timestamp,
    ) -> Option<WorkRow> {
        let blocks = self.blocks.get(&lead)?;
        let (last, _) = *blocks.last()?;
        let mut work = Vec::new();
        for (id, position) in blocks {
            if let Some(TranscriptItem::Assistant { children, .. }) =
                state.transcript.get(*position)
                && let Some(ItemFold::Block { reply_start, .. }) = self.get(*id)
            {
                work.extend(&children[..reply_start.min(children.len())]);
            }
        }
        let summary = fold_summary(state, lead, last, work.iter().copied(), now);
        let subtext = summary
            .live
            .then(|| subtext_of(state, work.iter().copied()));
        Some(WorkRow { summary, subtext })
    }
}

/// A fold being gathered: its run, its blocks so far, the rows between
/// them, and the rows since its last block, which join it only if another
/// of its blocks follows.
struct Building<'a> {
    run: Option<cookie_agent_protocol::RunId>,
    blocks: Vec<(u64, usize, &'a [AssistantChild])>,
    rows: Vec<u64>,
    pending: Vec<u64>,
}

/// Whether `item` is a background row a fold takes in when its run goes on
/// past it: an ACP agent's background command that ended, a background
/// call's report, a diagnostic below the warning level, or a producer
/// message not shown yet.
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
    let step = text.lines().rev().find_map(|line| {
        let line = line.trim();
        let heading = line.trim_start_matches('#');
        if heading.len() < line.len() && heading.starts_with(' ') {
            return Some(heading.trim());
        }
        line.strip_prefix("**")
            .and_then(|rest| rest.strip_suffix("**"))
            .filter(|inner| !inner.is_empty() && !inner.contains("**"))
            .map(str::trim)
    });
    step.filter(|step| !step.is_empty())
        .map(crate::tool_header::flatten_header_argument)
        .or_else(|| newest_line(text))
}

/// The newest line of `text` that has any, whitespace collapsed.
fn newest_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(crate::tool_header::flatten_header_argument)
        .find(|line| !line.is_empty())
}

#[cfg(test)]
#[path = "transcript_mode_tests.rs"]
mod tests;

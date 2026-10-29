//! How much of an agent's work the transcript shows in every client
//! (`docs/site/specs/reduced-transcript.md`): every thinking block and tool
//! call, or each assistant block's work folded into one `Working…` /
//! `Worked for` row above its reply.

use std::time::Duration;

use cookie_agent_protocol::ToolCallId;

use crate::{AssistantChild, SessionState, ToolStatus, reply::format_elapsed};

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
    let end = state.assistant_ends.get(&item_id).copied();
    let live = end.is_none() && state.assistant_live(item_id);
    let started = state.item_time(item_id);
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
    work.iter()
        .rev()
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

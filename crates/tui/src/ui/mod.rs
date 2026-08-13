//! Ratatui layout composition and UI module wiring.

mod app;
mod events;
mod input;
mod pickers;
mod provider;
mod slash;
mod transcript;

pub use app::{App, run_with_client, run_with_new_session};

use ratatui::layout::Rect;

const MAX_AGENT_HEIGHT: u16 = 10;
/// Composer ceiling in total rows: five text rows plus top/bottom borders.
const MAX_INPUT_HEIGHT: u16 = 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UiLayout {
    pub(crate) agent: Rect,
    pub(crate) conversation: Rect,
    /// The steer-queue strip between the conversation pane and the status
    /// line; zero height while the selected session's queue is empty.
    pub(crate) queue: Rect,
    pub(crate) input: Rect,
    pub(crate) status: Rect,
    pub(crate) bar: Rect,
}

/// Resolve the single top-to-bottom pane geometry for a known visible tree
/// row count, queue-strip row demand, and composer text-row demand. The
/// Agents panel is exactly `clamp(visible_tree_row_count, 1, 8)` text rows
/// with its borders outside that count, so the conversation starts
/// immediately below it. The composer takes one text row by default and
/// grows with its content up to five; every added composer row is reclaimed
/// from the conversation pane. The queue strip reclaims conversation rows
/// the same way, keeping the status line and composer pinned and always
/// leaving the conversation at least one row.
pub(crate) fn terminal_layout_with_tree_rows(
    area: Rect,
    visible_tree_rows: usize,
    queue_rows: u16,
    input_text_rows: u16,
) -> UiLayout {
    let agent_text_rows = visible_tree_rows.clamp(1, 8) as u16;
    let agent_height = agent_text_rows.saturating_add(2).min(MAX_AGENT_HEIGHT);
    let bar_height = u16::from(area.height > 0);
    let input_height = input_text_rows
        .clamp(1, input::MAX_TEXT_ROWS)
        .saturating_add(2)
        .min(MAX_INPUT_HEIGHT)
        .min(area.height.saturating_sub(bar_height));
    let available_height = area
        .height
        .saturating_sub(input_height)
        .saturating_sub(bar_height);
    let status_height = u16::from(available_height >= 3);
    let content_height = available_height.saturating_sub(status_height);
    let agent_height = if content_height > 1 {
        agent_height.min(content_height - 1)
    } else {
        content_height
    };
    let conversation_available = content_height.saturating_sub(agent_height);
    // The strip never starves the conversation completely: on a cramped
    // terminal it shrinks away rather than taking the last row.
    let queue_height = queue_rows.min(conversation_available.saturating_sub(1));
    let conversation_height = conversation_available.saturating_sub(queue_height);
    let agent = Rect::new(area.x, area.y, area.width, agent_height);
    let conversation = Rect::new(
        area.x,
        area.y.saturating_add(agent_height),
        area.width,
        conversation_height,
    );
    let queue = Rect::new(
        area.x,
        conversation.y.saturating_add(conversation_height),
        area.width,
        queue_height,
    );
    let status = Rect::new(
        area.x,
        queue.y.saturating_add(queue_height),
        area.width,
        status_height,
    );
    let input = Rect::new(
        area.x,
        area.y
            .saturating_add(area.height.saturating_sub(bar_height + input_height)),
        area.width,
        input_height,
    );
    let bar = Rect::new(
        area.x,
        area.y
            .saturating_add(area.height.saturating_sub(bar_height)),
        area.width,
        bar_height,
    );
    UiLayout {
        agent,
        conversation,
        queue,
        status,
        input,
        bar,
    }
}

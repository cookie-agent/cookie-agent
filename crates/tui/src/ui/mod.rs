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

const MAX_AGENT_HEIGHT: u16 = 5;
const INPUT_HEIGHT: u16 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UiLayout {
    pub(crate) agent: Rect,
    pub(crate) conversation: Rect,
    pub(crate) input: Rect,
    pub(crate) status: Rect,
    pub(crate) bar: Rect,
}

/// Resolve the single top-to-bottom pane geometry for a known visible tree
/// row count. The Agents panel is exactly
/// `clamp(visible_tree_row_count, 1, 3)` text rows with its borders outside
/// that count, so the conversation starts immediately below it.
pub(crate) fn terminal_layout_with_tree_rows(area: Rect, visible_tree_rows: usize) -> UiLayout {
    let agent_text_rows = visible_tree_rows.clamp(1, 3) as u16;
    let agent_height = agent_text_rows.saturating_add(2).min(MAX_AGENT_HEIGHT);
    let bar_height = u16::from(area.height > 0);
    let input_height = INPUT_HEIGHT.min(area.height.saturating_sub(bar_height));
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
    let conversation_height = content_height.saturating_sub(agent_height);
    let agent = Rect::new(area.x, area.y, area.width, agent_height);
    let conversation = Rect::new(
        area.x,
        area.y.saturating_add(agent_height),
        area.width,
        conversation_height,
    );
    let status = Rect::new(
        area.x,
        conversation.y.saturating_add(conversation_height),
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
        status,
        input,
        bar,
    }
}

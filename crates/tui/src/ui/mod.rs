//! Ratatui layout composition and UI module wiring.

mod app;
mod events;
mod input;
mod management;
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
    pub(crate) status: Rect,
    /// The persistent goal bar directly above the composer; zero height when
    /// it is not visible or the terminal has no space for it.
    pub(crate) goal: Rect,
    pub(crate) input: Rect,
    pub(crate) bar: Rect,
}

/// Resolve the single top-to-bottom pane geometry for a known visible tree
/// row count, queue-strip row demand, goal visibility, and composer text-row
/// demand. The Agents panel is hidden when `visible_tree_row_count` is zero or
/// one; otherwise it is exactly `clamp(visible_tree_row_count, 1, 8)` text rows
/// with its borders outside that count, so the conversation starts immediately
/// below it. The composer takes one text row by default and grows with its
/// content up to five; every added composer row is reclaimed from the
/// conversation pane. The goal bar takes exactly one row when it is visible
/// and space remains above the composer, with the ephemeral status line
/// yielding first on short terminals. The queue strip reclaims conversation
/// rows the same way and leaves the conversation at least one row whenever
/// content space remains after fixed bars.
pub(crate) fn terminal_layout_with_tree_rows(
    area: Rect,
    visible_tree_rows: usize,
    queue_rows: u16,
    goal_visible: bool,
    input_text_rows: u16,
) -> UiLayout {
    let agent_height = if visible_tree_rows <= 1 {
        0
    } else {
        (visible_tree_rows.clamp(1, 8) as u16)
            .saturating_add(2)
            .min(MAX_AGENT_HEIGHT)
    };
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
    let goal_height = u16::from(goal_visible && available_height > 0);
    let status_height = u16::from(available_height.saturating_sub(goal_height) >= 3);
    let content_height = available_height
        .saturating_sub(goal_height)
        .saturating_sub(status_height);
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
    let goal = Rect::new(
        area.x,
        status.y.saturating_add(status_height),
        area.width,
        goal_height,
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
        goal,
        input,
        bar,
    }
}

#[cfg(test)]
mod tests {
    use super::terminal_layout_with_tree_rows;
    use ratatui::layout::Rect;

    #[test]
    fn hidden_goal_preserves_existing_geometry_without_a_blank_row() {
        for width in [1, 3, 7, 8, 20, 40, 80] {
            for height in 0..=24 {
                let area = Rect::new(2, 3, width, height);
                let layout = terminal_layout_with_tree_rows(area, 3, 5, false, 2);

                assert_eq!(layout.goal.height, 0, "{width}x{height}");
                assert_eq!(layout.goal.y, layout.input.y, "{width}x{height}");
                assert_eq!(
                    layout.status.y + layout.status.height,
                    layout.input.y,
                    "{width}x{height}"
                );
            }
        }
    }

    #[test]
    fn visible_goal_is_one_row_when_space_exists_and_never_overlaps() {
        for width in [1, 3, 7, 8, 20, 40, 80] {
            for height in 0..=24 {
                let area = Rect::new(2, 3, width, height);
                let hidden = terminal_layout_with_tree_rows(area, 3, 5, false, 1);
                let shown = terminal_layout_with_tree_rows(area, 3, 5, true, 1);
                let expected_goal_height = u16::from(
                    area.height
                        .saturating_sub(hidden.input.height)
                        .saturating_sub(hidden.bar.height)
                        > 0,
                );

                assert_eq!(shown.goal.height, expected_goal_height, "{width}x{height}");
                assert_eq!(shown.goal.y + shown.goal.height, shown.input.y);
                assert_eq!(shown.input, hidden.input, "{width}x{height}");
                assert_eq!(shown.bar, hidden.bar, "{width}x{height}");

                let panes = [
                    shown.agent,
                    shown.conversation,
                    shown.queue,
                    shown.status,
                    shown.goal,
                    shown.input,
                    shown.bar,
                ];
                for pane in panes {
                    assert_eq!(pane.x, area.x, "{width}x{height}: {pane:?}");
                    assert_eq!(pane.width, area.width, "{width}x{height}: {pane:?}");
                    assert!(
                        pane.y >= area.y && pane.y + pane.height <= area.y + area.height,
                        "{width}x{height}: {pane:?} outside {area:?}"
                    );
                }
                for pair in panes.windows(2) {
                    assert!(
                        pair[0].y + pair[0].height <= pair[1].y,
                        "{width}x{height}: {:?} overlaps {:?}",
                        pair[0],
                        pair[1]
                    );
                }
            }
        }
    }

    #[test]
    fn visible_goal_is_reserved_before_ephemeral_status() {
        let area = Rect::new(0, 0, 20, 7);
        let hidden = terminal_layout_with_tree_rows(area, 0, 0, false, 1);
        let shown = terminal_layout_with_tree_rows(area, 0, 0, true, 1);

        assert_eq!(hidden.status.height, 1);
        assert_eq!(hidden.goal.height, 0);
        assert_eq!(shown.status.height, 0);
        assert_eq!(shown.goal.height, 1);
        assert_eq!(shown.goal.y + shown.goal.height, shown.input.y);
        assert_eq!(shown.input, hidden.input);
        assert_eq!(shown.bar, hidden.bar);
    }
}

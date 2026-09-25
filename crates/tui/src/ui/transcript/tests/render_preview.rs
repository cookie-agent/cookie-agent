//! Visual preview for transcript design work: replays a real `events.jsonl`,
//! expands every tool and thinking block, renders the app, and dumps each
//! cell's symbol, colours and modifiers as one JSON array per row.
//! `scripts/tui_preview_png.py` turns the dump into a PNG.
//!
//! ```sh
//! PREVIEW_EVENTS=path/to/events.jsonl PREVIEW_OUT=/tmp/preview.jsonl \
//!     cargo test -p cookie_agent_tui render_preview -- --ignored
//! ```
//!
//! Optional: `PREVIEW_WIDTH` (110), `PREVIEW_HEIGHT` (120), `PREVIEW_OFFSET`
//! (scroll row; the tail is followed when unset), `PREVIEW_EVENT_LIMIT`
//! (replay only the first N events), and `PREVIEW_DARK` (dark theme).

use std::collections::HashSet;

use cookie_agent_protocol::StoredEvent;
use ratatui::{Terminal, backend::TestBackend};

use crate::theme::{ColorLevel, Theme, ThemeKind};
use crate::ui::transcript::BlockId;

use super::support::*;

#[tokio::test]
#[ignore = "manual visual preview"]
async fn render_preview() {
    let events_path = std::env::var("PREVIEW_EVENTS").expect("PREVIEW_EVENTS");
    let out = std::env::var("PREVIEW_OUT").expect("PREVIEW_OUT");
    let width: u16 = env_or("PREVIEW_WIDTH", 110);
    let height: u16 = env_or("PREVIEW_HEIGHT", 120);
    let offset: Option<usize> = std::env::var("PREVIEW_OFFSET")
        .ok()
        .map(|v| v.parse().unwrap());
    let dark = std::env::var("PREVIEW_DARK").is_ok();
    let limit: usize = env_or("PREVIEW_EVENT_LIMIT", usize::MAX);

    let events = std::fs::read_to_string(events_path)
        .unwrap()
        .lines()
        .take(limit)
        .map(|line| serde_json::from_str::<StoredEvent>(line).unwrap())
        .collect::<Vec<_>>();
    let session = events[0].session_id;
    let mut app = test_app().await;
    app.theme = Theme::new(
        if dark {
            ThemeKind::Dark
        } else {
            ThemeKind::Default
        },
        ColorLevel::TrueColor,
    );
    assert!(app.store.rebuild_session(session, 1, events));
    app.selected = Some(session);
    app.tree_root = Some(session);
    let state = &app.store.sessions[&session];
    let mut expanded = state
        .tools
        .keys()
        .map(|id| BlockId::Tool(*id))
        .collect::<HashSet<_>>();
    for item in &state.transcript {
        if let crate::state::TranscriptItem::Assistant { children, .. } = item {
            for child in children {
                if let crate::state::AssistantChild::Thinking { id, .. } = child {
                    expanded.insert(BlockId::Thinking(*id));
                }
            }
        }
    }
    app.expanded_blocks.insert(session, expanded);

    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw_for_test(frame)).unwrap();
    if let Some(offset) = offset {
        app.conversation_scroll.following = false;
        app.conversation_scroll.offset = offset;
    }
    terminal.draw(|frame| app.draw_for_test(frame)).unwrap();

    let buffer = terminal.backend().buffer();
    let mut dump = String::new();
    for y in 0..buffer.area.height {
        let row = (0..buffer.area.width)
            .map(|x| {
                let cell = &buffer[(x, y)];
                serde_json::json!([
                    cell.symbol(),
                    format!("{:?}", cell.fg),
                    format!("{:?}", cell.bg),
                    format!("{:?}", cell.modifier),
                ])
            })
            .collect::<Vec<_>>();
        dump.push_str(&serde_json::to_string(&row).unwrap());
        dump.push('\n');
    }
    std::fs::write(out, dump).unwrap();
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

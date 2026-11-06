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
//! (replay only the first N events), `PREVIEW_DARK` (dark theme),
//! `PREVIEW_OPEN_WORK` (open the reduced mode's work rows too), and
//! `PREVIEW_TAIL_BYTES` (open the session newest first, as the display view
//! sends a window of that many bytes).

use std::{collections::HashSet, path::Path};

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

    // Whole, as the daemon reads them: history format 2 keeps large values
    // in the tree's blobs.
    let events = cookie_agent_engine::events::load_whole_records(Path::new(&events_path))
        .unwrap()
        .into_iter()
        .take(limit)
        .collect::<Vec<StoredEvent>>();
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
    match std::env::var("PREVIEW_TAIL_BYTES") {
        Ok(budget) => open_newest_first(&mut app, events, budget.parse().unwrap()),
        Err(_) => assert!(app.store.rebuild_session(session, 1, events)),
    }
    app.selected = Some(session);
    app.tree_root = Some(session);
    let state = &app.store.sessions[&session];
    // Every call open, unless `PREVIEW_FOLDED` asks to see their headers.
    let mut expanded = state
        .tools
        .keys()
        .filter(|_| std::env::var("PREVIEW_FOLDED").is_err())
        .map(|id| BlockId::Tool(*id))
        .collect::<HashSet<_>>();
    let open_work = std::env::var("PREVIEW_OPEN_WORK").is_ok();
    for item in &state.transcript {
        if let crate::state::TranscriptItem::Assistant { id, children, .. } = item {
            for (index, child) in children.iter().enumerate() {
                if let crate::state::AssistantChild::Thinking { id, .. } = child {
                    expanded.insert(BlockId::Thinking(*id));
                }
                // Wherever a fold may start: only the real ones are read.
                if open_work {
                    expanded.insert(BlockId::Work {
                        item: *id,
                        child: u32::try_from(index).unwrap(),
                    });
                }
            }
        }
    }
    app.expanded_blocks.insert(session, expanded);
    if open_work {
        app.tui_config.transcript =
            cookie_agent_client_state::transcript_mode::TranscriptMode::Reduced;
    }

    // `PREVIEW_MACHINES=local=yunhao-labserver,unicorn=unicorn` labels calls as
    // that listing would.
    if let Ok(machines) = std::env::var("PREVIEW_MACHINES") {
        let listing = cookie_agent_protocol::RuntimesListResult {
            runtimes: machines
                .split(',')
                .map(|pair| {
                    let (runtime, machine) = pair.split_once('=').expect("runtime=machine");
                    cookie_agent_protocol::RuntimeListing {
                        runtime_id: cookie_agent_protocol::RuntimeId::new(runtime).unwrap(),
                        lifetime: cookie_agent_protocol::RuntimeLifetime::Spawned,
                        connected: true,
                        capabilities: Vec::new(),
                        platform: None,
                        machine: Some(cookie_agent_protocol::MachineName::new(machine).unwrap()),
                        home: None,
                        message: None,
                        active_runs: 0,
                        background_calls: 0,
                    }
                })
                .collect(),
            instances: Vec::new(),
        };
        app.runtime_machines =
            Some(cookie_agent_client_state::call_machines::RuntimeMachines::from_listing(&listing));
    }

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

/// Opens `events`' session as a newest-first replay in the display view
/// delivers a window of `budget` bytes.
fn open_newest_first(app: &mut crate::ui::app::App, events: Vec<StoredEvent>, budget: usize) {
    use cookie_agent_protocol_transport::{ClientDelivery, ReplayHistory, ReplayWindow};
    let events = events
        .into_iter()
        .map(std::sync::Arc::new)
        .collect::<Vec<_>>();
    let session_id = events[0].session_id;
    let window = cookie_agent_protocol::display_view::tail_window(
        &events,
        cookie_agent_protocol::EventsView::Display,
        budget,
    );
    let final_seq = window.events.last().unwrap().seq;
    let mut deliveries = vec![ClientDelivery::ReplayStart {
        session_id,
        generation: 1,
        final_seq,
        rebuild: true,
        history: Some(Box::new(ReplayHistory {
            window: Some(ReplayWindow {
                header: window
                    .header
                    .iter()
                    .map(|event| (**event).clone())
                    .collect(),
                older_cursor: window.older_cursor,
                gap: window.gap,
            }),
            cut: window.cut.clone(),
        })),
    }];
    deliveries.extend(
        window
            .events
            .iter()
            .map(|event| ClientDelivery::ReplayEvent {
                session_id,
                generation: 1,
                final_seq,
                event: Box::new((**event).clone()),
            }),
    );
    deliveries.push(ClientDelivery::ReplayEnd {
        session_id,
        generation: 1,
        final_seq,
    });
    for delivery in deliveries {
        let _ = app.store.apply_delivery(delivery);
    }
}

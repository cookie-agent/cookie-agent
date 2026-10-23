//! Every clickable element hovers: the pointer resolver that clicks act on
//! is the one hover reads, and each hoverable target visibly restyles its
//! own cells.

use std::collections::BTreeSet;

use cookie_agent_protocol::{
    BackgroundActor, BackgroundCall, GoalStatus, SessionId, SessionTree, ToolCallId,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};

use crate::state::ToolStatus;
use crate::theme::{ColorLevel, Theme, ThemeKind};
use crate::ui::app::memory_panel::sample_memory;
use crate::ui::app::*;
use crate::ui::management::{RuntimeView, sample_machines, sample_permissions};
use crate::ui::transcript::BlockId;

use super::support::*;

const WIDTH: u16 = 110;
const HEIGHT: u16 = 56;

fn draw(app: &mut App) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("render");
    terminal.backend().buffer().clone()
}

/// Every variant, so a new target cannot join the enum without this sweep
/// naming it.
fn kind(target: PointerTarget) -> &'static str {
    match target {
        PointerTarget::PaletteRow(_) => "palette row",
        PointerTarget::GoalClose => "goal close",
        PointerTarget::PermissionPanelMode => "permission panel mode",
        PointerTarget::PermissionEffect(_) => "permission effect",
        PointerTarget::PermissionRow(_) => "permission row",
        PointerTarget::BackgroundRow(_) => "background row",
        PointerTarget::BackgroundStop(_) => "background stop",
        PointerTarget::RuntimeTab(_) => "runtime tab",
        PointerTarget::MachineRow(_) => "machine row",
        PointerTarget::MachineAction(_) => "machine action",
        PointerTarget::MemoryRow(_) => "memory row",
        PointerTarget::MemoryAction(_) => "memory action",
        PointerTarget::ConfirmButton(_) => "confirm button",
        PointerTarget::PickerInput => "picker input",
        PointerTarget::PickerRow(_) => "picker row",
        PointerTarget::SessionPickerAction(_) => "session picker action",
        PointerTarget::ProviderSubmit => "provider submit",
        PointerTarget::ProviderCancel => "provider cancel",
        PointerTarget::ProviderField(_) => "provider field",
        PointerTarget::ApprovalAction(_) => "approval action",
        PointerTarget::ApprovalDetails => "approval details",
        PointerTarget::GoalAction(_) => "goal action",
        PointerTarget::BackgroundBar => "background bar",
        PointerTarget::PermissionMode => "permission mode",
        PointerTarget::SessionCost => "session cost",
        PointerTarget::ProjectLabel => "project label",
        PointerTarget::EventLevelFilter => "event level filter",
        PointerTarget::TitleSegment(_) => "title segment",
        PointerTarget::ComposerText => "composer text",
        PointerTarget::QueueEntry(_) => "queue entry",
        PointerTarget::PassiveQueueEntry(_) => "passive queue entry",
        PointerTarget::Scrollbar(ScrollbarTarget::Conversation) => "conversation scrollbar",
        PointerTarget::Scrollbar(ScrollbarTarget::Input) => "composer scrollbar",
        PointerTarget::UserMessage(_) => "user message",
        PointerTarget::BackgroundButton(_) => "background button",
        PointerTarget::TranscriptBlock(_) => "transcript block",
        PointerTarget::TreeExpand(_) => "tree expand",
        PointerTarget::TreeRow(_) => "tree row",
    }
}

/// Targets that live inside the conversation viewport.
fn is_transcript(target: PointerTarget) -> bool {
    matches!(
        target,
        PointerTarget::TranscriptBlock(_)
            | PointerTarget::BackgroundButton(_)
            | PointerTarget::UserMessage(_)
    )
}

/// Every theme kind at every colour level.
fn every_theme() -> impl Iterator<Item = Theme> {
    [
        ThemeKind::Default,
        ThemeKind::Dark,
        ThemeKind::Mono,
        ThemeKind::HighContrast,
    ]
    .into_iter()
    .flat_map(|kind| {
        [
            ColorLevel::None,
            ColorLevel::Ansi16,
            ColorLevel::Ansi256,
            ColorLevel::TrueColor,
        ]
        .into_iter()
        .map(move |level| Theme::new(kind, level))
    })
}

/// Sweep every cell of the frame. Wherever the pointer resolves a target,
/// hover resolves the same one unless it is a text field or a passive queue
/// row; and in every theme, hovering each hoverable target restyles at
/// least one of the cells that resolve to it, and a transcript target's
/// hover never leaves the conversation viewport. Returns the kinds of
/// target seen.
fn sweep(app: &mut App, surface: &str) -> BTreeSet<&'static str> {
    app.hover = None;
    draw(app);
    let mut targets: Vec<(PointerTarget, Vec<(u16, u16)>)> = Vec::new();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let Some(target) = app.pointer_target_at(x, y) else {
                assert_eq!(app.hover_target_at(x, y), None, "{surface} at {x},{y}");
                continue;
            };
            if target.is_hoverable() {
                assert_eq!(
                    app.hover_target_at(x, y),
                    Some(target),
                    "{surface} at {x},{y}"
                );
            } else {
                assert!(
                    matches!(
                        target,
                        PointerTarget::PickerInput
                            | PointerTarget::ComposerText
                            | PointerTarget::PassiveQueueEntry(_)
                    ),
                    "{surface}: only text fields and passive queue rows stay quiet, not {target:?}"
                );
                assert_eq!(app.hover_target_at(x, y), None, "{surface} at {x},{y}");
            }
            if is_transcript(target) {
                assert!(
                    app.hit_map
                        .conversation
                        .is_some_and(|viewport| viewport.contains((x, y).into())),
                    "{surface}: {target:?} resolves at {x},{y} outside the conversation"
                );
            }
            match targets.iter_mut().find(|(seen, _)| *seen == target) {
                Some((_, cells)) => cells.push((x, y)),
                None => targets.push((target, vec![(x, y)])),
            }
        }
    }
    let theme = app.theme.clone();
    for candidate in every_theme() {
        let key = candidate.key();
        app.theme = candidate;
        app.hover = None;
        let plain = draw(app);
        for (target, cells) in &targets {
            if !target.is_hoverable() {
                continue;
            }
            app.hover = Some(*target);
            let hovered = draw(app);
            let restyled = |x: u16, y: u16| hovered[(x, y)].style() != plain[(x, y)].style();
            assert!(
                cells.iter().any(|&(x, y)| restyled(x, y)),
                "{surface} in {key:?}: hovering {target:?} restyles none of its cells"
            );
            if is_transcript(*target) {
                let viewport = app.hit_map.conversation.expect("conversation");
                for position in hovered.area.positions() {
                    assert!(
                        !restyled(position.x, position.y) || viewport.contains(position),
                        "{surface} in {key:?}: hovering {target:?} restyles {position:?} outside the conversation"
                    );
                }
            }
        }
    }
    app.theme = theme;
    app.hover = None;
    targets.iter().map(|(target, _)| kind(*target)).collect()
}

fn background_call(job: &str) -> BackgroundCall {
    BackgroundCall {
        job: job.into(),
        tool: "bash".into(),
        tool_call_id: ToolCallId::new_v7(),
        display: Some("sleep 30".into()),
        started_at: jiff::Timestamp::now(),
        detached_at: jiff::Timestamp::now(),
        by: BackgroundActor::Model,
    }
}

/// One session showing every content surface at once: an overflowing
/// conversation with user messages, a collapsed and a background-capable
/// running call, a tree whose child has its own child, background calls,
/// a goal, queued inputs (one from another client), and an overflowing
/// composer draft.
async fn content_app() -> (App, SessionId, ToolCallId) {
    let (mut app, session, run) = app_with_active_run().await;
    app.theme = Theme::new(ThemeKind::Default, ColorLevel::TrueColor);
    let running = read_tool_state("src/lib.rs", ToolStatus::Running, "");
    let call_id = read_tool_id(&running);
    let done = read_tool_state("src/main.rs", ToolStatus::Completed, "output");
    {
        let state = app.store.sessions.get_mut(&session).expect("session");
        state
            .transcript
            .extend(tall_transcript_state(40).transcript);
        state.transcript.extend(done.transcript);
        state.tools.extend(done.tools);
        state.transcript.extend(running.transcript);
        state.tools.extend(running.tools);
        // A model's memory change, whose line opens the Memory panel.
        state
            .transcript
            .push(crate::state::TranscriptItem::MemoryChange {
                id: 9_000,
                seq: 9_000,
                change: cookie_agent_protocol::MemoryChangeKind::Saved,
                url: "project://web/build".into(),
            });
        state.tools.get_mut(&call_id).expect("call").backgroundable = true;
        state.background_calls = vec![background_call("job_1"), background_call("job_2")];
        state.estimated_cost_usd = Some(0.18);
        state.goal = Some(test_goal(
            GoalStatus::Active,
            vec![goal_item("Hover everything", false)],
        ));
    }
    assert!(
        app.store
            .apply_event(user_input(session, 1, run, "first question"))
    );
    assert!(app.store.apply_event(admitted(session, 2, run, "mine")));
    let mut theirs = admitted(session, 3, run, "theirs");
    theirs.origin = Some(
        cookie_agent_protocol::EventOrigin::new("client:tui-0192f3a45b6c7d8e9f00112233445566")
            .expect("origin"),
    );
    assert!(app.store.apply_event(theirs));
    let child = SessionId::new_v7();
    let grandchild = SessionId::new_v7();
    app.tree_root = Some(session);
    app.tree = Some(SessionTree {
        session: titled_meta(session, "root", 1),
        children: vec![SessionTree {
            session: delegated_meta(child, session, "worker"),
            children: vec![SessionTree {
                session: delegated_meta(grandchild, session, "helper"),
                children: Vec::new(),
            }],
        }],
    });
    app.handle_paste("a\nb\nc\nd\ne\nf\ng\nh");
    (app, session, call_id)
}

#[tokio::test]
async fn every_clickable_element_hovers_visibly() {
    let mut seen = BTreeSet::new();

    let (mut app, session, _) = content_app().await;
    seen.extend(sweep(&mut app, "content"));

    let bar = app.hit_map.background_bar.expect("background bar");
    app.handle_click(bar.x, bar.y).await;
    assert_eq!(app.modal, Modal::Background);
    seen.extend(sweep(&mut app, "background panel"));

    app.modal = Modal::None;
    draw(&mut app);
    let (details, _) = app
        .hit_map
        .goal_actions
        .iter()
        .copied()
        .find(|(_, action)| *action == GoalBarAction::Details)
        .expect("goal details");
    app.handle_click(details.x, details.y).await;
    assert_eq!(app.modal, Modal::GoalDetail);
    seen.extend(sweep(&mut app, "goal detail"));

    app.modal = Modal::None;
    app.open_command_palette();
    seen.extend(sweep(&mut app, "palette"));
    app.close_command_palette();

    app.open_selection_modal(Modal::Models);
    assert_eq!(app.modal, Modal::Models);
    seen.extend(sweep(&mut app, "model picker"));
    app.modal = Modal::None;

    let listed = app.sessions.clone();
    app.sessions = vec![titled_meta(session, "root", 1)];
    app.modal = Modal::Sessions;
    seen.extend(sweep(&mut app, "session picker"));
    app.session_delete = Some(SessionDeletePrompt {
        session: titled_meta(session, "root", 1),
        subagents: Some(2),
    });
    seen.extend(sweep(&mut app, "session delete confirmation"));
    app.session_delete = None;
    app.sessions = listed;
    app.modal = Modal::None;

    app.modal = Modal::Permissions;
    app.permission_panel.install(sample_permissions());
    app.permission_panel.selection.select(Some(7));
    seen.extend(sweep(&mut app, "permissions"));
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
        .await;
    assert!(app.permission_panel.form.is_some());
    seen.extend(sweep(&mut app, "permission form"));
    app.permission_panel.form = None;
    app.modal = Modal::None;

    app.modal = Modal::Runtimes;
    seen.extend(sweep(&mut app, "runtime panel"));
    app.runtime_panel.view = RuntimeView::Machines;
    app.runtime_panel.install_machines(Ok(sample_machines()));
    // A pinned runtime offers both of a runtime's actions.
    app.runtime_panel.machine_selection.select(Some(1));
    seen.extend(sweep(&mut app, "machines view"));
    app.runtime_panel.machine_selection.select(Some(0));
    seen.extend(sweep(&mut app, "machines view, a machine selected"));
    app.runtime_panel = Default::default();
    app.modal = Modal::None;

    app.open_memory_panel();
    app.memory_panel.listing = Some(Ok(sample_memory()));
    app.memory_panel.refresh_in_flight = false;
    // The second row is a memory, which offers every row action.
    app.memory_panel.selection.select(Some(1));
    seen.extend(sweep(&mut app, "memory panel"));
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
        .await;
    assert!(app.memory_panel.confirm_delete.is_some());
    seen.extend(sweep(&mut app, "memory delete confirmation"));
    app.memory_panel = Default::default();
    app.modal = Modal::None;

    app.begin_provider_form(multi_auth_provider());
    seen.extend(sweep(&mut app, "provider form"));
    app.provider_form = None;
    app.modal = Modal::None;

    app.store
        .sessions
        .get_mut(&session)
        .expect("session")
        .approvals
        .push(approval(session));
    seen.extend(sweep(&mut app, "approval"));

    // An idle session in a project: the bottom bar's project, and its
    // picker.
    let (mut app, _, _, _) = super::projects::project_app().await;
    seen.extend(sweep(&mut app, "project label"));
    app.open_project_picker();
    seen.extend(sweep(&mut app, "project picker"));

    let (mut app, _, _) = app_with_user_messages().await;
    rendered_frame(&mut app, WIDTH, HEIGHT);
    let hit = user_hit(&app, 2);
    app.handle_click(hit.rect.x + 2, hit.rect.y).await;
    assert_eq!(app.modal, Modal::UserMessage);
    seen.extend(sweep(&mut app, "user message menu"));
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::RevertConfirm);
    seen.extend(sweep(&mut app, "revert confirm"));

    // The surfaces above reach every target there is.
    let missing = [
        "palette row",
        "goal close",
        "permission panel mode",
        "permission effect",
        "permission row",
        "background row",
        "background stop",
        "runtime tab",
        "machine row",
        "machine action",
        "memory row",
        "memory action",
        "confirm button",
        "picker input",
        "picker row",
        "session picker action",
        "provider submit",
        "provider cancel",
        "provider field",
        "approval action",
        "approval details",
        "goal action",
        "background bar",
        "permission mode",
        "session cost",
        "project label",
        "event level filter",
        "title segment",
        "composer text",
        "queue entry",
        "passive queue entry",
        "conversation scrollbar",
        "composer scrollbar",
        "user message",
        "background button",
        "transcript block",
        "tree expand",
        "tree row",
    ]
    .into_iter()
    .filter(|kind| !seen.contains(kind))
    .collect::<Vec<_>>();
    assert!(missing.is_empty(), "never swept: {missing:?}");
}

/// Cells of `rect` whose style differs between two frames.
fn restyled(before: &Buffer, after: &Buffer, rect: Rect) -> usize {
    rect.positions()
        .filter(|position| after[*position].style() != before[*position].style())
        .count()
}

#[tokio::test]
async fn a_tool_header_hover_stops_before_its_background_button() {
    let (mut app, session, call_id) = content_app().await;
    for expanded in [false, true] {
        if expanded {
            app.expanded_blocks
                .entry(session)
                .or_default()
                .insert(BlockId::Tool(call_id));
        }
        app.hover = None;
        let plain = draw(&mut app);
        let hit = *app
            .hit_map
            .blocks
            .iter()
            .find(|hit| hit.id == BlockId::Tool(call_id))
            .expect("call hit");
        let button = hit.action_rect.expect("background button");
        let header = hit.hover_rect.expect("header");
        // The button is its own target; the blank column before it and
        // the rest of the title row still toggle the call.
        for x in button.x..button.right() {
            assert_eq!(
                app.hover_target_at(x, button.y),
                Some(PointerTarget::BackgroundButton(call_id)),
                "expanded {expanded}"
            );
        }
        for x in [header.x, button.x - 1, button.right()] {
            assert_eq!(
                app.hover_target_at(x, button.y),
                Some(PointerTarget::TranscriptBlock(BlockId::Tool(call_id))),
                "expanded {expanded} at {x}"
            );
        }
        let gap = Rect::new(button.x - 1, button.y, header.right() - (button.x - 1), 1);
        let title = Rect::new(header.x, button.y, button.x - 1 - header.x, 1);

        // The header highlight covers the title and stops a column short
        // of the button, which stays exactly as drawn.
        app.hover = Some(PointerTarget::TranscriptBlock(BlockId::Tool(call_id)));
        let hovered = draw(&mut app);
        assert!(restyled(&plain, &hovered, title) > 0);
        assert_eq!(restyled(&plain, &hovered, gap), 0, "expanded {expanded}");
        let last = (button.x - 2, button.y);
        assert_eq!(hovered[last].bg, app.theme.block_hover().bg.unwrap());

        // The button fills alone; the title is untouched.
        app.hover = Some(PointerTarget::BackgroundButton(call_id));
        let hovered = draw(&mut app);
        assert_eq!(restyled(&plain, &hovered, title), 0);
        assert_eq!(
            restyled(&plain, &hovered, Rect::new(button.x - 1, button.y, 1, 1)),
            0
        );
        for position in button.positions() {
            assert_eq!(hovered[position].bg, app.theme.hover_fill().bg.unwrap());
        }
    }

    // Clicks keep their meaning: the button backgrounds, the title toggles.
    app.hover = None;
    draw(&mut app);
    let hit = *app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == BlockId::Tool(call_id))
        .expect("call hit");
    let button = hit.action_rect.expect("background button");
    app.handle_click(button.x, button.y).await;
    assert_eq!(app.status, "moving the call to the background…");
    app.handle_click(button.x - 1, button.y).await;
    assert!(!app.expanded_blocks[&session].contains(&BlockId::Tool(call_id)));

    // A row too narrow for the button keeps the whole header hover.
    let mut terminal = Terminal::new(TestBackend::new(44, HEIGHT)).expect("terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("render");
    let hit = *app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == BlockId::Tool(call_id))
        .expect("call hit");
    assert_eq!(hit.action_rect, None);
    assert_eq!(
        hit.hover_cells(app.hit_map.conversation),
        vec![
            hit.hover_rect
                .expect("header")
                .intersection(app.hit_map.conversation.unwrap())
        ]
    );
}

#[tokio::test]
async fn a_tree_expand_marker_hovers_apart_from_its_row() {
    let (mut app, _, _) = content_app().await;
    let plain = draw(&mut app);
    let hit = *app
        .hit_map
        .tree_rows
        .iter()
        .find(|hit| hit.expand_rect.is_some())
        .expect("row with children");
    let marker = hit.expand_rect.expect("marker");
    let id = hit.session_id;
    assert_eq!(
        app.hover_target_at(marker.x, marker.y),
        Some(PointerTarget::TreeExpand(id))
    );
    assert_eq!(
        app.hover_target_at(marker.right(), marker.y),
        Some(PointerTarget::TreeRow(id))
    );
    let row = hit.hover_rect();
    assert!(row.x > marker.right(), "{row:?} {marker:?}");

    // The row glazes past the marker and its space; the marker stays put.
    app.hover = Some(PointerTarget::TreeRow(id));
    let hovered = draw(&mut app);
    assert_eq!(restyled(&plain, &hovered, marker), 0);
    assert_eq!(
        restyled(&plain, &hovered, Rect::new(marker.right(), marker.y, 1, 1)),
        0
    );
    assert!(restyled(&plain, &hovered, row) > 0);

    // The marker fills alone.
    app.hover = Some(PointerTarget::TreeExpand(id));
    let hovered = draw(&mut app);
    assert_eq!(
        hovered[(marker.x, marker.y)].bg,
        app.theme.hover_fill().bg.unwrap()
    );
    assert_eq!(restyled(&plain, &hovered, hit.rect), 1);

    // Clicking the marker collapses; clicking the row watches.
    app.hover = None;
    app.handle_click(marker.x, marker.y).await;
    assert!(app.collapsed_sessions.contains(&id));
    app.handle_click(row.x, row.y).await;
    assert_eq!(app.tree_cursor, Some(id));
}

#[tokio::test]
async fn background_bar_rows_and_stops_hover() {
    let (mut app, _, _) = content_app().await;
    let plain = draw(&mut app);
    let bar = app.hit_map.background_bar.expect("bar");
    assert_eq!(
        app.hover_target_at(bar.x, bar.y),
        Some(PointerTarget::BackgroundBar)
    );
    app.hover = Some(PointerTarget::BackgroundBar);
    let hovered = draw(&mut app);
    // Like the other bottom-bar segments: the text glaze over the bar.
    assert!(restyled(&plain, &hovered, bar) > 0);
    assert_eq!(
        hovered[(bar.right() - 1, bar.y)].bg,
        app.theme.hover().bg.unwrap()
    );
    app.hover = None;
    app.handle_click(bar.x, bar.y).await;
    assert_eq!(app.modal, Modal::Background);

    let plain = draw(&mut app);
    let (row, index) = app.hit_map.background_rows[1];
    let (stop, stop_index) = app.hit_map.background_stops[1];
    assert_eq!((index, stop_index), (1, 1));
    assert_eq!(
        app.hover_target_at(row.x, row.y),
        Some(PointerTarget::BackgroundRow(1))
    );
    assert_eq!(
        app.hover_target_at(stop.x, stop.y),
        Some(PointerTarget::BackgroundStop(1))
    );
    // Rows glaze like picker rows and leave their stop button alone.
    app.hover = Some(PointerTarget::BackgroundRow(1));
    let hovered = draw(&mut app);
    assert!(restyled(&plain, &hovered, row) > 0);
    assert_eq!(restyled(&plain, &hovered, stop), 0);
    // Stop buttons fill like approval buttons and leave their row alone.
    app.hover = Some(PointerTarget::BackgroundStop(1));
    let hovered = draw(&mut app);
    assert_eq!(restyled(&plain, &hovered, row), 0);
    assert_eq!(
        hovered[(stop.x, stop.y)].bg,
        app.theme.hover_fill().bg.unwrap()
    );

    // A row click selects; a click beside the rows still closes the panel.
    app.hover = None;
    app.handle_click(row.x, row.y).await;
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
        .await;
    assert_eq!(app.status, "stopping job_2…");
    app.handle_click(0, 0).await;
    assert_eq!(app.modal, Modal::None);
}

#[tokio::test]
async fn a_hidden_goal_bar_leaves_no_target_behind() {
    let (mut app, session, _) = content_app().await;
    draw(&mut app);
    let actions = app.hit_map.goal_actions.clone();
    assert!(!actions.is_empty());
    // The goal goes away before the next frame: its stale bar geometry
    // neither hovers nor clicks.
    app.store.sessions.get_mut(&session).expect("session").goal = None;
    for (rect, _) in &actions {
        assert!(!matches!(
            app.hover_target_at(rect.x, rect.y),
            Some(PointerTarget::GoalAction(_))
        ));
    }
    let (details, _) = actions[0];
    app.handle_click(details.x, details.y).await;
    assert_ne!(app.modal, Modal::GoalDetail);
    // The next frame drops the geometry altogether.
    draw(&mut app);
    assert!(app.hit_map.goal_actions.is_empty());
}

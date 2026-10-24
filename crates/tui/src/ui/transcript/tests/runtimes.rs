use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::app::*;
use crate::ui::slash::SlashCommand;

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Handles RPC updates and deliveries until `done` holds.
async fn pump_until(
    app: &mut App,
    deliveries: &mut tokio::sync::mpsc::UnboundedReceiver<cookie_agent_protocol::ClientDelivery>,
    done: impl Fn(&App) -> bool,
) {
    for _ in 0..200 {
        if done(app) {
            return;
        }
        tokio::select! {
            update = app.rpc_updates_rx.recv() => {
                app.handle_rpc_update(update.expect("rpc update"));
            }
            delivery = deliveries.recv() => {
                app.handle_delivery(delivery.expect("delivery")).await;
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("nothing happened");
            }
        }
    }
    panic!("condition never held");
}

fn attachment_names(app: &App) -> Vec<String> {
    app.runtime_rows()
        .into_iter()
        .map(|row| row.attachment.name.to_string())
        .collect()
}

#[tokio::test]
async fn the_runtime_panel_detaches_and_attaches_and_the_status_line_follows() {
    let (_directory, server) = crate::tests::in_process_server();
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    let session = client
        .create_session(cookie_agent_protocol::SessionCreateParams::new(
            crate::tests::test_run_selection(),
        ))
        .await
        .expect("session")
        .session
        .session_id;
    let mut app = App::new(client.clone()).await.expect("app");
    let mut deliveries = app.take_deliveries();
    app.open_session(session).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.store
            .sessions
            .get(&session)
            .is_some_and(|state| !state.runtime_attachments.attachments().is_empty())
    })
    .await;

    app.run_command(SlashCommand::Runtime).await;
    assert_eq!(app.modal, Modal::Runtimes);
    pump_until(&mut app, &mut deliveries, |app| {
        app.runtime_panel.listing.is_some()
    })
    .await;
    let names = attachment_names(&app);
    assert_eq!(names.len(), 2, "{names:?}");
    assert_eq!(names[0], "global");
    assert!(names[1].starts_with("local"), "{names:?}");
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("Attached runtimes"), "{rendered}");
    assert!(rendered.contains("ready"), "{rendered}");
    assert!(
        rendered.contains("local · "),
        "the bottom bar names the one runtime besides global, with its workspace: {rendered}"
    );

    // Detach the selected first row, `global`.
    app.handle_key(key(KeyCode::Char('d'))).await;
    pump_until(&mut app, &mut deliveries, |app| {
        attachment_names(app) == [names[1].clone()]
    })
    .await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.status.contains("detached runtime `global`")
    })
    .await;

    // Attach it again: `global` is the first template and takes no workspace.
    app.handle_key(key(KeyCode::Char('a'))).await;
    assert!(app.runtime_panel.form.is_some());
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("Attach runtime"), "{rendered}");
    assert!(rendered.contains("takes no workspace"), "{rendered}");
    app.handle_key(key(KeyCode::Enter)).await;
    pump_until(&mut app, &mut deliveries, |app| {
        attachment_names(app) == [names[1].clone(), "global".to_owned()]
    })
    .await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.status.contains("attached runtime as `global`")
    })
    .await;

    // A rejected attach shows the daemon's reason.
    app.handle_key(key(KeyCode::Char('a'))).await;
    app.handle_key(key(KeyCode::Enter)).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.status.starts_with("attach failed")
    })
    .await;
    assert!(app.status.contains("already attached"), "{}", app.status);
}

/// Before the first prompt, `/runtime` edits the runtimes the new session
/// will attach: they open at once, so their status shows, and the session
/// is created with exactly them.
#[tokio::test]
async fn a_new_session_draft_chooses_its_runtimes_before_the_session_exists() {
    let (_directory, server) = crate::tests::in_process_server();
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    let mut app = App::new_with_new_session(client.clone())
        .await
        .expect("app");
    let mut deliveries = app.take_deliveries();
    assert!(app.new_session_draft.is_some());
    assert!(app.selected.is_none());

    app.run_command(SlashCommand::Runtime).await;
    assert_eq!(app.modal, Modal::Runtimes);
    // `local` opens before any session attaches it.
    pump_until(&mut app, &mut deliveries, |app| {
        app.runtime_panel.listing.is_some()
            && app
                .runtime_rows()
                .iter()
                .any(|row| row.attachment.name.as_str().starts_with("local_"))
    })
    .await;
    let names = attachment_names(&app);
    assert_eq!(names.len(), 2, "{names:?}");
    assert_eq!(names[0], "global");
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("Attached runtimes"), "{rendered}");
    assert!(rendered.contains("ready"), "{rendered}");
    assert!(
        rendered.contains("local · "),
        "the bottom bar names the draft's runtime: {rendered}"
    );

    // Removing and adding edit the draft only; an instance it already
    // attaches is refused.
    app.handle_key(key(KeyCode::Char('a'))).await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(app.status.contains("already attaches"), "{}", app.status);
    app.handle_key(key(KeyCode::Esc)).await;
    app.handle_key(key(KeyCode::Char('d'))).await;
    assert_eq!(attachment_names(&app), [names[1].clone()]);
    app.handle_key(key(KeyCode::Char('a'))).await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(
        attachment_names(&app),
        [names[1].clone(), "global".to_owned()]
    );
    app.handle_key(key(KeyCode::Char('a'))).await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(app.status.contains("already attaches"), "{}", app.status);
    app.handle_key(key(KeyCode::Esc)).await;
    app.handle_key(key(KeyCode::Down)).await;
    app.handle_key(key(KeyCode::Char('d'))).await;
    assert_eq!(attachment_names(&app), [names[1].clone()]);
    app.handle_key(key(KeyCode::Esc)).await;
    assert!(
        client
            .list_sessions(cookie_agent_protocol::SessionListParams::default())
            .await
            .expect("sessions")
            .sessions
            .is_empty(),
        "the draft creates no session"
    );

    // The first prompt creates the session with the draft's runtimes: here
    // without `global`.
    type_input(&mut app, "hello").await;
    app.handle_key(key(KeyCode::Enter)).await;
    let created = app.selected.expect("created session");
    assert!(app.draft_runtimes.is_none());
    pump_until(&mut app, &mut deliveries, |app| {
        app.store
            .sessions
            .get(&created)
            .is_some_and(|state| !state.runtime_attachments.attachments().is_empty())
    })
    .await;
    let attached = app.store.sessions[&created]
        .runtime_attachments
        .attachments()
        .iter()
        .map(|attachment| attachment.name.to_string())
        .collect::<Vec<_>>();
    assert_eq!(attached, [names[1].clone()]);
}

fn machine_names(app: &App) -> Vec<String> {
    match &app.runtime_panel.machines {
        Some(Ok(listing)) => listing
            .machines
            .iter()
            .map(|machine| machine.name.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

async fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        app.handle_key(key(KeyCode::Char(character))).await;
    }
}

/// The `/runtime` panel's machines view groups runtimes by machine, moves
/// a runtime to a new machine, renames and merges machines, and shows the
/// daemon's refusal.
#[tokio::test]
async fn the_machines_view_regroups_runtimes_and_renames_and_merges_machines() {
    let (_directory, server) = crate::tests::in_process_server();
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    let session = client
        .create_session(cookie_agent_protocol::SessionCreateParams::new(
            crate::tests::test_run_selection(),
        ))
        .await
        .expect("session")
        .session
        .session_id;
    let mut app = App::new(client.clone()).await.expect("app");
    let mut deliveries = app.take_deliveries();
    app.open_session(session).await;
    app.run_command(SlashCommand::Runtime).await;
    // The Machines tab opens the view.
    rendered_frame(&mut app, 120, 40);
    let (tab, _) = app
        .hit_map
        .runtimes
        .tabs
        .iter()
        .copied()
        .find(|(_, view)| *view == crate::ui::management::RuntimeView::Machines)
        .expect("machines tab");
    app.handle_click(tab.x, tab.y).await;
    assert_eq!(
        app.runtime_panel.view,
        crate::ui::management::RuntimeView::Machines
    );
    pump_until(&mut app, &mut deliveries, |app| {
        machine_names(app).len() == 1 && app.runtime_panel.listing.is_some()
    })
    .await;
    let daemon = machine_names(&app)[0].clone();
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains(&daemon), "{rendered}");
    assert!(rendered.contains("  local  by machine ID"), "{rendered}");

    // Move `local` (the third row: the machine, `global`, `local`) to a new
    // machine.
    app.handle_key(key(KeyCode::Down)).await;
    app.handle_key(key(KeyCode::Down)).await;
    app.handle_key(key(KeyCode::Char('v'))).await;
    assert!(app.runtime_panel.machine_form.is_some());
    type_text(&mut app, "sandbox").await;
    app.handle_key(key(KeyCode::Enter)).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.status == "pinned runtime local to machine sandbox"
    })
    .await;
    let mut expected = vec![daemon.clone(), "sandbox".to_owned()];
    expected.sort();
    assert_eq!(machine_names(&app), expected);

    // The attachments view names the new machine.
    app.handle_key(key(KeyCode::Char('m'))).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.runtime_panel.listing.as_ref().is_some_and(|listing| {
            listing.runtimes.iter().any(|runtime| {
                runtime.runtime_id.as_str() == "local"
                    && runtime.machine.as_ref().map(|machine| machine.as_str()) == Some("sandbox")
            })
        })
    })
    .await;
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("on sandbox"), "{rendered}");
    app.handle_key(key(KeyCode::Char('m'))).await;

    // A taken name is refused with the daemon's reason.
    let row = app
        .runtime_panel
        .machine_entries()
        .iter()
        .position(|entry| {
            *entry
                == crate::ui::management::MachineEntry::Machine(
                    cookie_agent_protocol::MachineName::new("sandbox").unwrap(),
                )
        })
        .expect("sandbox row");
    app.runtime_panel.machine_selection.select(Some(row));
    // The rename action is a button too.
    rendered_frame(&mut app, 120, 40);
    let (rename, _) = app
        .hit_map
        .runtimes
        .machine_actions
        .iter()
        .copied()
        .find(|(_, action)| *action == crate::ui::management::MachineAction::Rename)
        .expect("rename button");
    app.handle_click(rename.x, rename.y).await;
    assert!(app.runtime_panel.machine_form.is_some());
    type_text(&mut app, &daemon).await;
    app.handle_key(key(KeyCode::Enter)).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.status.starts_with("machine change failed")
    })
    .await;
    assert!(app.status.contains("is taken"), "{}", app.status);

    // An invalid name never reaches the daemon.
    app.handle_key(key(KeyCode::Char('n'))).await;
    type_text(&mut app, "Not Valid").await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(
        app.status.contains("is not a machine name"),
        "{}",
        app.status
    );
    app.handle_key(key(KeyCode::Esc)).await;
    assert!(app.runtime_panel.machine_form.is_none());
    assert_eq!(app.modal, Modal::Runtimes);

    // Merging the sandbox back leaves one machine.
    app.handle_key(key(KeyCode::Char('g'))).await;
    type_text(&mut app, &daemon).await;
    app.handle_key(key(KeyCode::Enter)).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.status.starts_with("merged machine sandbox")
    })
    .await;
    assert_eq!(machine_names(&app), [daemon]);
}

/// A machine change announced while `machines.list` is on its way may
/// postdate what that listing read: the panel lists the machines again
/// once it lands, so the change is not lost behind it.
#[tokio::test]
async fn a_machine_change_announced_during_a_listing_lists_again() {
    let mut app = test_app().await;
    let (client, recorded) = answering_client(|method, _| match method {
        "machines.list" => {
            Ok(serde_json::to_value(crate::ui::management::sample_machines()).unwrap())
        }
        "runtimes.list" => Ok(serde_json::json!({"runtimes": [], "instances": []})),
        "mcp.server.list" => Ok(serde_json::json!({"servers": []})),
        _ => Err(serde_json::json!({"code": -32601, "message": "not scripted"})),
    });
    app.client = client;
    app.modal = Modal::Runtimes;
    app.poll_runtimes();
    assert!(app.runtime_panel.machines_refresh.in_flight());
    app.handle_delivery(cookie_agent_protocol::ClientDelivery::MachinesChanged(
        cookie_agent_protocol::MachinesChangedNotification {},
    ))
    .await;
    updates_until(&mut app, |app| {
        matches!(app.runtime_panel.machines, Some(Ok(_)))
    })
    .await;
    wait_for_recorded_request(&recorded, "machines.list", 2).await;
    updates_until(&mut app, |app| {
        !app.runtime_panel.machines_refresh.in_flight() && !app.runtime_panel.refresh.in_flight()
    })
    .await;
    // Settled: nothing further is asked for.
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "machines.list"), 2);
}

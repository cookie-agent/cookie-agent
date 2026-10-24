//! `/memory`: the daemon's memories by layer, edited in an external editor
//! and saved at the revision read, created, and deleted; invalid files and
//! other entries are shown with the reason, and a memory changed while it
//! was open is reloaded or overwritten only when the user says so.

use cookie_agent_protocol::{ClientDelivery, MemoryLayer};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::app::memory_panel::{
    MemoryAction, MemoryRow, NEW_MEMORY, PendingEdit, sample_memory,
};
use crate::ui::app::*;
use crate::ui::slash::SlashCommand;

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Handles RPC updates and deliveries until `done` holds.
async fn pump_until(
    app: &mut App,
    deliveries: &mut tokio::sync::mpsc::UnboundedReceiver<ClientDelivery>,
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
                panic!("no update in time; status: {}", app.status);
            }
        }
    }
    panic!("condition never held; status: {}", app.status);
}

fn row_of(app: &App, wanted: impl Fn(&MemoryRow) -> bool) -> usize {
    app.memory_panel
        .rows()
        .iter()
        .position(wanted)
        .expect("the row")
}

/// `text` without the stamp lines the daemon writes into a memory's
/// frontmatter.
fn unstamped(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| !line.starts_with("created: ") && !line.starts_with("modified: "))
        .collect()
}

fn memory_row(app: &App, url: &str) -> Option<usize> {
    app.memory_panel
        .rows()
        .iter()
        .position(|row| matches!(row, MemoryRow::Memory(memory) if memory.url == url))
}

async fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        app.handle_key(key(KeyCode::Char(character))).await;
    }
}

#[tokio::test]
async fn memories_are_browsed_created_edited_and_deleted_from_the_panel() {
    let (directory, server) = crate::tests::in_process_server();
    let memory = directory.path().join("data/memory");
    std::fs::create_dir_all(memory.join("machines/labpc")).unwrap();
    std::fs::write(memory.join("machines/labpc/gpu.md"), "no frontmatter\n").unwrap();
    std::fs::write(memory.join("user/notes.txt"), "stray").unwrap();
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    // A project memory needs its project.
    client
        .create_project(cookie_agent_protocol::ProjectCreateParams {
            name: cookie_agent_protocol::ProjectName::new("web").expect("project name"),
            emoji: None,
            defaults: None,
        })
        .await
        .expect("project.create");
    let mut app = App::new(client).await.expect("app");
    let mut deliveries = app.take_deliveries();

    // The panel lists every layer; invalid files and stray entries show
    // with the reason.
    app.run_command(SlashCommand::Memory).await;
    assert_eq!(app.modal, Modal::Memory);
    pump_until(&mut app, &mut deliveries, |app| {
        matches!(app.memory_panel.listing, Some(Ok(_)))
    })
    .await;
    assert!(memory_row(&app, "user://preferences").is_some());
    let rendered = rendered_frame(&mut app, 120, 50);
    for text in [
        "Memory",
        "User",
        "preferences",
        "Machines",
        "labpc",
        "gpu  invalid",
        "Projects",
        "user/notes.txt  not a `.md` file",
    ] {
        assert!(rendered.contains(text), "{text}:\n{rendered}");
    }
    let gpu = memory_row(&app, "machine://labpc/gpu").expect("gpu row");
    app.memory_panel.selection.select(Some(gpu));
    assert!(
        rendered_frame(&mut app, 120, 50).contains("invalid:"),
        "the details give the reason"
    );

    // `n` on the Projects heading starts a project URL; invalid text is
    // offered again as it was left.
    let projects = row_of(&app, |row| *row == MemoryRow::Layer(MemoryLayer::Project));
    app.memory_panel.selection.select(Some(projects));
    app.handle_key(key(KeyCode::Char('n'))).await;
    assert_eq!(
        app.memory_panel
            .new_url
            .as_ref()
            .map(|input| input.as_str().to_owned()),
        Some("project://".to_owned())
    );
    type_text(&mut app, "web/build").await;
    app.handle_key(key(KeyCode::Enter)).await;
    let request = app.pending_memory_editor.take().expect("a new memory");
    assert_eq!(request.url, "project://web/build");
    assert_eq!(request.text, NEW_MEMORY);
    assert_eq!(request.revision, None);
    app.finish_memory_edit(request, Ok("---\ndescription: \n---\n".into()));
    pump_until(&mut app, &mut deliveries, |app| {
        matches!(app.memory_panel.pending, Some(PendingEdit::Invalid { .. }))
    })
    .await;
    assert!(app.status.starts_with("Not saved:"), "{}", app.status);
    app.handle_key(key(KeyCode::Char('e'))).await;
    let request = app
        .pending_memory_editor
        .take()
        .expect("the invalid text again");
    assert_eq!(request.text, "---\ndescription: \n---\n");
    let valid = "---\ndescription: Build conventions\n---\nRun the gates.\n";
    app.finish_memory_edit(request, Ok(valid.into()));
    // The new row can arrive with the daemon's change notice before the
    // save's own answer, which sets the status.
    pump_until(&mut app, &mut deliveries, |app| {
        memory_row(app, "project://web/build").is_some() && !app.status.starts_with("saving")
    })
    .await;
    assert_eq!(app.status, "Created project://web/build.");
    let path = memory.join("projects/web/build.md");
    assert_eq!(unstamped(&std::fs::read_to_string(&path).unwrap()), valid);

    // Enter opens the selected memory at its revision; an unchanged edit
    // saves nothing.
    let build = memory_row(&app, "project://web/build").unwrap();
    app.memory_panel.selection.select(Some(build));
    app.handle_key(key(KeyCode::Enter)).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.pending_memory_editor.is_some()
    })
    .await;
    let request = app.pending_memory_editor.take().unwrap();
    // The daemon stamped the file; the editor leaves its stamp lines out.
    let stamped = std::fs::read_to_string(&path).unwrap();
    let created = stamped
        .lines()
        .find(|line| line.starts_with("created: "))
        .expect("the daemon's created stamp")
        .to_owned();
    assert!(stamped.contains("\nmodified: "), "{stamped}");
    assert_eq!(request.text, valid);
    assert_eq!(request.original, valid);
    assert!(request.revision.is_some());
    let opened = request.text.clone();
    app.finish_memory_edit(request, Ok(opened));
    assert_eq!(app.status, "No changes to project://web/build.");

    // A memory changed while it was open is not overwritten: reload opens
    // the current version.
    app.handle_key(key(KeyCode::Enter)).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.pending_memory_editor.is_some()
    })
    .await;
    let request = app.pending_memory_editor.take().unwrap();
    let theirs = "---\ndescription: Build conventions\n---\nRun every gate.\n";
    // Another writer changes the body and keeps the stamps: dropping them
    // would leave the file's own time as its creation, which the overwrite
    // below could not tell from when the memory was made.
    let theirs_on_disk = stamped.replace("Run the gates.\n", "Run every gate.\n");
    assert_eq!(unstamped(&theirs_on_disk), theirs);
    std::fs::write(&path, &theirs_on_disk).unwrap();
    let mine = "---\ndescription: Build conventions\n---\nMine.\n";
    app.finish_memory_edit(request.clone(), Ok(mine.into()));
    pump_until(&mut app, &mut deliveries, |app| {
        matches!(app.memory_panel.pending, Some(PendingEdit::Changed { .. }))
    })
    .await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs_on_disk);
    assert!(
        rendered_frame(&mut app, 120, 50).contains("changed since you opened it"),
        "the details say why"
    );
    app.handle_key(key(KeyCode::Char('r'))).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.pending_memory_editor.is_some()
    })
    .await;
    let reloaded = app.pending_memory_editor.take().unwrap();
    assert_eq!(reloaded.text, theirs);
    assert!(app.memory_panel.pending.is_none());

    // Or the user overwrites the current version with the edit.
    app.finish_memory_edit(request, Ok(mine.into()));
    pump_until(&mut app, &mut deliveries, |app| {
        matches!(app.memory_panel.pending, Some(PendingEdit::Changed { .. }))
    })
    .await;
    app.handle_key(key(KeyCode::Char('o'))).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.status == "Saved project://web/build."
    })
    .await;
    let overwritten = std::fs::read_to_string(&path).unwrap();
    assert_eq!(unstamped(&overwritten), mine);
    // Saving text without the stamps keeps when the memory was created.
    assert!(
        overwritten.lines().any(|line| line == created),
        "{overwritten}"
    );

    // `d` asks first; Esc keeps the memory, and the button deletes it.
    pump_until(&mut app, &mut deliveries, |app| {
        !app.memory_panel.refresh_in_flight
    })
    .await;
    let build = memory_row(&app, "project://web/build").unwrap();
    app.memory_panel.selection.select(Some(build));
    app.handle_key(key(KeyCode::Char('d'))).await;
    assert!(app.memory_panel.confirm_delete.is_some());
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.status, "Delete cancelled.");
    assert_eq!(app.modal, Modal::Memory);
    app.handle_key(key(KeyCode::Char('d'))).await;
    rendered_frame(&mut app, 120, 50);
    let (button, _) = app
        .hit_map
        .memory
        .actions
        .iter()
        .copied()
        .find(|(_, action)| *action == MemoryAction::ConfirmDelete)
        .expect("the delete button");
    app.handle_click(button.x, button.y).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.status == "Deleted project://web/build."
    })
    .await;
    assert!(!path.exists());
    pump_until(&mut app, &mut deliveries, |app| {
        memory_row(app, "project://web/build").is_none()
    })
    .await;

    // Clicking a row selects it; Esc closes the panel.
    rendered_frame(&mut app, 120, 50);
    let (rect, index) = app.hit_map.memory.rows[2];
    app.handle_click(rect.x, rect.y).await;
    assert_eq!(app.memory_panel.selection.selected(), Some(index));
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.modal, Modal::None);
}

/// The daemon keeps a memory file for each agent and owns its description:
/// a save that changes it keeps the daemon's, and the status line says so.
#[tokio::test]
async fn saving_an_agent_memory_says_what_the_daemon_kept() {
    let (_directory, server) = crate::tests::in_process_server();
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    let mut app = App::new(client).await.expect("app");
    let mut deliveries = app.take_deliveries();
    app.run_command(SlashCommand::Memory).await;
    pump_until(&mut app, &mut deliveries, |app| {
        matches!(app.memory_panel.listing, Some(Ok(_)))
    })
    .await;
    let agent = row_of(
        &app,
        |row| matches!(row, MemoryRow::Memory(memory) if memory.layer == MemoryLayer::Agent),
    );
    let MemoryRow::Memory(memory) = app.memory_panel.rows()[agent].clone() else {
        unreachable!("an agent memory row");
    };
    app.memory_panel.selection.select(Some(agent));
    app.handle_key(key(KeyCode::Enter)).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.pending_memory_editor.is_some()
    })
    .await;
    let request = app.pending_memory_editor.take().unwrap();
    app.finish_memory_edit(
        request,
        Ok("---\ndescription: Mine\nkeywords: [extra]\n---\n- a lesson\n".into()),
    );
    pump_until(&mut app, &mut deliveries, |app| {
        app.status.starts_with("Saved")
    })
    .await;
    let description = memory.description.expect("a valid agent memory");
    assert!(
        app.status.starts_with(&format!(
            "Saved {}. The description of {} is the daemon's and cannot be changed, so it stays \"{description}\".",
            memory.url, memory.url
        )),
        "{}",
        app.status
    );
}

/// The details give a memory's stamps with their exact times and its size
/// against its limit; a memory over its limit is marked in its row and
/// says what the daemon still accepts.
#[tokio::test]
async fn the_panel_shows_stamps_sizes_and_what_is_over_its_limit() {
    let mut app = test_app().await;
    app.open_memory_panel();
    app.install_memory_listing(Ok(sample_memory()));
    let feedback = memory_row(&app, "user://feedback").expect("feedback row");
    app.memory_panel.selection.select(Some(feedback));
    let rendered = rendered_frame(&mut app, 120, 50);
    for text in [
        "created: 2026-10-01 09:12:44 UTC (",
        "modified: 2026-10-20 16:03:10 UTC (",
        "last read: 2026-10-21 08:30:00 UTC (",
        "size: 64 bytes of 2 KB",
        "gpu  The GPU  over its limit",
    ] {
        assert!(rendered.contains(text), "{text}:\n{rendered}");
    }
    assert!(
        !rendered.contains("only changes that make it smaller"),
        "{rendered}"
    );
    let gpu = memory_row(&app, "machine://labpc/gpu").expect("gpu row");
    app.memory_panel.selection.select(Some(gpu));
    let rendered = rendered_frame(&mut app, 120, 50);
    for text in [
        "size: 2.9 KB of 2 KB, over its limit",
        "only changes that make it smaller are saved",
    ] {
        assert!(rendered.contains(text), "{text}:\n{rendered}");
    }
}

/// A model's memory change is a transcript line whose click opens the
/// panel at that memory, and a line about a memory that is gone says so.
#[tokio::test]
async fn a_memory_change_line_opens_the_panel_at_its_memory() {
    use cookie_agent_protocol::{EventPayload, MemoryChangeKind};

    use crate::ui::transcript::BlockId;

    let (mut app, session, run) = app_with_active_run().await;
    let changed = |seq, change, url: &str| {
        event(
            session,
            seq,
            run,
            EventPayload::MemoryChanged {
                change,
                url: url.into(),
            },
        )
    };
    assert!(
        app.store
            .apply_event(changed(1, MemoryChangeKind::Saved, "machine://labpc/gpu"))
    );
    assert!(
        app.store
            .apply_event(changed(2, MemoryChangeKind::Deleted, "project://web/old"))
    );
    let rendered = rendered_frame(&mut app, 100, 30);
    for text in ["Saved machine://labpc/gpu", "Deleted project://web/old"] {
        assert!(rendered.contains(text), "{text}:\n{rendered}");
    }
    let line = |app: &App, seq| {
        app.hit_map
            .blocks
            .iter()
            .find(|hit| hit.id == BlockId::MemoryChange(seq))
            .and_then(|hit| hit.toggle_rect)
            .expect("a clickable line")
    };
    let saved = line(&app, 1);
    assert_eq!(
        app.hover_target_at(saved.x, saved.y),
        Some(PointerTarget::TranscriptBlock(BlockId::MemoryChange(1))),
        "the line hovers"
    );
    app.handle_click(saved.x, saved.y).await;
    assert_eq!(app.modal, Modal::Memory);
    app.install_memory_listing(Ok(sample_memory()));
    assert_eq!(
        app.memory_panel.selection.selected(),
        memory_row(&app, "machine://labpc/gpu")
    );
    // A later listing leaves the selection to the user.
    app.memory_panel.selection.select(Some(0));
    app.install_memory_listing(Ok(sample_memory()));
    assert_eq!(app.memory_panel.selection.selected(), Some(0));

    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.modal, Modal::None);
    rendered_frame(&mut app, 100, 30);
    let deleted = line(&app, 2);
    app.handle_click(deleted.x, deleted.y).await;
    assert_eq!(app.modal, Modal::Memory);
    app.install_memory_listing(Ok(sample_memory()));
    assert_eq!(app.status, "project://web/old is not in memory now.");
}

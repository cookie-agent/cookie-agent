//! `/config`: the daemon's configuration documents, edited in an external
//! editor and applied, with refused edits kept for the next try.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::app::*;
use crate::ui::slash::SlashCommand;

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

async fn pump_until(app: &mut App, done: impl Fn(&App) -> bool) {
    for _ in 0..200 {
        if done(app) {
            return;
        }
        let update =
            tokio::time::timeout(std::time::Duration::from_secs(5), app.rpc_updates_rx.recv())
                .await
                .expect("an update in time")
                .expect("rpc update");
        app.handle_rpc_update(update);
    }
    panic!("condition never held");
}

fn labels(app: &App) -> Vec<String> {
    app.config_panel
        .rows()
        .into_iter()
        .map(|row| row.label)
        .collect()
}

const REVIEWER: &str = "---\ndescription: Reviewer\nmode: subagent\nenabled: true\nmodels: [{ model: \"gateway/arbitrary-model\" }]\npermissions: {}\n---\nReview.\n";

#[tokio::test]
async fn config_documents_are_edited_created_and_deleted_from_the_panel() {
    let (directory, server) = crate::tests::in_process_server_with_configuration();
    let root = directory.path().join(".cookie-agent");
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    let mut app = App::new(client).await.expect("app");
    let _deliveries = app.take_deliveries();

    app.run_command(SlashCommand::Config).await;
    assert_eq!(app.modal, Modal::Config);
    pump_until(&mut app, |app| app.config_panel.config.is_some()).await;
    assert_eq!(labels(&app), ["config.toml (not created)"]);
    assert!(rendered_frame(&mut app, 120, 40).contains("Configuration"));

    // Enter opens the selected document in the editor.
    app.handle_key(key(KeyCode::Enter)).await;
    let request = app.pending_editor.take().expect("an editor request");
    assert_eq!(request.path, "config.toml");
    assert_eq!(request.text, "");

    // A refused edit is kept and reopened by the next Enter.
    app.finish_config_edit(request, Ok("[unknown]\n".into()));
    pump_until(&mut app, |app| {
        app.config_panel.drafts.contains_key("config.toml")
    })
    .await;
    assert!(
        app.config_panel
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unknown")),
        "{:?}",
        app.config_panel.error
    );
    app.handle_key(key(KeyCode::Enter)).await;
    let request = app.pending_editor.take().expect("the refused edit again");
    assert_eq!(request.text, "[unknown]\n");
    app.finish_config_edit(request, Ok("[loop_warning]\nenabled = false\n".into()));
    pump_until(&mut app, |app| {
        app.status.starts_with("Applied config.toml")
    })
    .await;
    assert!(app.config_panel.drafts.is_empty());
    assert_eq!(
        std::fs::read_to_string(root.join("config.toml")).unwrap(),
        "[loop_warning]\nenabled = false\n"
    );
    pump_until(&mut app, |app| labels(app) == ["config.toml"]).await;

    // An unchanged edit applies nothing.
    app.handle_key(key(KeyCode::Enter)).await;
    let request = app.pending_editor.take().expect("editor request");
    let text = request.text.clone();
    app.finish_config_edit(request, Ok(text));
    assert_eq!(app.status, "No changes to config.toml.");

    // `n` creates an agent from the template.
    app.handle_key(key(KeyCode::Char('n'))).await;
    type_input(&mut app, "reviewer.md").await;
    app.handle_key(key(KeyCode::Enter)).await;
    let request = app.pending_editor.take().expect("a new agent");
    assert_eq!(request.path, "agents/reviewer.md");
    assert!(request.text.starts_with("---\ndescription:"));
    assert_eq!(request.expected_fingerprint, None);
    app.finish_config_edit(request, Ok(REVIEWER.into()));
    pump_until(&mut app, |app| {
        labels(app).contains(&"agent reviewer".to_owned())
    })
    .await;
    assert!(root.join("agents/reviewer.md").exists());

    // `d` then `y` deletes it.
    app.handle_key(key(KeyCode::Down)).await;
    app.handle_key(key(KeyCode::Char('d'))).await;
    assert_eq!(
        app.config_panel.confirm_delete.as_deref(),
        Some("agents/reviewer.md")
    );
    app.handle_key(key(KeyCode::Char('y'))).await;
    pump_until(&mut app, |app| labels(app) == ["config.toml"]).await;
    assert!(!root.join("agents/reviewer.md").exists());

    // `r` reloads hand edits.
    std::fs::write(root.join("config.toml"), "").unwrap();
    app.handle_key(key(KeyCode::Char('r'))).await;
    pump_until(&mut app, |app| app.status == "Reloaded the configuration.").await;
}

/// A configuration listing whose plugins wait for a restart.
pub(crate) fn sample_config() -> cookie_agent_protocol::ConfigGetResult {
    let revision = cookie_agent_protocol::ConfigRevision::new(format!("sha256:{}", "3".repeat(64)))
        .expect("revision");
    cookie_agent_protocol::ConfigGetResult {
        revision,
        documents: vec![cookie_agent_protocol::ConfigDocument {
            path: "config.toml".into(),
            text: "[plugins]\n".into(),
            fingerprint: format!("sha256:{}", "4".repeat(64)),
        }],
        empty_presets: Vec::new(),
        status: super::pointer::restart_waiting_status(),
    }
}

/// `R` and the panel's Restart daemon button open the restart
/// confirmation over the panel, which it returns to; an embedded daemon
/// refuses the restart and the TUI says why. `/restart` opens the same
/// confirmation, and the daemon's announcement shows in the banner.
#[tokio::test]
async fn the_config_panel_restarts_the_daemon_and_returns_to_the_panel() {
    let (_directory, server) = crate::tests::in_process_server_with_configuration();
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    let mut app = App::new(client).await.expect("app");
    let _deliveries = app.take_deliveries();

    app.run_command(SlashCommand::Config).await;
    pump_until(&mut app, |app| app.config_panel.config.is_some()).await;
    let frame = rendered_frame(&mut app, 120, 40);
    assert!(frame.contains("Reload from disk (r)"), "{frame}");
    assert!(frame.contains("Restart daemon (R)"), "{frame}");

    app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT))
        .await;
    assert_eq!(app.modal, Modal::RestartConfirm);
    let frame = rendered_frame(&mut app, 120, 40);
    assert!(frame.contains("Restart when idle"), "{frame}");
    assert!(frame.contains("Restart now"), "{frame}");
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.modal, Modal::Config, "cancelling returns to the panel");

    rendered_frame(&mut app, 120, 40);
    let (button, _) = app
        .hit_map
        .config_actions
        .iter()
        .copied()
        .find(|(_, action)| *action == crate::ui::app::config_panel::ConfigAction::Restart)
        .expect("the restart button");
    app.handle_click(button.x, button.y).await;
    assert_eq!(app.modal, Modal::RestartConfirm);
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(app.modal, Modal::Config);
    pump_until(&mut app, |app| app.status.contains("can't restart itself")).await;

    app.modal = Modal::None;
    app.run_command(SlashCommand::Restart).await;
    assert_eq!(app.modal, Modal::RestartConfirm);
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.modal, Modal::None);

    app.handle_delivery(
        cookie_agent_protocol_transport::ClientDelivery::DaemonRestarting(
            cookie_agent_protocol::DaemonRestartingNotification {
                when: cookie_agent_protocol::RestartWhen::Idle,
                active_runs: 2,
                background_calls: 0,
                reason: None,
            },
        ),
    )
    .await;
    let frame = rendered_frame(&mut app, 120, 40);
    assert!(
        frame.contains("Restarting when idle… (2 running turns)"),
        "{frame}"
    );
}

//! `/webhooks`: the daemon's webhooks with their target, state, check, and
//! last outcome; the selected webhook's details and events; creating and
//! editing a webhook as a form in the editor; the secret shown once;
//! enabling, disabling, testing, rotating, and deleting; an event opened to
//! its request; `/webhooks new` for the session on screen; and the mark of
//! a session a webhook started.

use std::sync::{Arc, Mutex};

use cookie_agent_protocol::{SessionId, SessionOrigin};
use cookie_agent_protocol_transport::ClientDelivery;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::ui::app::webhook_panel::{WebhookAction, WebhookConfirm, sample_webhooks};
use crate::ui::app::*;
use crate::ui::slash::SlashCommand;

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// `field: value` set in a form, replacing the line of that field.
fn with_field(form: &str, field: &str, value: &str) -> String {
    form.lines()
        .map(|line| {
            if line.starts_with(&format!("{field}:")) {
                format!("{field}: {value}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect()
}

/// A form with its prompt replaced by `prompt`.
fn with_prompt(form: &str, prompt: &str) -> String {
    let (header, _) = form.split_once("\n---\n").expect("the prompt line");
    format!("{header}\n---\n{prompt}\n")
}

/// A daemon of client protocol `version` that lists `sample_webhooks`,
/// echoes what it is asked to create or change as a webhook, and answers
/// tests, rotations, secrets, and events.
async fn webhooks_client(
    version: &str,
) -> (
    cookie_agent_protocol_transport::Client,
    Arc<Mutex<Vec<Value>>>,
) {
    let version = version.to_owned();
    let (client, recorded) = answering_client(move |method, params| {
        let sample = serde_json::to_value(sample_webhooks()).unwrap();
        let first = sample["webhooks"][0].clone();
        match method {
            "handshake" => Ok(json!({
                "protocol_version": version,
                "engine_version": "0.4.0",
                "origin": "client:tui-0192f3a45b6c7d8e9f00112233445566",
                "features": []
            })),
            "webhooks.list" => Ok(sample),
            "webhooks.create" => {
                let mut webhook = first;
                webhook["id"] = json!("n3wh00k0");
                webhook["target"] = params["target"].clone();
                webhook["history"] = json!([]);
                let secret = params.get("verify").map(|_| json!("whsec_made"));
                Ok(json!({ "webhook": webhook, "secret": secret }))
            }
            "webhooks.update" => {
                let mut webhook = first;
                if let Some(enabled) = params.get("enabled") {
                    webhook["enabled"] = enabled.clone();
                }
                Ok(json!({ "webhook": webhook }))
            }
            "webhooks.rotate" => {
                let mut webhook = first;
                webhook["path"] = json!("/hooks/NewT0ken0000");
                let secret = (params["what"] == "secret").then(|| json!("s3cret-new"));
                Ok(json!({ "webhook": webhook, "secret": secret }))
            }
            "webhooks.secret" => Ok(json!({ "secret": "s3cret-old" })),
            "webhooks.test" => Ok(json!({ "webhook": first, "event": "e3j8x0pq" })),
            "webhooks.event" => Ok(json!({
                "event": first["history"][0].clone(),
                "method": "POST",
                "headers": [{"name": "Content-Type", "value": "application/json"}],
                "body_text": "{\"action\":\"opened\"}"
            })),
            "webhooks.delete" => Ok(json!({})),
            _ => Err(json!({"code": -32601, "message": "not scripted"})),
        }
    });
    client.handshake().await.expect("handshake");
    (client, recorded)
}

/// A test app viewing a session, talking to a daemon of client protocol
/// `version` (see `webhooks_client`).
async fn webhook_app(version: &str) -> (App, SessionId, Arc<Mutex<Vec<Value>>>) {
    let mut app = test_app().await;
    let viewed: SessionId = "0190a5b2-0000-7000-8000-000000000001".parse().unwrap();
    app.sessions = vec![titled_meta(viewed, "Build fixes", 1)];
    app.selected = Some(viewed);
    app.tree_root = Some(viewed);
    let (client, recorded) = webhooks_client(version).await;
    app.client = client;
    (app, viewed, recorded)
}

async fn listed(app: &mut App) {
    updates_until(app, |app| {
        matches!(app.webhook_panel.listing, Some(Ok(_))) && !app.webhook_panel.refresh.in_flight()
    })
    .await;
}

fn button(app: &App, wanted: WebhookAction) -> ratatui::layout::Rect {
    app.hit_map
        .webhooks
        .actions
        .iter()
        .find(|(_, action)| *action == wanted)
        .map(|(rect, _)| *rect)
        .unwrap_or_else(|| panic!("the {wanted:?} button"))
}

#[tokio::test]
async fn the_panel_lists_webhooks_and_their_urls() {
    let (mut app, viewed, recorded) = webhook_app("1.96.0").await;
    app.run_command(SlashCommand::Webhooks).await;
    assert_eq!(app.modal, Modal::Webhooks);
    listed(&mut app).await;
    assert_eq!(recorded_method_count(&recorded, "webhooks.list"), 1);
    let rows = frame_rows(&mut app, 160, 50).join("\n");
    // The URL is the public base and the webhook's path.
    assert!(
        rows.contains("https://hooks.example.com/hooks/Xq3vT9aLk2M"),
        "{rows}"
    );
    // One row per webhook, and the target links to its session.
    assert_eq!(app.hit_map.webhooks.rows.len(), 2);
    assert_eq!(
        app.hit_map
            .webhooks
            .sessions
            .iter()
            .map(|(_, session)| *session)
            .collect::<Vec<_>>(),
        [viewed]
    );
    // The selected webhook has one event to open.
    assert_eq!(app.hit_map.webhooks.events.len(), 1);

    // A disabled webhook offers enabling, and one with a check its secret.
    let actions = |app: &App| app.webhook_panel.actions(true);
    assert!(actions(&app).contains(&WebhookAction::Disable));
    assert!(!actions(&app).contains(&WebhookAction::RevealSecret));
    app.handle_key(key(KeyCode::Down)).await;
    assert!(actions(&app).contains(&WebhookAction::Enable));
    assert!(actions(&app).contains(&WebhookAction::RevealSecret));
    assert!(!actions(&app).contains(&WebhookAction::Events));

    // A click selects a row; Esc closes the panel.
    let (rect, index) = app.hit_map.webhooks.rows[0];
    rendered_frame(&mut app, 160, 50);
    app.handle_click(rect.x, rect.y).await;
    assert_eq!(app.webhook_panel.selection.selected(), Some(index));
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.modal, Modal::None);
}

#[tokio::test]
async fn a_daemon_before_minor_ninety_six_is_never_asked_for_webhooks() {
    let (mut app, _, recorded) = webhook_app("1.95.0").await;
    let new = crate::ui::slash::COMMANDS
        .iter()
        .find(|spec| spec.name == "webhooks new")
        .expect("/webhooks new");
    assert!(!app.command_is_available(new));
    app.run_command(SlashCommand::Webhooks).await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(recorded_method_count(&recorded, "webhooks.list"), 0);
}

#[tokio::test]
async fn webhooks_new_delivers_into_the_session_on_screen() {
    let (mut app, viewed, recorded) = webhook_app("1.96.0").await;
    let new = crate::ui::slash::COMMANDS
        .iter()
        .find(|spec| spec.name == "webhooks new")
        .expect("/webhooks new");
    assert!(app.command_is_available(new));
    app.run_command(SlashCommand::WebhooksNew).await;
    assert_eq!(app.modal, Modal::Webhooks);
    let request = app.pending_webhook_editor.take().expect("a new form");
    assert_eq!(request.webhook, None);
    // A session target's form names no new-session field.
    assert!(!request.text.contains("\nagent:"), "{}", request.text);

    // An unchanged form creates nothing.
    app.finish_webhook_edit(request.clone(), Ok(request.text.clone()));
    assert!(app.webhook_panel.pending.is_none());
    assert_eq!(recorded_method_count(&recorded, "webhooks.create"), 0);

    // A form that cannot be read is kept to edit again, as it was left.
    let broken = with_field(&request.text, "delivery", "sometimes");
    app.finish_webhook_edit(request.clone(), Ok(broken.clone()));
    assert!(app.webhook_panel.pending.is_some());
    rendered_frame(&mut app, 160, 50);
    assert_eq!(
        app.hit_map
            .webhooks
            .actions
            .iter()
            .map(|(_, action)| *action)
            .collect::<Vec<_>>(),
        [WebhookAction::EditAgain, WebhookAction::Cancel]
    );
    app.handle_key(key(KeyCode::Char('e'))).await;
    let again = app.pending_webhook_editor.take().expect("the form again");
    assert_eq!(again.text, broken);

    let form = with_field(&request.text, "name", "CI");
    let form = with_field(&form, "check", "github");
    let form = with_prompt(&form, "Look at the build.");
    app.finish_webhook_edit(again, Ok(form));
    updates_until(&mut app, |app| app.webhook_panel.secret.is_some()).await;
    let params = last_request_params(&recorded, "webhooks.create");
    assert_eq!(params["target"], json!({"session": viewed.to_string()}));
    assert_eq!(params["name"], json!("CI"));
    assert_eq!(params["prompt"], json!("Look at the build."));
    assert_eq!(params["verify"]["kind"], json!("github"));
    assert!(app.webhook_panel.pending.is_none());

    // The secret the create made is shown once: copy it, then hide it.
    let copied = Arc::new(Mutex::new(Vec::new()));
    app.clipboard_sink = ClipboardSink::Capture(copied.clone());
    assert_eq!(
        app.webhook_panel.actions(true),
        [WebhookAction::CopySecret, WebhookAction::Cancel]
    );
    app.handle_key(key(KeyCode::Char('c'))).await;
    assert_eq!(*copied.lock().unwrap(), ["whsec_made"]);
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.webhook_panel.secret, None);
    assert_eq!(app.modal, Modal::Webhooks);
}

#[tokio::test]
async fn a_subagent_on_screen_is_targeted_as_a_subagent() {
    let (mut app, viewed, _) = webhook_app("1.96.0").await;
    let root = SessionId::new_v7();
    app.sessions[0].origin = SessionOrigin::Delegated {
        root_session_id: root,
        parent_session_id: root,
        parent_run_id: run_id(),
        parent_tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
        invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
        depth: 1,
    };
    app.run_command(SlashCommand::WebhooksNew).await;
    let request = app.pending_webhook_editor.take().expect("a new form");
    let params = request.draft.create_params().expect("params");
    assert_eq!(
        params.target,
        cookie_agent_protocol::WebhookTarget::Subagent(viewed)
    );
}

#[tokio::test]
async fn a_new_session_webhook_takes_its_thread_key_and_thread_prompt() {
    let (mut app, _, recorded) = webhook_app("1.96.0").await;
    app.run_command(SlashCommand::Webhooks).await;
    listed(&mut app).await;
    app.handle_key(key(KeyCode::Char('s'))).await;
    let request = app.pending_webhook_editor.take().expect("a new form");
    assert!(request.text.contains("\nagent: "), "{}", request.text);
    let form = with_field(&request.text, "thread_key", "json:/issue/number");
    let form = format!(
        "{}--- thread prompt\nYou look after one issue.\n",
        with_prompt(&form, "A new event.")
    );
    app.finish_webhook_edit(request, Ok(form));
    updates_until(&mut app, |app| app.webhook_panel.focus.is_some()).await;
    let params = last_request_params(&recorded, "webhooks.create");
    let template = &params["target"]["new"];
    assert_eq!(template["thread_key"], json!({"json": "/issue/number"}));
    assert_eq!(
        template["thread_prompt"],
        json!("You look after one issue.")
    );
    assert_eq!(params["prompt"], json!("A new event."));
}

#[tokio::test]
async fn webhooks_are_toggled_tested_rotated_and_deleted_by_key_and_button() {
    let (mut app, _, recorded) = webhook_app("1.96.0").await;
    app.run_command(SlashCommand::Webhooks).await;
    listed(&mut app).await;

    // Enter opens the selected webhook's form; an edit sends what changed.
    app.handle_key(key(KeyCode::Enter)).await;
    let request = app.pending_webhook_editor.take().expect("the form");
    let edited = with_field(&request.text, "max_per_hour", "5");
    app.finish_webhook_edit(request, Ok(edited));
    updates_until(&mut app, |_| {
        recorded_method_count(&recorded, "webhooks.update") == 1
    })
    .await;
    assert_eq!(
        last_request_params(&recorded, "webhooks.update"),
        json!({"id": "w7k2m9qa", "max_per_hour": 5})
    );

    // `t` disables the enabled webhook.
    listed(&mut app).await;
    app.handle_key(key(KeyCode::Char('t'))).await;
    updates_until(&mut app, |_| {
        recorded_method_count(&recorded, "webhooks.update") == 2
    })
    .await;
    assert_eq!(
        last_request_params(&recorded, "webhooks.update"),
        json!({"id": "w7k2m9qa", "enabled": false})
    );

    // The test button sends a test event.
    listed(&mut app).await;
    rendered_frame(&mut app, 160, 50);
    let test = button(&app, WebhookAction::Test);
    assert_eq!(
        app.hover_target_at(test.x, test.y),
        Some(PointerTarget::WebhookAction(WebhookAction::Test))
    );
    app.handle_click(test.x, test.y).await;
    updates_until(&mut app, |_| {
        recorded_method_count(&recorded, "webhooks.test") == 1
    })
    .await;
    assert_eq!(
        last_request_params(&recorded, "webhooks.test"),
        json!({"id": "w7k2m9qa"})
    );

    // A new URL asks first; Esc keeps the old one, `y` rotates it.
    listed(&mut app).await;
    app.handle_key(key(KeyCode::Char('u'))).await;
    assert!(matches!(
        app.webhook_panel.confirm,
        Some(WebhookConfirm::Rotate(
            _,
            cookie_agent_protocol::WebhookRotate::Url
        ))
    ));
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.webhook_panel.confirm, None);
    assert_eq!(app.modal, Modal::Webhooks);
    assert_eq!(recorded_method_count(&recorded, "webhooks.rotate"), 0);
    app.handle_key(key(KeyCode::Char('u'))).await;
    app.handle_key(key(KeyCode::Char('y'))).await;
    updates_until(&mut app, |_| {
        recorded_method_count(&recorded, "webhooks.rotate") == 1
    })
    .await;
    assert_eq!(
        last_request_params(&recorded, "webhooks.rotate"),
        json!({"id": "w7k2m9qa", "what": "url"})
    );
    updates_until(&mut app, |app| app.status.contains("/hooks/NewT0ken0000")).await;

    // Copying puts the URL on the clipboard.
    listed(&mut app).await;
    let copied = Arc::new(Mutex::new(Vec::new()));
    app.clipboard_sink = ClipboardSink::Capture(copied.clone());
    app.handle_key(key(KeyCode::Char('c'))).await;
    assert_eq!(
        *copied.lock().unwrap(),
        ["https://hooks.example.com/hooks/Xq3vT9aLk2M"]
    );

    // The delete button deletes after its confirmation.
    app.handle_key(key(KeyCode::Char('d'))).await;
    rendered_frame(&mut app, 160, 50);
    let confirm = button(&app, WebhookAction::Confirm);
    app.handle_click(confirm.x, confirm.y).await;
    updates_until(&mut app, |_| {
        recorded_method_count(&recorded, "webhooks.delete") == 1
    })
    .await;
    assert_eq!(
        last_request_params(&recorded, "webhooks.delete"),
        json!({"id": "w7k2m9qa"})
    );
}

#[tokio::test]
async fn a_webhook_with_a_check_shows_and_rotates_its_secret() {
    let (mut app, _, recorded) = webhook_app("1.96.0").await;
    app.run_command(SlashCommand::Webhooks).await;
    listed(&mut app).await;
    app.handle_key(key(KeyCode::Down)).await;

    app.handle_key(key(KeyCode::Char('r'))).await;
    updates_until(&mut app, |app| app.webhook_panel.secret.is_some()).await;
    assert_eq!(
        app.webhook_panel
            .secret
            .as_ref()
            .map(|(_, secret)| secret.as_str()),
        Some("s3cret-old")
    );
    app.handle_key(key(KeyCode::Esc)).await;

    app.handle_key(key(KeyCode::Char('k'))).await;
    app.handle_key(key(KeyCode::Char('y'))).await;
    updates_until(&mut app, |app| app.webhook_panel.secret.is_some()).await;
    assert_eq!(
        last_request_params(&recorded, "webhooks.rotate"),
        json!({"id": "m7p2r4st", "what": "secret"})
    );
    assert_eq!(
        app.webhook_panel
            .secret
            .as_ref()
            .map(|(_, secret)| secret.as_str()),
        Some("s3cret-new")
    );
}

#[tokio::test]
async fn an_event_opens_to_its_request_and_esc_goes_back() {
    let (mut app, _, recorded) = webhook_app("1.96.0").await;
    app.run_command(SlashCommand::Webhooks).await;
    listed(&mut app).await;
    rendered_frame(&mut app, 160, 50);
    let (row, index) = app.hit_map.webhooks.events[0];
    assert_eq!(
        app.hover_target_at(row.x, row.y),
        Some(PointerTarget::WebhookEvent(index))
    );
    app.handle_click(row.x, row.y).await;
    updates_until(&mut app, |app| {
        app.webhook_panel
            .event
            .as_ref()
            .is_some_and(|view| view.request.is_some())
    })
    .await;
    assert_eq!(
        last_request_params(&recorded, "webhooks.event"),
        json!({"id": "w7k2m9qa", "event": "e3j8x0pq"})
    );
    let rows = frame_rows(&mut app, 160, 50).join("\n");
    // The JSON body is pretty-printed.
    assert!(rows.contains("\"action\": \"opened\""), "{rows}");
    // While an event is open, the rows take no clicks.
    assert!(app.hit_map.webhooks.rows.is_empty());
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.webhook_panel.event, None);
    assert_eq!(app.modal, Modal::Webhooks);
}

#[tokio::test]
async fn a_change_announced_while_open_lists_again() {
    let (mut app, _, recorded) = webhook_app("1.96.0").await;
    assert!(
        !app.handle_delivery(ClientDelivery::WebhooksChanged(
            cookie_agent_protocol::WebhooksChangedNotification {},
        ))
        .await
    );
    assert_eq!(recorded_method_count(&recorded, "webhooks.list"), 0);
    app.run_command(SlashCommand::Webhooks).await;
    listed(&mut app).await;
    app.handle_delivery(ClientDelivery::WebhooksChanged(
        cookie_agent_protocol::WebhooksChangedNotification {},
    ))
    .await;
    listed(&mut app).await;
    assert_eq!(recorded_method_count(&recorded, "webhooks.list"), 2);
}

#[tokio::test]
async fn a_webhook_session_has_its_own_mark_in_the_session_list() {
    use jiff::tz::TimeZone;

    let now: jiff::Timestamp = "2026-11-12T12:00:00Z".parse().unwrap();
    let mut started = titled_meta(SessionId::new_v7(), "GitHub issues · 1234", 1);
    started.last_activity = now;
    started.webhook = Some(cookie_agent_protocol::SessionWebhook {
        webhook_id: cookie_agent_protocol::WebhookId::new("w7k2m9qa").unwrap(),
        event_id: "e3j8x0pq".into(),
        thread_key: Some("1234".into()),
    });
    let mut plain = titled_meta(SessionId::new_v7(), "Build fixes", 1);
    plain.last_activity = now;
    let rows = crate::ui::pickers::session_search_rows(
        [&started, &plain],
        "",
        now,
        &TimeZone::UTC,
        None,
        &[],
    );
    let labels: Vec<String> = rows
        .iter()
        .filter_map(|row| match row {
            crate::ui::pickers::SessionSearchRow::Session { label, .. } => Some(label.clone()),
            crate::ui::pickers::SessionSearchRow::Header(_) => None,
        })
        .collect();
    let mark = crate::ui::app::webhook_panel::WEBHOOK_RUN_MARK;
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with(&format!("{mark} GitHub issues"))),
        "{labels:?}"
    );
    assert!(
        labels.iter().any(|label| label.starts_with("Build fixes")),
        "{labels:?}"
    );
}

/// Visual preview of the panel, dumped as `render_preview` dumps a
/// transcript, for `scripts/tui_preview_png.py`:
///
/// ```sh
/// PREVIEW_OUT=/tmp/webhooks.jsonl PREVIEW_STATE=list|event|secret \
///     cargo test -p cookie_agent_tui webhook_panel_preview -- --ignored
/// ```
#[tokio::test]
#[ignore = "manual visual preview"]
async fn webhook_panel_preview() {
    use crate::theme::{ColorLevel, Theme, ThemeKind};
    use ratatui::{Terminal, backend::TestBackend};

    let out = std::env::var("PREVIEW_OUT").expect("PREVIEW_OUT");
    let (mut app, _, _) = webhook_app("1.96.0").await;
    app.theme = Theme::new(
        if std::env::var("PREVIEW_DARK").is_ok() {
            ThemeKind::Dark
        } else {
            ThemeKind::Default
        },
        ColorLevel::TrueColor,
    );
    app.run_command(SlashCommand::Webhooks).await;
    listed(&mut app).await;
    match std::env::var("PREVIEW_STATE").as_deref() {
        Ok("event") => {
            app.handle_key(key(KeyCode::Char('v'))).await;
            updates_until(&mut app, |app| {
                app.webhook_panel
                    .event
                    .as_ref()
                    .is_some_and(|view| view.request.is_some())
            })
            .await;
        }
        Ok("secret") => {
            app.handle_key(key(KeyCode::Down)).await;
            app.handle_key(key(KeyCode::Char('r'))).await;
            updates_until(&mut app, |app| app.webhook_panel.secret.is_some()).await;
        }
        Ok("new") => app.handle_key(key(KeyCode::Down)).await,
        _ => {}
    }
    let mut terminal = Terminal::new(TestBackend::new(140, 44)).unwrap();
    terminal.draw(|frame| app.draw_for_test(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut dump = String::new();
    for y in 0..buffer.area.height {
        let row = (0..buffer.area.width)
            .map(|x| {
                let cell = &buffer[(x, y)];
                json!([
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

#[tokio::test]
async fn a_thread_prompt_is_labelled_with_its_sender_not_as_the_user() {
    use cookie_agent_protocol::{ThreadOwnerRef, WebhookId, thread_prompt_origin};

    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let run = run_id();
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.store
        .sessions
        .insert(session, crate::state::SessionState::default());
    let owner = ThreadOwnerRef::Webhook(WebhookId::new("w7k2m9qa").unwrap());
    let mut prompt = user_input(session, 1, run, "You look after one GitHub issue.");
    prompt.origin = Some(thread_prompt_origin(&owner));
    assert!(app.store.apply_event(prompt));
    let mut typed = user_input(session, 2, run, "And close stale ones.");
    typed.origin = Some(cookie_agent_protocol::EventOrigin::new("client:tui-other").unwrap());
    assert!(app.store.apply_event(typed));

    let state = &app.store.sessions[&session];
    assert_eq!(
        state
            .thread_prompt_origins
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [1]
    );
    // The webhook is named once, above its prompt, and the user's own
    // message carries no sender.
    let rows = frame_rows(&mut app, 100, 30);
    let named = rows
        .iter()
        .position(|row| row.contains("w7k2m9qa"))
        .expect("the sender");
    assert_eq!(
        rows.iter().filter(|row| row.contains("w7k2m9qa")).count(),
        1
    );
    let prompt_row = rows
        .iter()
        .position(|row| row.contains("You look after one GitHub issue."))
        .expect("the prompt");
    let typed_row = rows
        .iter()
        .position(|row| row.contains("And close stale ones."))
        .expect("the typed message");
    assert!(named < prompt_row && prompt_row < typed_row, "{rows:#?}");
}

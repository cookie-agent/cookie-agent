//! `/cron`: the daemon's cron jobs with their schedule read plainly, next
//! fire, target, state, and last outcome; the selected job's details and
//! history; creating and editing a job as a form in the editor; enabling,
//! disabling, running now, and deleting; `/cron new` for the session on
//! screen; and the clock that marks a session a job started.

use std::sync::{Arc, Mutex};

use cookie_agent_protocol::{ClientDelivery, SessionId, SessionOrigin};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::ui::app::cron_panel::{CronAction, CronEditRequest, parse_form, sample_crons};
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

/// Handles RPC updates until `done` holds.
async fn updates_until(app: &mut App, done: impl Fn(&App) -> bool) {
    for _ in 0..50 {
        if done(app) {
            return;
        }
        let update =
            tokio::time::timeout(std::time::Duration::from_secs(5), app.rpc_updates_rx.recv())
                .await
                .expect("rpc update timeout")
                .expect("rpc update");
        app.handle_rpc_update(update);
    }
    panic!("condition never held; status: {}", app.status);
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

/// A daemon of client protocol `version` that lists `sample_crons`,
/// echoes what it is asked to create or change as a job, and runs a job
/// now into its session.
async fn crons_client(version: &str) -> (cookie_agent_protocol::Client, Arc<Mutex<Vec<Value>>>) {
    let version = version.to_owned();
    let (client, recorded) = answering_client(move |method, params| {
        let sample = serde_json::to_value(sample_crons()).unwrap();
        let first = sample["jobs"][0].clone();
        match method {
            "handshake" => Ok(json!({
                "protocol_version": version,
                "engine_version": "0.3.0",
                "origin": "client:tui-0192f3a45b6c7d8e9f00112233445566",
                "features": []
            })),
            "crons.list" => Ok(sample),
            "crons.create" => {
                let mut job = first;
                job["id"] = json!("n3wj0b00");
                job["prompt"] = params["prompt"].clone();
                job["schedule"] = params["schedule"].clone();
                job["target"] = params["target"].clone();
                job["history"] = json!([]);
                match params.get("name") {
                    Some(name) => job["name"] = name.clone(),
                    None => {
                        job.as_object_mut().unwrap().remove("name");
                    }
                }
                Ok(json!({ "job": job }))
            }
            "crons.update" => {
                let mut job = first;
                if let Some(enabled) = params.get("enabled") {
                    job["enabled"] = enabled.clone();
                }
                Ok(json!({ "job": job }))
            }
            "crons.run" => {
                let mut job = first;
                job["history"].as_array_mut().unwrap().push(json!({
                    "fired_at": "2026-10-23T12:00:00Z",
                    "run_now": true,
                    "outcome": {"kind": "delivered"},
                    "session_id": "0190a5b2-0000-7000-8000-000000000001"
                }));
                Ok(json!({ "job": job, "session_id": "0190a5b2-0000-7000-8000-000000000001" }))
            }
            "crons.delete" => Ok(json!({})),
            _ => Err(json!({"code": -32601, "message": "not scripted"})),
        }
    });
    client.handshake().await.expect("handshake");
    (client, recorded)
}

/// A test app viewing a session, talking to a daemon of client protocol
/// `version` (see `crons_client`).
async fn cron_app(version: &str) -> (App, SessionId, Arc<Mutex<Vec<Value>>>) {
    let mut app = test_app().await;
    let viewed: SessionId = "0190a5b2-0000-7000-8000-000000000001".parse().unwrap();
    app.sessions = vec![titled_meta(viewed, "Build fixes", 1)];
    app.selected = Some(viewed);
    app.tree_root = Some(viewed);
    let (client, recorded) = crons_client(version).await;
    app.client = client;
    (app, viewed, recorded)
}

#[tokio::test]
async fn the_panel_lists_jobs_with_their_reading_target_state_and_outcome() {
    let (mut app, _, recorded) = cron_app("1.15.0").await;
    app.run_command(SlashCommand::Cron).await;
    assert_eq!(app.modal, Modal::Cron);
    updates_until(&mut app, |app| {
        matches!(app.cron_panel.listing, Some(Ok(_)))
    })
    .await;
    assert_eq!(recorded_method_count(&recorded, "crons.list"), 1);
    let rows = frame_rows(&mut app, 160, 50).join("\n");
    for text in [
        "Cron jobs",
        "Nightly check  every day at 03:00  next in ",
        "→ this session  enabled  last completed",
        "Summarize yesterday's commits.  every Monday to Friday at 09:30, once  no next fire",
        "→ new session · builder  failed 3 times in a row",
        "schedule: 0 3 * * *",
        "reads: every day at 03:00",
        "target: this session",
        "delivery: queue",
        "Check for outdated dependencies.",
        "History, newest first",
        "delivered · completed",
        " edit (e) ",
        " disable (t) ",
        " run now (r) ",
        " delete (d) ",
    ] {
        assert!(rows.contains(text), "{text}:\n{rows}");
    }

    // The second job is disabled: it offers enabling, and its history has
    // the failure.
    app.handle_key(key(KeyCode::Down)).await;
    let rows = frame_rows(&mut app, 160, 50).join("\n");
    for text in [
        " enable (t) ",
        "failed: no model is available",
        "state: failed 3 times in a row",
        "agent: builder",
        "mode: auto-n",
    ] {
        assert!(rows.contains(text), "{text}:\n{rows}");
    }

    // A click selects a row; Esc closes the panel.
    let (rect, index) = app.hit_map.cron.rows[0];
    app.handle_click(rect.x, rect.y).await;
    assert_eq!(app.cron_panel.selection.selected(), Some(index));
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.modal, Modal::None);
}

#[tokio::test]
async fn a_daemon_before_minor_fifteen_is_never_asked_for_jobs() {
    let (mut app, _, recorded) = cron_app("1.14.0").await;
    app.run_command(SlashCommand::Cron).await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(
        app.status,
        "This daemon does not keep cron jobs; it needs client protocol 1.15."
    );
    let new = crate::ui::slash::COMMANDS
        .iter()
        .find(|spec| spec.name == "cron new")
        .expect("/cron new");
    assert!(!app.command_is_available(new));
    assert_eq!(recorded_method_count(&recorded, "crons.list"), 0);
}

#[tokio::test]
async fn cron_new_schedules_a_prompt_in_the_session_on_screen() {
    let (mut app, viewed, recorded) = cron_app("1.15.0").await;
    let new = crate::ui::slash::COMMANDS
        .iter()
        .find(|spec| spec.name == "cron new")
        .expect("/cron new");
    assert!(app.command_is_available(new));
    app.run_command(SlashCommand::CronNew).await;
    assert_eq!(app.modal, Modal::Cron);
    let request = app.pending_cron_editor.take().expect("a new job's form");
    assert_eq!(request.job, None);
    assert_eq!(request.session, Some((viewed, false)));
    for line in [
        "target: session",
        "delivery: queue",
        "agent: primary",
        "session fires into this session",
    ] {
        assert!(request.text.contains(line), "{line}:\n{}", request.text);
    }

    // An unchanged form creates nothing.
    app.finish_cron_edit(request.clone(), Ok(request.text.clone()));
    assert_eq!(app.status, "No changes; the job was not created.");

    // A form that cannot be read is offered again as it was left.
    let broken = with_field(&request.text, "delivery", "sometimes");
    app.finish_cron_edit(request.clone(), Ok(broken.clone()));
    assert_eq!(
        app.status,
        "Not saved: delivery is queue or steer, not `sometimes`"
    );
    rendered_frame(&mut app, 160, 50);
    assert_eq!(
        app.hit_map
            .cron
            .actions
            .iter()
            .map(|(_, action)| *action)
            .collect::<Vec<_>>(),
        [CronAction::EditAgain, CronAction::Cancel]
    );
    app.handle_key(key(KeyCode::Char('e'))).await;
    let again = app.pending_cron_editor.take().expect("the form again");
    assert_eq!(again.text, broken);

    let form = with_field(&request.text, "schedule", "in 20m");
    let form = with_field(&form, "name", "Check the build");
    let form = with_field(&form, "delivery", "steer");
    let form = with_prompt(&form, "Is the build green?");
    app.finish_cron_edit(again, Ok(form));
    updates_until(&mut app, |app| app.status.starts_with("Created")).await;
    assert!(
        app.status.starts_with("Created Check the build"),
        "{}",
        app.status
    );
    let params = last_request_params(&recorded, "crons.create");
    assert_eq!(
        params,
        json!({
            "name": "Check the build",
            "prompt": "Is the build green?",
            "schedule": {"in": "20m"},
            "target": {"session": viewed.to_string()},
            "delivery": "steer"
        })
    );
    assert_eq!(app.cron_panel.pending, None);
}

#[tokio::test]
async fn a_subagent_on_screen_is_targeted_as_a_subagent() {
    let (mut app, viewed, _) = cron_app("1.15.0").await;
    let root = SessionId::new_v7();
    app.sessions[0].origin = SessionOrigin::Delegated {
        root_session_id: root,
        parent_session_id: root,
        parent_run_id: run_id(),
        parent_tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
        invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
        depth: 1,
    };
    app.run_command(SlashCommand::CronNew).await;
    let request = app.pending_cron_editor.take().expect("a new job's form");
    assert_eq!(request.session, Some((viewed, true)));
    let draft = parse_form(
        &with_prompt(
            &with_field(&request.text, "schedule", "0 9 * * *"),
            "Report",
        ),
        &request,
    )
    .expect("the form");
    assert_eq!(
        draft.create_params().expect("params").target,
        cookie_agent_protocol::CronTarget::Subagent(viewed)
    );
}

#[tokio::test]
async fn a_new_session_job_takes_its_agent_model_pin_and_mode_from_the_form() {
    let (mut app, _, recorded) = cron_app("1.15.0").await;
    app.run_command(SlashCommand::CronNew).await;
    let request = app.pending_cron_editor.take().expect("a new job's form");
    let form = with_field(&request.text, "target", "new");
    let form = with_field(&form, "schedule", "0 3 * * *");
    let form = with_field(&form, "max_runs", "unlimited");
    let form = with_field(&form, "agent", "builder");
    let form = with_field(&form, "model", "deepseek/deepseek-flash[high]");
    let form = with_field(&form, "permission_mode", "ask");
    let form = with_prompt(&form, "Check for outdated dependencies.\nOpen a summary.");
    app.finish_cron_edit(request, Ok(form));
    updates_until(&mut app, |app| app.status.starts_with("Created")).await;
    let params = last_request_params(&recorded, "crons.create");
    assert_eq!(params["target"]["new"]["agent"], "builder");
    assert_eq!(
        params["target"]["new"]["model"],
        json!({"model": "deepseek/deepseek-flash", "variant": "high"})
    );
    assert_eq!(params["permission_mode"], "ask");
    assert_eq!(params["max_runs"], "unlimited");
    assert_eq!(params.get("delivery"), None);
    assert_eq!(
        params["prompt"],
        "Check for outdated dependencies.\nOpen a summary."
    );
}

#[tokio::test]
async fn a_form_names_only_the_fields_of_a_job() {
    let request = CronEditRequest {
        job: None,
        session: None,
        template: None,
        text: String::new(),
        original: String::new(),
    };
    let parse = |text: &str| parse_form(text, &request).map(|_| ());
    assert_eq!(
        parse("name: a\nschedule: in 5m\n"),
        Err("the form needs the --- line before the prompt".into())
    );
    assert_eq!(
        parse("colour: red\n---\nx\n"),
        Err("`colour` is not a field of a job".into())
    );
    assert_eq!(
        parse("name: a\nname: b\n---\nx\n"),
        Err("`name` is given twice".into())
    );
    assert_eq!(
        parse("target: session\n---\nx\n"),
        Err("target: session needs an open session; use target: new".into())
    );
    assert_eq!(
        parse("target: new\n---\nx\n"),
        Err("a new-session job needs an agent".into())
    );
    assert_eq!(
        parse("target: new\nagent: builder\npermission_mode: sometimes\n---\nx\n"),
        Err("`sometimes` is not a permission mode".into())
    );
    // A `---` in the prompt stays in it.
    let draft = parse_form(
        "# comment\ntarget: new\nagent: builder\nschedule: at 2026-10-23T03:00:00Z\n---\nabove\n---\nbelow\n",
        &request,
    )
    .expect("the form");
    assert_eq!(draft.prompt, "above\n---\nbelow\n");
    assert_eq!(draft.schedule, "at 2026-10-23T03:00:00Z");
}

#[tokio::test]
async fn jobs_are_edited_toggled_run_now_and_deleted_by_key_and_button() {
    let (mut app, _, recorded) = cron_app("1.15.0").await;
    app.run_command(SlashCommand::Cron).await;
    updates_until(&mut app, |app| {
        matches!(app.cron_panel.listing, Some(Ok(_)))
    })
    .await;

    // Enter opens the selected job's form; an edit sends what changed.
    app.handle_key(key(KeyCode::Enter)).await;
    let request = app.pending_cron_editor.take().expect("the job's form");
    assert!(
        request.text.contains("name: Nightly check"),
        "{}",
        request.text
    );
    assert!(
        request.text.contains("schedule: 0 3 * * *"),
        "{}",
        request.text
    );
    assert!(
        request.text.contains("max_runs: unlimited"),
        "{}",
        request.text
    );
    let edited = with_field(&request.text, "max_runs", "5");
    app.finish_cron_edit(request.clone(), Ok(edited));
    updates_until(&mut app, |app| app.status.starts_with("Saved")).await;
    assert_eq!(
        last_request_params(&recorded, "crons.update"),
        json!({"id": "k3v9q2xa", "max_runs": 5})
    );
    // A form whose text changed but whose job did not saves nothing.
    let reordered = request.text.replace("# A cron job.", "# A cron job!");
    app.finish_cron_edit(request, Ok(reordered));
    assert_eq!(app.status, "No changes to Nightly check.");

    // `t` disables the enabled job.
    updates_until(&mut app, |app| !app.cron_panel.refresh.in_flight()).await;
    app.handle_key(key(KeyCode::Char('t'))).await;
    updates_until(&mut app, |app| app.status.starts_with("Disabled")).await;
    assert_eq!(app.status, "Disabled Nightly check.");
    assert_eq!(
        last_request_params(&recorded, "crons.update"),
        json!({"id": "k3v9q2xa", "enabled": false})
    );

    // The run-now button fires it.
    updates_until(&mut app, |app| !app.cron_panel.refresh.in_flight()).await;
    rendered_frame(&mut app, 160, 50);
    let (button, _) = app
        .hit_map
        .cron
        .actions
        .iter()
        .copied()
        .find(|(_, action)| *action == CronAction::RunNow)
        .expect("the run-now button");
    assert_eq!(
        app.hover_target_at(button.x, button.y),
        Some(PointerTarget::CronAction(CronAction::RunNow))
    );
    app.handle_click(button.x, button.y).await;
    updates_until(&mut app, |app| app.status.starts_with("Ran")).await;
    assert_eq!(app.status, "Ran Nightly check now: run now · delivered.");
    assert_eq!(
        last_request_params(&recorded, "crons.run"),
        json!({"id": "k3v9q2xa"})
    );

    // `d` asks first; Esc keeps the job, and the button deletes it.
    updates_until(&mut app, |app| !app.cron_panel.refresh.in_flight()).await;
    app.handle_key(key(KeyCode::Char('d'))).await;
    assert!(
        frame_rows(&mut app, 160, 50)
            .join("\n")
            .contains("Delete Nightly check? Its run sessions stay.")
    );
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.status, "Delete cancelled.");
    assert_eq!(app.modal, Modal::Cron);
    app.handle_key(key(KeyCode::Char('d'))).await;
    rendered_frame(&mut app, 160, 50);
    let (button, _) = app
        .hit_map
        .cron
        .actions
        .iter()
        .copied()
        .find(|(_, action)| *action == CronAction::ConfirmDelete)
        .expect("the delete button");
    app.handle_click(button.x, button.y).await;
    updates_until(&mut app, |app| app.status.starts_with("Deleted")).await;
    assert_eq!(app.status, "Deleted Nightly check.");
    assert_eq!(
        last_request_params(&recorded, "crons.delete"),
        json!({"id": "k3v9q2xa"})
    );
}

#[tokio::test]
async fn a_cron_run_session_shows_a_clock_in_the_session_list() {
    use jiff::tz::TimeZone;

    let now: jiff::Timestamp = "2026-10-23T12:00:00Z".parse().unwrap();
    let mut run = titled_meta(SessionId::new_v7(), "Nightly check · 03:00", 1);
    run.last_activity = now;
    run.cron = Some(cookie_agent_protocol::SessionCron {
        job_id: cookie_agent_protocol::CronJobId::new("k3v9q2xa").unwrap(),
        fired_at: now,
    });
    let mut plain = titled_meta(SessionId::new_v7(), "Build fixes", 1);
    plain.last_activity = now;
    let rows =
        crate::ui::pickers::session_search_rows([&run, &plain], "", now, &TimeZone::UTC, None, &[]);
    let labels: Vec<String> = rows
        .iter()
        .filter_map(|row| match row {
            crate::ui::pickers::SessionSearchRow::Session { label, .. } => Some(label.clone()),
            crate::ui::pickers::SessionSearchRow::Header(_) => None,
        })
        .collect();
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("🕒 Nightly check · 03:00")),
        "{labels:?}"
    );
    assert!(
        labels.iter().any(|label| label.starts_with("Build fixes")),
        "{labels:?}"
    );
}

/// The real daemon: a job made in the panel is listed, refreshed on
/// `crons.changed`, toggled, and deleted.
#[tokio::test]
async fn jobs_round_trip_through_an_in_process_daemon() {
    let (_directory, server) = crate::tests::in_process_server();
    let client = Arc::clone(&server).connect_in_process();
    client.handshake().await.expect("handshake");
    let other = server.connect_in_process();
    other.handshake().await.expect("handshake");
    let mut app = App::new(client).await.expect("app");
    let mut deliveries = app.take_deliveries();
    app.run_command(SlashCommand::Cron).await;
    pump_until(&mut app, &mut deliveries, |app| {
        matches!(app.cron_panel.listing, Some(Ok(_)))
    })
    .await;
    assert!(app.cron_panel.jobs().is_empty());
    assert!(
        frame_rows(&mut app, 160, 50)
            .join("\n")
            .contains("No cron jobs yet.")
    );

    // With no session on screen a new job starts a session per fire.
    app.handle_key(key(KeyCode::Char('n'))).await;
    let request = app.pending_cron_editor.take().expect("a new job's form");
    assert!(request.text.contains("target: new"), "{}", request.text);
    let form = with_field(&request.text, "schedule", "0 3 * * *");
    let form = with_field(&form, "name", "Nightly");
    let form = with_field(&form, "agent", "primary");
    let form = with_prompt(&form, "Check dependencies.");
    app.finish_cron_edit(request, Ok(form));
    pump_until(&mut app, &mut deliveries, |app| {
        app.cron_panel.jobs().len() == 1 && app.status.starts_with("Created")
    })
    .await;
    assert_eq!(app.cron_panel.selection.selected(), Some(0));
    let id = app.cron_panel.jobs()[0].id.clone();

    // Another client's change arrives as crons.changed and refreshes.
    let mut disable = cookie_agent_protocol::CronUpdateParams::new(id.clone());
    disable.enabled = Some(false);
    other.update_cron(disable).await.expect("crons.update");
    pump_until(&mut app, &mut deliveries, |app| {
        app.cron_panel
            .jobs()
            .first()
            .is_some_and(|job| !job.enabled)
    })
    .await;
    assert!(
        frame_rows(&mut app, 160, 50)
            .join("\n")
            .contains("disabled by you")
    );

    app.handle_key(key(KeyCode::Char('t'))).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.cron_panel.jobs().first().is_some_and(|job| job.enabled)
    })
    .await;

    app.handle_key(key(KeyCode::Char('d'))).await;
    app.handle_key(key(KeyCode::Char('y'))).await;
    pump_until(&mut app, &mut deliveries, |app| {
        app.cron_panel.jobs().is_empty() && app.status.starts_with("Deleted")
    })
    .await;
    assert_eq!(app.status, "Deleted Nightly.");
}

/// A change announced while a listing is on its way may postdate what that
/// listing read: the panel lists again once it lands, so the change is not
/// lost behind it.
#[tokio::test]
async fn a_change_announced_during_a_listing_lists_again() {
    let (mut app, _, recorded) = cron_app("1.15.0").await;
    app.run_command(SlashCommand::Cron).await;
    assert!(app.cron_panel.refresh.in_flight());
    app.handle_delivery(ClientDelivery::CronsChanged(
        cookie_agent_protocol::CronsChangedNotification {},
    ))
    .await;
    updates_until(&mut app, |app| {
        matches!(app.cron_panel.listing, Some(Ok(_)))
    })
    .await;
    updates_until(&mut app, |_| {
        recorded_method_count(&recorded, "crons.list") == 2
    })
    .await;
    updates_until(&mut app, |app| !app.cron_panel.refresh.in_flight()).await;
    // Settled: nothing further is asked for.
    assert_eq!(recorded_method_count(&recorded, "crons.list"), 2);
}

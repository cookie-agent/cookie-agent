//! Reads that a change notice can overtake: a notice that lands while the
//! read it asks for is already on its way may postdate what that read saw,
//! so the read runs once more when it lands, and an answer older than a
//! value a notice carried is not installed over it.

use cookie_agent_protocol::{ClientDelivery, MessageFrame, RuntimeChangeReason};
use serde_json::{Value, json};

use crate::ui::app::*;

use super::support::*;

/// A daemon of client protocol 1.22 that answers `method` with `result`
/// and refuses anything else.
async fn daemon_answering(
    method: &'static str,
    result: Value,
) -> (
    cookie_agent_protocol::Client,
    std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
) {
    let (client, recorded) = answering_client(move |asked, _| match asked {
        "handshake" => Ok(json!({
            "protocol_version": "1.22.0",
            "engine_version": "0.4.0",
            "origin": "client:tui-0192f3a45b6c7d8e9f00112233445566",
            "features": []
        })),
        asked if asked == method => Ok(result.clone()),
        _ => Err(json!({"code": -32601, "message": "not scripted"})),
    });
    client.handshake().await.expect("handshake");
    (client, recorded)
}

/// Handles RPC updates until `method` was asked for `count` times and the
/// app is idle again, then checks nothing further was asked for.
async fn settles_after(
    app: &mut App,
    recorded: &std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    method: &str,
    count: usize,
    idle: impl Fn(&App) -> bool,
) {
    updates_until(app, |_| recorded_method_count(recorded, method) >= count).await;
    updates_until(app, idle).await;
    settle_recording().await;
    assert_eq!(recorded_method_count(recorded, method), count);
}

fn config_changed(app: &App) -> ClientDelivery {
    // The installed snapshot again: no resync, only the reason.
    let snapshot = app.runtime.snapshot().cloned().expect("installed snapshot");
    ClientDelivery::RuntimeChanged(Box::new(
        cookie_agent_protocol::RuntimeChangedNotification {
            previous_revision: None,
            snapshot,
            reasons: vec![RuntimeChangeReason::ConfigReloaded],
        },
    ))
}

#[tokio::test]
async fn a_configuration_change_during_the_status_read_reads_it_again() {
    let mut app = test_app().await;
    let status = super::pointer::restart_waiting_status();
    let (client, recorded) = daemon_answering(
        "config.get",
        json!({
            "revision": status.applied_revision,
            "documents": [],
            "status": status,
        }),
    )
    .await;
    app.client = client;
    app.poll_config_status();
    let changed = config_changed(&app);
    app.handle_delivery(changed).await;
    settles_after(&mut app, &recorded, "config.get", 2, |app| {
        app.restart.config_status.is_some()
    })
    .await;
}

/// A test app on a daemon of client protocol 1.22 that holds every request
/// until the test answers it.
async fn held_app() -> (
    App,
    std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    tokio::sync::mpsc::UnboundedSender<MessageFrame>,
) {
    let mut app = test_app().await;
    let (client, recorded, incoming) = live_recording_client();
    let handshake = tokio::spawn({
        let client = client.clone();
        async move { client.handshake().await.expect("handshake") }
    });
    let id = wait_for_recorded_request(&recorded, "handshake", 1).await;
    answer(
        &incoming,
        id,
        json!({
            "protocol_version": "1.22.0",
            "engine_version": "0.4.0",
            "origin": "client:tui-0192f3a45b6c7d8e9f00112233445566",
            "features": []
        }),
    );
    handshake.await.expect("handshake task");
    app.client = client;
    (app, recorded, incoming)
}

/// Answers the `method` requests as they come, the newest first, until no
/// more are asked for: the first was read before a change, the later ones
/// after it.
async fn answer_newest_first(
    app: &mut App,
    recorded: &std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    incoming: &tokio::sync::mpsc::UnboundedSender<MessageFrame>,
    method: &str,
    before: &Value,
    after: &Value,
) {
    let mut answered = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let ids = recorded
            .lock()
            .expect("recorded")
            .iter()
            .filter(|value| value["method"].as_str() == Some(method))
            .filter_map(|value| value["id"].as_i64())
            .collect::<Vec<_>>();
        if ids.len() == answered {
            return;
        }
        for (index, id) in ids.iter().enumerate().skip(answered).rev() {
            answer(incoming, *id, if index == 0 { before } else { after });
        }
        for _ in answered..ids.len() {
            let update =
                tokio::time::timeout(std::time::Duration::from_secs(5), app.rpc_updates_rx.recv())
                    .await
                    .expect("rpc update timeout")
                    .expect("rpc update");
            app.handle_rpc_update(update);
        }
        answered = ids.len();
    }
}

#[tokio::test]
async fn a_project_change_during_the_projects_read_reads_them_again() {
    let (mut app, recorded, incoming) = held_app().await;
    app.poll_projects();
    app.handle_delivery(ClientDelivery::ProjectsChanged(
        cookie_agent_protocol::ProjectsChangedNotification {},
    ))
    .await;
    let before = json!({"projects": [super::projects::project("api1", "api", None)]});
    let after = json!({"projects": [super::projects::project("api1", "backend", None)]});
    answer_newest_first(
        &mut app,
        &recorded,
        &incoming,
        "projects.list",
        &before,
        &after,
    )
    .await;
    assert_eq!(
        app.projects
            .iter()
            .map(|project| project.name.to_string())
            .collect::<Vec<_>>(),
        ["backend"]
    );
    assert!(!app.projects_refresh.in_flight());
}

#[tokio::test]
async fn a_runtime_change_during_the_homes_read_reads_them_again() {
    let (mut app, recorded, incoming) = held_app().await;
    app.poll_runtime_homes();
    app.handle_delivery(ClientDelivery::RuntimesChanged(
        cookie_agent_protocol::RuntimesChangedNotification {
            failures: Vec::new(),
        },
    ))
    .await;
    let listing = |home: &str| {
        json!({
            "runtimes": [{"runtime_id": "labpc", "lifetime": "dial_in", "connected": true,
                          "capabilities": [], "home": home}],
            "instances": []
        })
    };
    answer_newest_first(
        &mut app,
        &recorded,
        &incoming,
        "runtimes.list",
        &listing("/srv/old"),
        &listing("/srv/lab"),
    )
    .await;
    let labpc = cookie_agent_protocol::RuntimeId::new("labpc").unwrap();
    assert_eq!(app.runtime_homes.get(&labpc), Some("/srv/lab"));
    assert!(!app.runtime_homes_refresh.in_flight());
}

fn out_of_chain(previous: &str, revision: &str) -> ClientDelivery {
    ClientDelivery::RuntimeChanged(Box::new(
        cookie_agent_protocol::RuntimeChangedNotification {
            previous_revision: Some(
                runtime_snapshot(previous, Vec::new(), Vec::new(), Vec::new()).revision,
            ),
            snapshot: runtime_snapshot(revision, Vec::new(), Vec::new(), Vec::new()),
            reasons: vec![RuntimeChangeReason::ConfigReloaded],
        },
    ))
}

fn answer(
    incoming: &tokio::sync::mpsc::UnboundedSender<MessageFrame>,
    id: i64,
    result: impl serde::Serialize,
) {
    incoming
        .send(MessageFrame::Value(
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        ))
        .expect("answer");
}

/// A second out-of-chain notification while the refetch it would ask for
/// is on its way: that refetch may have read the runtime before the second
/// change, so the client fetches the snapshot once more.
#[tokio::test]
async fn a_runtime_notification_during_a_resync_resyncs_again() {
    let mut app = test_app().await;
    let (client, recorded, incoming) = live_recording_client();
    app.client = client;
    app.handle_delivery(out_of_chain("2", "3")).await;
    let first = wait_for_recorded_request(
        &recorded,
        cookie_agent_protocol::RUNTIME_SNAPSHOT_GET_METHOD,
        1,
    )
    .await;
    app.handle_delivery(out_of_chain("3", "4")).await;
    settle_recording().await;
    assert_eq!(
        recorded_method_count(
            &recorded,
            cookie_agent_protocol::RUNTIME_SNAPSHOT_GET_METHOD
        ),
        1
    );
    // The first refetch read the runtime before the second change.
    let read_before = runtime_snapshot("3", Vec::new(), vec![model_descriptor()], Vec::new());
    answer(
        &incoming,
        first,
        cookie_agent_protocol::RuntimeSnapshotResult {
            snapshot: read_before.clone(),
        },
    );
    updates_until(&mut app, |app| {
        app.runtime.revision() == Some(&read_before.revision)
    })
    .await;
    let second = wait_for_recorded_request(
        &recorded,
        cookie_agent_protocol::RUNTIME_SNAPSHOT_GET_METHOD,
        2,
    )
    .await;
    let current = runtime_snapshot("4", Vec::new(), vec![model_descriptor()], Vec::new());
    answer(
        &incoming,
        second,
        cookie_agent_protocol::RuntimeSnapshotResult {
            snapshot: current.clone(),
        },
    );
    updates_until(&mut app, |app| {
        app.runtime.revision() == Some(&current.revision) && !app.runtime_resync.in_flight()
    })
    .await;
}

/// `onboarding.changed` while `onboarding.get` is on its way: the answer
/// may have been read before the change, so it does not replace it.
#[tokio::test]
async fn an_onboarding_answer_older_than_its_change_is_dropped() {
    use crate::ui::app::onboarding::sample_status;
    use cookie_agent_protocol::OnboardingRequirement;

    let (mut app, recorded, incoming) = held_app().await;
    app.poll_onboarding();
    let asked = wait_for_recorded_request(&recorded, "onboarding.get", 1).await;
    // Another client finishes setting up the daemon.
    app.handle_delivery(ClientDelivery::OnboardingChanged(sample_status(&[])))
        .await;
    // The answer, read before that, says a model is missing.
    answer(
        &incoming,
        asked,
        sample_status(&[OnboardingRequirement::DefaultModel]),
    );
    let update = tokio::time::timeout(std::time::Duration::from_secs(5), app.rpc_updates_rx.recv())
        .await
        .expect("rpc update timeout")
        .expect("rpc update");
    app.handle_rpc_update(update);
    assert!(
        app.onboarding
            .status
            .as_ref()
            .is_some_and(|status| status.ready)
    );
    assert!(!app.onboarding_notice_visible());

    // A read asked after the change is installed.
    app.poll_onboarding();
    let asked = wait_for_recorded_request(&recorded, "onboarding.get", 2).await;
    answer(
        &incoming,
        asked,
        sample_status(&[OnboardingRequirement::SmallModel]),
    );
    updates_until(&mut app, |app| app.onboarding_notice_visible()).await;
}

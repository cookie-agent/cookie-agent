//! Each session is subscribed at most once per connection while its
//! subscription is healthy, counted on the wire against the real client,
//! server and engine: opening, switching away and back, and watching a tree
//! node again reuse the live subscription; a recovery and a new connection
//! are what replay a session again.

use std::sync::Arc;

use cookie_agent_protocol::{
    ClientDelivery, ClientRenameId, SessionCreateParams, SessionId, SessionRenameChange,
    SessionRenameParams, SessionTitle,
};
use tokio::sync::mpsc;

use super::live_stream::{attached_app, settle};
use crate::ui::App;

async fn create_session(server: &Arc<cookie_agent_server::Server>) -> SessionId {
    let client = Arc::clone(server).connect_in_process();
    client.handshake().await.expect("handshake");
    client
        .create_session(SessionCreateParams {
            selection: crate::tests::test_run_selection(),
        })
        .await
        .expect("create session")
        .session
        .session_id
}

/// Appends a durable event to `session` from another connection.
async fn rename_elsewhere(server: &Arc<cookie_agent_server::Server>, session: SessionId) {
    let client = Arc::clone(server).connect_in_process();
    client.handshake().await.expect("handshake");
    client
        .rename_session(SessionRenameParams {
            session_id: session,
            client_rename_id: ClientRenameId::new(format!("rename-{}", uuid::Uuid::now_v7()))
                .expect("rename ID"),
            change: SessionRenameChange::Set {
                title: SessionTitle::new("renamed elsewhere").expect("title"),
            },
        })
        .await
        .expect("rename session");
}

async fn open(
    app: &mut App,
    deliveries: &mut mpsc::UnboundedReceiver<ClientDelivery>,
    session: SessionId,
) {
    app.open_session(session).await;
    settle(app, deliveries).await;
    assert_eq!(app.selected, Some(session));
}

#[tokio::test]
async fn opening_and_switching_between_sessions_subscribes_each_once() {
    let (_directory, server) = crate::tests::in_process_server();
    let first = create_session(&server).await;
    let (mut app, mut deliveries, subscribes) = attached_app(&server).await;
    settle(&mut app, &mut deliveries).await;
    assert_eq!(app.selected, Some(first));
    assert_eq!(subscribes.count(first), 1, "startup attach");
    assert_eq!(app.subscription_state_for_test(first), "live");

    let second = create_session(&server).await;
    open(&mut app, &mut deliveries, second).await;
    assert_eq!(subscribes.count(second), 1, "opening another session");

    // Its tail keeps a session left in the background current, so coming
    // back only switches the view.
    let seen = app.store.sessions[&first].last_seq;
    rename_elsewhere(&server, first).await;
    settle(&mut app, &mut deliveries).await;
    assert!(app.store.sessions[&first].last_seq > seen);
    for _ in 0..2 {
        open(&mut app, &mut deliveries, first).await;
        open(&mut app, &mut deliveries, second).await;
    }
    assert_eq!(subscribes.count(first), 1, "switching back to the first");
    assert_eq!(subscribes.count(second), 1, "switching back to the second");
    assert_eq!(subscribes.total(), 2);
}

#[tokio::test]
async fn watching_a_tree_node_again_reuses_its_subscription() {
    let (_directory, server) = crate::tests::in_process_server();
    let session = create_session(&server).await;
    let (mut app, mut deliveries, subscribes) = attached_app(&server).await;
    settle(&mut app, &mut deliveries).await;
    app.refresh_tree().await;
    assert!(app.tree.is_some());
    for _ in 0..2 {
        app.watch_session(session);
        settle(&mut app, &mut deliveries).await;
        app.refresh_tree().await;
    }
    assert_eq!(app.selected, Some(session));
    assert_eq!(subscribes.count(session), 1);
}

#[tokio::test]
async fn a_recovery_replays_once_and_the_session_stays_live_after_it() {
    let (_directory, server) = crate::tests::in_process_server();
    let session = create_session(&server).await;
    let (mut app, mut deliveries, subscribes) = attached_app(&server).await;
    settle(&mut app, &mut deliveries).await;
    assert_eq!(subscribes.count(session), 1);

    app.recover_session(session);
    assert_eq!(app.subscription_state_for_test(session), "recovering");
    // Opening it while it recovers joins the recovery.
    open(&mut app, &mut deliveries, session).await;
    assert_eq!(subscribes.count(session), 2, "the recovery replays it");
    assert_eq!(app.subscription_state_for_test(session), "live");

    open(&mut app, &mut deliveries, session).await;
    assert_eq!(subscribes.count(session), 2, "reopening after it");
    // It still follows the tail.
    let seen = app.store.sessions[&session].last_seq;
    rename_elsewhere(&server, session).await;
    settle(&mut app, &mut deliveries).await;
    assert!(app.store.sessions[&session].last_seq > seen);
}

#[tokio::test]
async fn a_new_connection_subscribes_each_watched_session_once_again() {
    let (_directory, server) = crate::tests::in_process_server();
    let first = create_session(&server).await;
    let second = create_session(&server).await;
    let (client, subscribes, sever) = crate::tests::connect_counting(&server);
    client.handshake().await.expect("handshake");
    let mut app = App::new(client).await.expect("app");
    let mut deliveries = app.take_deliveries();
    settle(&mut app, &mut deliveries).await;
    let startup = app.selected.expect("startup session");
    let other = if startup == first { second } else { first };
    open(&mut app, &mut deliveries, other).await;
    open(&mut app, &mut deliveries, startup).await;
    assert_eq!(subscribes.count(first), 1);
    assert_eq!(subscribes.count(second), 1);

    sever.sever();
    settle(&mut app, &mut deliveries).await;
    assert!(app.status.contains("connection failed"), "{}", app.status);
    for session in [first, second] {
        assert_eq!(app.subscription_state_for_test(session), "idle");
    }

    // The view has no reconnect of its own; a replacement connection
    // stands in for one.
    let (client, resubscribes, _sever) = crate::tests::connect_counting(&server);
    client.handshake().await.expect("handshake");
    deliveries = client.subscribe_deliveries().expect("deliveries");
    app.client = client;
    for _ in 0..2 {
        open(&mut app, &mut deliveries, other).await;
        open(&mut app, &mut deliveries, startup).await;
    }
    assert_eq!(resubscribes.count(first), 1);
    assert_eq!(resubscribes.count(second), 1);
    assert_eq!(subscribes.total(), 2, "nothing more on the dead connection");
}

//! The browser WebSocket transport against a real `cookie daemon`, in a
//! headless browser. `scripts/test-wasm-client.sh` starts the daemons and
//! passes their endpoints in at compile time:
//!
//! - `COOKIE_WASM_TEST_URL` and `COOKIE_WASM_TEST_TOKEN`: a daemon, which
//!   takes the test page from its foreign origin like any other;
//! - `COOKIE_WASM_TEST_WORKSPACE`: a directory sessions there may use.
//!
//! Built without them, each test passes without connecting.

#![cfg(all(target_arch = "wasm32", feature = "websocket"))]

use std::time::Duration;

use cookie_agent_protocol::{
    ClientIdentity, ClientKind, ClientRenameId, HandshakeToken, ModelSelection, RunSelection,
    SessionCreateParams, SessionId, SessionRenameChange, SessionRenameParams, SessionTitle,
};
use cookie_agent_protocol_transport::{Client, ClientDelivery, ClientError};
use tokio::sync::mpsc::UnboundedReceiver;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

wasm_bindgen_test_configure!(run_in_browser);

const URL: Option<&str> = option_env!("COOKIE_WASM_TEST_URL");
const TOKEN: Option<&str> = option_env!("COOKIE_WASM_TEST_TOKEN");
const WORKSPACE: Option<&str> = option_env!("COOKIE_WASM_TEST_WORKSPACE");

/// How long one delivery may take to arrive.
const DELIVERY_WAIT: Duration = Duration::from_secs(10);

fn daemon() -> Option<(&'static str, &'static str)> {
    let daemon = URL.zip(TOKEN);
    if daemon.is_none() {
        log("no daemon given at compile time; skipping");
    }
    daemon
}

fn log(message: &str) {
    wasm_bindgen_test::console_log!("{message}");
}

fn identity() -> ClientIdentity {
    ClientIdentity::generate(ClientKind::new("web").expect("valid client kind"))
}

async fn connect(identity: ClientIdentity) -> Client {
    let (url, token) = daemon().expect("a daemon");
    let client = Client::connect_browser_websocket(url)
        .await
        .expect("connect");
    client
        .handshake_with_token(identity, HandshakeToken::new(token))
        .await
        .expect("handshake");
    client
}

fn selection() -> RunSelection {
    RunSelection {
        agent: "primary".parse().expect("agent"),
        model: ModelSelection {
            model: "custom.local/test".parse().expect("model"),
            variant: None,
        },
        preset: None,
    }
}

async fn create_session(client: &Client) -> SessionId {
    client
        .create_session(SessionCreateParams {
            cwd: WORKSPACE.map(str::to_owned),
            ..SessionCreateParams::new(selection())
        })
        .await
        .expect("create a session")
        .session
        .session_id
}

async fn next(deliveries: &mut UnboundedReceiver<ClientDelivery>) -> ClientDelivery {
    let wait = gloo_timers::future::sleep(DELIVERY_WAIT);
    let delivery = std::pin::pin!(deliveries.recv());
    match futures_util::future::select(delivery, wait).await {
        futures_util::future::Either::Left((Some(delivery), _)) => delivery,
        futures_util::future::Either::Left((None, _)) => panic!("the deliveries ended"),
        futures_util::future::Either::Right(_) => panic!("no delivery within {DELIVERY_WAIT:?}"),
    }
}

/// Replays `session_id` after `cursor` and returns the replayed events'
/// JSON and the replay's final sequence number.
async fn replay(
    client: &Client,
    deliveries: &mut UnboundedReceiver<ClientDelivery>,
    session_id: SessionId,
    cursor: Option<u64>,
) -> (Vec<serde_json::Value>, u64) {
    client
        .subscribe_events(session_id, cursor)
        .await
        .expect("subscribe");
    let mut events = Vec::new();
    loop {
        match next(deliveries).await {
            ClientDelivery::ReplayEvent { event, .. } if event.session_id == session_id => {
                events.push(serde_json::to_value(&*event).expect("event JSON"));
            }
            ClientDelivery::ReplayEnd {
                session_id: ended,
                final_seq,
                ..
            } if ended == session_id => return (events, final_seq),
            ClientDelivery::Disconnected { error } => panic!("disconnected: {error}"),
            ClientDelivery::RecoveryFailed { error, .. } => panic!("recovery failed: {error}"),
            _ => {}
        }
    }
}

#[wasm_bindgen_test]
async fn a_page_authenticates_with_the_token_in_its_handshake() {
    let Some((url, token)) = daemon() else {
        return;
    };
    let identity = identity();
    let client = Client::connect_browser_websocket(url)
        .await
        .expect("connect");
    let hello = client
        .handshake_with_token(identity.clone(), HandshakeToken::new(token))
        .await
        .expect("handshake");
    assert_eq!(hello.origin, identity.origin());
    assert_eq!(client.origin(), Some(identity.origin()));
    let listed = client
        .list_sessions(cookie_agent_protocol::SessionListParams::default())
        .await;
    assert!(listed.is_ok(), "{listed:?}");
    client.shutdown();
}

#[wasm_bindgen_test]
async fn a_wrong_token_is_refused_in_the_handshake() {
    let Some((url, _)) = daemon() else {
        return;
    };
    let client = Client::connect_browser_websocket(url)
        .await
        .expect("connect");
    let error = client
        .handshake_with_token(identity(), HandshakeToken::new("A".repeat(43)))
        .await
        .expect_err("a wrong token is refused");
    assert_eq!(error.error_code(), Some("unauthenticated"), "{error}");
}

#[wasm_bindgen_test]
async fn nothing_listening_is_refused_before_the_handshake() {
    if daemon().is_none() {
        return;
    }
    // Port 9 (discard) has no listener on a test machine.
    let error = Client::connect_browser_websocket("ws://127.0.0.1:9/ws")
        .await
        .err()
        .expect("nothing listens");
    assert!(
        matches!(error, ClientError::ConnectionRefusedBeforeHandshake { .. }),
        "{error:?}"
    );
}

#[wasm_bindgen_test]
async fn an_invalid_endpoint_is_refused_before_connecting() {
    let error = Client::connect_browser_websocket("ws://127.0.0.1:9/elsewhere")
        .await
        .err()
        .expect("not a daemon endpoint");
    assert!(matches!(error, ClientError::WebSocket(_)), "{error:?}");
}

#[wasm_bindgen_test]
async fn a_page_may_name_the_daemon_under_a_subpath() {
    let Some((url, _)) = daemon() else {
        return;
    };
    // The daemon serves `/ws` only, so the subpath reaches no endpoint; it
    // is still a URL a page may connect to, refused by the daemon and not
    // by the client.
    let subpath = format!("{}/cookie/ws", url.trim_end_matches("/ws"));
    let error = Client::connect_browser_websocket(&subpath)
        .await
        .err()
        .expect("no endpoint there");
    assert!(
        matches!(error, ClientError::ConnectionRefusedBeforeHandshake { .. }),
        "{error:?}"
    );
}

#[wasm_bindgen_test]
async fn a_reconnect_keeps_the_client_identity_and_origin() {
    if daemon().is_none() {
        return;
    }
    let identity = identity();
    let first = connect(identity.clone()).await;
    let origin = first.origin().expect("origin");
    first.shutdown();
    drop(first);
    let second = connect(identity.clone()).await;
    assert_eq!(second.origin(), Some(origin));
    assert_eq!(second.origin(), Some(identity.origin()));
    second.shutdown();
}

#[wasm_bindgen_test]
async fn a_reconnect_replays_what_happened_across_the_gap() {
    if daemon().is_none() {
        return;
    }
    let identity = identity();
    let first = connect(identity.clone()).await;
    let session_id = create_session(&first).await;
    let mut deliveries = first.subscribe_deliveries().expect("deliveries");
    let (events, cursor) = replay(&first, &mut deliveries, session_id, None).await;
    assert!(!events.is_empty(), "a new session has events");
    first.shutdown();
    drop(first);

    // While this page is away, another client renames the session.
    let other = connect(self::identity()).await;
    let title = "Renamed across the gap";
    other
        .rename_session(SessionRenameParams {
            session_id,
            client_rename_id: ClientRenameId::new("browser-test-rename").expect("rename id"),
            change: SessionRenameChange::Set {
                title: SessionTitle::new(title).expect("title"),
            },
        })
        .await
        .expect("rename");
    other.shutdown();

    let again = connect(identity).await;
    let mut deliveries = again.subscribe_deliveries().expect("deliveries");
    let (events, final_seq) = replay(&again, &mut deliveries, session_id, Some(cursor)).await;
    assert!(final_seq > cursor, "{final_seq} after {cursor}");
    assert!(
        events
            .iter()
            .all(|event| event["seq"].as_u64().is_some_and(|seq| seq > cursor)),
        "only events after the cursor: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            event["payload"]["type"] == "session_title_committed"
                && event["payload"]["change"]["title"] == title
        }),
        "the rename is replayed: {events:?}"
    );
    again.shutdown();
}

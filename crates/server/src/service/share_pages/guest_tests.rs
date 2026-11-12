//! Conversation shares through a running daemon: the guest page, and what a
//! guest connection reaches, on the main listener and the share-only one.

use std::{sync::Arc, time::Duration};

use cookie_agent_protocol::{
    RunCancelParams, SessionCreateParams, SessionGetParams, SessionGoalSetParams, SessionId,
    SessionListParams, SessionSelectModelParams, SessionShareCreateParams,
    SessionShareUpdateParams, ShareExpiry, ShareId, SharesListParams, guest_label,
};
use cookie_agent_protocol_transport::{Client, ClientDelivery, ClientError};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(10);

struct Daemon {
    harness: crate::tests::Harness,
    running: crate::RunningServer,
    owner: Client,
    root: SessionId,
    other: SessionId,
}

fn is_refused<T: std::fmt::Debug>(answer: Result<T, ClientError>) -> bool {
    match answer {
        Err(ClientError::Rpc(error)) => {
            error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(serde_json::Value::as_str)
                == Some("not_allowed_for_guest")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

async fn daemon() -> Daemon {
    let mut catalog = (*crate::tests::openai_catalog()).clone();
    Arc::make_mut(&mut catalog.providers)
        .values_mut()
        .next()
        .expect("OpenAI provider")
        .record
        .as_mut()
        .expect("OpenAI provider record")
        .api = Some("http://127.0.0.1:9/v1".into());
    let harness = crate::tests::harness_with_catalog(Arc::new(catalog), |_| {});
    let token = crate::generate_token().unwrap();
    let running = Arc::clone(&harness.server)
        .serve_on(
            std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            token.clone(),
            None,
        )
        .await
        .unwrap();
    let url = format!("ws://{}/ws", running.address());
    let owner = Client::connect_websocket_with_token(&url, &token)
        .await
        .unwrap();
    owner.handshake().await.unwrap();
    let revision = harness.engine.runtime_snapshot().unwrap().snapshot.revision;
    let _: serde_json::Value = owner
        .call(
            "provider.connect",
            &json!({
                "provider_id": "openai",
                "expected_revision": revision,
                "setup_values": {},
                "auth_method": "bearer-api-key-v1",
                "auth_values": { "api_key": "test-secret" },
                "client_connect_id": "guest-tests",
            }),
        )
        .await
        .unwrap();
    let create = || {
        serde_json::from_value::<SessionCreateParams>(json!({
            "selection": {
                "agent": "default",
                "model": { "model": "openai/gpt-5-mini", "variant": null },
            },
            "runtimes": [],
            "global": false,
        }))
        .unwrap()
    };
    let root = owner
        .create_session(create())
        .await
        .unwrap()
        .session
        .session_id;
    let other = owner
        .create_session(create())
        .await
        .unwrap()
        .session
        .session_id;
    Daemon {
        harness,
        running,
        owner,
        root,
        other,
    }
}

impl Daemon {
    async fn link(&self, can_send: bool, expiry: ShareExpiry) -> ShareId {
        self.owner
            .create_session_share(SessionShareCreateParams {
                session_id: self.root,
                label: Some("Alice".into()),
                can_send,
                expiry,
            })
            .await
            .unwrap()
            .share
            .id
    }

    async fn guest(&self, id: &ShareId) -> Client {
        guest_on(self.running.address().port(), id).await
    }
}

/// A connection to `/s/<id>/ws`, which presents no token.
async fn guest_on(port: u16, id: &ShareId) -> Client {
    Client::connect_guest_websocket(&format!("ws://127.0.0.1:{port}/s/{id}/ws"))
        .await
        .expect("a guest connects")
}

#[tokio::test]
async fn a_guest_reaches_its_session_and_nothing_else_of_the_daemon() {
    let daemon = daemon().await;
    let id = daemon.link(true, ShareExpiry::Never).await;
    let guest = daemon.guest(&id).await;
    let hello = guest.handshake().await.unwrap();

    let about = hello.guest.as_ref().expect("the hello says it is a guest");
    assert_eq!(about.session_id, daemon.root);
    assert_eq!(about.label, "Alice");
    assert!(about.can_send);
    assert_eq!(hello.page_ticket, None);
    assert_eq!(hello.page_url, None);
    assert_eq!(guest_label(&hello.origin).as_deref(), Some("Alice"));

    let session = guest
        .get_session(SessionGetParams {
            session_id: daemon.root,
        })
        .await
        .unwrap();
    assert_eq!(session.session.session_id, daemon.root);

    assert!(is_refused(
        guest
            .get_session(SessionGetParams {
                session_id: daemon.other,
            })
            .await
    ));
    // Its session's runtimes, and no other's.
    guest.list_session_runtimes(daemon.root).await.unwrap();
    assert!(is_refused(guest.list_session_runtimes(daemon.other).await));
    assert!(is_refused(guest.list_runtimes().await));
    assert!(is_refused(
        guest.list_sessions(SessionListParams::default()).await
    ));
    assert!(is_refused(guest.get_config().await));
    assert!(is_refused(
        guest.list_shares(SharesListParams::default()).await
    ));
    assert!(is_refused(
        guest
            .create_session_share(SessionShareCreateParams {
                session_id: daemon.root,
                label: None,
                can_send: true,
                expiry: ShareExpiry::Never,
            })
            .await
    ));
    // Its own session's setup is the owner's.
    assert!(is_refused(
        guest
            .select_model(SessionSelectModelParams {
                session_id: daemon.root,
                selection: serde_json::from_value(json!({
                    "agent": "default",
                    "model": { "model": "openai/gpt-5-mini", "variant": null },
                }))
                .unwrap(),
            })
            .await
    ));
}

#[tokio::test]
async fn a_link_set_to_watch_only_closes_its_guests_and_then_refuses_every_act() {
    let daemon = daemon().await;
    let id = daemon.link(true, ShareExpiry::Never).await;
    let guest = daemon.guest(&id).await;
    guest.handshake().await.unwrap();
    daemon
        .owner
        .update_session_share(SessionShareUpdateParams {
            id: id.clone(),
            label: None,
            can_send: Some(false),
            expiry: None,
        })
        .await
        .unwrap();
    closes(&guest).await;

    let guest = daemon.guest(&id).await;
    let hello = guest.handshake().await.unwrap();
    assert!(!hello.guest.unwrap().can_send);
    guest
        .get_session(SessionGetParams {
            session_id: daemon.root,
        })
        .await
        .expect("it still reads");
    assert!(is_refused(
        guest
            .compact_session(cookie_agent_protocol::SessionCompactParams {
                session_id: daemon.root,
                focus: None,
            })
            .await
    ));
    assert!(is_refused(
        guest
            .cancel_run(RunCancelParams {
                run_id: cookie_agent_protocol::RunId::new_v7(),
            })
            .await
    ));
}

#[tokio::test]
async fn a_guest_acts_in_the_session_but_cannot_choose_its_agent_or_model() {
    let daemon = daemon().await;
    let id = daemon.link(true, ShareExpiry::Never).await;
    let guest = daemon.guest(&id).await;
    guest.handshake().await.unwrap();
    let selection: cookie_agent_protocol::RunSelection = serde_json::from_value(json!({
        "agent": "default",
        "model": { "model": "openai/gpt-5-mini", "variant": null },
    }))
    .unwrap();

    guest
        .set_session_goal(SessionGoalSetParams {
            session_id: daemon.root,
            objective: "ship it".into(),
            selection: None,
        })
        .await
        .expect("a guest sets the goal");
    assert!(is_refused(
        guest
            .set_session_goal(SessionGoalSetParams {
                session_id: daemon.root,
                objective: "ship it another way".into(),
                selection: Some(selection.clone()),
            })
            .await
    ));
    assert!(is_refused(
        guest
            .set_session_goal(SessionGoalSetParams {
                session_id: daemon.other,
                objective: "in another session".into(),
                selection: None,
            })
            .await
    ));
    let start: Result<serde_json::Value, _> = guest
        .call(
            "run.start",
            &json!({
                "session_id": daemon.root,
                "client_run_id": "guest",
                "selection": selection,
                "content": "hi",
            }),
        )
        .await;
    assert!(is_refused(start));
}

#[tokio::test]
async fn a_guest_cannot_act_on_a_run_of_another_session() {
    let daemon = daemon().await;
    let id = daemon.link(true, ShareExpiry::Never).await;
    let guest = daemon.guest(&id).await;
    guest.handshake().await.unwrap();
    // No active run has this ID, so it belongs to no session the link names.
    assert!(is_refused(
        guest
            .cancel_run(RunCancelParams {
                run_id: cookie_agent_protocol::RunId::new_v7(),
            })
            .await
    ));
}

/// Waits until the connection reports it closed.
async fn closes(client: &Client) {
    let mut deliveries = client.subscribe_deliveries().expect("a delivery stream");
    tokio::time::timeout(WAIT, async {
        while let Some(delivery) = deliveries.recv().await {
            if matches!(delivery, ClientDelivery::Disconnected { .. }) {
                return;
            }
        }
    })
    .await
    .expect("the connection closes");
}

#[tokio::test]
async fn revoking_a_link_closes_its_guests_and_it_opens_no_more() {
    let daemon = daemon().await;
    let id = daemon.link(true, ShareExpiry::Never).await;
    let guest = daemon.guest(&id).await;
    guest.handshake().await.unwrap();
    let listed = daemon
        .owner
        .list_shares(SharesListParams::default())
        .await
        .unwrap();
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(listed.sessions[0].connected, 1);
    assert_eq!(listed.sessions[0].label, "Alice");

    daemon.owner.revoke_share(id.clone()).await.unwrap();
    closes(&guest).await;

    let port = daemon.running.address().port();
    let url = format!("ws://127.0.0.1:{port}/s/{id}/ws");
    assert!(Client::connect_guest_websocket(&url).await.is_err());
}

#[tokio::test]
async fn an_expired_link_closes_its_guests() {
    let daemon = daemon().await;
    let at = jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_secs(1))
        .unwrap();
    let id = daemon.link(true, ShareExpiry::At { at }).await;
    let guest = daemon.guest(&id).await;
    guest.handshake().await.unwrap();
    closes(&guest).await;
}

#[tokio::test]
async fn deleting_the_session_removes_its_link() {
    let daemon = daemon().await;
    daemon.link(true, ShareExpiry::Never).await;
    let meta = daemon.harness.engine.get_session(daemon.root).unwrap();
    daemon
        .owner
        .delete_session(cookie_agent_protocol::SessionDeleteParams {
            session_id: daemon.root,
            expected_last_seq: meta.last_event_seq,
        })
        .await
        .unwrap();
    let listed = daemon
        .owner
        .list_shares(SharesListParams::default())
        .await
        .unwrap();
    assert!(listed.sessions.is_empty());
}

#[tokio::test]
async fn a_label_is_checked() {
    let daemon = daemon().await;
    let refused = daemon
        .owner
        .create_session_share(SessionShareCreateParams {
            session_id: daemon.root,
            label: Some("two\nlines".into()),
            can_send: true,
            expiry: ShareExpiry::Never,
        })
        .await;
    assert!(matches!(refused, Err(ClientError::Rpc(_))));
}

async fn get(port: u16, path: &str) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head.to_ascii_lowercase(), body.to_owned())
}

#[tokio::test]
async fn the_guest_page_runs_in_a_sandbox_and_its_files_answer_any_origin() {
    let daemon = daemon().await;
    let id = daemon.link(true, ShareExpiry::Never).await;
    let port = daemon.running.address().port();

    let (status, head, _) = get(port, &format!("/s/{id}")).await;
    assert_eq!(status, 200);
    let policy = head
        .lines()
        .find_map(|line| line.strip_prefix("content-security-policy:"))
        .expect("a policy");
    assert!(policy.contains("sandbox allow-scripts"), "{policy}");
    assert!(!policy.contains("allow-same-origin"), "{policy}");
    assert!(head.contains("cache-control: no-store"), "{head}");

    let (status, head, _) = get(port, "/s/_app/viewer.html").await;
    if cookie_agent_web_assets::is_built() {
        assert_eq!(status, 200);
        assert!(head.contains("access-control-allow-origin: *"), "{head}");
    }
    // The page is only ever served in its sandbox.
    assert_eq!(get(port, "/s/_app/index.html").await.0, 404);
}

#[tokio::test]
async fn the_share_port_serves_guests_and_still_nothing_else() {
    let daemon = daemon().await;
    let id = daemon.link(true, ShareExpiry::Never).await;
    let listener = crate::DaemonListener::new(cookie_agent_engine::status::StatusBoard::new(
        cookie_agent_protocol::DaemonPhase::Starting,
    ));
    listener.hand_over(Arc::clone(&daemon.harness.server));
    let share_port = listener
        .serve_public_on(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let port = share_port.address().port();

    let guest = guest_on(port, &id).await;
    let hello = guest.handshake().await.unwrap();
    assert_eq!(hello.guest.unwrap().session_id, daemon.root);
    for path in ["/", "/ws", "/runtime", "/plugins/x/"] {
        assert_eq!(get(port, path).await.0, 404, "{path}");
    }
}

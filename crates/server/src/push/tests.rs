//! The push service against a local push service that records what it got
//! and answers with scripted statuses (push notifications spec section
//! 13.1).

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use aws_lc_rs::{
    aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    agreement::{self, ECDH_P256, PrivateKey, UnparsedPublicKey},
    hkdf::{HKDF_SHA256, KeyType, Salt},
};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cookie_agent_engine::{
    NeedsYouItem, NeedsYouSignal, PushSignal, SettledOutcome, SettledSignal,
};
use cookie_agent_protocol::{NotificationsSubscribeParams, NotificationsTestStatus, SessionId};
use jiff::Timestamp;

use super::{PushError, PushService};

/// What the local push service received, and what it answers next.
#[derive(Default)]
struct Recorded {
    requests: Vec<(HeaderMap, Vec<u8>)>,
    /// Statuses to answer with, each with an optional `Retry-After`; 201
    /// once they run out.
    script: VecDeque<(u16, Option<&'static str>)>,
}

type Shared = Arc<Mutex<Recorded>>;

async fn receive(
    State(shared): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, HeaderMap, &'static str) {
    let mut recorded = shared.lock().unwrap();
    recorded.requests.push((headers, body.to_vec()));
    let (status, retry_after) = recorded.script.pop_front().unwrap_or((201, None));
    let mut headers = HeaderMap::new();
    if let Some(retry_after) = retry_after {
        headers.insert("Retry-After", retry_after.parse().unwrap());
    }
    (
        StatusCode::from_u16(status).unwrap(),
        headers,
        "push service says no",
    )
}

/// A local push service; its endpoint for `name` is `<base>/push/<name>`.
async fn push_service() -> (String, Shared) {
    let shared = Shared::default();
    let router = Router::new()
        .route("/push/{name}", post(receive))
        .with_state(Arc::clone(&shared));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}"), shared)
}

/// A browser's subscription keys: its private key and `auth` secret.
struct Browser {
    private: PrivateKey,
    auth: [u8; 16],
}

impl Browser {
    fn new() -> Self {
        Self {
            private: PrivateKey::generate(&ECDH_P256).unwrap(),
            auth: [9; 16],
        }
    }

    fn params(&self, endpoint: &str, label: &str) -> NotificationsSubscribeParams {
        NotificationsSubscribeParams {
            endpoint: endpoint.to_owned(),
            p256dh: URL_SAFE_NO_PAD.encode(self.private.compute_public_key().unwrap().as_ref()),
            auth: URL_SAFE_NO_PAD.encode(self.auth),
            label: label.to_owned(),
        }
    }

    /// Decrypts a message as the browser does (RFC 8291 section 3.4).
    fn decrypt(&self, body: &[u8]) -> serde_json::Value {
        self.decrypt_maybe(body)
            .expect("a message for this browser")
    }

    /// [`Self::decrypt`], `None` when the message is another browser's.
    fn decrypt_maybe(&self, body: &[u8]) -> Option<serde_json::Value> {
        struct Length(usize);
        impl KeyType for Length {
            fn len(&self) -> usize {
                self.0
            }
        }
        fn hkdf(salt: &[u8], ikm: &[u8], info: &[&[u8]], length: usize) -> Vec<u8> {
            let mut out = vec![0; length];
            Salt::new(HKDF_SHA256, salt)
                .extract(ikm)
                .expand(info, Length(length))
                .unwrap()
                .fill(&mut out)
                .unwrap();
            out
        }
        let salt = &body[..16];
        let id_length = usize::from(body[20]);
        let as_public = &body[21..21 + id_length];
        let ua_public = self.private.compute_public_key().unwrap();
        let ecdh = agreement::agree(
            &self.private,
            UnparsedPublicKey::new(&ECDH_P256, as_public),
            (),
            |secret| Ok(secret.to_vec()),
        )
        .unwrap();
        let ikm = hkdf(
            &self.auth,
            &ecdh,
            &[b"WebPush: info\0", ua_public.as_ref(), as_public],
            32,
        );
        let cek = hkdf(salt, &ikm, &[b"Content-Encoding: aes128gcm\0"], 16);
        let nonce = hkdf(salt, &ikm, &[b"Content-Encoding: nonce\0"], 12);
        let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).unwrap());
        let mut record = body[21 + id_length..].to_vec();
        let plaintext = key
            .open_in_place(
                Nonce::try_assume_unique_for_key(&nonce).unwrap(),
                Aad::empty(),
                &mut record,
            )
            .ok()?;
        assert_eq!(plaintext.last(), Some(&2), "the last record's delimiter");
        Some(serde_json::from_slice(&plaintext[..plaintext.len() - 1]).unwrap())
    }
}

struct Fixture {
    harness: crate::tests::Harness,
    service: Arc<PushService>,
    base: String,
    recorded: Shared,
}

async fn fixture() -> Fixture {
    let harness = crate::tests::harness();
    let service = PushService::with_options(harness.engine.clone(), true);
    let (base, recorded) = push_service().await;
    Fixture {
        harness,
        service,
        base,
        recorded,
    }
}

impl Fixture {
    fn requests(&self) -> usize {
        self.recorded.lock().unwrap().requests.len()
    }

    fn script(&self, statuses: &[(u16, Option<&'static str>)]) {
        self.recorded
            .lock()
            .unwrap()
            .script
            .extend(statuses.iter().copied());
    }

    async fn wait_for_requests(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.requests() < count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the push service got the requests");
    }

    async fn shutdown(self) {
        self.harness.server.shutdown();
        self.harness.engine.shutdown().await;
    }
}

fn settled(root: SessionId) -> PushSignal {
    PushSignal::Settled(SettledSignal {
        root,
        outcome: SettledOutcome::Completed {
            final_text: Some("**Done**: the `cron` test waits on its fire.".into()),
        },
        title: Some("Fix the flaky test".into()),
        short_id: Some("ab12".into()),
        cron: false,
        at: Timestamp::now(),
    })
}

fn approval(root: SessionId, tool: &str) -> PushSignal {
    PushSignal::NeedsYou(NeedsYouSignal {
        root,
        session: root,
        handle: None,
        root_title: Some("Release".into()),
        root_short_id: None,
        item: NeedsYouItem::Approval {
            tool: tool.into(),
            summary: Some("cargo test -p engine".into()),
            expires_at: None,
        },
        at: Timestamp::now(),
    })
}

#[tokio::test]
async fn subscriptions_are_checked_kept_by_endpoint_and_removed() {
    let fixture = fixture().await;
    let browser = Browser::new();
    let first = fixture.service.get().expect("get");
    assert_eq!(URL_SAFE_NO_PAD.decode(&first.public_key).unwrap().len(), 65);
    assert!(first.subscriptions.is_empty());

    let endpoint = format!("{}/push/phone", fixture.base);
    let subscribed = fixture
        .service
        .subscribe(browser.params(&endpoint, " Phone "))
        .expect("subscribes");
    assert_eq!(subscribed.label, "Phone");
    assert_eq!(subscribed.push_service, "127.0.0.1");
    // The same endpoint keeps its id and takes the new label and keys.
    let again = fixture
        .service
        .subscribe(Browser::new().params(&endpoint, "Pixel"))
        .expect("again");
    assert_eq!(again.id, subscribed.id);
    assert_eq!(again.label, "Pixel");
    assert_eq!(fixture.service.get().unwrap().subscriptions.len(), 1);
    assert_eq!(fixture.service.get().unwrap().public_key, first.public_key);

    for (params, field) in [
        (browser.params("ftp://push.example/x", "Phone"), "endpoint"),
        (browser.params("not a url", "Phone"), "endpoint"),
        (browser.params(&endpoint, ""), "label"),
        (browser.params(&endpoint, &"x".repeat(81)), "label"),
        (
            NotificationsSubscribeParams {
                p256dh: "AAAA".into(),
                ..browser.params(&endpoint, "Phone")
            },
            "p256dh",
        ),
        (
            NotificationsSubscribeParams {
                auth: URL_SAFE_NO_PAD.encode([1; 15]),
                ..browser.params(&endpoint, "Phone")
            },
            "auth",
        ),
    ] {
        match fixture.service.subscribe(params) {
            Err(PushError::Invalid { field: refused, .. }) => assert_eq!(refused, field),
            other => panic!("{field}: {other:?}"),
        }
    }
    assert!(
        fixture
            .service
            .subscribe(browser.params(&endpoint, &"x".repeat(80)))
            .is_ok()
    );

    fixture
        .service
        .unsubscribe(&subscribed.id)
        .expect("removes");
    assert!(fixture.service.get().unwrap().subscriptions.is_empty());
    assert!(matches!(
        fixture.service.unsubscribe(&subscribed.id),
        Err(PushError::NotFound(_))
    ));
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_production_service_takes_only_https_endpoints() {
    let harness = crate::tests::harness();
    let service = PushService::new(harness.engine.clone());
    let browser = Browser::new();
    assert!(matches!(
        service.subscribe(browser.params("http://push.example/x", "Phone")),
        Err(PushError::Invalid {
            field: "endpoint",
            ..
        })
    ));
    assert!(
        service
            .subscribe(browser.params("https://push.example/x", "Phone"))
            .is_ok()
    );
    harness.server.shutdown();
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn a_push_is_encrypted_signed_and_carries_the_headers() {
    let fixture = fixture().await;
    let browser = Browser::new();
    let endpoint = format!("{}/push/phone", fixture.base);
    fixture
        .service
        .subscribe(browser.params(&endpoint, "Phone"))
        .unwrap();
    let root = SessionId::new_v7();
    fixture.service.signal(settled(root));
    fixture.wait_for_requests(1).await;

    let (headers, body) = fixture.recorded.lock().unwrap().requests[0].clone();
    assert_eq!(headers["content-encoding"], "aes128gcm");
    assert_eq!(headers["ttl"], "86400");
    assert_eq!(headers["urgency"], "normal");
    assert_eq!(headers["topic"], super::payload::topic(root).as_str());
    let authorization = headers["authorization"].to_str().unwrap();
    let public_key = fixture.service.get().unwrap().public_key;
    assert!(authorization.starts_with("vapid t="), "{authorization}");
    assert!(
        authorization.ends_with(&format!(", k={public_key}")),
        "{authorization}"
    );
    let token = &authorization["vapid t=".len()..authorization.find(", k=").unwrap()];
    let claims: serde_json::Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(token.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(claims["aud"], fixture.base);
    assert_eq!(claims["sub"], env!("CARGO_PKG_REPOSITORY"));

    let payload = browser.decrypt(&body);
    assert_eq!(payload["kind"], "settled");
    assert_eq!(payload["session"], root.to_string());
    assert_eq!(payload["title"], "Fix the flaky test");
    assert_eq!(payload["body"], "Done: the cron test waits on its fire.");
    assert_eq!(payload["outcome"], "completed");
    fixture.shutdown().await;
}

#[tokio::test]
async fn deliveries_are_recorded_retried_and_gone_subscriptions_dropped() {
    let fixture = fixture().await;
    let browser = Browser::new();
    let id = fixture
        .service
        .subscribe(browser.params(&format!("{}/push/phone", fixture.base), "Phone"))
        .unwrap()
        .id;
    let mut changes = fixture.service.subscribe_changes();
    changes.mark_unchanged();

    let sent = fixture.service.test(&id).await.unwrap();
    assert_eq!(sent.status, NotificationsTestStatus::Sent);
    assert!(
        fixture.service.get().unwrap().subscriptions[0]
            .last_sent_at
            .is_some()
    );
    assert_eq!(
        browser.decrypt(&fixture.recorded.lock().unwrap().requests[0].1)["kind"],
        "test"
    );

    // Slowed down, then a server error: retried until it goes through.
    fixture.script(&[(429, Some("0")), (503, Some("0"))]);
    let retried = fixture.service.test(&id).await.unwrap();
    assert_eq!(retried.status, NotificationsTestStatus::Sent);
    assert_eq!(fixture.requests(), 4);

    // Refused: recorded, not retried, kept.
    fixture.script(&[(400, None)]);
    let refused = fixture.service.test(&id).await.unwrap();
    assert_eq!(refused.status, NotificationsTestStatus::Failed);
    assert!(refused.detail.as_deref().unwrap().starts_with("400"));
    assert_eq!(fixture.requests(), 5);
    let shown = &fixture.service.get().unwrap().subscriptions[0];
    assert!(
        shown
            .last_error
            .as_deref()
            .unwrap()
            .contains("push service says no")
    );

    // Gone: removed and announced.
    fixture.script(&[(410, None)]);
    let gone = fixture.service.test(&id).await.unwrap();
    assert_eq!(gone.status, NotificationsTestStatus::Gone);
    assert!(fixture.service.get().unwrap().subscriptions.is_empty());
    assert!(changes.has_changed().unwrap());
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_new_key_drops_the_subscriptions_bound_to_the_old_one() {
    let fixture = fixture().await;
    let browser = Browser::new();
    fixture
        .service
        .subscribe(browser.params(&format!("{}/push/phone", fixture.base), "Phone"))
        .unwrap();
    let old = fixture.service.get().unwrap().public_key;
    std::fs::remove_file(
        fixture
            .harness
            .engine
            .data_root()
            .join(super::store::PUSH_DIR)
            .join("vapid.pk8"),
    )
    .unwrap();
    // A daemon that starts again reads the subscriptions but no key.
    let restarted = PushService::with_options(fixture.harness.engine.clone(), true);
    let fresh = restarted.get().unwrap();
    assert_ne!(fresh.public_key, old);
    assert!(fresh.subscriptions.is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_watched_tree_is_not_pushed_and_presence_ends_with_the_connection() {
    let fixture = fixture().await;
    let browser = Browser::new();
    fixture
        .service
        .subscribe(browser.params(&format!("{}/push/phone", fixture.base), "Phone"))
        .unwrap();
    let watched = SessionId::new_v7();
    let other = SessionId::new_v7();
    assert!(fixture.service.set_presence(7, Some(watched)));
    assert!(!fixture.service.set_presence(7, Some(watched)));
    fixture.service.signal(settled(watched));
    fixture.service.signal(settled(other));
    fixture.wait_for_requests(1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fixture.requests(), 1);
    let (_, body) = fixture.recorded.lock().unwrap().requests[0].clone();
    assert_eq!(browser.decrypt(&body)["session"], other.to_string());

    fixture.service.clear_presence(7);
    fixture.service.signal(settled(watched));
    fixture.wait_for_requests(2).await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn approvals_of_one_tree_gather_into_one_push() {
    let fixture = fixture().await;
    let browser = Browser::new();
    fixture
        .service
        .subscribe(browser.params(&format!("{}/push/phone", fixture.base), "Phone"))
        .unwrap();
    let root = SessionId::new_v7();
    fixture.service.signal(approval(root, "bash"));
    fixture.service.signal(approval(root, "write"));
    fixture.wait_for_requests(1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fixture.requests(), 1);
    let (headers, body) = fixture.recorded.lock().unwrap().requests[0].clone();
    assert_eq!(headers["urgency"], "high");
    let payload = browser.decrypt(&body);
    assert_eq!(payload["kind"], "approval");
    assert_eq!(payload["title"], "Approval · Release");
    assert_eq!(payload["body"], "2 waiting · bash: cargo test -p engine");

    // An approval about to expire is not pushed.
    let soon = Timestamp::now()
        .checked_add(Duration::from_secs(2))
        .unwrap();
    let PushSignal::NeedsYou(mut expiring) = approval(root, "bash") else {
        unreachable!()
    };
    expiring.item = NeedsYouItem::Approval {
        tool: "bash".into(),
        summary: None,
        expires_at: Some(soon),
    };
    fixture.service.signal(PushSignal::NeedsYou(expiring));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fixture.requests(), 1);
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_restart_is_announced_to_every_device_but_the_one_that_asked() {
    let fixture = fixture().await;
    let asker = Browser::new();
    let other = Browser::new();
    let asked = fixture
        .service
        .subscribe(asker.params(&format!("{}/push/asker", fixture.base), "Asker"))
        .unwrap();
    fixture
        .service
        .subscribe(other.params(&format!("{}/push/other", fixture.base), "Other"))
        .unwrap();
    assert!(fixture.service.note_registered(7, asked.id.clone()));
    fixture.service.note_restart_requested(7);
    fixture
        .service
        .announce_restarting(Duration::from_secs(10))
        .await;
    {
        let recorded = fixture.recorded.lock().unwrap();
        assert_eq!(recorded.requests.len(), 1, "only the other device");
        let (headers, body) = &recorded.requests[0];
        assert_eq!(headers["topic"], "daemon");
        let payload = other.decrypt(body);
        assert_eq!(payload["kind"], "daemon");
        assert_eq!(payload["tag"], "daemon");
        assert_eq!(payload["body"], "Restarting…");
        assert!(payload.get("session").is_none(), "{payload}");
    }

    // The daemon that comes back tells every device it restarted, once.
    let restarted = PushService::with_options(fixture.harness.engine.clone(), true);
    restarted.announce_started(Duration::from_secs(10)).await;
    fixture.wait_for_requests(3).await;
    let bodies = fixture.recorded.lock().unwrap().requests[1..]
        .iter()
        .map(|(_, body)| {
            let payload = other
                .decrypt_maybe(body)
                .or_else(|| asker.decrypt_maybe(body))
                .expect("for one of the devices");
            payload["body"].as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies
            .iter()
            .all(|body| body.starts_with("Restarted: cookie ")),
        "{bodies:?}"
    );
    // A start the daemon did not ask for says it started.
    restarted.announce_started(Duration::from_secs(10)).await;
    fixture.wait_for_requests(5).await;
    let (_, body) = fixture.recorded.lock().unwrap().requests[4].clone();
    let payload = other
        .decrypt_maybe(&body)
        .or_else(|| asker.decrypt_maybe(&body))
        .unwrap();
    assert!(
        payload["body"]
            .as_str()
            .unwrap()
            .starts_with("Started: cookie "),
        "{payload}"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_restart_asked_by_a_device_without_notifications_tells_them_all() {
    let fixture = fixture().await;
    let browser = Browser::new();
    fixture
        .service
        .subscribe(browser.params(&format!("{}/push/phone", fixture.base), "Phone"))
        .unwrap();
    // A TUI, say: its connection registered no device.
    fixture.service.note_restart_requested(9);
    fixture
        .service
        .announce_restarting(Duration::from_secs(10))
        .await;
    assert_eq!(fixture.requests(), 1);
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_signal_nobody_watched_marks_its_root_unread_and_presence_reads_it() {
    let fixture = fixture().await;
    let unread = || {
        std::fs::read_to_string(fixture.harness.engine.data_root().join("unread.json"))
            .unwrap_or_default()
    };
    let (watched, settled_away, asked_away) = (
        SessionId::new_v7(),
        SessionId::new_v7(),
        SessionId::new_v7(),
    );
    fixture.service.set_presence(7, Some(watched));
    fixture.service.signal(settled(watched));
    fixture.service.signal(settled(settled_away));
    fixture.service.signal(approval(asked_away, "bash"));
    let file = unread();
    assert!(!file.contains(&watched.to_string()), "{file}");
    assert!(file.contains(&settled_away.to_string()), "{file}");
    assert!(file.contains(&asked_away.to_string()), "{file}");
    // Presence on it reads it.
    fixture.service.set_presence(7, Some(settled_away));
    let file = unread();
    assert!(!file.contains(&settled_away.to_string()), "{file}");
    assert!(file.contains(&asked_away.to_string()), "{file}");
    fixture.shutdown().await;
}

/// A push service's error body is read only as far as its detail needs: one
/// that sends a body without end still yields a bounded detail at once.
#[tokio::test]
async fn an_error_body_is_read_only_up_to_its_limit() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        // Announces a gigabyte, sends 64 KiB, and then holds the connection.
        stream
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\ncontent-type: text/plain\r\ncontent-length: 1000000000\r\n\r\n",
            )
            .await
            .unwrap();
        stream.write_all(&vec![b'x'; 64 * 1024]).await.unwrap();
        std::future::pending::<()>().await;
    });
    let response = reqwest::Client::new()
        .get(format!("http://{address}/"))
        .send()
        .await
        .unwrap();
    let text = tokio::time::timeout(Duration::from_secs(5), super::error_text(response))
        .await
        .expect("the read stops at the limit");
    assert_eq!(text.len(), super::ERROR_BODY_LIMIT);
    server.abort();
}

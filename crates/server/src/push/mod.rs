//! Web Push notifications (`docs/site/specs/push-notifications.md`): the
//! daemon's VAPID identity, the browsers it pushes to, what each connection
//! shows the user, and the task that turns the engine's push signals into
//! encrypted messages for every subscription.

mod crypto;
mod payload;
mod store;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use cookie_agent_config::NotificationsConfig;
use cookie_agent_engine::{Engine, NeedsYouSignal, PushSignal};
use cookie_agent_protocol::{
    NotificationsGetResult, NotificationsTestResult, NotificationsTestStatus,
    PUSH_ENDPOINT_MAX_BYTES, PUSH_LABEL_MAX_CHARS, PushSubscription, SessionId,
};
use jiff::Timestamp;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use self::{
    crypto::{VapidKey, decode_key},
    payload::{DaemonEvent, Payload},
    store::{PushStore, StoredSubscription},
};

/// How long needs-you signals of one tree gather into one push.
const NEEDS_YOU_WINDOW: Duration = Duration::from_secs(1);
/// An approval with less time left than this is not pushed.
const MIN_APPROVAL_LEFT: Duration = Duration::from_secs(5);
/// How long a push service keeps a message for an offline device.
const DEFAULT_TTL_SECONDS: u64 = 86_400;
/// Retries of a send the push service asked to slow down or failed.
const RETRIES: u32 = 3;
/// How long one send keeps retrying.
const RETRY_BUDGET: Duration = Duration::from_secs(60);
/// The wait before retry `n` (from 1) without a `Retry-After`: 2^n s.
const RETRY_BACKOFF: cookie_agent_identity::Backoff =
    cookie_agent_identity::Backoff::new(Duration::from_secs(1), RETRY_BUDGET);
/// The VAPID token's lifetime; it is reused until an hour before it ends.
const TOKEN_LIFETIME_SECONDS: i64 = 12 * 3600;
const TOKEN_REFRESH_MARGIN_SECONDS: i64 = 3600;

/// Why a notifications request failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PushError {
    #[error("there is no push subscription {0}")]
    NotFound(String),
    #[error("{field}: {reason}")]
    Invalid { field: &'static str, reason: String },
    #[error("{0}")]
    Unavailable(String),
}

/// What one send came to.
#[derive(Debug, PartialEq)]
enum Delivery {
    Sent,
    Gone,
    Failed(String),
}

/// One message, encrypted per subscription as it is sent.
struct Message {
    payload: Vec<u8>,
    ttl: u64,
    urgency: &'static str,
    topic: Option<String>,
    /// Retries stop here: an approval's expiry.
    deadline: Option<Timestamp>,
}

struct Subscriptions {
    /// The stored subscriptions, or why they could not be read.
    list: Result<Vec<StoredSubscription>, String>,
}

/// The daemon's push notifications.
pub(crate) struct PushService {
    engine: Engine,
    store: PushStore,
    subscriptions: Mutex<Subscriptions>,
    /// Serializes the changes to the subscriptions and their writes, so the
    /// file and [`Self::subscriptions`] change in the same order while
    /// readers of the list never wait for a write.
    subscription_writes: Mutex<()>,
    key: Mutex<Option<Arc<VapidKey>>>,
    tokens: Mutex<HashMap<(String, String), (String, i64)>>,
    /// The session each connection shows the user, by connection id.
    presence: Mutex<HashMap<u64, SessionId>>,
    /// Bumped whenever the subscriptions change.
    changes: watch::Sender<u64>,
    /// Needs-you signals gathering per root, sent when their window ends.
    gathering: Mutex<HashMap<SessionId, Vec<NeedsYouSignal>>>,
    /// The subscription each connection registered, by connection id.
    registered: Mutex<HashMap<u64, String>>,
    /// The subscription of the device that asked for the restart under
    /// way: it is not told the daemon is restarting.
    restart_requester: Mutex<Option<String>>,
    http: reqwest::Client,
    /// Tests send to a local `http://` push service.
    allow_http: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs `work`, which reads or writes files, off the async worker when on
/// a multi-threaded runtime.
fn off_worker<T>(work: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|runtime| runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}

impl PushService {
    pub(crate) fn new(engine: Engine) -> Arc<Self> {
        Self::with_options(engine, false)
    }

    fn with_options(engine: Engine, allow_http: bool) -> Arc<Self> {
        let store = PushStore::new(engine.data_root());
        let list = store
            .read_subscriptions()
            .map_err(|error| error.to_string());
        if let Err(error) = &list {
            eprintln!("push notifications are unavailable: {error}");
        }
        Arc::new(Self {
            engine,
            store,
            subscriptions: Mutex::new(Subscriptions { list }),
            subscription_writes: Mutex::new(()),
            key: Mutex::new(None),
            tokens: Mutex::new(HashMap::new()),
            presence: Mutex::new(HashMap::new()),
            changes: watch::Sender::new(0),
            gathering: Mutex::new(HashMap::new()),
            registered: Mutex::new(HashMap::new()),
            restart_requester: Mutex::new(None),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            allow_http,
        })
    }

    fn config(&self) -> NotificationsConfig {
        self.engine.configuration().runtime.notifications.clone()
    }

    /// Follows the engine's push signals until `shutdown`.
    pub(crate) fn start(self: &Arc<Self>, shutdown: CancellationToken) {
        let mut signals = self.engine.subscribe_push_signals();
        let service = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let signal = tokio::select! {
                    () = shutdown.cancelled() => return,
                    signal = signals.recv() => signal,
                };
                match signal {
                    Ok(signal) => service.signal(signal),
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        eprintln!("push notifications: {missed} signals dropped");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    fn signal(self: &Arc<Self>, signal: PushSignal) {
        // A signal nobody present on the tree saw marks its root unread,
        // whatever `[notifications]` says of pushing it (unread sessions
        // spec section 2).
        let root = match &signal {
            PushSignal::Settled(settled) => settled.root,
            PushSignal::NeedsYou(needs) => needs.root,
        };
        if !self.watched(root) {
            self.engine.mark_unread(root);
        }
        let config = self.config();
        if !config.enabled {
            return;
        }
        match signal {
            PushSignal::Settled(settled) => {
                if settled.cron && !config.cron_sessions {
                    return;
                }
                if self.watched(settled.root) {
                    return;
                }
                let message = Message {
                    urgency: if settled.failed() { "high" } else { "normal" },
                    topic: Some(payload::topic(settled.root)),
                    payload: Payload::settled(&settled, config.preview).into_bytes(),
                    ttl: DEFAULT_TTL_SECONDS,
                    deadline: None,
                };
                self.send_all(message);
            }
            PushSignal::NeedsYou(needs) => {
                if !config.approvals {
                    return;
                }
                let root = needs.root;
                let first = {
                    let mut gathering = lock(&self.gathering);
                    let items = gathering.entry(root).or_default();
                    items.push(needs);
                    items.len() == 1
                };
                if first {
                    let service = Arc::clone(self);
                    tokio::spawn(async move {
                        tokio::time::sleep(NEEDS_YOU_WINDOW).await;
                        service.flush_needs_you(root);
                    });
                }
            }
        }
    }

    /// Sends what gathered for `root`: the items with time left, as one
    /// push.
    fn flush_needs_you(self: &Arc<Self>, root: SessionId) {
        let items = lock(&self.gathering).remove(&root).unwrap_or_default();
        let config = self.config();
        if !config.enabled || !config.approvals || self.watched(root) {
            return;
        }
        let now = Timestamp::now();
        let items = items
            .into_iter()
            .filter(|item| {
                item.expires_at().is_none_or(|expires| {
                    expires.duration_since(now).unsigned_abs() >= MIN_APPROVAL_LEFT && expires > now
                })
            })
            .collect::<Vec<_>>();
        let Some(first) = items.first() else {
            return;
        };
        let deadline = items.iter().filter_map(NeedsYouSignal::expires_at).min();
        let ttl = deadline.map_or(DEFAULT_TTL_SECONDS, |deadline| {
            u64::try_from(deadline.duration_since(now).as_secs())
                .unwrap_or(1)
                .max(1)
        });
        let message = Message {
            payload: Payload::needs_you(first, items.len(), config.preview).into_bytes(),
            ttl,
            urgency: "high",
            topic: Some(payload::topic(root)),
            deadline,
        };
        self.send_all(message);
    }

    /// Whether a connection shows the user a session of `root`'s tree.
    fn watched(&self, root: SessionId) -> bool {
        let shown = lock(&self.presence).values().copied().collect::<Vec<_>>();
        shown
            .into_iter()
            .any(|session| self.engine.root_session_of(session) == root)
    }

    /// Records what connection `connection` shows the user; `None` when the
    /// user is not there.
    pub(crate) fn set_presence(&self, connection: u64, session: Option<SessionId>) -> bool {
        let mut presence = lock(&self.presence);
        let first = !presence.contains_key(&connection);
        match session {
            Some(session) => {
                presence.insert(connection, session);
            }
            None => {
                presence.remove(&connection);
            }
        }
        drop(presence);
        // The user is there: the tree is read.
        if let Some(session) = session {
            self.engine.mark_tree_read(session);
        }
        first && session.is_some()
    }

    pub(crate) fn clear_presence(&self, connection: u64) {
        lock(&self.presence).remove(&connection);
    }

    /// Records that connection `connection` registered subscription `id`:
    /// its device. Returns whether the connection registered none before.
    pub(crate) fn note_registered(&self, connection: u64, id: String) -> bool {
        lock(&self.registered).insert(connection, id).is_none()
    }

    /// The connection ended.
    pub(crate) fn forget_connection(&self, connection: u64) {
        lock(&self.registered).remove(&connection);
    }

    /// Connection `connection` asked for a restart: its device, when it
    /// registered one, is not told the daemon is restarting.
    pub(crate) fn note_restart_requested(&self, connection: u64) {
        let device = lock(&self.registered).get(&connection).cloned();
        *lock(&self.restart_requester) = device;
    }

    /// The daemon goes down to restart: every device but the one that
    /// asked is told, and the daemon that comes back finds the mark
    /// ([`Self::announce_started`]). Waits at most `wait` for the sends.
    pub(crate) async fn announce_restarting(&self, wait: Duration) {
        if let Err(error) = self.store.mark_restarting() {
            eprintln!("push notifications: could not mark the restart: {error}");
        }
        let requester = lock(&self.restart_requester).take();
        if !self.config().enabled {
            return;
        }
        let message = daemon_message(DaemonEvent::Restarting, 600);
        self.send_and_wait(&message, requester.as_deref(), wait)
            .await;
    }

    /// The persistent daemon is listening: every device is told it
    /// restarted, or started when the daemon before it did not go down to
    /// restart (a service manager's restart, a boot). Waits at most `wait`.
    pub(crate) async fn announce_started(&self, wait: Duration) {
        let restarted = self.store.take_restarting();
        if !self.config().enabled {
            return;
        }
        let event = if restarted {
            DaemonEvent::Restarted
        } else {
            DaemonEvent::Started
        };
        let message = daemon_message(event, 3600);
        self.send_and_wait(&message, None, wait).await;
    }

    /// Sends `message` to every subscription but `except`, together, and
    /// waits at most `wait` for them.
    async fn send_and_wait(&self, message: &Message, except: Option<&str>, wait: Duration) {
        let Ok(subscriptions) = self.read_subscriptions(|list| Ok(list.to_vec())) else {
            return;
        };
        let sends = subscriptions
            .into_iter()
            .filter(|subscription| Some(subscription.id.as_str()) != except)
            .map(|subscription| async move {
                let delivery = self.deliver(&subscription, message).await;
                self.record(&subscription.id, &delivery);
            });
        let _ = tokio::time::timeout(wait, futures_util::future::join_all(sends)).await;
    }

    pub(crate) fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    /// The daemon's VAPID key, generated and stored on first use. A new key
    /// drops every subscription: they were bound to the one before.
    fn key(&self) -> Result<Arc<VapidKey>, PushError> {
        if let Some(key) = lock(&self.key).as_ref() {
            return Ok(Arc::clone(key));
        }
        // First use reads, and may write, files: off the worker, so the
        // threads that wait for the key do not hold one up either.
        off_worker(|| self.load_key())
    }

    fn load_key(&self) -> Result<Arc<VapidKey>, PushError> {
        let mut slot = lock(&self.key);
        if let Some(key) = slot.as_ref() {
            return Ok(Arc::clone(key));
        }
        let unavailable = |error: std::io::Error| PushError::Unavailable(error.to_string());
        let key = match self.store.read_key().map_err(unavailable)? {
            Some(pkcs8) => VapidKey::from_pkcs8(&pkcs8)
                .map_err(|error| PushError::Unavailable(error.to_string()))?,
            None => {
                let (key, pkcs8) = VapidKey::generate()
                    .map_err(|error| PushError::Unavailable(error.to_string()))?;
                self.store.write_key(&pkcs8).map_err(unavailable)?;
                self.change_subscriptions(|list| {
                    let dropped = !list.is_empty();
                    list.clear();
                    Ok(((), dropped))
                })?;
                key
            }
        };
        let key = Arc::new(key);
        *slot = Some(Arc::clone(&key));
        Ok(key)
    }

    /// What `read` makes of the subscriptions as they are now.
    fn read_subscriptions<T>(
        &self,
        read: impl FnOnce(&[StoredSubscription]) -> Result<T, PushError>,
    ) -> Result<T, PushError> {
        let subscriptions = lock(&self.subscriptions);
        let list = subscriptions
            .list
            .as_ref()
            .map_err(|error| PushError::Unavailable(error.clone()))?;
        read(list)
    }

    /// Applies `change` to the subscriptions, and writes them when it says
    /// to. The write happens off the worker and without the subscriptions'
    /// lock: readers keep the list as it was until the file holds the new
    /// one.
    fn change_subscriptions<T>(
        &self,
        change: impl FnOnce(&mut Vec<StoredSubscription>) -> Result<(T, bool), PushError>,
    ) -> Result<T, PushError> {
        let _writes = lock(&self.subscription_writes);
        let mut next = lock(&self.subscriptions)
            .list
            .as_ref()
            .map_err(|error| PushError::Unavailable(error.clone()))?
            .clone();
        let (value, write) = change(&mut next)?;
        if write {
            off_worker(|| self.store.write_subscriptions(&next))
                .map_err(|error| PushError::Unavailable(error.to_string()))?;
            lock(&self.subscriptions).list = Ok(next);
            self.changed();
        }
        Ok(value)
    }

    pub(crate) fn get(&self) -> Result<NotificationsGetResult, PushError> {
        let public_key = self.key()?.public_key();
        let subscriptions =
            self.read_subscriptions(|list| Ok(list.iter().map(visible).collect::<Vec<_>>()))?;
        Ok(NotificationsGetResult {
            public_key,
            enabled: self.config().enabled,
            subscriptions,
        })
    }

    pub(crate) fn subscribe(
        &self,
        params: cookie_agent_protocol::NotificationsSubscribeParams,
    ) -> Result<PushSubscription, PushError> {
        self.key()?;
        let endpoint = params.endpoint.trim().to_owned();
        let url = reqwest::Url::parse(&endpoint).map_err(|error| PushError::Invalid {
            field: "endpoint",
            reason: error.to_string(),
        })?;
        if url.scheme() != "https" && !(self.allow_http && url.scheme() == "http") {
            return Err(PushError::Invalid {
                field: "endpoint",
                reason: "must be an https:// URL".into(),
            });
        }
        if endpoint.len() > PUSH_ENDPOINT_MAX_BYTES || url.host_str().is_none() {
            return Err(PushError::Invalid {
                field: "endpoint",
                reason: format!(
                    "must be a URL with a host, at most {PUSH_ENDPOINT_MAX_BYTES} bytes"
                ),
            });
        }
        let p256dh = decode_key(&params.p256dh, "p256dh")
            .ok()
            .filter(|key| key.len() == 65 && key[0] == 4)
            .ok_or_else(|| PushError::Invalid {
                field: "p256dh",
                reason: "must be an uncompressed P-256 point in base64url".into(),
            })?;
        let auth = decode_key(&params.auth, "auth")
            .ok()
            .filter(|secret| secret.len() == 16)
            .ok_or_else(|| PushError::Invalid {
                field: "auth",
                reason: "must be a 16-byte secret in base64url".into(),
            })?;
        let label = params.label.trim().to_owned();
        let characters = label.chars().count();
        if characters == 0 || characters > PUSH_LABEL_MAX_CHARS {
            return Err(PushError::Invalid {
                field: "label",
                reason: format!("must be 1 to {PUSH_LABEL_MAX_CHARS} characters"),
            });
        }
        use base64::Engine as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        self.change_subscriptions(|list| {
            if let Some(existing) = list.iter_mut().find(|stored| stored.endpoint == endpoint) {
                existing.p256dh = encode(&p256dh);
                existing.auth = encode(&auth);
                existing.label = label;
                return Ok((visible(existing), true));
            }
            let stored = StoredSubscription {
                id: new_id(list),
                endpoint,
                p256dh: encode(&p256dh),
                auth: encode(&auth),
                label,
                created_at: Timestamp::now(),
                last_sent_at: None,
                last_error: None,
            };
            let shown = visible(&stored);
            list.push(stored);
            Ok((shown, true))
        })
    }

    pub(crate) fn unsubscribe(&self, id: &str) -> Result<(), PushError> {
        self.change_subscriptions(|list| {
            let before = list.len();
            list.retain(|stored| stored.id != id);
            if list.len() == before {
                return Err(PushError::NotFound(id.to_owned()));
            }
            Ok(((), true))
        })
    }

    pub(crate) async fn test(&self, id: &str) -> Result<NotificationsTestResult, PushError> {
        let subscription = self.read_subscriptions(|list| {
            list.iter()
                .find(|stored| stored.id == id)
                .cloned()
                .ok_or_else(|| PushError::NotFound(id.to_owned()))
        })?;
        let message = Message {
            payload: Payload::test().into_bytes(),
            ttl: 60,
            urgency: "normal",
            topic: None,
            deadline: None,
        };
        let delivery = self.deliver(&subscription, &message).await;
        self.record(&subscription.id, &delivery);
        Ok(match delivery {
            Delivery::Sent => NotificationsTestResult {
                status: NotificationsTestStatus::Sent,
                detail: None,
            },
            Delivery::Gone => NotificationsTestResult {
                status: NotificationsTestStatus::Gone,
                detail: None,
            },
            Delivery::Failed(detail) => NotificationsTestResult {
                status: NotificationsTestStatus::Failed,
                detail: Some(detail),
            },
        })
    }

    /// Sends `message` to every subscription, each on its own task.
    fn send_all(self: &Arc<Self>, message: Message) {
        let Ok(subscriptions) = self.read_subscriptions(|list| Ok(list.to_vec())) else {
            return;
        };
        let message = Arc::new(message);
        for subscription in subscriptions {
            let service = Arc::clone(self);
            let message = Arc::clone(&message);
            tokio::spawn(async move {
                let delivery = service.deliver(&subscription, &message).await;
                service.record(&subscription.id, &delivery);
            });
        }
    }

    /// Keeps what a send came to: when it was sent, why it failed, or, for
    /// a subscription that is gone, nothing.
    fn record(&self, id: &str, delivery: &Delivery) {
        let result = self.change_subscriptions(|list| {
            let Some(index) = list.iter().position(|stored| stored.id == id) else {
                return Ok(((), false));
            };
            match delivery {
                Delivery::Sent => {
                    list[index].last_sent_at = Some(Timestamp::now());
                    list[index].last_error = None;
                }
                Delivery::Gone => {
                    list.remove(index);
                }
                Delivery::Failed(error) => list[index].last_error = Some(error.clone()),
            }
            Ok(((), true))
        });
        if let Err(error) = result {
            eprintln!("push notifications: could not record a send: {error}");
        }
    }

    /// The `Authorization` value for the push service at `audience`.
    fn authorization(&self, audience: &str) -> Result<String, String> {
        let key = self.key().map_err(|error| error.to_string())?;
        let subject = self
            .config()
            .contact
            .unwrap_or_else(|| env!("CARGO_PKG_REPOSITORY").to_owned());
        let now = Timestamp::now().as_second();
        let cache_key = (audience.to_owned(), subject.clone());
        let mut tokens = lock(&self.tokens);
        let token = match tokens.get(&cache_key) {
            Some((token, expires)) if *expires - TOKEN_REFRESH_MARGIN_SECONDS > now => {
                token.clone()
            }
            _ => {
                let expires = now + TOKEN_LIFETIME_SECONDS;
                let token = key
                    .token(audience, expires, &subject)
                    .map_err(|error| error.to_string())?;
                tokens.insert(cache_key, (token.clone(), expires));
                token
            }
        };
        Ok(format!("vapid t={token}, k={}", key.public_key()))
    }

    /// Sends `message` to one subscription, retrying what the push service
    /// asked to retry.
    async fn deliver(&self, subscription: &StoredSubscription, message: &Message) -> Delivery {
        let prepared = (|| {
            let url =
                reqwest::Url::parse(&subscription.endpoint).map_err(|error| error.to_string())?;
            let audience = url.origin().ascii_serialization();
            let authorization = self.authorization(&audience)?;
            let p256dh =
                decode_key(&subscription.p256dh, "p256dh").map_err(|error| error.to_string())?;
            let auth = decode_key(&subscription.auth, "auth").map_err(|error| error.to_string())?;
            let body = crypto::encrypt(&p256dh, &auth, &message.payload)
                .map_err(|error| error.to_string())?;
            Ok::<_, String>((url, authorization, body))
        })();
        let (url, authorization, body) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Delivery::Failed(error),
        };
        let started = tokio::time::Instant::now();
        let mut attempt = 0;
        loop {
            let mut request = self
                .http
                .post(url.clone())
                .header("Authorization", &authorization)
                .header("Content-Encoding", "aes128gcm")
                .header("Content-Type", "application/octet-stream")
                .header("TTL", message.ttl.to_string())
                .header("Urgency", message.urgency)
                .body(body.clone());
            if let Some(topic) = &message.topic {
                request = request.header("Topic", topic);
            }
            let (retry_after, failure) = match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Delivery::Sent;
                    }
                    if status.as_u16() == 404 || status.as_u16() == 410 {
                        return Delivery::Gone;
                    }
                    let retry_after = response
                        .headers()
                        .get("Retry-After")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    let text = error_text(response).await;
                    let detail =
                        format!("{status}: {}", text.chars().take(200).collect::<String>());
                    if !retryable {
                        return Delivery::Failed(detail);
                    }
                    (retry_after, detail)
                }
                Err(error) => {
                    // A network error is retried like a push service that
                    // is down.
                    (None, error.to_string())
                }
            };
            attempt += 1;
            let wait = retry_after.unwrap_or_else(|| RETRY_BACKOFF.delay(attempt));
            let past_deadline = message.deadline.is_some_and(|deadline| {
                Timestamp::now()
                    .checked_add(wait)
                    .is_ok_and(|then| then >= deadline)
            });
            if attempt > RETRIES || started.elapsed() + wait > RETRY_BUDGET || past_deadline {
                return Delivery::Failed(failure);
            }
            tokio::time::sleep(wait).await;
        }
    }
}

/// A daemon notification, kept by the push service for `ttl` seconds.
fn daemon_message(event: DaemonEvent, ttl: u64) -> Message {
    Message {
        payload: Payload::daemon(event).into_bytes(),
        ttl,
        urgency: "normal",
        topic: Some("daemon".to_owned()),
        deadline: None,
    }
}

/// A subscription as clients see it.
fn visible(stored: &StoredSubscription) -> PushSubscription {
    PushSubscription {
        id: stored.id.clone(),
        label: stored.label.clone(),
        push_service: reqwest::Url::parse(&stored.endpoint)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_default(),
        created_at: stored.created_at,
        last_sent_at: stored.last_sent_at,
        last_error: stored.last_error.clone(),
    }
}

/// Eight lowercase letters and digits no subscription in `list` has.
fn new_id(list: &[StoredSubscription]) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    loop {
        let mut random = [0u8; 8];
        let _ = getrandom::getrandom(&mut random);
        let id = random
            .iter()
            .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
            .collect::<String>();
        if !list.iter().any(|stored| stored.id == id) {
            return id;
        }
    }
}

/// The most of a push service's error body the daemon reads.
const ERROR_BODY_LIMIT: usize = 4 * 1024;

/// The start of an error answer's body, for its detail: at most
/// [`ERROR_BODY_LIMIT`] bytes, so a push service that answers with a huge or
/// endless body cannot hold the delivery or its memory.
async fn error_text(mut response: reqwest::Response) -> String {
    let mut body = Vec::new();
    while body.len() < ERROR_BODY_LIMIT {
        match response.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            Ok(None) | Err(_) => break,
        }
    }
    body.truncate(ERROR_BODY_LIMIT);
    String::from_utf8_lossy(&body).into_owned()
}

#[cfg(test)]
mod tests;

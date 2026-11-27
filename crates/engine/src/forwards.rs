//! Forwards (`docs/site/specs/forwards.md`): a machine's web app at the
//! daemon's address, and a TCP or UDP port of one machine on another.
//!
//! The daemon keeps every forward in `<data_root>/forwards.json` and its
//! live state in memory. A supervisor keeps each TCP or UDP forward's
//! listener on its machine while both its machines are connected, and the
//! relay moves each connection's `net/*` notifications from one runtime to
//! the other in order, through one outbox per runtime connection. Every
//! byte goes through the daemon; nothing is buffered beyond what a runtime
//! sent before its window filled.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use cookie_agent_protocol::{
    Forward, ForwardConnection, ForwardKind, ForwardListen, ForwardStamp, ForwardStatus,
    ForwardTarget, SessionId,
    runtime_protocol::{
        NET_EOF, NetAcceptedParams, NetAckParams, NetCloseParams, NetDataParams, NetDatagramParams,
        NetProtocol, method,
    },
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::runtimes::RuntimeClient;

const FORWARDS_FILE: &str = "forwards.json";
const FORWARDS_VERSION: u32 = 1;
/// The most forwards the daemon keeps.
pub(crate) const MAX_FORWARDS: usize = 64;
/// The most connections one forward carries at once.
const MAX_CONNECTIONS: usize = 256;
/// How often the supervisor looks again when nothing woke it.
const TICK: Duration = Duration::from_secs(15);
/// How long a forward whose listen failed waits before it tries again.
const RETRY: Duration = Duration::from_secs(30);
/// How long a UDP peer is remembered without datagrams.
const UDP_IDLE: Duration = Duration::from_secs(60);
/// How long the relay keeps notifications of a connection it does not know
/// yet: one `net/connect` just opened, whose reply is still on its way.
const ORPHAN_KEEP: Duration = Duration::from_secs(10);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A forward request was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    /// No forward has this handle.
    #[error("there is no forward {0}")]
    NotFound(String),
    /// The request cannot be done, and why.
    #[error("{0}")]
    Invalid(String),
}

/// A forward as `forwards.json` keeps it: [`Forward`] without its live
/// state.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ForwardRecord {
    pub(crate) id: String,
    pub(crate) handle: String,
    pub(crate) kind: ForwardKind,
    pub(crate) to: ForwardTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) from: Option<ForwardListen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner: Option<SessionId>,
    pub(crate) created: ForwardStamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<jiff::Timestamp>,
}

#[derive(Default, Deserialize, Serialize)]
struct ForwardsFile {
    version: u32,
    #[serde(default)]
    forwards: Vec<ForwardRecord>,
}

/// What a forward is doing now; never stored.
#[derive(Default)]
struct Live {
    status: Option<ForwardStatus>,
    reason: Option<String>,
    address: Option<String>,
    listener: Option<Listening>,
    failed_at: Option<Instant>,
    open: u64,
    total: u64,
    bytes_in: u64,
    bytes_out: u64,
    recent: VecDeque<(u64, ForwardConnection)>,
}

/// A TCP or UDP forward's listener on its machine.
struct Listening {
    key: RuntimeKey,
    listener: u64,
    client: RuntimeClient,
}

/// One runtime connection: a reconnect is another epoch, whose numbers
/// start again.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RuntimeKey {
    runtime: String,
    epoch: u64,
}

impl RuntimeKey {
    fn of(client: &RuntimeClient) -> Self {
        Self {
            runtime: client.runtime_id().to_owned(),
            epoch: client.epoch(),
        }
    }
}

/// A connection or listener of one runtime connection.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct EndKey {
    runtime: RuntimeKey,
    id: u64,
}

/// One end of a relayed TCP connection.
struct Link {
    forward: String,
    /// The end it relays to, once connected; until then what it sent.
    other: Other,
    /// It ended its direction.
    eof: bool,
    /// The listening machine's end, whose bytes count in.
    inbound: bool,
    /// Its entry among the forward's recent connections.
    recent: u64,
}

enum Other {
    Pending(Vec<Out>),
    Runtime(EndKey),
}

/// A message to a runtime, in the order its outbox sends them.
enum Out {
    Data(NetDataParams),
    Ack(NetAckParams),
    Close(NetCloseParams),
    Datagram(NetDatagramParams),
}

/// A UDP peer of a listener, and the connection on the target its
/// datagrams go through.
struct Flow {
    forward: String,
    connection: Option<EndKey>,
    pending: Vec<String>,
    seen: Instant,
}

#[derive(Default)]
struct Relay {
    /// Listeners, to their forward.
    listeners: HashMap<EndKey, String>,
    links: HashMap<EndKey, Link>,
    /// UDP peers by listener and peer address.
    flows: HashMap<(EndKey, String), Flow>,
    /// A UDP connection on a target, to its listener and peer.
    back: HashMap<EndKey, (EndKey, String)>,
    outboxes: HashMap<RuntimeKey, (RuntimeClient, mpsc::UnboundedSender<Out>)>,
    next_recent: u64,
    /// Notifications of connections not registered yet. A runtime starts
    /// reading a connection once it replied to `net/connect`, so its first
    /// bytes can arrive before the relay has the reply's number.
    orphans: HashMap<EndKey, (Instant, Vec<Orphan>)>,
}

/// A notification held for a connection the relay does not know yet.
enum Orphan {
    Data(NetDataParams),
    Close(NetCloseParams),
    Datagram(NetDatagramParams),
}

impl Relay {
    /// Keeps `orphan` for `end` until its connection is registered, at most
    /// [`ORPHAN_KEEP`]; the oldest go first.
    fn hold(&mut self, end: EndKey, orphan: Orphan) {
        self.orphans.retain(|_, (at, _)| at.elapsed() < ORPHAN_KEEP);
        self.orphans
            .entry(end)
            .or_insert_with(|| (Instant::now(), Vec::new()))
            .1
            .push(orphan);
    }

    /// Queues `out` to the runtime connection `key`, in order.
    fn send(&mut self, key: &RuntimeKey, out: Out) {
        if let Some((_, outbox)) = self.outboxes.get(key) {
            let _ = outbox.send(out);
        }
    }

    /// The outbox of `client`, made the first time it is needed.
    fn outbox(&mut self, client: &RuntimeClient) {
        let key = RuntimeKey::of(client);
        if self.outboxes.contains_key(&key) {
            return;
        }
        let (sender, mut queued) = mpsc::unbounded_channel::<Out>();
        let sending = client.clone();
        tokio::spawn(async move {
            while let Some(out) = queued.recv().await {
                let sent = match out {
                    Out::Data(params) => sending.agent_notify(method::NET_DATA, &params).await,
                    Out::Ack(params) => sending.agent_notify(method::NET_ACK, &params).await,
                    Out::Close(params) => sending.agent_notify(method::NET_CLOSE, &params).await,
                    Out::Datagram(params) => {
                        sending.agent_notify(method::NET_DATAGRAM, &params).await
                    }
                };
                if sent.is_err() {
                    return;
                }
            }
        });
        self.outboxes.insert(key, (client.clone(), sender));
    }
}

/// The daemon's forwards: their store, live state, and relay.
pub(crate) struct Forwards {
    path: PathBuf,
    records: Mutex<Vec<ForwardRecord>>,
    live: Mutex<HashMap<String, Live>>,
    relay: Mutex<Relay>,
    changes: tokio::sync::watch::Sender<u64>,
    kick: tokio::sync::Notify,
}

impl Forwards {
    /// The forwards of `data_root`; none when it has no file yet.
    pub(crate) fn open(data_root: &Path) -> std::io::Result<Arc<Self>> {
        let path = data_root.join(FORWARDS_FILE);
        let records = match std::fs::read(&path) {
            Ok(bytes) => {
                let file: ForwardsFile = serde_json::from_slice(&bytes).map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{} does not read: {error}", path.display()),
                    )
                })?;
                if file.version != FORWARDS_VERSION {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "{} has version {}, which this build does not read",
                            path.display(),
                            file.version
                        ),
                    ));
                }
                file.forwards
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Arc::new(Self {
            path,
            records: Mutex::new(records),
            live: Mutex::default(),
            relay: Mutex::default(),
            changes: tokio::sync::watch::channel(0).0,
            kick: tokio::sync::Notify::new(),
        }))
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    /// Asks the supervisor to look again now.
    pub(crate) fn kick(&self) {
        self.kick.notify_one();
    }

    pub(crate) fn records(&self) -> Vec<ForwardRecord> {
        lock(&self.records).clone()
    }

    /// Changes the stored forwards with `change` and writes them; the
    /// supervisor applies what changed.
    pub(crate) fn mutate<T>(
        &self,
        change: impl FnOnce(&mut Vec<ForwardRecord>) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut records = lock(&self.records);
        let mut next = records.clone();
        let value = change(&mut next)?;
        write_atomic(
            &self.path,
            &serde_json::to_vec_pretty(&ForwardsFile {
                version: FORWARDS_VERSION,
                forwards: next.clone(),
            })
            .map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("{}: {error}", self.path.display()))?;
        *records = next;
        drop(records);
        self.changed();
        self.kick();
        Ok(value)
    }

    /// The forwards as clients see them, `session`'s tree's or all.
    pub(crate) fn list(&self, owner: Option<SessionId>) -> Vec<Forward> {
        let records = self.records();
        let live = lock(&self.live);
        let mut forwards = records
            .into_iter()
            .filter(|record| owner.is_none_or(|owner| record.owner == Some(owner)))
            .map(|record| forward_of(record, &live))
            .collect::<Vec<_>>();
        forwards.sort_by_key(|forward| std::cmp::Reverse(forward.created.at));
        forwards
    }

    /// The forward `handle` as clients see it.
    pub(crate) fn get(&self, handle: &str) -> Option<Forward> {
        let record = self
            .records()
            .into_iter()
            .find(|record| record.handle == handle)?;
        let live = lock(&self.live);
        Some(forward_of(record, &live))
    }

    /// Records a connection of forward `handle` from `peer`; its entry.
    fn opened(&self, handle: &str, peer: String) -> Option<u64> {
        let mut live = lock(&self.live);
        let entry = live.get_mut(handle)?;
        if entry.open as usize >= MAX_CONNECTIONS {
            return None;
        }
        let id = {
            let mut relay = lock(&self.relay);
            relay.next_recent += 1;
            relay.next_recent
        };
        entry.open += 1;
        entry.total += 1;
        entry.recent.push_front((
            id,
            ForwardConnection {
                at: jiff::Timestamp::now(),
                peer,
                ended_at: None,
                bytes_in: 0,
                bytes_out: 0,
                ended: None,
            },
        ));
        entry
            .recent
            .truncate(cookie_agent_protocol::FORWARD_RECENT_MAX);
        drop(live);
        self.changed();
        Some(id)
    }

    fn counted(&self, handle: &str, recent: u64, bytes: u64, inbound: bool) {
        let mut live = lock(&self.live);
        let Some(entry) = live.get_mut(handle) else {
            return;
        };
        if inbound {
            entry.bytes_in += bytes;
        } else {
            entry.bytes_out += bytes;
        }
        if let Some((_, connection)) = entry.recent.iter_mut().find(|(id, _)| *id == recent) {
            if inbound {
                connection.bytes_in += bytes;
            } else {
                connection.bytes_out += bytes;
            }
        }
    }

    fn ended(&self, handle: &str, recent: u64, how: &str) {
        let mut live = lock(&self.live);
        let Some(entry) = live.get_mut(handle) else {
            return;
        };
        if let Some((_, connection)) = entry.recent.iter_mut().find(|(id, _)| *id == recent)
            && connection.ended_at.is_none()
        {
            connection.ended_at = Some(jiff::Timestamp::now());
            connection.ended = Some(how.to_owned());
            entry.open = entry.open.saturating_sub(1);
        }
        drop(live);
        self.changed();
    }
}

fn forward_of(record: ForwardRecord, live: &HashMap<String, Live>) -> Forward {
    let state = live.get(&record.handle);
    let expired = record
        .expires_at
        .is_some_and(|at| at <= jiff::Timestamp::now());
    Forward {
        status: if expired {
            ForwardStatus::Expired
        } else {
            state
                .and_then(|state| state.status)
                .unwrap_or(ForwardStatus::Waiting)
        },
        reason: state.and_then(|state| state.reason.clone()),
        address: state.and_then(|state| state.address.clone()),
        connections_open: state.map_or(0, |state| state.open),
        connections_total: state.map_or(0, |state| state.total),
        bytes_in: state.map_or(0, |state| state.bytes_in),
        bytes_out: state.map_or(0, |state| state.bytes_out),
        recent: state.map_or_else(Vec::new, |state| {
            state
                .recent
                .iter()
                .map(|(_, connection)| connection.clone())
                .collect()
        }),
        id: record.id,
        handle: record.handle,
        kind: record.kind,
        to: record.to,
        from: record.from,
        title: record.title,
        owner: record.owner,
        created: record.created,
        expires_at: record.expires_at,
    }
}

/// Writes `bytes` to `path` through a temporary file renamed over it.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let directory = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(".{FORWARDS_FILE}.{}.tmp", uuid::Uuid::now_v7()));
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}

/// A new handle, `fw_` and 8 hex digits, none of `taken`.
pub(crate) fn new_handle(taken: &HashSet<String>) -> String {
    loop {
        // The last digits of a v7 UUID are random; its first are a time.
        let digits = uuid::Uuid::now_v7().simple().to_string();
        let handle = format!("fw_{}", &digits[digits.len() - 8..]);
        if !taken.contains(&handle) {
            return handle;
        }
    }
}

/// Whether `bind` is a loopback address, which only the machine itself
/// reaches.
pub(crate) fn is_loopback(bind: &str) -> bool {
    bind == "localhost"
        || bind
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

// --- the relay ---------------------------------------------------------------

/// Receives the runtimes' `net/*` notifications for the engine's forwards.
pub(crate) struct NetRelay {
    pub(crate) forwards: Arc<Forwards>,
    /// Finds a machine's runtime, to connect to a forward's target.
    pub(crate) engine: Weak<crate::runtime::Inner>,
}

impl NetRelay {
    fn engine(&self) -> Option<crate::Engine> {
        self.engine.upgrade().map(|inner| crate::Engine { inner })
    }
}

impl crate::runtimes::NetRelayHandler for NetRelay {
    fn accepted(&self, client: &RuntimeClient, params: NetAcceptedParams) {
        let listener = EndKey {
            runtime: RuntimeKey::of(client),
            id: params.listener,
        };
        let end = EndKey {
            runtime: RuntimeKey::of(client),
            id: params.connection,
        };
        let forwards = &self.forwards;
        let Some(handle) = lock(&forwards.relay).listeners.get(&listener).cloned() else {
            let mut relay = lock(&forwards.relay);
            relay.outbox(client);
            relay.send(
                &end.runtime,
                Out::Close(NetCloseParams {
                    connection: end.id,
                    reason: Some("no forward listens there now".into()),
                }),
            );
            return;
        };
        let Some(recent) = forwards.opened(&handle, params.peer) else {
            let mut relay = lock(&forwards.relay);
            relay.outbox(client);
            relay.send(
                &end.runtime,
                Out::Close(NetCloseParams {
                    connection: end.id,
                    reason: Some("the forward carries as many connections as it may".into()),
                }),
            );
            return;
        };
        {
            let mut relay = lock(&forwards.relay);
            relay.outbox(client);
            relay.links.insert(
                end.clone(),
                Link {
                    forward: handle.clone(),
                    other: Other::Pending(Vec::new()),
                    eof: false,
                    inbound: true,
                    recent,
                },
            );
        }
        let Some(record) = forwards
            .records()
            .into_iter()
            .find(|record| record.handle == handle)
        else {
            return;
        };
        let (Some(engine), forwards) = (self.engine(), Arc::clone(forwards)) else {
            return;
        };
        tokio::spawn(async move {
            let target = engine.forward_client(&record.to.machine).await;
            let connected = match target {
                Some(target) => target
                    .net_connect(NetProtocol::Tcp, &record.to.host, record.to.port)
                    .await
                    .map(|connection| (target, connection)),
                None => Err(format!("{} is not connected", record.to.machine)),
            };
            let mut relay = lock(&forwards.relay);
            match connected {
                Ok((target, connection)) => {
                    relay.outbox(&target);
                    let other = EndKey {
                        runtime: RuntimeKey::of(&target),
                        id: connection,
                    };
                    let Some(link) = relay.links.get_mut(&end) else {
                        // The accepted end closed meanwhile.
                        relay.send(
                            &other.runtime,
                            Out::Close(NetCloseParams {
                                connection,
                                reason: None,
                            }),
                        );
                        return;
                    };
                    let pending =
                        match std::mem::replace(&mut link.other, Other::Runtime(other.clone())) {
                            Other::Pending(pending) => pending,
                            Other::Runtime(_) => Vec::new(),
                        };
                    relay.links.insert(
                        other.clone(),
                        Link {
                            forward: handle.clone(),
                            other: Other::Runtime(end.clone()),
                            eof: false,
                            inbound: false,
                            recent,
                        },
                    );
                    for out in pending {
                        relay.send(&other.runtime, retarget(out, connection));
                    }
                    drop(relay);
                    // What the target sent before its number was known.
                    forwards.replay(&other);
                }
                Err(reason) => {
                    relay.links.remove(&end);
                    relay.send(
                        &end.runtime,
                        Out::Close(NetCloseParams {
                            connection: end.id,
                            reason: Some(reason.clone()),
                        }),
                    );
                    drop(relay);
                    forwards.ended(&handle, recent, &reason);
                }
            }
        });
    }

    fn data(&self, client: &RuntimeClient, params: NetDataParams) {
        let end = EndKey {
            runtime: RuntimeKey::of(client),
            id: params.connection,
        };
        self.forwards.data_at(end, params);
    }

    fn ack(&self, client: &RuntimeClient, params: NetAckParams) {
        let end = EndKey {
            runtime: RuntimeKey::of(client),
            id: params.connection,
        };
        let mut relay = lock(&self.forwards.relay);
        let other = match relay.links.get(&end).map(|link| &link.other) {
            Some(Other::Runtime(other)) => other.clone(),
            _ => return,
        };
        relay.send(
            &other.runtime,
            Out::Ack(NetAckParams {
                connection: other.id,
                bytes: params.bytes,
            }),
        );
    }

    fn close(&self, client: &RuntimeClient, params: NetCloseParams) {
        let end = EndKey {
            runtime: RuntimeKey::of(client),
            id: params.connection,
        };
        self.forwards.close_at(end, params);
    }

    fn datagram(&self, client: &RuntimeClient, params: NetDatagramParams) {
        self.datagram_from(client, params);
    }

    fn connection_lost(&self, client: &RuntimeClient) {
        self.lost(client);
    }
}

impl Forwards {
    /// Replays what was held for `end`, now registered.
    fn replay(&self, end: &EndKey) {
        let held = lock(&self.relay).orphans.remove(end);
        for orphan in held.map(|(_, held)| held).unwrap_or_default() {
            match orphan {
                Orphan::Data(params) => self.data_at(end.clone(), params),
                Orphan::Close(params) => self.close_at(end.clone(), params),
                Orphan::Datagram(params) => self.datagram_back(end.clone(), params),
            }
        }
    }

    /// `net/data` of connection `end`.
    fn data_at(&self, end: EndKey, params: NetDataParams) {
        let bytes = decoded_length(&params.data);
        let mut relay = lock(&self.relay);
        let Some(link) = relay.links.get_mut(&end) else {
            relay.hold(end, Orphan::Data(params));
            return;
        };
        let (handle, recent, inbound) = (link.forward.clone(), link.recent, link.inbound);
        match &mut link.other {
            Other::Pending(pending) => pending.push(Out::Data(params)),
            Other::Runtime(other) => {
                let other = other.clone();
                relay.send(
                    &other.runtime,
                    Out::Data(NetDataParams {
                        connection: other.id,
                        data: params.data,
                    }),
                );
            }
        }
        drop(relay);
        self.counted(&handle, recent, bytes, inbound);
    }

    /// `net/close` of connection `end`.
    fn close_at(&self, end: EndKey, params: NetCloseParams) {
        let eof = params.reason.as_deref() == Some(NET_EOF);
        let mut relay = lock(&self.relay);
        let Some(link) = relay.links.get_mut(&end) else {
            // A UDP connection the target closed.
            if let Some((listener, peer)) = relay.back.remove(&end) {
                relay.flows.remove(&(listener, peer));
            } else {
                relay.hold(end, Orphan::Close(params));
            }
            return;
        };
        let (handle, recent) = (link.forward.clone(), link.recent);
        if let Other::Pending(pending) = &mut link.other {
            if eof {
                link.eof = true;
                pending.push(Out::Close(params));
            } else {
                relay.links.remove(&end);
                drop(relay);
                self.ended(
                    &handle,
                    recent,
                    params.reason.as_deref().unwrap_or("closed"),
                );
            }
            return;
        }
        let Other::Runtime(other) = &link.other else {
            return;
        };
        let other = other.clone();
        if eof {
            link.eof = true;
            relay.send(
                &other.runtime,
                Out::Close(NetCloseParams {
                    connection: other.id,
                    reason: Some(NET_EOF.to_owned()),
                }),
            );
            let both = relay.links.get(&other).is_some_and(|other| other.eof);
            if !both {
                return;
            }
            // Both directions ended: the connection is done on both sides.
            for done in [&end, &other] {
                relay.send(
                    &done.runtime,
                    Out::Close(NetCloseParams {
                        connection: done.id,
                        reason: Some("closed".into()),
                    }),
                );
            }
            relay.links.remove(&end);
            relay.links.remove(&other);
            drop(relay);
            self.ended(&handle, recent, "closed");
            return;
        }
        relay.send(
            &other.runtime,
            Out::Close(NetCloseParams {
                connection: other.id,
                reason: params.reason.clone(),
            }),
        );
        relay.links.remove(&end);
        relay.links.remove(&other);
        drop(relay);
        self.ended(
            &handle,
            recent,
            params.reason.as_deref().unwrap_or("closed"),
        );
    }

    /// A datagram of UDP connection `end` on a target: back to its
    /// listener's peer.
    fn datagram_back(&self, end: EndKey, params: NetDatagramParams) {
        let bytes = decoded_length(&params.data);
        let mut relay = lock(&self.relay);
        let Some((listener, peer)) = relay.back.get(&end).cloned() else {
            relay.hold(end, Orphan::Datagram(params));
            return;
        };
        let handle = match relay.flows.get_mut(&(listener.clone(), peer.clone())) {
            Some(flow) => {
                flow.seen = Instant::now();
                flow.forward.clone()
            }
            None => return,
        };
        relay.send(
            &listener.runtime,
            Out::Datagram(NetDatagramParams {
                listener: Some(listener.id),
                connection: None,
                peer: Some(peer),
                data: params.data,
            }),
        );
        drop(relay);
        self.counted(&handle, 0, bytes, false);
    }
}

impl NetRelay {
    fn datagram_from(&self, client: &RuntimeClient, params: NetDatagramParams) {
        let runtime = RuntimeKey::of(client);
        let bytes = decoded_length(&params.data);
        if let Some(connection) = params.connection {
            self.forwards.datagram_back(
                EndKey {
                    runtime,
                    id: connection,
                },
                params,
            );
            return;
        }
        let (Some(listener), Some(peer)) = (params.listener, params.peer) else {
            return;
        };
        let listener = EndKey {
            runtime,
            id: listener,
        };
        let mut relay = lock(&self.forwards.relay);
        relay.outbox(client);
        let Some(handle) = relay.listeners.get(&listener).cloned() else {
            return;
        };
        let key = (listener.clone(), peer.clone());
        if let Some(flow) = relay.flows.get_mut(&key) {
            flow.seen = Instant::now();
            match flow.connection.clone() {
                Some(connection) => relay.send(
                    &connection.runtime,
                    Out::Datagram(NetDatagramParams {
                        listener: None,
                        connection: Some(connection.id),
                        peer: None,
                        data: params.data,
                    }),
                ),
                None => flow.pending.push(params.data),
            }
            drop(relay);
            self.forwards.counted(&handle, 0, bytes, true);
            return;
        }
        relay.flows.insert(
            key.clone(),
            Flow {
                forward: handle.clone(),
                connection: None,
                pending: vec![params.data],
                seen: Instant::now(),
            },
        );
        drop(relay);
        self.forwards.counted(&handle, 0, bytes, true);
        let Some(record) = self
            .forwards
            .records()
            .into_iter()
            .find(|record| record.handle == handle)
        else {
            return;
        };
        let (Some(engine), forwards) = (self.engine(), Arc::clone(&self.forwards)) else {
            return;
        };
        forwards.opened(&handle, peer.clone());
        tokio::spawn(async move {
            let connected = match engine.forward_client(&record.to.machine).await {
                Some(target) => target
                    .net_connect(NetProtocol::Udp, &record.to.host, record.to.port)
                    .await
                    .map(|connection| (target, connection)),
                None => Err(format!("{} is not connected", record.to.machine)),
            };
            let mut relay = lock(&forwards.relay);
            let Ok((target, connection)) = connected else {
                relay.flows.remove(&key);
                return;
            };
            relay.outbox(&target);
            let end = EndKey {
                runtime: RuntimeKey::of(&target),
                id: connection,
            };
            let Some(flow) = relay.flows.get_mut(&key) else {
                relay.send(
                    &end.runtime,
                    Out::Close(NetCloseParams {
                        connection,
                        reason: None,
                    }),
                );
                return;
            };
            flow.connection = Some(end.clone());
            let pending = std::mem::take(&mut flow.pending);
            relay.back.insert(end.clone(), key.clone());
            for data in pending {
                relay.send(
                    &end.runtime,
                    Out::Datagram(NetDatagramParams {
                        listener: None,
                        connection: Some(connection),
                        peer: None,
                        data,
                    }),
                );
            }
            drop(relay);
            forwards.replay(&end);
        });
    }

    fn lost(&self, client: &RuntimeClient) {
        let lost = RuntimeKey::of(client);
        let mut relay = lock(&self.forwards.relay);
        relay.outboxes.remove(&lost);
        relay
            .listeners
            .retain(|listener, _| listener.runtime != lost);
        let ends = relay
            .links
            .keys()
            .filter(|end| end.runtime == lost)
            .cloned()
            .collect::<Vec<_>>();
        let mut ended = Vec::new();
        for end in ends {
            let Some(link) = relay.links.remove(&end) else {
                continue;
            };
            if let Other::Runtime(other) = link.other {
                relay.links.remove(&other);
                relay.send(
                    &other.runtime,
                    Out::Close(NetCloseParams {
                        connection: other.id,
                        reason: Some(format!("{} disconnected", lost.runtime)),
                    }),
                );
            }
            ended.push((link.forward, link.recent));
        }
        relay.flows.retain(|(listener, _), flow| {
            listener.runtime != lost
                && flow
                    .connection
                    .as_ref()
                    .is_none_or(|connection| connection.runtime != lost)
        });
        relay
            .back
            .retain(|end, (listener, _)| end.runtime != lost && listener.runtime != lost);
        drop(relay);
        for (handle, recent) in ended {
            self.forwards
                .ended(&handle, recent, &format!("{} disconnected", lost.runtime));
        }
        self.forwards.kick();
    }
}

/// `out`, sent to the connection `connection` instead.
fn retarget(out: Out, connection: u64) -> Out {
    match out {
        Out::Data(params) => Out::Data(NetDataParams {
            connection,
            data: params.data,
        }),
        Out::Ack(params) => Out::Ack(NetAckParams {
            connection,
            bytes: params.bytes,
        }),
        Out::Close(params) => Out::Close(NetCloseParams {
            connection,
            reason: params.reason,
        }),
        Out::Datagram(params) => Out::Datagram(params),
    }
}

/// The bytes base64 `data` decodes to.
fn decoded_length(data: &str) -> u64 {
    let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count();
    ((data.len() / 4) * 3).saturating_sub(padding) as u64
}

// --- the supervisor -------------------------------------------------------------

impl Forwards {
    /// Brings every forward to where it should be: listening while its
    /// machines are connected, stopped once deleted or expired. Removes the
    /// expired ones.
    pub(crate) async fn reconcile(self: &Arc<Self>, engine: &crate::Engine) {
        let now = jiff::Timestamp::now();
        let expired = self
            .records()
            .iter()
            .filter(|record| record.expires_at.is_some_and(|at| at <= now))
            .map(|record| record.handle.clone())
            .collect::<HashSet<_>>();
        if !expired.is_empty() {
            let _ = self.mutate(|records| {
                records.retain(|record| !expired.contains(&record.handle));
                Ok(())
            });
            engine.revoke_forward_shares(&expired);
        }
        let records = self.records();
        let wanted = records
            .iter()
            .map(|record| record.handle.clone())
            .collect::<HashSet<_>>();
        // Forwards gone: their listeners stop and their connections end.
        let gone = lock(&self.live)
            .keys()
            .filter(|handle| !wanted.contains(*handle))
            .cloned()
            .collect::<Vec<_>>();
        for handle in gone {
            let live = lock(&self.live).remove(&handle);
            if let Some(Listening {
                listener, client, ..
            }) = live.and_then(|live| live.listener)
            {
                client.net_unlisten(listener).await;
            }
            self.stop_connections(&handle);
        }
        let mut changed = false;
        for record in records {
            let (status, reason, address) = self.reconcile_one(engine, &record).await;
            let mut live = lock(&self.live);
            let entry = live.entry(record.handle.clone()).or_default();
            if entry.status != Some(status) || entry.reason != reason || entry.address != address {
                entry.status = Some(status);
                entry.reason = reason;
                entry.address = address;
                changed = true;
            }
        }
        self.sweep_flows();
        if changed {
            self.changed();
        }
    }

    async fn reconcile_one(
        self: &Arc<Self>,
        engine: &crate::Engine,
        record: &ForwardRecord,
    ) -> (ForwardStatus, Option<String>, Option<String>) {
        let target = engine.forward_client(&record.to.machine).await;
        let waiting = |machine: &str| {
            (
                ForwardStatus::Waiting,
                Some(format!("{machine} is not connected")),
                None,
            )
        };
        let Some(from) = &record.from else {
            // An `http` forward is served by the daemon.
            return match target {
                Some(_) => (
                    ForwardStatus::Active,
                    None,
                    Some(format!("p/{}/", record.handle)),
                ),
                None => waiting(&record.to.machine),
            };
        };
        let source = engine.forward_client(&from.machine).await;
        let (Some(source), Some(_)) = (source, target) else {
            self.unlisten(&record.handle).await;
            let machine = if engine.forward_client(&from.machine).await.is_none() {
                &from.machine
            } else {
                &record.to.machine
            };
            return waiting(machine);
        };
        let key = RuntimeKey::of(&source);
        let current = lock(&self.live).get(&record.handle).and_then(|live| {
            live.listener
                .as_ref()
                .filter(|listening| listening.key == key && !listening.client.is_closed())
                .map(|_| live.address.clone())
        });
        if let Some(address) = current {
            return (ForwardStatus::Active, None, address);
        }
        self.unlisten(&record.handle).await;
        let recently_failed = lock(&self.live)
            .get(&record.handle)
            .and_then(|live| live.failed_at)
            .is_some_and(|at| at.elapsed() < RETRY);
        if recently_failed {
            let reason = lock(&self.live)
                .get(&record.handle)
                .and_then(|live| live.reason.clone());
            return (ForwardStatus::Failed, reason, None);
        }
        let protocol = match record.kind {
            ForwardKind::Udp => NetProtocol::Udp,
            _ => NetProtocol::Tcp,
        };
        match source.net_listen(protocol, &from.bind, from.port).await {
            Ok(listened) => {
                let address = format!("{}:{}", from.bind, listened.port);
                {
                    let mut relay = lock(&self.relay);
                    relay.outbox(&source);
                    relay.listeners.insert(
                        EndKey {
                            runtime: key.clone(),
                            id: listened.listener,
                        },
                        record.handle.clone(),
                    );
                }
                let mut live = lock(&self.live);
                let entry = live.entry(record.handle.clone()).or_default();
                entry.listener = Some(Listening {
                    key,
                    listener: listened.listener,
                    client: source,
                });
                entry.failed_at = None;
                (ForwardStatus::Active, None, Some(address))
            }
            Err(reason) => {
                lock(&self.live)
                    .entry(record.handle.clone())
                    .or_default()
                    .failed_at = Some(Instant::now());
                (ForwardStatus::Failed, Some(reason), None)
            }
        }
    }

    /// Stops forward `handle`'s listener, if it has one.
    async fn unlisten(&self, handle: &str) {
        let listening = lock(&self.live)
            .get_mut(handle)
            .and_then(|live| live.listener.take());
        let Some(listening) = listening else {
            return;
        };
        lock(&self.relay).listeners.remove(&EndKey {
            runtime: listening.key.clone(),
            id: listening.listener,
        });
        if !listening.client.is_closed() {
            listening.client.net_unlisten(listening.listener).await;
        }
    }

    /// Ends every connection of forward `handle`.
    fn stop_connections(&self, handle: &str) {
        let mut relay = lock(&self.relay);
        relay.listeners.retain(|_, forward| forward != handle);
        let ends = relay
            .links
            .iter()
            .filter(|(_, link)| link.forward == handle)
            .map(|(end, _)| end.clone())
            .collect::<Vec<_>>();
        for end in ends {
            relay.links.remove(&end);
            relay.send(
                &end.runtime,
                Out::Close(NetCloseParams {
                    connection: end.id,
                    reason: Some("the forward was stopped".into()),
                }),
            );
        }
        let flows = relay
            .flows
            .iter()
            .filter(|(_, flow)| flow.forward == handle)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in flows {
            if let Some(Flow {
                connection: Some(connection),
                ..
            }) = relay.flows.remove(&key)
            {
                relay.back.remove(&connection);
                relay.send(
                    &connection.runtime,
                    Out::Close(NetCloseParams {
                        connection: connection.id,
                        reason: None,
                    }),
                );
            }
        }
    }

    /// Forgets UDP peers quiet for [`UDP_IDLE`].
    fn sweep_flows(&self) {
        let mut relay = lock(&self.relay);
        let idle = relay
            .flows
            .iter()
            .filter(|(_, flow)| flow.seen.elapsed() >= UDP_IDLE)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let mut ended = Vec::new();
        for key in idle {
            if let Some(flow) = relay.flows.remove(&key) {
                if let Some(connection) = flow.connection {
                    relay.back.remove(&connection);
                    relay.send(
                        &connection.runtime,
                        Out::Close(NetCloseParams {
                            connection: connection.id,
                            reason: None,
                        }),
                    );
                }
                ended.push(flow.forward);
            }
        }
        drop(relay);
        for handle in ended {
            let mut live = lock(&self.live);
            if let Some(entry) = live.get_mut(&handle) {
                entry.open = entry.open.saturating_sub(1);
            }
        }
    }

    /// Runs the supervisor until the engine is gone: on every runtime
    /// change, every change of the forwards, and every [`TICK`].
    pub(crate) async fn supervise(self: Arc<Self>, engine: Weak<crate::runtime::Inner>) {
        let Some(mut runtimes) = engine
            .upgrade()
            .map(|inner| crate::Engine { inner }.subscribe_runtime_instance_changes())
        else {
            return;
        };
        loop {
            let Some(inner) = engine.upgrade() else {
                return;
            };
            let current = crate::Engine { inner };
            self.reconcile(&current).await;
            drop(current);
            tokio::select! {
                changed = runtimes.changed() => if changed.is_err() { return; },
                () = self.kick.notified() => {}
                () = tokio::time::sleep(TICK) => {}
            }
        }
    }
}

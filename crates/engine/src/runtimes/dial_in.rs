//! Dial-in runtimes (`docs/site/specs/runtime-capabilities.md` section 5):
//! runtimes that connect to the daemon on their own, persistently or for a
//! client's lifetime, instead of the daemon starting them.
//!
//! A [`DialInTemplate`] exists from a runtime's first connection on and
//! keeps its ID. It has a connection only while the runtime is connected; a
//! runtime that dials in again under the same ID takes over the template, and
//! the daemon reopens the instances its sessions use over the new
//! connection.

use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_protocol::{CapabilityName, RuntimeCapabilityInfo, RuntimeLifetime};
use cookie_agent_runtime_api::protocol::RuntimeSettings;

use super::{
    McpHub, RuntimeClient,
    registry::{RuntimeListingState, RuntimeTemplate, StartError, StartedRuntime},
};

/// How long a dial-in runtime gets to answer `runtime/initialize` and each
/// `capability/open`.
pub(crate) const DIAL_IN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a stopping dial-in runtime gets to answer `runtime/shutdown`.
const DIAL_IN_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Why a session cannot use dial-in runtime `runtime_id` now: it is not
/// connected, and was last heard from at `last_seen` when that is known.
pub(crate) fn not_connected(runtime_id: &str, last_seen: Option<jiff::Timestamp>) -> String {
    let last_seen = last_seen.map(|seen| seen.round(jiff::Unit::Second).unwrap_or(seen));
    match last_seen {
        Some(seen) => format!("runtime `{runtime_id}` is not connected (last seen {seen})"),
        None => format!("runtime `{runtime_id}` is not connected"),
    }
}

/// Why the daemon refused a runtime that dialed in.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DialInRefusal {
    /// The ID is not a runtime ID.
    #[error("invalid runtime ID `{0}`: it must match [a-z][a-z0-9_]{{0,31}}")]
    InvalidId(String),
    /// The ID is a built-in runtime's.
    #[error("runtime ID `{0}` is reserved for the daemon's built-in runtime")]
    Reserved(String),
    /// The daemon configures a runtime of the ID itself.
    #[error("runtime `{0}` is configured on the daemon ([runtimes.{0}]) and cannot dial in")]
    Configured(String),
    /// A runtime is connected under the ID now.
    #[error("runtime `{0}` is already connected")]
    Connected(String),
    /// The runtime did not complete `runtime/initialize`.
    #[error("runtime `{0}` failed to start: {1}")]
    Initialize(String, String),
}

impl DialInRefusal {
    /// Whether dialing in again under the ID cannot succeed until the
    /// runtime's ID or the daemon's configuration changes.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::InvalidId(_) | Self::Reserved(_) | Self::Configured(_)
        )
    }
}

/// A runtime that dials in.
pub(crate) struct DialInTemplate {
    runtime_id: String,
    /// The connection, while the runtime is connected.
    connection: Mutex<Option<RuntimeClient>>,
    /// Whether a runtime dialing in under the ID is being accepted now.
    claimed: AtomicBool,
    /// What the runtime offered when it last connected.
    offered: Mutex<RuntimeListingState>,
    /// When the runtime was last heard from, once a connection ended.
    last_seen: Mutex<Option<jiff::Timestamp>>,
    /// How its connections' liveness is checked.
    liveness: super::Liveness,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl DialInTemplate {
    pub(crate) fn new(runtime_id: &str) -> Self {
        Self {
            runtime_id: runtime_id.to_owned(),
            connection: Mutex::new(None),
            claimed: AtomicBool::new(false),
            offered: Mutex::default(),
            last_seen: Mutex::default(),
            liveness: super::Liveness::DEFAULT,
        }
    }

    /// Reserves the ID for one runtime dialing in, until
    /// [`Self::release`]: refused while a runtime is connected under it or
    /// another is being accepted. A runtime that dials in while its old
    /// connection still looks open has most likely lost that connection
    /// without a close, so the old one is checked at once, and the runtime
    /// dialing in again gets in once it is ended.
    pub(crate) fn claim(&self) -> Result<(), DialInRefusal> {
        // Checked under the connection's lock: an accepted runtime's
        // connection is set before its claim is released.
        let connection = lock(&self.connection);
        let connected = connection.as_ref().filter(|client| !client.is_closed());
        if let Some(client) = connected {
            client.suspect();
            return Err(DialInRefusal::Connected(self.runtime_id.clone()));
        }
        if self.claimed.swap(true, Ordering::AcqRel) {
            return Err(DialInRefusal::Connected(self.runtime_id.clone()));
        }
        Ok(())
    }

    /// Ends a claim [`Self::claim`] made.
    pub(crate) fn release(&self) {
        self.claimed.store(false, Ordering::Release);
    }

    /// The live connection, if the runtime is connected.
    fn current(&self) -> Option<RuntimeClient> {
        lock(&self.connection)
            .clone()
            .filter(|client| !client.is_closed())
    }

    /// Whether the runtime is connected now.
    pub(crate) fn is_connected(&self) -> bool {
        self.current().is_some()
    }

    /// Initializes the runtime over `connection` and makes it the template's
    /// connection. The runtime brings its own MCP servers, so the daemon
    /// sends none.
    pub(crate) async fn connect(
        &self,
        connection: cookie_agent_runtime_api::RuntimeConnection,
        settings: &RuntimeSettings,
        mcp_hub: Option<Arc<McpHub>>,
    ) -> Result<RuntimeClient, String> {
        let initialize = RuntimeClient::initialize(
            connection,
            &self.runtime_id,
            settings,
            DIAL_IN_SHUTDOWN_GRACE,
            mcp_hub,
        );
        let (client, result) = tokio::time::timeout(DIAL_IN_TIMEOUT, initialize)
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "the runtime did not initialize within {} s",
                    DIAL_IN_TIMEOUT.as_secs()
                ))
            })
            .inspect_err(|reason| lock(&self.offered).message = Some(reason.clone()))?;
        client.keep_alive(self.liveness);
        *lock(&self.offered) = RuntimeListingState {
            connected: true,
            capabilities: client.capabilities(),
            platform: result.platform,
            machine: result
                .machine
                .filter(cookie_agent_protocol::runtime_protocol::RuntimeMachine::is_valid),
            home: client.home(),
            message: None,
        };
        *lock(&self.connection) = Some(client.clone());
        Ok(client)
    }

    /// Records that connection `epoch`, last heard from at `last_seen`,
    /// ended for `reason`, unless a newer connection took over meanwhile.
    pub(crate) fn disconnected(&self, epoch: u64, reason: &str, last_seen: jiff::Timestamp) {
        let connection = lock(&self.connection);
        if connection
            .as_ref()
            .is_some_and(|client| client.epoch() != epoch)
        {
            return;
        }
        *lock(&self.last_seen) = Some(last_seen);
        lock(&self.offered).message = Some(format!(
            "runtime `{}` disconnected: {reason}",
            self.runtime_id
        ));
    }

    fn not_connected(&self) -> String {
        let recorded = *lock(&self.last_seen);
        // A connection that has just ended may not be recorded yet.
        let last_seen = recorded.or_else(|| {
            lock(&self.connection)
                .as_ref()
                .map(RuntimeClient::last_heard_at)
        });
        not_connected(&self.runtime_id, last_seen)
    }
}

#[async_trait]
impl RuntimeTemplate for DialInTemplate {
    fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    fn lifetime(&self) -> RuntimeLifetime {
        RuntimeLifetime::DialIn
    }

    async fn capabilities(&self) -> Result<Vec<RuntimeCapabilityInfo>, String> {
        self.current()
            .map(|client| client.capabilities())
            .ok_or_else(|| self.not_connected())
    }

    fn listing(&self) -> RuntimeListingState {
        let mut listing = lock(&self.offered).clone();
        listing.connected = self.is_connected();
        if listing.connected {
            listing.message = None;
        }
        listing
    }

    fn offline(&self) -> Option<String> {
        (!self.is_connected()).then(|| self.not_connected())
    }

    /// Opens `capability` over the live connection: refused at once when
    /// the runtime is not connected, or its connection no longer answers.
    async fn start(
        &self,
        capability: &CapabilityName,
        workspace: Option<&Path>,
    ) -> Result<StartedRuntime, StartError> {
        let client = self
            .current()
            .ok_or_else(|| StartError::Connect(self.not_connected()))?;
        if let Err(reason) = client.verify_live().await {
            return Err(StartError::Connect(format!(
                "runtime `{}` is not connected: {reason}",
                self.runtime_id
            )));
        }
        tokio::time::timeout(DIAL_IN_TIMEOUT, client.open(capability, workspace))
            .await
            .unwrap_or_else(|_| {
                client.suspect();
                Err(StartError::Open(format!(
                    "the runtime did not open `{capability}` within {} s",
                    DIAL_IN_TIMEOUT.as_secs()
                )))
            })
    }

    fn disconnect(&self) {
        lock(&self.connection).take();
    }
}

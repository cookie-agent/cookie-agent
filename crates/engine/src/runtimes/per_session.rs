//! Templates whose process is per session tree
//! (`docs/site/specs/temporary-runtimes.md`).
//!
//! A `scope = "session"` template is registered once, under its runtime ID,
//! as a [`PerSessionTemplate`]: what clients list, configure, and
//! disconnect. It is never started itself. Each root session that uses it
//! gets a [`ProtocolTemplate`] of its own, the *owned* template, whose
//! command has `{session_id}` replaced with the root's ID, and the registry
//! keys that root's instances by it ([`RuntimeTemplate::owned`]). Every
//! owned template speaks for the same runtime ID, so attachments,
//! permissions, and tool origins never see the owner.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use cookie_agent_config::RuntimeTemplateConfig;
use cookie_agent_protocol::{CapabilityName, RuntimeCapabilityInfo, SessionId};
use cookie_agent_runtime_api::protocol::{self, RuntimeSettings};

use super::{
    McpHub, ProcessConnector, ProcessRecords, ProtocolTemplate, RuntimeAcpAgents,
    RuntimeListingState, RuntimeShellEnvironment, RuntimeTemplate, StartError, StartedRuntime,
    registry::RuntimeLink,
};
use crate::EngineError;

/// A `scope = "session"` template and the owned templates of the root
/// sessions that used it.
pub(crate) struct PerSessionTemplate {
    runtime_id: String,
    config: RuntimeTemplateConfig,
    mcp_hub: Arc<McpHub>,
    /// What the owned templates initialize with; every one gets a change.
    settings: Mutex<RuntimeSettings>,
    /// By owning root session. An owned template stays here after its
    /// process stops, so the next use connects it again.
    owned: Mutex<BTreeMap<SessionId, Arc<ProtocolTemplate>>>,
    /// What the latest owned template reported when it connected, for
    /// listings while none is connected.
    last: Mutex<RuntimeListingState>,
    /// Where each owner's process is recorded.
    records: Arc<ProcessRecords>,
    /// Each owned template's connector, by owner, to wait for its last
    /// process's `stop_command`.
    connectors: Mutex<BTreeMap<SessionId, Arc<ProcessConnector>>>,
}

impl PerSessionTemplate {
    pub(crate) fn new(
        runtime_id: &str,
        config: RuntimeTemplateConfig,
        settings: RuntimeSettings,
        mcp_hub: Arc<McpHub>,
        records: Arc<ProcessRecords>,
    ) -> Self {
        Self {
            runtime_id: runtime_id.to_owned(),
            config,
            mcp_hub,
            settings: Mutex::new(settings),
            owned: Mutex::default(),
            last: Mutex::default(),
            records,
            connectors: Mutex::default(),
        }
    }

    fn templates(&self) -> Vec<Arc<ProtocolTemplate>> {
        lock(&self.owned).values().cloned().collect()
    }

    /// The owned templates whose process is connected now.
    fn connected(&self) -> Vec<Arc<ProtocolTemplate>> {
        self.templates()
            .into_iter()
            .filter(|template| template.listing().connected)
            .collect()
    }

    fn not_started(&self) -> String {
        format!(
            "runtime `{}` starts one process per session tree, when a session uses it",
            self.runtime_id
        )
    }
}

#[async_trait]
impl RuntimeTemplate for PerSessionTemplate {
    fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    async fn capabilities(&self) -> Result<Vec<RuntimeCapabilityInfo>, String> {
        let known = self.listing().capabilities;
        if known.is_empty() {
            Err(self.not_started())
        } else {
            Ok(known)
        }
    }

    /// Connected while any owner's process is. The machine is never the
    /// one a process reports: each is a fresh container or sandbox, so the
    /// daemon places the runtime on a machine of its own
    /// (`docs/site/specs/temporary-runtimes.md` section 6).
    fn listing(&self) -> RuntimeListingState {
        let connected = self.connected();
        let mut listing = match connected.first() {
            Some(template) => {
                let listing = template.listing();
                lock(&self.last).clone_from(&listing);
                listing
            }
            None => {
                let mut last = lock(&self.last).clone();
                last.connected = false;
                last
            }
        };
        listing.machine = None;
        listing
    }

    async fn start(
        &self,
        _capability: &CapabilityName,
        _workspace: Option<&Path>,
    ) -> Result<StartedRuntime, StartError> {
        Err(StartError::Connect(self.not_started()))
    }

    fn disconnect(&self) -> Vec<RuntimeLink> {
        self.templates()
            .iter()
            .flat_map(|template| template.disconnect())
            .collect()
    }

    fn update_settings(&self, settings: &RuntimeSettings) {
        lock(&self.settings).clone_from(settings);
        for template in self.templates() {
            template.update_settings(settings);
        }
    }

    fn runtime_mcp_servers(&self) -> Vec<cookie_agent_protocol::McpServerInfo> {
        self.connected()
            .first()
            .map(|template| template.runtime_mcp_servers())
            .unwrap_or_default()
    }

    async fn reconnect_runtime_mcp(&self, server: &str) -> Result<(), EngineError> {
        let connected = self.connected();
        if connected.is_empty() {
            return Err(EngineError::Mcp(format!(
                "runtime `{}` is not running",
                self.runtime_id
            )));
        }
        for template in connected {
            template.reconnect_runtime_mcp(server).await?;
        }
        Ok(())
    }

    fn acp_agents(&self) -> RuntimeAcpAgents {
        match self.connected().first() {
            Some(template) => template.acp_agents(),
            None => RuntimeAcpAgents {
                agents: lock(&self.settings)
                    .acp_agents
                    .iter()
                    .map(protocol::AcpAgentReport::from)
                    .collect(),
                available: true,
            },
        }
    }

    fn shell_environment(&self) -> Option<RuntimeShellEnvironment> {
        self.connected()
            .first()
            .and_then(|template| template.shell_environment())
    }

    fn refresh_shell_environment(&self) {
        for template in self.connected() {
            template.refresh_shell_environment();
        }
    }

    async fn agent_connection(&self) -> Result<RuntimeLink, String> {
        Err(self.not_started())
    }

    fn per_session(&self) -> bool {
        true
    }

    fn idle_stop_after(&self) -> Option<std::time::Duration> {
        self.config.idle_stop_after
    }

    fn connected_owners(&self) -> Vec<Option<SessionId>> {
        lock(&self.owned)
            .iter()
            .filter(|(_, template)| template.listing().connected)
            .map(|(owner, _)| Some(*owner))
            .collect()
    }

    async fn release_owner(&self, owner: SessionId) {
        let template = lock(&self.owned).remove(&owner);
        let connector = lock(&self.connectors).remove(&owner);
        drop(template);
        if let Some(connector) = connector {
            connector.settled().await;
        }
        super::temporary::run_delete(&self.records, &self.runtime_id, &self.config, owner).await;
    }

    fn owned(&self, owner: SessionId) -> Option<Arc<dyn RuntimeTemplate>> {
        let mut owned = lock(&self.owned);
        let template = owned.entry(owner).or_insert_with(|| {
            let launch = self.config.launch(Some(&owner.to_string()));
            let connector = Arc::new(
                ProcessConnector::new(launch.command, launch.env, self.config.shutdown_grace())
                    .with_lifecycle(Arc::new(super::temporary::TemporaryProcess::new(
                        &self.runtime_id,
                        Some(owner),
                        self.config.clone(),
                        Arc::clone(&self.records),
                    ))),
            );
            lock(&self.connectors).insert(owner, Arc::clone(&connector));
            Arc::new(
                ProtocolTemplate::new(
                    &self.runtime_id,
                    connector,
                    lock(&self.settings).clone(),
                    self.config.startup_timeout(),
                    self.config.shutdown_grace(),
                )
                .with_mcp_hub(Arc::clone(&self.mcp_hub)),
            )
        });
        Some(Arc::clone(template) as Arc<dyn RuntimeTemplate>)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

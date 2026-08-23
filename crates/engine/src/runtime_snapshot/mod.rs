//! One coherent immutable engine runtime publication.

mod agents;
pub(crate) mod projection;

use std::{collections::BTreeMap, sync::Arc};

use cookie_agent_models::{CompiledModelRuntime, manifests::ModelSnapshotManifestIndex};
use cookie_agent_protocol::{RuntimeChangedNotification, RuntimeSnapshotResult};

pub(crate) use agents::{AgentRegistry, ResolvedAgent, ResolvedAgentFallback, delegation_targets};
pub(crate) use projection::build_runtime_snapshot;

/// Exact executable and wire state published as one atomic value.
#[derive(Clone)]
pub struct PublishedRuntime {
    pub result: RuntimeSnapshotResult,
    pub models: Arc<CompiledModelRuntime>,
    pub agents: Arc<AgentRegistry>,
    pub agent_presets: BTreeMap<String, Arc<AgentRegistry>>,
    pub manifests: Arc<ModelSnapshotManifestIndex>,
    pub current_manifest: Arc<cookie_agent_protocol::ModelSnapshotManifestV1>,
}

impl PublishedRuntime {
    pub(crate) fn agents_for_preset(
        &self,
        preset: Option<&str>,
    ) -> Result<Arc<AgentRegistry>, crate::EngineError> {
        match preset {
            None => Ok(Arc::clone(&self.agents)),
            Some(name) => self
                .agent_presets
                .get(name)
                .cloned()
                .ok_or_else(|| crate::EngineError::UnknownAgentPreset(name.to_owned())),
        }
    }
}

impl std::fmt::Debug for PublishedRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublishedRuntime")
            .field("runtime_revision", &self.result.snapshot.runtime_revision)
            .field("models", &self.result.snapshot.models.len())
            .field("agents", &self.result.snapshot.agents.len())
            .field("manifests", &self.manifests.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct RuntimePublication {
    pub runtime: Arc<PublishedRuntime>,
    pub notification: RuntimeChangedNotification,
}

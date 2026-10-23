//! One coherent immutable engine runtime publication.

mod agents;
pub(crate) mod projection;

use std::{collections::BTreeMap, sync::Arc};

use cookie_agent_models::CompiledModelRuntime;
use cookie_agent_protocol::{RuntimeChangedNotification, RuntimeSnapshotResult};

pub(crate) use agents::{
    AcpOffers, AgentRegistry, ModelVariables, ResolvedAgent, ResolvedAgentFallback, RuntimeAgent,
    configured_selection, delegation_targets, join_runtimes, selection_available,
};
pub(crate) use projection::build_runtime_snapshot;

/// The revisions a runtime was compiled from. Session history freezes
/// them; clients see only the catalog view's one opaque revision, which is
/// `runtime_revision`'s text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotRevisions {
    pub runtime_revision: cookie_agent_protocol::RuntimeRevision,
    pub recipe_registry_revision: cookie_agent_protocol::RecipeRegistryRevision,
    pub catalog_revision: cookie_agent_protocol::CatalogRevision,
    pub provider_state_revision: cookie_agent_protocol::ProviderStateRevision,
    pub model_revision: cookie_agent_protocol::ModelRevision,
    pub agent_revision: cookie_agent_protocol::AgentRevision,
    pub config_revision: cookie_agent_protocol::ConfigRevision,
}

/// Exact executable and wire state published as one atomic value.
#[derive(Clone)]
pub struct PublishedRuntime {
    pub result: RuntimeSnapshotResult,
    pub revisions: SnapshotRevisions,
    pub models: Arc<CompiledModelRuntime>,
    pub agents: Arc<AgentRegistry>,
    pub agent_presets: BTreeMap<String, Arc<AgentRegistry>>,
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
            .field("runtime_revision", &self.revisions.runtime_revision)
            .field("models", &self.result.snapshot.models.len())
            .field("agents", &self.result.snapshot.agents.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct RuntimePublication {
    pub runtime: Arc<PublishedRuntime>,
    pub notification: RuntimeChangedNotification,
}

//! Coherent protocol-9 runtime snapshot state machine.

use cookie_agent_protocol::{
    CatalogSource, RuntimeChangedNotification, RuntimeRevision, RuntimeSnapshotV1,
};

pub const EMPTY_RUNTIME_GUIDANCE: &str = "type /connect to continue";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimePhase {
    Loading,
    Empty,
    Ready,
    Stale,
    Bootstrap,
    ErrorRetry,
}

#[derive(Clone, Debug)]
pub struct RuntimeState {
    snapshot: Option<RuntimeSnapshotV1>,
    phase: RuntimePhase,
    durable_explanation: Option<String>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            snapshot: None,
            phase: RuntimePhase::Loading,
            durable_explanation: None,
        }
    }
}

impl RuntimeState {
    pub fn snapshot(&self) -> Option<&RuntimeSnapshotV1> {
        self.snapshot.as_ref()
    }

    pub const fn phase(&self) -> RuntimePhase {
        self.phase
    }

    pub fn durable_explanation(&self) -> Option<&str> {
        self.durable_explanation.as_deref()
    }

    pub fn revision(&self) -> Option<&RuntimeRevision> {
        self.snapshot
            .as_ref()
            .map(|snapshot| &snapshot.runtime_revision)
    }

    pub fn is_empty(&self) -> bool {
        self.phase == RuntimePhase::Empty
    }

    pub fn install_initial(&mut self, snapshot: RuntimeSnapshotV1) {
        if self.snapshot.is_none() {
            self.install(snapshot);
        }
    }

    /// Install an operation response only if no newer runtime was installed
    /// while that operation was in flight.
    pub fn install_response(
        &mut self,
        baseline: Option<&RuntimeRevision>,
        snapshot: RuntimeSnapshotV1,
    ) -> bool {
        if self.revision() == Some(&snapshot.runtime_revision) {
            return false;
        }
        if self.revision() != baseline {
            return false;
        }
        self.install(snapshot);
        true
    }

    /// Runtime notifications form a predecessor-linked monotonic stream.
    /// Duplicate and stale/out-of-order notifications never replace the
    /// currently installed coherent snapshot.
    pub fn apply_notification(&mut self, changed: RuntimeChangedNotification) -> bool {
        if self.revision() == Some(&changed.snapshot.runtime_revision) {
            return false;
        }
        if let Some(current) = self.revision()
            && changed.previous_revision.as_ref() != Some(current)
        {
            return false;
        }
        self.install(changed.snapshot);
        true
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.durable_explanation = Some(error);
        if self.snapshot.is_none() {
            self.phase = RuntimePhase::ErrorRetry;
        }
    }

    fn install(&mut self, snapshot: RuntimeSnapshotV1) {
        let empty = snapshot.models.is_empty();
        let (phase, explanation) = if empty {
            (RuntimePhase::Empty, catalog_explanation(&snapshot))
        } else if snapshot.catalog_source == CatalogSource::Bootstrap {
            (
                RuntimePhase::Bootstrap,
                Some("Using bundled bootstrap catalog; provider and model availability may be limited.".into()),
            )
        } else if snapshot.catalog_state.stale {
            (RuntimePhase::Stale, catalog_explanation(&snapshot))
        } else {
            (RuntimePhase::Ready, None)
        };
        self.snapshot = Some(snapshot);
        self.phase = phase;
        self.durable_explanation = explanation;
    }
}

fn catalog_explanation(snapshot: &RuntimeSnapshotV1) -> Option<String> {
    if snapshot.catalog_source == CatalogSource::Bootstrap {
        return Some(
            "Using bundled bootstrap catalog; provider and model availability may be limited."
                .into(),
        );
    }
    snapshot.catalog_state.last_error.as_ref().map(|error| {
        format!(
            "Using stale catalog cache after {} at {}: {}",
            error.code, error.time, error.message
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revision<T>() -> T
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(serde_json::json!(format!("sha256:{}", "a".repeat(64))))
            .expect("revision")
    }

    fn snapshot(with_model: bool) -> RuntimeSnapshotV1 {
        let model = cookie_agent_protocol::AvailableModelDescriptor {
            key: "custom.test/model".parse().expect("model key"),
            display_name: "Model".to_owned(),
            capabilities: cookie_agent_protocol::ModelCapabilities {
                input: [cookie_agent_protocol::Modality::Text]
                    .into_iter()
                    .collect(),
                output: [cookie_agent_protocol::Modality::Text]
                    .into_iter()
                    .collect(),
                context_tokens: 4096,
                output_tokens: 1024,
                tool_calling: true,
                parallel_tool_calls: true,
                structured_output: false,
                reasoning: false,
                temperature: true,
                top_p: true,
                seed: true,
                native_replay: cookie_agent_protocol::ReplayCapability::Unsupported,
                cancellation: cookie_agent_protocol::CancellationCapability::LocalOnly,
                media: Default::default(),
            },
            variants: Vec::new(),
            variant_order: Vec::new(),
            default_variant: None,
            behavior_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"model"),
        };
        let selection = cookie_agent_protocol::ModelSelection {
            model: model.key.clone(),
            variant: None,
        };
        RuntimeSnapshotV1 {
            snapshot_schema_version: cookie_agent_protocol::RuntimeSnapshotSchemaVersion::current(),
            recipe_registry_revision: revision(),
            catalog_revision: revision(),
            catalog_source: CatalogSource::Network,
            catalog_state: cookie_agent_protocol::CatalogRuntimeState {
                stale: false,
                provider_quarantine_count: 0,
                model_quarantine_count: 0,
                quarantine_digest: cookie_agent_protocol::Sha256Digest::of_bytes(b"quarantine"),
                last_error: None,
            },
            provider_state_revision: revision(),
            provider_store_generation: cookie_agent_protocol::ProviderStoreGeneration::new(1)
                .expect("generation"),
            model_revision: revision(),
            agent_revision: revision(),
            runtime_revision: revision(),
            providers: Vec::new(),
            models: if with_model { vec![model] } else { Vec::new() },
            agents: if with_model {
                vec![cookie_agent_protocol::AgentDescriptor {
                    id: cookie_agent_protocol::AgentId::new("default").expect("agent ID"),
                    description: "Built-in default coding agent".to_owned(),
                    mode: cookie_agent_protocol::AgentMode::Primary,
                    enabled: true,
                    runnable_as_root: true,
                    resolved_fallback: vec![selection],
                    delegation_targets: Vec::new(),
                }]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn empty_guidance_is_exact() {
        assert_eq!(
            EMPTY_RUNTIME_GUIDANCE.as_bytes(),
            b"type /connect to continue"
        );
    }

    #[test]
    fn models_with_synthetic_agent_are_not_empty() {
        let mut state = RuntimeState::default();
        state.install_initial(snapshot(true));
        assert_eq!(state.phase(), RuntimePhase::Ready);
    }

    #[test]
    fn runtime_without_available_models_is_empty() {
        let mut state = RuntimeState::default();
        state.install_initial(snapshot(false));
        assert_eq!(state.phase(), RuntimePhase::Empty);
    }
}

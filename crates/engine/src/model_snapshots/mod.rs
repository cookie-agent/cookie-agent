//! Durable-before-reference model manifest integration.

use std::sync::Arc;

use cookie_agent_models::{
    CompiledModelRuntime,
    manifests::{
        ManifestError, ModelSnapshotManifestIndex, ModelSnapshotManifestStore, RehydrationError,
        behavior_fingerprint, frozen_binding, selection_fingerprint,
    },
};
use cookie_agent_protocol::{FrozenModelBinding, ModelSelection};

use crate::EngineError;

#[derive(Debug)]
pub(crate) struct RuntimeManifest {
    pub manifest: Arc<cookie_agent_protocol::ModelSnapshotManifestV1>,
    pub index: Arc<ModelSnapshotManifestIndex>,
}

pub(crate) fn prepare_runtime_manifest(
    store: &ModelSnapshotManifestStore,
    runtime: &CompiledModelRuntime,
) -> Result<RuntimeManifest, EngineError> {
    let manifest = store.write(runtime.manifest_payload()?)?;
    let index = Arc::new(store.scan()?);
    Ok(RuntimeManifest { manifest, index })
}

pub(crate) fn binding_for_selection(
    manifest: &cookie_agent_protocol::ModelSnapshotManifestV1,
    _runtime: &CompiledModelRuntime,
    selection: &ModelSelection,
) -> Result<FrozenModelBinding, EngineError> {
    let blueprint = manifest
        .payload
        .blueprints
        .iter()
        .find(|blueprint| blueprint.selection.model == selection.model)
        .ok_or(EngineError::NoRunnableModel)?;
    if selection
        .variant
        .as_ref()
        .is_some_and(|variant| !blueprint.variants.iter().any(|value| &value.id == variant))
    {
        return Err(EngineError::NoRunnableModel);
    }
    if cookie_agent_models::adapters::wire_adapter_for_protocol(blueprint.protocol_recipe.as_str())
        .is_none()
    {
        return Err(EngineError::RuntimeCompileFailed);
    }
    frozen_binding(manifest.revision.clone(), blueprint, selection.clone())
        .map_err(EngineError::from)
}

pub(crate) fn validate_referenced_binding(
    index: &ModelSnapshotManifestIndex,
    _runtime: &CompiledModelRuntime,
    binding: &FrozenModelBinding,
) -> Result<(), EngineError> {
    if cookie_agent_models::adapters::wire_adapter_for_protocol(binding.protocol_recipe.as_str())
        .is_none()
    {
        return Err(EngineError::RuntimeCompileFailed);
    }
    let manifest = index.require(&binding.manifest_revision).map_err(|_| {
        EngineError::SnapshotRehydration(RehydrationError::SnapshotRehydrationMismatch)
    })?;
    let blueprint = manifest
        .payload
        .blueprints
        .iter()
        .find(|blueprint| blueprint.blueprint_fingerprint == binding.blueprint_fingerprint)
        .ok_or(EngineError::SnapshotRehydration(
            RehydrationError::SnapshotRehydrationMismatch,
        ))?;
    if !binding.matches_blueprint(blueprint)
        || behavior_fingerprint(blueprint, &binding.selection)
            .map_or(true, |value| value != binding.behavior_fingerprint)
        || selection_fingerprint(blueprint, &binding.selection)
            .map_or(true, |value| value != binding.selection_fingerprint)
    {
        return Err(EngineError::SnapshotRehydration(
            RehydrationError::SnapshotRehydrationMismatch,
        ));
    }
    Ok(())
}

impl From<ManifestError> for EngineError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

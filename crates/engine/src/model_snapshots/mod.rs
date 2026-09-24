//! The current runtime's in-memory model manifest and run bindings.
//!
//! Bindings are frozen against the manifest of the runtime a run was admitted
//! with and recorded in its events as a description of what ran. Nothing is
//! persisted or rehydrated: a later run resolves its models afresh.

use std::sync::Arc;

use cookie_agent_models::{
    CompiledModelRuntime,
    manifests::{ManifestError, build_manifest, frozen_binding},
};
use cookie_agent_protocol::{FrozenModelBinding, ModelSelection};

use crate::EngineError;

pub(crate) fn prepare_runtime_manifest(
    runtime: &CompiledModelRuntime,
) -> Result<Arc<cookie_agent_protocol::ModelSnapshotManifestV1>, EngineError> {
    Ok(build_manifest(runtime.manifest_payload()?)?)
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

impl From<ManifestError> for EngineError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

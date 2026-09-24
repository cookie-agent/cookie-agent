//! In-memory model manifests, schema 1: the canonical, fingerprinted form of
//! the current runtime's models that run bindings are frozen against. Nothing
//! here is persisted.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use cookie_agent_identity::ModelSnapshotRevision;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::secure_store::SecureStoreError;

pub use cookie_agent_protocol::{
    CompiledSafeModelBlueprint, FrozenAuthParameterValue, FrozenCredentialBinding,
    FrozenCredentialSource, FrozenModelBinding, FrozenProviderOptions, FrozenProviderSource,
    FrozenRequestDefaults, FrozenResolvedRequestDefaults, FrozenSetupBinding,
    FrozenVariantBlueprint, HeaderName, ModelSnapshotManifestSchemaVersion,
    ModelSnapshotManifestV1, ModelSnapshotPayloadV1, NormalizedDecimal, SafeEndpointIdentity,
    SafeStaticHeaderValue, Sha256Digest,
};

/// Hard per-manifest byte limit.
pub const MODEL_SNAPSHOT_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Hard direct matching-file limit.
pub const MODEL_SNAPSHOT_MAX_FILES: usize = 4096;

const MAX_IJSON_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_ITEMS: usize = 1_000_000;

/// Canonicalizes and validates a payload into the current in-memory manifest.
/// Nothing is persisted: runs resolve their models from the live runtime, and
/// events keep their bindings only as a record of what ran.
pub fn build_manifest(
    mut payload: ModelSnapshotPayloadV1,
) -> Result<Arc<ModelSnapshotManifestV1>, ManifestError> {
    normalize_payload(&mut payload)?;
    validate_payload(&payload)?;
    let canonical = canonical_payload_bytes(&payload)?;
    if canonical.len() as u64 > MODEL_SNAPSHOT_MAX_BYTES {
        return Err(ManifestError::InvalidModelSnapshotManifest);
    }
    let digest = sha256_hex(&canonical);
    let revision = ModelSnapshotRevision::new(format!("sha256:{digest}"))
        .map_err(|_| ManifestError::InvalidModelSnapshotManifest)?;
    Ok(Arc::new(ModelSnapshotManifestV1 {
        schema_version: ModelSnapshotManifestSchemaVersion::current(),
        revision,
        payload,
    }))
}

/// Returns exact RFC-8785 JCS bytes for an integer-only payload.
pub fn canonical_payload_bytes(payload: &ModelSnapshotPayloadV1) -> Result<Vec<u8>, ManifestError> {
    let value =
        serde_json::to_value(payload).map_err(|_| ManifestError::InvalidModelSnapshotManifest)?;
    validate_json_value(&value, 0, &mut 0)?;
    let mut output = Vec::new();
    write_jcs(&value, &mut output)?;
    Ok(output)
}

/// Computes the schema-1 blueprint fingerprint over every field except itself.
pub fn blueprint_fingerprint(
    blueprint: &CompiledSafeModelBlueprint,
) -> Result<Sha256Digest, ManifestError> {
    let mut value =
        serde_json::to_value(blueprint).map_err(|_| ManifestError::InvalidModelSnapshotManifest)?;
    value
        .as_object_mut()
        .ok_or(ManifestError::InvalidModelSnapshotManifest)?
        .remove("blueprint_fingerprint");
    validate_json_value(&value, 0, &mut 0)?;
    let mut canonical = Vec::new();
    write_jcs(&value, &mut canonical)?;
    let mut hasher = Sha256::new();
    hasher.update(b"cookie-agent/model-blueprint/v1\0");
    hasher.update(canonical);
    Sha256Digest::new(format!("{:x}", hasher.finalize()))
        .map_err(|_| ManifestError::InvalidModelSnapshotManifest)
}

pub struct FrozenBehaviorRef<'a> {
    pub descriptor: &'a oven_sdk::LanguageModelDescriptor,
    pub defaults: &'a FrozenResolvedRequestDefaults,
    pub options: &'a FrozenProviderOptions,
    pub static_headers: &'a BTreeMap<HeaderName, SafeStaticHeaderValue>,
    pub behavior_fingerprint: &'a Sha256Digest,
    pub selection_fingerprint: &'a Sha256Digest,
}

#[must_use]
pub fn selected_behavior<'a>(
    blueprint: &'a CompiledSafeModelBlueprint,
    selection: &cookie_agent_identity::ModelSelection,
) -> Option<FrozenBehaviorRef<'a>> {
    if selection.model != blueprint.selection.model {
        return None;
    }
    match selection.variant.as_ref() {
        None => Some(FrozenBehaviorRef {
            descriptor: &blueprint.descriptor,
            defaults: &blueprint.defaults,
            options: &blueprint.options,
            static_headers: &blueprint.static_headers,
            behavior_fingerprint: &blueprint.behavior_fingerprint,
            selection_fingerprint: &blueprint.selection_fingerprint,
        }),
        Some(id) => blueprint
            .variants
            .iter()
            .find(|variant| &variant.id == id)
            .map(|variant| FrozenBehaviorRef {
                descriptor: &variant.descriptor,
                defaults: &variant.defaults,
                options: &variant.options,
                static_headers: &variant.static_headers,
                behavior_fingerprint: &variant.behavior_fingerprint,
                selection_fingerprint: &variant.selection_fingerprint,
            }),
    }
}

pub fn behavior_fingerprint(
    blueprint: &CompiledSafeModelBlueprint,
    selection: &cookie_agent_identity::ModelSelection,
) -> Result<Sha256Digest, ManifestError> {
    let behavior = selected_behavior(blueprint, selection)
        .ok_or(ManifestError::InvalidModelSnapshotManifest)?;
    hash_canonical(
        b"cookie-agent/model-behavior/v1\0",
        &serde_json::json!({
            "selection": selection,
            "source": blueprint.source,
            "config_override_fingerprint": blueprint.config_override_fingerprint,
            "setup_binding": blueprint.setup_binding,
            "credential_binding": blueprint.credential_binding,
            "endpoint_identity": blueprint.endpoint_identity,
            "provider_recipe": blueprint.provider_recipe,
            "protocol_recipe": blueprint.protocol_recipe,
            "setup_recipe": blueprint.setup_recipe,
            "auth_method": blueprint.auth_method,
            "compiler_version": blueprint.compiler_version,
            "descriptor": behavior.descriptor,
            "defaults": behavior.defaults,
            "options": behavior.options,
            "static_headers": behavior.static_headers,
        }),
    )
}

pub fn selection_fingerprint(
    blueprint: &CompiledSafeModelBlueprint,
    selection: &cookie_agent_identity::ModelSelection,
) -> Result<Sha256Digest, ManifestError> {
    let behavior = selected_behavior(blueprint, selection)
        .ok_or(ManifestError::InvalidModelSnapshotManifest)?;
    hash_canonical(
        b"cookie-agent/model-selection/v1\0",
        &serde_json::json!({
            "selection": selection,
            "descriptor": behavior.descriptor,
            "defaults": behavior.defaults,
            "options": behavior.options,
            "behavior_fingerprint": behavior.behavior_fingerprint,
        }),
    )
}

pub fn frozen_binding(
    manifest_revision: ModelSnapshotRevision,
    blueprint: &CompiledSafeModelBlueprint,
    selection: cookie_agent_identity::ModelSelection,
) -> Result<FrozenModelBinding, ManifestError> {
    let behavior = selected_behavior(blueprint, &selection)
        .ok_or(ManifestError::InvalidModelSnapshotManifest)?;
    let binding = FrozenModelBinding {
        manifest_revision,
        blueprint_fingerprint: blueprint.blueprint_fingerprint.clone(),
        selection,
        source: blueprint.source.clone(),
        config_override_fingerprint: blueprint.config_override_fingerprint.clone(),
        credential_binding: blueprint.credential_binding.clone(),
        setup_binding: blueprint.setup_binding.clone(),
        endpoint_identity: blueprint.endpoint_identity.clone(),
        provider_recipe: blueprint.provider_recipe.clone(),
        protocol_recipe: blueprint.protocol_recipe.clone(),
        setup_recipe: blueprint.setup_recipe.clone(),
        compiler_version: blueprint.compiler_version.clone(),
        descriptor: behavior.descriptor.clone(),
        defaults: behavior.defaults.clone(),
        options: behavior.options.clone(),
        static_headers: behavior.static_headers.clone(),
        behavior_fingerprint: behavior.behavior_fingerprint.clone(),
        selection_fingerprint: behavior.selection_fingerprint.clone(),
    };
    binding
        .validate()
        .map_err(|_| ManifestError::InvalidModelSnapshotManifest)?;
    Ok(binding)
}

fn hash_canonical(domain: &[u8], value: &Value) -> Result<Sha256Digest, ManifestError> {
    validate_json_value(value, 0, &mut 0)?;
    let mut canonical = Vec::new();
    write_jcs(value, &mut canonical)?;
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(canonical);
    Sha256Digest::new(format!("{:x}", hasher.finalize()))
        .map_err(|_| ManifestError::InvalidModelSnapshotManifest)
}

pub(crate) fn canonical_state_fingerprint(
    domain: &[u8],
    value: &impl serde::Serialize,
) -> Result<crate::Sha256Digest, ManifestError> {
    let value =
        serde_json::to_value(value).map_err(|_| ManifestError::InvalidModelSnapshotManifest)?;
    let digest = hash_canonical(domain, &value)?;
    crate::Sha256Digest::new(digest.as_str().to_owned())
        .map_err(|_| ManifestError::InvalidModelSnapshotManifest)
}

fn normalize_payload(payload: &mut ModelSnapshotPayloadV1) -> Result<(), ManifestError> {
    for blueprint in &mut payload.blueprints {
        blueprint
            .credential_binding
            .fields
            .sort_by(|left, right| left.as_str().cmp(right.as_str()));
        blueprint.credential_binding.owned_headers.sort();
        blueprint
            .variants
            .sort_by(|left, right| left.id.cmp(&right.id));
        let base_selection = blueprint.selection.clone();
        blueprint.behavior_fingerprint = behavior_fingerprint(blueprint, &base_selection)?;
        blueprint.selection_fingerprint = selection_fingerprint(blueprint, &base_selection)?;
        for index in 0..blueprint.variants.len() {
            let selection = cookie_agent_identity::ModelSelection {
                model: blueprint.selection.model.clone(),
                variant: Some(blueprint.variants[index].id.clone()),
            };
            let behavior = behavior_fingerprint(blueprint, &selection)?;
            blueprint.variants[index].behavior_fingerprint = behavior;
            let selection_fingerprint = selection_fingerprint(blueprint, &selection)?;
            blueprint.variants[index].selection_fingerprint = selection_fingerprint;
        }
        blueprint.blueprint_fingerprint = blueprint_fingerprint(blueprint)?;
    }
    payload.blueprints.sort_by(|left, right| {
        left.selection
            .model
            .cmp(&right.selection.model)
            .then_with(|| left.selection.variant.cmp(&right.selection.variant))
    });
    Ok(())
}

fn validate_payload(payload: &ModelSnapshotPayloadV1) -> Result<(), ManifestError> {
    if payload.blueprints.len() > MODEL_SNAPSHOT_MAX_FILES {
        return Err(ManifestError::InvalidModelSnapshotManifest);
    }
    let mut models = BTreeSet::new();
    let mut fingerprints = BTreeSet::new();
    for blueprint in &payload.blueprints {
        if !models.insert(blueprint.selection.model.clone())
            || !fingerprints.insert(blueprint.blueprint_fingerprint.as_str().to_owned())
            || blueprint.credential_binding.fields.len() > 32
            || blueprint
                .credential_binding
                .fields
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || blueprint.credential_binding.owned_headers.len() > 32
            || blueprint
                .credential_binding
                .owned_headers
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || blueprint.selection.variant.is_some()
            || blueprint.setup_recipe != blueprint.setup_binding.setup_recipe
            || blueprint.auth_method != blueprint.credential_binding.auth_method
            || matches!(
                &blueprint.source,
                FrozenProviderSource::Managed { provider_recipe, .. }
                    if provider_recipe != &blueprint.provider_recipe
            )
            || blueprint.descriptor.identity.provider_id.as_str()
                != blueprint.selection.model.provider_id().as_str()
            || blueprint.descriptor.identity.model_id.as_str()
                != blueprint.selection.model.model_id().as_str()
            || blueprint.defaults.validate().is_err()
            || blueprint.variants.len() > 256
            || blueprint
                .variants
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
            || blueprint.variants.iter().any(|variant| {
                variant.descriptor.identity != blueprint.descriptor.identity
                    || variant.defaults.validate().is_err()
            })
            || behavior_fingerprint(blueprint, &blueprint.selection)?
                != blueprint.behavior_fingerprint
            || selection_fingerprint(blueprint, &blueprint.selection)?
                != blueprint.selection_fingerprint
            || blueprint.variants.iter().any(|variant| {
                let selection = cookie_agent_identity::ModelSelection {
                    model: blueprint.selection.model.clone(),
                    variant: Some(variant.id.clone()),
                };
                behavior_fingerprint(blueprint, &selection)
                    .map_or(true, |value| value != variant.behavior_fingerprint)
                    || selection_fingerprint(blueprint, &selection)
                        .map_or(true, |value| value != variant.selection_fingerprint)
            })
            || blueprint_fingerprint(blueprint)? != blueprint.blueprint_fingerprint
        {
            return Err(ManifestError::InvalidModelSnapshotManifest);
        }
    }
    Ok(())
}

fn validate_json_value(
    value: &Value,
    depth: usize,
    items: &mut usize,
) -> Result<(), ManifestError> {
    if depth > MAX_JSON_DEPTH {
        return Err(ManifestError::InvalidModelSnapshotManifest);
    }
    *items = items
        .checked_add(1)
        .ok_or(ManifestError::InvalidModelSnapshotManifest)?;
    if *items > MAX_JSON_ITEMS {
        return Err(ManifestError::InvalidModelSnapshotManifest);
    }
    match value {
        Value::Number(number) => {
            let valid = number
                .as_i64()
                .is_some_and(|value| value.unsigned_abs() <= MAX_IJSON_INTEGER)
                || number
                    .as_u64()
                    .is_some_and(|value| value <= MAX_IJSON_INTEGER);
            if !valid {
                return Err(ManifestError::InvalidModelSnapshotManifest);
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_json_value(value, depth + 1, items)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_json_value(value, depth + 1, items)?;
            }
        }
        Value::String(_) => {}
        Value::Null | Value::Bool(_) => {}
    }
    Ok(())
}

fn write_jcs(value: &Value, output: &mut Vec<u8>) -> Result<(), ManifestError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => output.extend_from_slice(number.to_string().as_bytes()),
        Value::String(value) => output.extend_from_slice(
            serde_json::to_string(value)
                .map_err(|_| ManifestError::InvalidModelSnapshotManifest)?
                .as_bytes(),
        ),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_jcs(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| compare_utf16(left, right));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(
                    serde_json::to_string(key)
                        .map_err(|_| ManifestError::InvalidModelSnapshotManifest)?
                        .as_bytes(),
                );
                output.push(b':');
                write_jcs(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn compare_utf16(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("invalid_model_snapshot_manifest")]
    InvalidModelSnapshotManifest,
    #[error("model_snapshot_digest_mismatch")]
    ModelSnapshotDigestMismatch,
    #[error("missing_model_snapshot_manifest")]
    MissingModelSnapshotManifest,
    #[error("model snapshot storage failed")]
    Storage(#[from] SecureStoreError),
}

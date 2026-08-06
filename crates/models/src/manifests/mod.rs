//! Secure project model-snapshot manifests, schema 1.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use cookie_agent_identity::{ModelSnapshotRevision, ProviderId};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    ProviderDefinition,
    provider_store::ProviderStoreSnapshot,
    secure_store::{SecureDirectory, SecureDirectoryLock, SecureStoreError},
};

pub use cookie_agent_protocol::{
    CompiledSafeModelBlueprint, FrozenAuthParameterValue, FrozenCredentialBinding,
    FrozenCredentialSource, FrozenModelBinding, FrozenProviderOptions, FrozenProviderSource,
    FrozenRequestDefaults, FrozenResolvedRequestDefaults, FrozenSetupBinding,
    FrozenVariantBlueprint, HeaderName, ModelSnapshotManifestSchemaVersion,
    ModelSnapshotManifestV1, ModelSnapshotPayloadV1, NormalizedDecimal, SafeEndpointIdentity,
    SafeStaticHeaderValue, Sha256Digest,
};

/// Fixed project manifest directory below the exact current working directory.
pub const MODEL_SNAPSHOT_DIRECTORY: &str = ".cookie-agent/model-snapshots";
/// Fixed cross-process manifest lock.
pub const MODEL_SNAPSHOT_LOCK_FILE: &str = "model-snapshots-v1.lock";
/// Hard per-manifest byte limit.
pub const MODEL_SNAPSHOT_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Hard direct matching-file limit.
pub const MODEL_SNAPSHOT_MAX_FILES: usize = 4096;

const MAX_IJSON_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_ITEMS: usize = 1_000_000;

/// Secure handle for one exact-cwd manifest directory.
#[derive(Debug)]
pub struct ModelSnapshotManifestStore {
    directory: SecureDirectory,
}

impl ModelSnapshotManifestStore {
    /// Opens the fixed project directory below an exact cwd.
    pub fn open(cwd: impl AsRef<Path>) -> Result<Self, ManifestError> {
        Ok(Self {
            directory: SecureDirectory::open_in_untrusted_project_anchor(
                cwd,
                MODEL_SNAPSHOT_DIRECTORY,
            )?,
        })
    }

    /// Opens an explicit private directory, primarily for deterministic tests.
    pub fn open_directory(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        Ok(Self {
            directory: SecureDirectory::open(path)?,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    /// Scans all direct schema-1 manifest filenames in sorted byte order.
    pub fn scan(&self) -> Result<ModelSnapshotManifestIndex, ManifestError> {
        let lock = self.directory.lock(MODEL_SNAPSHOT_LOCK_FILE)?;
        scan_locked(&self.directory, &lock)
    }

    /// Canonicalizes, validates, and durably installs a payload before returning its reference.
    pub fn write(
        &self,
        payload: ModelSnapshotPayloadV1,
    ) -> Result<Arc<ModelSnapshotManifestV1>, ManifestError> {
        Ok(self.prepare(payload)?.manifest)
    }

    /// Validates the complete existing index before any mutation, then installs one manifest.
    pub fn prepare(
        &self,
        mut payload: ModelSnapshotPayloadV1,
    ) -> Result<PreparedModelSnapshotManifest, ManifestError> {
        normalize_payload(&mut payload)?;
        validate_payload(&payload)?;
        let canonical = canonical_payload_bytes(&payload)?;
        if canonical.len() as u64 > MODEL_SNAPSHOT_MAX_BYTES {
            return Err(ManifestError::InvalidModelSnapshotManifest);
        }
        let digest = sha256_hex(&canonical);
        let revision = ModelSnapshotRevision::new(format!("sha256:{digest}"))
            .map_err(|_| ManifestError::InvalidModelSnapshotManifest)?;
        let manifest = Arc::new(ModelSnapshotManifestV1 {
            schema_version: ModelSnapshotManifestSchemaVersion::current(),
            revision,
            payload,
        });
        let bytes = serde_json::to_vec_pretty(manifest.as_ref())
            .map_err(|_| ManifestError::InvalidModelSnapshotManifest)?;
        if bytes.len() as u64 > MODEL_SNAPSHOT_MAX_BYTES {
            return Err(ManifestError::InvalidModelSnapshotManifest);
        }
        let name = format!("{digest}.json");
        let lock = self.directory.lock(MODEL_SNAPSHOT_LOCK_FILE)?;
        let mut index = scan_locked(&self.directory, &lock)?;
        if let Some(existing) = index.get(&manifest.revision).cloned() {
            if existing.as_ref() != manifest.as_ref() {
                return Err(ManifestError::ModelSnapshotDigestMismatch);
            }
            return Ok(PreparedModelSnapshotManifest {
                manifest: existing,
                index,
            });
        }
        if index.len() >= MODEL_SNAPSHOT_MAX_FILES {
            return Err(ManifestError::InvalidModelSnapshotManifest);
        }
        lock.atomic_replace(&name, &bytes)?;
        index
            .manifests
            .insert(manifest.revision.clone(), Arc::clone(&manifest));
        Ok(PreparedModelSnapshotManifest { manifest, index })
    }
}

#[derive(Clone, Debug)]
pub struct PreparedModelSnapshotManifest {
    pub manifest: Arc<ModelSnapshotManifestV1>,
    pub index: ModelSnapshotManifestIndex,
}

fn scan_locked(
    directory: &SecureDirectory,
    lock: &SecureDirectoryLock<'_>,
) -> Result<ModelSnapshotManifestIndex, ManifestError> {
    let mut names = direct_manifest_names(directory)?;
    if names.len() > MODEL_SNAPSHOT_MAX_FILES {
        return Err(ManifestError::InvalidModelSnapshotManifest);
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let mut manifests = BTreeMap::new();
    for name in names {
        let bytes = lock
            .read(&name, MODEL_SNAPSHOT_MAX_BYTES)?
            .ok_or(ManifestError::InvalidModelSnapshotManifest)?;
        let manifest = Arc::new(decode_and_verify(&name, &bytes)?);
        if manifests
            .insert(manifest.revision.clone(), manifest)
            .is_some()
        {
            return Err(ManifestError::InvalidModelSnapshotManifest);
        }
    }
    Ok(ModelSnapshotManifestIndex { manifests })
}

/// Immutable validated manifest index. Schema 1 performs no automatic GC.
#[derive(Clone, Debug, Default)]
pub struct ModelSnapshotManifestIndex {
    manifests: BTreeMap<ModelSnapshotRevision, Arc<ModelSnapshotManifestV1>>,
}

impl ModelSnapshotManifestIndex {
    #[must_use]
    pub fn get(&self, revision: &ModelSnapshotRevision) -> Option<&Arc<ModelSnapshotManifestV1>> {
        self.manifests.get(revision)
    }

    pub fn require(
        &self,
        revision: &ModelSnapshotRevision,
    ) -> Result<Arc<ModelSnapshotManifestV1>, ManifestError> {
        self.get(revision)
            .cloned()
            .ok_or(ManifestError::MissingModelSnapshotManifest)
    }

    pub fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (&ModelSnapshotRevision, &Arc<ModelSnapshotManifestV1>)>
    {
        self.manifests.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.manifests.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.manifests.is_empty()
    }
}

/// A successfully checked retained safe blueprint and its exact credential category.
#[derive(Clone, Debug)]
pub struct RehydratedBlueprint {
    pub manifest: Arc<ModelSnapshotManifestV1>,
    pub blueprint: CompiledSafeModelBlueprint,
}

impl ModelSnapshotManifestIndex {
    /// Performs exact source/config/store checks without source substitution.
    pub fn rehydrate(
        &self,
        binding: &FrozenModelBinding,
        authored: &BTreeMap<ProviderId, ProviderDefinition>,
        store: &ProviderStoreSnapshot,
        config_fingerprint: impl Fn(&ProviderId, &ProviderDefinition) -> crate::Sha256Digest,
    ) -> Result<RehydratedBlueprint, RehydrationError> {
        let manifest = self
            .require(&binding.manifest_revision)
            .map_err(|_| RehydrationError::SnapshotRehydrationMismatch)?;
        let blueprint = manifest
            .payload
            .blueprints
            .iter()
            .find(|blueprint| blueprint.blueprint_fingerprint == binding.blueprint_fingerprint)
            .cloned()
            .ok_or(RehydrationError::SnapshotRehydrationMismatch)?;
        if !binding.matches_blueprint(&blueprint)
            || behavior_fingerprint(&blueprint, &binding.selection)
                .map_or(true, |value| value != binding.behavior_fingerprint)
            || selection_fingerprint(&blueprint, &binding.selection)
                .map_or(true, |value| value != binding.selection_fingerprint)
        {
            return Err(RehydrationError::SnapshotRehydrationMismatch);
        }
        let provider_id = blueprint.selection.model.provider_id();

        if let FrozenProviderSource::Managed {
            provider_recipe, ..
        } = &blueprint.source
        {
            let recipe = crate::recipes::family_registry()
                .by_npm(match &blueprint.source {
                    FrozenProviderSource::Managed { package_claim, .. } => package_claim,
                    FrozenProviderSource::Custom { .. } => unreachable!(),
                })
                .filter(|recipe| managed_recipe_matches_blueprint(recipe, &provider_id, &blueprint))
                .ok_or(RehydrationError::UnsupportedSnapshotRecipe)?;
            if recipe.family.id() != provider_recipe.as_str()
                || recipe.family.id() != blueprint.provider_recipe.as_str()
            {
                return Err(RehydrationError::UnsupportedSnapshotRecipe);
            }
        }
        if !credential_shape_is_supported(&blueprint) {
            return Err(RehydrationError::SnapshotRehydrationMismatch);
        }

        match (&blueprint.source, blueprint.credential_binding.source) {
            (
                FrozenProviderSource::Custom {
                    safe_definition_fingerprint,
                },
                source,
            ) => {
                if !matches!(
                    source,
                    FrozenCredentialSource::AuthoredApiKey
                        | FrozenCredentialSource::AuthoredOverride
                        | FrozenCredentialSource::NoAuth
                ) {
                    return Err(RehydrationError::SnapshotConfigMismatch);
                }
                let definition = authored
                    .get(&provider_id)
                    .ok_or(RehydrationError::SnapshotConfigMismatch)?;
                if !matches!(definition, ProviderDefinition::Custom(_))
                    || config_fingerprint(&provider_id, definition).as_str()
                        != safe_definition_fingerprint.as_str()
                    || config_fingerprint(&provider_id, definition).as_str()
                        != blueprint.config_override_fingerprint.as_str()
                    || !custom_auth_shape_matches(definition, &blueprint)
                {
                    return Err(RehydrationError::SnapshotConfigMismatch);
                }
            }
            (FrozenProviderSource::Managed { .. }, FrozenCredentialSource::ProviderStore) => {
                let connection = store
                    .provider(&provider_id)
                    .ok_or(RehydrationError::SnapshotCredentialsUnavailable)?;
                if crate::manager::retained_family_match(&provider_id, connection)
                    != crate::manager::RetainedFamilyMatch::SupportedRemoved
                    || connection.policy.compiler_version != blueprint.compiler_version
                    || connection.policy.setup_recipe != blueprint.setup_recipe
                    || connection.setup_fingerprint.as_str()
                        != blueprint.setup_binding.setup_fingerprint.as_str()
                    || !managed_source_auth_matches(
                        connection.auth_method.as_str(),
                        connection
                            .credential_fields()
                            .map(cookie_agent_identity::AuthFieldName::as_str),
                        &blueprint,
                    )
                {
                    return Err(RehydrationError::SnapshotCredentialsUnavailable);
                }
            }
            (
                FrozenProviderSource::Managed { .. },
                FrozenCredentialSource::AuthoredApiKey | FrozenCredentialSource::AuthoredOverride,
            ) => {
                let definition = authored
                    .get(&provider_id)
                    .ok_or(RehydrationError::SnapshotConfigMismatch)?;
                if !matches!(definition, ProviderDefinition::ModelsDev(_))
                    || config_fingerprint(&provider_id, definition).as_str()
                        != blueprint.config_override_fingerprint.as_str()
                    || !authored_shape_matches(definition, &blueprint)
                {
                    return Err(RehydrationError::SnapshotConfigMismatch);
                }
            }
            (FrozenProviderSource::Managed { .. }, FrozenCredentialSource::NoAuth) => {
                if !blueprint.credential_binding.fields.is_empty()
                    || crate::recipes::auth_method(blueprint.auth_method.as_str())
                        .is_none_or(|method| !method.credentials.is_empty())
                {
                    return Err(RehydrationError::SnapshotRehydrationMismatch);
                }
            }
        }

        Ok(RehydratedBlueprint {
            manifest,
            blueprint,
        })
    }
}

fn managed_recipe_matches_blueprint(
    recipe: &crate::recipes::FamilyRecipe,
    _provider_id: &ProviderId,
    blueprint: &CompiledSafeModelBlueprint,
) -> bool {
    let FrozenProviderSource::Managed {
        provider_recipe,
        recipe_fingerprint,
        package_claim,
        ..
    } = &blueprint.source
    else {
        return false;
    };
    let expected_package = recipe.npm;
    let current_recipe_fingerprint =
        crate::manager::retained_recipe_fingerprint(recipe, blueprint.auth_method.as_str()).ok();
    provider_recipe.as_str() == recipe.family.id()
        && blueprint.provider_recipe.as_str() == recipe.family.id()
        && blueprint.setup_recipe.as_str() == "family-derived-setup-v1"
        && blueprint.setup_binding.setup_recipe.as_str() == "family-derived-setup-v1"
        && blueprint.protocol_recipe.as_str() == blueprint.descriptor.adapter_id.as_str()
        && recipe
            .allowed_auth_methods
            .contains(&blueprint.auth_method.as_str())
        && blueprint.credential_binding.auth_method == blueprint.auth_method
        && blueprint.compiler_version.as_str() == crate::recipes::COMPILER_VERSION
        && package_claim == expected_package
        && current_recipe_fingerprint
            .is_some_and(|fingerprint| fingerprint.as_str() == recipe_fingerprint.as_str())
        && blueprint.static_headers.is_empty()
}

fn credential_shape_is_supported(blueprint: &CompiledSafeModelBlueprint) -> bool {
    let Some(method) = crate::recipes::auth_method(blueprint.auth_method.as_str()) else {
        return false;
    };
    let expected_fields = method
        .credentials
        .iter()
        .map(|field| field.name)
        .collect::<Vec<_>>();
    if blueprint
        .credential_binding
        .fields
        .iter()
        .map(cookie_agent_identity::AuthFieldName::as_str)
        .ne(expected_fields)
    {
        return false;
    }
    let mut expected_headers = method
        .owned_headers
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if method.id == "api-key-header-v1" {
        let Some(header) = blueprint
            .credential_binding
            .parameters
            .iter()
            .find(|(name, _)| name.as_str() == "header_name")
            .map(|(_, value)| value.as_str())
        else {
            return false;
        };
        if blueprint.credential_binding.parameters.len() != 1 {
            return false;
        }
        expected_headers.insert(header);
    } else if !blueprint.credential_binding.parameters.is_empty() {
        return false;
    }
    blueprint
        .credential_binding
        .owned_headers
        .iter()
        .map(HeaderName::as_str)
        .eq(expected_headers)
}

fn custom_auth_shape_matches(
    definition: &ProviderDefinition,
    blueprint: &CompiledSafeModelBlueprint,
) -> bool {
    let ProviderDefinition::Custom(provider) = definition else {
        return false;
    };
    provider.auth.method == blueprint.auth_method
        && provider
            .auth
            .values
            .keys()
            .map(cookie_agent_identity::AuthFieldName::as_str)
            .eq(blueprint
                .credential_binding
                .fields
                .iter()
                .map(cookie_agent_identity::AuthFieldName::as_str))
        && provider
            .auth
            .parameters
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .eq(blueprint
                .credential_binding
                .parameters
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())))
}

fn authored_shape_matches(
    definition: &ProviderDefinition,
    blueprint: &CompiledSafeModelBlueprint,
) -> bool {
    let ProviderDefinition::ModelsDev(provider) = definition else {
        return false;
    };
    match blueprint.credential_binding.source {
        FrozenCredentialSource::AuthoredApiKey => {
            provider.api_key.is_some()
                && provider.auth_override.is_none()
                && blueprint.credential_binding.fields.len() == 1
                && blueprint.credential_binding.fields[0].as_str() == "api_key"
        }
        FrozenCredentialSource::AuthoredOverride => {
            provider.auth_override.as_ref().is_some_and(|auth| {
                managed_source_auth_matches(
                    auth.method.as_str(),
                    auth.values
                        .keys()
                        .map(cookie_agent_identity::AuthFieldName::as_str),
                    blueprint,
                )
            })
        }
        FrozenCredentialSource::ProviderStore | FrozenCredentialSource::NoAuth => false,
    }
}

fn managed_source_auth_matches<'a>(
    source_method: &str,
    source_fields: impl Iterator<Item = &'a str>,
    blueprint: &CompiledSafeModelBlueprint,
) -> bool {
    let FrozenProviderSource::Managed { package_claim, .. } = &blueprint.source else {
        return false;
    };
    let Some(recipe) = crate::recipes::family_registry().by_npm(package_claim) else {
        return false;
    };
    if crate::recipes::compatible_auth_method(source_method, recipe)
        != Some(blueprint.auth_method.as_str())
    {
        return false;
    }
    let source_fields = source_fields.collect::<BTreeSet<_>>();
    blueprint.credential_binding.fields.iter().all(|target| {
        crate::recipes::compatible_credential_field(source_method, target.as_str())
            .is_some_and(|source| source_fields.contains(source))
    })
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
            "static_headers": blueprint.static_headers,
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
        static_headers: blueprint.static_headers.clone(),
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

fn decode_and_verify(name: &str, bytes: &[u8]) -> Result<ModelSnapshotManifestV1, ManifestError> {
    let filename_digest =
        manifest_filename_digest(name).ok_or(ManifestError::InvalidModelSnapshotManifest)?;
    let strict: StrictValue =
        serde_json::from_slice(bytes).map_err(|_| ManifestError::InvalidModelSnapshotManifest)?;
    validate_json_value(&strict.0, 0, &mut 0)?;
    let manifest: ModelSnapshotManifestV1 = serde_json::from_value(strict.0)
        .map_err(|_| ManifestError::InvalidModelSnapshotManifest)?;
    validate_payload(&manifest.payload)?;
    let canonical = canonical_payload_bytes(&manifest.payload)?;
    let payload_digest = sha256_hex(&canonical);
    let revision_digest = manifest
        .revision
        .as_str()
        .strip_prefix("sha256:")
        .ok_or(ManifestError::ModelSnapshotDigestMismatch)?;
    if payload_digest != filename_digest || payload_digest != revision_digest {
        return Err(ManifestError::ModelSnapshotDigestMismatch);
    }
    Ok(manifest)
}

fn direct_manifest_names(directory: &SecureDirectory) -> Result<Vec<String>, ManifestError> {
    let clone = directory
        .directory
        .try_clone()
        .map_err(SecureStoreError::Io)?;
    let mut stream = rustix::fs::Dir::read_from(&clone).map_err(|error| {
        ManifestError::Storage(SecureStoreError::Io(std::io::Error::from(error)))
    })?;
    let mut names = Vec::new();
    for entry in &mut stream {
        let entry = entry.map_err(|error| {
            ManifestError::Storage(SecureStoreError::Io(std::io::Error::from(error)))
        })?;
        let bytes = entry.file_name().to_bytes();
        if matches!(bytes, b"." | b"..") {
            continue;
        }
        let Ok(name) = std::str::from_utf8(bytes) else {
            continue;
        };
        if manifest_filename_digest(name).is_some() {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

fn manifest_filename_digest(name: &str) -> Option<&str> {
    let digest = name.strip_suffix(".json")?;
    (digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(digest)
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

struct StrictValue(Value);

impl<'de> serde::Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Value;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("strict integer-only I-JSON")
            }
            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(Value::Bool(value))
            }
            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.unsigned_abs() > MAX_IJSON_INTEGER {
                    Err(E::custom("integer exceeds I-JSON safe range"))
                } else {
                    Ok(Value::Number(value.into()))
                }
            }
            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value > MAX_IJSON_INTEGER {
                    Err(E::custom("integer exceeds I-JSON safe range"))
                } else {
                    Ok(Value::Number(value.into()))
                }
            }
            fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Err(E::custom("floating JSON numbers are forbidden"))
            }
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(Value::String(value.to_owned()))
            }
            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(Value::String(value))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(Value::Null)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Value::Null)
            }
            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<StrictValue>()? {
                    values.push(value.0);
                }
                Ok(Value::Array(values))
            }
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, StrictValue>()? {
                    if values.insert(key, value.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                }
                Ok(Value::Object(values))
            }
        }
        deserializer.deserialize_any(Visitor).map(Self)
    }
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

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RehydrationError {
    #[error("snapshot_config_mismatch")]
    SnapshotConfigMismatch,
    #[error("snapshot_credentials_unavailable")]
    SnapshotCredentialsUnavailable,
    #[error("unsupported_snapshot_recipe")]
    UnsupportedSnapshotRecipe,
    #[error("snapshot_rehydration_mismatch")]
    SnapshotRehydrationMismatch,
}

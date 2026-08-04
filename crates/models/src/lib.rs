//! Provider-centric model configuration, immutable variants, and Oven adapters.

mod catalog;
mod credentials;
mod manager;
mod provider;
mod schema;

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use cookie_agent_identity::{ModelKey, ModelSelection, ProviderId, ProviderModelId, VariantId};
use oven_sdk::{
    AbortSignal, BoxFuture, CompactionRequest, CompactionResult, LanguageModel,
    LanguageModelDescriptor, ModelError, Request, StreamPart, StreamResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

pub use catalog::{
    Catalog, CatalogError, CatalogModel, CatalogModelCapabilities, CatalogModelLimits,
    CatalogModelModalities, CatalogModelStatus, CatalogProvider, CatalogReasoningOption,
    CatalogRecipe, CatalogSnapshot, MODELS_DEV_ARTIFACT_BYTES, MODELS_DEV_ARTIFACT_SHA256,
    MODELS_DEV_COMMIT, MODELS_DEV_FETCHED_AT, MODELS_DEV_SOURCE, UnsupportedReason,
};
pub use credentials::{
    CredentialConnectOutcome, CredentialConnectReceipt, CredentialConnectRequest,
    CredentialSnapshot, CredentialStore, CredentialStoreError,
};
pub use manager::{ModelSetManager, ModelSetManagerError, ModelSnapshot};
pub use provider::{
    AdaptorId, AuthDefinition, AuthFieldName, CancellationCapability, CompactionCapability,
    ExplicitModelConfig, ExplicitProvider, FiniteF32, HeaderName, MediaCapability, MediaKind,
    MimeType, Modality, ModelBuildError, ModelCapabilities, ModelsDevModelConfig,
    ModelsDevProvider, OpenResponsesMode, ProviderDefinition, ProviderOptions, ReasoningBehavior,
    ReasoningEffort, ReplayCapability, RequestDefaults, ResolvedRequestDefaults, SecretString,
    ToolChoice, VariantDirective, build_model_set,
};

/// Validated lowercase SHA-256 digest.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelSetError> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(ModelSetError::InvalidFingerprint)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn hash(domain: &str, value: &impl Serialize) -> Result<Self, serde_json::Error> {
        let mut hasher = Sha256::new();
        hasher.update(domain.as_bytes());
        hasher.update([0]);
        hasher.update(serde_json::to_vec(value)?);
        Ok(Self(format!("{:x}", hasher.finalize())))
    }
}

pub(crate) struct ConstructedAdapter {
    pub model: Arc<dyn LanguageModel>,
}

/// Origin of one enabled named variant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VariantOrigin {
    ModelsDevEffort,
    ModelsDevToggle,
    ModelsDevBudgetTokens,
    Explicit,
}

/// One fully compiled named behavior preset.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelVariant {
    pub id: VariantId,
    pub display_name: String,
    pub origin: VariantOrigin,
    pub defaults: ResolvedRequestDefaults,
    pub provider_options: ProviderOptions,
    pub behavior_fingerprint: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AvailableVariantDescriptor {
    pub id: VariantId,
    pub display_name: String,
    pub origin: VariantOrigin,
    pub behavior_fingerprint: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AvailableModelDescriptor {
    pub key: ModelKey,
    pub display_name: String,
    pub capabilities: ModelCapabilities,
    pub variants: Vec<AvailableVariantDescriptor>,
    pub default_variant: Option<VariantId>,
    pub behavior_fingerprint: Sha256Digest,
}

/// Safe exact selection identity retained in frozen state.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedModelRef {
    pub selection: ModelSelection,
    pub provider_id: ProviderId,
    pub model_id: ProviderModelId,
    pub adapter_id: AdaptorId,
    pub selection_fingerprint: Sha256Digest,
}

/// Serializable exact model/variant behavior frozen into a run.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenModelBinding {
    pub resolved: ResolvedModelRef,
    pub descriptor: LanguageModelDescriptor,
    pub defaults: ResolvedRequestDefaults,
    pub provider_options: ProviderOptions,
    pub behavior_fingerprint: Sha256Digest,
}

#[derive(Clone)]
struct ExecutableBehavior {
    model: Option<Arc<dyn LanguageModel>>,
    descriptor: LanguageModelDescriptor,
    defaults: ResolvedRequestDefaults,
    provider_options: ProviderOptions,
    behavior_fingerprint: Sha256Digest,
}

/// Exact executable model selected by a base or named-variant binding.
#[derive(Clone)]
pub struct ResolvedModel {
    selection: ModelSelection,
    model: Arc<dyn LanguageModel>,
    descriptor: LanguageModelDescriptor,
    defaults: ResolvedRequestDefaults,
    provider_options: ProviderOptions,
    behavior_fingerprint: Sha256Digest,
}

impl ResolvedModel {
    #[must_use]
    pub fn selection(&self) -> &ModelSelection {
        &self.selection
    }

    #[must_use]
    pub fn model(&self) -> &Arc<dyn LanguageModel> {
        &self.model
    }

    #[must_use]
    pub fn descriptor(&self) -> &LanguageModelDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub fn behavior_fingerprint(&self) -> &Sha256Digest {
        &self.behavior_fingerprint
    }

    #[must_use]
    pub fn prepare_request(&self, request: Request) -> Request {
        self.defaults.apply(&self.provider_options, request)
    }
}

impl fmt::Debug for ResolvedModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedModel")
            .field("selection", &self.selection)
            .field("descriptor", &self.descriptor)
            .field("behavior_fingerprint", &self.behavior_fingerprint)
            .finish_non_exhaustive()
    }
}

/// One configured direct model key with base behavior and named variants.
#[derive(Clone)]
pub struct ModelEntry {
    key: ModelKey,
    display_name: String,
    adapter_id: AdaptorId,
    base: ExecutableBehavior,
    capabilities: ModelCapabilities,
    variants: BTreeMap<VariantId, ModelVariant>,
    variant_behaviors: BTreeMap<VariantId, ExecutableBehavior>,
    default_variant: Option<VariantId>,
    behavior_fingerprint: Sha256Digest,
    available: bool,
}

impl ModelEntry {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        key: ModelKey,
        display_name: String,
        adapter_id: AdaptorId,
        model: Option<Arc<dyn LanguageModel>>,
        descriptor: LanguageModelDescriptor,
        capabilities: ModelCapabilities,
        defaults: ResolvedRequestDefaults,
        provider_options: ProviderOptions,
        variants: BTreeMap<VariantId, ModelVariant>,
        variant_models: BTreeMap<VariantId, Option<Arc<dyn LanguageModel>>>,
        variant_descriptors: BTreeMap<VariantId, LanguageModelDescriptor>,
        default_variant: Option<VariantId>,
        behavior_fingerprint: Sha256Digest,
        available: bool,
    ) -> Self {
        Self {
            key,
            display_name,
            adapter_id,
            base: ExecutableBehavior {
                model,
                descriptor,
                defaults,
                provider_options,
                behavior_fingerprint: behavior_fingerprint.clone(),
            },
            capabilities,
            variant_behaviors: variants
                .iter()
                .map(|(id, variant)| {
                    (
                        id.clone(),
                        ExecutableBehavior {
                            model: variant_models
                                .get(id)
                                .cloned()
                                .expect("compiled variant model"),
                            descriptor: variant_descriptors
                                .get(id)
                                .cloned()
                                .expect("compiled variant descriptor"),
                            defaults: variant.defaults.clone(),
                            provider_options: variant.provider_options.clone(),
                            behavior_fingerprint: variant.behavior_fingerprint.clone(),
                        },
                    )
                })
                .collect(),
            variants,
            default_variant,
            behavior_fingerprint,
            available,
        }
    }

    #[must_use]
    pub fn key(&self) -> &ModelKey {
        &self.key
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    #[must_use]
    pub const fn adapter_id(&self) -> AdaptorId {
        self.adapter_id
    }

    #[must_use]
    pub fn descriptor(&self) -> &LanguageModelDescriptor {
        &self.base.descriptor
    }

    #[must_use]
    pub fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    #[must_use]
    pub fn variants(&self) -> &BTreeMap<VariantId, ModelVariant> {
        &self.variants
    }

    #[must_use]
    pub fn default_variant(&self) -> Option<&VariantId> {
        self.default_variant.as_ref()
    }

    #[must_use]
    pub fn default_selection(&self) -> ModelSelection {
        ModelSelection {
            model: self.key.clone(),
            variant: self.default_variant.clone(),
        }
    }

    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.available
    }

    #[must_use]
    pub fn behavior_fingerprint(&self) -> &Sha256Digest {
        &self.behavior_fingerprint
    }

    fn behavior(&self, variant: Option<&VariantId>) -> Result<&ExecutableBehavior, ModelSetError> {
        match variant {
            None => Ok(&self.base),
            Some(id) => {
                self.variant_behaviors
                    .get(id)
                    .ok_or_else(|| ModelSetError::UnknownVariant {
                        model: self.key.clone(),
                        variant: id.clone(),
                    })
            }
        }
    }
}

impl fmt::Debug for ModelEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelEntry")
            .field("key", &self.key)
            .field("display_name", &self.display_name)
            .field("adapter_id", &self.adapter_id)
            .field("descriptor", &self.base.descriptor)
            .field("variants", &self.variants.keys().collect::<Vec<_>>())
            .field("default_variant", &self.default_variant)
            .field("behavior_fingerprint", &self.behavior_fingerprint)
            .field("available", &self.available)
            .finish_non_exhaustive()
    }
}

/// Immutable direct-key model set.
#[derive(Clone)]
pub struct ModelSet {
    entries: Arc<BTreeMap<ModelKey, ModelEntry>>,
    fingerprint: Sha256Digest,
}

impl ModelSet {
    pub fn new(
        entries: impl IntoIterator<Item = (ModelKey, ModelEntry)>,
        fingerprint: Sha256Digest,
    ) -> Result<Self, ModelSetError> {
        let mut built = BTreeMap::new();
        for (key, entry) in entries {
            if key != entry.key {
                return Err(ModelSetError::KeyMismatch);
            }
            if built.insert(key.clone(), entry).is_some() {
                return Err(ModelSetError::DuplicateKey(key));
            }
        }
        Ok(Self {
            entries: Arc::new(built),
            fingerprint,
        })
    }

    #[must_use]
    pub fn get(&self, key: &ModelKey) -> Option<&ModelEntry> {
        self.entries.get(key)
    }

    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&ModelKey, &ModelEntry)> {
        self.entries.iter()
    }

    pub fn descriptors(&self) -> Vec<AvailableModelDescriptor> {
        self.entries
            .values()
            .map(|entry| AvailableModelDescriptor {
                key: entry.key.clone(),
                display_name: entry.display_name.clone(),
                capabilities: entry.capabilities.clone(),
                variants: entry
                    .variants
                    .values()
                    .map(|variant| AvailableVariantDescriptor {
                        id: variant.id.clone(),
                        display_name: variant.display_name.clone(),
                        origin: variant.origin,
                        behavior_fingerprint: variant.behavior_fingerprint.clone(),
                    })
                    .collect(),
                default_variant: entry.default_variant.clone(),
                behavior_fingerprint: entry.behavior_fingerprint.clone(),
            })
            .collect()
    }

    #[must_use]
    pub fn fingerprint(&self) -> &Sha256Digest {
        &self.fingerprint
    }

    pub fn freeze(&self, selection: &ModelSelection) -> Result<FrozenModelBinding, ModelSetError> {
        let entry = self
            .get(&selection.model)
            .ok_or_else(|| ModelSetError::UnknownModel(selection.model.clone()))?;
        if !entry.is_available() {
            return Err(ModelSetError::ModelUnavailable(selection.model.clone()));
        }
        let behavior = entry.behavior(selection.variant.as_ref())?;
        let selection_fingerprint = Sha256Digest::hash(
            "cookie-agent/model-selection/v1",
            &(
                selection,
                entry.adapter_id,
                &behavior.descriptor,
                &behavior.behavior_fingerprint,
            ),
        )
        .map_err(ModelSetError::FingerprintEncoding)?;
        Ok(FrozenModelBinding {
            resolved: ResolvedModelRef {
                selection: selection.clone(),
                provider_id: selection.model.provider_id(),
                model_id: selection.model.model_id(),
                adapter_id: entry.adapter_id,
                selection_fingerprint,
            },
            descriptor: behavior.descriptor.clone(),
            defaults: behavior.defaults.clone(),
            provider_options: behavior.provider_options.clone(),
            behavior_fingerprint: behavior.behavior_fingerprint.clone(),
        })
    }

    pub fn resolve_selection(
        &self,
        selection: &ModelSelection,
    ) -> Result<ResolvedModel, ModelSetError> {
        let entry = self
            .get(&selection.model)
            .ok_or_else(|| ModelSetError::UnknownModel(selection.model.clone()))?;
        if !entry.is_available() {
            return Err(ModelSetError::ModelUnavailable(selection.model.clone()));
        }
        let behavior = entry.behavior(selection.variant.as_ref())?;
        Ok(ResolvedModel {
            selection: selection.clone(),
            model: behavior
                .model
                .clone()
                .ok_or_else(|| ModelSetError::ModelUnavailable(selection.model.clone()))?,
            descriptor: behavior.descriptor.clone(),
            defaults: behavior.defaults.clone(),
            provider_options: behavior.provider_options.clone(),
            behavior_fingerprint: behavior.behavior_fingerprint.clone(),
        })
    }

    pub fn resolve(&self, binding: &FrozenModelBinding) -> Result<ResolvedModel, ModelSetError> {
        let current = self.freeze(&binding.resolved.selection)?;
        if current != *binding {
            return Err(ModelSetError::BindingMismatch);
        }
        self.resolve_selection(&binding.resolved.selection)
    }
}

impl fmt::Debug for ModelSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelSet")
            .field("keys", &self.entries.keys().collect::<Vec<_>>())
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ModelSetError {
    #[error("fingerprint must be lowercase SHA-256 hexadecimal")]
    InvalidFingerprint,
    #[error("duplicate model key `{0}`")]
    DuplicateKey(ModelKey),
    #[error("model map key does not match entry key")]
    KeyMismatch,
    #[error("unknown model `{0}`")]
    UnknownModel(ModelKey),
    #[error("model `{0}` is configured but unavailable")]
    ModelUnavailable(ModelKey),
    #[error("unknown variant `{variant}` for model `{model}`")]
    UnknownVariant { model: ModelKey, variant: VariantId },
    #[error("frozen model binding no longer matches retained behavior")]
    BindingMismatch,
    #[error("model fingerprint canonicalization failed")]
    FingerprintEncoding(#[source] serde_json::Error),
}

/// One scripted call consumed by [`ScriptedModel`].
#[derive(Clone, Debug)]
pub enum ScriptedStep {
    Stream {
        creation_delay: Duration,
        items: Vec<ScriptedStreamItem>,
    },
    Error {
        delay: Duration,
        error: ModelError,
    },
}

impl ScriptedStep {
    pub fn stream(items: impl IntoIterator<Item = Result<StreamPart, ModelError>>) -> Self {
        Self::Stream {
            creation_delay: Duration::ZERO,
            items: items.into_iter().map(ScriptedStreamItem::item).collect(),
        }
    }

    #[must_use]
    pub fn delayed_stream(creation_delay: Duration, items: Vec<ScriptedStreamItem>) -> Self {
        Self::Stream {
            creation_delay,
            items,
        }
    }

    #[must_use]
    pub fn error(error: ModelError) -> Self {
        Self::Error {
            delay: Duration::ZERO,
            error,
        }
    }

    #[must_use]
    pub fn delayed_error(delay: Duration, error: ModelError) -> Self {
        Self::Error { delay, error }
    }
}

#[derive(Clone, Debug)]
pub enum ScriptedStreamItem {
    Item(Box<Result<StreamPart, ModelError>>),
    Delay(Duration),
}

impl ScriptedStreamItem {
    #[must_use]
    pub fn item(item: Result<StreamPart, ModelError>) -> Self {
        Self::Item(Box::new(item))
    }
}

#[derive(Clone, Debug)]
pub enum ScriptedCompactionStep {
    Result {
        delay: Duration,
        result: Box<CompactionResult>,
    },
    Error {
        delay: Duration,
        error: ModelError,
    },
}

impl ScriptedCompactionStep {
    #[must_use]
    pub fn result(result: CompactionResult) -> Self {
        Self::Result {
            delay: Duration::ZERO,
            result: Box::new(result),
        }
    }
}

#[derive(Clone)]
pub struct ScriptedModel {
    descriptor: LanguageModelDescriptor,
    steps: Arc<Mutex<VecDeque<ScriptedStep>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    compaction_steps: Arc<Mutex<VecDeque<ScriptedCompactionStep>>>,
    compaction_requests: Arc<Mutex<Vec<CompactionRequest>>>,
}

impl ScriptedModel {
    #[must_use]
    pub fn new(
        descriptor: LanguageModelDescriptor,
        steps: impl IntoIterator<Item = ScriptedStep>,
    ) -> Self {
        Self {
            descriptor,
            steps: Arc::new(Mutex::new(steps.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
            compaction_steps: Arc::new(Mutex::new(VecDeque::new())),
            compaction_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[must_use]
    pub fn with_compactions(
        mut self,
        steps: impl IntoIterator<Item = ScriptedCompactionStep>,
    ) -> Self {
        self.compaction_steps = Arc::new(Mutex::new(steps.into_iter().collect()));
        self
    }

    #[must_use]
    pub fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    #[must_use]
    pub fn compaction_requests(&self) -> Vec<CompactionRequest> {
        self.compaction_requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.steps
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}

impl LanguageModel for ScriptedModel {
    fn descriptor(&self) -> LanguageModelDescriptor {
        self.descriptor.clone()
    }

    fn validate_request(&self, request: &Request) -> Result<(), ModelError> {
        request.validate_for(&self.descriptor.capabilities)
    }

    fn supports_request(&self, request: &Request) -> bool {
        self.validate_request(request).is_ok()
    }

    fn stream<'a>(
        &'a self,
        request: Request,
        abort: AbortSignal,
    ) -> BoxFuture<'a, Result<StreamResponse, ModelError>> {
        Box::pin(async move {
            self.validate_request(&request)?;
            if abort.is_aborted() {
                return Err(ModelError::abort("scripted model request was aborted"));
            }
            self.requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(request);
            let step = self
                .steps
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop_front()
                .ok_or_else(|| ModelError::invalid_response("scripted model exhausted"))?;
            match step {
                ScriptedStep::Error { delay, error } => {
                    wait_or_abort(delay, &abort).await?;
                    Err(error)
                }
                ScriptedStep::Stream {
                    creation_delay,
                    items,
                } => {
                    wait_or_abort(creation_delay, &abort).await?;
                    let stream = futures_util::stream::unfold(
                        (VecDeque::from(items), abort, false),
                        |(mut items, abort, finished)| async move {
                            if finished {
                                return None;
                            }
                            loop {
                                if abort.is_aborted() {
                                    return Some((
                                        Err(ModelError::abort("scripted stream aborted")),
                                        (items, abort, true),
                                    ));
                                }
                                match items.pop_front() {
                                    Some(ScriptedStreamItem::Delay(delay)) => {
                                        if let Err(error) = wait_or_abort(delay, &abort).await {
                                            return Some((Err(error), (items, abort, true)));
                                        }
                                    }
                                    Some(ScriptedStreamItem::Item(item)) => {
                                        let item = *item;
                                        let finished = item.is_err();
                                        return Some((item, (items, abort, finished)));
                                    }
                                    None => return None,
                                }
                            }
                        },
                    );
                    Ok(StreamResponse::new(Box::pin(stream)))
                }
            }
        })
    }

    fn compact<'a>(
        &'a self,
        request: CompactionRequest,
        abort: AbortSignal,
    ) -> BoxFuture<'a, Result<CompactionResult, ModelError>> {
        Box::pin(async move {
            self.validate_compaction(&request)?;
            self.compaction_requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(request);
            let step = self
                .compaction_steps
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop_front()
                .ok_or_else(|| ModelError::invalid_response("scripted compaction exhausted"))?;
            match step {
                ScriptedCompactionStep::Result { delay, result } => {
                    wait_or_abort(delay, &abort).await?;
                    Ok(*result)
                }
                ScriptedCompactionStep::Error { delay, error } => {
                    wait_or_abort(delay, &abort).await?;
                    Err(error)
                }
            }
        })
    }
}

async fn wait_or_abort(delay: Duration, abort: &AbortSignal) -> Result<(), ModelError> {
    if abort.is_aborted() {
        return Err(ModelError::abort("scripted operation aborted"));
    }
    if delay.is_zero() {
        return Ok(());
    }
    tokio::select! {
        () = tokio::time::sleep(delay) => Ok(()),
        () = abort.aborted() => Err(ModelError::abort("scripted operation aborted")),
    }
}

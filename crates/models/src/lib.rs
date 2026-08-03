//! Immutable configured model bindings and explicit Oven adapter construction.

mod catalog;
mod credentials;
mod manager;
mod schema;

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use oven_sdk::{
    AbortSignal, BoxFuture, CompactionRequest, CompactionResult, LanguageModel,
    LanguageModelDescriptor, ModelError, ProviderOptions, Request, StreamPart, StreamResponse,
};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use thiserror::Error;

pub use catalog::{
    Catalog, CatalogBuildError, CatalogError, CatalogModel, CatalogModelCapabilities,
    CatalogModelLimits, CatalogModelModalities, CatalogModelStatus, CatalogProvider, CatalogRecipe,
    CatalogSnapshot, MODELS_DEV_ARTIFACT_BYTES, MODELS_DEV_ARTIFACT_SHA256, MODELS_DEV_COMMIT,
    MODELS_DEV_FETCHED_AT, MODELS_DEV_SOURCE, UnsupportedReason,
};
pub use credentials::{
    CredentialConnectOutcome, CredentialConnectReceipt, CredentialConnectRequest,
    CredentialSnapshot, CredentialStore, CredentialStoreError, StoredConnection,
};
pub use manager::{ModelSetManager, ModelSetManagerError, ModelSnapshot};
pub use schema::{ConfiguredModel, ModelBuildError, build_model_set, configuration_fingerprint};

/// Safe SHA-256 identity of behavior-affecting model configuration.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ConfigurationFingerprint(String);

impl ConfigurationFingerprint {
    /// Creates a validated lowercase SHA-256 fingerprint.
    pub fn new(value: impl Into<String>) -> Result<Self, ModelSetError> {
        let value = value.into();
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ModelSetError::InvalidFingerprint);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    /// Returns the lowercase hexadecimal digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Immutable defaults applied before a configured model validates a request.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestDefaults {
    /// Default maximum output tokens.
    pub max_output_tokens: Option<u64>,
    /// Default temperature.
    pub temperature: Option<f64>,
    /// Default top-p value.
    pub top_p: Option<f64>,
    /// Default provider-neutral reasoning effort.
    pub reasoning_effort: Option<String>,
    /// Default raw-event inclusion.
    #[serde(default)]
    pub include_raw: bool,
    /// Exact typed provider options serialized into Oven namespaces.
    #[serde(default)]
    pub provider_options: ProviderOptions,
}

impl RequestDefaults {
    /// Fills unset normalized fields and absent provider namespaces.
    #[must_use]
    pub fn apply(&self, mut request: Request) -> Request {
        request.inference.max_output_tokens = request
            .inference
            .max_output_tokens
            .or(self.max_output_tokens);
        request.inference.temperature = request.inference.temperature.or(self.temperature);
        request.inference.top_p = request.inference.top_p.or(self.top_p);
        request.inference.reasoning_effort = request
            .inference
            .reasoning_effort
            .clone()
            .or_else(|| self.reasoning_effort.clone());
        request.stream_options.include_raw |= self.include_raw;
        for (namespace, value) in &self.provider_options {
            request
                .provider_options
                .entry(namespace.clone())
                .or_insert_with(|| value.clone());
        }
        request
    }
}

/// One exact configured Oven model and its immutable defaults.
#[derive(Clone)]
pub struct ModelEntry {
    alias: String,
    model: Arc<dyn LanguageModel>,
    descriptor: LanguageModelDescriptor,
    defaults: RequestDefaults,
    behavior_fingerprint: ConfigurationFingerprint,
}

impl ModelEntry {
    /// Creates an entry and snapshots its descriptor.
    pub fn new(
        alias: impl Into<String>,
        model: Arc<dyn LanguageModel>,
        defaults: RequestDefaults,
    ) -> Result<Self, ModelSetError> {
        let descriptor = model.descriptor();
        let behavior_fingerprint = entry_behavior_fingerprint(&descriptor, &defaults);
        Self::new_with_behavior_fingerprint(alias, model, defaults, behavior_fingerprint)
    }

    /// Creates an entry with a complete caller-computed secret-free behavior fingerprint.
    pub fn new_with_behavior_fingerprint(
        alias: impl Into<String>,
        model: Arc<dyn LanguageModel>,
        defaults: RequestDefaults,
        behavior_fingerprint: ConfigurationFingerprint,
    ) -> Result<Self, ModelSetError> {
        let alias = alias.into();
        validate_alias(&alias)?;
        let descriptor = model.descriptor();
        Ok(Self {
            alias,
            model,
            descriptor,
            defaults,
            behavior_fingerprint,
        })
    }

    /// Returns the configuration alias.
    #[must_use]
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Returns the configured Oven model.
    #[must_use]
    pub fn model(&self) -> &Arc<dyn LanguageModel> {
        &self.model
    }

    /// Returns the frozen descriptor.
    #[must_use]
    pub fn descriptor(&self) -> &LanguageModelDescriptor {
        &self.descriptor
    }

    /// Returns immutable request defaults.
    #[must_use]
    pub fn defaults(&self) -> &RequestDefaults {
        &self.defaults
    }

    /// Returns the complete secret-free identity of behavior-affecting configuration.
    #[must_use]
    pub fn behavior_fingerprint(&self) -> &ConfigurationFingerprint {
        &self.behavior_fingerprint
    }

    /// Applies defaults to a request.
    #[must_use]
    pub fn prepare_request(&self, request: Request) -> Request {
        self.defaults.apply(request)
    }
}

impl fmt::Debug for ModelEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelEntry")
            .field("alias", &self.alias)
            .field("descriptor", &self.descriptor)
            .field("defaults", &self.defaults)
            .field("behavior_fingerprint", &self.behavior_fingerprint)
            .finish_non_exhaustive()
    }
}

/// Serializable model binding frozen into session policy.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenModelBinding {
    /// Configuration alias used by the agent chain.
    pub alias: String,
    /// Exact descriptor observed when configuration was materialized.
    pub descriptor: LanguageModelDescriptor,
    /// Immutable request defaults.
    pub defaults: RequestDefaults,
    /// Complete secret-free behavior identity for the exact configured entry.
    pub behavior_fingerprint: ConfigurationFingerprint,
    /// Safe model-set configuration fingerprint.
    pub configuration_fingerprint: ConfigurationFingerprint,
}

/// Immutable alias-indexed configured models.
#[derive(Clone)]
pub struct ModelSet {
    entries: Arc<BTreeMap<String, ModelEntry>>,
    fingerprint: ConfigurationFingerprint,
}

impl ModelSet {
    /// Creates a set once; duplicate aliases and alias/entry mismatches fail closed.
    pub fn new(
        entries: impl IntoIterator<Item = (String, ModelEntry)>,
        fingerprint: ConfigurationFingerprint,
    ) -> Result<Self, ModelSetError> {
        let mut built = BTreeMap::new();
        for (alias, entry) in entries {
            validate_alias(&alias)?;
            if alias != entry.alias {
                return Err(ModelSetError::AliasMismatch {
                    key: alias,
                    entry: entry.alias,
                });
            }
            if built.insert(alias.clone(), entry).is_some() {
                return Err(ModelSetError::DuplicateAlias(alias));
            }
        }
        Ok(Self {
            entries: Arc::new(built),
            fingerprint,
        })
    }

    /// Returns one exact entry.
    #[must_use]
    pub fn get(&self, alias: &str) -> Option<&ModelEntry> {
        self.entries.get(alias)
    }

    /// Iterates aliases in stable order.
    pub fn aliases(&self) -> impl ExactSizeIterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Iterates exact aliases and entries in stable order.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&str, &ModelEntry)> {
        self.entries
            .iter()
            .map(|(alias, entry)| (alias.as_str(), entry))
    }

    /// Returns the safe model-set fingerprint.
    #[must_use]
    pub fn fingerprint(&self) -> &ConfigurationFingerprint {
        &self.fingerprint
    }

    /// Freezes one alias for a session policy snapshot.
    pub fn freeze(&self, alias: &str) -> Result<FrozenModelBinding, ModelSetError> {
        let entry = self
            .get(alias)
            .ok_or_else(|| ModelSetError::UnknownAlias(alias.to_owned()))?;
        Ok(FrozenModelBinding {
            alias: alias.to_owned(),
            descriptor: entry.descriptor.clone(),
            defaults: entry.defaults.clone(),
            behavior_fingerprint: entry.behavior_fingerprint.clone(),
            configuration_fingerprint: self.fingerprint.clone(),
        })
    }

    /// Resolves a frozen binding only against the exact immutable model set.
    pub fn resolve(&self, binding: &FrozenModelBinding) -> Result<&ModelEntry, ModelSetError> {
        if binding.configuration_fingerprint != self.fingerprint {
            return Err(ModelSetError::FingerprintMismatch);
        }
        let entry = self
            .get(&binding.alias)
            .ok_or_else(|| ModelSetError::UnknownAlias(binding.alias.clone()))?;
        if entry.descriptor != binding.descriptor
            || entry.defaults != binding.defaults
            || entry.behavior_fingerprint != binding.behavior_fingerprint
        {
            return Err(ModelSetError::BindingMismatch(binding.alias.clone()));
        }
        Ok(entry)
    }
}

impl fmt::Debug for ModelSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelSet")
            .field("aliases", &self.entries.keys().collect::<Vec<_>>())
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// Immutable model-set construction or binding error.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ModelSetError {
    /// Alias is empty or contains control characters.
    #[error("model alias must be non-empty and contain no control characters")]
    InvalidAlias,
    /// Fingerprint is not a SHA-256 hexadecimal digest.
    #[error("configuration fingerprint must be a 64-character SHA-256 digest")]
    InvalidFingerprint,
    /// Alias occurred more than once.
    #[error("duplicate model alias `{0}`")]
    DuplicateAlias(String),
    /// Map key and entry alias differ.
    #[error("model entry alias `{entry}` does not match key `{key}`")]
    AliasMismatch { key: String, entry: String },
    /// Alias was not configured.
    #[error("unknown model alias `{0}`")]
    UnknownAlias(String),
    /// Binding belongs to another configuration.
    #[error("frozen model binding configuration fingerprint does not match")]
    FingerprintMismatch,
    /// Binding descriptor/defaults do not match the entry.
    #[error("frozen model binding does not match configured alias `{0}`")]
    BindingMismatch(String),
}

fn validate_alias(alias: &str) -> Result<(), ModelSetError> {
    if alias.trim().is_empty() || alias.chars().any(char::is_control) {
        Err(ModelSetError::InvalidAlias)
    } else {
        Ok(())
    }
}

fn entry_behavior_fingerprint(
    descriptor: &LanguageModelDescriptor,
    defaults: &RequestDefaults,
) -> ConfigurationFingerprint {
    let encoded = serde_json::to_vec(&(descriptor, defaults))
        .expect("model descriptor and defaults always serialize");
    ConfigurationFingerprint::new(format!("{:x}", sha2::Sha256::digest(encoded)))
        .expect("SHA-256 is a valid configuration fingerprint")
}

/// One scripted call consumed by [`ScriptedModel`].
#[derive(Clone, Debug)]
pub enum ScriptedStep {
    /// Create a stream after an optional delay.
    Stream {
        /// Delay before returning [`StreamResponse`].
        creation_delay: Duration,
        /// Ordered stream items and delays.
        items: Vec<ScriptedStreamItem>,
    },
    /// Fail before returning a stream, after an optional delay.
    Error {
        /// Delay before returning the error.
        delay: Duration,
        /// Planned call error.
        error: ModelError,
    },
}

/// One deterministic native-compaction call consumed by [`ScriptedModel`].
#[derive(Clone, Debug)]
pub enum ScriptedCompactionStep {
    /// Return a native-context result after an optional delay.
    Result {
        /// Delay before returning the result.
        delay: Duration,
        /// Planned compaction result.
        result: Box<CompactionResult>,
    },
    /// Return a compaction error after an optional delay.
    Error {
        /// Delay before returning the error.
        delay: Duration,
        /// Planned compaction error.
        error: ModelError,
    },
}

impl ScriptedCompactionStep {
    /// Creates an immediate successful compaction result.
    #[must_use]
    pub fn result(result: CompactionResult) -> Self {
        Self::Result {
            delay: Duration::ZERO,
            result: Box::new(result),
        }
    }

    /// Creates a delayed successful compaction result.
    #[must_use]
    pub fn delayed_result(delay: Duration, result: CompactionResult) -> Self {
        Self::Result {
            delay,
            result: Box::new(result),
        }
    }

    /// Creates an immediate compaction error.
    #[must_use]
    pub fn error(error: ModelError) -> Self {
        Self::Error {
            delay: Duration::ZERO,
            error,
        }
    }
}

impl ScriptedStep {
    /// Creates an immediate stream from queued item results.
    pub fn stream(items: impl IntoIterator<Item = Result<StreamPart, ModelError>>) -> Self {
        Self::Stream {
            creation_delay: Duration::ZERO,
            items: items.into_iter().map(ScriptedStreamItem::item).collect(),
        }
    }

    /// Creates a stream after `creation_delay`.
    #[must_use]
    pub fn delayed_stream(creation_delay: Duration, items: Vec<ScriptedStreamItem>) -> Self {
        Self::Stream {
            creation_delay,
            items,
        }
    }

    /// Creates an immediate call error.
    #[must_use]
    pub fn error(error: ModelError) -> Self {
        Self::Error {
            delay: Duration::ZERO,
            error,
        }
    }

    /// Creates a delayed call error.
    #[must_use]
    pub fn delayed_error(delay: Duration, error: ModelError) -> Self {
        Self::Error { delay, error }
    }
}

/// One action in a scripted response stream.
#[derive(Clone, Debug)]
pub enum ScriptedStreamItem {
    /// Emit a successful part or a mid-stream model error.
    Item(Box<Result<StreamPart, ModelError>>),
    /// Wait while remaining cancellation-aware.
    Delay(Duration),
}

impl ScriptedStreamItem {
    /// Queues one stream item result.
    #[must_use]
    pub fn item(item: Result<StreamPart, ModelError>) -> Self {
        Self::Item(Box::new(item))
    }
}

impl From<Result<StreamPart, ModelError>> for ScriptedStreamItem {
    fn from(item: Result<StreamPart, ModelError>) -> Self {
        Self::item(item)
    }
}

/// Deterministic, thread-safe Oven model for package and harness tests.
#[derive(Clone)]
pub struct ScriptedModel {
    descriptor: LanguageModelDescriptor,
    steps: Arc<Mutex<VecDeque<ScriptedStep>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    compaction_steps: Arc<Mutex<VecDeque<ScriptedCompactionStep>>>,
    compaction_requests: Arc<Mutex<Vec<CompactionRequest>>>,
}

impl ScriptedModel {
    /// Creates a scripted model with an immutable descriptor and FIFO steps.
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

    /// Installs deterministic FIFO native-compaction steps.
    #[must_use]
    pub fn with_compactions(
        mut self,
        steps: impl IntoIterator<Item = ScriptedCompactionStep>,
    ) -> Self {
        self.compaction_steps = Arc::new(Mutex::new(steps.into_iter().collect()));
        self
    }

    /// Returns captured requests in call order.
    #[must_use]
    pub fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Returns captured native-compaction requests in call order.
    #[must_use]
    pub fn compaction_requests(&self) -> Vec<CompactionRequest> {
        self.compaction_requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Returns the number of unconsumed steps.
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
                ScriptedStep::Stream {
                    creation_delay,
                    items,
                } => {
                    wait_or_abort(creation_delay, &abort, "before scripted stream creation")
                        .await?;
                    let state = ScriptedStreamState {
                        items: items.into(),
                        abort,
                        finished: false,
                    };
                    let stream = futures_util::stream::unfold(state, |mut state| async move {
                        loop {
                            if state.finished {
                                return None;
                            }
                            if state.abort.is_aborted() {
                                state.finished = true;
                                return Some((Err(scripted_abort("after stream creation")), state));
                            }
                            match state.items.pop_front() {
                                Some(ScriptedStreamItem::Delay(delay)) => {
                                    if let Err(error) = wait_or_abort(
                                        delay,
                                        &state.abort,
                                        "during scripted stream delay",
                                    )
                                    .await
                                    {
                                        state.finished = true;
                                        return Some((Err(error), state));
                                    }
                                }
                                Some(ScriptedStreamItem::Item(item)) => {
                                    let item = *item;
                                    state.finished = item.is_err();
                                    return Some((item, state));
                                }
                                None => {
                                    return None;
                                }
                            }
                        }
                    });
                    Ok(StreamResponse::new(Box::pin(stream)))
                }
                ScriptedStep::Error { delay, error } => {
                    wait_or_abort(delay, &abort, "before scripted call error").await?;
                    Err(error)
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
            if abort.is_aborted() {
                return Err(ModelError::abort(
                    "scripted native compaction was aborted before dispatch",
                ));
            }
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
                    wait_or_abort(delay, &abort, "during scripted native compaction").await?;
                    Ok(*result)
                }
                ScriptedCompactionStep::Error { delay, error } => {
                    wait_or_abort(delay, &abort, "before scripted native compaction error").await?;
                    Err(error)
                }
            }
        })
    }
}

struct ScriptedStreamState {
    items: VecDeque<ScriptedStreamItem>,
    abort: AbortSignal,
    finished: bool,
}

async fn wait_or_abort(
    delay: Duration,
    abort: &AbortSignal,
    phase: &'static str,
) -> Result<(), ModelError> {
    if abort.is_aborted() {
        return Err(scripted_abort(phase));
    }
    if delay.is_zero() {
        return Ok(());
    }
    tokio::select! {
        () = tokio::time::sleep(delay) => Ok(()),
        () = abort.aborted() => Err(scripted_abort(phase)),
    }
}

fn scripted_abort(phase: &str) -> ModelError {
    ModelError::abort(format!("scripted model request was aborted {phase}"))
}

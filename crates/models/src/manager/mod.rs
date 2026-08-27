//! Atomic dynamic model runtime construction and provider-store orchestration.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{Arc, Mutex},
};

use arc_swap::ArcSwap;
use cookie_agent_identity::{
    AuthFieldName, AuthMethodId, AuthParameterId, CatalogRevision, ModelKey, ModelRevision,
    ModelSelection, ProtocolRecipeId, ProviderId, ProviderRecipeId, ProviderSetupRecipeId,
    ProviderStateRevision, RecipeCompilerVersion, RuntimeRevision, SetupFieldId,
};
use cookie_agent_protocol as protocol;
use oven_sdk::{
    AdapterId, CancellationCapability as OvenCancellation, Capability,
    CompactionCapability as OvenCompaction, LanguageModel, LanguageModelDescriptor,
    MediaCapabilities, MediaInputSupport, MediaSourceSupport, Modalities, Modality as OvenModality,
    ModelCapabilities as OvenCapabilities, ModelId, ModelIdentity, ModelLimits,
    ProviderId as OvenProviderId, ReplayCapability as OvenReplay, ReplayDeclaration, ReplayPolicy,
    Request,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    BoundedSetupString, ProviderDefinition, SafeSetupValue, SecretString, Sha256Digest,
    adapters::{
        CacheStrategyConfig, GoogleCacheMode, OpenAiCacheMode, OpenAiPromptCacheRetention,
        OpenAiPromptCacheTtl, OvenAdapterFamily,
        oven::{AnthropicCacheStrategyConfig, AnthropicCacheTtlConfig, ModelBuildError},
    },
    authoring::{AuthOverride, ModelsDevProvider},
    catalog::CatalogSnapshot,
    compiler::{
        AuthSourceCategory, CompiledAuthShape, CompiledDynamicModel, CompiledModelStatus,
        DynamicCompileError, DynamicCompiler, ExecutableBehaviorInput,
        ExecutableCredentialMaterial, compile_executable,
    },
    manifests::{
        CompiledSafeModelBlueprint, FrozenAuthParameterValue, FrozenCredentialBinding,
        FrozenCredentialSource, FrozenProviderSource, FrozenResolvedRequestDefaults,
        FrozenSetupBinding, FrozenVariantBlueprint, HeaderName, ModelSnapshotPayloadV1,
        NormalizedDecimal, SafeEndpointIdentity, SafeStaticHeaderValue, behavior_fingerprint,
        blueprint_fingerprint, canonical_state_fingerprint, selected_behavior,
        selection_fingerprint,
    },
    provider_store::{
        ClientConnectId, ClientRequestId, ConnectMutation, ConnectProposal, DisconnectMutation,
        DisconnectProposal, DurableConnectionDescriptor, ProviderAuthValues,
        ProviderConnectionGeneration, ProviderStore, ProviderStoreError, ProviderStoreMutation,
        ProviderStoreSnapshot, SafePolicyString, StoredManagedConnection,
        StoredProviderPolicyProjection,
    },
    recipes::{
        COMPILER_VERSION, FamilyRecipe, FamilyRecipeRegistry, SetupRecipe, auth_method,
        family_registry, validate_setup,
    },
};

/// Exact credential category selected after authored/store/no-auth precedence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveCredentialSource {
    AuthoredApiKey,
    AuthoredOverride,
    ProviderStore,
    NoAuth,
    Unavailable,
}

/// Current catalog presence for one managed provider row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderPresence {
    Current,
    Removed,
}

/// Current Registry-1 verdict for a removed provider's retained store policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedFamilyMatch {
    SupportedRemoved,
    RemovedWithoutRetainedFamilyMatch,
}

impl RetainedFamilyMatch {
    #[must_use]
    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::SupportedRemoved => None,
            Self::RemovedWithoutRetainedFamilyMatch => {
                Some("removed_without_retained_recipe_match")
            }
        }
    }
}

/// Safe current provider row, including configured providers removed from the catalog.
#[derive(Clone, Debug)]
pub struct CompiledProviderState {
    pub id: ProviderId,
    pub display_name: String,
    pub presence: ProviderPresence,
    pub support_reason: Option<String>,
    pub retained_family_match: Option<RetainedFamilyMatch>,
    pub authored: bool,
    pub stored: bool,
    pub effective_auth: EffectiveCredentialSource,
    pub durable_connection: Option<DurableConnectionDescriptor>,
}

/// One complete compiled model plus its exact safe source and credential bindings.
#[derive(Clone)]
pub struct CompiledRuntimeModel {
    pub key: ModelKey,
    pub model: CompiledDynamicModel,
    pub source: RuntimeProviderSource,
    pub config_override_fingerprint: Sha256Digest,
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub setup_fingerprint: Sha256Digest,
    pub setup_recipe: ProviderSetupRecipeId,
    pub credential_source: EffectiveCredentialSource,
    pub static_headers: BTreeMap<crate::HeaderName, crate::SafeStaticHeaderValue>,
    executable: Option<ExecutableBehavior>,
    variant_executables: BTreeMap<cookie_agent_identity::VariantId, ExecutableBehavior>,
}

#[derive(Clone)]
struct ExecutableBehavior {
    adapter: OvenAdapterFamily,
    model: Arc<dyn LanguageModel>,
    defaults: crate::ResolvedRequestDefaults,
    provider_options: oven_sdk::ProviderOptions,
    behavior_fingerprint: Sha256Digest,
}

/// Exact executable model behavior retained by one runtime publication.
#[derive(Clone)]
pub struct ResolvedExecutableModel {
    selection: ModelSelection,
    model: Arc<dyn LanguageModel>,
    defaults: crate::ResolvedRequestDefaults,
    provider_options: oven_sdk::ProviderOptions,
    behavior_fingerprint: Sha256Digest,
    adapter: OvenAdapterFamily,
}

impl ResolvedExecutableModel {
    #[must_use]
    pub fn selection(&self) -> &ModelSelection {
        &self.selection
    }

    #[must_use]
    pub fn model(&self) -> &Arc<dyn LanguageModel> {
        &self.model
    }

    #[must_use]
    pub fn behavior_fingerprint(&self) -> &Sha256Digest {
        &self.behavior_fingerprint
    }

    #[must_use]
    pub const fn adapter_family(&self) -> OvenAdapterFamily {
        self.adapter
    }

    #[must_use]
    pub fn prepare_request(&self, request: Request) -> Request {
        self.prepare_request_inner(request, None)
    }

    #[must_use]
    pub fn prepare_request_with_cache_strategy(
        &self,
        request: Request,
        strategy: Option<&CacheStrategyConfig>,
    ) -> Request {
        self.prepare_request_inner(request, Some(strategy))
    }

    /// Applies model defaults while deferring prompt-cache placement until interception completes.
    #[must_use]
    pub fn prepare_request_before_cache_strategy(
        &self,
        request: Request,
        strategy: Option<&CacheStrategyConfig>,
    ) -> Request {
        self.prepare_request_defaults(request, Some(strategy)).0
    }

    /// Applies prompt-cache placement once to an authoritative intercepted request.
    #[must_use]
    pub fn apply_prompt_cache_strategy(
        &self,
        mut request: Request,
        strategy: Option<&CacheStrategyConfig>,
    ) -> Request {
        if let Some(strategy) = strategy {
            apply_cache_strategy(&mut request, self.adapter, strategy);
        }
        request
    }

    /// Serializes the prepared provider request at the engine interception boundary.
    pub fn provider_request_payload(
        &self,
        request: &Request,
    ) -> Result<Value, Box<oven_sdk::ModelError>> {
        serde_json::to_value(request)
            .map_err(|error| Box::new(oven_sdk::ModelError::invalid_request(error.to_string())))
    }

    /// Rebuilds and validates a plugin-replaced request payload before provider execution.
    pub fn request_from_provider_payload(
        &self,
        payload: Value,
    ) -> Result<Request, Box<oven_sdk::ModelError>> {
        if !payload.is_object() {
            return Err(Box::new(oven_sdk::ModelError::invalid_request(
                "replacement provider payload must be a JSON object",
            )));
        }
        let request: Request = serde_json::from_value(payload)
            .map_err(|error| Box::new(oven_sdk::ModelError::invalid_request(error.to_string())))?;
        self.model.validate_request(&request).map_err(Box::new)?;
        Ok(request)
    }

    fn prepare_request_inner(
        &self,
        request: Request,
        strategy_override: Option<Option<&CacheStrategyConfig>>,
    ) -> Request {
        let (mut request, strategy) = self.prepare_request_defaults(request, strategy_override);
        if (self
            .model
            .capabilities()
            .features
            .contains(Capability::PROMPT_CACHING)
            || matches!(
                self.adapter,
                OvenAdapterFamily::OpenaiChat
                    | OvenAdapterFamily::OpenaiResponses
                    | OvenAdapterFamily::AzureOpenaiChat
                    | OvenAdapterFamily::AzureOpenaiResponses
            ))
            && let Some(strategy) = strategy.as_ref()
        {
            apply_cache_strategy(&mut request, self.adapter, strategy);
        }
        request
    }

    fn prepare_request_defaults(
        &self,
        request: Request,
        strategy_override: Option<Option<&CacheStrategyConfig>>,
    ) -> (Request, Option<CacheStrategyConfig>) {
        let mut request = self
            .defaults
            .apply(&crate::ProviderOptions::default(), request);
        request
            .provider_options
            .extend(self.provider_options.clone());
        let strategy = strategy_override.flatten().cloned();
        (request, strategy)
    }
}

fn apply_cache_strategy(
    request: &mut Request,
    adapter: OvenAdapterFamily,
    strategy: &CacheStrategyConfig,
) {
    match (adapter, strategy) {
        (
            OvenAdapterFamily::Anthropic | OvenAdapterFamily::AnthropicCompatible,
            CacheStrategyConfig::Anthropic(strategy),
        ) => apply_anthropic_cache_strategy(request, strategy),
        (OvenAdapterFamily::AwsBedrockConverse, CacheStrategyConfig::Bedrock(strategy)) => {
            let mut strategy = strategy.clone();
            if request.tools.is_empty() {
                strategy.tools = None;
            }
            if !request.history.iter().any(|turn| {
                matches!(
                    turn,
                    oven_sdk::HistoryTurn::System(message)
                        if message.content.iter().any(|part| {
                            matches!(part, oven_sdk::SystemPart::Text(text) if !text.text.is_empty())
                        })
                )
            }) {
                strategy.system = None;
            }
            let last_message = request
                .history
                .iter()
                .rposition(|turn| !matches!(turn, oven_sdk::HistoryTurn::System(_)));
            strategy.messages.retain_mut(|point| {
                if point.history_index != usize::MAX {
                    return true;
                }
                if let Some(index) = last_message {
                    point.history_index = index;
                    true
                } else {
                    false
                }
            });
            set_option(
                &mut request.provider_options,
                "bedrock",
                None,
                "cache",
                Some(serde_json::to_value(strategy).expect("Bedrock cache strategy serializes")),
            );
        }
        (
            OvenAdapterFamily::GoogleGemini | OvenAdapterFamily::GoogleVertexGemini,
            CacheStrategyConfig::Google(strategy),
        ) => {
            let namespace = if adapter == OvenAdapterFamily::GoogleGemini {
                "google"
            } else {
                "google_vertex"
            };
            match strategy.mode {
                GoogleCacheMode::Implicit => {}
                GoogleCacheMode::Off => set_option(
                    &mut request.provider_options,
                    namespace,
                    None,
                    "cached_content",
                    None,
                ),
                GoogleCacheMode::Explicit => set_option(
                    &mut request.provider_options,
                    namespace,
                    None,
                    "cached_content",
                    strategy.cached_content.clone().map(Value::String),
                ),
            }
        }
        (
            OvenAdapterFamily::OpenaiChat
            | OvenAdapterFamily::OpenaiResponses
            | OvenAdapterFamily::AzureOpenaiChat
            | OvenAdapterFamily::AzureOpenaiResponses,
            CacheStrategyConfig::OpenAi(strategy),
        ) => {
            let (namespace, section) = match adapter {
                OvenAdapterFamily::OpenaiChat => ("openai", "chat"),
                OvenAdapterFamily::OpenaiResponses => ("openai", "responses"),
                OvenAdapterFamily::AzureOpenaiChat => ("azure_openai", "chat"),
                OvenAdapterFamily::AzureOpenaiResponses => ("azure_openai", "responses"),
                _ => unreachable!("OpenAI family match checked"),
            };
            set_option(
                &mut request.provider_options,
                namespace,
                Some(section),
                "prompt_cache_key",
                strategy.prompt_cache_key.clone().map(Value::String),
            );
            set_option(
                &mut request.provider_options,
                namespace,
                Some(section),
                "prompt_cache_retention",
                strategy.prompt_cache_retention.map(|retention| {
                    Value::String(
                        match retention {
                            OpenAiPromptCacheRetention::InMemory => "in_memory",
                            OpenAiPromptCacheRetention::TwentyFourHours => "24h",
                        }
                        .into(),
                    )
                }),
            );
            set_option(
                &mut request.provider_options,
                namespace,
                Some(section),
                "prompt_cache_options",
                openai_prompt_cache_options(adapter, strategy),
            );
            apply_openai_cache_breakpoints(request, adapter, strategy);
        }
        _ => {}
    }
}

fn openai_prompt_cache_options(
    adapter: OvenAdapterFamily,
    strategy: &crate::adapters::OpenAiCacheStrategyConfig,
) -> Option<Value> {
    let (Some(mode), Some(ttl)) = (strategy.mode, strategy.ttl) else {
        return None;
    };
    Some(match adapter {
        OvenAdapterFamily::OpenaiChat | OvenAdapterFamily::OpenaiResponses => {
            serde_json::to_value(oven_sdk_openai::OpenAiPromptCacheOptions {
                mode: match mode {
                    OpenAiCacheMode::Implicit => oven_sdk_openai::OpenAiPromptCacheMode::Implicit,
                    OpenAiCacheMode::Explicit => oven_sdk_openai::OpenAiPromptCacheMode::Explicit,
                },
                ttl: match ttl {
                    OpenAiPromptCacheTtl::ThirtyMinutes => {
                        oven_sdk_openai::OpenAiPromptCacheTtl::ThirtyMinutes
                    }
                },
            })
            .expect("OpenAI cache options serialize")
        }
        OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses => {
            serde_json::to_value(oven_sdk_azure::AzureOpenAiPromptCacheOptions {
                mode: match mode {
                    OpenAiCacheMode::Implicit => {
                        oven_sdk_azure::AzureOpenAiPromptCacheMode::Implicit
                    }
                    OpenAiCacheMode::Explicit => {
                        oven_sdk_azure::AzureOpenAiPromptCacheMode::Explicit
                    }
                },
                ttl: match ttl {
                    OpenAiPromptCacheTtl::ThirtyMinutes => {
                        oven_sdk_azure::AzureOpenAiPromptCacheTtl::ThirtyMinutes
                    }
                },
            })
            .expect("Azure OpenAI cache options serialize")
        }
        _ => return None,
    })
}

fn apply_openai_cache_breakpoints(
    request: &mut Request,
    adapter: OvenAdapterFamily,
    strategy: &crate::adapters::OpenAiCacheStrategyConfig,
) {
    if strategy.system
        && let Some(oven_sdk::HistoryTurn::System(message)) = request.history.first_mut()
        && let Some(text) = message
            .content
            .iter_mut()
            .rev()
            .find_map(|part| match part {
                oven_sdk::SystemPart::Text(text) if !text.text.is_empty() => Some(text),
                oven_sdk::SystemPart::Text(_) | oven_sdk::SystemPart::Custom(_) => None,
            })
    {
        mark_openai_cache_breakpoint(text, adapter);
    }
    if strategy.rolling
        && let Some(index) = request
            .history
            .iter()
            .rposition(|turn| matches!(turn, oven_sdk::HistoryTurn::User(_)))
        && let oven_sdk::HistoryTurn::User(message) = &mut request.history[index]
        && let Some(text) = message
            .content
            .iter_mut()
            .rev()
            .find_map(|part| match part {
                oven_sdk::InputPart::Text(text) if !text.text.is_empty() => Some(text),
                oven_sdk::InputPart::Text(_)
                | oven_sdk::InputPart::File(_)
                | oven_sdk::InputPart::Custom(_) => None,
            })
    {
        mark_openai_cache_breakpoint(text, adapter);
    }
}

fn mark_openai_cache_breakpoint(text: &mut oven_sdk::TextPart, adapter: OvenAdapterFamily) {
    *text = match adapter {
        OvenAdapterFamily::OpenaiChat | OvenAdapterFamily::OpenaiResponses => {
            oven_sdk_openai::OpenAiPromptCacheBreakpointExt::with_openai_prompt_cache_breakpoint(
                text.clone(),
            )
        }
        OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses => {
            oven_sdk_azure::AzureOpenAiPromptCacheBreakpointExt::with_azure_openai_prompt_cache_breakpoint(
                text.clone(),
            )
        }
        _ => return,
    };
}

fn set_option(
    options: &mut oven_sdk::ProviderOptions,
    namespace: &str,
    section: Option<&str>,
    key: &str,
    value: Option<Value>,
) {
    let namespace = options
        .entry(namespace.into())
        .or_insert_with(|| Value::Object(Default::default()));
    let Value::Object(namespace) = namespace else {
        return;
    };
    let target = if let Some(section) = section {
        let section = namespace
            .entry(section)
            .or_insert_with(|| Value::Object(Default::default()));
        let Value::Object(section) = section else {
            return;
        };
        section
    } else {
        namespace
    };
    if let Some(value) = value {
        target.insert(key.into(), value);
    } else {
        target.remove(key);
    }
}

fn clear_message_cache(options: &mut oven_sdk::ProviderOptions) {
    if let Some(Value::Object(anthropic)) = options.get_mut("anthropic") {
        anthropic.remove("cache_control");
    }
}

fn set_message_cache(options: &mut oven_sdk::ProviderOptions, ttl: AnthropicCacheTtlConfig) {
    let anthropic = options
        .entry("anthropic".into())
        .or_insert_with(|| Value::Object(Default::default()));
    if let Value::Object(anthropic) = anthropic {
        anthropic.insert("cache_control".into(), json!({ "ttl": ttl }));
    }
}

fn apply_anthropic_cache_strategy(request: &mut Request, strategy: &AnthropicCacheStrategyConfig) {
    for tool in &mut request.tools {
        clear_message_cache(&mut tool.provider_options);
    }
    for turn in &mut request.history {
        clear_message_cache(turn_provider_options_mut(turn));
    }

    let system_index = request.history.first().and_then(|turn| match turn {
        oven_sdk::HistoryTurn::System(message) if system_message_is_eligible(message) => Some(0),
        _ => None,
    });
    if let (Some(index), Some(ttl)) = (system_index, strategy.system) {
        set_message_cache(turn_provider_options_mut(&mut request.history[index]), ttl);
    }

    if !request.tools.is_empty()
        && !matches!(request.tool_choice, oven_sdk::ToolChoice::None)
        && let Some(ttl) = strategy.tools
        && let Some(tool) = request.tools.last_mut()
    {
        set_message_cache(&mut tool.provider_options, ttl);
    }

    if let Some(ttl) = strategy.rolling
        && let Some(index) = request.history.iter().rposition(turn_is_rolling_eligible)
        && Some(index) != system_index
    {
        set_message_cache(turn_provider_options_mut(&mut request.history[index]), ttl);
    }
}

fn turn_provider_options_mut(turn: &mut oven_sdk::HistoryTurn) -> &mut oven_sdk::ProviderOptions {
    match turn {
        oven_sdk::HistoryTurn::System(message) => &mut message.provider_options,
        oven_sdk::HistoryTurn::User(message) => &mut message.provider_options,
        oven_sdk::HistoryTurn::Assistant(turn) => &mut turn.message.provider_options,
        oven_sdk::HistoryTurn::Tool(message) => &mut message.provider_options,
    }
}

fn system_message_is_eligible(message: &oven_sdk::SystemMessage) -> bool {
    message
        .content
        .iter()
        .any(|part| matches!(part, oven_sdk::SystemPart::Text(text) if !text.text.is_empty()))
}

fn turn_is_rolling_eligible(turn: &oven_sdk::HistoryTurn) -> bool {
    match turn {
        oven_sdk::HistoryTurn::System(message) => system_message_is_eligible(message),
        oven_sdk::HistoryTurn::User(message) => message.content.iter().any(|part| match part {
            oven_sdk::InputPart::Text(text) => !text.text.is_empty(),
            oven_sdk::InputPart::File(_) => true,
            oven_sdk::InputPart::Custom(_) => false,
        }),
        oven_sdk::HistoryTurn::Assistant(turn) => turn.message.content.iter().any(|part| {
            matches!(part, oven_sdk::AssistantPart::Text(text) if !text.text.is_empty())
                || matches!(part, oven_sdk::AssistantPart::ToolCall(_))
        }),
        oven_sdk::HistoryTurn::Tool(_) => false,
    }
}

impl fmt::Debug for ResolvedExecutableModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedExecutableModel")
            .field("selection", &self.selection)
            .field("behavior_fingerprint", &self.behavior_fingerprint)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for CompiledRuntimeModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledRuntimeModel")
            .field("key", &self.key)
            .field("model", &self.model)
            .field("source", &self.source)
            .field(
                "config_override_fingerprint",
                &self.config_override_fingerprint,
            )
            .field("setup_values", &self.setup_values)
            .field("setup_fingerprint", &self.setup_fingerprint)
            .field("setup_recipe", &self.setup_recipe)
            .field("credential_source", &self.credential_source)
            .field("static_headers", &self.static_headers)
            .field("executable", &self.executable.is_some())
            .field(
                "variant_executables",
                &self.variant_executables.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum RuntimeProviderSource {
    Managed {
        family_id: ProviderRecipeId,
        source_record_digest: Sha256Digest,
        recipe_fingerprint: Sha256Digest,
        package_claim: String,
    },
    Custom {
        safe_definition_fingerprint: Sha256Digest,
    },
}

/// Immutable complete model runtime used by all manager mutations.
#[derive(Clone)]
pub struct CompiledModelRuntime {
    authored: Arc<BTreeMap<ProviderId, ProviderDefinition>>,
    catalog: Arc<CatalogSnapshot>,
    store: ProviderStoreSnapshot,
    recipe_registry_revision: cookie_agent_identity::RecipeRegistryRevision,
    model_revision: ModelRevision,
    runtime_revision: RuntimeRevision,
    providers: Arc<Vec<CompiledProviderState>>,
    models: Arc<BTreeMap<ModelKey, CompiledRuntimeModel>>,
}

impl CompiledModelRuntime {
    #[must_use]
    pub fn authored(&self) -> &BTreeMap<ProviderId, ProviderDefinition> {
        &self.authored
    }

    #[must_use]
    pub fn catalog(&self) -> &Arc<CatalogSnapshot> {
        &self.catalog
    }

    #[must_use]
    pub fn store(&self) -> &ProviderStoreSnapshot {
        &self.store
    }

    #[must_use]
    pub fn provider_state_revision(&self) -> ProviderStateRevision {
        self.store.provider_state_revision()
    }

    #[must_use]
    pub fn model_revision(&self) -> &ModelRevision {
        &self.model_revision
    }

    #[must_use]
    pub fn runtime_revision(&self) -> &RuntimeRevision {
        &self.runtime_revision
    }

    #[must_use]
    pub fn providers(&self) -> &[CompiledProviderState] {
        &self.providers
    }

    #[must_use]
    pub fn models(&self) -> &BTreeMap<ModelKey, CompiledRuntimeModel> {
        &self.models
    }

    #[must_use]
    pub fn model(&self, key: &ModelKey) -> Option<&CompiledRuntimeModel> {
        self.models.get(key)
    }

    pub fn resolve(
        &self,
        selection: &ModelSelection,
    ) -> Result<ResolvedExecutableModel, ModelManagerError> {
        self.models
            .get(&selection.model)
            .ok_or_else(|| ModelManagerError::UnknownModel(selection.model.clone()))?
            .resolve(selection)
    }

    pub fn resolve_frozen(
        &self,
        binding: &protocol::FrozenModelBinding,
        blueprint: &CompiledSafeModelBlueprint,
    ) -> Result<ResolvedExecutableModel, ModelManagerError> {
        if !binding.matches_blueprint(blueprint)
            || behavior_fingerprint(blueprint, &binding.selection)? != binding.behavior_fingerprint
            || selection_fingerprint(blueprint, &binding.selection)?
                != binding.selection_fingerprint
        {
            return Err(ModelManagerError::RuntimeCompileFailed);
        }
        if let Some(current) = self.model(&binding.selection.model) {
            let current_blueprint = current.blueprint()?;
            if binding.matches_blueprint(&current_blueprint) {
                let mut resolved = current.resolve(&binding.selection)?;
                resolved.behavior_fingerprint =
                    Sha256Digest::new(binding.behavior_fingerprint.as_str().to_owned())
                        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
                return Ok(resolved);
            }
        }
        compile_frozen_managed(self, binding, blueprint)
    }

    /// Allocates a secret-free schema-1 payload for durable-before-reference storage.
    pub fn manifest_payload(&self) -> Result<ModelSnapshotPayloadV1, ModelManagerError> {
        let mut blueprints = self
            .models
            .values()
            .filter(|model| model.model.status == CompiledModelStatus::Available)
            .map(CompiledRuntimeModel::blueprint)
            .collect::<Result<Vec<_>, _>>()?;
        blueprints.sort_by(|left, right| left.selection.model.cmp(&right.selection.model));
        Ok(ModelSnapshotPayloadV1 {
            catalog_revision: self.catalog.revision.clone(),
            recipe_registry_revision: self.recipe_registry_revision.clone(),
            provider_state_revision: self.store.provider_state_revision(),
            model_revision: self.model_revision.clone(),
            blueprints,
        })
    }
}

impl fmt::Debug for CompiledModelRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledModelRuntime")
            .field("catalog_revision", &self.catalog.revision)
            .field("provider_state_revision", &self.provider_state_revision())
            .field("model_revision", &self.model_revision)
            .field("runtime_revision", &self.runtime_revision)
            .field("providers", &self.providers.len())
            .field("models", &self.models.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl CompiledRuntimeModel {
    pub fn resolve(
        &self,
        selection: &ModelSelection,
    ) -> Result<ResolvedExecutableModel, ModelManagerError> {
        if selection.model != self.key {
            return Err(ModelManagerError::UnknownModel(selection.model.clone()));
        }
        let behavior = match selection.variant.as_ref() {
            Some(variant) => self
                .variant_executables
                .get(variant)
                .ok_or_else(|| ModelManagerError::UnknownVariant(selection.clone()))?,
            None => self
                .executable
                .as_ref()
                .ok_or_else(|| ModelManagerError::ModelUnavailable(self.key.clone()))?,
        };
        Ok(ResolvedExecutableModel {
            selection: selection.clone(),
            adapter: behavior.adapter,
            model: Arc::clone(&behavior.model),
            defaults: behavior.defaults.clone(),
            provider_options: behavior.provider_options.clone(),
            behavior_fingerprint: behavior.behavior_fingerprint.clone(),
        })
    }

    pub fn resolved_defaults(
        &self,
        variant: Option<&cookie_agent_identity::VariantId>,
    ) -> Option<&crate::ResolvedRequestDefaults> {
        match variant {
            Some(variant) => self
                .variant_executables
                .get(variant)
                .map(|value| &value.defaults),
            None => self.executable.as_ref().map(|value| &value.defaults),
        }
    }

    pub fn frozen_behavior(
        &self,
        variant: Option<&cookie_agent_identity::VariantId>,
    ) -> Result<
        (
            protocol::FrozenResolvedRequestDefaults,
            protocol::FrozenProviderOptions,
            protocol::Sha256Digest,
            protocol::Sha256Digest,
        ),
        ModelManagerError,
    > {
        let blueprint = self.blueprint()?;
        let selection = ModelSelection {
            model: self.key.clone(),
            variant: variant.cloned(),
        };
        let behavior = selected_behavior(&blueprint, &selection)
            .ok_or(ModelManagerError::UnknownVariant(selection))?;
        Ok((
            behavior.defaults.clone(),
            behavior.options.clone(),
            behavior.behavior_fingerprint.clone(),
            behavior.selection_fingerprint.clone(),
        ))
    }

    fn blueprint(&self) -> Result<CompiledSafeModelBlueprint, ModelManagerError> {
        let provider_id = self.key.provider_id();
        let source = match &self.source {
            RuntimeProviderSource::Managed {
                family_id,
                source_record_digest,
                recipe_fingerprint,
                package_claim,
            } => FrozenProviderSource::Managed {
                provider_recipe: family_id.clone(),
                source_record_digest: protocol_digest(source_record_digest),
                recipe_fingerprint: protocol_digest(recipe_fingerprint),
                package_claim: package_claim.clone(),
            },
            RuntimeProviderSource::Custom {
                safe_definition_fingerprint,
            } => FrozenProviderSource::Custom {
                safe_definition_fingerprint: protocol_digest(safe_definition_fingerprint),
            },
        };
        let credential_source = match self.credential_source {
            EffectiveCredentialSource::AuthoredApiKey => FrozenCredentialSource::AuthoredApiKey,
            EffectiveCredentialSource::AuthoredOverride => FrozenCredentialSource::AuthoredOverride,
            EffectiveCredentialSource::ProviderStore => FrozenCredentialSource::ProviderStore,
            EffectiveCredentialSource::NoAuth => FrozenCredentialSource::NoAuth,
            EffectiveCredentialSource::Unavailable => {
                return Err(ModelManagerError::RuntimeCompileFailed);
            }
        };
        let setup_values = protocol_setup_values(&self.setup_values)?;
        let defaults = frozen_defaults(&crate::ResolvedRequestDefaults {
            request: self.model.defaults.clone(),
            reasoning: None,
        })?;
        let options = protocol_options(&self.model, &self.setup_values)?;
        let descriptor = safe_descriptor(&provider_id, &self.model)?;
        let mut variants = self
            .model
            .variants
            .iter()
            .map(|(id, variant)| {
                Ok(FrozenVariantBlueprint {
                    id: id.clone(),
                    descriptor: descriptor.clone(),
                    defaults: frozen_defaults(&crate::ResolvedRequestDefaults {
                        request: variant.defaults.clone(),
                        reasoning: variant.reasoning.clone(),
                    })?,
                    options: {
                        let mut selected = self.model.clone();
                        selected.options = variant.options.clone();
                        protocol_options(&selected, &self.setup_values)?
                    },
                    behavior_fingerprint: protocol::Sha256Digest::of_bytes(b"pending"),
                    selection_fingerprint: protocol::Sha256Digest::of_bytes(b"pending"),
                })
            })
            .collect::<Result<Vec<_>, ModelManagerError>>()?;
        variants.sort_by(|left, right| left.id.cmp(&right.id));
        let static_headers = self
            .static_headers
            .iter()
            .map(|(name, value)| {
                Ok((
                    HeaderName::new(name.as_str())
                        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                    SafeStaticHeaderValue::new(value.as_str())
                        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, ModelManagerError>>()?;
        let setup_recipe = self.setup_recipe.clone();
        let auth_method = AuthMethodId::new(self.model.auth.method.clone())
            .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
        let family_id = ProviderRecipeId::new(self.model.family_id.clone())
            .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
        let protocol_recipe = ProtocolRecipeId::new(self.model.adapter_id.clone())
            .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
        let compiler_version = RecipeCompilerVersion::new(COMPILER_VERSION)
            .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
        let endpoint = self
            .model
            .endpoint
            .as_ref()
            .ok_or(ModelManagerError::RuntimeCompileFailed)?;
        let mut blueprint = CompiledSafeModelBlueprint {
            blueprint_fingerprint: protocol::Sha256Digest::of_bytes(b"pending"),
            selection: ModelSelection {
                model: self.key.clone(),
                variant: None,
            },
            source,
            config_override_fingerprint: protocol_digest(&self.config_override_fingerprint),
            setup_binding: FrozenSetupBinding {
                setup_recipe: setup_recipe.clone(),
                values: setup_values,
                setup_fingerprint: protocol_digest(&self.setup_fingerprint),
            },
            credential_binding: FrozenCredentialBinding {
                source: credential_source,
                auth_method: auth_method.clone(),
                fields: self
                    .model
                    .auth
                    .credential_fields
                    .iter()
                    .map(|field| AuthFieldName::new(field.clone()))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                parameters: self
                    .model
                    .auth
                    .safe_parameters
                    .iter()
                    .map(|(name, value)| {
                        Ok((
                            AuthParameterId::new(name.clone())
                                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                            FrozenAuthParameterValue::new(value.clone())
                                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                        ))
                    })
                    .collect::<Result<_, ModelManagerError>>()?,
                owned_headers: self
                    .model
                    .auth
                    .owned_headers
                    .iter()
                    .map(|header| {
                        HeaderName::new(header.clone())
                            .map_err(|_| ModelManagerError::RuntimeCompileFailed)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            },
            endpoint_identity: SafeEndpointIdentity::new(endpoint.clone())
                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            provider_recipe: family_id,
            protocol_recipe,
            setup_recipe,
            auth_method,
            compiler_version,
            descriptor,
            defaults,
            options,
            static_headers,
            variants,
            behavior_fingerprint: protocol::Sha256Digest::of_bytes(b"pending"),
            selection_fingerprint: protocol::Sha256Digest::of_bytes(b"pending"),
        };
        blueprint.credential_binding.fields.sort();
        blueprint.credential_binding.owned_headers.sort();
        blueprint.behavior_fingerprint = behavior_fingerprint(&blueprint, &blueprint.selection)?;
        blueprint.selection_fingerprint = selection_fingerprint(&blueprint, &blueprint.selection)?;
        for index in 0..blueprint.variants.len() {
            let selection = ModelSelection {
                model: blueprint.selection.model.clone(),
                variant: Some(blueprint.variants[index].id.clone()),
            };
            blueprint.variants[index].behavior_fingerprint =
                behavior_fingerprint(&blueprint, &selection)?;
            blueprint.variants[index].selection_fingerprint =
                selection_fingerprint(&blueprint, &selection)?;
        }
        blueprint.blueprint_fingerprint = blueprint_fingerprint(&blueprint)?;
        Ok(blueprint)
    }
}

/// High-level connect request. Policy and store expectations are manager-owned.
#[derive(Clone, Debug)]
pub struct ProviderConnectRequest {
    pub provider_id: ProviderId,
    pub expected_catalog_revision: CatalogRevision,
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub auth_method: AuthMethodId,
    pub auth_values: ProviderAuthValues,
    pub client_connect_id: ClientConnectId,
}

#[derive(Clone, Debug)]
pub struct ProviderDisconnectRequest {
    pub provider_id: ProviderId,
    pub expected_runtime_revision: RuntimeRevision,
    pub expected_provider_state_revision: ProviderStateRevision,
    pub expected_connection_generation: Option<ProviderConnectionGeneration>,
    pub client_request_id: ClientRequestId,
}

/// A mutation result whose optional publication was fully prepared before commit.
#[derive(Debug)]
pub struct ModelMutationResult<P> {
    pub mutation: ProviderStoreMutation,
    pub runtime: Arc<CompiledModelRuntime>,
    pub effective_auth: EffectiveCredentialSource,
    pub replayed: bool,
    pub publication: Option<P>,
}

/// Atomic manager for current and retained exact compiled model runtimes.
pub struct ModelManager {
    store: ProviderStore,
    current: ArcSwap<CompiledModelRuntime>,
    retained: ArcSwap<BTreeMap<ModelRevision, Vec<Arc<CompiledModelRuntime>>>>,
    mutation: Mutex<()>,
}

impl ModelManager {
    pub fn new(
        authored: BTreeMap<ProviderId, ProviderDefinition>,
        catalog: Arc<CatalogSnapshot>,
        store: ProviderStore,
    ) -> Result<Self, ModelManagerError> {
        let store_snapshot = store.load()?;
        let initial = Arc::new(compile_runtime(
            Arc::new(authored),
            catalog,
            store_snapshot,
        )?);
        let retained = Arc::new(BTreeMap::from([(
            initial.model_revision.clone(),
            vec![Arc::clone(&initial)],
        )]));
        Ok(Self {
            store,
            current: ArcSwap::from(initial),
            retained: ArcSwap::from(retained),
            mutation: Mutex::new(()),
        })
    }

    #[must_use]
    pub fn current(&self) -> Arc<CompiledModelRuntime> {
        self.current.load_full()
    }

    #[must_use]
    pub fn retained(&self, revision: &ModelRevision) -> Option<Arc<CompiledModelRuntime>> {
        self.retained
            .load()
            .get(revision)
            .and_then(|snapshots| snapshots.last())
            .cloned()
    }

    #[must_use]
    pub fn retained_all(&self, revision: &ModelRevision) -> Vec<Arc<CompiledModelRuntime>> {
        self.retained
            .load()
            .get(revision)
            .cloned()
            .unwrap_or_default()
    }

    pub fn connect<P>(
        &self,
        request: ProviderConnectRequest,
        prepare_publication: impl FnOnce(
            &Arc<CompiledModelRuntime>,
            &ProviderStoreMutation,
        ) -> Result<P, ModelManagerError>,
    ) -> Result<ModelMutationResult<P>, ModelManagerError> {
        let _guard = lock(&self.mutation);
        let current = self.current();
        let transaction = self.store.begin_transaction()?;
        let store_snapshot = transaction.snapshot();
        let mutation = normalize_connect(&current, &store_snapshot, request)?;
        match transaction.propose_connect(&mutation, &current.catalog.revision)? {
            ConnectProposal::Replay(replayed) => {
                let mutation = *replayed;
                Ok(ModelMutationResult {
                    effective_auth: effective_auth_for(&current, mutation_provider(&mutation)),
                    mutation,
                    runtime: current,
                    replayed: true,
                    publication: None,
                })
            }
            ConnectProposal::Proposed(proposal) => {
                let candidate = Arc::new(compile_runtime(
                    Arc::clone(&current.authored),
                    Arc::clone(&current.catalog),
                    proposal.snapshot(),
                )?);
                let mutation = proposal.mutation().clone();
                let publication = prepare_publication(&candidate, &mutation)?;
                let retained = prepared_retained(&self.retained.load_full(), &candidate);
                let effective_auth = effective_auth_for(&candidate, mutation_provider(&mutation));
                transaction.commit(*proposal)?;
                self.retained.store(retained);
                self.current.store(Arc::clone(&candidate));
                Ok(ModelMutationResult {
                    mutation,
                    runtime: candidate,
                    effective_auth,
                    replayed: false,
                    publication: Some(publication),
                })
            }
        }
    }

    pub fn disconnect<P>(
        &self,
        request: ProviderDisconnectRequest,
        prepare_publication: impl FnOnce(
            &Arc<CompiledModelRuntime>,
            &ProviderStoreMutation,
        ) -> Result<P, ModelManagerError>,
    ) -> Result<ModelMutationResult<P>, ModelManagerError> {
        let _guard = lock(&self.mutation);
        let current = self.current();
        if matches!(
            current.authored.get(&request.provider_id),
            Some(ProviderDefinition::Custom(_))
        ) {
            return Err(ModelManagerError::CustomProviderNotStoreBacked);
        }
        let transaction = self.store.begin_transaction()?;
        let snapshot = transaction.snapshot();
        let mutation = DisconnectMutation {
            client_request_id: request.client_request_id,
            provider_id: request.provider_id,
            expected_runtime_revision: request.expected_runtime_revision,
            expected_provider_state_revision: request.expected_provider_state_revision,
            expected_store_generation: snapshot.generation(),
            expected_store_revision: snapshot.store_revision().clone(),
            expected_connection_generation: request.expected_connection_generation,
        };
        match transaction.propose_disconnect(&mutation, current.runtime_revision())? {
            DisconnectProposal::Replay(replayed) => {
                let mutation = *replayed;
                Ok(ModelMutationResult {
                    effective_auth: effective_auth_for(&current, mutation_provider(&mutation)),
                    mutation,
                    runtime: current,
                    replayed: true,
                    publication: None,
                })
            }
            DisconnectProposal::Proposed(proposal) => {
                let candidate = Arc::new(compile_runtime(
                    Arc::clone(&current.authored),
                    Arc::clone(&current.catalog),
                    proposal.snapshot(),
                )?);
                let mutation = proposal.mutation().clone();
                let publication = prepare_publication(&candidate, &mutation)?;
                let retained = prepared_retained(&self.retained.load_full(), &candidate);
                let effective_auth = effective_auth_for(&candidate, mutation_provider(&mutation));
                transaction.commit(*proposal)?;
                self.retained.store(retained);
                self.current.store(Arc::clone(&candidate));
                Ok(ModelMutationResult {
                    mutation,
                    runtime: candidate,
                    effective_auth,
                    replayed: false,
                    publication: Some(publication),
                })
            }
        }
    }

    /// Recompiles and publishes another process's newer provider-store generation.
    pub fn reload_store_if_changed<P>(
        &self,
        prepare_publication: impl FnOnce(&Arc<CompiledModelRuntime>) -> Result<P, ModelManagerError>,
    ) -> Result<Option<(Arc<CompiledModelRuntime>, P)>, ModelManagerError> {
        let _guard = lock(&self.mutation);
        let current = self.current();
        let Some(store) = self.store.reload_if_changed(current.store.generation())? else {
            return Ok(None);
        };
        let candidate = Arc::new(compile_runtime(
            Arc::clone(&current.authored),
            Arc::clone(&current.catalog),
            store,
        )?);
        let publication = prepare_publication(&candidate)?;
        let retained = prepared_retained(&self.retained.load_full(), &candidate);
        self.retained.store(retained);
        self.current.store(Arc::clone(&candidate));
        Ok(Some((candidate, publication)))
    }

    /// Atomically recompiles a config/catalog replacement before publication.
    pub fn reload_inputs<P>(
        &self,
        authored: BTreeMap<ProviderId, ProviderDefinition>,
        catalog: Arc<CatalogSnapshot>,
        prepare_publication: impl FnOnce(&Arc<CompiledModelRuntime>) -> Result<P, ModelManagerError>,
    ) -> Result<(Arc<CompiledModelRuntime>, P), ModelManagerError> {
        let _guard = lock(&self.mutation);
        let store = self.store.load()?;
        let candidate = Arc::new(compile_runtime(Arc::new(authored), catalog, store)?);
        let publication = prepare_publication(&candidate)?;
        let retained = prepared_retained(&self.retained.load_full(), &candidate);
        self.retained.store(retained);
        self.current.store(Arc::clone(&candidate));
        Ok((candidate, publication))
    }
}

impl fmt::Debug for ModelManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelManager")
            .field("current", &self.current())
            .field("retained", &self.retained.load().len())
            .finish_non_exhaustive()
    }
}

fn compile_runtime(
    authored: Arc<BTreeMap<ProviderId, ProviderDefinition>>,
    catalog: Arc<CatalogSnapshot>,
    store: ProviderStoreSnapshot,
) -> Result<CompiledModelRuntime, ModelManagerError> {
    let registry = family_registry();
    let compiler = DynamicCompiler::family_registry();
    let mut provider_ids = catalog.providers.keys().cloned().collect::<BTreeSet<_>>();
    provider_ids.extend(
        authored
            .iter()
            .filter(|(_, definition)| matches!(definition, ProviderDefinition::ModelsDev(_)))
            .map(|(id, _)| id.clone()),
    );
    provider_ids.extend(store.providers().keys().cloned());

    let mut providers = Vec::new();
    let mut models = BTreeMap::new();
    for provider_id in provider_ids {
        let authored_managed = match authored.get(&provider_id) {
            Some(ProviderDefinition::ModelsDev(provider)) => Some(provider),
            _ => None,
        };
        let stored = store.provider(&provider_id);
        let entry = catalog.provider(&provider_id);
        let presence = if entry.is_some() {
            ProviderPresence::Current
        } else {
            ProviderPresence::Removed
        };
        let mut state = CompiledProviderState {
            id: provider_id.clone(),
            display_name: entry
                .and_then(|entry| entry.record.as_ref())
                .map_or_else(|| provider_id.to_string(), |record| record.name.clone()),
            presence,
            support_reason: None,
            retained_family_match: None,
            authored: authored_managed.is_some(),
            stored: stored.is_some(),
            effective_auth: EffectiveCredentialSource::Unavailable,
            durable_connection: stored.map(StoredManagedConnection::descriptor),
        };
        let Some(record) = entry.and_then(|entry| entry.record.as_ref()) else {
            if let Some(reason) = entry.and_then(|entry| entry.quarantine.as_ref()) {
                state.support_reason = Some(reason.code().to_owned());
            } else {
                let retained_match = stored.map_or(
                    RetainedFamilyMatch::RemovedWithoutRetainedFamilyMatch,
                    |connection| retained_family_match(&provider_id, connection),
                );
                state.retained_family_match = Some(retained_match);
                state.support_reason = retained_match.reason().map(str::to_owned);
                if retained_match == RetainedFamilyMatch::SupportedRemoved {
                    state.effective_auth = EffectiveCredentialSource::ProviderStore;
                }
            }
            providers.push(state);
            continue;
        };
        let Some(family) = registry.classify(record) else {
            state.support_reason = Some("no_known_protocol_family".to_owned());
            providers.push(state);
            continue;
        };
        let effective = effective_managed(&provider_id, record, family, authored_managed, stored)?;
        state.effective_auth = effective.credential_source;
        let compiled =
            compiler.compile_managed(catalog.revision.as_str(), record, Some(&effective.provider));
        let compiled = match compiled {
            Ok(compiled) => compiled,
            Err(DynamicCompileError::UnsupportedProvider) => {
                state.support_reason = Some("no_known_protocol_family".to_owned());
                providers.push(state);
                continue;
            }
            Err(error) if authored_managed.is_none() && stored.is_none() => {
                state.support_reason = Some(error.to_string());
                providers.push(state);
                continue;
            }
            Err(error) => return Err(ModelManagerError::DynamicCompile(error)),
        };
        for (model_id, model) in compiled.models {
            let key = ModelKey::new(provider_id.clone(), model_id)
                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
            let model = with_effective_status(model, effective.credential_source);
            let model_credentials = if model.status == CompiledModelStatus::Available {
                mapped_credentials(&effective.credentials, &model.auth)?
            } else {
                ExecutableCredentialMaterial {
                    method: model.auth.method.clone(),
                    values: BTreeMap::new(),
                }
            };
            let (executable, variant_executables) =
                compile_behaviors(&provider_id, &model, &BTreeMap::new(), &model_credentials)?;
            let model_recipe = family_registry()
                .by_npm(&model.effective_npm)
                .ok_or(ModelManagerError::RuntimeCompileFailed)?;
            let runtime_model = CompiledRuntimeModel {
                key: key.clone(),
                source: RuntimeProviderSource::Managed {
                    family_id: ProviderRecipeId::new(model.family_id.clone())
                        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                    source_record_digest: effective.source_record_digest.clone(),
                    recipe_fingerprint: retained_recipe_fingerprint(
                        model_recipe,
                        &model.auth.method,
                    )?,
                    package_claim: model.effective_npm.clone(),
                },
                config_override_fingerprint: effective.config_fingerprint.clone(),
                setup_values: effective.setup_values.clone(),
                setup_fingerprint: effective.setup_fingerprint.clone(),
                setup_recipe: ProviderSetupRecipeId::new("family-derived-setup-v1")
                    .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                credential_source: effective.credential_source,
                static_headers: BTreeMap::new(),
                model,
                executable,
                variant_executables,
            };
            if models.insert(key.clone(), runtime_model).is_some() {
                return Err(ModelManagerError::DuplicateModel(key));
            }
        }
        providers.push(state);
    }

    for (provider_id, definition) in authored.iter() {
        let ProviderDefinition::Custom(custom) = definition else {
            continue;
        };
        let compiled = compiler.compile_custom(provider_id, custom)?;
        let fingerprint = safe_definition_fingerprint(provider_id, definition);
        let setup_recipe = crate::adapters::custom_setup_recipe(
            OvenAdapterFamily::parse(custom.adaptor.as_str())
                .ok_or(ModelManagerError::RuntimeCompileFailed)?,
        );
        let setup_values = normalized_setup_values(setup_recipe, &custom.setup)?;
        let setup_fingerprint = setup_fingerprint(&setup_values);
        let credential_source = if custom.auth.method.as_str() == "no-auth-v1" {
            EffectiveCredentialSource::NoAuth
        } else {
            EffectiveCredentialSource::AuthoredOverride
        };
        let credentials = ExecutableCredentialMaterial {
            method: custom.auth.method.as_str().to_owned(),
            values: custom
                .auth
                .values
                .iter()
                .map(|(field, value)| (field.clone(), value.expose().to_owned()))
                .collect(),
        };
        for (model_id, model) in compiled.models {
            let key = ModelKey::new(provider_id.clone(), model_id)
                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
            let (executable, variant_executables) =
                compile_behaviors(provider_id, &model, &custom.headers, &credentials)?;
            let runtime_model = CompiledRuntimeModel {
                key: key.clone(),
                model,
                source: RuntimeProviderSource::Custom {
                    safe_definition_fingerprint: fingerprint.clone(),
                },
                config_override_fingerprint: fingerprint.clone(),
                setup_values: setup_values.clone(),
                setup_fingerprint: setup_fingerprint.clone(),
                setup_recipe: ProviderSetupRecipeId::new(setup_recipe.id)
                    .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                credential_source,
                static_headers: custom.headers.clone(),
                executable,
                variant_executables,
            };
            if models.insert(key.clone(), runtime_model).is_some() {
                return Err(ModelManagerError::DuplicateModel(key));
            }
        }
    }

    providers.sort_by(|left, right| left.id.cmp(&right.id));
    let model_revision = revision::<ModelRevision, _>(
        "cookie-agent/model-runtime/v1",
        &models
            .iter()
            .map(|(key, model)| {
                (
                    key,
                    &model.model.behavior_fingerprint,
                    model.credential_source,
                    &model.setup_fingerprint,
                    &model.config_override_fingerprint,
                )
            })
            .collect::<Vec<_>>(),
        ModelRevision::new,
    )?;
    let runtime_revision = revision::<RuntimeRevision, _>(
        "cookie-agent/model-manager-runtime/v1",
        &(
            registry.revision(),
            &catalog.revision,
            store.provider_state_revision(),
            &model_revision,
        ),
        RuntimeRevision::new,
    )?;
    Ok(CompiledModelRuntime {
        authored,
        catalog,
        store,
        recipe_registry_revision: registry.revision(),
        model_revision,
        runtime_revision,
        providers: Arc::new(providers),
        models: Arc::new(models),
    })
}

struct EffectiveManaged {
    provider: ModelsDevProvider,
    credential_source: EffectiveCredentialSource,
    setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    setup_fingerprint: Sha256Digest,
    source_record_digest: Sha256Digest,
    config_fingerprint: Sha256Digest,
    credentials: ExecutableCredentialMaterial,
}

fn effective_managed(
    provider_id: &ProviderId,
    record: &crate::catalog::CatalogProviderRecord,
    recipe: &'static FamilyRecipe,
    authored: Option<&ModelsDevProvider>,
    stored: Option<&StoredManagedConnection>,
) -> Result<EffectiveManaged, ModelManagerError> {
    let source_record_digest = validated_provider_source_digest(record)?;
    let config_fingerprint = authored.map_or_else(no_authored_override_fingerprint, |authored| {
        safe_definition_fingerprint(
            provider_id,
            &ProviderDefinition::ModelsDev(authored.clone()),
        )
    });
    let authored_setup = authored.filter(|value| !value.setup.is_empty());
    let stored_endpoint = stored
        .map(|connection| resolved_provider_endpoint(record, recipe, &connection.setup_values))
        .transpose()?;
    let store_eligible = authored.is_none_or(|value| value.base_url.is_none())
        && stored.is_some_and(|connection| {
            retained_recipe_for_endpoint(connection, provider_id, stored_endpoint.as_deref())
                .is_some_and(|retained| retained.family == recipe.family)
        });
    let setup_input = authored_setup.map(|value| &value.setup).or_else(|| {
        store_eligible
            .then_some(stored)
            .flatten()
            .map(|value| &value.setup_values)
    });
    let setup_values = setup_input.cloned().unwrap_or_default();
    let setup_fingerprint = setup_fingerprint(&setup_values);

    let credential_source = if authored.is_some_and(|value| value.api_key.is_some()) {
        EffectiveCredentialSource::AuthoredApiKey
    } else if authored.is_some_and(|value| value.auth_override.is_some()) {
        EffectiveCredentialSource::AuthoredOverride
    } else if store_eligible
        && stored.is_some_and(|connection| {
            connection.setup_fingerprint == setup_fingerprint
                && recipe
                    .allowed_auth_methods
                    .contains(&connection.auth_method.as_str())
        })
    {
        EffectiveCredentialSource::ProviderStore
    } else if auth_method(recipe.default_auth_method)
        .is_some_and(|method| method.credentials.is_empty())
    {
        EffectiveCredentialSource::NoAuth
    } else {
        EffectiveCredentialSource::Unavailable
    };

    let mut provider = authored.cloned().unwrap_or(ModelsDevProvider {
        base_url: None,
        setup: BTreeMap::new(),
        api_key: None,
        auth_override: None,
        shape: None,
        model_overrides: BTreeMap::new(),
    });
    provider.setup = setup_values.clone();
    let credentials = match credential_source {
        EffectiveCredentialSource::AuthoredApiKey => ExecutableCredentialMaterial {
            method: recipe.default_auth_method.to_owned(),
            values: BTreeMap::from([(
                AuthFieldName::new("api_key")
                    .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                authored
                    .and_then(|value| value.api_key.as_ref())
                    .ok_or(ModelManagerError::RuntimeCompileFailed)?
                    .expose()
                    .to_owned(),
            )]),
        },
        EffectiveCredentialSource::AuthoredOverride => {
            let override_ = authored
                .and_then(|value| value.auth_override.as_ref())
                .ok_or(ModelManagerError::RuntimeCompileFailed)?;
            ExecutableCredentialMaterial {
                method: override_.method.as_str().to_owned(),
                values: override_
                    .values
                    .iter()
                    .map(|(field, value)| (field.clone(), value.expose().to_owned()))
                    .collect(),
            }
        }
        EffectiveCredentialSource::ProviderStore => {
            let connection = stored.ok_or(ModelManagerError::RuntimeCompileFailed)?;
            ExecutableCredentialMaterial {
                method: connection.auth_method.as_str().to_owned(),
                values: connection
                    .credential_fields()
                    .map(|field| {
                        Ok((
                            field.clone(),
                            connection
                                .credential(field)
                                .ok_or(ModelManagerError::RuntimeCompileFailed)?
                                .to_owned(),
                        ))
                    })
                    .collect::<Result<_, ModelManagerError>>()?,
            }
        }
        EffectiveCredentialSource::NoAuth => ExecutableCredentialMaterial {
            method: recipe.default_auth_method.to_owned(),
            values: BTreeMap::new(),
        },
        EffectiveCredentialSource::Unavailable => ExecutableCredentialMaterial {
            method: recipe.default_auth_method.to_owned(),
            values: BTreeMap::new(),
        },
    };
    if credential_source == EffectiveCredentialSource::ProviderStore {
        let connection = stored.expect("store credential source has a connection");
        provider.api_key = None;
        provider.auth_override = Some(AuthOverride {
            method: connection.auth_method.clone(),
            values: connection
                .credential_fields()
                .map(|field| {
                    Ok((
                        field.clone(),
                        SecretString::new("present")
                            .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                    ))
                })
                .collect::<Result<_, ModelManagerError>>()?,
        });
    } else if credential_source == EffectiveCredentialSource::NoAuth {
        provider.api_key = None;
        provider.auth_override = Some(AuthOverride {
            method: AuthMethodId::new(recipe.default_auth_method)
                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            values: BTreeMap::new(),
        });
    }
    Ok(EffectiveManaged {
        provider,
        credential_source,
        setup_values,
        setup_fingerprint,
        source_record_digest,
        config_fingerprint,
        credentials,
    })
}

fn compile_behaviors(
    provider_id: &ProviderId,
    model: &CompiledDynamicModel,
    static_headers: &BTreeMap<crate::HeaderName, crate::SafeStaticHeaderValue>,
    credentials: &ExecutableCredentialMaterial,
) -> Result<
    (
        Option<ExecutableBehavior>,
        BTreeMap<cookie_agent_identity::VariantId, ExecutableBehavior>,
    ),
    ModelManagerError,
> {
    if model.status != CompiledModelStatus::Available {
        return Ok((None, BTreeMap::new()));
    }
    let headers = static_headers
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.as_str().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let capabilities = oven_capabilities(&model.capabilities, model.adapter)?;
    let base = compile_executable(
        provider_id.as_str(),
        model,
        capabilities.clone(),
        headers.clone(),
        credentials,
        ExecutableBehaviorInput {
            defaults: &model.defaults,
            options: &model.options,
            reasoning: None,
        },
    )?;
    let executable = ExecutableBehavior {
        adapter: model.adapter,
        model: base.model,
        defaults: crate::ResolvedRequestDefaults {
            request: model.defaults.clone(),
            reasoning: None,
        },
        provider_options: base.provider_options,
        behavior_fingerprint: model.behavior_fingerprint.clone(),
    };
    let variants = model
        .variants
        .iter()
        .map(|(id, variant)| {
            let compiled = compile_executable(
                provider_id.as_str(),
                model,
                capabilities.clone(),
                headers.clone(),
                credentials,
                ExecutableBehaviorInput {
                    defaults: &variant.defaults,
                    options: &variant.options,
                    reasoning: variant.reasoning.as_ref(),
                },
            )?;
            let behavior_fingerprint = safe_hash(
                "cookie-agent/model-variant/v1",
                &(id, &variant.defaults, &variant.options, &variant.reasoning),
            );
            Ok((
                id.clone(),
                ExecutableBehavior {
                    adapter: model.adapter,
                    model: compiled.model,
                    defaults: crate::ResolvedRequestDefaults {
                        request: variant.defaults.clone(),
                        reasoning: variant.reasoning.clone(),
                    },
                    provider_options: compiled.provider_options,
                    behavior_fingerprint,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>, ModelManagerError>>()?;
    Ok((Some(executable), variants))
}

fn mapped_credentials(
    source: &ExecutableCredentialMaterial,
    target: &CompiledAuthShape,
) -> Result<ExecutableCredentialMaterial, ModelManagerError> {
    if target.source == AuthSourceCategory::Unavailable {
        return Ok(ExecutableCredentialMaterial {
            method: target.method.clone(),
            values: BTreeMap::new(),
        });
    }
    let values = target
        .credential_fields
        .iter()
        .map(|target_field| {
            let source_field =
                crate::recipes::compatible_credential_field(&source.method, target_field)
                    .ok_or(ModelManagerError::RuntimeCompileFailed)?;
            let value = source
                .values
                .iter()
                .find(|(field, _)| field.as_str() == source_field)
                .map(|(_, value)| value.clone());
            if value.is_none()
                && auth_method(&target.method).is_some_and(|method| {
                    method
                        .credentials
                        .iter()
                        .any(|field| field.name == target_field && field.required)
                })
            {
                return Err(ModelManagerError::RuntimeCompileFailed);
            }
            value
                .map(|value| {
                    AuthFieldName::new(target_field.clone())
                        .map(|field| (field, value))
                        .map_err(|_| ModelManagerError::RuntimeCompileFailed)
                })
                .transpose()
        })
        .filter_map(Result::transpose)
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    Ok(ExecutableCredentialMaterial {
        method: target.method.clone(),
        values,
    })
}

fn compile_frozen_managed(
    runtime: &CompiledModelRuntime,
    binding: &protocol::FrozenModelBinding,
    blueprint: &CompiledSafeModelBlueprint,
) -> Result<ResolvedExecutableModel, ModelManagerError> {
    if !matches!(blueprint.source, FrozenProviderSource::Managed { .. }) {
        return Err(ModelManagerError::ModelUnavailable(
            binding.selection.model.clone(),
        ));
    }
    let provider_id = binding.selection.model.provider_id();
    family_registry()
        .by_npm(match &blueprint.source {
            FrozenProviderSource::Managed { package_claim, .. } => package_claim,
            FrozenProviderSource::Custom { .. } => {
                return Err(ModelManagerError::RuntimeCompileFailed);
            }
        })
        .ok_or(ModelManagerError::RuntimeCompileFailed)?;
    let adapter = adapter_for_protocol(blueprint.protocol_recipe.as_str())
        .ok_or(ModelManagerError::RuntimeCompileFailed)?;
    let frozen_behavior = selected_behavior(blueprint, &binding.selection)
        .ok_or(ModelManagerError::RuntimeCompileFailed)?;
    let setup_values = blueprint
        .setup_binding
        .values
        .iter()
        .map(|(id, value)| {
            let value = match value {
                protocol::SafeSetupValue::String(value) => value.as_str(),
                protocol::SafeSetupValue::Code(value) => value.as_str(),
                protocol::SafeSetupValue::Integer(_) | protocol::SafeSetupValue::Bool(_) => {
                    return Err(ModelManagerError::RuntimeCompileFailed);
                }
            };
            Ok((id.as_str().to_owned(), value.to_owned()))
        })
        .collect::<Result<BTreeMap<_, _>, ModelManagerError>>()?;
    let defaults = thaw_defaults(frozen_behavior.defaults)?;
    let options = thaw_provider_options(frozen_behavior.options)?;
    let capabilities = thaw_capabilities(&frozen_behavior.descriptor.capabilities);
    let model = CompiledDynamicModel {
        id: binding.selection.model.model_id(),
        display_name: binding.selection.model.to_string(),
        family_id: blueprint.provider_recipe.as_str().to_owned(),
        effective_npm: match &blueprint.source {
            FrozenProviderSource::Managed { package_claim, .. } => package_claim.clone(),
            FrozenProviderSource::Custom { .. } => "custom".to_owned(),
        },
        adapter_id: blueprint.protocol_recipe.as_str().to_owned(),
        resolved_shape: if matches!(
            adapter,
            OvenAdapterFamily::OpenaiResponses | OvenAdapterFamily::AzureOpenaiResponses
        ) {
            "responses"
        } else {
            "chat"
        }
        .to_owned(),
        reasoning_field: "reasoning_content".to_owned(),
        adapter,
        endpoint: Some(blueprint.endpoint_identity.as_str().to_owned()),
        setup: Some(crate::recipes::ValidatedSetup {
            recipe_id: "family-derived-setup-v1",
            values: setup_values,
        }),
        auth: CompiledAuthShape {
            method: blueprint.auth_method.as_str().to_owned(),
            safe_parameters: blueprint
                .credential_binding
                .parameters
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.as_str().to_owned()))
                .collect(),
            credential_fields: blueprint
                .credential_binding
                .fields
                .iter()
                .map(|field| field.as_str().to_owned())
                .collect(),
            owned_headers: blueprint
                .credential_binding
                .owned_headers
                .iter()
                .map(|header| header.as_str().to_owned())
                .collect(),
            source: AuthSourceCategory::Unavailable,
        },
        capabilities,
        defaults: defaults.request.clone(),
        options: options.clone(),
        cost: None,
        variants: BTreeMap::new(),
        variant_order: Vec::new(),
        default_variant: None,
        status: CompiledModelStatus::Available,
        behavior_fingerprint: Sha256Digest::new(
            frozen_behavior
                .behavior_fingerprint
                .as_str()
                .strip_prefix("sha256:")
                .unwrap_or(frozen_behavior.behavior_fingerprint.as_str()),
        )
        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
    };
    let credentials = frozen_credentials(runtime, blueprint)?;
    let headers = blueprint
        .static_headers
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.as_str().to_owned()))
        .collect();
    let compiled = compile_executable(
        provider_id.as_str(),
        &model,
        frozen_behavior.descriptor.capabilities.clone(),
        headers,
        &credentials,
        ExecutableBehaviorInput {
            defaults: &defaults.request,
            options: &options,
            reasoning: defaults.reasoning.as_ref(),
        },
    )?;
    Ok(ResolvedExecutableModel {
        selection: binding.selection.clone(),
        adapter: adapter_for_protocol(binding.protocol_recipe.as_str())
            .ok_or(ModelManagerError::RuntimeCompileFailed)?,
        model: compiled.model,
        defaults,
        provider_options: compiled.provider_options,
        behavior_fingerprint: model.behavior_fingerprint,
    })
}

fn adapter_for_protocol(value: &str) -> Option<OvenAdapterFamily> {
    crate::adapters::wire_adapter_for_protocol(value).or_else(|| {
        if value.starts_with("oven.openai-compatible.chat.") {
            Some(OvenAdapterFamily::OpenaiCompatible)
        } else if value.starts_with("oven.anthropic-compatible.messages.") {
            Some(OvenAdapterFamily::AnthropicCompatible)
        } else {
            None
        }
    })
}

fn thaw_capabilities(value: &OvenCapabilities) -> crate::ModelCapabilities {
    crate::ModelCapabilities {
        input: BTreeSet::from([crate::Modality::Text]),
        output: BTreeSet::from([crate::Modality::Text]),
        context_tokens: value.limits.context.unwrap_or(1),
        output_tokens: value.limits.output.unwrap_or(1),
        tool_calling: value.features.contains(Capability::TOOL_CALLING),
        parallel_tool_calls: value.features.contains(Capability::PARALLEL_TOOLS),
        structured_output: value.features.contains(Capability::STRUCTURED_OUTPUT),
        reasoning: value.features.contains(Capability::REASONING),
        temperature: value.features.contains(Capability::TEMPERATURE),
        top_p: value.features.contains(Capability::TOP_P),
        seed: false,
        compaction: match value.compaction {
            OvenCompaction::Unsupported => crate::CompactionCapability::Unsupported,
            OvenCompaction::Native => crate::CompactionCapability::Native,
        },
        native_replay: match value.replay.capability {
            OvenReplay::Unsupported => crate::ReplayCapability::Unsupported,
            OvenReplay::Optional => crate::ReplayCapability::Optional,
            OvenReplay::Required => crate::ReplayCapability::Required,
        },
        cancellation: match value.cancellation {
            OvenCancellation::Unsupported | OvenCancellation::LocalOnly => {
                crate::CancellationCapability::LocalOnly
            }
            OvenCancellation::RemoteBestEffort => crate::CancellationCapability::Provider,
        },
        media: BTreeMap::new(),
    }
}

fn thaw_provider_options(
    value: &protocol::FrozenProviderOptions,
) -> Result<crate::ProviderOptions, ModelManagerError> {
    let mut options = crate::ProviderOptions::default();
    match value {
        protocol::ProviderOptions::Anthropic { api_version, beta } => {
            options.api_version.clone_from(api_version);
            options.beta.clone_from(beta);
        }
        protocol::ProviderOptions::OpenAiChat {
            organization,
            project,
        } => {
            options.organization.clone_from(organization);
            options.project.clone_from(project);
        }
        protocol::ProviderOptions::OpenAiResponses {
            organization,
            project,
            store,
        } => {
            options.organization.clone_from(organization);
            options.project.clone_from(project);
            options.store = *store;
        }
        protocol::ProviderOptions::OpenAiCompatible { api_path } => {
            options.api_path.clone_from(api_path);
        }
        protocol::ProviderOptions::GoogleGemini { api_version }
        | protocol::ProviderOptions::CohereV2Chat { api_version } => {
            options.api_version.clone_from(api_version);
        }
        protocol::ProviderOptions::GoogleVertexGemini { project, location } => {
            options.project = Some(project.clone());
            options.location = Some(location.clone());
        }
        protocol::ProviderOptions::AwsBedrockConverse { region } => {
            options.region = Some(region.clone());
        }
        protocol::ProviderOptions::AzureOpenAiChat {
            deployment,
            api_version,
        }
        | protocol::ProviderOptions::AzureOpenAiResponses {
            deployment,
            api_version,
        } => {
            options.deployment = Some(deployment.clone());
            options.api_version = Some(api_version.clone());
        }
    }
    Ok(options)
}

fn frozen_credentials(
    runtime: &CompiledModelRuntime,
    blueprint: &CompiledSafeModelBlueprint,
) -> Result<ExecutableCredentialMaterial, ModelManagerError> {
    let provider_id = blueprint.selection.model.provider_id();
    let (source_method, values) = match blueprint.credential_binding.source {
        FrozenCredentialSource::AuthoredApiKey => {
            let ProviderDefinition::ModelsDev(provider) = runtime
                .authored
                .get(&provider_id)
                .ok_or(ModelManagerError::RuntimeCompileFailed)?
            else {
                return Err(ModelManagerError::RuntimeCompileFailed);
            };
            let method = runtime
                .catalog
                .provider(&provider_id)
                .and_then(|entry| entry.record.as_ref())
                .and_then(|record| family_registry().classify(record))
                .map(|recipe| recipe.default_auth_method.to_owned())
                .ok_or(ModelManagerError::RuntimeCompileFailed)?;
            (
                method,
                BTreeMap::from([(
                    AuthFieldName::new("api_key")
                        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                    provider
                        .api_key
                        .as_ref()
                        .ok_or(ModelManagerError::RuntimeCompileFailed)?
                        .expose()
                        .to_owned(),
                )]),
            )
        }
        FrozenCredentialSource::AuthoredOverride => {
            let ProviderDefinition::ModelsDev(provider) = runtime
                .authored
                .get(&provider_id)
                .ok_or(ModelManagerError::RuntimeCompileFailed)?
            else {
                return Err(ModelManagerError::RuntimeCompileFailed);
            };
            let auth = provider
                .auth_override
                .as_ref()
                .ok_or(ModelManagerError::RuntimeCompileFailed)?;
            (
                auth.method.as_str().to_owned(),
                auth.values
                    .iter()
                    .map(|(field, value)| (field.clone(), value.expose().to_owned()))
                    .collect(),
            )
        }
        FrozenCredentialSource::ProviderStore => {
            let connection = runtime
                .store
                .provider(&provider_id)
                .ok_or(ModelManagerError::RuntimeCompileFailed)?;
            (
                connection.auth_method.as_str().to_owned(),
                connection
                    .credential_fields()
                    .map(|field| {
                        Ok((
                            field.clone(),
                            connection
                                .credential(field)
                                .ok_or(ModelManagerError::RuntimeCompileFailed)?
                                .to_owned(),
                        ))
                    })
                    .collect::<Result<_, ModelManagerError>>()?,
            )
        }
        FrozenCredentialSource::NoAuth => ("no-auth-v1".to_owned(), BTreeMap::new()),
    };
    mapped_credentials(
        &ExecutableCredentialMaterial {
            method: source_method,
            values,
        },
        &CompiledAuthShape {
            method: blueprint.auth_method.as_str().to_owned(),
            safe_parameters: blueprint
                .credential_binding
                .parameters
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.as_str().to_owned()))
                .collect(),
            credential_fields: blueprint
                .credential_binding
                .fields
                .iter()
                .map(|field| field.as_str().to_owned())
                .collect(),
            owned_headers: blueprint
                .credential_binding
                .owned_headers
                .iter()
                .map(|header| header.as_str().to_owned())
                .collect(),
            source: AuthSourceCategory::AuthoredOverride,
        },
    )
}

#[must_use]
pub fn retained_family_match(
    provider_id: &ProviderId,
    connection: &StoredManagedConnection,
) -> RetainedFamilyMatch {
    if retained_recipe(connection, provider_id).is_some() {
        RetainedFamilyMatch::SupportedRemoved
    } else {
        RetainedFamilyMatch::RemovedWithoutRetainedFamilyMatch
    }
}

pub(crate) fn validated_provider_source_digest(
    record: &crate::catalog::CatalogProviderRecord,
) -> Result<Sha256Digest, ModelManagerError> {
    canonical_state_fingerprint(b"cookie-agent/catalog-provider-source/v1\0", record)
        .map_err(ModelManagerError::from)
}

pub(crate) fn retained_recipe_fingerprint(
    recipe: &FamilyRecipe,
    selected_auth_method: &str,
) -> Result<Sha256Digest, ModelManagerError> {
    let registry = family_registry();
    let auth = auth_method(selected_auth_method).ok_or(ModelManagerError::UnsupportedAuthMethod)?;
    canonical_state_fingerprint(
        b"cookie-agent/retained-provider-recipe/v1\0",
        &(
            registry.revision(),
            COMPILER_VERSION,
            recipe,
            auth,
            "catalog-derived-model-semantics-v1",
        ),
    )
    .map_err(ModelManagerError::from)
}

pub(crate) fn retained_recipe_package(recipe: &FamilyRecipe) -> Option<&'static str> {
    Some(recipe.npm)
}

fn retained_recipe(
    connection: &StoredManagedConnection,
    provider_id: &ProviderId,
) -> Option<&'static FamilyRecipe> {
    retained_recipe_for_endpoint(connection, provider_id, None)
}

fn retained_recipe_for_endpoint(
    connection: &StoredManagedConnection,
    provider_id: &ProviderId,
    current_endpoint: Option<&str>,
) -> Option<&'static FamilyRecipe> {
    if &connection.provider_id != provider_id {
        return None;
    }
    let package = connection.policy.package_claim.as_str();
    let recipe = family_registry().by_npm(package)?;
    let auth = auth_method(connection.auth_method.as_str())?;
    let credential_fields = connection
        .credential_fields()
        .map(AuthFieldName::as_str)
        .collect::<Vec<_>>();
    let expected_fields = auth
        .credentials
        .iter()
        .map(|credential| credential.name)
        .collect::<Vec<_>>();
    let recipe_fingerprint =
        retained_recipe_fingerprint(recipe, connection.auth_method.as_str()).ok()?;
    // Endpoint identity is only enforced when the caller can resolve the
    // current catalog endpoint. Callers without catalog access (snapshot
    // rehydration, removed-provider reconnect) cannot perform this check:
    // comparing against the family default endpoint would falsely reject
    // nested providers whose catalog endpoint differs from the default.
    if current_endpoint
        .is_some_and(|endpoint| connection.policy.default_endpoint_identity.as_str() != endpoint)
    {
        return None;
    }
    (connection.policy.family_id.as_str() == recipe.family.id()
        && connection.policy.adapter_id.as_str() == recipe.family.id()
        && connection.policy.setup_recipe.as_str() == "family-derived-setup-v1"
        && connection.policy.compiler_version.as_str() == COMPILER_VERSION
        && recipe
            .allowed_auth_methods
            .contains(&connection.auth_method.as_str())
        && connection.policy.package_claim.as_str() == package
        && connection.policy.recipe_fingerprint == recipe_fingerprint
        && connection.setup_fingerprint == setup_fingerprint(&connection.setup_values)
        && credential_fields == expected_fields)
        .then_some(recipe)
}

fn with_effective_status(
    mut model: CompiledDynamicModel,
    source: EffectiveCredentialSource,
) -> CompiledDynamicModel {
    if model.setup.is_none() {
        model.status = CompiledModelStatus::SetupUnavailable;
    } else if source == EffectiveCredentialSource::Unavailable
        || model.auth.source == AuthSourceCategory::Unavailable
    {
        model.status = CompiledModelStatus::CredentialsUnavailable;
    } else {
        model.status = CompiledModelStatus::Available;
    }
    model
}

fn matched_family(
    registry: FamilyRecipeRegistry,
    record: &crate::catalog::CatalogProviderRecord,
) -> Result<&'static FamilyRecipe, ModelManagerError> {
    registry
        .classify(record)
        .ok_or(ModelManagerError::UnsupportedProvider)
}

fn normalize_connect(
    current: &CompiledModelRuntime,
    store: &ProviderStoreSnapshot,
    request: ProviderConnectRequest,
) -> Result<ConnectMutation, ModelManagerError> {
    if request.provider_id.as_str().starts_with("custom.") {
        return Err(ModelManagerError::CustomProviderNotStoreBacked);
    }
    let (recipe, package_claim, source_record_digest, catalog_revision) = if let Some(record) =
        current
            .catalog
            .provider(&request.provider_id)
            .and_then(|entry| entry.record.as_ref())
    {
        let recipe = matched_family(family_registry(), record)?;
        (
            recipe,
            retained_recipe_package(recipe)
                .ok_or(ModelManagerError::RuntimeCompileFailed)?
                .to_owned(),
            validated_provider_source_digest(record)?,
            current.catalog.revision.clone(),
        )
    } else if let Some(connection) = store.provider(&request.provider_id) {
        let recipe = retained_recipe(connection, &request.provider_id)
            .ok_or(ModelManagerError::RemovedWithoutRetainedRecipeMatch)?;
        (
            recipe,
            retained_recipe_package(recipe)
                .ok_or(ModelManagerError::RemovedWithoutRetainedRecipeMatch)?
                .to_owned(),
            connection.policy.source_record_digest.clone(),
            current.catalog.revision.clone(),
        )
    } else {
        return Err(ModelManagerError::UnknownProvider);
    };
    let setup_values = request.setup_values.clone();
    let method = auth_method(request.auth_method.as_str())
        .filter(|method| recipe.allowed_auth_methods.contains(&method.id))
        .ok_or(ModelManagerError::UnsupportedAuthMethod)?;
    let actual = request
        .auth_values
        .field_names()
        .map(AuthFieldName::as_str)
        .collect::<BTreeSet<_>>();
    let required = method
        .credentials
        .iter()
        .filter(|field| field.required)
        .map(|field| field.name)
        .collect::<BTreeSet<_>>();
    let allowed = method
        .credentials
        .iter()
        .map(|field| field.name)
        .collect::<BTreeSet<_>>();
    if !required.is_subset(&actual) || !actual.is_subset(&allowed) {
        return Err(ModelManagerError::InvalidCredentials);
    }
    let endpoint = if let Some(record) = current
        .catalog
        .provider(&request.provider_id)
        .and_then(|entry| entry.record.as_ref())
    {
        resolved_provider_endpoint(record, recipe, &setup_values)?
    } else {
        store
            .provider(&request.provider_id)
            .map(|connection| {
                connection
                    .policy
                    .default_endpoint_identity
                    .as_str()
                    .to_owned()
            })
            .ok_or(ModelManagerError::InvalidSetup)?
    };
    let recipe_fingerprint = retained_recipe_fingerprint(recipe, request.auth_method.as_str())?;
    Ok(ConnectMutation {
        client_connect_id: request.client_connect_id,
        provider_id: request.provider_id,
        expected_catalog_revision: request.expected_catalog_revision,
        expectation: store.expectation(),
        setup_values,
        auth_method: request.auth_method,
        auth_values: request.auth_values,
        policy: StoredProviderPolicyProjection {
            catalog_revision,
            family_id: SafePolicyString::new(recipe.family.id())
                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            setup_recipe: ProviderSetupRecipeId::new("family-derived-setup-v1")
                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            adapter_id: SafePolicyString::new(recipe.family.id())
                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            compiler_version: RecipeCompilerVersion::new(COMPILER_VERSION)
                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            default_endpoint_identity: SafePolicyString::new(endpoint)
                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            package_claim: SafePolicyString::new(package_claim)
                .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            source_record_digest,
            recipe_fingerprint,
            model_overrides: BTreeMap::new(),
        },
    })
}

fn normalized_setup_values(
    recipe: &SetupRecipe,
    values: &BTreeMap<SetupFieldId, SafeSetupValue>,
) -> Result<BTreeMap<SetupFieldId, SafeSetupValue>, ModelManagerError> {
    let validated = validate_setup(recipe, values).map_err(|_| ModelManagerError::InvalidSetup)?;
    validated
        .values
        .into_iter()
        .map(|(id, value)| {
            Ok((
                SetupFieldId::new(id).map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                SafeSetupValue::String(
                    BoundedSetupString::new(value)
                        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
                ),
            ))
        })
        .collect()
}

fn resolved_provider_endpoint(
    record: &crate::catalog::CatalogProviderRecord,
    recipe: &FamilyRecipe,
    setup: &BTreeMap<SetupFieldId, SafeSetupValue>,
) -> Result<String, ModelManagerError> {
    let values = setup
        .iter()
        .map(|(id, value)| {
            let value = match value {
                SafeSetupValue::String(value) => value.as_str(),
                SafeSetupValue::Code(value) => value.as_str(),
                SafeSetupValue::Integer(_) | SafeSetupValue::Bool(_) => {
                    return Err(ModelManagerError::InvalidSetup);
                }
            };
            Ok((id.as_str().to_owned(), value.to_owned()))
        })
        .collect::<Result<BTreeMap<_, _>, ModelManagerError>>()?;
    let template = record.api.as_deref().or(recipe.default_endpoint);
    if let Some(template) = template {
        return crate::recipes::substitute_placeholders(template, &values)
            .map(|value| value.trim_end_matches('/').to_owned())
            .ok_or(ModelManagerError::InvalidSetup);
    }
    match recipe.family {
        crate::recipes::FamilyKind::Vertex | crate::recipes::FamilyKind::VertexAnthropic => {
            let project = values
                .get("project")
                .ok_or(ModelManagerError::InvalidSetup)?;
            let location = values
                .get("location")
                .ok_or(ModelManagerError::InvalidSetup)?;
            Ok(format!(
                "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}"
            ))
        }
        _ => Err(ModelManagerError::InvalidSetup),
    }
}

pub(crate) fn setup_fingerprint(values: &BTreeMap<SetupFieldId, SafeSetupValue>) -> Sha256Digest {
    crate::provider_store::setup_fingerprint(values)
        .expect("normalized setup values have a canonical provider-store fingerprint")
}

/// Secret-free exact authored provider fingerprint used by manifests and rehydration.
#[must_use]
pub fn safe_definition_fingerprint(
    provider_id: &ProviderId,
    definition: &ProviderDefinition,
) -> Sha256Digest {
    let value = match definition {
        ProviderDefinition::ModelsDev(provider) => json!({
            "provider_id": provider_id,
            "source": "models_dev",
            "base_url": provider.base_url.as_ref().map(crate::EndpointUrl::as_str),
            "setup": provider.setup,
            "auth": if provider.api_key.is_some() {
                json!({
                    "source":"api_key",
                    "method": "catalog_family_default",
                    "fields":["api_key"]
                })
            } else if let Some(auth) = &provider.auth_override {
                json!({"source":"auth_override","method":auth.method,"fields":auth.values.keys().collect::<Vec<_>>()})
            } else {
                Value::Null
            },
            "model_overrides": provider.model_overrides.iter().map(|(id, value)| (id.as_str(), json!({
                "enabled": value.enabled,
                "display_name": value.display_name,
                "defaults": value.defaults,
                "variants": value.variants,
                "default_variant": value.default_variant,
                "shape": value.shape,
            }))).collect::<BTreeMap<_, _>>(),
            "shape": provider.shape,
        }),
        ProviderDefinition::Custom(provider) => json!({
            "provider_id": provider_id,
            "source": "custom",
            "endpoint": provider.endpoint.as_str(),
            "adaptor": provider.adaptor,
            "setup": provider.setup,
            "auth": {
                "method": provider.auth.method,
                "parameters": provider.auth.parameters,
                "fields": provider.auth.values.keys().collect::<Vec<_>>(),
            },
            "headers": provider.headers,
            "models": provider.models.iter().map(|(id, value)| (id.as_str(), json!({
                "enabled": value.enabled,
                "display_name": value.display_name,
                "capabilities": value.capabilities,
                "defaults": value.defaults,
                "options": value.options,
                "variants": value.variants,
                "default_variant": value.default_variant,
            }))).collect::<BTreeMap<_, _>>(),
        }),
    };
    safe_hash("cookie-agent/authored-provider-definition/v1", &value)
}

fn no_authored_override_fingerprint() -> Sha256Digest {
    safe_hash(
        "cookie-agent/authored-provider-definition/v1",
        &"no-authored-override",
    )
}

fn safe_hash(domain: &str, value: &impl Serialize) -> Sha256Digest {
    Sha256Digest::hash(domain, value).expect("safe model state serializes")
}

fn revision<T, E>(
    domain: &str,
    value: &impl Serialize,
    constructor: impl FnOnce(String) -> Result<T, E>,
) -> Result<T, ModelManagerError> {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(value).map_err(|_| ModelManagerError::RuntimeCompileFailed)?);
    constructor(format!("sha256:{:x}", hasher.finalize()))
        .map_err(|_| ModelManagerError::RuntimeCompileFailed)
}

fn protocol_digest(value: &Sha256Digest) -> protocol::Sha256Digest {
    protocol::Sha256Digest::new(value.as_str()).expect("model digest is protocol-safe")
}

fn protocol_setup_values(
    values: &BTreeMap<SetupFieldId, SafeSetupValue>,
) -> Result<BTreeMap<SetupFieldId, protocol::SafeSetupValue>, ModelManagerError> {
    values
        .iter()
        .map(|(id, value)| {
            let value = serde_json::from_value(
                serde_json::to_value(value).map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            )
            .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
            Ok((id.clone(), value))
        })
        .collect()
}

fn protocol_options(
    model: &CompiledDynamicModel,
    setup: &BTreeMap<SetupFieldId, SafeSetupValue>,
) -> Result<protocol::ProviderOptions, ModelManagerError> {
    let string = |name: &str| {
        setup
            .iter()
            .find(|(id, _)| id.as_str() == name)
            .and_then(|(_, value)| match value {
                SafeSetupValue::String(value) => Some(value.as_str().to_owned()),
                SafeSetupValue::Code(value) => Some(value.as_str().to_owned()),
                SafeSetupValue::Integer(_) | SafeSetupValue::Bool(_) => None,
            })
    };
    Ok(match model.adapter {
        OvenAdapterFamily::Anthropic | OvenAdapterFamily::AnthropicCompatible => {
            protocol::ProviderOptions::Anthropic {
                api_version: model.options.api_version.clone(),
                beta: model.options.beta.clone(),
            }
        }
        OvenAdapterFamily::OpenaiChat => protocol::ProviderOptions::OpenAiChat {
            organization: model.options.organization.clone(),
            project: model.options.project.clone(),
        },
        OvenAdapterFamily::OpenaiResponses => protocol::ProviderOptions::OpenAiResponses {
            organization: model.options.organization.clone(),
            project: model.options.project.clone(),
            store: model.options.store,
        },
        OvenAdapterFamily::OpenaiCompatible => protocol::ProviderOptions::OpenAiCompatible {
            api_path: model.options.api_path.clone(),
        },
        OvenAdapterFamily::GoogleGemini => protocol::ProviderOptions::GoogleGemini {
            api_version: model.options.api_version.clone(),
        },
        OvenAdapterFamily::GoogleVertexGemini => protocol::ProviderOptions::GoogleVertexGemini {
            project: string("project").ok_or(ModelManagerError::RuntimeCompileFailed)?,
            location: string("location").ok_or(ModelManagerError::RuntimeCompileFailed)?,
        },
        OvenAdapterFamily::AwsBedrockConverse => protocol::ProviderOptions::AwsBedrockConverse {
            region: string("region").ok_or(ModelManagerError::RuntimeCompileFailed)?,
        },
        OvenAdapterFamily::AzureOpenaiChat => protocol::ProviderOptions::AzureOpenAiChat {
            deployment: string("deployment").unwrap_or_else(|| model.id.as_str().to_owned()),
            api_version: string("api_version").unwrap_or_else(|| "v1".to_owned()),
        },
        OvenAdapterFamily::AzureOpenaiResponses => {
            protocol::ProviderOptions::AzureOpenAiResponses {
                deployment: string("deployment").unwrap_or_else(|| model.id.as_str().to_owned()),
                api_version: string("api_version").unwrap_or_else(|| "v1".to_owned()),
            }
        }
        OvenAdapterFamily::CohereV2Chat => protocol::ProviderOptions::CohereV2Chat {
            api_version: model.options.api_version.clone(),
        },
    })
}

fn frozen_defaults(
    value: &crate::ResolvedRequestDefaults,
) -> Result<FrozenResolvedRequestDefaults, ModelManagerError> {
    let temperature = value
        .request
        .temperature
        .map(|value| NormalizedDecimal::from_f32(value.get()))
        .transpose()
        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
    let top_p = value
        .request
        .top_p
        .map(|value| NormalizedDecimal::from_f32(value.get()))
        .transpose()
        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
    let tool_choice = value
        .request
        .tool_choice
        .as_ref()
        .map(|choice| {
            serde_json::from_value(
                serde_json::to_value(choice)
                    .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            )
            .map_err(|_| ModelManagerError::RuntimeCompileFailed)
        })
        .transpose()?;
    let reasoning = value
        .reasoning
        .as_ref()
        .map(|reasoning| {
            serde_json::from_value(
                serde_json::to_value(reasoning)
                    .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
            )
            .map_err(|_| ModelManagerError::RuntimeCompileFailed)
        })
        .transpose()?;
    let frozen = FrozenResolvedRequestDefaults {
        request: protocol::FrozenRequestDefaults {
            temperature,
            top_p,
            max_output_tokens: value.request.max_output_tokens,
            stop: value.request.stop.clone(),
            seed: value.request.seed,
            tool_choice,
        },
        reasoning,
    };
    frozen
        .validate()
        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
    Ok(frozen)
}

fn thaw_defaults(
    value: &FrozenResolvedRequestDefaults,
) -> Result<crate::ResolvedRequestDefaults, ModelManagerError> {
    serde_json::from_value(json!({
        "request": {
            "temperature": value.request.temperature.as_ref().map(NormalizedDecimal::get),
            "top_p": value.request.top_p.as_ref().map(NormalizedDecimal::get),
            "max_output_tokens": value.request.max_output_tokens,
            "stop": value.request.stop,
            "seed": value.request.seed,
            "tool_choice": value.request.tool_choice,
        },
        "reasoning": value.reasoning,
    }))
    .map_err(|_| ModelManagerError::RuntimeCompileFailed)
}

fn safe_descriptor(
    provider_id: &ProviderId,
    model: &CompiledDynamicModel,
) -> Result<LanguageModelDescriptor, ModelManagerError> {
    LanguageModelDescriptor::new(
        ModelIdentity::new(
            OvenProviderId::new(provider_id.as_str()),
            ModelId::new(model.id.as_str()),
        )
        .map_err(|_| ModelManagerError::RuntimeCompileFailed)?,
        AdapterId::new(model.adapter_id.clone()),
        oven_capabilities(&model.capabilities, model.adapter)?,
    )
    .map_err(|_| ModelManagerError::RuntimeCompileFailed)
}

fn oven_capabilities(
    value: &crate::ModelCapabilities,
    adapter: OvenAdapterFamily,
) -> Result<OvenCapabilities, ModelManagerError> {
    let mut features = Capability::MAX_OUTPUT_TOKENS;
    if value.tool_calling {
        features |= Capability::TOOL_CALLING;
    }
    if value.parallel_tool_calls {
        features |= Capability::PARALLEL_TOOLS;
    }
    if value.structured_output {
        features |= Capability::STRUCTURED_OUTPUT;
    }
    if value.reasoning {
        features |= Capability::REASONING;
    }
    if value.temperature {
        features |= Capability::TEMPERATURE;
    }
    if value.top_p {
        features |= Capability::TOP_P;
    }
    if matches!(
        adapter,
        OvenAdapterFamily::Anthropic
            | OvenAdapterFamily::AnthropicCompatible
            | OvenAdapterFamily::GoogleGemini
            | OvenAdapterFamily::GoogleVertexGemini
            | OvenAdapterFamily::AwsBedrockConverse
    ) {
        features |= Capability::PROMPT_CACHING;
    }
    let modality = |value: &crate::Modality| match value {
        crate::Modality::Text => OvenModality::text(),
        crate::Modality::Image => OvenModality::image(),
        crate::Modality::Audio => OvenModality::audio(),
        crate::Modality::Pdf => OvenModality::pdf(),
        crate::Modality::Video => OvenModality::video(),
    };
    let media = value
        .media
        .iter()
        .map(|(kind, support)| {
            let modality = modality(&match kind {
                crate::MediaKind::Image => crate::Modality::Image,
                crate::MediaKind::Audio => crate::Modality::Audio,
                crate::MediaKind::Pdf => crate::Modality::Pdf,
                crate::MediaKind::Video => crate::Modality::Video,
            });
            let support = MediaInputSupport::new(
                support
                    .mime_types
                    .iter()
                    .map(|mime| mime.as_str().to_owned()),
                MediaSourceSupport::INLINE_BYTES,
            )
            .map_err(|_| ModelManagerError::RuntimeCompileFailed)?;
            Ok((modality, support))
        })
        .collect::<Result<BTreeMap<_, _>, ModelManagerError>>()?;
    Ok(OvenCapabilities {
        features,
        limits: ModelLimits::new(Some(value.context_tokens), None, Some(value.output_tokens)),
        modalities: Modalities::new(
            value.input.iter().map(modality),
            value.output.iter().map(modality),
        ),
        media: MediaCapabilities { input: media },
        cancellation: match value.cancellation {
            crate::CancellationCapability::LocalOnly => OvenCancellation::LocalOnly,
            crate::CancellationCapability::Provider => OvenCancellation::RemoteBestEffort,
        },
        compaction: match value.compaction {
            crate::CompactionCapability::Unsupported => OvenCompaction::Unsupported,
            crate::CompactionCapability::Native => OvenCompaction::Native,
        },
        replay: ReplayDeclaration {
            policy: if value.native_replay == crate::ReplayCapability::Unsupported {
                ReplayPolicy::Never
            } else {
                ReplayPolicy::IfValid
            },
            capability: match value.native_replay {
                crate::ReplayCapability::Unsupported => OvenReplay::Unsupported,
                crate::ReplayCapability::Optional => OvenReplay::Optional,
                crate::ReplayCapability::Required => OvenReplay::Required,
            },
            reasoning: value.reasoning
                && value.native_replay != crate::ReplayCapability::Unsupported,
        },
    })
}

fn prepared_retained(
    current: &Arc<BTreeMap<ModelRevision, Vec<Arc<CompiledModelRuntime>>>>,
    candidate: &Arc<CompiledModelRuntime>,
) -> Arc<BTreeMap<ModelRevision, Vec<Arc<CompiledModelRuntime>>>> {
    let mut retained = current.as_ref().clone();
    retained
        .entry(candidate.model_revision.clone())
        .or_default()
        .push(Arc::clone(candidate));
    Arc::new(retained)
}

#[cfg(test)]
mod cache_strategy_tests {
    use super::*;
    use crate::adapters::oven::{AdapterConfig, AuthConfig, CommonDefaults, ConcreteModel};
    use crate::adapters::{
        BedrockCachePoint, BedrockCacheStrategy, BedrockCacheTtl, BedrockMessageCachePoint,
        GoogleCacheStrategyConfig, OpenAiCacheStrategyConfig,
    };
    use crate::{ScriptedModel, ScriptedStep};
    use oven_sdk::{
        AbortSignal, InputPart, JsonSchema, StreamPart, SystemMessage, SystemPart, TextPart,
        ToolDefinition, ToolMessage, UserMessage,
    };

    #[test]
    fn video_media_capability_maps_to_open_oven_modality() {
        let capabilities = crate::ModelCapabilities {
            input: BTreeSet::from([crate::Modality::Text, crate::Modality::Video]),
            output: BTreeSet::from([crate::Modality::Text]),
            context_tokens: 128_000,
            output_tokens: 8_192,
            tool_calling: false,
            parallel_tool_calls: false,
            structured_output: false,
            reasoning: false,
            temperature: true,
            top_p: false,
            seed: false,
            compaction: crate::CompactionCapability::Unsupported,
            native_replay: crate::ReplayCapability::Unsupported,
            cancellation: crate::CancellationCapability::LocalOnly,
            media: BTreeMap::from([(
                crate::MediaKind::Video,
                crate::MediaCapability {
                    mime_types: BTreeSet::from([crate::MimeType::new("video/mp4").unwrap()]),
                    max_bytes: 25 * 1024 * 1024,
                    max_count: 2,
                },
            )]),
        };
        let oven = oven_capabilities(&capabilities, OvenAdapterFamily::OpenaiResponses).unwrap();

        assert!(oven.modalities.input.contains(&OvenModality::video()));
        assert!(oven.media.input.contains_key(&OvenModality::video()));
    }

    fn resolved(prompt_caching: bool, steps: usize) -> (ResolvedExecutableModel, ScriptedModel) {
        let capabilities: OvenCapabilities = serde_json::from_value(json!({
            "features": if prompt_caching { vec!["prompt_caching"] } else { Vec::<&str>::new() },
            "limits": {"context": 4096, "input": null, "output": 1024},
            "modalities": {"input": ["text"], "output": ["text"]},
            "media": {"input": {}},
            "cancellation": "local_only",
            "compaction": "unsupported",
            "replay": {"policy": "never", "capability": "unsupported", "reasoning": false}
        }))
        .unwrap();
        let descriptor = LanguageModelDescriptor::new(
            ModelIdentity::new(OvenProviderId::new("test"), ModelId::new("group/model")).unwrap(),
            AdapterId::new("test.scripted"),
            capabilities,
        )
        .unwrap();
        let scripted = ScriptedModel::new(
            descriptor,
            (0..steps).map(|_| {
                ScriptedStep::stream([Ok(StreamPart::StreamStart {
                    warnings: Vec::new(),
                })])
            }),
        );
        let resolved = ResolvedExecutableModel {
            selection: ModelSelection {
                model: "test/group/model".parse().unwrap(),
                variant: None,
            },
            model: Arc::new(scripted.clone()),
            adapter: OvenAdapterFamily::Anthropic,
            defaults: crate::ResolvedRequestDefaults::default(),
            provider_options: BTreeMap::new(),
            behavior_fingerprint: Sha256Digest::new("0".repeat(64)).unwrap(),
        };
        (resolved, scripted)
    }

    fn strategy() -> CacheStrategyConfig {
        CacheStrategyConfig::Anthropic(AnthropicCacheStrategyConfig {
            system: Some(AnthropicCacheTtlConfig::OneHour),
            tools: Some(AnthropicCacheTtlConfig::OneHour),
            rolling: Some(AnthropicCacheTtlConfig::FiveMinutes),
        })
    }

    fn marker(options: &oven_sdk::ProviderOptions) -> Option<&str> {
        options
            .get("anthropic")?
            .get("cache_control")?
            .get("ttl")?
            .as_str()
    }

    fn request() -> Request {
        Request::new(vec![
            oven_sdk::HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(
                TextPart::new("stable system"),
            )])),
            oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
                "eligible user",
            ))])),
            oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(""))])),
            oven_sdk::HistoryTurn::tool(ToolMessage::new(Vec::new())),
        ])
        .with_tools(vec![
            ToolDefinition::new(
                "first",
                "first tool",
                JsonSchema::new(json!({"type":"object"})).unwrap(),
            ),
            ToolDefinition::new(
                "last",
                "last tool",
                JsonSchema::new(json!({"type":"object"})).unwrap(),
            ),
        ])
    }

    #[tokio::test]
    async fn strategy_places_three_ordered_markers_with_empty_and_tool_fallback() {
        let (resolved, scripted) = resolved(true, 1);
        let strategy = strategy();
        let prepared = resolved.prepare_request_with_cache_strategy(request(), Some(&strategy));
        let _ = scripted
            .stream(prepared, AbortSignal::default())
            .await
            .unwrap();
        let captured = scripted.requests().pop().unwrap();

        let oven_sdk::HistoryTurn::System(system) = &captured.history[0] else {
            panic!("system turn");
        };
        let oven_sdk::HistoryTurn::User(rolling) = &captured.history[1] else {
            panic!("rolling user turn");
        };
        assert_eq!(marker(&system.provider_options), Some("one_hour"));
        assert_eq!(marker(&rolling.provider_options), Some("five_minutes"));
        assert_eq!(marker(&captured.tools[0].provider_options), None);
        assert_eq!(
            marker(&captured.tools[1].provider_options),
            Some("one_hour")
        );
        assert_eq!(
            captured
                .history
                .iter()
                .filter(|turn| marker(match turn {
                    oven_sdk::HistoryTurn::System(message) => &message.provider_options,
                    oven_sdk::HistoryTurn::User(message) => &message.provider_options,
                    oven_sdk::HistoryTurn::Assistant(turn) => &turn.message.provider_options,
                    oven_sdk::HistoryTurn::Tool(message) => &message.provider_options,
                })
                .is_some())
                .count()
                + captured
                    .tools
                    .iter()
                    .filter(|tool| marker(&tool.provider_options).is_some())
                    .count(),
            3
        );
    }

    #[test]
    fn capability_gate_and_compaction_reanchor_are_stable() {
        let (without_capability, _) = resolved(false, 0);
        let strategy = strategy();
        let gated =
            without_capability.prepare_request_with_cache_strategy(request(), Some(&strategy));
        assert!(gated.history.iter().all(|turn| {
            marker(match turn {
                oven_sdk::HistoryTurn::System(message) => &message.provider_options,
                oven_sdk::HistoryTurn::User(message) => &message.provider_options,
                oven_sdk::HistoryTurn::Assistant(turn) => &turn.message.provider_options,
                oven_sdk::HistoryTurn::Tool(message) => &message.provider_options,
            })
            .is_none()
        }));

        let (resolved, _) = resolved(true, 0);
        let mut compacted = request();
        compacted
            .history
            .push(oven_sdk::HistoryTurn::system(SystemMessage::new(vec![
                SystemPart::Text(TextPart::new("compacted summary")),
            ])));
        let prepared = resolved.prepare_request_with_cache_strategy(compacted, Some(&strategy));
        let oven_sdk::HistoryTurn::System(first) = &prepared.history[0] else {
            panic!("first system turn");
        };
        let oven_sdk::HistoryTurn::System(last) = prepared.history.last().unwrap() else {
            panic!("summary system turn");
        };
        assert_eq!(marker(&first.provider_options), Some("one_hour"));
        assert_eq!(marker(&last.provider_options), Some("five_minutes"));
    }

    #[test]
    fn bedrock_strategy_expands_last_message_placement() {
        let mut request = Request::new(vec![
            oven_sdk::HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(
                TextPart::new("stable system"),
            )])),
            oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
                "current input",
            ))])),
        ]);
        let strategy = CacheStrategyConfig::Bedrock(BedrockCacheStrategy {
            system: Some(BedrockCachePoint {
                ttl: Some(BedrockCacheTtl::OneHour),
            }),
            tools: Some(BedrockCachePoint {
                ttl: Some(BedrockCacheTtl::OneHour),
            }),
            messages: vec![BedrockMessageCachePoint {
                history_index: usize::MAX,
                cache_point: BedrockCachePoint {
                    ttl: Some(BedrockCacheTtl::FiveMinutes),
                },
            }],
        });

        apply_cache_strategy(
            &mut request,
            OvenAdapterFamily::AwsBedrockConverse,
            &strategy,
        );

        assert_eq!(
            request.provider_options["bedrock"]["cache"],
            json!({
                "system": {"ttl": "1h"},
                "tools": null,
                "messages": [{
                    "historyIndex": 1,
                    "ttl": "5m"
                }]
            })
        );
    }

    #[test]
    fn google_cache_modes_set_clear_or_preserve_cached_content() {
        let explicit = CacheStrategyConfig::Google(GoogleCacheStrategyConfig {
            mode: GoogleCacheMode::Explicit,
            cached_content: Some("cachedContents/example".into()),
        });
        let mut request = Request::new(Vec::new());
        apply_cache_strategy(&mut request, OvenAdapterFamily::GoogleGemini, &explicit);
        assert_eq!(
            request.provider_options["google"]["cached_content"],
            "cachedContents/example"
        );

        let off = CacheStrategyConfig::Google(GoogleCacheStrategyConfig {
            mode: GoogleCacheMode::Off,
            cached_content: None,
        });
        apply_cache_strategy(&mut request, OvenAdapterFamily::GoogleGemini, &off);
        assert!(
            request.provider_options["google"]
                .get("cached_content")
                .is_none()
        );

        request
            .provider_options
            .insert("google_vertex".into(), json!({"topK": 3}));
        let implicit = CacheStrategyConfig::Google(GoogleCacheStrategyConfig {
            mode: GoogleCacheMode::Implicit,
            cached_content: None,
        });
        apply_cache_strategy(
            &mut request,
            OvenAdapterFamily::GoogleVertexGemini,
            &implicit,
        );
        assert_eq!(request.provider_options["google_vertex"]["topK"], 3);
    }

    #[test]
    fn openai_cache_strategy_uses_endpoint_specific_namespace() {
        let strategy = CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
            prompt_cache_key: Some("session-key".into()),
            prompt_cache_retention: Some(OpenAiPromptCacheRetention::TwentyFourHours),
            mode: Some(OpenAiCacheMode::Explicit),
            ttl: Some(OpenAiPromptCacheTtl::ThirtyMinutes),
            system: true,
            rolling: true,
        });
        for (adapter, namespace, section) in [
            (OvenAdapterFamily::OpenaiChat, "openai", "chat"),
            (OvenAdapterFamily::OpenaiResponses, "openai", "responses"),
            (OvenAdapterFamily::AzureOpenaiChat, "azure_openai", "chat"),
            (
                OvenAdapterFamily::AzureOpenaiResponses,
                "azure_openai",
                "responses",
            ),
        ] {
            let mut request = Request::new(vec![
                oven_sdk::HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(
                    TextPart::new("stable system"),
                )])),
                oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(
                    TextPart::new("eligible latest user"),
                )])),
            ]);
            apply_cache_strategy(&mut request, adapter, &strategy);
            assert_eq!(
                request.provider_options[namespace][section]["prompt_cache_key"],
                "session-key"
            );
            assert_eq!(
                request.provider_options[namespace][section]["prompt_cache_retention"],
                "24h"
            );
            assert_eq!(
                request.provider_options[namespace][section]["prompt_cache_options"],
                json!({"mode":"explicit", "ttl":"30m"})
            );
            let marker = if namespace == "openai" {
                "openai.prompt_cache_breakpoint"
            } else {
                "azure_openai.prompt_cache_breakpoint"
            };
            let oven_sdk::HistoryTurn::System(system) = &request.history[0] else {
                panic!("system turn");
            };
            let oven_sdk::SystemPart::Text(system) = &system.content[0] else {
                panic!("system text");
            };
            let oven_sdk::HistoryTurn::User(rolling) = &request.history[1] else {
                panic!("rolling user turn");
            };
            let oven_sdk::InputPart::Text(rolling) = &rolling.content[0] else {
                panic!("rolling user text");
            };
            assert_eq!(
                system
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get(marker))
                    .and_then(Value::as_bool),
                Some(true)
            );
            assert_eq!(
                rolling
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get(marker))
                    .and_then(Value::as_bool),
                Some(true)
            );
        }
    }

    fn openai_strategy(system: bool, rolling: bool) -> CacheStrategyConfig {
        CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
            prompt_cache_key: None,
            prompt_cache_retention: None,
            mode: Some(OpenAiCacheMode::Explicit),
            ttl: Some(OpenAiPromptCacheTtl::ThirtyMinutes),
            system,
            rolling,
        })
    }

    fn has_openai_breakpoint(metadata: &oven_sdk::PartMetadata) -> bool {
        metadata
            .as_ref()
            .and_then(|metadata| metadata.get("openai.prompt_cache_breakpoint"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    #[test]
    fn openai_rolling_does_not_fall_back_from_latest_file_only_user_turn() {
        let mut request = Request::new(vec![
            oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
                "earlier eligible text",
            ))])),
            oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::File(
                oven_sdk::FilePart::document(
                    "application/pdf",
                    oven_sdk::FileSource::Text("latest file".into()),
                ),
            )])),
        ]);

        apply_cache_strategy(
            &mut request,
            OvenAdapterFamily::OpenaiChat,
            &openai_strategy(false, true),
        );

        let oven_sdk::HistoryTurn::User(earlier) = &request.history[0] else {
            panic!("earlier user turn");
        };
        let InputPart::Text(earlier) = &earlier.content[0] else {
            panic!("earlier user text");
        };
        let oven_sdk::HistoryTurn::User(latest) = &request.history[1] else {
            panic!("latest user turn");
        };
        let InputPart::File(latest) = &latest.content[0] else {
            panic!("latest user file");
        };
        assert!(!has_openai_breakpoint(&earlier.metadata));
        assert!(!has_openai_breakpoint(&latest.metadata));
    }

    #[test]
    fn openai_system_does_not_fall_forward_from_ineligible_first_turn() {
        let mut request = Request::new(vec![
            oven_sdk::HistoryTurn::system(SystemMessage::new(vec![
                SystemPart::Text(TextPart::new("")),
                SystemPart::Custom(oven_sdk::CustomPart::new(
                    "test.system",
                    json!({"value":"not text"}),
                )),
            ])),
            oven_sdk::HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(
                TextPart::new("later eligible system text"),
            )])),
        ]);

        apply_cache_strategy(
            &mut request,
            OvenAdapterFamily::OpenaiChat,
            &openai_strategy(true, false),
        );

        let oven_sdk::HistoryTurn::System(first) = &request.history[0] else {
            panic!("first system turn");
        };
        let SystemPart::Text(first) = &first.content[0] else {
            panic!("empty system text");
        };
        let oven_sdk::HistoryTurn::System(later) = &request.history[1] else {
            panic!("later system turn");
        };
        let SystemPart::Text(later) = &later.content[0] else {
            panic!("later system text");
        };
        assert!(!has_openai_breakpoint(&first.metadata));
        assert!(!has_openai_breakpoint(&later.metadata));
    }

    fn real_openai_resolved(
        adapter: OvenAdapterFamily,
        endpoint: String,
    ) -> ResolvedExecutableModel {
        let capabilities = resolved(false, 0).0.model().capabilities().clone();
        if matches!(
            adapter,
            OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses
        ) {
            let provider = oven_sdk::ProviderConfig::new(
                OvenProviderId::new(oven_sdk_azure::AZURE_OPENAI_PROVIDER_ID),
                oven_sdk::ApiEndpoint::parse(endpoint).unwrap(),
                oven_sdk_azure::AzureOpenAiAuth::ApiKey(oven_sdk::SecretString::new("test-key")),
                oven_sdk::HeaderConfig::empty(),
            )
            .unwrap();
            let declaration =
                oven_sdk::ModelDeclaration::new(ModelId::new("gpt-5.6-test"), capabilities)
                    .unwrap();
            let model: Arc<dyn LanguageModel> = match adapter {
                OvenAdapterFamily::AzureOpenaiChat => Arc::new(
                    oven_sdk_azure::AzureOpenAiChatModel::new(oven_sdk::ModelConfig::new(
                        provider,
                        declaration,
                        oven_sdk_azure::AzureOpenAiChatSettings::default(),
                    ))
                    .unwrap(),
                ),
                OvenAdapterFamily::AzureOpenaiResponses => Arc::new(
                    oven_sdk_azure::AzureOpenAiResponsesModel::new(oven_sdk::ModelConfig::new(
                        provider,
                        declaration,
                        oven_sdk_azure::AzureOpenAiResponsesSettings::default(),
                    ))
                    .unwrap(),
                ),
                _ => unreachable!("Azure family checked"),
            };
            return ResolvedExecutableModel {
                selection: ModelSelection {
                    model: "test/group/model".parse().unwrap(),
                    variant: None,
                },
                model,
                adapter,
                defaults: crate::ResolvedRequestDefaults::default(),
                provider_options: BTreeMap::new(),
                behavior_fingerprint: Sha256Digest::new("0".repeat(64)).unwrap(),
            };
        }
        let adapter_config: AdapterConfig = serde_json::from_value(match adapter {
            OvenAdapterFamily::OpenaiChat => json!({
                "adaptor":"openai-chat",
                "settings":{
                    "system_message_role":"developer",
                    "max_tokens_field":"max_tokens",
                    "stream_usage":false,
                    "structured_output":"unsupported",
                    "reasoning_field":"none",
                    "routing_discriminator":null
                },
                "options":{}
            }),
            OvenAdapterFamily::OpenaiResponses => json!({
                "adaptor":"openai-responses",
                "settings":{"routing_discriminator":null,"compaction":"unsupported"},
                "options":{}
            }),
            OvenAdapterFamily::AzureOpenaiChat => json!({
                "adaptor":"azure-chat",
                "settings":{
                    "route":{"kind":"v1"},
                    "revision":null,
                    "system_role":"developer",
                    "max_tokens_field":"max_tokens",
                    "stream_usage":false,
                    "structured_output":"unsupported",
                    "reasoning_field":"none",
                    "omit_reasoning_sampling":false
                },
                "options":{}
            }),
            OvenAdapterFamily::AzureOpenaiResponses => json!({
                "adaptor":"azure-responses",
                "settings":{
                    "route":{"kind":"v1"},
                    "revision":null,
                    "compaction":{"kind":"unsupported"}
                },
                "options":{}
            }),
            _ => panic!("OpenAI endpoint family"),
        })
        .unwrap();
        let constructed = ConcreteModel {
            provider_id: if matches!(
                adapter,
                OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses
            ) {
                "azure.openai".into()
            } else {
                "openai".into()
            },
            model_id: "gpt-5.6-test".into(),
            endpoint,
            auth: if matches!(
                adapter,
                OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses
            ) {
                AuthConfig::ApiKey {
                    value: "test-key".into(),
                }
            } else {
                AuthConfig::Openai {
                    api_key: "test-key".into(),
                    organization: None,
                    project: None,
                }
            },
            headers: BTreeMap::new(),
            capabilities,
            defaults: CommonDefaults::default(),
            adapter: adapter_config,
        }
        .build()
        .unwrap();
        ResolvedExecutableModel {
            selection: ModelSelection {
                model: "test/group/model".parse().unwrap(),
                variant: None,
            },
            model: constructed.model,
            adapter,
            defaults: crate::ResolvedRequestDefaults::default(),
            provider_options: constructed.provider_options,
            behavior_fingerprint: Sha256Digest::new("0".repeat(64)).unwrap(),
        }
    }

    async fn wire_capture_server(
        responses: bool,
    ) -> (String, tokio::task::JoinHandle<serde_json::Value>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let (body_start, content_length) = loop {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0, "request ended before headers");
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|value| value == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                break (header_end + 4, content_length);
            };
            while request.len() < body_start + content_length {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0, "request ended before body");
                request.extend_from_slice(&buffer[..read]);
            }
            let body = serde_json::from_slice(
                &request[body_start..body_start.saturating_add(content_length)],
            )
            .unwrap();
            let stream = if responses {
                concat!(
                    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.6-test\"}}\n\n",
                    "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"ok\"}\n\n",
                    "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}}\n\n",
                    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.6-test\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
                )
            } else {
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{stream}",
                stream.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            body
        });
        (format!("http://{address}/v1"), task)
    }

    fn wire_breakpoint_count(value: &Value) -> usize {
        match value {
            Value::Object(object) => {
                usize::from(object.contains_key("prompt_cache_breakpoint"))
                    + object.values().map(wire_breakpoint_count).sum::<usize>()
            }
            Value::Array(values) => values.iter().map(wire_breakpoint_count).sum(),
            _ => 0,
        }
    }

    #[tokio::test]
    async fn openai_cache_controls_reach_all_four_provider_wires() {
        let strategy = CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
            prompt_cache_key: Some("wire-key".into()),
            prompt_cache_retention: Some(OpenAiPromptCacheRetention::TwentyFourHours),
            mode: Some(OpenAiCacheMode::Explicit),
            ttl: Some(OpenAiPromptCacheTtl::ThirtyMinutes),
            system: true,
            rolling: true,
        });
        for adapter in [
            OvenAdapterFamily::OpenaiChat,
            OvenAdapterFamily::OpenaiResponses,
            OvenAdapterFamily::AzureOpenaiChat,
            OvenAdapterFamily::AzureOpenaiResponses,
        ] {
            let responses = matches!(
                adapter,
                OvenAdapterFamily::OpenaiResponses | OvenAdapterFamily::AzureOpenaiResponses
            );
            let (mut endpoint, captured) = wire_capture_server(responses).await;
            if matches!(
                adapter,
                OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses
            ) {
                endpoint.truncate(endpoint.len() - "/v1".len());
            }
            let model = real_openai_resolved(adapter, endpoint);
            let request = Request::new(vec![
                oven_sdk::HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(
                    TextPart::new("stable system"),
                )])),
                oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(
                    TextPart::new("current input"),
                )])),
            ]);
            let request = model.prepare_request_with_cache_strategy(request, Some(&strategy));
            model
                .model()
                .complete(request, AbortSignal::default())
                .await
                .unwrap();
            let body = captured.await.unwrap();
            assert_eq!(body["prompt_cache_key"], "wire-key", "{adapter:?}");
            assert_eq!(body["prompt_cache_retention"], "24h", "{adapter:?}");
            assert_eq!(
                body["prompt_cache_options"],
                json!({"mode":"explicit", "ttl":"30m"}),
                "{adapter:?}"
            );
            assert_eq!(wire_breakpoint_count(&body), 2, "{adapter:?}");
        }
    }
}

fn mutation_provider(mutation: &ProviderStoreMutation) -> &ProviderId {
    match mutation {
        ProviderStoreMutation::Connect {
            durable_connection, ..
        } => &durable_connection.provider_id,
        ProviderStoreMutation::Disconnect { provider_id, .. } => provider_id,
    }
}

fn effective_auth_for(
    runtime: &CompiledModelRuntime,
    provider_id: &ProviderId,
) -> EffectiveCredentialSource {
    runtime
        .providers
        .iter()
        .find(|provider| &provider.id == provider_id)
        .map_or(EffectiveCredentialSource::Unavailable, |provider| {
            provider.effective_auth
        })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Debug, Error)]
pub enum ModelManagerError {
    #[error("unknown_provider")]
    UnknownProvider,
    #[error("unsupported_provider")]
    UnsupportedProvider,
    #[error("removed_without_retained_recipe_match")]
    RemovedWithoutRetainedRecipeMatch,
    #[error("custom_provider_not_store_backed")]
    CustomProviderNotStoreBacked,
    #[error("invalid_setup")]
    InvalidSetup,
    #[error("unsupported_auth_method")]
    UnsupportedAuthMethod,
    #[error("invalid_credentials")]
    InvalidCredentials,
    #[error("runtime_compile_failed")]
    RuntimeCompileFailed,
    #[error("unknown_model")]
    UnknownModel(ModelKey),
    #[error("unknown_variant")]
    UnknownVariant(ModelSelection),
    #[error("model_unavailable")]
    ModelUnavailable(ModelKey),
    #[error("duplicate model `{0}`")]
    DuplicateModel(ModelKey),
    #[error("dynamic model compilation failed: {0}")]
    DynamicCompile(#[from] DynamicCompileError),
    #[error("runtime_compile_failed")]
    ExecutableBuild(#[from] ModelBuildError),
    #[error("provider_store_reload_failed")]
    ProviderStore(#[from] ProviderStoreError),
    #[error("model manifest construction failed")]
    Manifest(#[from] crate::manifests::ManifestError),
}

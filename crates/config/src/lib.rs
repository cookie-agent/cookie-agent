//! Strict schema-6 runtime configuration and schema-1 Markdown agents.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt, fs, io,
    path::{Component, Path, PathBuf},
};

use cookie_agent_identity::{
    AgentId, ConfiguredVariantRef, ModelKey, ModelSelection, ProviderId, SafeCode, WildcardPattern,
};
use cookie_agent_models::{ModelSet, ProviderDefinition, Sha256Digest};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

const CONFIG_SCHEMA: u32 = 6;
const AGENT_SCHEMA: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_AGENT_BYTES: u64 = 256 * 1024;
const MAX_FRONTMATTER_BYTES: usize = 128 * 1024;
const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_LIST: usize = 256;
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 7419;

/// Exact schema-6 marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigSchemaVersion;

impl Serialize for ConfigSchemaVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(CONFIG_SCHEMA)
    }
}
impl<'de> Deserialize<'de> for ConfigSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        if value == CONFIG_SCHEMA {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("schema_version must be exactly 6"))
        }
    }
}

/// Exact schema-1 agent marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct AgentSchemaVersion;
impl<'de> Deserialize<'de> for AgentSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        if value == AGENT_SCHEMA {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("agent schema must be exactly 1"))
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub schema_version: ConfigSchemaVersion,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub tool_output: ToolOutputConfig,
    #[serde(default)]
    pub approval: ApprovalConfig,
    #[serde(default)]
    pub context_compaction: ContextCompactionConfig,
    #[serde(default)]
    pub session_title: SessionTitleConfig,
    pub providers: BTreeMap<ProviderId, ProviderDefinition>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeLayer {
    schema_version: ConfigSchemaVersion,
    server: Option<ServerConfig>,
    tool_output: Option<ToolOutputConfig>,
    approval: Option<ApprovalConfig>,
    context_compaction: Option<ContextCompactionConfig>,
    session_title: Option<SessionTitleConfig>,
    providers: BTreeMap<ProviderId, ProviderDefinition>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}
fn default_host() -> String {
    DEFAULT_HOST.into()
}
const fn default_port() -> u16 {
    DEFAULT_PORT
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputConfig {
    #[serde(default = "default_output_lines")]
    pub max_lines: usize,
    #[serde(default = "default_output_bytes")]
    pub max_bytes: usize,
}
impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            max_lines: default_output_lines(),
            max_bytes: default_output_bytes(),
        }
    }
}
const fn default_output_lines() -> usize {
    2_000
}
const fn default_output_bytes() -> usize {
    50 * 1024
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalConfig {
    #[serde(default = "default_approval_timeout")]
    pub timeout_ms: u64,
}
impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            timeout_ms: default_approval_timeout(),
        }
    }
}
const fn default_approval_timeout() -> u64 {
    30_000
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCompactionConfig {
    #[serde(default = "default_soft")]
    pub soft_threshold_percent: u8,
    #[serde(default = "default_hard")]
    pub hard_threshold_percent: u8,
    #[serde(default = "default_target")]
    pub target_percent: u8,
    #[serde(default = "default_summary")]
    pub max_summary_bytes: usize,
    #[serde(default = "default_native")]
    pub max_native_context_bytes: usize,
}
impl Default for ContextCompactionConfig {
    fn default() -> Self {
        Self {
            soft_threshold_percent: default_soft(),
            hard_threshold_percent: default_hard(),
            target_percent: default_target(),
            max_summary_bytes: default_summary(),
            max_native_context_bytes: default_native(),
        }
    }
}
const fn default_soft() -> u8 {
    70
}
const fn default_hard() -> u8 {
    85
}
const fn default_target() -> u8 {
    50
}
const fn default_summary() -> usize {
    256 * 1024
}
const fn default_native() -> usize {
    2 * 1024 * 1024
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTitleConfig {
    #[serde(default = "default_title_chars")]
    pub max_chars: usize,
    #[serde(default = "default_title_messages")]
    pub max_input_messages: usize,
    #[serde(default = "yes")]
    pub generate_on_first_turn: bool,
    #[serde(default = "yes")]
    pub fallback_to_input_excerpt: bool,
}
impl Default for SessionTitleConfig {
    fn default() -> Self {
        Self {
            max_chars: default_title_chars(),
            max_input_messages: default_title_messages(),
            generate_on_first_turn: true,
            fallback_to_input_excerpt: true,
        }
    }
}
const fn default_title_chars() -> usize {
    80
}
const fn default_title_messages() -> usize {
    4
}
const fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Primary,
    Subagent,
    All,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolName {
    Read,
    Write,
    Edit,
    Bash,
    Grep,
    Glob,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    Read,
    Write,
    Bash,
    Grep,
    Glob,
    Delegate,
    ExternalDirectory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionEffect {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    pub id: SafeCode,
    pub action: PermissionAction,
    pub resource: WildcardPattern,
    pub effect: PermissionEffect,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDelegationConfig {
    pub agents: Vec<AgentId>,
    pub max_depth: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModelFallback {
    pub model: ModelKey,
    pub variant: Option<ConfiguredVariantRef>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentFrontmatter {
    pub schema: AgentSchemaVersion,
    pub description: String,
    pub mode: AgentMode,
    pub enabled: bool,
    pub model_fallback: Vec<AgentModelFallback>,
    pub tools: Vec<ToolName>,
    pub permissions: Vec<PermissionRule>,
    pub delegation: Option<AgentDelegationConfig>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDocumentSource {
    BuiltIn,
    User,
    Workspace,
}

#[derive(Clone, Debug)]
pub struct AgentDocument {
    pub id: AgentId,
    pub frontmatter: AgentFrontmatter,
    pub body: String,
    pub source: AgentDocumentSource,
    pub document_fingerprint: Sha256Digest,
    pub prompt_fingerprint: Sha256Digest,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDescriptor {
    pub id: AgentId,
    pub description: String,
    pub mode: AgentMode,
    pub enabled: bool,
    pub runnable_as_root: bool,
    pub resolved_fallback: Vec<ModelSelection>,
    pub tools: Vec<ToolName>,
    pub delegation_targets: Vec<AgentId>,
}

#[derive(Clone, Debug)]
pub struct ResolvedAgent {
    pub document: AgentDocument,
    pub resolved_fallback: Vec<ModelSelection>,
    pub runnable_as_root: bool,
    model_snapshot_fingerprint: Sha256Digest,
}

/// Exact executable fallback plan for a public root selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootModelPlan {
    selections: Vec<ModelSelection>,
}

impl RootModelPlan {
    #[must_use]
    pub fn selections(&self) -> &[ModelSelection] {
        &self.selections
    }

    #[must_use]
    pub fn into_selections(self) -> Vec<ModelSelection> {
        self.selections
    }
}

/// Existing chain-only suffix plan used for delegated agents with authored fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedModelPlan {
    selections: Vec<ModelSelection>,
}

impl DelegatedModelPlan {
    #[must_use]
    pub fn selections(&self) -> &[ModelSelection] {
        &self.selections
    }

    #[must_use]
    pub fn into_selections(self) -> Vec<ModelSelection> {
        self.selections
    }
}

impl ResolvedAgent {
    #[must_use]
    pub fn model_snapshot_fingerprint(&self) -> &Sha256Digest {
        &self.model_snapshot_fingerprint
    }

    /// Builds a root plan from one coherent model snapshot.
    pub fn plan_root_selection(
        &self,
        selection: &ModelSelection,
        models: &ModelSet,
    ) -> Result<RootModelPlan, ConfigError> {
        self.validate_model_snapshot(models)?;
        if !self.runnable_as_root {
            return Err(ConfigError::IneligibleRootAgent(self.document.id.clone()));
        }
        validate_selection(&self.document.id, selection, models)?;

        let authored_start = self
            .resolved_fallback
            .iter()
            .position(|entry| entry.model == selection.model);
        let authored = authored_start.map_or(self.resolved_fallback.as_slice(), |index| {
            &self.resolved_fallback[index..]
        });
        let mut selections = authored
            .iter()
            .filter(|entry| model_is_available(models, &entry.model))
            .cloned()
            .collect::<Vec<_>>();

        if authored_start.is_some() {
            selections[0] = selection.clone();
        } else {
            selections.insert(0, selection.clone());
        }
        Ok(RootModelPlan { selections })
    }

    /// Selects the unique authored suffix for delegated planning.
    pub fn plan_delegated_selection(
        &self,
        selection: &ModelSelection,
        models: &ModelSet,
    ) -> Result<DelegatedModelPlan, ConfigError> {
        self.validate_model_snapshot(models)?;
        let index = self
            .resolved_fallback
            .iter()
            .position(|entry| entry.model == selection.model)
            .ok_or_else(|| ConfigError::InvalidRunSelection {
                agent: self.document.id.clone(),
                model: selection.model.clone(),
            })?;
        validate_selection(&self.document.id, selection, models)?;
        let mut suffix = self.resolved_fallback[index..].to_vec();
        suffix[0] = selection.clone();
        Ok(DelegatedModelPlan { selections: suffix })
    }

    fn validate_model_snapshot(&self, models: &ModelSet) -> Result<(), ConfigError> {
        if self.model_snapshot_fingerprint == *models.fingerprint() {
            Ok(())
        } else {
            Err(ConfigError::ModelSnapshotMismatch(self.document.id.clone()))
        }
    }
}

fn validate_selection(
    agent: &AgentId,
    selection: &ModelSelection,
    models: &ModelSet,
) -> Result<(), ConfigError> {
    let model = models
        .get(&selection.model)
        .filter(|model| model.is_available())
        .ok_or_else(|| ConfigError::InvalidRunSelection {
            agent: agent.clone(),
            model: selection.model.clone(),
        })?;
    if let Some(variant) = &selection.variant
        && !model.variants().contains_key(variant)
    {
        return Err(ConfigError::UnknownVariant {
            agent: agent.clone(),
            model: selection.model.clone(),
            variant: variant.to_string(),
        });
    }
    Ok(())
}

fn model_is_available(models: &ModelSet, key: &ModelKey) -> bool {
    models
        .get(key)
        .is_some_and(cookie_agent_models::ModelEntry::is_available)
}

#[derive(Clone, Debug)]
pub struct AgentRegistry {
    agents: BTreeMap<AgentId, ResolvedAgent>,
}

impl AgentRegistry {
    pub fn resolve(
        documents: BTreeMap<AgentId, AgentDocument>,
        models: &ModelSet,
    ) -> Result<Self, ConfigError> {
        for document in documents.values() {
            validate_agent_document(document, &documents)?;
        }
        let mut agents = BTreeMap::new();
        for (id, document) in documents {
            let mut resolved = Vec::with_capacity(document.frontmatter.model_fallback.len());
            let mut seen = BTreeSet::new();
            for fallback in &document.frontmatter.model_fallback {
                if !seen.insert(fallback.model.clone()) {
                    return Err(ConfigError::DuplicateFallbackModel {
                        agent: id.clone(),
                        model: fallback.model.clone(),
                    });
                }
                let entry =
                    models
                        .get(&fallback.model)
                        .ok_or_else(|| ConfigError::UnknownModel {
                            agent: id.clone(),
                            model: fallback.model.clone(),
                        })?;
                let variant = match &fallback.variant {
                    None => entry.default_variant().cloned(),
                    Some(ConfiguredVariantRef::Base) => None,
                    Some(ConfiguredVariantRef::Named(variant)) => {
                        if !entry.variants().contains_key(variant) {
                            return Err(ConfigError::UnknownVariant {
                                agent: id.clone(),
                                model: fallback.model.clone(),
                                variant: variant.to_string(),
                            });
                        }
                        Some(variant.clone())
                    }
                };
                resolved.push(ModelSelection {
                    model: fallback.model.clone(),
                    variant,
                });
            }
            let available = resolved
                .iter()
                .any(|selection| model_is_available(models, &selection.model));
            let runnable_as_root = document.frontmatter.enabled
                && matches!(
                    document.frontmatter.mode,
                    AgentMode::Primary | AgentMode::All
                )
                && !resolved.is_empty()
                && available;
            agents.insert(
                id,
                ResolvedAgent {
                    document,
                    resolved_fallback: resolved,
                    runnable_as_root,
                    model_snapshot_fingerprint: models.fingerprint().clone(),
                },
            );
        }
        Ok(Self { agents })
    }

    #[must_use]
    pub fn get(&self, id: &AgentId) -> Option<&ResolvedAgent> {
        self.agents.get(id)
    }
    pub fn agents(&self) -> impl ExactSizeIterator<Item = (&AgentId, &ResolvedAgent)> {
        self.agents.iter()
    }
    pub fn descriptors(&self) -> Vec<AgentDescriptor> {
        self.agents
            .iter()
            .map(|(id, agent)| AgentDescriptor {
                id: id.clone(),
                description: agent.document.frontmatter.description.clone(),
                mode: agent.document.frontmatter.mode,
                enabled: agent.document.frontmatter.enabled,
                runnable_as_root: agent.runnable_as_root,
                resolved_fallback: agent.resolved_fallback.clone(),
                tools: agent.document.frontmatter.tools.clone(),
                delegation_targets: agent
                    .document
                    .frontmatter
                    .delegation
                    .as_ref()
                    .map_or_else(Vec::new, |delegation| delegation.agents.clone()),
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct LoadedConfiguration {
    pub runtime: RuntimeConfig,
    pub agents: BTreeMap<AgentId, AgentDocument>,
}

impl LoadedConfiguration {
    pub fn resolve_agents(&self, models: &ModelSet) -> Result<AgentRegistry, ConfigError> {
        AgentRegistry::resolve(self.agents.clone(), models)
    }
}

/// Loads user config and exact-cwd `.cookie-agent` with no upward search.
pub fn load(cwd: &Path) -> Result<LoadedConfiguration, ConfigError> {
    let cwd = cwd.canonicalize().map_err(ConfigError::Io)?;
    let user = user_root();
    load_from_roots(user.as_deref(), Some(&cwd.join(".cookie-agent")))
}

/// Loads explicit roots, primarily for composition and adversarial tests.
pub fn load_from_roots(
    user_root: Option<&Path>,
    workspace_root: Option<&Path>,
) -> Result<LoadedConfiguration, ConfigError> {
    let user = user_root
        .map(|path| open_layer_root(path, AgentDocumentSource::User))
        .transpose()?
        .flatten();
    let workspace = workspace_root
        .map(|path| open_layer_root(path, AgentDocumentSource::Workspace))
        .transpose()?
        .flatten();
    let mut runtime = RuntimeConfig {
        schema_version: ConfigSchemaVersion,
        server: ServerConfig::default(),
        tool_output: ToolOutputConfig::default(),
        approval: ApprovalConfig::default(),
        context_compaction: ContextCompactionConfig::default(),
        session_title: SessionTitleConfig::default(),
        providers: BTreeMap::new(),
    };
    let mut agents = BTreeMap::new();
    for root in [user.as_ref(), workspace.as_ref()].into_iter().flatten() {
        if let Some(layer) = root.load_runtime()? {
            apply_layer(&mut runtime, layer);
        }
        for (id, document) in root.load_agents()? {
            agents.insert(id, document);
        }
    }
    validate_runtime(&runtime)?;
    Ok(LoadedConfiguration { runtime, agents })
}

fn apply_layer(runtime: &mut RuntimeConfig, layer: RuntimeLayer) {
    runtime.schema_version = layer.schema_version;
    if let Some(value) = layer.server {
        runtime.server = value;
    }
    if let Some(value) = layer.tool_output {
        runtime.tool_output = value;
    }
    if let Some(value) = layer.approval {
        runtime.approval = value;
    }
    if let Some(value) = layer.context_compaction {
        runtime.context_compaction = value;
    }
    if let Some(value) = layer.session_title {
        runtime.session_title = value;
    }
    for (id, provider) in layer.providers {
        runtime.providers.insert(id, provider);
    }
}

fn validate_runtime(runtime: &RuntimeConfig) -> Result<(), ConfigError> {
    if runtime.providers.is_empty() {
        return Err(ConfigError::EmptyProviders);
    }
    if runtime.server.host.is_empty()
        || runtime.server.host.len() > 255
        || runtime.tool_output.max_lines == 0
        || runtime.tool_output.max_bytes == 0
        || runtime.approval.timeout_ms == 0
    {
        return Err(ConfigError::InvalidRuntime);
    }
    let context = &runtime.context_compaction;
    if !(context.target_percent < context.soft_threshold_percent
        && context.soft_threshold_percent < context.hard_threshold_percent
        && context.hard_threshold_percent <= 100)
        || context.max_summary_bytes == 0
        || context.max_summary_bytes > 2 * 1024 * 1024
        || context.max_native_context_bytes == 0
        || context.max_native_context_bytes > 2 * 1024 * 1024
    {
        return Err(ConfigError::InvalidRuntime);
    }
    if runtime.session_title.max_chars == 0 || runtime.session_title.max_input_messages == 0 {
        return Err(ConfigError::InvalidRuntime);
    }
    Ok(())
}

fn validate_agent_document(
    document: &AgentDocument,
    all: &BTreeMap<AgentId, AgentDocument>,
) -> Result<(), ConfigError> {
    let frontmatter = &document.frontmatter;
    if frontmatter.description.is_empty()
        || frontmatter.description.len() > 512
        || frontmatter.description.chars().any(char::is_control)
    {
        return Err(ConfigError::AgentField {
            agent: document.id.clone(),
            field: "description",
        });
    }
    if matches!(frontmatter.mode, AgentMode::Primary) && frontmatter.model_fallback.is_empty() {
        return Err(ConfigError::PrimaryFallback(document.id.clone()));
    }
    for length in [
        frontmatter.model_fallback.len(),
        frontmatter.tools.len(),
        frontmatter.permissions.len(),
    ] {
        if length > MAX_LIST {
            return Err(ConfigError::AgentLimit(document.id.clone()));
        }
    }
    if frontmatter.tools.iter().collect::<BTreeSet<_>>().len() != frontmatter.tools.len() {
        return Err(ConfigError::DuplicateTool(document.id.clone()));
    }
    let mut rules = BTreeSet::new();
    for rule in &frontmatter.permissions {
        if !rules.insert(rule.id.clone()) {
            return Err(ConfigError::PermissionRule(document.id.clone()));
        }
    }
    if let Some(delegation) = &frontmatter.delegation {
        if delegation.agents.is_empty()
            || delegation.agents.len() > MAX_LIST
            || delegation.agents.iter().collect::<BTreeSet<_>>().len() != delegation.agents.len()
        {
            return Err(ConfigError::Delegation(document.id.clone()));
        }
        for target in &delegation.agents {
            let target_document =
                all.get(target)
                    .ok_or_else(|| ConfigError::UnknownDelegationTarget {
                        agent: document.id.clone(),
                        target: target.clone(),
                    })?;
            if !target_document.frontmatter.enabled
                || !matches!(
                    target_document.frontmatter.mode,
                    AgentMode::Subagent | AgentMode::All
                )
            {
                return Err(ConfigError::IneligibleDelegationTarget {
                    agent: document.id.clone(),
                    target: target.clone(),
                });
            }
        }
    }
    Ok(())
}

/// `*` matches any characters including `/`; `?` matches exactly one character.
#[must_use]
pub fn simple_wildcard_match(pattern: &str, resource: &str) -> bool {
    wildcard_match(pattern, resource)
        || pattern
            .strip_suffix(" *")
            .is_some_and(|prefix| wildcard_match(prefix, resource))
}
fn wildcard_match(pattern: &str, resource: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let resource = resource.chars().collect::<Vec<_>>();
    let (mut p, mut r, mut star, mut retry) = (0, 0, None, 0);
    while r < resource.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == resource[r]) {
            p += 1;
            r += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            retry = r;
        } else if let Some(index) = star {
            p = index + 1;
            retry += 1;
            r = retry;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

struct LayerRoot {
    directory: fs::File,
    source: AgentDocumentSource,
}

impl LayerRoot {
    fn load_runtime(&self) -> Result<Option<RuntimeLayer>, ConfigError> {
        let Some(bytes) = read_optional_file(
            &self.directory,
            "config.toml",
            MAX_CONFIG_BYTES,
            self.source,
        )?
        else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| ConfigError::Utf8("config.toml"))?;
        reject_toml_datetime(text)?;
        let mut value = text
            .parse::<toml::Value>()
            .map_err(|error| ConfigError::Toml(safe_toml_error(&error)))?;
        interpolate_provider_values(&mut value, &mut Vec::new())?;
        let layer: RuntimeLayer = value
            .try_into()
            .map_err(|error| ConfigError::Toml(safe_toml_error(&error)))?;
        if layer.providers.is_empty() {
            return Err(ConfigError::EmptyProviders);
        }
        Ok(Some(layer))
    }

    fn load_agents(&self) -> Result<BTreeMap<AgentId, AgentDocument>, ConfigError> {
        let Some(directory) = open_optional_directory(&self.directory, "agents", self.source)?
        else {
            return Ok(BTreeMap::new());
        };
        let mut names = Vec::new();
        let mut stream = rustix::fs::Dir::read_from(&directory)
            .map_err(|error| ConfigError::Io(error.into()))?;
        for entry in &mut stream {
            let entry = entry.map_err(|error| ConfigError::Io(error.into()))?;
            let bytes = entry.file_name().to_bytes();
            if matches!(bytes, b"." | b"..") {
                continue;
            }
            let name = std::str::from_utf8(bytes)
                .map_err(|_| ConfigError::UnsafePath)?
                .to_owned();
            if name.ends_with(".md") {
                names.push(name);
            } else if !regular_file_at(&directory, &name)? {
                return Err(ConfigError::UnsafePath);
            }
        }
        names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        let mut documents = BTreeMap::new();
        for name in names {
            let stem = name.strip_suffix(".md").expect("suffix checked");
            let id = AgentId::new(stem).map_err(|_| ConfigError::AgentFilename(name.clone()))?;
            let bytes = read_required_file(&directory, &name, MAX_AGENT_BYTES, self.source)?;
            let document = parse_agent(id.clone(), &bytes, self.source)?;
            if documents.insert(id.clone(), document).is_some() {
                return Err(ConfigError::DuplicateAgent(id));
            }
        }
        Ok(documents)
    }
}

#[cfg(unix)]
fn regular_file_at(parent: &fs::File, name: &str) -> Result<bool, ConfigError> {
    use std::os::unix::fs::MetadataExt as _;
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC;
    let file = fs::File::from(
        rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()).map_err(path_error)?,
    );
    let metadata = file.metadata().map_err(ConfigError::Io)?;
    Ok(metadata.is_file() && !metadata.file_type().is_symlink() && metadata.nlink() == 1)
}

#[cfg(not(unix))]
fn regular_file_at(_parent: &fs::File, _name: &str) -> Result<bool, ConfigError> {
    Err(ConfigError::UnsupportedPlatform)
}

fn parse_agent(
    id: AgentId,
    bytes: &[u8],
    source: AgentDocumentSource,
) -> Result<AgentDocument, ConfigError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ConfigError::Utf8("agent document"))?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if text.contains("${env:") {
        return Err(ConfigError::AgentFrontmatter(id));
    }
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| ConfigError::AgentFrontmatter(id.clone()))?;
    let closing = rest
        .find("\n---\n")
        .ok_or_else(|| ConfigError::AgentFrontmatter(id.clone()))?;
    let yaml = &rest[..closing];
    let raw_body = &rest[closing + "\n---\n".len()..];
    if yaml.len() > MAX_FRONTMATTER_BYTES || raw_body.len() > MAX_BODY_BYTES || forbidden_yaml(yaml)
    {
        return Err(ConfigError::AgentFrontmatter(id));
    }
    let yaml_value: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|_| ConfigError::AgentFrontmatter(id.clone()))?;
    validate_yaml_limits(&yaml_value, 0)?;
    let frontmatter: AgentFrontmatter = serde_yaml::from_value(yaml_value)
        .map_err(|_| ConfigError::AgentFrontmatter(id.clone()))?;
    let body = format!("{}\n", raw_body.trim_end_matches('\n'));
    if !body.chars().any(|character| !character.is_whitespace()) {
        return Err(ConfigError::EmptyPrompt(id));
    }
    let document_fingerprint = hash(
        "cookie-agent/agent-document/v1",
        &(id.as_str(), yaml, &body),
    )?;
    let prompt_fingerprint = hash("cookie-agent/system-prompt/v1", &body)?;
    Ok(AgentDocument {
        id,
        frontmatter,
        body,
        source,
        document_fingerprint,
        prompt_fingerprint,
    })
}

fn forbidden_yaml(yaml: &str) -> bool {
    yaml.lines().any(|line| {
        if line.trim_start().starts_with("<<:") {
            return true;
        }
        let mut single = false;
        let mut double = false;
        let mut escaped = false;
        for (index, character) in line.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if double && character == '\\' {
                escaped = true;
                continue;
            }
            match character {
                '\'' if !double => single = !single,
                '"' if !single => double = !double,
                '#' if !single && !double => break,
                '&' | '!' | '*' if !single && !double => {
                    let boundary = index == 0
                        || line[..index]
                            .chars()
                            .next_back()
                            .is_some_and(char::is_whitespace);
                    if boundary {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    })
}

fn validate_yaml_limits(value: &serde_yaml::Value, depth: usize) -> Result<(), ConfigError> {
    if depth > 16 {
        return Err(ConfigError::AgentYamlLimit);
    }
    match value {
        serde_yaml::Value::Sequence(values) => {
            if values.len() > MAX_LIST {
                return Err(ConfigError::AgentYamlLimit);
            }
            for value in values {
                validate_yaml_limits(value, depth + 1)?;
            }
        }
        serde_yaml::Value::Mapping(values) => {
            if values.len() > MAX_LIST {
                return Err(ConfigError::AgentYamlLimit);
            }
            for (key, value) in values {
                validate_yaml_limits(key, depth + 1)?;
                validate_yaml_limits(value, depth + 1)?;
            }
        }
        serde_yaml::Value::Tagged(_) => return Err(ConfigError::AgentYamlLimit),
        serde_yaml::Value::String(value) if value.len() > 128 * 1024 => {
            return Err(ConfigError::AgentYamlLimit);
        }
        _ => {}
    }
    Ok(())
}

fn hash(domain: &str, value: &impl Serialize) -> Result<Sha256Digest, ConfigError> {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(value).map_err(ConfigError::Json)?);
    Sha256Digest::new(format!("{:x}", hasher.finalize())).map_err(|_| ConfigError::Fingerprint)
}

fn user_root() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config/cookie_agent"))
}

#[cfg(unix)]
fn open_layer_root(
    path: &Path,
    source: AgentDocumentSource,
) -> Result<Option<LayerRoot>, ConfigError> {
    use std::os::unix::fs::MetadataExt as _;
    if !path.is_absolute() {
        return Err(ConfigError::UnsafePath);
    }
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let mut current = fs::File::from(
        rustix::fs::open("/", flags, rustix::fs::Mode::empty()).map_err(path_error)?,
    );
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                match rustix::fs::openat(&current, name, flags, rustix::fs::Mode::empty()) {
                    Ok(fd) => current = fs::File::from(fd),
                    Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
                    Err(error) => return Err(path_error(error)),
                }
            }
            _ => return Err(ConfigError::UnsafePath),
        }
    }
    let metadata = current.metadata().map_err(ConfigError::Io)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ConfigError::UnsafePath);
    }
    if source == AgentDocumentSource::User
        && (metadata.uid() != rustix::process::getuid().as_raw()
            || metadata.mode() & 0o777 != 0o700)
    {
        return Err(ConfigError::UnsafePath);
    }
    Ok(Some(LayerRoot {
        directory: current,
        source,
    }))
}

#[cfg(not(unix))]
fn open_layer_root(
    _path: &Path,
    _source: AgentDocumentSource,
) -> Result<Option<LayerRoot>, ConfigError> {
    Err(ConfigError::UnsupportedPlatform)
}

#[cfg(unix)]
fn open_optional_directory(
    parent: &fs::File,
    name: &str,
    source: AgentDocumentSource,
) -> Result<Option<fs::File>, ConfigError> {
    use std::os::unix::fs::MetadataExt as _;
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let file = match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
        Ok(fd) => fs::File::from(fd),
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) => return Err(path_error(error)),
    };
    let metadata = file.metadata().map_err(ConfigError::Io)?;
    if !metadata.is_dir()
        || source == AgentDocumentSource::User
            && (metadata.uid() != rustix::process::getuid().as_raw()
                || metadata.mode() & 0o777 != 0o700)
    {
        return Err(ConfigError::UnsafePath);
    }
    Ok(Some(file))
}

#[cfg(not(unix))]
fn open_optional_directory(
    _parent: &fs::File,
    _name: &str,
    _source: AgentDocumentSource,
) -> Result<Option<fs::File>, ConfigError> {
    Err(ConfigError::UnsupportedPlatform)
}

fn read_optional_file(
    parent: &fs::File,
    name: &str,
    limit: u64,
    source: AgentDocumentSource,
) -> Result<Option<Vec<u8>>, ConfigError> {
    match read_file(parent, name, limit, source) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(ConfigError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}
fn read_required_file(
    parent: &fs::File,
    name: &str,
    limit: u64,
    source: AgentDocumentSource,
) -> Result<Vec<u8>, ConfigError> {
    read_file(parent, name, limit, source)
}

#[cfg(unix)]
fn read_file(
    parent: &fs::File,
    name: &str,
    limit: u64,
    source: AgentDocumentSource,
) -> Result<Vec<u8>, ConfigError> {
    use std::{io::Read as _, os::unix::fs::MetadataExt as _};
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC;
    let file = match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
        Ok(fd) => fs::File::from(fd),
        Err(error) if error == rustix::io::Errno::NOENT => return Err(ConfigError::NotFound),
        Err(error) => return Err(path_error(error)),
    };
    let metadata = file.metadata().map_err(ConfigError::Io)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.nlink() != 1
        || source == AgentDocumentSource::User
            && (metadata.uid() != rustix::process::getuid().as_raw()
                || metadata.mode() & 0o777 != 0o600)
    {
        return Err(ConfigError::UnsafePath);
    }
    if metadata.len() > limit {
        return Err(ConfigError::TooLarge(name.to_owned()));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(ConfigError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(ConfigError::TooLarge(name.to_owned()));
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_file(
    _parent: &fs::File,
    _name: &str,
    _limit: u64,
    _source: AgentDocumentSource,
) -> Result<Vec<u8>, ConfigError> {
    Err(ConfigError::UnsupportedPlatform)
}

#[cfg(unix)]
fn path_error(error: rustix::io::Errno) -> ConfigError {
    let error: io::Error = error.into();
    if matches!(
        error.raw_os_error(),
        Some(code)
            if matches!(
                code,
                libc::ELOOP
                    | libc::ENOTDIR
                    | libc::ENXIO
                    | libc::ENODEV
                    | libc::EOPNOTSUPP
            )
    ) {
        ConfigError::UnsafePath
    } else {
        ConfigError::Io(error)
    }
}

fn reject_toml_datetime(text: &str) -> Result<(), ConfigError> {
    let value = text
        .parse::<toml::Value>()
        .map_err(|error| ConfigError::Toml(safe_toml_error(&error)))?;
    fn visit(value: &toml::Value, depth: usize) -> Result<(), ConfigError> {
        if depth > 32 {
            return Err(ConfigError::TomlLimit);
        }
        match value {
            toml::Value::Datetime(_) => Err(ConfigError::TomlLimit),
            toml::Value::String(value) if value.len() > 256 * 1024 => Err(ConfigError::TomlLimit),
            toml::Value::Array(values) => {
                if values.len() > 4096 {
                    return Err(ConfigError::TomlLimit);
                }
                for value in values {
                    visit(value, depth + 1)?;
                }
                Ok(())
            }
            toml::Value::Table(values) => {
                if values.len() > 4096 {
                    return Err(ConfigError::TomlLimit);
                }
                for value in values.values() {
                    visit(value, depth + 1)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    visit(&value, 0)
}

fn safe_toml_error(error: &toml::de::Error) -> String {
    let message = error.message();
    if let Some(span) = error.span() {
        format!("{message} at bytes {}..{}", span.start, span.end)
    } else {
        message.to_owned()
    }
}

fn interpolate_provider_values(
    value: &mut toml::Value,
    path: &mut Vec<String>,
) -> Result<(), ConfigError> {
    match value {
        toml::Value::String(text) if interpolation_allowed(path) => {
            *text = interpolate(text, path)?
        }
        toml::Value::String(text) if text.contains("${env:") => {
            return Err(ConfigError::Interpolation(path.join(".")));
        }
        toml::Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                path.push(index.to_string());
                interpolate_provider_values(value, path)?;
                path.pop();
            }
        }
        toml::Value::Table(values) => {
            for (key, value) in values {
                path.push(key.clone());
                interpolate_provider_values(value, path)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

fn interpolation_allowed(path: &[String]) -> bool {
    matches!(path, [providers, _, endpoint] if providers == "providers" && endpoint == "endpoint")
        || matches!(path, [providers, _, headers, _] if providers == "providers" && headers == "headers")
        || matches!(path, [providers, _, auth, field] if providers == "providers" && auth == "auth" && matches!(field.as_str(), "token" | "key" | "username" | "password"))
        || matches!(path, [providers, _, auth, values, _] if providers == "providers" && auth == "auth" && values == "values")
}

fn interpolate(value: &str, path: &[String]) -> Result<String, ConfigError> {
    let mut output = String::new();
    let mut cursor = 0;
    let bytes = value.as_bytes();
    while cursor < bytes.len() {
        if bytes[cursor] != b'$' {
            output.push(value[cursor..].chars().next().expect("valid utf8"));
            cursor += value[cursor..]
                .chars()
                .next()
                .expect("valid utf8")
                .len_utf8();
            continue;
        }
        if bytes.get(cursor + 1) == Some(&b'$') {
            output.push('$');
            cursor += 2;
            continue;
        }
        if !value[cursor..].starts_with("${env:") {
            return Err(ConfigError::Interpolation(path.join(".")));
        }
        let start = cursor + 6;
        let end = value[start..]
            .find('}')
            .map(|offset| start + offset)
            .ok_or_else(|| ConfigError::Interpolation(path.join(".")))?;
        let name = &value[start..end];
        if !valid_env_name(name) {
            return Err(ConfigError::Interpolation(path.join(".")));
        }
        let resolved = env::var_os(name)
            .ok_or_else(|| ConfigError::MissingEnvironment {
                path: path.join("."),
                variable: name.to_owned(),
            })?
            .into_string()
            .map_err(|_| ConfigError::NonUtf8Environment {
                path: path.join("."),
                variable: name.to_owned(),
            })?;
        output.push_str(&resolved);
        cursor = end + 1;
    }
    Ok(output)
}
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && (name.as_bytes()[0].is_ascii_uppercase() || name.as_bytes()[0] == b'_')
        && name
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

#[derive(Error)]
pub enum ConfigError {
    #[error("configuration I/O failed")]
    Io(#[source] io::Error),
    #[error("configuration path is unsafe")]
    UnsafePath,
    #[error("configuration loading is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("required file was not found")]
    NotFound,
    #[error("{0} exceeds its byte limit")]
    TooLarge(String),
    #[error("{0} is not UTF-8")]
    Utf8(&'static str),
    #[error("invalid TOML: {0}")]
    Toml(String),
    #[error("TOML resource limit or unsupported datetime exceeded")]
    TomlLimit,
    #[error("runtime providers must be nonempty")]
    EmptyProviders,
    #[error("runtime settings are invalid")]
    InvalidRuntime,
    #[error("environment interpolation is invalid at `{0}`")]
    Interpolation(String),
    #[error("environment variable `{variable}` is missing at `{path}`")]
    MissingEnvironment { path: String, variable: String },
    #[error("environment variable `{variable}` is not UTF-8 at `{path}`")]
    NonUtf8Environment { path: String, variable: String },
    #[error("invalid agent filename `{0}`")]
    AgentFilename(String),
    #[error("duplicate agent `{0}`")]
    DuplicateAgent(AgentId),
    #[error("invalid frontmatter for agent `{0}`")]
    AgentFrontmatter(AgentId),
    #[error("agent YAML resource limit exceeded")]
    AgentYamlLimit,
    #[error("agent `{0}` has an empty system prompt")]
    EmptyPrompt(AgentId),
    #[error("agent `{agent}` has invalid `{field}`")]
    AgentField { agent: AgentId, field: &'static str },
    #[error("agent `{0}` exceeds a list limit")]
    AgentLimit(AgentId),
    #[error("primary agent `{0}` requires a nonempty fallback")]
    PrimaryFallback(AgentId),
    #[error("agent `{0}` lists a tool more than once")]
    DuplicateTool(AgentId),
    #[error("agent `{0}` has an invalid or duplicate permission rule")]
    PermissionRule(AgentId),
    #[error("agent `{0}` has invalid delegation")]
    Delegation(AgentId),
    #[error("agent `{agent}` delegates to unknown agent `{target}`")]
    UnknownDelegationTarget { agent: AgentId, target: AgentId },
    #[error("agent `{agent}` delegates to ineligible agent `{target}`")]
    IneligibleDelegationTarget { agent: AgentId, target: AgentId },
    #[error("agent `{agent}` repeats fallback model `{model}`")]
    DuplicateFallbackModel { agent: AgentId, model: ModelKey },
    #[error("agent `{agent}` references unknown or disabled model `{model}`")]
    UnknownModel { agent: AgentId, model: ModelKey },
    #[error("agent `{agent}` references unknown variant `{variant}` for `{model}`")]
    UnknownVariant {
        agent: AgentId,
        model: ModelKey,
        variant: String,
    },
    #[error("agent `{0}` is not currently runnable as root")]
    IneligibleRootAgent(AgentId),
    #[error("agent `{0}` was resolved against a different model snapshot")]
    ModelSnapshotMismatch(AgentId),
    #[error("agent `{agent}` cannot select unavailable or unknown model `{model}`")]
    InvalidRunSelection { agent: AgentId, model: ModelKey },
    #[error("fingerprint encoding failed")]
    Json(#[source] serde_json::Error),
    #[error("fingerprint construction failed")]
    Fingerprint,
}

impl fmt::Debug for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ConfigError")
            .field(&self.to_string())
            .finish()
    }
}

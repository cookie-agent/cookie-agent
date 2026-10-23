use std::collections::{BTreeMap, BTreeSet};

use cookie_agent_identity::{AgentId, ConfiguredVariantRef, ModelKey, RuntimeId, WildcardPattern};
pub use cookie_agent_protocol::{AgentMode, PermissionAction, PermissionEffect, PermissionRule};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{AgentDocument, ConfigError};

const MAX_LIST: usize = 256;
pub const BUILT_IN_DEFAULT_AGENT_ID: &str = "default";
pub const BUILT_IN_APPROVAL_AGENT_ID: &str = "approval";
pub const BUILT_IN_COMPACTION_AGENT_ID: &str = "compaction";
pub const BUILT_IN_TITLE_AGENT_ID: &str = "title";
pub const PARENT_MODEL_EXPRESSION: &str = "${parent_model}";
pub const DEFAULT_MODEL_EXPRESSION: &str = "${default_model}";
pub const SMALL_MODEL_EXPRESSION: &str = "${small_model}";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PermissionValue {
    Effect(PermissionEffect),
    Resources(IndexMap<WildcardPattern, PermissionEffect>),
}

impl<'de> Deserialize<'de> for PermissionValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PermissionValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a permission effect or ordered resource map")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let effect = match value {
                    "allow" => PermissionEffect::Allow,
                    "ask" => PermissionEffect::Ask,
                    "deny" => PermissionEffect::Deny,
                    _ => return Err(E::custom("invalid permission effect")),
                };
                Ok(PermissionValue::Effect(effect))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut resources = IndexMap::new();
                while let Some((resource, effect)) =
                    map.next_entry::<WildcardPattern, PermissionEffect>()?
                {
                    if resources.insert(resource, effect).is_some() {
                        return Err(serde::de::Error::custom("duplicate permission resource"));
                    }
                }
                Ok(PermissionValue::Resources(resources))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// A permission map key: an action with an optional runtime selector,
/// written `action` or `action@runtime_id` (for example `bash@labpc`).
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PermissionKey {
    pub action: PermissionAction,
    pub runtime: Option<RuntimeId>,
}

/// An unqualified key hashes like its bare action, so a permission map can be
/// looked up by [`PermissionAction`].
impl std::hash::Hash for PermissionKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.action.hash(state);
        if let Some(runtime) = &self.runtime {
            runtime.hash(state);
        }
    }
}

/// A bare action looks up its unqualified key.
impl indexmap::Equivalent<PermissionKey> for PermissionAction {
    fn equivalent(&self, key: &PermissionKey) -> bool {
        key.runtime.is_none() && key.action == *self
    }
}

impl From<&PermissionKey> for PermissionKey {
    fn from(key: &PermissionKey) -> Self {
        key.clone()
    }
}

impl PermissionKey {
    /// The key for `action` on every runtime and the base layer.
    #[must_use]
    pub const fn any(action: PermissionAction) -> Self {
        Self {
            action,
            runtime: None,
        }
    }
}

impl From<PermissionAction> for PermissionKey {
    fn from(action: PermissionAction) -> Self {
        Self::any(action)
    }
}

impl std::fmt::Display for PermissionKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let action = self.action.as_str();
        match &self.runtime {
            Some(runtime) => write!(formatter, "{action}@{runtime}"),
            None => formatter.write_str(action),
        }
    }
}

impl std::str::FromStr for PermissionKey {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (action, runtime) = cookie_agent_protocol::parse_permission_key(value)?;
        Ok(Self { action, runtime })
    }
}

impl Serialize for PermissionKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PermissionKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

fn deserialize_permissions<'de, D>(
    deserializer: D,
) -> Result<IndexMap<PermissionKey, PermissionValue>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = IndexMap<PermissionKey, PermissionValue>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an ordered action permission map")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut permissions = IndexMap::new();
            while let Some((key, value)) = map.next_entry::<PermissionKey, PermissionValue>()? {
                if permissions.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate permission action"));
                }
            }
            Ok(permissions)
        }
    }
    deserializer.deserialize_map(Visitor)
}

impl PermissionValue {
    /// The rules this value declares under `key`, in declaration order.
    pub fn rules(&self, key: impl Into<PermissionKey>) -> Vec<PermissionRule> {
        let key = key.into();
        match self {
            Self::Effect(effect) => vec![PermissionRule {
                action: key.action.clone(),
                runtime: key.runtime.clone(),
                resource: WildcardPattern::new("*").expect("static wildcard"),
                effect: *effect,
            }],
            Self::Resources(resources) => resources
                .iter()
                .map(|(resource, effect)| PermissionRule {
                    action: key.action.clone(),
                    runtime: key.runtime.clone(),
                    resource: resource.clone(),
                    effect: *effect,
                })
                .collect(),
        }
    }
}

/// The model of a `models` entry: a model key, or one of the variables
/// `${parent_model}` (internal agents only), `${default_model}`, and
/// `${small_model}`, which carry a model and its variant.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum AgentModelRef {
    Model(ModelKey),
    ParentModel,
    /// `${default_model}`: the configuration's `default_model`.
    DefaultModel,
    /// `${small_model}`: the configuration's `small_model`.
    SmallModel,
}

impl AgentModelRef {
    /// The configuration field a variable names: `default_model` or
    /// `small_model`; `None` for a model key and `${parent_model}`.
    #[must_use]
    pub const fn config_field(&self) -> Option<&'static str> {
        match self {
            Self::DefaultModel => Some("default_model"),
            Self::SmallModel => Some("small_model"),
            Self::Model(_) | Self::ParentModel => None,
        }
    }
}

impl std::str::FromStr for AgentModelRef {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            PARENT_MODEL_EXPRESSION => Ok(Self::ParentModel),
            DEFAULT_MODEL_EXPRESSION => Ok(Self::DefaultModel),
            SMALL_MODEL_EXPRESSION => Ok(Self::SmallModel),
            _ => value
                .parse::<ModelKey>()
                .map(Self::Model)
                .map_err(|error| error.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for AgentModelRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// One `models` entry: a mapping `{ model, variant, cache }`, or a plain
/// string, which is the entry's `model` with nothing overridden.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AgentModelFallback {
    pub model: AgentModelRef,
    /// Overrides the variant a variable carries; for a model key, the
    /// model's default variant when absent.
    pub variant: Option<ConfiguredVariantRef>,
    pub cache: Option<crate::CacheConfig>,
}

impl<'de> Deserialize<'de> for AgentModelFallback {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Entry {
            model: AgentModelRef,
            #[serde(default)]
            variant: Option<ConfiguredVariantRef>,
            #[serde(default)]
            cache: Option<crate::CacheConfig>,
        }

        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = AgentModelFallback;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a model string or a mapping { model, variant, cache }")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(AgentModelFallback {
                    model: value.parse().map_err(E::custom)?,
                    variant: None,
                    cache: None,
                })
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let entry = Entry::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(AgentModelFallback {
                    model: entry.model,
                    variant: entry.variant,
                    cache: entry.cache,
                })
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentLimits {
    pub timeout_ms: u64,
    pub max_output_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentFrontmatter {
    pub description: String,
    pub mode: AgentMode,
    pub enabled: bool,
    pub models: Vec<AgentModelFallback>,
    #[serde(default)]
    pub limits: AgentLimits,
    #[serde(default, deserialize_with = "deserialize_permissions")]
    pub permissions: IndexMap<PermissionKey, PermissionValue>,
    /// Per-agent AGENTS.md loading. `None` defers to `[agent_md] enabled`;
    /// a value overrides it for root runs of this agent.
    #[serde(default)]
    pub agent_md: Option<bool>,
    /// Present on the document of a runtime-hosted ACP agent
    /// (`docs/site/specs/acp-agents.md` section 4), which names no models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpDocumentSettings>,
}

/// The `acp` block of an ACP agent's document.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AcpDocumentSettings {
    /// The mode the agent starts its sessions in, by mode ID; the agent's
    /// `auto` mode, else its default, when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Initial values of the agent's config options, by option ID.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
    /// Patterns (`*`, `?`) over cookie's tool names the agent does not get;
    /// cookie's coding runtime tools when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hide_tools: Option<Vec<String>>,
}

impl AcpDocumentSettings {
    /// The tools an ACP agent does not get when its document names none:
    /// the coding runtime's, which the agent has its own of.
    pub const DEFAULT_HIDE_TOOLS: [&'static str; 5] = ["read", "write", "edit", "bash", "webfetch"];

    /// The patterns of the tools the agent does not get.
    #[must_use]
    pub fn hidden_tools(&self) -> Vec<String> {
        self.hide_tools.clone().unwrap_or_else(|| {
            Self::DEFAULT_HIDE_TOOLS
                .iter()
                .map(|tool| (*tool).to_owned())
                .collect()
        })
    }
}

impl<'de> Deserialize<'de> for AgentFrontmatter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct RawLimits {
            timeout_ms: Option<u64>,
            max_output_tokens: Option<u64>,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawFrontmatter {
            description: String,
            mode: AgentMode,
            #[serde(default = "enabled_by_default")]
            enabled: bool,
            #[serde(default)]
            models: Vec<AgentModelFallback>,
            #[serde(default)]
            limits: RawLimits,
            #[serde(default, deserialize_with = "deserialize_permissions")]
            permissions: IndexMap<PermissionKey, PermissionValue>,
            #[serde(default)]
            agent_md: Option<bool>,
            #[serde(default)]
            acp: Option<AcpDocumentSettings>,
        }

        const fn enabled_by_default() -> bool {
            true
        }

        let raw = RawFrontmatter::deserialize(deserializer)?;
        let internal = raw.mode == AgentMode::Internal;
        Ok(Self {
            description: raw.description,
            mode: raw.mode,
            enabled: raw.enabled,
            models: raw.models,
            limits: AgentLimits {
                timeout_ms: raw
                    .limits
                    .timeout_ms
                    .unwrap_or(if internal { 30_000 } else { 0 }),
                // Internal agents default to no document cap: at run time a zero
                // document cap inherits the owner run's cap, mirroring how they
                // inherit `${parent_model}`.
                max_output_tokens: raw.limits.max_output_tokens.unwrap_or(0),
            },
            permissions: raw.permissions,
            agent_md: raw.agent_md,
            acp: raw.acp,
        })
    }
}

/// Validated authored-agent input for downstream runtime materialization.
///
/// `root_eligible` covers only authored policy. P7 computes `runnable_as_root`
/// by additionally requiring at least one available materialized fallback.
#[derive(Clone, Copy, Debug)]
pub struct AgentMaterializationInput<'a> {
    pub document: &'a AgentDocument,
    pub root_eligible: bool,
}

/// Atomically validated authored agent documents, independent of model execution.
#[derive(Clone, Debug)]
pub struct AgentRegistry {
    agents: BTreeMap<AgentId, AgentDocument>,
}

impl AgentRegistry {
    pub fn validate(agents: BTreeMap<AgentId, AgentDocument>) -> Result<Self, ConfigError> {
        Self::validate_ref(&agents)?;
        Ok(Self { agents })
    }

    pub(crate) fn validate_ref(
        agents: &BTreeMap<AgentId, AgentDocument>,
    ) -> Result<(), ConfigError> {
        for document in agents.values() {
            validate_agent_document(document, agents)?;
        }
        Ok(())
    }

    pub(crate) fn from_validated(agents: BTreeMap<AgentId, AgentDocument>) -> Self {
        Self { agents }
    }

    #[must_use]
    pub fn get(&self, id: &AgentId) -> Option<&AgentDocument> {
        self.agents.get(id)
    }

    pub fn agents(&self) -> impl ExactSizeIterator<Item = (&AgentId, &AgentDocument)> {
        self.agents.iter()
    }

    #[must_use]
    pub fn documents(&self) -> &BTreeMap<AgentId, AgentDocument> {
        &self.agents
    }

    pub fn materialization_inputs(
        &self,
    ) -> impl ExactSizeIterator<Item = AgentMaterializationInput<'_>> {
        self.agents
            .values()
            .map(|document| AgentMaterializationInput {
                document,
                root_eligible: authored_root_eligible(document),
            })
    }
}

fn authored_root_eligible(document: &AgentDocument) -> bool {
    document.frontmatter.acp.is_none()
        && document.frontmatter.enabled
        && matches!(
            document.frontmatter.mode,
            AgentMode::Primary | AgentMode::All
        )
        && !document.frontmatter.models.is_empty()
}

/// An ACP agent's document: the runtime launches the agent and the agent
/// picks its model, so it names no models, and it reads its own context
/// files.
fn validate_acp_settings(
    document: &AgentDocument,
    acp: &AcpDocumentSettings,
) -> Result<(), ConfigError> {
    let frontmatter = &document.frontmatter;
    let field = if !frontmatter.models.is_empty() {
        Some("models")
    } else if frontmatter.agent_md.is_some() {
        Some("agent_md")
    } else if frontmatter.mode == AgentMode::Internal {
        Some("mode")
    } else if frontmatter.limits != AgentLimits::default() {
        Some("limits")
    } else if acp.mode.as_ref().is_some_and(|mode| !valid_acp_label(mode))
        || acp
            .config
            .iter()
            .any(|(id, value)| !valid_acp_label(id) || !valid_acp_label(value))
    {
        Some("acp")
    } else if acp.hide_tools.as_ref().is_some_and(|patterns| {
        patterns.len() > MAX_LIST
            || patterns.iter().any(|pattern| {
                pattern.is_empty() || pattern.len() > 128 || pattern.chars().any(char::is_control)
            })
    }) {
        Some("acp.hide_tools")
    } else {
        None
    };
    match field {
        Some(field) => Err(ConfigError::AgentField {
            agent: document.id.clone(),
            field,
        }),
        None => Ok(()),
    }
}

/// An ACP mode, option, or value ID as a document names it: a nonempty
/// label of at most 256 bytes without control characters.
fn valid_acp_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= 256 && !label.chars().any(char::is_control)
}

fn validate_agent_document(
    document: &AgentDocument,
    all: &BTreeMap<AgentId, AgentDocument>,
) -> Result<(), ConfigError> {
    if document.id.as_str() == BUILT_IN_DEFAULT_AGENT_ID {
        return Err(ConfigError::ReservedAgentId(document.id.clone()));
    }
    let frontmatter = &document.frontmatter;
    if matches!(
        document.id.as_str(),
        BUILT_IN_APPROVAL_AGENT_ID | BUILT_IN_COMPACTION_AGENT_ID | BUILT_IN_TITLE_AGENT_ID
    ) && frontmatter.mode != AgentMode::Internal
    {
        return Err(ConfigError::AgentField {
            agent: document.id.clone(),
            field: "mode",
        });
    }
    if let Some(allowed) =
        crate::prompt_template::internal_prompt_placeholders(document.id.as_str())
        && let Some(placeholder) =
            crate::prompt_template::unknown_placeholder(&document.body, allowed)
    {
        return Err(ConfigError::UnknownPromptPlaceholder {
            agent: document.id.clone(),
            placeholder,
        });
    }
    if frontmatter.description.is_empty()
        || frontmatter.description.len() > 512
        || frontmatter.description.chars().any(char::is_control)
    {
        return Err(ConfigError::AgentField {
            agent: document.id.clone(),
            field: "description",
        });
    }
    if let Some(acp) = &frontmatter.acp {
        validate_acp_settings(document, acp)?;
    }
    // Only root runs load AGENTS.md, so the override is meaningless on agents
    // that can never run as a root.
    if frontmatter.agent_md.is_some()
        && matches!(frontmatter.mode, AgentMode::Subagent | AgentMode::Internal)
    {
        return Err(ConfigError::AgentField {
            agent: document.id.clone(),
            field: "agent_md",
        });
    }
    if matches!(frontmatter.mode, AgentMode::Primary)
        && frontmatter.models.is_empty()
        && frontmatter.acp.is_none()
    {
        return Err(ConfigError::PrimaryFallback(document.id.clone()));
    }
    if frontmatter.models.len() > MAX_LIST {
        return Err(ConfigError::AgentLimit(document.id.clone()));
    }
    let mut fallback_models = BTreeSet::new();
    for fallback in &frontmatter.models {
        // `${parent_model}` names the model of the run that owns an
        // internal agent's run; other agents have none.
        if matches!(fallback.model, AgentModelRef::ParentModel)
            && frontmatter.mode != AgentMode::Internal
        {
            return Err(ConfigError::AgentField {
                agent: document.id.clone(),
                field: "models",
            });
        }
        if !fallback_models.insert(fallback.model.clone()) {
            return Err(ConfigError::AgentField {
                agent: document.id.clone(),
                field: "models",
            });
        }
    }
    if frontmatter.mode != AgentMode::Internal && frontmatter.limits.timeout_ms != 0 {
        return Err(ConfigError::AgentTimeoutInternalOnly(document.id.clone()));
    }
    let permission_rules = frontmatter
        .permissions
        .values()
        .map(|value| match value {
            PermissionValue::Effect(_) => 1,
            PermissionValue::Resources(resources) => resources.len(),
        })
        .sum::<usize>();
    if permission_rules > MAX_LIST {
        return Err(ConfigError::AgentLimit(document.id.clone()));
    }
    let delegate_key = PermissionKey::any(PermissionAction::DELEGATE);
    if let Some(delegate) = frontmatter.permissions.get(&delegate_key) {
        let rules = delegate.rules(&delegate_key);
        // Expanded targets land in AgentDescriptor and FrozenDelegationPolicy,
        // both limited to 256 IDs; validate the deduplicated union exactly as
        // the runtime expansion produces it.
        let mut expanded = BTreeSet::new();
        for rule in rules {
            if rule.effect == PermissionEffect::Deny {
                continue;
            }
            let eligible = |target_document: &AgentDocument| {
                target_document.frontmatter.enabled
                    && matches!(
                        target_document.frontmatter.mode,
                        AgentMode::Subagent | AgentMode::All
                    )
            };
            let pattern = rule.resource.as_str();
            if pattern.contains('*') || pattern.contains('?') {
                // Wildcard delegation uses the same runtime matching as every
                // other permission. A pattern that matches no eligible agent
                // yet may match an ACP agent a runtime reports later: the
                // engine reports it as a configuration diagnostic.
                expanded.extend(
                    all.iter()
                        .filter(|(id, target_document)| {
                            crate::simple_wildcard_match(pattern, id.as_str())
                                && eligible(target_document)
                        })
                        .map(|(id, _)| id.clone()),
                );
                continue;
            }
            let target =
                AgentId::new(pattern).map_err(|_| ConfigError::Delegation(document.id.clone()))?;
            // A target without a document may be an ACP agent a runtime
            // reports later; the engine reports it as a configuration
            // diagnostic until one does.
            let Some(target_document) = all.get(&target) else {
                continue;
            };
            if !eligible(target_document) {
                return Err(ConfigError::IneligibleDelegationTarget {
                    agent: document.id.clone(),
                    target: target.clone(),
                });
            }
            expanded.insert(target);
        }
        if expanded.len() > MAX_LIST {
            return Err(ConfigError::AgentLimit(document.id.clone()));
        }
    }
    for (key, value) in &frontmatter.permissions {
        if value.rules(key).iter().any(|rule| {
            !crate::agent_document::permission_expressions_valid(
                key.action.as_str(),
                rule.resource.as_str(),
            )
        }) {
            return Err(ConfigError::AgentPermissionExpression(document.id.clone()));
        }
    }
    Ok(())
}

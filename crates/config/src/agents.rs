use std::collections::{BTreeMap, BTreeSet};

use cookie_agent_identity::{AgentId, ConfiguredVariantRef, ModelKey, WildcardPattern};
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

fn deserialize_permissions<'de, D>(
    deserializer: D,
) -> Result<IndexMap<PermissionAction, PermissionValue>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = IndexMap<PermissionAction, PermissionValue>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an ordered action permission map")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut permissions = IndexMap::new();
            while let Some((action, value)) =
                map.next_entry::<PermissionAction, PermissionValue>()?
            {
                if permissions.insert(action, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate permission action"));
                }
            }
            Ok(permissions)
        }
    }
    deserializer.deserialize_map(Visitor)
}

impl PermissionValue {
    pub fn rules(&self, action: PermissionAction) -> Vec<PermissionRule> {
        match self {
            Self::Effect(effect) => vec![PermissionRule {
                action,
                resource: WildcardPattern::new("*").expect("static wildcard"),
                effect: *effect,
            }],
            Self::Resources(resources) => resources
                .iter()
                .map(|(resource, effect)| PermissionRule {
                    action,
                    resource: resource.clone(),
                    effect: *effect,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum AgentModelRef {
    Model(ModelKey),
    ParentModel,
}

impl<'de> Deserialize<'de> for AgentModelRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == PARENT_MODEL_EXPRESSION {
            Ok(Self::ParentModel)
        } else {
            value
                .parse::<ModelKey>()
                .map(Self::Model)
                .map_err(serde::de::Error::custom)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModelFallback {
    pub model: AgentModelRef,
    pub variant: Option<ConfiguredVariantRef>,
    pub cache: Option<crate::CacheConfig>,
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
    pub permissions: IndexMap<PermissionAction, PermissionValue>,
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
            enabled: bool,
            models: Vec<AgentModelFallback>,
            #[serde(default)]
            limits: RawLimits,
            #[serde(default, deserialize_with = "deserialize_permissions")]
            permissions: IndexMap<PermissionAction, PermissionValue>,
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
                max_output_tokens: raw.limits.max_output_tokens.unwrap_or(if internal {
                    2_048
                } else {
                    0
                }),
            },
            permissions: raw.permissions,
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

    #[must_use]
    pub fn into_documents(self) -> BTreeMap<AgentId, AgentDocument> {
        self.agents
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
    document.frontmatter.enabled
        && matches!(
            document.frontmatter.mode,
            AgentMode::Primary | AgentMode::All
        )
        && !document.frontmatter.models.is_empty()
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
    if frontmatter.description.is_empty()
        || frontmatter.description.len() > 512
        || frontmatter.description.chars().any(char::is_control)
    {
        return Err(ConfigError::AgentField {
            agent: document.id.clone(),
            field: "description",
        });
    }
    if matches!(frontmatter.mode, AgentMode::Primary) && frontmatter.models.is_empty() {
        return Err(ConfigError::PrimaryFallback(document.id.clone()));
    }
    if frontmatter.models.len() > MAX_LIST {
        return Err(ConfigError::AgentLimit(document.id.clone()));
    }
    let mut fallback_models = BTreeSet::new();
    for fallback in &frontmatter.models {
        if matches!(fallback.model, AgentModelRef::ParentModel)
            && (frontmatter.mode != AgentMode::Internal || fallback.variant.is_some())
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
    if let Some(delegate) = frontmatter.permissions.get(&PermissionAction::Delegate) {
        let rules = delegate.rules(PermissionAction::Delegate);
        for rule in rules {
            if rule.effect == PermissionEffect::Deny {
                continue;
            }
            let target = AgentId::new(rule.resource.as_str())
                .map_err(|_| ConfigError::Delegation(document.id.clone()))?;
            let target_document =
                all.get(&target)
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
    for (action, value) in &frontmatter.permissions {
        if !matches!(action, PermissionAction::Read | PermissionAction::Write)
            && value.rules(*action).iter().any(|rule| {
                rule.resource
                    .as_str()
                    .contains(WildcardPattern::WORKSPACE_DIR_EXPRESSION)
            })
        {
            return Err(ConfigError::AgentPermissionExpression(document.id.clone()));
        }
    }
    Ok(())
}

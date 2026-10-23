use std::collections::BTreeMap;

use cookie_agent_config::ConfiguredModel;
use cookie_agent_config::{
    AgentDocument, AgentDocumentSource, AgentFrontmatter, AgentLimits, AgentMode,
    AgentModelFallback, AgentModelRef, AgentRegistry as ConfigAgentRegistry,
    BUILT_IN_APPROVAL_AGENT_ID, BUILT_IN_COMPACTION_AGENT_ID, BUILT_IN_DEFAULT_AGENT_ID,
    BUILT_IN_TITLE_AGENT_ID, PermissionAction, PermissionEffect, PermissionValue,
};
use cookie_agent_identity::{
    AgentId as IdentityAgentId, ConfiguredVariantRef, ModelKey, WildcardPattern,
};
use cookie_agent_models::{CompiledModelRuntime, compiler::CompiledModelStatus};
use cookie_agent_protocol::{AgentDescriptor, AgentId, ModelSelection};
use indexmap::IndexMap;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::EngineError;

#[derive(Clone, Debug)]
pub(crate) struct ResolvedAgent {
    pub document: AgentDocument,
    pub resolved_fallback: Vec<ResolvedAgentFallback>,
    /// The configuration fields (`default_model`, `small_model`) that
    /// entries of `models` name but that are not set; those entries are
    /// skipped. Sorted, without repeats.
    pub unset_models: Vec<&'static str>,
    pub runnable_as_root: bool,
    /// The runtimes that offer an ACP agent; `None` for the daemon's own
    /// agents.
    pub acp: Option<AcpOffers>,
}

/// An ACP agent on one runtime that declares it
/// (`docs/site/specs/acp-agents.md` section 3): what a session that
/// attaches that runtime runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeAgent {
    /// The agent's ID: the key of the runtime's declaration.
    pub id: AgentId,
    pub runtime: cookie_agent_protocol::RuntimeId,
    /// The agent's ID on the runtime.
    pub agent: AgentId,
    /// The runtime's description of it.
    pub description: String,
    pub enabled: bool,
    /// Whether its runtime is connected, or started on demand.
    pub connected: bool,
}

impl RuntimeAgent {
    /// Whether a session or a delegation can use it now.
    pub(crate) fn available(&self) -> bool {
        self.enabled && self.connected
    }
}

/// The runtimes that offer one ACP agent, in runtime ID order
/// (`docs/site/specs/acp-agents.md` section 15, naming of 2026-10-23).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcpOffers {
    pub id: AgentId,
    pub offers: Vec<RuntimeAgent>,
}

impl AcpOffers {
    /// The IDs of the runtimes that offer it.
    pub(crate) fn runtimes(&self) -> Vec<cookie_agent_protocol::RuntimeId> {
        self.offers
            .iter()
            .map(|offer| offer.runtime.clone())
            .collect()
    }

    /// Whether some runtime that offers it could run it now.
    pub(crate) fn any_available(&self) -> bool {
        self.offers.iter().any(RuntimeAgent::available)
    }

    /// The agent on the one runtime of `attached` that offers it. Refused,
    /// saying why: no attached runtime offers it, two or more do, or the
    /// one that does is away or has it disabled.
    pub(crate) fn place(
        &self,
        attached: &[cookie_agent_protocol::RuntimeId],
    ) -> Result<RuntimeAgent, String> {
        let offering = self
            .offers
            .iter()
            .filter(|offer| attached.contains(&offer.runtime))
            .collect::<Vec<_>>();
        match offering[..] {
            [] => Err(format!(
                "`{}` needs a runtime that offers it attached: {}",
                self.id,
                join_runtimes(&self.runtimes())
            )),
            [offer] if !offer.enabled => Err(format!(
                "ACP agent `{}` is disabled on runtime `{}`",
                self.id, offer.runtime
            )),
            [offer] if !offer.connected => Err(format!(
                "ACP agent `{}` is unavailable: runtime `{}` is not connected",
                self.id, offer.runtime
            )),
            [offer] => Ok(offer.clone()),
            _ => Err(format!(
                "`{}` is offered by more than one attached runtime ({}); a session runs it on \
                 one, so detach the others",
                self.id,
                join_runtimes(
                    &offering
                        .iter()
                        .map(|offer| offer.runtime.clone())
                        .collect::<Vec<_>>()
                )
            )),
        }
    }
}

/// `a, b, c`.
pub(crate) fn join_runtimes(runtimes: &[cookie_agent_protocol::RuntimeId]) -> String {
    runtimes
        .iter()
        .map(cookie_agent_protocol::RuntimeId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone, Debug)]
pub(crate) enum ResolvedAgentFallback {
    Selection {
        selection: ModelSelection,
        cache: Option<cookie_agent_config::CacheConfig>,
    },
    ParentModel {
        /// Replaces the variant of the owner run's model when set.
        variant: Option<cookie_agent_identity::ConfiguredVariantRef>,
        cache: Option<cookie_agent_config::CacheConfig>,
    },
}

/// The values of `${default_model}` and `${small_model}`: the
/// configuration's `default_model` and `small_model`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ModelVariables<'a> {
    pub default_model: Option<&'a ConfiguredModel>,
    pub small_model: Option<&'a ConfiguredModel>,
}

impl<'a> ModelVariables<'a> {
    pub(crate) fn of(config: &'a cookie_agent_config::EngineConfig) -> Self {
        Self {
            default_model: config.default_model.as_ref(),
            small_model: config.small_model.as_ref(),
        }
    }

    fn get(&self, model: &AgentModelRef) -> Option<&'a ConfiguredModel> {
        match model {
            AgentModelRef::DefaultModel => self.default_model,
            AgentModelRef::SmallModel => self.small_model,
            AgentModelRef::Model(_) | AgentModelRef::ParentModel => None,
        }
    }
}

/// The variant `variant` selects of `model`: its default variant when
/// absent.
fn resolve_variant(
    models: &CompiledModelRuntime,
    model: &ModelKey,
    variant: Option<&ConfiguredVariantRef>,
) -> Option<cookie_agent_identity::VariantId> {
    match variant {
        None => models
            .model(model)
            .and_then(|model| model.model.default_variant.clone()),
        Some(ConfiguredVariantRef::Base) => None,
        Some(ConfiguredVariantRef::Named(id)) => Some(id.clone()),
    }
}

/// The selection `${default_model}` or `${small_model}` resolves to, with
/// `variant` (an entry's override) in place of the configured one.
pub(crate) fn configured_selection(
    models: &CompiledModelRuntime,
    configured: &ConfiguredModel,
    variant: Option<&ConfiguredVariantRef>,
) -> ModelSelection {
    ModelSelection {
        model: configured.model.clone(),
        variant: resolve_variant(
            models,
            &configured.model,
            variant.or(configured.variant.as_ref()),
        ),
    }
}

#[derive(Clone, Debug)]
pub struct AgentRegistry {
    preset: Option<String>,
    agents: BTreeMap<AgentId, ResolvedAgent>,
    descriptors: Vec<AgentDescriptor>,
}

impl AgentRegistry {
    pub(crate) fn agents(&self) -> &BTreeMap<AgentId, ResolvedAgent> {
        &self.agents
    }

    pub(crate) fn resolve(
        authored: &ConfigAgentRegistry,
        models: &CompiledModelRuntime,
        preset: Option<String>,
        runtime_agents: &[RuntimeAgent],
        variables: ModelVariables<'_>,
    ) -> Result<Self, EngineError> {
        let mut documents = built_in_internal_documents()?;
        // An ACP agent's document is an agent only while a runtime declares
        // the agent.
        documents.extend(
            authored
                .documents()
                .iter()
                .filter(|(_, document)| document.frontmatter.acp.is_none())
                .map(|(id, document)| (id.clone(), document.clone())),
        );
        let mut agents = BTreeMap::new();
        // Every runtime that declares an ID offers the one agent of that ID.
        let mut offered = BTreeMap::<AgentId, Vec<RuntimeAgent>>::new();
        for runtime_agent in runtime_agents {
            offered
                .entry(runtime_agent.id.clone())
                .or_default()
                .push(runtime_agent.clone());
        }
        for (id, mut offers) in offered {
            if documents.contains_key(&id) {
                // A daemon agent of the same ID wins; the configuration
                // status names the ignored declarations.
                continue;
            }
            offers.sort_by(|left, right| left.runtime.cmp(&right.runtime));
            let offers = AcpOffers { id, offers };
            let document = match authored.get(&offers.id) {
                Some(document) => document.clone(),
                None => runtime_agent_default_document(&offers)?,
            };
            // Whether it can run depends on the session's runtimes: it is
            // a root choice while some runtime could run it.
            let runnable_as_root = offers.any_available()
                && document.frontmatter.enabled
                && matches!(
                    document.frontmatter.mode,
                    AgentMode::Primary | AgentMode::All
                );
            agents.insert(
                offers.id.clone(),
                ResolvedAgent {
                    document,
                    resolved_fallback: vec![ResolvedAgentFallback::Selection {
                        selection: cookie_agent_protocol::acp_model_selection(&offers.id),
                        cache: None,
                    }],
                    unset_models: Vec::new(),
                    runnable_as_root,
                    acp: Some(offers),
                },
            );
        }
        for document in documents.into_values() {
            let id = document.id.clone();
            if id.as_str() == BUILT_IN_DEFAULT_AGENT_ID {
                return Err(EngineError::RuntimeCompileFailed);
            }
            let mut resolved_fallback = Vec::new();
            let mut unset_models = Vec::new();
            for fallback in &document.frontmatter.models {
                match &fallback.model {
                    AgentModelRef::ParentModel => {
                        resolved_fallback.push(ResolvedAgentFallback::ParentModel {
                            variant: fallback.variant.clone(),
                            cache: fallback.cache.clone(),
                        });
                    }
                    AgentModelRef::Model(model_key) => {
                        resolved_fallback.push(ResolvedAgentFallback::Selection {
                            selection: ModelSelection {
                                model: model_key.clone(),
                                variant: resolve_variant(
                                    models,
                                    model_key,
                                    fallback.variant.as_ref(),
                                ),
                            },
                            cache: fallback.cache.clone(),
                        });
                    }
                    variable @ (AgentModelRef::DefaultModel | AgentModelRef::SmallModel) => {
                        match variables.get(variable) {
                            Some(configured) => {
                                resolved_fallback.push(ResolvedAgentFallback::Selection {
                                    selection: configured_selection(
                                        models,
                                        configured,
                                        fallback.variant.as_ref(),
                                    ),
                                    cache: fallback.cache.clone(),
                                });
                            }
                            // The entry is skipped; the agent reports the
                            // field to set.
                            None => unset_models.extend(variable.config_field()),
                        }
                    }
                }
            }
            unset_models.sort_unstable();
            unset_models.dedup();
            // A chain names each model once: an entry resolving to a model an
            // earlier one already did (`${default_model}` and the key it
            // names) adds nothing.
            let mut seen = std::collections::BTreeSet::new();
            resolved_fallback.retain(|fallback| match fallback {
                ResolvedAgentFallback::Selection { selection, .. } => {
                    seen.insert(selection.model.clone())
                }
                ResolvedAgentFallback::ParentModel { .. } => true,
            });
            let available = resolved_fallback
                .iter()
                .filter_map(|fallback| match fallback {
                    ResolvedAgentFallback::Selection { selection, .. } => Some(selection),
                    ResolvedAgentFallback::ParentModel { .. } => None,
                })
                .any(|selection| selection_available(models, selection));
            let runnable_as_root = document.frontmatter.enabled
                && matches!(
                    document.frontmatter.mode,
                    AgentMode::Primary | AgentMode::All
                )
                && available;
            agents.insert(
                id,
                ResolvedAgent {
                    document,
                    resolved_fallback,
                    unset_models,
                    runnable_as_root,
                    acp: None,
                },
            );
        }
        if !agents.values().any(|agent| agent.runnable_as_root) {
            // `${default_model}` first, then the first available model.
            let configured = variables
                .default_model
                .map(|configured| configured_selection(models, configured, None))
                .filter(|selection| selection_available(models, selection));
            let first = first_available_selection(models)
                .filter(|first| configured.as_ref().is_none_or(|c| c.model != first.model));
            let resolved_fallback = configured
                .iter()
                .chain(first.iter())
                .map(|selection| ResolvedAgentFallback::Selection {
                    selection: selection.clone(),
                    cache: None,
                })
                .collect::<Vec<_>>();
            if !resolved_fallback.is_empty() {
                let document = built_in_default_document(first.as_ref())?;
                agents.insert(
                    document.id.clone(),
                    ResolvedAgent {
                        document,
                        resolved_fallback,
                        unset_models: if variables.default_model.is_none() {
                            vec!["default_model"]
                        } else {
                            Vec::new()
                        },
                        runnable_as_root: true,
                        acp: None,
                    },
                );
            }
        }
        let descriptors = agents
            .iter()
            .map(|(id, agent)| AgentDescriptor {
                id: id.clone(),
                preset: preset.clone(),
                description: agent.document.frontmatter.description.clone(),
                mode: agent.document.frontmatter.mode,
                enabled: agent.document.frontmatter.enabled,
                runnable_as_root: agent.runnable_as_root,
                resolved_fallback: agent
                    .resolved_fallback
                    .iter()
                    .filter_map(|fallback| match fallback {
                        ResolvedAgentFallback::Selection { selection, .. } => {
                            Some(selection.clone())
                        }
                        ResolvedAgentFallback::ParentModel { .. } => None,
                    })
                    .collect(),
                delegation_targets: delegation_targets(
                    &agent.document.frontmatter.permissions,
                    &agents,
                ),
                unset_models: agent
                    .unset_models
                    .iter()
                    .map(|field| (*field).to_owned())
                    .collect(),
                runtime: agent.acp.as_ref().and_then(|acp| match &acp.offers[..] {
                    [offer] => Some(agent_runtime(offer)),
                    _ => None,
                }),
                runtimes: agent
                    .acp
                    .iter()
                    .flat_map(|acp| acp.offers.iter().map(agent_runtime))
                    .collect(),
            })
            .collect();
        Ok(Self {
            preset,
            agents,
            descriptors,
        })
    }

    pub(crate) fn get(&self, id: &AgentId) -> Option<&ResolvedAgent> {
        self.agents.get(id)
    }

    pub(crate) fn descriptors(&self) -> &[AgentDescriptor] {
        &self.descriptors
    }

    pub(crate) fn preset(&self) -> Option<&str> {
        self.preset.as_deref()
    }

    /// The delegation rules of its agents that reach no agent yet: a named
    /// target no agent has, and a pattern that matches no eligible agent.
    /// Either may be an ACP agent a runtime has not reported (or a typo);
    /// the configuration loads, and the diagnostic clears once a runtime
    /// reports a matching agent.
    pub(crate) fn delegation_diagnostics(&self) -> Vec<String> {
        let mut diagnostics = Vec::new();
        for (id, agent) in &self.agents {
            let Some(PermissionValue::Resources(resources)) =
                agent.document.frontmatter.permissions.get(
                    &cookie_agent_config::PermissionKey::any(PermissionAction::DELEGATE),
                )
            else {
                continue;
            };
            for (resource, effect) in resources {
                if *effect == PermissionEffect::Deny {
                    continue;
                }
                let pattern = resource.as_str();
                if pattern.contains('*') || pattern.contains('?') {
                    let single = IndexMap::from([(
                        cookie_agent_config::PermissionKey::any(PermissionAction::DELEGATE),
                        PermissionValue::Resources(IndexMap::from([(resource.clone(), *effect)])),
                    )]);
                    if delegation_targets(&single, &self.agents).is_empty() {
                        diagnostics.push(format!(
                            "agent `{id}` delegates to `{pattern}`, which matches no agent yet: \
                             no enabled subagent document matches it and no connected runtime \
                             reports an agent it matches"
                        ));
                    }
                } else if AgentId::new(pattern)
                    .is_ok_and(|target| !self.agents.contains_key(&target))
                {
                    diagnostics.push(format!(
                        "agent `{id}` delegates to `{pattern}`, which is not known yet: no \
                         agent document has that ID and no connected runtime reports it"
                    ));
                }
            }
        }
        diagnostics
    }
}

fn first_available_selection(models: &CompiledModelRuntime) -> Option<ModelSelection> {
    models.models().values().find_map(|model| {
        (model.model.status == CompiledModelStatus::Available).then(|| ModelSelection {
            model: model.key.clone(),
            variant: model.model.default_variant.clone(),
        })
    })
}

/// The built-in internal agent documents.
///
/// Each declares `max_output_tokens: 0`, so every built-in internal agent
/// inherits the owner run's document cap at execution time.
fn built_in_internal_documents() -> Result<BTreeMap<AgentId, AgentDocument>, EngineError> {
    // Approval and titles run on the small model when one is set, then on
    // the run's own; compaction stays on the run's model, whose prompt
    // cache the session keeps warm.
    let small_then_parent = [AgentModelRef::SmallModel, AgentModelRef::ParentModel];
    [
        (
            BUILT_IN_APPROVAL_AGENT_ID,
            "Built-in approval evaluator",
            &small_then_parent[..],
            AgentLimits {
                timeout_ms: 30_000,
                max_output_tokens: 0,
            },
        ),
        (
            BUILT_IN_COMPACTION_AGENT_ID,
            "Built-in context compaction agent",
            &[AgentModelRef::ParentModel][..],
            AgentLimits {
                timeout_ms: 180_000,
                max_output_tokens: 0,
            },
        ),
        (
            BUILT_IN_TITLE_AGENT_ID,
            "Built-in session title agent",
            &small_then_parent[..],
            AgentLimits {
                timeout_ms: 30_000,
                max_output_tokens: 0,
            },
        ),
    ]
    .into_iter()
    .map(|(id, description, models, limits)| {
        let document =
            built_in_internal_document(id, description, "{{default_prompt}}\n", models, limits)?;
        Ok((document.id.clone(), document))
    })
    .collect()
}

fn built_in_internal_document(
    id: &str,
    description: &str,
    body: &str,
    models: &[AgentModelRef],
    limits: AgentLimits,
) -> Result<AgentDocument, EngineError> {
    let id = IdentityAgentId::new(id).map_err(|_| EngineError::RuntimeCompileFailed)?;
    let body = body.to_owned();
    let frontmatter = AgentFrontmatter {
        description: description.to_owned(),
        mode: AgentMode::Internal,
        enabled: true,
        models: models
            .iter()
            .map(|model| AgentModelFallback {
                model: model.clone(),
                variant: None,
                cache: None,
            })
            .collect(),
        limits,
        permissions: IndexMap::new(),
        agent_md: None,
        acp: None,
    };
    let document_fingerprint = fingerprint(
        "cookie-agent/built-in-internal-agent-document/v2",
        &(id.as_str(), &frontmatter, &body),
    )?;
    let prompt_fingerprint = fingerprint("cookie-agent/system-prompt/v1", &body)?;
    Ok(AgentDocument {
        id,
        frontmatter,
        body,
        source: AgentDocumentSource::BuiltIn,
        document_fingerprint,
        prompt_fingerprint,
    })
}

fn agent_runtime(offer: &RuntimeAgent) -> cookie_agent_protocol::AgentRuntime {
    cookie_agent_protocol::AgentRuntime {
        runtime_id: offer.runtime.clone(),
        agent: offer.agent.clone(),
        available: offer.available(),
    }
}

/// The document of an ACP agent the daemon has none for
/// (`docs/site/specs/acp-agents.md` section 4): mode `all`, the first
/// offering runtime's description, no rules of its own, and an empty body.
fn runtime_agent_default_document(agent: &AcpOffers) -> Result<AgentDocument, EngineError> {
    let description = agent
        .offers
        .iter()
        .map(|offer| offer.description.trim())
        .find(|description| !description.is_empty())
        .map_or_else(
            || {
                format!(
                    "ACP agent `{}` on {}",
                    agent.id,
                    join_runtimes(&agent.runtimes())
                )
            },
            str::to_owned,
        );
    let frontmatter = AgentFrontmatter {
        description,
        mode: AgentMode::All,
        enabled: true,
        models: Vec::new(),
        limits: AgentLimits::default(),
        permissions: IndexMap::new(),
        agent_md: None,
        acp: Some(cookie_agent_config::AcpDocumentSettings::default()),
    };
    let body = String::new();
    let document_fingerprint = fingerprint(
        "cookie-agent/runtime-agent-document/v1",
        &(agent.id.as_str(), &frontmatter, &body),
    )?;
    let prompt_fingerprint = fingerprint("cookie-agent/system-prompt/v1", &body)?;
    Ok(AgentDocument {
        id: agent.id.clone(),
        frontmatter,
        body,
        source: AgentDocumentSource::BuiltIn,
        document_fingerprint,
        prompt_fingerprint,
    })
}

/// The built-in `default` agent: `${default_model}`, then `first`, the
/// first available model, when it is another model.
fn built_in_default_document(first: Option<&ModelSelection>) -> Result<AgentDocument, EngineError> {
    let id = IdentityAgentId::new(BUILT_IN_DEFAULT_AGENT_ID)
        .map_err(|_| EngineError::RuntimeCompileFailed)?;
    let body = "You are Cookie Agent's built-in default coding agent. Help the user inspect, understand, and modify software safely and precisely.\n".to_owned();
    let models = std::iter::once(AgentModelFallback {
        model: AgentModelRef::DefaultModel,
        variant: None,
        cache: None,
    })
    .chain(first.map(|selection| AgentModelFallback {
        model: AgentModelRef::Model(selection.model.clone()),
        variant: selection.variant.clone().map(ConfiguredVariantRef::Named),
        cache: None,
    }))
    .collect();
    let frontmatter = AgentFrontmatter {
        description: "Built-in default coding agent".to_owned(),
        mode: AgentMode::Primary,
        enabled: true,
        models,
        limits: AgentLimits::default(),
        permissions: built_in_default_permissions()?,
        agent_md: None,
        acp: None,
    };
    let document_fingerprint = fingerprint(
        "cookie-agent/built-in-default-document/v3",
        &(id.as_str(), &frontmatter, &body),
    )?;
    let prompt_fingerprint = fingerprint("cookie-agent/system-prompt/v1", &body)?;
    Ok(AgentDocument {
        id,
        frontmatter,
        body,
        source: AgentDocumentSource::BuiltIn,
        document_fingerprint,
        prompt_fingerprint,
    })
}

fn fingerprint(
    domain: &str,
    value: &impl Serialize,
) -> Result<cookie_agent_models::Sha256Digest, EngineError> {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(value).map_err(|_| EngineError::RuntimeCompileFailed)?);
    cookie_agent_models::Sha256Digest::new(format!("{:x}", hasher.finalize()))
        .map_err(|_| EngineError::RuntimeCompileFailed)
}

fn built_in_default_permissions()
-> Result<IndexMap<cookie_agent_config::PermissionKey, PermissionValue>, EngineError> {
    let definitions = [
        (
            PermissionAction::READ,
            "tool_result:*",
            PermissionEffect::Allow,
        ),
        (PermissionAction::READ, "*?*", PermissionEffect::Allow),
        (PermissionAction::WRITE, "*", PermissionEffect::Ask),
        (PermissionAction::BASH, "*", PermissionEffect::Ask),
        (PermissionAction::DELEGATE, "*", PermissionEffect::Ask),
        (PermissionAction::READ, "/*", PermissionEffect::Ask),
        (PermissionAction::READ, ".env", PermissionEffect::Deny),
        (PermissionAction::READ, "*/.env", PermissionEffect::Deny),
        (PermissionAction::READ, ".env.*", PermissionEffect::Deny),
        (PermissionAction::READ, "*/.env.*", PermissionEffect::Deny),
        (
            PermissionAction::READ,
            ".env.example",
            PermissionEffect::Allow,
        ),
        (
            PermissionAction::READ,
            "*/.env.example",
            PermissionEffect::Allow,
        ),
        (
            PermissionAction::READ,
            "store-v3.json",
            PermissionEffect::Deny,
        ),
        (
            PermissionAction::READ,
            "*/store-v3.json",
            PermissionEffect::Deny,
        ),
        (PermissionAction::READ, "id_*", PermissionEffect::Deny),
        (PermissionAction::READ, "*/id_*", PermissionEffect::Deny),
        (PermissionAction::READ, ".netrc", PermissionEffect::Deny),
        (PermissionAction::READ, "*/.netrc", PermissionEffect::Deny),
        (
            PermissionAction::READ,
            "application_default_credentials.json",
            PermissionEffect::Deny,
        ),
        (
            PermissionAction::READ,
            "*/application_default_credentials.json",
            PermissionEffect::Deny,
        ),
    ];
    let mut permissions = IndexMap::new();
    for (action, resource, effect) in definitions {
        let entry = permissions
            .entry(action.into())
            .or_insert_with(|| PermissionValue::Resources(IndexMap::new()));
        let PermissionValue::Resources(resources) = entry else {
            unreachable!("synthetic permissions use map form")
        };
        resources.insert(
            WildcardPattern::new(resource).map_err(|_| EngineError::RuntimeCompileFailed)?,
            effect,
        );
    }
    Ok(permissions)
}

pub(crate) fn delegation_targets(
    permissions: &IndexMap<cookie_agent_config::PermissionKey, PermissionValue>,
    known_agents: &BTreeMap<AgentId, ResolvedAgent>,
) -> Vec<AgentId> {
    let eligible_matches = |pattern: &str| -> Vec<AgentId> {
        known_agents
            .iter()
            .filter(move |(id, agent)| {
                // Only eligible subagents become targets; runtime
                // spawn enforces the same eligibility again.
                agent.document.frontmatter.enabled
                    && matches!(
                        agent.document.frontmatter.mode,
                        cookie_agent_protocol::AgentMode::Subagent
                            | cookie_agent_protocol::AgentMode::All
                    )
                    && cookie_agent_config::simple_wildcard_match(pattern, id.as_str())
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    let mut targets = Vec::new();
    match permissions.get(&cookie_agent_config::PermissionKey::any(
        PermissionAction::DELEGATE,
    )) {
        // Scalar effects are documented as equivalent to a `"*"` mapping:
        // expand them over every eligible subagent like any wildcard rule.
        Some(PermissionValue::Effect(effect)) if *effect != PermissionEffect::Deny => {
            targets.extend(eligible_matches("*"));
        }
        Some(PermissionValue::Resources(resources)) => {
            for (resource, effect) in resources {
                if *effect == PermissionEffect::Deny {
                    continue;
                }
                let pattern = resource.as_str();
                if pattern.contains('*') || pattern.contains('?') {
                    // Wildcard delegation uses the same matching as every
                    // other permission: expand against the known agent set.
                    targets.extend(eligible_matches(pattern));
                } else if let Ok(target) = AgentId::new(pattern)
                    && known_agents.contains_key(&target)
                {
                    // A name no agent has yet (an ACP agent whose runtime
                    // has not reported it) becomes a target once one does;
                    // until then it is a configuration diagnostic.
                    targets.push(target);
                }
            }
        }
        _ => {}
    }
    targets.sort();
    targets.dedup();
    targets
}

pub(crate) fn selection_available(
    models: &CompiledModelRuntime,
    selection: &ModelSelection,
) -> bool {
    models.model(&selection.model).is_some_and(|model| {
        model.model.status == CompiledModelStatus::Available
            && selection
                .variant
                .as_ref()
                .is_none_or(|variant| model.model.variants.contains_key(variant))
    })
}

#[cfg(test)]
mod tests {
    use cookie_agent_config::{PermissionAction, PermissionEffect, PermissionValue};
    use cookie_agent_identity::WildcardPattern;
    use indexmap::IndexMap;

    use std::collections::BTreeMap;

    use super::{
        ResolvedAgent, built_in_default_permissions, built_in_internal_documents,
        delegation_targets, fingerprint,
    };
    use cookie_agent_config::{
        AgentDocument, AgentDocumentSource, AgentFrontmatter, AgentLimits, AgentMode,
        AgentModelFallback, AgentModelRef, BUILT_IN_APPROVAL_AGENT_ID,
        BUILT_IN_COMPACTION_AGENT_ID, BUILT_IN_TITLE_AGENT_ID,
    };
    use cookie_agent_identity::AgentId as IdentityAgentId;

    #[test]
    fn delegation_targets_require_named_non_deny_permissions() {
        let known = BTreeMap::new();
        assert!(delegation_targets(&IndexMap::new(), &known).is_empty());

        let mut resources = IndexMap::new();
        resources.insert(WildcardPattern::new("*").unwrap(), PermissionEffect::Deny);
        resources.insert(
            WildcardPattern::new("reviewer").unwrap(),
            PermissionEffect::Ask,
        );
        let permissions = IndexMap::from([(
            cookie_agent_config::PermissionKey::any(PermissionAction::DELEGATE),
            PermissionValue::Resources(resources),
        )]);
        // A named target is one only once an agent has the name.
        assert!(delegation_targets(&permissions, &known).is_empty());
        let reviewer = cookie_agent_protocol::AgentId::new("reviewer").unwrap();
        let known = BTreeMap::from([(
            reviewer.clone(),
            ResolvedAgent {
                document: built_in_internal_documents()
                    .expect("built-in internal documents")
                    .into_values()
                    .next()
                    .expect("a document"),
                resolved_fallback: Vec::new(),
                unset_models: Vec::new(),
                runnable_as_root: false,
                acp: None,
            },
        )]);
        assert_eq!(delegation_targets(&permissions, &known), [reviewer]);
    }

    #[test]
    fn built_in_agents_ship_without_message_rules() {
        let default_permissions =
            built_in_default_permissions().expect("built-in default permissions");
        assert!(
            default_permissions
                .get(&PermissionAction::MESSAGE)
                .is_none(),
            "built-in default agent must not grant message authority"
        );
        for document in built_in_internal_documents()
            .expect("built-in internal documents")
            .values()
        {
            assert!(
                document
                    .frontmatter
                    .permissions
                    .get(&PermissionAction::MESSAGE)
                    .is_none(),
                "internal agent `{}` must not grant message authority",
                document.id
            );
        }
    }

    #[test]
    fn built_in_and_authored_internal_documents_declare_no_output_cap() {
        let authored: AgentFrontmatter = serde_json::from_value(serde_json::json!({
            "description": "Authored internal",
            "mode": "internal",
            "enabled": true,
            "models": [{ "model": "${parent_model}" }]
        }))
        .expect("authored internal frontmatter");
        // Zero means "inherit the owner run's cap", not "unbounded".
        assert_eq!(authored.limits.max_output_tokens, 0);

        let documents = built_in_internal_documents().expect("built-in internal documents");
        assert_eq!(documents.len(), 3);
        for document in documents.values() {
            assert_eq!(
                document.frontmatter.limits.max_output_tokens, 0,
                "built-in internal agent `{}` must inherit the owner run's output cap",
                document.id
            );
        }

        let explicit: AgentFrontmatter = serde_json::from_value(serde_json::json!({
            "description": "Authored internal",
            "mode": "internal",
            "enabled": true,
            "models": [{ "model": "${parent_model}" }],
            "limits": { "max_output_tokens": 64 }
        }))
        .expect("authored internal frontmatter");
        assert_eq!(explicit.limits.max_output_tokens, 64);
    }

    #[test]
    fn built_in_internal_documents_declare_their_timeouts() {
        let documents = built_in_internal_documents().expect("built-in internal documents");
        let timeout = |id: &str| {
            documents
                .values()
                .find(|document| document.id.as_str() == id)
                .unwrap_or_else(|| panic!("built-in document `{id}`"))
                .frontmatter
                .limits
                .timeout_ms
        };
        assert_eq!(timeout(BUILT_IN_APPROVAL_AGENT_ID), 30_000);
        assert_eq!(timeout(BUILT_IN_COMPACTION_AGENT_ID), 180_000);
        assert_eq!(timeout(BUILT_IN_TITLE_AGENT_ID), 30_000);
    }

    fn test_agent(id: &str, mode: AgentMode, enabled: bool) -> ResolvedAgent {
        let id = IdentityAgentId::new(id).expect("agent ID");
        let body = "Test agent.\n".to_owned();
        let frontmatter = AgentFrontmatter {
            description: "Test agent".to_owned(),
            mode,
            enabled,
            models: vec![AgentModelFallback {
                model: AgentModelRef::ParentModel,
                variant: None,
                cache: None,
            }],
            limits: AgentLimits {
                timeout_ms: 0,
                max_output_tokens: 1_024,
            },
            permissions: IndexMap::new(),
            agent_md: None,
            acp: None,
        };
        let document_fingerprint = fingerprint(
            "cookie-agent/test-agent-document/v1",
            &(id.as_str(), &frontmatter, &body),
        )
        .expect("fingerprint");
        let prompt_fingerprint =
            fingerprint("cookie-agent/system-prompt/v1", &body).expect("fingerprint");
        ResolvedAgent {
            document: AgentDocument {
                id,
                frontmatter,
                body,
                source: AgentDocumentSource::BuiltIn,
                document_fingerprint,
                prompt_fingerprint,
            },
            resolved_fallback: Vec::new(),
            unset_models: Vec::new(),
            runnable_as_root: false,
            acp: None,
        }
    }

    #[test]
    fn wildcard_delegation_expands_to_matching_eligible_subagents() {
        let mut known = BTreeMap::new();
        for (id, mode, enabled) in [
            ("sub-worker", AgentMode::Subagent, true),
            ("sub-disabled", AgentMode::Subagent, false),
            ("sub-primary", AgentMode::Primary, true),
            ("explore", AgentMode::Subagent, true),
        ] {
            let agent = test_agent(id, mode, enabled);
            known.insert(agent.document.id.clone(), agent);
        }
        let mut resources = IndexMap::new();
        resources.insert(
            WildcardPattern::new("sub-*").unwrap(),
            PermissionEffect::Allow,
        );
        resources.insert(
            WildcardPattern::new("explore").unwrap(),
            PermissionEffect::Allow,
        );
        let permissions = IndexMap::from([(
            cookie_agent_config::PermissionKey::any(PermissionAction::DELEGATE),
            PermissionValue::Resources(resources),
        )]);
        // Disabled and non-subagent matches are excluded; concrete and
        // wildcard targets merge and dedupe.
        assert_eq!(
            delegation_targets(&permissions, &known),
            [
                cookie_agent_protocol::AgentId::new("explore").unwrap(),
                cookie_agent_protocol::AgentId::new("sub-worker").unwrap(),
            ]
        );

        // A scalar `delegate: allow` is equivalent to a `"*"` mapping and
        // expands over every eligible subagent.
        let scalar = IndexMap::from([(
            cookie_agent_config::PermissionKey::any(PermissionAction::DELEGATE),
            PermissionValue::Effect(PermissionEffect::Allow),
        )]);
        assert_eq!(
            delegation_targets(&scalar, &known),
            [
                cookie_agent_protocol::AgentId::new("explore").unwrap(),
                cookie_agent_protocol::AgentId::new("sub-worker").unwrap(),
            ]
        );
        let scalar_deny = IndexMap::from([(
            cookie_agent_config::PermissionKey::any(PermissionAction::DELEGATE),
            PermissionValue::Effect(PermissionEffect::Deny),
        )]);
        assert!(delegation_targets(&scalar_deny, &known).is_empty());
    }
}

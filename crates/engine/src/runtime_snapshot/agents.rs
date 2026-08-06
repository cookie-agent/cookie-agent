use std::collections::BTreeMap;

use cookie_agent_config::{
    AgentDocument, AgentDocumentSource, AgentFrontmatter, AgentMode as ConfigAgentMode,
    AgentModelFallback, AgentRegistry as ConfigAgentRegistry, AgentSchemaVersion,
    BUILT_IN_DEFAULT_AGENT_ID, PermissionAction, PermissionEffect, PermissionRule,
    ToolName as ConfigToolName,
};
use cookie_agent_identity::{AgentId as IdentityAgentId, SafeCode, WildcardPattern};
use cookie_agent_models::{CompiledModelRuntime, compiler::CompiledModelStatus};
use cookie_agent_protocol::{AgentDescriptor, AgentId, AgentMode, ModelSelection, ToolName};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::EngineError;

#[derive(Clone, Debug)]
pub(crate) struct ResolvedAgent {
    pub document: AgentDocument,
    pub resolved_fallback: Vec<ModelSelection>,
    pub runnable_as_root: bool,
}

#[derive(Clone, Debug)]
pub struct AgentRegistry {
    agents: BTreeMap<AgentId, ResolvedAgent>,
    descriptors: Vec<AgentDescriptor>,
}

impl AgentRegistry {
    pub(crate) fn resolve(
        authored: &ConfigAgentRegistry,
        models: &CompiledModelRuntime,
    ) -> Result<Self, EngineError> {
        let mut agents = BTreeMap::new();
        for input in authored.materialization_inputs() {
            let id = input.document.id.clone();
            if id.as_str() == BUILT_IN_DEFAULT_AGENT_ID {
                return Err(EngineError::RuntimeCompileFailed);
            }
            let document = input.document.clone();
            let mut resolved_fallback = Vec::new();
            for fallback in &document.frontmatter.model_fallback {
                let variant = match (&fallback.variant, models.model(&fallback.model)) {
                    (None, Some(model)) => model.model.default_variant.clone(),
                    (Some(cookie_agent_identity::ConfiguredVariantRef::Base), _) => None,
                    (Some(cookie_agent_identity::ConfiguredVariantRef::Named(id)), _) => {
                        Some(id.clone())
                    }
                    (None, None) => None,
                };
                resolved_fallback.push(ModelSelection {
                    model: fallback.model.clone(),
                    variant,
                });
            }
            let available = resolved_fallback
                .iter()
                .any(|selection| selection_available(models, selection));
            let runnable_as_root = input.root_eligible && available;
            agents.insert(
                id,
                ResolvedAgent {
                    document,
                    resolved_fallback,
                    runnable_as_root,
                },
            );
        }
        if !agents.values().any(|agent| agent.runnable_as_root)
            && let Some(selection) = first_available_selection(models)
        {
            let document = built_in_default_document(&selection)?;
            agents.insert(
                document.id.clone(),
                ResolvedAgent {
                    document,
                    resolved_fallback: vec![selection],
                    runnable_as_root: true,
                },
            );
        }
        let descriptors = agents
            .iter()
            .map(|(id, agent)| AgentDescriptor {
                id: id.clone(),
                description: agent.document.frontmatter.description.clone(),
                mode: wire_mode(agent.document.frontmatter.mode),
                enabled: agent.document.frontmatter.enabled,
                runnable_as_root: agent.runnable_as_root,
                resolved_fallback: agent.resolved_fallback.clone(),
                tools: agent
                    .document
                    .frontmatter
                    .tools
                    .iter()
                    .copied()
                    .map(wire_tool)
                    .collect(),
                delegation_targets: agent.document.frontmatter.delegation.as_ref().map_or_else(
                    Vec::new,
                    |delegation| {
                        let mut targets = delegation.agents.clone();
                        targets.sort();
                        targets
                    },
                ),
            })
            .collect();
        Ok(Self {
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
}

fn first_available_selection(models: &CompiledModelRuntime) -> Option<ModelSelection> {
    models.models().values().find_map(|model| {
        (model.model.status == CompiledModelStatus::Available).then(|| ModelSelection {
            model: model.key.clone(),
            variant: model.model.default_variant.clone(),
        })
    })
}

fn built_in_default_document(selection: &ModelSelection) -> Result<AgentDocument, EngineError> {
    let id = IdentityAgentId::new(BUILT_IN_DEFAULT_AGENT_ID)
        .map_err(|_| EngineError::RuntimeCompileFailed)?;
    let body = "You are Cookie Agent's built-in default coding agent. Help the user inspect, understand, and modify software safely and precisely.\n".to_owned();
    let frontmatter = AgentFrontmatter {
        schema: AgentSchemaVersion,
        description: "Built-in default coding agent".to_owned(),
        mode: ConfigAgentMode::Primary,
        enabled: true,
        model_fallback: vec![AgentModelFallback {
            model: selection.model.clone(),
            variant: selection
                .variant
                .clone()
                .map(cookie_agent_identity::ConfiguredVariantRef::Named),
        }],
        tools: vec![
            ConfigToolName::Read,
            ConfigToolName::Grep,
            ConfigToolName::Glob,
            ConfigToolName::Write,
            ConfigToolName::Edit,
            ConfigToolName::Bash,
        ],
        permissions: built_in_default_permissions()?,
        delegation: None,
    };
    let document_fingerprint = fingerprint("cookie-agent/built-in-default-document/v1", selection)?;
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

fn built_in_default_permissions() -> Result<Vec<PermissionRule>, EngineError> {
    let definitions = [
        (
            "allow-workspace-read",
            PermissionAction::Read,
            "*",
            PermissionEffect::Allow,
        ),
        (
            "allow-workspace-search",
            PermissionAction::Grep,
            "*",
            PermissionEffect::Allow,
        ),
        (
            "allow-workspace-glob",
            PermissionAction::Glob,
            "*",
            PermissionEffect::Allow,
        ),
        (
            "ask-write",
            PermissionAction::Write,
            "*",
            PermissionEffect::Ask,
        ),
        (
            "ask-bash",
            PermissionAction::Bash,
            "*",
            PermissionEffect::Ask,
        ),
        (
            "ask-delegate",
            PermissionAction::Delegate,
            "*",
            PermissionEffect::Ask,
        ),
        (
            "ask-external-directory",
            PermissionAction::ExternalDirectory,
            "*",
            PermissionEffect::Ask,
        ),
        (
            "deny-read-root-dotenv",
            PermissionAction::Read,
            ".env",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-nested-dotenv",
            PermissionAction::Read,
            "*/.env",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-root-dotenv-variants",
            PermissionAction::Read,
            ".env.*",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-nested-dotenv-variants",
            PermissionAction::Read,
            "*/.env.*",
            PermissionEffect::Deny,
        ),
        (
            "allow-read-root-dotenv-example",
            PermissionAction::Read,
            ".env.example",
            PermissionEffect::Allow,
        ),
        (
            "allow-read-nested-dotenv-example",
            PermissionAction::Read,
            "*/.env.example",
            PermissionEffect::Allow,
        ),
        (
            "deny-read-root-credential-store",
            PermissionAction::Read,
            "store-v3.json",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-nested-credential-store",
            PermissionAction::Read,
            "*/store-v3.json",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-root-daemon-token",
            PermissionAction::Read,
            "token-v1",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-nested-daemon-token",
            PermissionAction::Read,
            "*/token-v1",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-root-private-keys",
            PermissionAction::Read,
            "id_*",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-nested-private-keys",
            PermissionAction::Read,
            "*/id_*",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-root-netrc",
            PermissionAction::Read,
            ".netrc",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-nested-netrc",
            PermissionAction::Read,
            "*/.netrc",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-root-cloud-credentials",
            PermissionAction::Read,
            "application_default_credentials.json",
            PermissionEffect::Deny,
        ),
        (
            "deny-read-nested-cloud-credentials",
            PermissionAction::Read,
            "*/application_default_credentials.json",
            PermissionEffect::Deny,
        ),
    ];
    definitions
        .into_iter()
        .map(|(id, action, resource, effect)| {
            Ok(PermissionRule {
                id: SafeCode::new(id).map_err(|_| EngineError::RuntimeCompileFailed)?,
                action,
                resource: WildcardPattern::new(resource)
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                effect,
            })
        })
        .collect()
}

fn selection_available(models: &CompiledModelRuntime, selection: &ModelSelection) -> bool {
    models.model(&selection.model).is_some_and(|model| {
        model.model.status == CompiledModelStatus::Available
            && selection
                .variant
                .as_ref()
                .is_none_or(|variant| model.model.variants.contains_key(variant))
    })
}

fn wire_mode(mode: ConfigAgentMode) -> AgentMode {
    match mode {
        ConfigAgentMode::Primary => AgentMode::Primary,
        ConfigAgentMode::Subagent => AgentMode::Subagent,
        ConfigAgentMode::All => AgentMode::All,
    }
}

fn wire_tool(tool: cookie_agent_config::ToolName) -> ToolName {
    match tool {
        cookie_agent_config::ToolName::Read => ToolName::Read,
        cookie_agent_config::ToolName::Write => ToolName::Write,
        cookie_agent_config::ToolName::Edit => ToolName::Edit,
        cookie_agent_config::ToolName::Bash => ToolName::Bash,
        cookie_agent_config::ToolName::Grep => ToolName::Grep,
        cookie_agent_config::ToolName::Glob => ToolName::Glob,
    }
}

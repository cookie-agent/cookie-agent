use std::collections::BTreeMap;

use cookie_agent_config::{
    AgentDocument, AgentMode as ConfigAgentMode, AgentRegistry as ConfigAgentRegistry,
};
use cookie_agent_models::{CompiledModelRuntime, compiler::CompiledModelStatus};
use cookie_agent_protocol::{AgentDescriptor, AgentId, AgentMode, ModelSelection, ToolName};

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

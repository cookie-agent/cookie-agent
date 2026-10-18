//! The base layer: tools the daemon implements for its own orchestration
//! (delegation, messaging, skills, goals, `read_artifact`,
//! `stop_background`). The engine
//! registers them itself when it opens; they have no workspace.

use std::sync::Arc;

use cookie_agent_runtime_api::authoring::{
    parse_args, prepared_operation, prepared_resource, safe_title, schema,
};

use crate::{Engine, EngineError, ToolLayer, ToolProvider, ToolSpec};

mod delegate;
mod goal;
mod message;
mod read_artifact;
mod skill;
mod stop_background;

use delegate::DelegateToolProvider;
use goal::GoalTools;
use message::MessageToolProvider;
use read_artifact::ReadArtifactTool;
use skill::SkillTool;
use stop_background::StopBackgroundTool;
pub(crate) use stop_background::TOOL_NAME as STOP_BACKGROUND_TOOL;

fn tool_error(error: impl std::fmt::Display) -> crate::ToolError {
    crate::ToolError::execution(error.to_string())
}

impl Engine {
    /// Registers the base layer's providers, in composition order.
    pub(crate) fn register_base_layer(&self) -> Result<(), EngineError> {
        let providers: [Arc<dyn ToolProvider>; 6] = [
            Arc::new(DelegateToolProvider::new(self.clone())),
            Arc::new(MessageToolProvider::new(self.clone())),
            Arc::new(SkillTool::new(self.clone())),
            Arc::new(ReadArtifactTool),
            Arc::new(GoalTools::new(self.clone())),
            Arc::new(StopBackgroundTool::new(self.clone())),
        ];
        for provider in providers {
            self.try_register_tool_provider(ToolLayer::Base, provider)?;
        }
        Ok(())
    }
}

/// Names of every base-layer tool. A runtime tool never takes one: it is
/// published as `<attachment>_<name>` instead, even while the base tool is
/// not offered.
pub(crate) fn reserved_tool_names() -> &'static std::collections::HashSet<String> {
    static NAMES: std::sync::OnceLock<std::collections::HashSet<String>> =
        std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        base_layer_tool_specs("reserved")
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    })
}

/// Every base-layer tool as the engine publishes it, for the tool reference.
/// `delegate_subagent` lists `delegate_target` as its only subagent.
#[must_use]
pub fn base_layer_tool_specs(delegate_target: &str) -> Vec<ToolSpec> {
    let mut specs = vec![SkillTool::spec()];
    specs.extend(DelegateToolProvider::specs(&[delegate_target]));
    specs.push(MessageToolProvider::spec());
    specs.push(GoalTools::get_spec());
    specs.push(GoalTools::update_spec());
    specs.push(ReadArtifactTool::spec());
    specs.push(StopBackgroundTool::spec());
    specs
}

#[cfg(test)]
fn test_turn_context() -> Arc<crate::TurnAgentContext> {
    Arc::new(crate::TurnAgentContext {
        agent: cookie_agent_protocol::AgentId::new("test").expect("test agent ID"),
        model: "test/model".parse().expect("test model key"),
        adapter: cookie_agent_protocol::AdaptorId::OPENAI_CHAT,
        adapter_family: cookie_agent_runtime_api::AdapterFamily::OpenaiChat,
        capabilities: cookie_agent_protocol::ModelCapabilities {
            input: std::collections::BTreeSet::from([cookie_agent_protocol::Modality::Text]),
            output: std::collections::BTreeSet::from([cookie_agent_protocol::Modality::Text]),
            context_tokens: 8_192,
            output_tokens: 2_048,
            tool_calling: true,
            parallel_tool_calls: true,
            structured_output: false,
            reasoning: false,
            temperature: true,
            top_p: true,
            seed: false,
            native_replay: cookie_agent_protocol::ReplayCapability::Optional,
            cancellation: cookie_agent_protocol::CancellationCapability::LocalOnly,
            media: std::collections::BTreeMap::new(),
        },
    })
}

#[cfg(test)]
mod tests {
    use crate::{ToolProvider, ToolResultTruncationPolicy};

    use super::{DelegateToolProvider, GoalTools, SkillTool};

    #[test]
    fn static_permission_names_cover_every_base_tool() {
        assert_eq!(
            SkillTool::get_permission_name("skill").unwrap(),
            Some("skill")
        );
        assert_eq!(
            GoalTools::get_permission_name("goal_get").unwrap(),
            Some("read")
        );
        assert_eq!(
            GoalTools::get_permission_name("goal_update").unwrap(),
            Some("write")
        );
        for name in ["delegate_subagent", "get_subagent_result"] {
            assert_eq!(
                DelegateToolProvider::get_permission_name(name).unwrap(),
                Some("delegate")
            );
        }
    }

    #[test]
    fn subagent_results_declare_truncation_opt_out() {
        assert_eq!(
            super::delegate::result_truncation_policy("get_subagent_result"),
            ToolResultTruncationPolicy::OptOut
        );
    }
}

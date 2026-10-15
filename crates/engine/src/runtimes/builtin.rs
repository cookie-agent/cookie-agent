//! The built-in `global` and `local` runtime templates.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use cookie_agent_config::SkillDocument;
use cookie_agent_runtime_api::{
    CodingToolsFactory, GLOBAL_RUNTIME_ID, LOCAL_RUNTIME_ID, RuntimeInfo, ToolProvider,
    WorkspaceContext,
};

use super::registry::{RuntimeTemplate, StartedRuntime};

/// The daemon's global runtime: top-level MCP servers, plugins, the
/// configured global tools, and user skills. It starts with the engine and is
/// never restarted.
pub(crate) struct GlobalTemplate;

#[async_trait]
impl RuntimeTemplate for GlobalTemplate {
    fn runtime_id(&self) -> &str {
        GLOBAL_RUNTIME_ID
    }

    fn takes_workspace(&self) -> bool {
        false
    }

    async fn start(&self, _workspace: Option<&Path>) -> Result<StartedRuntime, String> {
        Err("the global runtime starts with the daemon".into())
    }
}

impl GlobalTemplate {
    pub(crate) fn started(
        providers: Vec<Arc<dyn ToolProvider>>,
        skills: Vec<SkillDocument>,
    ) -> StartedRuntime {
        StartedRuntime {
            info: RuntimeInfo::global(),
            providers,
            skills,
        }
    }
}

/// The built-in coding runtime for the daemon's working directory.
pub(crate) struct LocalTemplate {
    workspace: WorkspaceContext,
    tools: CodingToolsFactory,
}

impl LocalTemplate {
    pub(crate) fn new(workspace: WorkspaceContext, tools: CodingToolsFactory) -> Self {
        Self { workspace, tools }
    }

    /// Starts the instance: builds its tools for the workspace.
    pub(crate) fn start_now(&self) -> Result<StartedRuntime, String> {
        Ok(StartedRuntime {
            info: RuntimeInfo::coding(LOCAL_RUNTIME_ID, self.workspace.clone()),
            providers: (self.tools)(&self.workspace),
            skills: Vec::new(),
        })
    }
}

#[async_trait]
impl RuntimeTemplate for LocalTemplate {
    fn runtime_id(&self) -> &str {
        LOCAL_RUNTIME_ID
    }

    fn takes_workspace(&self) -> bool {
        false
    }

    async fn start(&self, _workspace: Option<&Path>) -> Result<StartedRuntime, String> {
        self.start_now()
    }
}

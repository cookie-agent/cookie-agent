//! The built-in `global` and `local` runtime templates.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use cookie_agent_config::SkillRegistry;
use cookie_agent_protocol::{
    CapabilityName, RuntimeCapabilityInfo, RuntimeKind, RuntimeLifetime,
    runtime_protocol::RuntimeMachine,
};
use cookie_agent_runtime_api::{
    CODING_RUNTIME_KIND, CodingToolsFactory, GLOBAL_RUNTIME_ID, LOCAL_RUNTIME_ID, RuntimeInfo,
    ToolProvider, WorkspaceContext,
};

use super::registry::{RuntimeListingState, RuntimeTemplate, StartedRuntime};

/// The `global` capability: the daemon's MCP servers, plugins, and global
/// tools, with no workspace.
fn global_capability() -> RuntimeCapabilityInfo {
    RuntimeCapabilityInfo {
        name: CapabilityName::global(),
        kind: RuntimeKind::new(GLOBAL_RUNTIME_ID).expect("`global` is a valid kind"),
        takes_workspace: false,
        description: "The daemon's MCP servers, plugins, and global tools".into(),
    }
}

/// The `coding` capability the built-in coding runtimes offer.
pub(crate) fn coding_capability() -> RuntimeCapabilityInfo {
    RuntimeCapabilityInfo {
        name: CapabilityName::coding(),
        kind: RuntimeKind::new(CODING_RUNTIME_KIND).expect("`coding` is a valid kind"),
        takes_workspace: true,
        description: "Read, write, edit, and run commands in a workspace".into(),
    }
}

/// The daemon's global runtime: top-level MCP servers, plugins, the
/// configured global tools, and user skills. It starts with the engine and is
/// never restarted.
pub(crate) struct GlobalTemplate;

#[async_trait]
impl RuntimeTemplate for GlobalTemplate {
    fn runtime_id(&self) -> &str {
        GLOBAL_RUNTIME_ID
    }

    fn lifetime(&self) -> RuntimeLifetime {
        RuntimeLifetime::Builtin
    }

    async fn capabilities(&self) -> Result<Vec<RuntimeCapabilityInfo>, String> {
        Ok(vec![global_capability()])
    }

    fn listing(&self) -> RuntimeListingState {
        RuntimeListingState {
            connected: true,
            capabilities: vec![global_capability()],
            platform: None,
            machine: RuntimeMachine::this_machine(),
            message: None,
        }
    }

    async fn start(
        &self,
        _capability: &CapabilityName,
        _workspace: Option<&Path>,
    ) -> Result<StartedRuntime, String> {
        Err("the global runtime starts with the daemon".into())
    }
}

impl GlobalTemplate {
    pub(crate) fn started(
        providers: Vec<Arc<dyn ToolProvider>>,
        skills: SkillRegistry,
    ) -> StartedRuntime {
        StartedRuntime {
            info: RuntimeInfo::global(),
            providers,
            skills,
            connection: None,
        }
    }
}

/// The built-in coding runtime called in-process, for tests and embedders
/// that do not need the protocol. Its instances open in any workspace, in
/// the daemon's working directory when a session names none.
pub(crate) struct LocalTemplate {
    workspace: WorkspaceContext,
    tools: CodingToolsFactory,
}

impl LocalTemplate {
    pub(crate) fn new(workspace: WorkspaceContext, tools: CodingToolsFactory) -> Self {
        Self { workspace, tools }
    }

    /// Opens the instance for `workspace`: builds its tools and finds the
    /// project skills from the repository root down to the workspace.
    pub(crate) fn open(&self, workspace: &WorkspaceContext) -> Result<StartedRuntime, String> {
        let skills = cookie_agent_config::load_project_skills(workspace.root())
            .map_err(|error| format!("project skills: {error}"))?;
        Ok(StartedRuntime {
            info: RuntimeInfo::coding(LOCAL_RUNTIME_ID, workspace.clone()),
            providers: (self.tools)(workspace),
            skills,
            connection: None,
        })
    }

    /// Opens the instance for the daemon's working directory.
    pub(crate) fn start_now(&self) -> Result<StartedRuntime, String> {
        self.open(&self.workspace)
    }
}

#[async_trait]
impl RuntimeTemplate for LocalTemplate {
    fn runtime_id(&self) -> &str {
        LOCAL_RUNTIME_ID
    }

    fn lifetime(&self) -> RuntimeLifetime {
        RuntimeLifetime::Builtin
    }

    async fn capabilities(&self) -> Result<Vec<RuntimeCapabilityInfo>, String> {
        Ok(vec![coding_capability()])
    }

    fn listing(&self) -> RuntimeListingState {
        RuntimeListingState {
            connected: true,
            capabilities: vec![coding_capability()],
            platform: self.workspace.platform().map(str::to_owned),
            machine: RuntimeMachine::this_machine(),
            message: None,
        }
    }

    fn default_workspace(&self, _capability: &CapabilityName) -> Option<PathBuf> {
        Some(self.workspace.root().to_owned())
    }

    async fn start(
        &self,
        _capability: &CapabilityName,
        workspace: Option<&Path>,
    ) -> Result<StartedRuntime, String> {
        let workspace = match workspace {
            Some(path) => WorkspaceContext::new(path).map_err(|error| error.to_string())?,
            None => self.workspace.clone(),
        };
        self.open(&workspace)
    }
}

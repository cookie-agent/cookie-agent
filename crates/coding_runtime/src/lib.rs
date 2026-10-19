//! The coding runtime's tools for cookie-agent client protocol 1.3: `read`, `write`,
//! `edit`, `bash`, and `webfetch`, prepared against the workspace of the
//! runtime that offers them. They implement the `cookie_agent_runtime_api`
//! contract and never depend on the engine.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
#[cfg(test)]
use cookie_agent_protocol::PermissionAction;
pub(crate) use cookie_agent_runtime_api::authoring::{parse_args, safe_title, schema};
use cookie_agent_runtime_api::{
    PreparedTool, RuntimeOptions, SessionToolContext, ToolCall, ToolError, ToolPreparationContext,
    ToolProvider, ToolSpec, WorkspaceContext,
};

pub mod bash;
mod display;
pub mod edit;
pub mod fs_cap;
mod path_args;
pub mod read;
pub mod server;
pub mod webfetch;
pub mod write;

/// Workspace of a coding runtime at `path`, for tests.
#[cfg(test)]
pub(crate) fn test_workspace(path: impl AsRef<Path>) -> WorkspaceContext {
    WorkspaceContext::new(path.as_ref()).expect("test workspace")
}

/// Origin of a tool of a coding runtime at `path`, for tests.
#[cfg(test)]
pub(crate) fn test_coding_origin(path: impl AsRef<Path>) -> cookie_agent_runtime_api::ToolOrigin {
    cookie_agent_runtime_api::ToolOrigin::Runtime(Arc::new(
        cookie_agent_runtime_api::RuntimeInfo::coding(
            cookie_agent_runtime_api::LOCAL_RUNTIME_ID,
            test_workspace(path),
        ),
    ))
}

/// `prepared`, a call of `provider`'s tool `name` in a coding runtime at
/// `workspace`, with the approval identity the daemon gives it.
#[cfg(test)]
pub(crate) fn identified(
    provider: &dyn ToolProvider,
    name: &str,
    workspace: &Path,
    prepared: PreparedTool,
) -> PreparedTool {
    cookie_agent_engine::testing::identify(provider, name, &test_coding_origin(workspace), prepared)
        .expect("the daemon identifies the call")
}

#[cfg(test)]
pub(crate) fn test_turn_context() -> std::sync::Arc<cookie_agent_runtime_api::TurnAgentContext> {
    std::sync::Arc::new(cookie_agent_runtime_api::TurnAgentContext {
        agent: cookie_agent_protocol::AgentId::new("test").expect("test agent ID"),
        model: "test/model".parse().expect("test model key"),
        adapter: cookie_agent_protocol::AdaptorId::OPENAI_CHAT,
        input_media: Vec::new(),
    })
}

/// The capabilities of a text-only test model.
#[cfg(test)]
pub(crate) fn test_capabilities() -> cookie_agent_protocol::ModelCapabilities {
    cookie_agent_protocol::ModelCapabilities {
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
    }
}

#[cfg(test)]
pub(crate) fn assert_workspace_rule_allows(
    prepared: &cookie_agent_runtime_api::PreparedTool,
    workspace: &Path,
    action: PermissionAction,
    resource: &str,
) {
    use cookie_agent_protocol::{
        AgentDocumentSource, AgentId, AgentMode, AgentSnapshot, PermissionEffect, PermissionRule,
        Sha256Digest, WildcardPattern,
    };

    let policy = AgentSnapshot {
        agent: AgentId::new("test").expect("agent id"),
        mode: AgentMode::Primary,
        description: "Test agent".into(),
        document_source: AgentDocumentSource::User,
        document_fingerprint: Sha256Digest::of_bytes(b"test document"),
        composed_prompt: "Test permission evaluation.\n".into(),
        prompt_fingerprint: Sha256Digest::of_bytes(b"Test permission evaluation.\n"),
        max_output_tokens: 0,
        permissions: vec![PermissionRule {
            runtime: None,
            action,
            resource: WildcardPattern::new(resource).expect("permission resource pattern"),
            effect: PermissionEffect::Allow,
        }],
        delegation: None,
        fallback_chain: Vec::new(),
        selected_suffix_start: 0,
    };
    let decision = cookie_agent_engine::permissions::PermissionPipeline::default()
        .decide_operation(
            &policy,
            prepared.operation(),
            prepared.policy_labels(),
            Some(workspace),
        );
    assert_eq!(decision.effect, PermissionEffect::Allow);
    assert_eq!(decision.evaluations.len(), 1);
}

/// Runs filesystem work on tokio's blocking pool instead of a runtime worker.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ToolError::execution(format!("filesystem task failed: {error}")))?
}

pub(crate) fn tool_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::execution(error.to_string())
}

fn normalized_path(path: &Path) -> String {
    #[cfg(windows)]
    let path = fs_cap::lexical_path_spelling(path);
    let value = readable_path(path.to_string_lossy().replace('\\', "/"));
    if value.is_empty() { ".".into() } else { value }
}

#[cfg(windows)]
fn readable_path(value: String) -> String {
    if let Some(path) = value.strip_prefix("//?/UNC/") {
        format!("//{path}")
    } else {
        value.strip_prefix("//?/").unwrap_or(&value).to_owned()
    }
}

#[cfg(not(windows))]
fn readable_path(value: String) -> String {
    value
}

pub(crate) fn permission_path_label(path: &str, workspace: &Path) -> String {
    let path = normalized_path(Path::new(path));
    #[cfg(not(windows))]
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_owned());
    #[cfg(windows)]
    let workspace = workspace.to_owned();
    if let Some(relative) = strip_absolute_prefix(&path, &normalized_path(&workspace)) {
        return relative;
    }
    path
}

pub(crate) fn abbreviated_display_path(path: &str, workspace: &Path) -> String {
    let path = normalized_path(Path::new(path));
    if !Path::new(&path).is_absolute() {
        return path;
    }
    #[cfg(not(windows))]
    {
        if let Some(relative) = strip_absolute_prefix(&path, &normalized_path(workspace)) {
            return relative;
        }
        if let Ok(home) = cookie_agent_protocol::paths::home_dir() {
            let home = normalized_path(&home);
            if let Some(relative) = strip_absolute_prefix(&path, &home) {
                return if relative == "." {
                    "~".into()
                } else {
                    format!("~/{relative}")
                };
            }
        }
        path
    }
    #[cfg(windows)]
    {
        if let Some(relative) = strip_absolute_prefix(&path, &normalized_path(workspace)) {
            return relative;
        }
        if let Ok(home) = cookie_agent_protocol::paths::home_dir()
            && let Some(relative) = strip_absolute_prefix(&path, &normalized_path(&home))
        {
            return if relative == "." {
                "~".into()
            } else {
                format!("~/{relative}")
            };
        }
        path
    }
}

fn strip_absolute_prefix(path: &str, prefix: &str) -> Option<String> {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return None;
    }
    if path_component_prefix(path, prefix) && path.len() == prefix.len() {
        return Some(".".into());
    }
    path_component_prefix(path, prefix)
        .then(|| &path[prefix.len()..])
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_owned)
}

#[cfg(windows)]
fn path_component_prefix(path: &str, prefix: &str) -> bool {
    path.get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

#[cfg(not(windows))]
fn path_component_prefix(path: &str, prefix: &str) -> bool {
    path.starts_with(prefix)
}

#[derive(Debug)]
pub struct BuiltinTools {
    read: read::ReadTool,
    write: write::WriteTool,
    edit: edit::EditTool,
    bash: bash::BashTool,
    webfetch: webfetch::WebfetchTool,
}

impl BuiltinTools {
    /// The coding runtime's tools, working in its `workspace`.
    #[must_use]
    pub fn new(workspace: &WorkspaceContext) -> Self {
        Self {
            read: read::ReadTool::new(workspace),
            write: write::WriteTool::new(workspace),
            edit: edit::EditTool::new(workspace),
            bash: bash::BashTool::new(workspace),
            webfetch: webfetch::WebfetchTool::coding(),
        }
    }
}

/// Runtime options for the daemon: the global runtime's fallback `webfetch`
/// and a local coding runtime at `workspace` offering [`BuiltinTools`],
/// served in this process through the runtime protocol. Open the engine with
/// `Engine::open_async`.
#[must_use]
pub fn runtime_options(workspace: impl Into<PathBuf>) -> RuntimeOptions {
    let mut options = RuntimeOptions::new(workspace);
    options.global_tools = vec![Arc::new(webfetch::WebfetchTool::global_fallback())];
    options.local =
        cookie_agent_runtime_api::LocalRuntime::Protocol(server::in_process_connector());
    options
}

#[async_trait]
impl ToolProvider for BuiltinTools {
    fn provider_id(&self) -> &'static str {
        "builtin.tools"
    }

    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let mut tools = Vec::new();
        tools.extend(self.read.tools_for_session(ctx)?);
        tools.extend(self.write.tools_for_session(ctx)?);
        tools.extend(self.edit.tools_for_session(ctx)?);
        tools.extend(self.bash.tools_for_session(ctx)?);
        tools.extend(self.webfetch.tools_for_session(ctx)?);
        Ok(tools)
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        match tool_name {
            "read" => read::ReadTool::get_permission_name(tool_name),
            "write" => write::WriteTool::get_permission_name(tool_name),
            "edit" => edit::EditTool::get_permission_name(tool_name),
            "bash" => bash::BashTool::get_permission_name(tool_name),
            "webfetch" => webfetch::WebfetchTool::get_permission_name(tool_name),
            _ => Err(tool_error(format!("unknown built-in tool `{tool_name}`"))),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        match name {
            "read" => self.read.get_permission_resource(name, arguments),
            "write" => self.write.get_permission_resource(name, arguments),
            "edit" => self.edit.get_permission_resource(name, arguments),
            "bash" => self.bash.get_permission_resource(name, arguments),
            "webfetch" => self.webfetch.get_permission_resource(name, arguments),
            _ => Err(tool_error(format!("unknown built-in tool `{name}`"))),
        }
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        match name {
            "read" => self.read.get_display_argument(name, arguments),
            "write" => self.write.get_display_argument(name, arguments),
            "edit" => self.edit.get_display_argument(name, arguments),
            "bash" => self.bash.get_display_argument(name, arguments),
            "webfetch" => self.webfetch.get_display_argument(name, arguments),
            _ => Err(tool_error(format!("unknown built-in tool `{name}`"))),
        }
    }

    fn presentation(&self, call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
        match call.name.as_str() {
            "read" => self.read.presentation(call),
            "write" => self.write.presentation(call),
            "edit" => self.edit.presentation(call),
            "bash" => self.bash.presentation(call),
            "webfetch" => self.webfetch.presentation(call),
            _ => cookie_agent_runtime_api::tool_title_only(&call.name),
        }
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        match call.name.as_str() {
            "read" => self.read.prepare(ctx, call).await,
            "write" => self.write.prepare(ctx, call).await,
            "edit" => self.edit.prepare(ctx, call).await,
            "bash" => self.bash.prepare(ctx, call).await,
            "webfetch" => self.webfetch.prepare(ctx, call).await,
            _ => Err(tool_error(format!("unknown built-in tool `{}`", call.name))),
        }
    }

    async fn prepare_parallel(
        &self,
        ctx: ToolPreparationContext,
        calls: Vec<ToolCall>,
    ) -> Vec<Result<PreparedTool, ToolError>> {
        // Route each same-tool group to the inner provider's batch
        // preparation so same-target edits and writes chain; every other
        // tool keeps per-call preparation.
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            if let Some((_, indexes)) = groups.iter_mut().find(|(name, _)| *name == call.name) {
                indexes.push(index);
            } else {
                groups.push((call.name.clone(), vec![index]));
            }
        }
        let mut results: Vec<Option<Result<PreparedTool, ToolError>>> =
            (0..calls.len()).map(|_| None).collect();
        for (name, indexes) in groups {
            let group_calls = indexes
                .iter()
                .map(|index| calls[*index].clone())
                .collect::<Vec<_>>();
            let prepared = match name.as_str() {
                "read" => self.read.prepare_parallel(ctx.clone(), group_calls).await,
                "write" => self.write.prepare_parallel(ctx.clone(), group_calls).await,
                "edit" => self.edit.prepare_parallel(ctx.clone(), group_calls).await,
                "bash" => self.bash.prepare_parallel(ctx.clone(), group_calls).await,
                _ => {
                    let mut prepared = Vec::with_capacity(group_calls.len());
                    for call in group_calls {
                        prepared.push(self.prepare(ctx.clone(), call).await);
                    }
                    prepared
                }
            };
            for (index, result) in indexes.into_iter().zip(prepared) {
                results[index] = Some(result);
            }
        }
        results
            .into_iter()
            .map(|result| result.expect("every built-in call is prepared"))
            .collect()
    }
}

#[cfg(test)]
mod tests;

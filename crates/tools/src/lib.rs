//! Exact cookie-agent protocol 21 prepared built-in tools.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedTool, SessionToolContext, ToolCall, ToolError, ToolPreparationContext, ToolProvider,
    ToolSpec,
};
use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, PermissionAction,
    PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
    PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity, Sha256Digest,
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};

pub mod bash;
pub mod delegate;
pub mod edit;
pub mod fs_cap;
pub mod goal;
pub mod message;
mod path_args;
pub mod read;
pub mod skill;
pub mod webfetch;
pub mod write;

#[cfg(test)]
pub(crate) fn test_turn_context() -> std::sync::Arc<cookie_agent_engine::TurnAgentContext> {
    std::sync::Arc::new(cookie_agent_engine::TurnAgentContext {
        agent: cookie_agent_protocol::AgentId::new("test").expect("test agent ID"),
        model: "test/model".parse().expect("test model key"),
        adapter: cookie_agent_protocol::AdaptorId::OpenaiChat,
        adapter_family: cookie_agent_models::adapters::OvenAdapterFamily::OpenaiChat,
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
pub(crate) fn assert_workspace_rule_allows(
    prepared: &cookie_agent_engine::PreparedTool,
    workspace: &Path,
    action: PermissionAction,
    resource: &str,
) {
    use cookie_agent_protocol::{
        AgentDocumentSource, AgentId, AgentMode, AgentSchemaVersion, AgentSnapshot,
        PermissionEffect, PermissionRule, WildcardPattern,
    };

    let policy = AgentSnapshot {
        agent: AgentId::new("test").expect("agent id"),
        schema: AgentSchemaVersion::current(),
        mode: AgentMode::Primary,
        description: "Test agent".into(),
        document_source: AgentDocumentSource::Workspace,
        document_fingerprint: Sha256Digest::of_bytes(b"test document"),
        composed_prompt: "Test permission evaluation.\n".into(),
        prompt_fingerprint: Sha256Digest::of_bytes(b"Test permission evaluation.\n"),
        max_output_tokens: 0,
        permissions: vec![PermissionRule {
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
            workspace,
        );
    assert_eq!(decision.effect, PermissionEffect::Allow);
    assert_eq!(decision.evaluations.len(), 1);
}

pub(crate) fn schema<T: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("tool schemas serialize")
}

pub(crate) fn tool_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::execution(error.to_string())
}

pub(crate) fn safe_title(value: impl AsRef<str>) -> cookie_agent_protocol::SafeDisplayText {
    let mut safe = String::new();
    for character in value.as_ref().chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if safe.len() + character.len_utf8() > cookie_agent_protocol::SafeDisplayText::MAX_BYTES {
            break;
        }
        safe.push(character);
    }
    cookie_agent_protocol::SafeDisplayText::new(if safe.is_empty() {
        "Tool result".to_owned()
    } else {
        safe
    })
    .expect("sanitized tool title")
}

pub(crate) fn parse_args<T: DeserializeOwned>(
    tool: &str,
    value: serde_json::Value,
) -> Result<T, ToolError> {
    serde_json::from_value(value).map_err(|error| {
        tool_error(format!(
            "The {tool} tool was called with invalid arguments: {error}.\nPlease rewrite the input so it satisfies the expected schema."
        ))
    })
}

pub(crate) fn prepared_operation<T: Serialize>(
    tool: &str,
    normalized_arguments: &T,
    capabilities: Vec<(PermissionAction, &str)>,
    resources: Vec<PreparedApprovalResource>,
    execution_context_bytes: &[u8],
) -> Result<PreparedOperationIdentity, ToolError> {
    let arguments = serde_json::to_vec(normalized_arguments).map_err(tool_error)?;
    let capabilities = capabilities
        .into_iter()
        .map(|(action, operation)| {
            Ok(ApprovalCapability {
                action,
                operation: PreparedCapabilityOperation::new(format!("{tool}:{operation}"))
                    .map_err(tool_error)?,
            })
        })
        .collect::<Result<Vec<_>, ToolError>>()?;
    PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(&arguments),
        capabilities,
        resources,
        Sha256Digest::of_bytes(execution_context_bytes),
    )
    .map_err(tool_error)
}

pub(crate) fn prepared_resource(
    action: PermissionAction,
    logical_kind: &str,
    stable_label_bytes: &[u8],
    binding_bytes: &[u8],
    lifetime: PreparedBindingLifetime,
    source: ApprovalResourceSource,
) -> Result<PreparedApprovalResource, ToolError> {
    let label = Sha256Digest::of_bytes(stable_label_bytes);
    let mut complete_binding = Vec::new();
    complete_binding.extend_from_slice(logical_kind.as_bytes());
    complete_binding.push(0);
    complete_binding.extend_from_slice(stable_label_bytes);
    complete_binding.push(0);
    complete_binding.extend_from_slice(binding_bytes);
    Ok(PreparedApprovalResource {
        capability: action,
        canonical: PreparedResourceIdentity::new(format!("{logical_kind}:{}", label.as_str()))
            .map_err(tool_error)?,
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(&complete_binding),
        binding_lifetime: lifetime,
        boundary: if action == PermissionAction::Bash {
            ApprovalBoundary::CommandPrefix {
                prefix: String::from_utf8_lossy(stable_label_bytes).into_owned(),
            }
        } else {
            ApprovalBoundary::Exact
        },
        source,
    })
}

pub(crate) fn prepared_path_resources(
    action: PermissionAction,
    logical_kind: &str,
    requested_path: &Path,
    workspace: &Path,
    binding_bytes: &[u8],
) -> Result<(Vec<PreparedApprovalResource>, Vec<String>), ToolError> {
    #[cfg(not(windows))]
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_owned());
    #[cfg(windows)]
    let workspace = workspace.to_owned();
    // Authorization names the lexical request, never its resolved destination.
    let label = permission_path_label(&normalized_path(requested_path), &workspace);
    let resource = prepared_resource(
        action,
        logical_kind,
        label.as_bytes(),
        binding_bytes,
        PreparedBindingLifetime::ProcessLocal,
        ApprovalResourceSource::PrimaryOperation,
    )?;
    Ok((vec![resource], vec![label]))
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
}

impl BuiltinTools {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            read: read::ReadTool::new(workspace.clone()),
            write: write::WriteTool::new(workspace.clone()),
            edit: edit::EditTool::new(workspace.clone()),
            bash: bash::BashTool::new(workspace),
        }
    }
}

impl Default for BuiltinTools {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
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
        tools.extend(webfetch::WebfetchTool.tools_for_session(ctx)?);
        Ok(tools)
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
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
    ) -> Result<(&'static str, Option<String>), ToolError> {
        match name {
            "read" => self.read.get_permission_resource(name, arguments),
            "write" => self.write.get_permission_resource(name, arguments),
            "edit" => self.edit.get_permission_resource(name, arguments),
            "bash" => self.bash.get_permission_resource(name, arguments),
            "webfetch" => webfetch::WebfetchTool.get_permission_resource(name, arguments),
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
            "webfetch" => webfetch::WebfetchTool.get_display_argument(name, arguments),
            _ => Err(tool_error(format!("unknown built-in tool `{name}`"))),
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
            "webfetch" => webfetch::WebfetchTool.prepare(ctx, call).await,
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

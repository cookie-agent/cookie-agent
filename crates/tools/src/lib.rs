//! Exact cookie-agent protocol 16 prepared built-in tools.

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
pub mod read;
pub mod read_tool_result;
pub mod skill;
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
        Ok(tools)
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "read" => read::ReadTool::get_permission_name(tool_name),
            "write" => write::WriteTool::get_permission_name(tool_name),
            "edit" => edit::EditTool::get_permission_name(tool_name),
            "bash" => bash::BashTool::get_permission_name(tool_name),
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
            _ => Err(tool_error(format!("unknown built-in tool `{}`", call.name))),
        }
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_engine::{
        SessionToolContext, ToolCall, ToolError, ToolPreparationContext, ToolProvider,
    };
    use cookie_agent_protocol::{
        ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, OperationFingerprint,
        PermissionAction, PreparedApprovalResource, PreparedBindingLifetime,
        PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
        PreparedResourceIdentity, RunId, SessionId, Sha256Digest, ToolCallId,
    };

    use super::{
        BuiltinTools, bash::BashTool, delegate::DelegateToolProvider, edit::EditTool,
        goal::GoalTools, read::ReadTool, read_tool_result::ReadToolResultProvider,
        write::WriteTool,
    };

    #[test]
    fn static_permission_names_cover_every_builtin_and_delegate_tool() {
        assert_eq!(ReadTool::get_permission_name("read").unwrap(), "read");
        assert_eq!(WriteTool::get_permission_name("write").unwrap(), "write");
        assert_eq!(EditTool::get_permission_name("edit").unwrap(), "write");
        assert_eq!(BashTool::get_permission_name("bash").unwrap(), "bash");
        assert_eq!(GoalTools::get_permission_name("goal_get").unwrap(), "read");
        assert_eq!(
            GoalTools::get_permission_name("goal_update").unwrap(),
            "write"
        );
        assert_eq!(
            ReadToolResultProvider::get_permission_name("read_tool_result").unwrap(),
            "read_tool_result"
        );
        for name in [
            "delegate_subagent",
            "get_subagent_result",
            "steer_subagent",
            "cancel_subagent",
        ] {
            assert_eq!(
                DelegateToolProvider::get_permission_name(name).unwrap(),
                "delegate"
            );
        }
        assert!(BuiltinTools::get_permission_name("grep").is_err());
    }

    #[test]
    fn self_paginating_tools_declare_truncation_opt_out() {
        let read = ReadTool::new("/tmp")
            .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
            .unwrap()
            .remove(0);
        assert_eq!(
            read.result_truncation,
            cookie_agent_engine::ToolResultTruncationPolicy::OptOut
        );
        assert_eq!(
            ReadToolResultProvider::tool_spec().result_truncation,
            cookie_agent_engine::ToolResultTruncationPolicy::OptOut
        );
        assert_eq!(
            super::delegate::result_truncation_policy("get_subagent_result"),
            cookie_agent_engine::ToolResultTruncationPolicy::OptOut
        );
    }

    #[test]
    fn builtin_tools_do_not_expose_grep_or_glob() {
        let tools = BuiltinTools::new("/tmp");
        let names = tools
            .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
            .expect("built-in tools")
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["read", "write", "edit", "bash"]);
        for name in ["grep", "glob"] {
            let arguments = serde_json::json!({});
            assert!(
                matches!(
                    tools.get_permission_resource(name, &arguments),
                    Err(ToolError::Failed(_))
                ),
                "{name} permission resource"
            );
            assert!(
                matches!(
                    tools.get_display_argument(name, &arguments),
                    Err(ToolError::Failed(_))
                ),
                "{name} display"
            );
        }
    }

    #[tokio::test]
    async fn unknown_grep_and_glob_calls_fail_closed() {
        let tools = super::BuiltinTools::new("/tmp");
        let context = ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            turn_context: crate::test_turn_context(),
        };
        for name in ["grep", "glob"] {
            let result = tools
                .prepare(
                    context.clone(),
                    ToolCall {
                        id: ToolCallId::new_v7(),
                        name: name.into(),
                        arguments: serde_json::json!({"pattern":"TODO"}),
                    },
                )
                .await;
            assert!(
                matches!(result, Err(ToolError::Failed(message)) if message.contains("unknown built-in tool")),
                "{name}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn builtin_relative_paths_revalidate_a_symlinked_cwd_route() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let destination = root.path().join("destination");
        std::fs::create_dir(&destination).expect("destination");
        std::fs::write(destination.join("value.txt"), "old").expect("fixture");
        let cwd = root.path().join("cwd");
        symlink(&destination, &cwd).expect("cwd symlink");
        let context = ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            cwd: cwd.clone(),
            workspace_root: root.path().to_owned(),
            turn_context: crate::test_turn_context(),
        };
        let tools = BuiltinTools::new(root.path());
        let mut prepared = Vec::new();
        for (name, arguments) in [
            ("read", serde_json::json!({"filePath":"value.txt"})),
            (
                "write",
                serde_json::json!({"filePath":"value.txt","content":"new"}),
            ),
            (
                "edit",
                serde_json::json!({
                    "filePath":"value.txt",
                    "oldString":"old",
                    "newString":"new"
                }),
            ),
        ] {
            prepared.push(
                tools
                    .prepare(
                        context.clone(),
                        ToolCall {
                            id: ToolCallId::new_v7(),
                            name: name.into(),
                            arguments,
                        },
                    )
                    .await
                    .expect("prepare through symlinked cwd"),
            );
        }

        std::fs::remove_file(&cwd).expect("remove cwd route");
        symlink(&destination, &cwd).expect("replace cwd route");
        for prepared in prepared {
            let error = prepared
                .execute_for_test(
                    cookie_agent_engine::ToolExecutionContext::for_test(
                        root.path().join("artifacts"),
                        crate::test_turn_context(),
                    )
                    .expect("execution context"),
                )
                .await
                .expect_err("cwd route swap must fail");
            assert!(matches!(error, ToolError::OperationChanged(_)));
        }
    }

    #[test]
    fn fixed_identity_v7_fingerprint_is_golden_and_deterministic() {
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"normalized arguments without raw paths"),
            vec![ApprovalCapability {
                action: PermissionAction::Bash,
                operation: PreparedCapabilityOperation::new("execute").expect("operation"),
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Bash,
                canonical: PreparedResourceIdentity::new("command:git-status").expect("identity"),
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    b"executable-content-and-open-directory-binding",
                ),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::CommandPrefix {
                    prefix: "git status".into(),
                },
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"execution context"),
        )
        .expect("prepared operation");
        let fingerprint = OperationFingerprint::from_prepared_operation(&operation);
        assert_eq!(
            fingerprint.digest().as_str(),
            "80b953e606ab07a7d25bde074c9d2086012b3b86a62ead3b3751e7cc5f5cbffd"
        );
        assert_eq!(
            fingerprint,
            OperationFingerprint::from_prepared_operation(&operation)
        );
    }

    #[test]
    fn abbreviated_display_path_prefers_workspace_then_home() {
        let workspace = tempfile::tempdir().expect("workspace");
        assert_eq!(
            super::abbreviated_display_path("src/lib.rs", workspace.path()),
            "src/lib.rs"
        );
        assert_eq!(
            super::abbreviated_display_path(
                &workspace.path().join("src/lib.rs").to_string_lossy(),
                workspace.path()
            ),
            "src/lib.rs"
        );
        assert_eq!(
            super::abbreviated_display_path(&workspace.path().to_string_lossy(), workspace.path()),
            "."
        );
        let home = cookie_agent_protocol::paths::home_dir().expect("home directory");
        assert_eq!(
            super::abbreviated_display_path(&home.to_string_lossy(), workspace.path()),
            "~"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_display_path_comparison_remains_lexical_for_symlinked_workspaces() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary root");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let alias = directory.path().join("workspace-alias");
        symlink(&workspace, &alias).expect("workspace symlink");

        let canonical_child = workspace.join("src/lib.rs");
        assert_eq!(
            super::abbreviated_display_path(&canonical_child.to_string_lossy(), &alias),
            super::normalized_path(&canonical_child),
        );
        let lexical_child = alias.join("src/lib.rs");
        assert_eq!(
            super::abbreviated_display_path(&lexical_child.to_string_lossy(), &alias),
            "src/lib.rs",
        );
    }

    #[test]
    fn relative_display_path_is_returned_before_workspace_comparison() {
        assert_eq!(
            super::abbreviated_display_path(
                "workspace/src/lib.rs",
                std::path::Path::new("workspace"),
            ),
            "workspace/src/lib.rs",
        );
    }

    #[cfg(windows)]
    fn windows_short_path(path: &std::path::Path) -> std::path::PathBuf {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let required = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
        assert_ne!(
            required,
            0,
            "GetShortPathNameW: {}",
            std::io::Error::last_os_error()
        );
        let mut short = vec![0; required as usize];
        let length = unsafe { GetShortPathNameW(wide.as_ptr(), short.as_mut_ptr(), required) };
        assert!(length > 0 && length < required);
        std::ffi::OsString::from_wide(&short[..length as usize]).into()
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_short_workspace_spelling_roundtrips_prepared_resources() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace with a long name");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("value.txt"), "value").expect("fixture");
        let long = workspace.canonicalize().expect("long workspace spelling");
        let short = windows_short_path(&long);
        let tool = ReadTool::new(&long);
        let prepared = tool
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: short.clone(),
                    workspace_root: long.clone(),
                    turn_context: crate::test_turn_context(),
                },
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "read".into(),
                    arguments: serde_json::json!({"filePath":"value.txt"}),
                },
            )
            .await
            .expect("prepare through short cwd");
        assert_eq!(prepared.operation().resources().len(), 1);
        assert_eq!(prepared.policy_labels(), [Some("value.txt".into())]);
        super::assert_workspace_rule_allows(&prepared, &long, PermissionAction::Read, "value.txt");
        assert_eq!(
            tool.get_permission_resource("read", prepared.normalized_arguments())
                .unwrap(),
            ("read", Some("value.txt".into()))
        );
        for (requested, workspace) in [(&short, &long), (&long, &short)] {
            assert_eq!(
                super::permission_path_label(
                    &requested.join("missing/file.txt").to_string_lossy(),
                    workspace
                ),
                "missing/file.txt"
            );
            assert_eq!(
                super::abbreviated_display_path(
                    &requested.join("missing/file.txt").to_string_lossy(),
                    workspace
                ),
                "missing/file.txt"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_permission_path_label_does_not_resolve_requested_path() {
        let directory = tempfile::tempdir().expect("temporary root");
        let workspace = directory.path().join("workspace");
        let destination = directory.path().join("destination");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::create_dir(&destination).expect("destination");
        let alias = workspace.join("alias");
        if let Err(error) = std::os::windows::fs::symlink_dir(&destination, &alias) {
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("create directory symlink: {error}");
        }
        let requested = alias.join("value.txt");

        assert_eq!(
            super::permission_path_label(&requested.to_string_lossy(), &workspace),
            "alias/value.txt"
        );
        assert_eq!(
            super::abbreviated_display_path(&requested.to_string_lossy(), &workspace),
            "alias/value.txt"
        );
        let outside_alias = directory.path().join("outside-alias");
        std::os::windows::fs::symlink_dir(&workspace, &outside_alias).expect("outside alias");
        let outside_request = outside_alias.join("value.txt");
        assert_eq!(
            super::permission_path_label(&outside_request.to_string_lossy(), &workspace),
            super::normalized_path(&outside_request)
        );
        let parent_display =
            super::abbreviated_display_path(&directory.path().to_string_lossy(), &workspace);
        assert_eq!(
            super::abbreviated_display_path(&outside_request.to_string_lossy(), &workspace),
            format!("{parent_display}/outside-alias/value.txt")
        );
        let destination_file = destination.join("a long destination filename.txt");
        std::fs::write(&destination_file, "external").expect("external fixture");
        let short_file = windows_short_path(&destination_file);
        let suffix = short_file.file_name().expect("short leaf");
        let requested = windows_short_path(&workspace).join("alias").join(suffix);
        let expected = format!("alias/{}", suffix.to_string_lossy());
        assert_eq!(
            super::permission_path_label(&requested.to_string_lossy(), &workspace),
            expected
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_display_paths_hide_verbatim_prefixes() {
        assert_eq!(
            super::normalized_path(std::path::Path::new(r"\\?\C:\Users\runneradmin\file.txt")),
            "C:/Users/runneradmin/file.txt"
        );
        assert_eq!(
            super::normalized_path(std::path::Path::new(r"\\?\UNC\server\share\file.txt")),
            "//server/share/file.txt"
        );
    }

    #[test]
    fn absent_write_v7_fingerprint_is_golden() {
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"write arguments with content digest"),
            vec![ApprovalCapability {
                action: PermissionAction::Write,
                operation: PreparedCapabilityOperation::new("write:replace").expect("operation"),
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Write,
                canonical: PreparedResourceIdentity::new("file:expected-absent").expect("identity"),
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    b"anchor-identity\0parent-identity\0basename\0expected-absent\0new-content",
                ),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"held workspace identity"),
        )
        .expect("prepared operation");
        assert_eq!(
            OperationFingerprint::from_prepared_operation(&operation)
                .digest()
                .as_str(),
            "7c3f201fb116f919296a492d3f64b8516bf70adf9cd7202d744d83173a4f4c11"
        );
    }
}

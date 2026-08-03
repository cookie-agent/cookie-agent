//! Protocol-v6 prepared built-in tools.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedTool, SessionToolContext, ToolCall, ToolError, ToolPreparationContext, ToolProvider,
    ToolSpec,
};
use cookie_agent_protocol::{
    ActionKind, ApprovalBoundary, ApprovalCapability, ApprovalResourceSource,
    PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
    PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity, Sha256Digest,
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};

pub mod bash;
pub mod delegate;
pub mod edit;
pub mod fs_cap;
pub mod glob;
pub mod grep;
pub mod read;
pub mod write;

pub(crate) fn schema<T: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("tool schemas serialize")
}

pub(crate) fn tool_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::execution(error.to_string())
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
    capabilities: Vec<(ActionKind, &str)>,
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
    action: ActionKind,
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
        boundary: if action == ActionKind::Bash {
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
    action: ActionKind,
    logical_kind: &str,
    canonical_path: &Path,
    workspace: &Path,
    binding_bytes: &[u8],
) -> Result<(Vec<PreparedApprovalResource>, Vec<String>, bool), ToolError> {
    let label = canonical_path.to_string_lossy();
    let mut resources = vec![prepared_resource(
        action,
        logical_kind,
        label.as_bytes(),
        binding_bytes,
        PreparedBindingLifetime::ProcessLocal,
        ApprovalResourceSource::PrimaryOperation,
    )?];
    let mut labels = vec![label.to_string()];
    let external = !canonical_path.starts_with(workspace);
    if external {
        resources.push(prepared_resource(
            ActionKind::ExternalDirectory,
            "external-directory",
            label.as_bytes(),
            binding_bytes,
            PreparedBindingLifetime::ProcessLocal,
            ApprovalResourceSource::ExternalDirectoryGuard,
        )?);
        labels.push(label.to_string());
    }
    Ok((resources, labels, external))
}

#[derive(Debug)]
pub struct BuiltinTools {
    read: read::ReadTool,
    write: write::WriteTool,
    edit: edit::EditTool,
    bash: bash::BashTool,
    grep: grep::GrepTool,
    glob: glob::GlobTool,
}

impl BuiltinTools {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            read: read::ReadTool::new(workspace.clone()),
            write: write::WriteTool::new(workspace.clone()),
            edit: edit::EditTool::new(workspace.clone()),
            bash: bash::BashTool::new(workspace.clone()),
            grep: grep::GrepTool::new(workspace.clone()),
            glob: glob::GlobTool::new(workspace),
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
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let mut tools = Vec::new();
        tools.extend(self.read.tools_for_session(ctx)?);
        tools.extend(self.write.tools_for_session(ctx)?);
        tools.extend(self.edit.tools_for_session(ctx)?);
        tools.extend(self.bash.tools_for_session(ctx)?);
        tools.extend(self.grep.tools_for_session(ctx)?);
        tools.extend(self.glob.tools_for_session(ctx)?);
        Ok(tools)
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
            "grep" => self.grep.prepare(ctx, call).await,
            "glob" => self.glob.prepare(ctx, call).await,
            _ => Err(tool_error(format!("unknown built-in tool `{}`", call.name))),
        }
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        ActionKind, ApprovalBoundary, ApprovalCapability, ApprovalResourceSource,
        OperationFingerprint, PreparedApprovalResource, PreparedBindingLifetime,
        PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
        PreparedResourceIdentity, Sha256Digest,
    };

    #[test]
    fn fixed_identity_v6_fingerprint_is_golden_and_deterministic() {
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"normalized arguments without raw paths"),
            vec![ApprovalCapability {
                action: ActionKind::Bash,
                operation: PreparedCapabilityOperation::new("execute").expect("operation"),
            }],
            vec![PreparedApprovalResource {
                capability: ActionKind::Bash,
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
            "87a4bc54435b2aa6d305ec0f423cb41f261d517b39918bcfe8a485e2508c81c5"
        );
        assert_eq!(
            fingerprint,
            OperationFingerprint::from_prepared_operation(&operation)
        );
    }

    #[test]
    fn absent_write_v6_fingerprint_is_golden() {
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"write arguments with content digest"),
            vec![ApprovalCapability {
                action: ActionKind::Write,
                operation: PreparedCapabilityOperation::new("write:replace").expect("operation"),
            }],
            vec![PreparedApprovalResource {
                capability: ActionKind::Write,
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
            "55e549ae8fe7646873db06b523032b14883c06e52a5c1e6ccbd4302031b465f7"
        );
    }
}

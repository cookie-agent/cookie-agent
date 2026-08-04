use std::path::PathBuf;

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedExecutor, PreparedSerializationKey, PreparedTool, SessionToolContext, ToolCall,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{PermissionAction, PersistedToolResult as ToolResult, Sha256Digest};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{fs_cap, parse_args, prepared_operation, prepared_path_resources, schema};

#[derive(Debug)]
pub struct EditTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
struct EditArgs {
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(rename = "oldString")]
    old_string: String,
    #[serde(rename = "newString")]
    new_string: String,
    #[serde(rename = "replaceAll", default)]
    replace_all: bool,
}

struct EditExecutor {
    target: fs_cap::PreparedExisting,
    new_bytes: Vec<u8>,
}

impl EditTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}
impl Default for EditTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for EditTool {
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "edit".into(),
            description: "Apply a precomputed semantic replacement atomically.".into(),
            parameters: schema::<EditArgs>(),
        }])
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: EditArgs = parse_args("edit", call.arguments)?;
        fs_cap::ensure_atomic_write_supported()?;
        if args.old_string.is_empty() {
            return Err(ToolError::execution("oldString must not be empty"));
        }
        let target = fs_cap::prepare_existing(&ctx.cwd, std::path::Path::new(&args.file_path))?;
        if target.directory {
            return Err(ToolError::unsupported_security(
                "edit target is a directory",
            ));
        }
        let bytes = target.read_bytes()?;
        let text =
            String::from_utf8(bytes).map_err(|_| ToolError::execution("edit requires UTF-8"))?;
        let count = text.matches(&args.old_string).count();
        if count == 0 {
            return Err(ToolError::execution("oldString was not found"));
        }
        if !args.replace_all && count != 1 {
            return Err(ToolError::execution(format!(
                "oldString matched {count} times"
            )));
        }
        let replaced = if args.replace_all {
            text.replace(&args.old_string, &args.new_string)
        } else {
            text.replacen(&args.old_string, &args.new_string, 1)
        };
        let new_bytes = replaced.into_bytes();
        let mut binding = target.identity.canonical_bytes();
        binding.extend_from_slice(target.content_digest.as_str().as_bytes());
        binding.extend_from_slice(
            Sha256Digest::of_bytes(args.old_string.as_bytes())
                .as_str()
                .as_bytes(),
        );
        binding.extend_from_slice(
            Sha256Digest::of_bytes(args.new_string.as_bytes())
                .as_str()
                .as_bytes(),
        );
        binding.extend_from_slice(&(count as u64).to_be_bytes());
        binding.extend_from_slice(Sha256Digest::of_bytes(&new_bytes).as_str().as_bytes());
        binding.extend_from_slice(&target.manifest_bytes()?);
        let (resources, policy_labels, external) = prepared_path_resources(
            PermissionAction::Write,
            "file",
            &target.display_path,
            &self.workspace,
            &binding,
            false,
        )?;
        let context = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "edit",
            &args,
            if external {
                vec![
                    (PermissionAction::Write, "edit"),
                    (PermissionAction::ExternalDirectory, "guard"),
                ]
            } else {
                vec![(PermissionAction::Write, "edit")]
            },
            resources,
            &context,
        )?;
        let mut serialization_key = target.identity.device.to_be_bytes().to_vec();
        serialization_key.extend_from_slice(&target.identity.inode.to_be_bytes());
        PreparedTool::new(
            operation,
            Some(PreparedSerializationKey::new(serialization_key)),
            Box::new(EditExecutor { target, new_bytes }),
        )
        .with_policy_labels(policy_labels)
    }
}

#[async_trait]
impl PreparedExecutor for EditExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        self.target.revalidate()
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution(
                "prepared edit cancelled before commit",
            ));
        }
        let outcome = self.target.replace_atomically(&self.new_bytes)?;
        Ok(ToolResult {
            title: crate::safe_title(format!("Edited {}", self.target.display_path.display())),
            output: "Edit applied atomically".into(),
            metadata: serde_json::json!({"new_sha256":Sha256Digest::of_bytes(&self.new_bytes),"cleanup_warning":outcome.cleanup_warning}),
            truncation: None,
            attachments: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cookie_agent_engine::{ToolCall, ToolError, ToolPreparationContext, ToolProvider};
    use cookie_agent_protocol::{RunId, SessionId, ToolCallId};

    use super::EditTool;

    #[tokio::test]
    async fn ambiguous_single_replacement_is_rejected_before_approval() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("value.txt"), "same same").expect("fixture");
        let result = EditTool::new(root.path())
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: root.path().to_owned(),
                    workspace_root: root.path().to_owned(),
                },
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "edit".into(),
                    arguments: serde_json::json!({
                        "filePath":"value.txt",
                        "oldString":"same",
                        "newString":"new"
                    }),
                },
            )
            .await;
        assert!(matches!(result, Err(ToolError::Failed(_))));
    }
}

use std::path::PathBuf;

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedExecutor, PreparedSerializationKey, PreparedTool, SessionToolContext, ToolCall,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{PermissionAction, PersistedToolResult as ToolResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{fs_cap, parse_args, prepared_operation, prepared_path_resources, schema};

#[derive(Debug)]
pub struct WriteTool {
    workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
struct WriteArgs {
    #[serde(rename = "filePath")]
    file_path: String,
    content: String,
}

struct WriteExecutor {
    target: fs_cap::PreparedTarget,
    bytes: Vec<u8>,
}

impl WriteTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for WriteTool {
    fn provider_id(&self) -> &'static str {
        "builtin.write"
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            result_truncation: Default::default(),
            name: "write".into(),
            permission_name: Self::get_permission_name("write")?.into(),
            description: "Atomically write an exact descriptor-bound target.".into(),
            parameters: schema::<WriteArgs>(),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "write" => Ok("write"),
            _ => Err(ToolError::execution("write provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let permission_name = Self::get_permission_name(name)?;
        let args: WriteArgs = parse_args("write", arguments.clone())?;
        if args.file_path.is_empty() {
            return Err(ToolError::execution("filePath must not be empty"));
        }
        Ok((
            permission_name,
            Some(crate::permission_path_label(
                &args.file_path,
                &self.workspace,
            )),
        ))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        let (_, Some(path)) = self.get_permission_resource(name, arguments)? else {
            return Err(ToolError::execution("write permission resource is missing"));
        };
        Ok(crate::abbreviated_display_path(&path, &self.workspace))
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: WriteArgs = parse_args("write", call.arguments)?;
        fs_cap::ensure_atomic_write_supported()?;
        let target = fs_cap::prepare_target(&ctx.cwd, std::path::Path::new(&args.file_path))?;
        let mut binding = match &target {
            fs_cap::PreparedTarget::Existing(existing) => {
                if existing.directory {
                    return Err(ToolError::unsupported_security(
                        "write target is a directory",
                    ));
                }
                let mut bytes = existing.identity.canonical_bytes();
                bytes.extend_from_slice(existing.content_digest.as_str().as_bytes());
                bytes
            }
            fs_cap::PreparedTarget::Absent(_) => b"expected-absent".to_vec(),
        };
        binding.extend_from_slice(
            cookie_agent_protocol::Sha256Digest::of_bytes(args.content.as_bytes())
                .as_str()
                .as_bytes(),
        );
        binding.extend_from_slice(&target.manifest_bytes()?);
        let display_path = match &target {
            fs_cap::PreparedTarget::Existing(target) => &target.display_path,
            fs_cap::PreparedTarget::Absent(target) => &target.display_path,
        };
        let (resources, policy_labels) = prepared_path_resources(
            PermissionAction::Write,
            "file",
            display_path,
            &self.workspace,
            &binding,
        )?;
        let serialization_key = target.serialization_bytes()?;
        let context = fs_cap::cwd_context_bytes(&ctx.cwd)?;
        let operation = prepared_operation(
            "write",
            &args,
            vec![(PermissionAction::Write, "replace")],
            resources,
            &context,
        )?;
        let normalized_arguments = serde_json::json!({
            "filePath": display_path,
            "content": args.content,
        });
        PreparedTool::new(
            operation,
            normalized_arguments,
            Some(PreparedSerializationKey::new(serialization_key)),
            Box::new(WriteExecutor {
                target,
                bytes: args.content.into_bytes(),
            }),
        )?
        .with_policy_labels(policy_labels)
    }
}

#[async_trait]
impl PreparedExecutor for WriteExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        self.target.revalidate()
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution(
                "prepared write cancelled before commit",
            ));
        }
        let (path, outcome) = match &self.target {
            fs_cap::PreparedTarget::Existing(target) => {
                let outcome = target.replace_atomically(&self.bytes)?;
                (target.display_path.clone(), outcome)
            }
            fs_cap::PreparedTarget::Absent(target) => {
                let outcome = target.create_atomically(&self.bytes)?;
                (target.display_path.clone(), outcome)
            }
        };
        Ok(ToolResult {
            title: crate::safe_title(format!("Wrote {}", path.display())),
            output: format!("Wrote {} bytes to {}", self.bytes.len(), path.display()),
            metadata: serde_json::json!({"bytes":self.bytes.len(),"cleanup_warning":outcome.cleanup_warning}),
            truncation: None,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_engine::{ToolCall, ToolError, ToolPreparationContext, ToolProvider};
    use cookie_agent_protocol::{
        OperationFingerprint, PermissionAction, RunId, SessionId, ToolCallId,
    };

    use super::WriteTool;

    #[test]
    fn permission_resource_is_the_file_path() {
        let tool = WriteTool::new("/tmp");
        assert_eq!(
            tool.get_permission_resource(
                "write",
                &serde_json::json!({"filePath":"out.txt","content":"x"})
            )
            .expect("permission resource"),
            ("write", Some("out.txt".into()))
        );
        assert!(matches!(
            tool.get_permission_resource("write", &serde_json::json!({"content":"x"})),
            Err(ToolError::Failed(_))
        ));
    }

    #[test]
    fn display_argument_abbreviates_workspace_and_home_paths() {
        let workspace = tempfile::tempdir().expect("workspace");
        let tool = WriteTool::new(workspace.path());
        assert_eq!(
            tool.get_display_argument(
                "write",
                &serde_json::json!({"filePath":"out.txt","content":"x"})
            )
            .expect("relative"),
            "out.txt"
        );
        assert_eq!(
            tool.get_display_argument(
                "write",
                &serde_json::json!({"filePath":workspace.path().join("out.txt"),"content":"x"})
            )
            .expect("workspace"),
            "out.txt"
        );
        let home = cookie_agent_protocol::paths::home_dir().expect("home directory");
        assert_eq!(
            tool.get_display_argument(
                "write",
                &serde_json::json!({"filePath": home.join("notes.txt"),"content":"x"})
            )
            .expect("home"),
            "~/notes.txt"
        );
    }

    fn context(root: &std::path::Path) -> ToolPreparationContext {
        ToolPreparationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            cwd: root.to_owned(),
            workspace_root: root.to_owned(),
            turn_context: crate::test_turn_context(),
        }
    }

    #[tokio::test]
    async fn distinct_write_content_has_distinct_fingerprint() {
        let root = tempfile::tempdir().expect("root");
        let tool = WriteTool::new(root.path());
        let first = tool
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "write".into(),
                    arguments: serde_json::json!({"filePath":"value.txt","content":"one"}),
                },
            )
            .await
            .expect("first");
        let second = tool
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "write".into(),
                    arguments: serde_json::json!({"filePath":"value.txt","content":"two"}),
                },
            )
            .await
            .expect("second");
        assert_ne!(
            OperationFingerprint::from_prepared_operation(first.operation()),
            OperationFingerprint::from_prepared_operation(second.operation())
        );
    }

    #[tokio::test]
    async fn missing_subtree_write_prepares_without_creating_components() {
        let root = tempfile::tempdir().expect("root");
        let prepared = WriteTool::new(root.path())
            .prepare(
                context(root.path()),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "write".into(),
                    arguments: serde_json::json!({"filePath":"a/b/value.txt","content":"value"}),
                },
            )
            .await
            .expect("prepare");
        assert!(!root.path().join("a").exists());
        assert_eq!(
            prepared.operation().resources()[0].capability,
            PermissionAction::Write
        );
    }
}

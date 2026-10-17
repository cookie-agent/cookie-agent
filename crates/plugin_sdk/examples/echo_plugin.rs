use cookie_agent_plugin_sdk::{PermissionAction, PluginServer, ToolDecl, ToolFailure, ToolOutput};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), cookie_agent_plugin_sdk::PluginError> {
    PluginServer::builder("echo", env!("CARGO_PKG_VERSION"))
        .tool(
            ToolDecl {
                name: "echo".into(),
                collision: Default::default(),
                // A tool may need no permission; this one asks for its own.
                permission: Some(PermissionAction::new("echo").expect("valid action")),
                description: "Echo text back to the model".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                    "additionalProperties": false
                }),
                concurrency: Default::default(),
                result_truncation: Default::default(),
                output: Default::default(),
                background: false,
                background_default: false,
            },
            |_call, arguments| async move {
                let text = arguments["text"]
                    .as_str()
                    .ok_or_else(|| ToolFailure::new("text is required"))?;
                Ok(ToolOutput::success(text))
            },
        )
        .run_stdio()
        .await
}

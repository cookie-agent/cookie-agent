use cookie_agent_plugin_sdk::{PluginServer, ToolDecl, ToolOutput};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), cookie_agent_plugin_sdk::PluginError> {
    PluginServer::builder("echo", env!("CARGO_PKG_VERSION"))
        .tool(
            ToolDecl {
                name: "echo".into(),
                description: "Echo text back to the model".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                    "additionalProperties": false
                }),
                permission_name: "echo".into(),
                primary_resource_param: None,
            },
            |_ctx, request| async move {
                let text = request.arguments["text"]
                    .as_str()
                    .ok_or_else(|| cookie_agent_plugin_sdk::ToolFailure::new("text is required"))?;
                Ok(ToolOutput::success(text))
            },
        )
        .run_stdio()
        .await
}

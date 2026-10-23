//! `list_runtimes`, a tool of the `global` runtime
//! (`docs/site/specs/runtime-delegation.md` section 4): every machine and
//! runtime of the daemon, and the calling session's own attachments, read
//! live at each call, under the `list_runtimes` action.
//!
//! Like the memory tools, the provider is built with the `global` runtime,
//! before the engine exists, and holds the engine through a handle bound
//! once the engine is made.

use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use cookie_agent_protocol::{
    ApprovalResourceSource, PermissionAction, PersistedToolResult as ToolResult,
    PreparedBindingLifetime,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolCompletion, ToolConcurrency,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider,
    ToolResultTruncationPolicy, ToolSpec,
    authoring::{parse_args, prepared_operation, prepared_resource, safe_title, schema},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Engine, runtime::Inner};

/// The provider's ID.
pub(crate) const PROVIDER_ID: &str = "builtin.runtimes";

const LIST_RUNTIMES: &str = "list_runtimes";
/// The policy label of the call's one resource.
const RESOURCE_LABEL: &str = "runtimes";

const DESCRIPTION: &str = "List the daemon's machines and runtimes, as they are now: each runtime's capabilities, whether it is connected or starts when used, its home directory, the ACP agents it offers (external coding agents such as Claude Code; a session runs one on the attached runtime that offers it), and this session's own attachments on it. Name a runtime capability from here in delegate_subagent's `capabilities`, as `<runtime id>/<capability name>`. A runtime that connects later shows up on the next call.";

/// `list_runtimes` takes no arguments.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ListRuntimesArgs {}

/// The `list_runtimes` tool, holding the engine once it is bound.
#[derive(Default)]
pub(crate) struct RuntimesTool {
    engine: OnceLock<Weak<Inner>>,
}

impl RuntimesTool {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Gives the provider its engine. Later calls are ignored.
    pub(crate) fn bind(&self, engine: &Engine) {
        let _ = self.engine.set(Arc::downgrade(&engine.inner));
    }

    fn engine(&self) -> Result<Engine, ToolError> {
        self.engine
            .get()
            .and_then(Weak::upgrade)
            .map(|inner| Engine { inner })
            .ok_or_else(|| ToolError::execution("the engine is gone"))
    }

    pub(crate) fn spec() -> ToolSpec {
        ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: ToolConcurrency::Parallel,
            // Its one output is bounded only by the event size limit.
            result_truncation: ToolResultTruncationPolicy::OptOut,
            name: LIST_RUNTIMES.into(),
            permission_name: Some(PermissionAction::LIST_RUNTIMES.as_str().to_owned()),
            description: DESCRIPTION.into(),
            parameters: schema::<ListRuntimesArgs>(),
        }
    }
}

/// `list_runtimes` as the `global` runtime publishes it, for the tool
/// reference.
#[must_use]
pub fn runtimes_tool_spec() -> ToolSpec {
    RuntimesTool::spec()
}

fn check_name(tool: &str) -> Result<(), ToolError> {
    if tool == LIST_RUNTIMES {
        Ok(())
    } else {
        Err(ToolError::execution(format!(
            "the runtimes provider received another tool, `{tool}`"
        )))
    }
}

#[async_trait]
impl ToolProvider for RuntimesTool {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![Self::spec()])
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        check_name(tool_name)?;
        Ok(Some("list_runtimes"))
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        check_name(name)?;
        Ok((Some("list_runtimes"), Some(RESOURCE_LABEL.to_owned())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        check_name(name)?;
        Ok(String::new())
    }

    fn presentation(&self, call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
        if call.name != LIST_RUNTIMES {
            return cookie_agent_runtime_api::tool_title_only(&call.name);
        }
        crate::base_tools::base_presentation(
            "List runtimes",
            cookie_agent_protocol::ToolHint::SEARCH,
            "",
            Vec::new(),
        )
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        check_name(&call.name)?;
        let engine = self.engine()?;
        let args: ListRuntimesArgs = parse_args(LIST_RUNTIMES, call.arguments)?;
        let resource = prepared_resource(
            PermissionAction::LIST_RUNTIMES,
            "runtimes",
            RESOURCE_LABEL.as_bytes(),
            RESOURCE_LABEL.as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            LIST_RUNTIMES,
            &args,
            vec![(PermissionAction::LIST_RUNTIMES, "list")],
            vec![resource],
            &ctx.context_bytes(),
        )?;
        let normalized =
            serde_json::to_value(&args).map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            normalized,
            None,
            Box::new(RuntimesExecutor { engine }),
        )?
        .with_policy_labels(vec![RESOURCE_LABEL.to_owned()])
    }
}

struct RuntimesExecutor {
    engine: Engine,
}

#[async_trait]
impl PreparedExecutor for RuntimesExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let engine = self.engine;
        let session = context.session;
        let output = tokio::task::spawn_blocking(move || engine.list_runtimes_text(session))
            .await
            .map_err(|error| ToolError::execution(format!("list_runtimes failed: {error}")))?
            .map_err(|error| ToolError::execution(error.to_string()))?;
        Ok(ToolCompletion::single(ToolResult {
            title: safe_title("Listed runtimes"),
            display: crate::base_tools::markdown_display(&format!("```xml\n{output}\n```")),
            output,
            background: None,
            retained_output: None,
            metadata: serde_json::json!({}),
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        }))
    }
}

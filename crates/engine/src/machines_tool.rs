//! `attach_machine` and `detach_machine`, tools of the `global` runtime
//! (`docs/site/specs/machines-on-demand.md` section 3): a run attaches a
//! machine's runtime capability to its own session, or detaches one, under
//! the `attach` action checked on the target runtime (`attach@<runtime>`)
//! with the workspace as resource. `transfer` (section 6) copies files
//! between the machines attached.
//!
//! Like the runtimes tool, the provider is built with the `global` runtime,
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

use cookie_agent_protocol::paths::PathStyle;

use crate::{Engine, runtime::Inner};

mod transfer;

/// The provider's ID.
pub(crate) const PROVIDER_ID: &str = "builtin.machines";

pub(crate) const ATTACH_MACHINE: &str = "attach_machine";
pub(crate) const DETACH_MACHINE: &str = "detach_machine";
pub(crate) use transfer::TRANSFER;

const ATTACH_DESCRIPTION: &str = "Attach a machine to this session: one runtime capability from list_runtimes, written `<runtime id>/<capability name>` (e.g. `labpc/coding`), at `workspace` when the capability takes one. Its tools join this session at once, and the result says where they run: the machine, its home, the workspace, and the workspace's AGENTS.md. A machine offers each tool once: a second directory on a machine already attached is reached by absolute path, or by detaching the first.";
const DETACH_DESCRIPTION: &str = "Detach a machine's runtime capability from this session, written `<runtime id>/<capability name>` as attach_machine took it. Its tools leave the session; refused while one of its background calls still runs.";

/// `attach_machine`'s arguments.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct AttachArgs {
    /// `<runtime_id>/<capability_id>`, from list_runtimes, e.g. `labpc/coding`
    capability: String,
    /// Directory on that runtime's machine; required when the capability
    /// takes a workspace
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
}

/// `detach_machine`'s arguments.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct DetachArgs {
    /// `<runtime_id>/<capability_id>`, as attach_machine took it
    capability: String,
}

/// The machines tools, holding the engine once it is bound.
#[derive(Default)]
pub(crate) struct MachinesTool {
    engine: OnceLock<Weak<Inner>>,
}

impl MachinesTool {
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

    /// The group this provider's tools are announced under when deferred.
    pub const GROUP: cookie_agent_runtime_api::ToolGroup = cookie_agent_runtime_api::ToolGroup {
        name: "machines",
        summary: "The user's machines: attach one to this session to work on it, detach it, or copy files between machines.",
    };

    pub(crate) fn specs() -> Vec<ToolSpec> {
        let spec = |name: &str, description: &str, parameters| ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            // Attachments change one at a time.
            concurrency: ToolConcurrency::Exclusive,
            result_truncation: ToolResultTruncationPolicy::OptOut,
            name: name.into(),
            permission_name: Some(PermissionAction::ATTACH.as_str().to_owned()),
            description: description.into(),
            parameters,
        };
        vec![
            spec(ATTACH_MACHINE, ATTACH_DESCRIPTION, schema::<AttachArgs>()),
            spec(DETACH_MACHINE, DETACH_DESCRIPTION, schema::<DetachArgs>()),
            transfer::spec(),
        ]
    }
}

/// The machines tools as the `global` runtime publishes them, for the tool
/// reference.
#[must_use]
pub fn machines_tool_specs() -> Vec<ToolSpec> {
    MachinesTool::specs()
}

fn wrong_tool(tool: &str) -> ToolError {
    ToolError::execution(format!(
        "the machines provider received another tool, `{tool}`"
    ))
}

/// The `attach` resource of `workspace` on a runtime whose home is `home`
/// and whose platform reads paths in `style`: a leading `~` expanded against
/// the home, as the runtime expands it, and the path labelled
/// ([`cookie_agent_protocol::paths::path_label`]), `.` and `..` collapsed and
/// a trailing `/` dropped, so a rule matches the directory the runtime opens.
/// Without a style the path is left as it is, for the rules to read both
/// ways.
fn attach_label(workspace: &str, home: Option<&str>, style: Option<PathStyle>) -> String {
    let expanded = match home {
        Some(home) if workspace == "~" => home.to_owned(),
        Some(home) => match workspace.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", home.trim_end_matches(['/', '\\'])),
            None => workspace.to_owned(),
        },
        None => workspace.to_owned(),
    };
    match style {
        Some(style) => cookie_agent_protocol::paths::path_label(&expanded, style),
        None => expanded,
    }
}

/// The style the paths of the runtime with ID `runtime` are read in, when
/// it reported its platform.
fn runtime_style(engine: &Engine, runtime: &str) -> Option<PathStyle> {
    engine
        .runtime_platform(runtime)
        .map(|platform| PathStyle::of_platform(&platform))
}

/// The capability and workspace a call names, checked.
fn target(
    capability: &str,
    workspace: Option<String>,
) -> Result<crate::DelegatedCapability, ToolError> {
    crate::runtime::delegation::runtimes::parse_capability(capability, workspace)
        .map_err(ToolError::execution)
}

#[async_trait]
impl ToolProvider for MachinesTool {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn tool_group(&self) -> Option<cookie_agent_runtime_api::ToolGroup> {
        Some(Self::GROUP)
    }

    fn tools_for_session(&self, context: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let mut specs = Self::specs();
        // `transfer` needs a machine to copy to.
        if !self
            .engine()
            .is_ok_and(|engine| engine.session_takes_transfers(context.session))
        {
            specs.retain(|spec| spec.name != TRANSFER);
        }
        Ok(specs)
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        match tool_name {
            ATTACH_MACHINE | DETACH_MACHINE => Ok(Some("attach")),
            TRANSFER => Ok(Some("write")),
            other => Err(wrong_tool(other)),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        let resource = match name {
            ATTACH_MACHINE => {
                let args: AttachArgs = parse_args(ATTACH_MACHINE, arguments.clone())?;
                match args.workspace {
                    Some(workspace) => {
                        let engine = self.engine().ok();
                        let runtime = target(&args.capability, None)
                            .ok()
                            .map(|target| target.runtime_id.to_string());
                        let (home, style) = engine.as_ref().zip(runtime.as_deref()).map_or(
                            (None, None),
                            |(engine, runtime)| {
                                (engine.runtime_home(runtime), runtime_style(engine, runtime))
                            },
                        );
                        attach_label(&workspace, home.as_deref(), style)
                    }
                    None => String::new(),
                }
            }
            DETACH_MACHINE => {
                let _: DetachArgs = parse_args(DETACH_MACHINE, arguments.clone())?;
                String::new()
            }
            TRANSFER => {
                return Ok((
                    Some("write"),
                    Some(transfer::permission_resource(arguments)?),
                ));
            }
            other => return Err(wrong_tool(other)),
        };
        Ok((Some("attach"), Some(resource)))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        match name {
            ATTACH_MACHINE => {
                let args: AttachArgs = parse_args(ATTACH_MACHINE, arguments.clone())?;
                Ok(match args.workspace {
                    Some(workspace) => format!("{} at {workspace}", args.capability),
                    None => args.capability,
                })
            }
            DETACH_MACHINE => {
                let args: DetachArgs = parse_args(DETACH_MACHINE, arguments.clone())?;
                Ok(args.capability)
            }
            TRANSFER => transfer::display_argument(arguments),
            other => Err(wrong_tool(other)),
        }
    }

    fn presentation(&self, call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
        let (title, argument) = match call.name.as_str() {
            ATTACH_MACHINE => (
                "Attach machine",
                self.get_display_argument(ATTACH_MACHINE, &call.arguments)
                    .unwrap_or_default(),
            ),
            DETACH_MACHINE => (
                "Detach machine",
                self.get_display_argument(DETACH_MACHINE, &call.arguments)
                    .unwrap_or_default(),
            ),
            TRANSFER => return transfer::presentation(call),
            other => return cookie_agent_runtime_api::tool_title_only(other),
        };
        crate::base_tools::base_presentation(
            title,
            cookie_agent_protocol::ToolHint::GENERIC,
            &argument,
            Vec::new(),
        )
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let engine = self.engine()?;
        if call.name == TRANSFER {
            return transfer::prepare(engine, ctx, call).await;
        }
        let (executor, normalized, label, runtime, operation_name, args_value): (
            MachinesExecutor,
            serde_json::Value,
            String,
            String,
            &str,
            serde_json::Value,
        ) = match call.name.as_str() {
            ATTACH_MACHINE => {
                let args: AttachArgs = parse_args(ATTACH_MACHINE, call.arguments)?;
                let target = target(&args.capability, args.workspace.clone())?;
                let normalized = serde_json::to_value(&args)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let home = engine.runtime_home(target.runtime_id.as_str());
                let style = runtime_style(&engine, target.runtime_id.as_str());
                let label = args
                    .workspace
                    .as_deref()
                    .map(|workspace| attach_label(workspace, home.as_deref(), style))
                    .unwrap_or_default();
                (
                    MachinesExecutor::Attach {
                        engine,
                        target: target.clone(),
                    },
                    normalized.clone(),
                    label,
                    target.runtime_id.to_string(),
                    "attach",
                    normalized,
                )
            }
            DETACH_MACHINE => {
                let args: DetachArgs = parse_args(DETACH_MACHINE, call.arguments)?;
                let target = target(&args.capability, None)?;
                let normalized = serde_json::to_value(&args)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let workspace = engine
                    .attachment_of_capability(ctx.session, &target)
                    .and_then(|attachment| attachment.workspace)
                    .map(|workspace| {
                        attach_label(
                            &workspace,
                            None,
                            runtime_style(&engine, target.runtime_id.as_str()),
                        )
                    })
                    .unwrap_or_default();
                (
                    MachinesExecutor::Detach {
                        engine,
                        target: target.clone(),
                    },
                    normalized.clone(),
                    workspace,
                    target.runtime_id.to_string(),
                    "detach",
                    normalized,
                )
            }
            other => return Err(wrong_tool(other)),
        };
        let resource = prepared_resource(
            PermissionAction::ATTACH,
            "workspace",
            label.as_bytes(),
            label.as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            &call.name,
            &args_value,
            vec![(PermissionAction::ATTACH, operation_name)],
            vec![resource],
            &ctx.context_bytes(),
        )?;
        PreparedTool::new(operation, normalized, None, Box::new(executor))?
            .with_policy_labels(vec![label])
            // Checked as `attach@<runtime>`.
            .map(|prepared| prepared.with_permission_runtimes(vec![runtime]))
    }
}

enum MachinesExecutor {
    Attach {
        engine: Engine,
        target: crate::DelegatedCapability,
    },
    Detach {
        engine: Engine,
        target: crate::DelegatedCapability,
    },
}

#[async_trait]
impl PreparedExecutor for MachinesExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let (title, output) = match *self {
            Self::Attach { engine, target } => {
                let policy = engine.run_policy(context.run);
                let attached = engine
                    .attach_from_run(
                        context.session,
                        target.runtime_id.as_str(),
                        target.capability.as_str(),
                        target.workspace.clone().map(std::path::PathBuf::from),
                        policy.as_deref(),
                    )
                    .await
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                (
                    format!("Attached {}/{}", target.runtime_id, target.capability),
                    attached_text(&attached),
                )
            }
            Self::Detach { engine, target } => {
                let detached = engine
                    .detach_from_run(
                        context.session,
                        target.runtime_id.as_str(),
                        target.capability.as_str(),
                    )
                    .await
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                (
                    format!("Detached {}/{}", target.runtime_id, target.capability),
                    format!(
                        "Detached `{}/{}` (attachment `{}`{}): its tools have left this session.",
                        detached.runtime_id,
                        detached.capability,
                        detached.name,
                        detached
                            .workspace
                            .as_deref()
                            .map(|workspace| format!(" at {workspace}"))
                            .unwrap_or_default()
                    ),
                )
            }
        };
        Ok(ToolCompletion::single(ToolResult {
            title: safe_title(&title),
            display: crate::base_tools::markdown_display(&output),
            output,
            background: None,
            retained_output: None,
            metadata: serde_json::json!({}),
            attachments: Vec::new(),
            additional_messages: Vec::new(),
            loaded_tools: Vec::new(),
        }))
    }
}

/// What `attach_machine` returns: where the new tools run, which they are,
/// then the workspace's `AGENTS.md`, which later runs get in their context
/// turn.
fn attached_text(attached: &crate::runtime::machines::attach::AttachedMachine) -> String {
    let attachment = &attached.attachment;
    let mut text = format!(
        "Attached `{}/{}` as attachment `{}`.",
        attachment.runtime_id, attachment.capability, attachment.name
    );
    if let Some(machine) = &attached.machine {
        text.push_str(&format!("\nMachine: {machine}"));
        if let Some(platform) = &attached.platform {
            text.push_str(&format!(" ({platform})"));
        }
    }
    if let Some(home) = &attached.home {
        text.push_str(&format!("\nHome: {home}"));
    }
    if let Some(workspace) = &attachment.workspace {
        text.push_str(&format!("\nWorking directory: {workspace}"));
    }
    match attached.tools.as_deref() {
        None => text.push_str("\nIts tools are available now."),
        Some([]) => text.push_str("\nIt brings no tools this session can call."),
        Some(tools) => {
            let (deferred, declared): (Vec<_>, Vec<_>) =
                tools.iter().partition(|tool| tool.deferred.is_some());
            if !declared.is_empty() {
                text.push_str("\nTools you can call there now:");
                for tool in declared {
                    text.push_str(&tool_line(tool));
                }
            }
            if !deferred.is_empty() {
                let groups = deferred
                    .iter()
                    .filter_map(|tool| tool.deferred.as_deref())
                    .map(|group| format!("`{group}`"))
                    .collect::<std::collections::BTreeSet<_>>();
                let plural = |count: usize, word: &str| match count {
                    1 => format!("1 {word}"),
                    count => format!("{count} {word}s"),
                };
                text.push_str(&format!(
                    "\nDeferred until tool_search finds them: {}, in {} {}.",
                    plural(deferred.len(), "tool"),
                    if groups.len() == 1 { "group" } else { "groups" },
                    groups.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    for entry in &attached.agent_md {
        text.push_str(&format!(
            "\n\n<contents from=\"{}\">\n{}\n</contents>",
            entry.source, entry.content
        ));
    }
    text
}

/// One line of `attach_machine`'s tool list.
fn tool_line(tool: &crate::runtime::machines::attach::AttachedTool) -> String {
    let mut line = format!("\n- `{}`", tool.name);
    if let Some(machine) = &tool.machine {
        line.push_str(&format!(" with `machine: \"{machine}\"`"));
    }
    if !tool.summary.is_empty() {
        line.push_str(&format!(": {}", tool.summary));
    }
    line
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AttachmentName, CapabilityName, MachineName, RuntimeAttachment, RuntimeId, RuntimeKind,
    };

    use cookie_agent_protocol::paths::PathStyle;

    use super::{attach_label, attached_text};
    use crate::runtime::machines::attach::{AttachedMachine, AttachedTool};

    fn attached(tools: Option<Vec<AttachedTool>>) -> AttachedMachine {
        AttachedMachine {
            attachment: RuntimeAttachment {
                name: AttachmentName::new("labpc").unwrap(),
                runtime_id: RuntimeId::new("labpc").unwrap(),
                capability: CapabilityName::coding(),
                kind: RuntimeKind::coding(),
                workspace: Some("/srv/api".into()),
            },
            machine: Some(MachineName::new("labpc").unwrap()),
            platform: Some("linux".into()),
            home: None,
            agent_md: Vec::new(),
            tools,
        }
    }

    fn tool(name: &str, machine: Option<&str>, deferred: Option<&str>) -> AttachedTool {
        AttachedTool {
            name: name.into(),
            machine: machine.map(Into::into),
            deferred: deferred.map(Into::into),
            summary: format!("{name} things."),
        }
    }

    #[test]
    fn the_result_lists_callable_tools_and_counts_deferred_ones_by_group() {
        let text = attached_text(&attached(Some(vec![
            tool("bash", Some("labpc"), None),
            tool("gh_issue", None, Some("github")),
            tool("gh_pr", None, Some("github")),
            tool("read", Some("labpc"), None),
            tool("snapshot", None, Some("chrome")),
        ])));
        assert_eq!(
            text,
            "Attached `labpc/coding` as attachment `labpc`.\n\
             Machine: labpc (linux)\n\
             Working directory: /srv/api\n\
             Tools you can call there now:\n\
             - `bash` with `machine: \"labpc\"`: bash things.\n\
             - `read` with `machine: \"labpc\"`: read things.\n\
             Deferred until tool_search finds them: 3 tools, in groups `chrome`, `github`."
        );
    }

    #[test]
    fn an_attach_resource_is_the_directory_the_runtime_opens() {
        let posix = Some(PathStyle::Posix);
        let windows = Some(PathStyle::Windows);
        for (workspace, home, style, label) in [
            ("/srv/projects/../../root", None, posix, "/root"),
            ("/srv/projects/api/", None, posix, "/srv/projects/api"),
            ("/srv/./projects/api", None, posix, "/srv/projects/api"),
            ("//root", None, posix, "/root"),
            ("~/src/api", Some("/home/ada"), posix, "/home/ada/src/api"),
            ("~/src/../../bob", Some("/home/ada/"), posix, "/home/bob"),
            ("~", Some("/home/ada"), posix, "/home/ada"),
            (
                "~/src/api",
                Some(r"C:\Users\ada"),
                windows,
                "C:/Users/ada/src/api",
            ),
            (r"\\server\share\..\x", None, windows, "//server/share/x"),
            // Without a home `~` stays literal, as rule patterns do.
            ("~/src/api", None, posix, "~/src/api"),
            ("~bob/src", Some("/home/ada"), posix, "~bob/src"),
            // Without a platform the rules read it both ways.
            ("//root/../x", None, None, "//root/../x"),
        ] {
            assert_eq!(attach_label(workspace, home, style), label, "{workspace}");
        }
    }

    #[test]
    fn the_result_says_when_every_tool_is_deferred_or_none_is_offered() {
        let text = attached_text(&attached(Some(vec![tool(
            "snapshot",
            None,
            Some("chrome"),
        )])));
        assert!(text.ends_with(
            "/srv/api\nDeferred until tool_search finds them: 1 tool, in group `chrome`."
        ));
        assert!(
            attached_text(&attached(Some(Vec::new())))
                .ends_with("\nIt brings no tools this session can call.")
        );
    }
}

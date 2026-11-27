//! The `forwards` tools (`docs/site/specs/forwards.md` section 7): an agent
//! makes, lists, and deletes forwards, under the `forward` permission
//! action, whose resource is what the call does (`create`, `read`, or
//! `delete`); a listener on an address that is not loopback is also
//! checked as `public`.

use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use cookie_agent_protocol::{
    ApprovalResourceSource, Forward, ForwardCreateParams, ForwardKind, ForwardListen,
    ForwardTarget, PermissionAction, PreparedBindingLifetime,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolCompletion, ToolConcurrency,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider,
    ToolResultTruncationPolicy, ToolSpec,
    authoring::{parse_args, parse_args_strict, prepared_operation, prepared_resource},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Engine,
    cron_tools::text_result,
    runtime::{Inner, forwards_api::ForwardMaker},
};

pub(crate) const PROVIDER_ID: &str = "builtin.forwards";
const CREATE: &str = "forward_create";
const LIST: &str = "forward_list";
const DELETE: &str = "forward_delete";

const CREATE_DESCRIPTION: &str = "Forward a port, so the user or another machine reaches it. `kind` `http` serves a web app of a machine (`to`, such as a dev server on `127.0.0.1:5173`) at the daemon's own address under `/p/<handle>/`, WebSockets included, for the user to open where they open cookie; it runs on the cookie page's origin and can read its storage, so forward only apps you trust. `tcp` or `udp` listens on one machine (`from`, `127.0.0.1` and a free port unless you name them) and connects each connection, or UDP peer, to `to` reached from another, every byte relayed by the daemon, slowly for bulk copies. `machine` names a machine as the machines block does; omitted, the session's workspace machine. A forward lasts `expires_in_hours` (default 24; 0 or less never ends) and is deleted when this conversation is. Pages load from `/p/<handle>/`, not the root: start a dev server with that base path (Vite `--base`, Jupyter `--ServerApp.base_url`, TensorBoard `--path_prefix`), which the result names. Show an http forward to the user with `<attach forward=\"fw_3f9a1c2e\" path=\"/\"/>`.";

const LIST_DESCRIPTION: &str = "List the forwards this conversation can use, its own and the user's, each with its handle, route, status, address, connections, and expiry.";

const DELETE_DESCRIPTION: &str = "Stop a forward by its handle: its listener closes, its connections end, and its share links stop working.";

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct TargetArg {
    /// The machine the target is reached from, as the machines block names it. Default: this session's workspace machine
    #[serde(default, skip_serializing_if = "Option::is_none")]
    machine: Option<String>,
    /// The host reached from that machine. Default: 127.0.0.1
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    port: u16,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ListenArg {
    /// The machine that listens, as the machines block names it. Default: this session's workspace machine
    #[serde(default, skip_serializing_if = "Option::is_none")]
    machine: Option<String>,
    /// The address it listens on. Default: 127.0.0.1; any other is reachable from that machine's network and is always asked about
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bind: Option<String>,
    /// The port. Default: 0, a free one, which the result names
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    /// "http", "tcp", or "udp"
    kind: ForwardKind,
    /// Where connections go
    to: TargetArg,
    /// For tcp and udp: where to listen
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<ListenArg>,
    /// Shown with the forward
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// Hours it lasts. Default 24; 0 or less never ends
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_in_hours: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    /// The forward's handle, such as fw_3f9a1c2e
    forward: String,
}

fn spec(name: &str, description: &str, parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        background: false,
        background_default: false,
        collision: Default::default(),
        output: Default::default(),
        concurrency: if name == LIST {
            ToolConcurrency::Parallel
        } else {
            ToolConcurrency::default()
        },
        result_truncation: ToolResultTruncationPolicy::Bounded,
        name: name.into(),
        permission_name: Some(PermissionAction::FORWARD.as_str().to_owned()),
        description: description.into(),
        parameters,
    }
}

/// The forward tools as the `global` runtime publishes them, built once.
pub(crate) fn specs() -> Vec<ToolSpec> {
    static SPECS: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    SPECS
        .get_or_init(|| {
            vec![
                spec(
                    CREATE,
                    CREATE_DESCRIPTION,
                    cookie_agent_runtime_api::authoring::schema::<CreateArgs>(),
                ),
                spec(
                    LIST,
                    LIST_DESCRIPTION,
                    cookie_agent_runtime_api::authoring::schema::<ListArgs>(),
                ),
                spec(
                    DELETE,
                    DELETE_DESCRIPTION,
                    cookie_agent_runtime_api::authoring::schema::<DeleteArgs>(),
                ),
            ]
        })
        .clone()
}

/// The forward tools, for the tool reference.
#[must_use]
pub fn forward_tool_specs() -> Vec<ToolSpec> {
    specs()
}

fn permission_name(tool: &str) -> Result<Option<&'static str>, ToolError> {
    match tool {
        CREATE | LIST | DELETE => Ok(Some("forward")),
        other => Err(ToolError::execution(format!(
            "the forward provider received another tool, `{other}`"
        ))),
    }
}

/// The resource a call is checked as.
fn resource(tool: &str) -> &'static str {
    match tool {
        CREATE => "create",
        DELETE => "delete",
        _ => "read",
    }
}

fn refused(message: impl Into<String>) -> ToolError {
    ToolError::execution(message.into())
}

enum ForwardCall {
    Create(ForwardCreateParams),
    List,
    Delete(String),
}

/// The forward tools, holding the engine once it is bound.
#[derive(Default)]
pub(crate) struct ForwardTools {
    engine: OnceLock<Weak<Inner>>,
}

impl ForwardTools {
    /// The group this provider's tools are announced under when deferred.
    pub const GROUP: cookie_agent_runtime_api::ToolGroup = cookie_agent_runtime_api::ToolGroup::new(
        "forwards",
        "Forwards: a machine's web app at the daemon's address, or a TCP or UDP port of one machine on another.",
    );

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
}

#[async_trait]
impl ToolProvider for ForwardTools {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn tool_group(&self, _tool: &str) -> Option<cookie_agent_runtime_api::ToolGroup> {
        Some(Self::GROUP)
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(specs())
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        permission_name(tool_name)
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Ok((permission_name(name)?, Some(resource(name).to_owned())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        permission_name(name)?;
        Ok(arguments
            .get("forward")
            .or_else(|| arguments.get("kind"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let engine = self.engine()?;
        permission_name(&call.name)?;
        let mut labels = vec![resource(&call.name).to_owned()];
        let forward_call = match call.name.as_str() {
            CREATE => {
                let args: CreateArgs = parse_args_strict(CREATE, call.arguments.clone())?;
                let default_machine = || engine.default_asset_machine(ctx.session);
                let to = ForwardTarget {
                    machine: match args.to.machine {
                        Some(machine) => machine,
                        None => default_machine()?,
                    },
                    host: args.to.host.unwrap_or_else(|| "127.0.0.1".to_owned()),
                    port: args.to.port,
                };
                let from = match (args.kind, args.from) {
                    (ForwardKind::Http, _) => None,
                    (_, from) => {
                        let from = from.unwrap_or(ListenArg {
                            machine: None,
                            bind: None,
                            port: None,
                        });
                        Some(ForwardListen {
                            machine: match from.machine {
                                Some(machine) => machine,
                                None => default_machine()?,
                            },
                            bind: from.bind.unwrap_or_else(|| "127.0.0.1".to_owned()),
                            port: from.port.unwrap_or(0),
                        })
                    }
                };
                // A listener others reach is asked about whatever the rules
                // allow `create`.
                if let Some(from) = &from
                    && !crate::forwards::is_loopback(&from.bind)
                {
                    labels.push(format!("public {}:{}", from.machine, from.bind));
                }
                ForwardCall::Create(ForwardCreateParams {
                    kind: args.kind,
                    to,
                    from,
                    title: args.title,
                    expires_in_hours: args.expires_in_hours,
                })
            }
            LIST => {
                let _: ListArgs = parse_args(LIST, call.arguments.clone())?;
                ForwardCall::List
            }
            DELETE => {
                let args: DeleteArgs = parse_args(DELETE, call.arguments.clone())?;
                let visible = engine
                    .session_forwards(ctx.session)
                    .iter()
                    .any(|forward| forward.handle == args.forward);
                if !visible {
                    return Err(refused(format!(
                        "there is no forward {} this conversation can use",
                        args.forward
                    )));
                }
                ForwardCall::Delete(args.forward)
            }
            _ => unreachable!("checked by permission_name"),
        };
        let resources = labels
            .iter()
            .map(|label| {
                prepared_resource(
                    PermissionAction::FORWARD,
                    "forward",
                    label.as_bytes(),
                    label.as_bytes(),
                    PreparedBindingLifetime::RestartStable,
                    ApprovalResourceSource::PrimaryOperation,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let operation = prepared_operation(
            &call.name,
            &call.arguments,
            vec![(PermissionAction::FORWARD, resource(&call.name))],
            resources,
            &ctx.context_bytes(),
        )?;
        PreparedTool::new(
            operation,
            call.arguments,
            None,
            Box::new(ForwardExecutor {
                engine,
                call: forward_call,
            }),
        )?
        .with_policy_labels(labels)
    }
}

struct ForwardExecutor {
    engine: Engine,
    call: ForwardCall,
}

#[async_trait]
impl PreparedExecutor for ForwardExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let engine = self.engine;
        match self.call {
            ForwardCall::Create(params) => {
                let forward = engine
                    .create_forward_as(
                        params,
                        &ForwardMaker::Agent {
                            session: context.session,
                            agent: context.turn_context.agent.clone(),
                        },
                    )
                    .map_err(|error| refused(error.to_string()))?;
                // Give the supervisor a moment to listen, so the result
                // names the port.
                let forward = settled(&engine, forward).await;
                Ok(text_result(
                    &format!("Forwarded {}", forward.handle),
                    created_text(&engine, &forward),
                ))
            }
            ForwardCall::List => {
                let forwards = engine.session_forwards(context.session);
                let output = if forwards.is_empty() {
                    "No forwards.".to_owned()
                } else {
                    forwards
                        .iter()
                        .map(|forward| forward_line(&engine, forward))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                Ok(text_result(
                    &format!("Listed {} forwards", forwards.len()),
                    output,
                ))
            }
            ForwardCall::Delete(handle) => {
                let forward = engine
                    .delete_forward_handle(&handle)
                    .map_err(|error| refused(error.to_string()))?;
                Ok(text_result(
                    &format!("Stopped {}", forward.handle),
                    format!("Stopped {} ({}).", forward.handle, forward.route()),
                ))
            }
        }
    }
}

/// `forward` once its status settled, waiting a few seconds at most.
async fn settled(engine: &Engine, forward: Forward) -> Forward {
    let mut changes = engine.subscribe_forward_changes();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let current = engine
            .list_forwards(Default::default())
            .ok()
            .and_then(|listed| {
                listed
                    .forwards
                    .into_iter()
                    .find(|listed| listed.handle == forward.handle)
            })
            .unwrap_or_else(|| forward.clone());
        let settled = current.status != cookie_agent_protocol::ForwardStatus::Waiting
            || current.reason.is_some();
        if settled {
            return current;
        }
        if tokio::time::timeout_at(deadline, changes.changed())
            .await
            .is_err()
        {
            return current;
        }
    }
}

/// Where an `http` forward is opened.
fn http_url(engine: &Engine, forward: &Forward) -> String {
    let path = format!("p/{}/", forward.handle);
    match engine.page_url() {
        Some(base) => format!("{}/{path}", base.trim_end_matches('/')),
        None => format!("/{path} on the daemon's address"),
    }
}

fn until(forward: &Forward) -> String {
    match forward.expires_at {
        Some(at) => format!(
            "until {}",
            at.to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%b %-d %H:%M UTC")
        ),
        None => "with no expiry".to_owned(),
    }
}

fn created_text(engine: &Engine, forward: &Forward) -> String {
    let title = forward
        .title
        .as_ref()
        .map(|title| format!(" {title:?}"))
        .unwrap_or_default();
    let status = match (forward.status, &forward.reason) {
        (cookie_agent_protocol::ForwardStatus::Active, _) => String::new(),
        (status, Some(reason)) => format!(" It is {status:?} for now: {reason}.").to_lowercase(),
        (status, None) => format!(" It is {status:?} for now.").to_lowercase(),
    };
    match forward.kind {
        ForwardKind::Http => format!(
            "Forwarded {}{title} ({}) at {}, {}.{status} Pages load from /p/{}/, not the root: start the server with that base path if its pages load from the root.",
            forward.handle,
            forward.route(),
            http_url(engine, forward),
            until(forward),
            forward.handle
        ),
        _ => format!(
            "Forwarded {}{title} ({}){}, {}.{status}",
            forward.handle,
            forward.route(),
            forward
                .address
                .as_ref()
                .zip(forward.from.as_ref())
                .map(|(address, from)| format!(", listening on {} {address}", from.machine))
                .unwrap_or_default(),
            until(forward)
        ),
    }
}

fn forward_line(engine: &Engine, forward: &Forward) -> String {
    let address = match forward.kind {
        ForwardKind::Http => http_url(engine, forward),
        _ => forward.address.clone().unwrap_or_else(|| "—".to_owned()),
    };
    format!(
        "{} {}({}) {:?} at {address}, {} open, {} in all, {}",
        forward.handle,
        forward
            .title
            .as_ref()
            .map(|title| format!("{title:?} "))
            .unwrap_or_default(),
        forward.route(),
        forward.status,
        forward.connections_open,
        forward.connections_total,
        until(forward)
    )
    .replace("Active", "active")
    .replace("Waiting", "waiting")
    .replace("Failed", "failed")
    .replace("Expired", "expired")
}

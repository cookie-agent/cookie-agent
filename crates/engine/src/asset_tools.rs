//! The asset tools of the `global` runtime (`docs/site/specs/assets.md`
//! section 5): `write_asset`, `read_asset`, `list_assets`, and
//! `delete_asset`, provider `builtin.assets`, group `assets`. An agent shows
//! an asset with `<attach asset="…"/>` in its reply
//! (`docs/site/specs/message-attachments.md`).
//!
//! Every tool is checked under the `asset` action with what the call does
//! as its resource (`read`, `write`, or `delete`); `write_asset` also checks
//! its file as `read@<runtime>` on the machine it reads it from, as
//! `transfer` checks its source. A session reaches its tree's assets and
//! its project's (the shared store for a session in no project); a handle
//! resolves in the tree's first.
//!
//! Like the cron tools, the provider is built with the `global` runtime,
//! before the engine exists, and holds the engine through a handle bound
//! once the engine is made.

use std::{
    collections::HashSet,
    sync::{Arc, OnceLock, Weak},
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    ASSET_MAX_BYTES, ASSET_TITLE_MAX_CHARS, ApprovalResourceSource, Asset, AssetHandle, AssetId,
    AssetScope, AssetStamp, DisplayBlock, Open, PermissionAction,
    PersistedToolResult as ToolResult, PreparedBindingLifetime, RuntimeId, ToolHint, human_bytes,
    runtime_protocol::{FsDirection, FsKind, FsPrepareResult, MAX_FS_CHUNK_BYTES},
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, ResourceScope, SessionToolContext, ToolCall, ToolCompletion,
    ToolConcurrency, ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider,
    ToolResultTruncationPolicy, ToolSpec,
    authoring::{parse_args, prepared_operation, prepared_resource, safe_title},
};
use serde::Deserialize;

use crate::{
    Engine,
    runtime::{Inner, assets::SessionAssetStores},
    runtimes::RuntimeClient,
};

/// The provider's ID.
pub(crate) const PROVIDER_ID: &str = "builtin.assets";

pub(crate) const WRITE: &str = "write_asset";
pub(crate) const READ: &str = "read_asset";
pub(crate) const LIST: &str = "list_assets";
pub(crate) const DELETE: &str = "delete_asset";

/// How many assets an unresolved handle's error lists.
const LISTED_ON_ERROR: usize = 20;
/// The most assets `list_assets` lists.
const LIST_MAX: usize = 100;

const WRITE_DESCRIPTION: &str = "Save a file as an asset, a file you give the user, or replace an asset you saved before. The file is read from a machine of this session: `path` resolves as `read` resolves it, against the machine's working directory or under `~`, and `machine` names the machine as the machines block and `list_runtimes` do; omitted, it is the default machine the machines block marks. Any file up to 25 MiB works. Images, SVG, video, and audio are shown as they are, and a Markdown file (`.md`) as a rendered document, for reports and notes. An HTML page is rendered live, with its scripts and network access, and must be one self-contained file. Anything else is offered as a download. Without `asset` this saves a new asset and returns its handle, such as `asset_3f9a1c2e`. With a handle it replaces that asset's content, and every place it was shown updates, so revise something you saved before by its handle rather than saving it again. `scope` is `session` by default: the asset belongs to this conversation and its subagents, and is deleted with it. Use `project` for something to keep after this conversation, which every session of the project can use. Saving does not show the asset: write `<attach asset=\"asset_3f9a1c2e\"/>` in your reply where it should show.";

const READ_DESCRIPTION: &str = "Read an asset's content by its handle, as `read` reads a file: text with `offset` and `limit` in 1-based lines, an image as an image when you take images, and otherwise only the asset's type and size.";

const LIST_DESCRIPTION: &str = "List the assets you can use: this conversation's and the project's, newest first, each with its handle, title, type, size, scope, and which session last saved it and when. At most 100 are listed.";

const DELETE_DESCRIPTION: &str = "Delete an asset by its handle. Every place it was shown then says it was deleted, and it cannot be brought back.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    #[serde(default)]
    machine: Option<String>,
    #[serde(default)]
    asset: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    scope: Option<AssetScope>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    asset: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    scope: Option<AssetScope>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    asset: String,
}

fn spec(name: &str, description: &str, parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        background: false,
        background_default: false,
        collision: Default::default(),
        output: Default::default(),
        concurrency: if matches!(name, READ | LIST) {
            ToolConcurrency::Parallel
        } else {
            ToolConcurrency::default()
        },
        result_truncation: ToolResultTruncationPolicy::Bounded,
        name: name.into(),
        permission_name: Some(PermissionAction::ASSET.as_str().to_owned()),
        description: description.into(),
        parameters,
    }
}

/// The asset tools as the `global` runtime publishes them, built once.
pub(crate) fn specs() -> Vec<ToolSpec> {
    static SPECS: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    SPECS.get_or_init(build_specs).clone()
}

fn build_specs() -> Vec<ToolSpec> {
    let handle = |description: &str| {
        serde_json::json!({
            "type": "string",
            "description": description,
        })
    };
    let scope = |description: &str| {
        serde_json::json!({
            "type": "string",
            "enum": ["session", "project"],
            "description": description,
        })
    };
    vec![
        spec(
            WRITE,
            WRITE_DESCRIPTION,
            serde_json::json!({
                "type": "object", "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": {"type": "string", "description": "The file to save: relative to the machine's working directory, absolute, or under `~`"},
                    "machine": {"type": "string", "description": "A machine of this session, as the machines block names it. Defaults to the default machine the machines block marks"},
                    "asset": handle("The handle of an asset to replace. Omit it to save a new asset"),
                    "title": {"type": "string", "maxLength": ASSET_TITLE_MAX_CHARS, "description": "Shown with the asset. Defaults to the file name for a new asset, and to the current title when replacing"},
                    "scope": scope("`session` (default): this conversation's, deleted with it. `project`: kept in the project's assets. Only for a new asset"),
                }
            }),
        ),
        spec(
            READ,
            READ_DESCRIPTION,
            serde_json::json!({
                "type": "object", "additionalProperties": false,
                "required": ["asset"],
                "properties": {
                    "asset": handle("The asset's handle"),
                    "offset": {"type": "integer", "minimum": 1, "description": "1-based first line, for text"},
                    "limit": {"type": "integer", "minimum": 1, "description": "Lines, for text"},
                }
            }),
        ),
        spec(
            LIST,
            LIST_DESCRIPTION,
            serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "scope": scope("Only that scope; both when omitted"),
                }
            }),
        ),
        spec(
            DELETE,
            DELETE_DESCRIPTION,
            serde_json::json!({
                "type": "object", "additionalProperties": false,
                "required": ["asset"],
                "properties": {"asset": handle("The asset's handle")}
            }),
        ),
    ]
}

/// The asset tools, for the tool reference.
#[must_use]
pub fn asset_tool_specs() -> Vec<ToolSpec> {
    specs()
}

/// The `asset` resource a tool's calls have.
fn resource(tool: &str) -> Result<&'static str, ToolError> {
    match tool {
        READ | LIST => Ok("read"),
        WRITE => Ok("write"),
        DELETE => Ok("delete"),
        other => Err(ToolError::execution(format!(
            "the assets provider received another tool, `{other}`"
        ))),
    }
}

fn refused(message: impl Into<String>) -> ToolError {
    ToolError::execution(message.into())
}

/// Checks a title or caption the model wrote: one line of at most `max`
/// characters, with no control characters.
fn check_line(value: &str, what: &str, max: usize) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(refused(format!("{what} is empty")));
    }
    if value.chars().count() > max {
        return Err(refused(format!("{what} has more than {max} characters")));
    }
    if value.chars().any(char::is_control) {
        return Err(refused(format!(
            "{what} must be one line without control characters"
        )));
    }
    Ok(())
}

/// The asset tools, holding the engine once it is bound.
#[derive(Default)]
pub(crate) struct AssetTools {
    engine: OnceLock<Weak<Inner>>,
}

impl AssetTools {
    /// The group this provider's tools are announced under when deferred.
    pub const GROUP: cookie_agent_runtime_api::ToolGroup = cookie_agent_runtime_api::ToolGroup::new(
        "assets",
        "Assets: files you give the user, such as charts, diagrams, HTML pages, and reports, shown in the conversation and kept for this session or the project.",
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

/// Runs `call` on `engine`'s asset stores off the async workers, with the
/// stores `session` reaches.
async fn blocking<T: Send + 'static>(
    engine: &Engine,
    session: cookie_agent_protocol::SessionId,
    call: impl FnOnce(&Engine, &SessionAssetStores) -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    let engine = engine.clone();
    tokio::task::spawn_blocking(move || {
        let stores = engine.session_asset_stores(session);
        call(&engine, &stores)
    })
    .await
    .map_err(|error| refused(format!("the asset request did not finish: {error}")))?
}

fn store_failed(error: crate::assets::AssetError) -> ToolError {
    refused(error.to_string())
}

/// The asset `reference` names among the assets `stores` reach: a handle,
/// or an asset's full ID, the tree's first. A reference that names none
/// fails with the assets listed.
pub(crate) fn resolve(
    engine: &Engine,
    stores: &SessionAssetStores,
    reference: &str,
) -> Result<Asset, ToolError> {
    let reference = reference.trim();
    let assets = engine.session_assets(stores).map_err(store_failed)?;
    let found = match reference.parse::<AssetId>() {
        Ok(id) => assets.iter().find(|asset| asset.id == id),
        Err(_) => assets
            .iter()
            .find(|asset| asset.handle.as_str() == reference),
    };
    if let Some(asset) = found {
        return Ok(asset.clone());
    }
    let mut known = assets;
    known.sort_by_key(|asset| std::cmp::Reverse(asset.updated.at));
    let listed = known
        .iter()
        .take(LISTED_ON_ERROR)
        .map(|asset| format!("- {}", asset.summary()))
        .collect::<Vec<_>>();
    Err(refused(if listed.is_empty() {
        format!("there is no asset {reference:?}: this session has no assets yet")
    } else {
        format!(
            "there is no asset {reference:?}; the assets you can use, newest first:\n{}{}",
            listed.join("\n"),
            if known.len() > LISTED_ON_ERROR {
                format!(
                    "\n({} more; call `list_assets`)",
                    known.len() - LISTED_ON_ERROR
                )
            } else {
                String::new()
            }
        )
    }))
}

/// `asset_3f9a1c2e "Loss curves" (text/html, 48 KB, session scope)`.
fn scoped_summary(asset: &Asset) -> String {
    format!(
        "{} {:?} ({}, {}, {} scope)",
        asset.handle,
        asset.title,
        asset.media_type,
        human_bytes(asset.byte_length),
        asset.store.scope().as_str()
    )
}

fn text_result(title: &str, output: String, display: Vec<Open<DisplayBlock>>) -> ToolCompletion {
    ToolCompletion::single(ToolResult {
        title: safe_title(title),
        display: if display.is_empty() {
            crate::base_tools::markdown_display(&format!("```text\n{output}\n```"))
        } else {
            display
        },
        output,
        background: None,
        retained_output: None,
        metadata: serde_json::json!({}),
        attachments: Vec::new(),
        additional_messages: Vec::new(),
        loaded_tools: Vec::new(),
    })
}

/// What a prepared asset call does.
enum AssetCall {
    Write(Box<WriteCall>),
    Read {
        asset: Asset,
        offset: Option<usize>,
        limit: Option<usize>,
    },
    List(Option<AssetScope>),
    Delete(Asset),
}

/// A prepared `write_asset`: the file's route on its machine, and where it
/// goes.
struct WriteCall {
    client: RuntimeClient,
    route: u64,
    armed: bool,
    machine: String,
    prepared: FsPrepareResult,
    stores: SessionAssetStores,
    store: cookie_agent_protocol::AssetStore,
    replace: Option<Asset>,
    title: Option<String>,
}

impl Drop for WriteCall {
    /// A route the call never opened is closed on its runtime.
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            let route = self.route;
            handle.spawn(async move { client.fs_close(route).await });
        }
    }
}

impl Engine {
    /// The machine `write_asset` reads from when it names none: the
    /// machine of the session's first attachment whose runtime takes
    /// transfers.
    pub(crate) fn default_asset_machine(
        &self,
        session: cookie_agent_protocol::SessionId,
    ) -> Result<String, ToolError> {
        let attachments = self
            .session_runtimes(session)
            .map_err(|error| refused(error.to_string()))?;
        attachments
            .iter()
            .filter(|attachment| attachment.runtime_id.as_str() != RuntimeId::GLOBAL)
            .find(|attachment| {
                self.inner
                    .runtimes
                    .instance(attachment)
                    .and_then(|instance| instance.connection())
                    .is_some_and(|connection| connection.client.files())
            })
            .map(|attachment| {
                self.runtime_machine(attachment.runtime_id.as_str())
                    .map_or_else(
                        || attachment.runtime_id.to_string(),
                        |name| name.to_string(),
                    )
            })
            .ok_or_else(|| {
                refused(
                    "this session has no machine to read a file from: attach one with \
                     attach_machine, or name one with `machine`",
                )
            })
    }
}

#[async_trait]
impl ToolProvider for AssetTools {
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
        resource(tool_name)?;
        Ok(Some("asset"))
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Ok((Some("asset"), Some(resource(name)?.to_owned())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        resource(name)?;
        let text = |member: &str| arguments.get(member).and_then(serde_json::Value::as_str);
        Ok(match name {
            WRITE => text("path").unwrap_or_default().to_owned(),
            LIST => text("scope").unwrap_or_default().to_owned(),
            _ => text("asset").unwrap_or_default().to_owned(),
        })
    }

    fn presentation(&self, call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
        let text = |member: &str| {
            call.arguments
                .get(member)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let primary = self
            .get_display_argument(&call.name, &call.arguments)
            .unwrap_or_default();
        let title = match call.name.as_str() {
            WRITE => match (text("asset"), text("title")) {
                (Some(asset), _) => format!("Replace asset {asset}"),
                (None, Some(title)) => format!("Save asset {title}"),
                (None, None) => "Save an asset".to_owned(),
            },
            READ => "Read an asset".to_owned(),
            LIST => "List assets".to_owned(),
            DELETE => "Delete an asset".to_owned(),
            _ => return cookie_agent_runtime_api::tool_title_only(&call.name),
        };
        let mut entries = Vec::new();
        for member in ["machine", "asset", "title", "scope"] {
            if let Some(value) = text(member) {
                entries.push((member, value));
            }
        }
        crate::base_tools::base_presentation(&title, ToolHint::GENERIC, &primary, entries)
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let engine = self.engine()?;
        let label = resource(&call.name)?;
        let session = ctx.session;
        let (asset_call, path) = match call.name.as_str() {
            WRITE => {
                let args: WriteArgs = parse_args(WRITE, call.arguments.clone())?;
                if let Some(title) = &args.title {
                    check_line(title, "title", ASSET_TITLE_MAX_CHARS)?;
                }
                if args.asset.is_some() && args.scope.is_some() {
                    return Err(refused(
                        "`scope` is only for a new asset: a replaced asset keeps its scope",
                    ));
                }
                let reference = args.asset.clone();
                let (stores, replace) = blocking(&engine, session, move |engine, stores| {
                    let replace = reference
                        .map(|reference| resolve(engine, stores, &reference))
                        .transpose()?;
                    Ok((stores.clone(), replace))
                })
                .await?;
                let machine = match &args.machine {
                    Some(machine) => machine.clone(),
                    None => engine.default_asset_machine(session)?,
                };
                let endpoint = engine
                    .transfer_endpoint(session, &machine)
                    .map_err(refused)?;
                let prepared = endpoint
                    .connection
                    .fs_prepare(&args.path, FsDirection::Read)
                    .await
                    .map_err(refused)?;
                let client = endpoint.connection.client.clone();
                let write = WriteCall {
                    route: prepared.route,
                    armed: true,
                    client,
                    machine: endpoint.machine.clone(),
                    store: match &replace {
                        Some(asset) => asset.store.clone(),
                        None => stores.of(args.scope.unwrap_or_default()).clone(),
                    },
                    stores,
                    replace,
                    title: args.title,
                    prepared,
                };
                // Refused before anyone is asked to approve it.
                match write.prepared.kind {
                    None => {
                        return Err(refused(format!(
                            "{} on {} does not exist",
                            write.prepared.path, write.machine
                        )));
                    }
                    Some(FsKind::Folder) => {
                        return Err(refused(format!(
                            "{} on {} is a folder; an asset is one file",
                            write.prepared.path, write.machine
                        )));
                    }
                    Some(_) => {}
                }
                if let Some(size) = write.prepared.size
                    && size > ASSET_MAX_BYTES
                {
                    return Err(ToolError::resource_limit(format!(
                        "{} holds {}, more than an asset's {}",
                        write.prepared.path,
                        human_bytes(size),
                        human_bytes(ASSET_MAX_BYTES)
                    )));
                }
                let path = (
                    write.prepared.label.clone(),
                    write.prepared.binding.clone(),
                    endpoint.scope,
                );
                (AssetCall::Write(Box::new(write)), Some(path))
            }
            READ => {
                let args: ReadArgs = parse_args(READ, call.arguments.clone())?;
                cookie_agent_runtime_api::text_tools::LineWindow::new(args.offset, args.limit)?;
                let reference = args.asset;
                let asset = blocking(&engine, session, move |engine, stores| {
                    resolve(engine, stores, &reference)
                })
                .await?;
                (
                    AssetCall::Read {
                        asset,
                        offset: args.offset,
                        limit: args.limit,
                    },
                    None,
                )
            }
            LIST => {
                let args: ListArgs = parse_args(LIST, call.arguments.clone())?;
                (AssetCall::List(args.scope), None)
            }
            DELETE => {
                let args: DeleteArgs = parse_args(DELETE, call.arguments.clone())?;
                let reference = args.asset;
                let asset = blocking(&engine, session, move |engine, stores| {
                    resolve(engine, stores, &reference)
                })
                .await?;
                (AssetCall::Delete(asset), None)
            }
            _ => unreachable!("checked by resource"),
        };
        let asset_resource = prepared_resource(
            PermissionAction::ASSET,
            "asset",
            label.as_bytes(),
            label.as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let executor = Box::new(AssetExecutor {
            engine,
            session,
            call: asset_call,
        });
        let Some((path_label, binding, scope)) = path else {
            let operation = prepared_operation(
                &call.name,
                &call.arguments,
                vec![(PermissionAction::ASSET, label)],
                vec![asset_resource],
                &ctx.context_bytes(),
            )?;
            return PreparedTool::new(operation, call.arguments, None, executor)?
                .with_policy_labels(vec![label.to_owned()]);
        };
        // The file is checked as `read` on its machine, and the asset as
        // `asset` on the daemon.
        let path_resource = prepared_resource(
            PermissionAction::READ,
            "path",
            path_label.as_bytes(),
            binding.as_bytes(),
            PreparedBindingLifetime::ProcessLocal,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            &call.name,
            &call.arguments,
            vec![
                (PermissionAction::READ, "read"),
                (PermissionAction::ASSET, label),
            ],
            vec![path_resource, asset_resource],
            &ctx.context_bytes(),
        )?;
        PreparedTool::new(operation, call.arguments, None, executor)?
            .with_policy_labels(vec![path_label, label.to_owned()])?
            .with_resource_scopes(vec![
                scope,
                ResourceScope {
                    runtime: RuntimeId::GLOBAL.to_owned(),
                    ..ResourceScope::default()
                },
            ])
    }
}

struct AssetExecutor {
    engine: Engine,
    session: cookie_agent_protocol::SessionId,
    call: AssetCall,
}

#[async_trait]
impl PreparedExecutor for AssetExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        if context.session != self.session {
            return Err(ToolError::operation_changed("session changed"));
        }
        let engine = self.engine;
        let session = self.session;
        match self.call {
            AssetCall::Write(write) => execute_write(engine, context, *write).await,
            AssetCall::Read {
                asset,
                offset,
                limit,
            } => execute_read(engine, context, asset, offset, limit).await,
            AssetCall::List(scope) => {
                let assets = blocking(&engine, session, move |engine, stores| {
                    let mut assets = engine.session_assets(stores).map_err(store_failed)?;
                    assets.retain(|asset| scope.is_none_or(|scope| asset.store.scope() == scope));
                    assets.sort_by_key(|asset| std::cmp::Reverse(asset.updated.at));
                    Ok(assets)
                })
                .await?;
                let tree = engine.root_session_of(session);
                let now = jiff::Timestamp::now();
                let mut lines = assets
                    .iter()
                    .take(LIST_MAX)
                    .map(|asset| list_line(&engine, tree, asset, now))
                    .collect::<Vec<_>>();
                if assets.len() > LIST_MAX {
                    lines.push(format!(
                        "({} more; pass `scope` to list one scope)",
                        assets.len() - LIST_MAX
                    ));
                }
                let output = if lines.is_empty() {
                    "No assets yet.".to_owned()
                } else {
                    lines.join("\n")
                };
                Ok(text_result(
                    &format!(
                        "Listed {} asset{}",
                        assets.len(),
                        if assets.len() == 1 { "" } else { "s" }
                    ),
                    output,
                    Vec::new(),
                ))
            }
            AssetCall::Delete(asset) => {
                let (store, id) = (asset.store.clone(), asset.id);
                engine
                    .assets_blocking(move |_, stores| Ok(stores.delete(&store, id)?))
                    .await
                    .map_err(|error| refused(error.to_string()))?;
                Ok(text_result(
                    &format!("Deleted {}", asset.title),
                    format!("Deleted {} {:?}.", asset.handle, asset.title),
                    Vec::new(),
                ))
            }
        }
    }
}

/// One `list_assets` line: the asset, its scope, and who saved it when.
fn list_line(
    engine: &Engine,
    tree: cookie_agent_protocol::SessionId,
    asset: &Asset,
    now: jiff::Timestamp,
) -> String {
    let saved_by = if engine.root_session_of(asset.updated.session) == tree {
        "this session".to_owned()
    } else {
        match engine.inner.store.get(asset.updated.session) {
            Ok(projection) => match projection.meta.title.as_ref().map(|title| title.as_str()) {
                Some(title) => format!("session {title:?} ({})", asset.updated.session),
                None => format!("session {}", asset.updated.session),
            },
            Err(_) => "a deleted session".to_owned(),
        }
    };
    format!(
        "{} {:?} {} {} · {} · saved by {saved_by} {} ago",
        asset.handle,
        asset.title,
        asset.media_type,
        human_bytes(asset.byte_length),
        asset.store.scope().as_str(),
        cookie_agent_protocol::memory_age(asset.updated.at, now),
    )
}

/// Reads the file of `write` from its machine, at most
/// [`ASSET_MAX_BYTES`].
async fn read_file(write: &mut WriteCall) -> Result<Vec<u8>, String> {
    write.armed = false;
    read_route(&write.client, write.route).await
}

/// Reads the file `client` prepared as `route`, at most
/// [`ASSET_MAX_BYTES`]; the route is used up either way.
pub(crate) async fn read_route(client: &RuntimeClient, route: u64) -> Result<Vec<u8>, String> {
    use base64::Engine as _;

    let opened = client.fs_read_open(route).await?;
    if opened.kind != FsKind::File {
        client.fs_close(opened.handle).await;
        return Err("it is not a file".into());
    }
    let mut bytes = Vec::with_capacity(opened.size.unwrap_or(0).min(ASSET_MAX_BYTES) as usize);
    loop {
        let chunk = match client
            .fs_read_chunk(opened.handle, MAX_FS_CHUNK_BYTES)
            .await
        {
            Ok(chunk) => chunk,
            Err(error) => {
                client.fs_close(opened.handle).await;
                return Err(error);
            }
        };
        let data = base64::engine::general_purpose::STANDARD
            .decode(chunk.data.as_bytes())
            .map_err(|error| format!("the runtime sent data that does not decode: {error}"))?;
        bytes.extend_from_slice(&data);
        if bytes.len() as u64 > ASSET_MAX_BYTES {
            client.fs_close(opened.handle).await;
            return Err(format!(
                "it holds more than {}",
                human_bytes(ASSET_MAX_BYTES)
            ));
        }
        if chunk.summary.is_some() {
            return Ok(bytes);
        }
    }
}

async fn execute_write(
    engine: Engine,
    context: ToolExecutionContext,
    mut write: WriteCall,
) -> Result<ToolCompletion, ToolError> {
    let shown = format!("{} on {}", write.prepared.path, write.machine);
    let bytes = tokio::select! {
        read = read_file(&mut write) => read,
        () = context.cancellation.cancelled() => Err("the call was stopped".into()),
    }
    .map_err(|error| refused(format!("reading {shown} failed: {error}")))?;
    let filename = std::path::Path::new(&write.prepared.path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty());
    let media_type = crate::assets::media_type(filename.as_deref().unwrap_or_default(), &bytes);
    let stamp = AssetStamp {
        at: jiff::Timestamp::now(),
        session: context.session,
        agent: Some(context.turn_context.agent.clone()),
    };
    let store = write.store.clone();
    let replace = write.replace.as_ref().map(|asset| asset.id);
    let stores = write.stores.clone();
    let request = crate::assets::AssetWrite {
        replace,
        title: write.title.clone(),
        filename,
        media_type: media_type.to_owned(),
        bytes,
        stamp,
    };
    let (asset, new) = engine
        .assets_blocking(move |_, assets| {
            // A new asset's handle is clear of both stores the session
            // reaches, so a handle names one asset there.
            let other = if store == stores.session {
                &stores.project
            } else {
                &stores.session
            };
            let avoid = assets
                .list(other)?
                .into_iter()
                .map(|asset| asset.handle)
                .collect::<HashSet<AssetHandle>>();
            Ok(assets.write(&store, request, &avoid)?)
        })
        .await
        .map_err(|error| refused(error.to_string()))?;
    let output = format!(
        "Saved {}, {}.",
        scoped_summary(&asset),
        if new { "new" } else { "replaced" }
    );
    let title = format!("Saved {}", asset.title);
    let display = crate::base_tools::markdown_display(&output);
    Ok(text_result(&title, output, display))
}

async fn execute_read(
    engine: Engine,
    context: ToolExecutionContext,
    asset: Asset,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<ToolCompletion, ToolError> {
    use cookie_agent_runtime_api::text_tools::{LineWindow, MAX_PAGE_BYTES, page_text};

    let header = scoped_summary(&asset);
    let (store, id, total) = (asset.store.clone(), asset.id, asset.byte_length);
    let (bytes, asset) = engine
        .assets_blocking(move |_, stores| Ok(stores.read(&store, id, 0, total)?))
        .await
        .map_err(|error| refused(error.to_string()))?;
    let accepted = context.turn_context.input_media.iter().find(|media| {
        media.delivery == cookie_agent_protocol::runtime_protocol::MediaDelivery::ToolResult
            && media.mime_types.iter().any(|accepted| {
                cookie_agent_protocol::media_sniff::mime_type_matches(accepted, &asset.media_type)
            })
            && bytes.len() as u64 <= media.max_bytes
    });
    if accepted.is_some()
        && let Ok(mime_type) = cookie_agent_protocol::MimeType::new(asset.media_type.clone())
    {
        let (reference, sha256) = context.artifacts.retain(context.session, &bytes)?;
        let attachment = cookie_agent_protocol::ToolAttachment {
            mime_type,
            filename: asset.filename.clone(),
            byte_length: bytes.len() as u64,
            sha256: cookie_agent_protocol::Sha256Digest::new(sha256)
                .map_err(|error| refused(error.to_string()))?,
            reference,
        };
        return Ok(ToolCompletion::single(ToolResult {
            title: safe_title(format!("Read {}", asset.title)),
            display: vec![Open::Known(DisplayBlock::Media {
                artifact: attachment.artifact_ref(),
                alt: Some(asset.title.clone()),
            })],
            output: format!("{header}: attached."),
            background: None,
            retained_output: None,
            metadata: serde_json::json!({}),
            attachments: vec![attachment],
            additional_messages: Vec::new(),
            loaded_tools: Vec::new(),
        }));
    }
    let text = std::str::from_utf8(&bytes)
        .ok()
        .filter(|text| !text.contains('\0'));
    let Some(text) = text else {
        return Ok(text_result(
            &format!("Read {}", asset.title),
            format!(
                "{header}: not text, and not a kind you take, so only its type and size are given."
            ),
            Vec::new(),
        ));
    };
    let page = page_text(text, LineWindow::new(offset, limit)?, MAX_PAGE_BYTES);
    page.check_in_range(asset.handle.as_str())?;
    let output = format!("{header}\n{}", page.text_with_continuation());
    let display = vec![Open::Known(
        page.excerpt_block(
            asset
                .filename
                .clone()
                .unwrap_or_else(|| asset.handle.to_string()),
            None,
        ),
    )];
    Ok(text_result(
        &format!("Read {}", asset.title),
        output,
        display,
    ))
}

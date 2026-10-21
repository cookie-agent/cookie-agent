//! The memory tools of the `global` runtime (`docs/site/specs/memory.md`
//! section 8): `memory_search` and `memory_read` under the `memory` action,
//! `memory_write`, `memory_edit`, and `memory_delete` under `memory_write`,
//! each with the memory URL as resource. `memory_search` is checked per
//! result instead (section 9.3), so it declares no permission.
//! `memory_delete` deletes `machine://` and `project://` memories; a
//! `user://` or `agent://` memory cannot be deleted by a model, so the call
//! clears its body and keeps its frontmatter (section 16.6).
//!
//! The provider is built with the `global` runtime, before the engine
//! exists, and holds the engine through a handle bound once the engine is
//! made. Composition publishes its tools only to runs that have memory
//! ([`crate::runtime::memory_runs`]); a delegated run's writes to anything
//! but its own agent's file are refused while preparing, before rules are
//! consulted.

use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use cookie_agent_protocol::{
    AgentId, ApprovalResourceSource, DisplayBlock, Open, PermissionAction,
    PersistedToolResult as ToolResult, PreparedBindingLifetime,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolCompletion, ToolConcurrency,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
    authoring::{parse_args, prepared_operation, prepared_resource, safe_title, schema},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Engine,
    memory::{
        file::{MemoryDocument, MemoryRevision},
        search::SearchHit,
        store::{ClearedBody, Origin, StoreError},
        url::{MemoryLayer, MemoryUrl},
    },
    runtime::Inner,
};

/// The provider's ID; composition recognizes memory tools by it.
pub(crate) const PROVIDER_ID: &str = "builtin.memory";

const SEARCH: &str = "memory_search";
const READ: &str = "memory_read";
const WRITE: &str = "memory_write";
const EDIT: &str = "memory_edit";
const DELETE: &str = "memory_delete";

/// The longest query, in characters.
const MAX_QUERY_CHARS: usize = 200;
/// The most results one search returns, and how many it returns by default.
const MAX_LIMIT: u32 = 25;
const DEFAULT_LIMIT: u32 = 10;
/// The longest matching body line a search result shows, in characters;
/// a longer one is cut and ends with `…`.
pub(crate) const MAX_HIT_LINE_CHARS: usize = 200;

/// The memory tools, holding the engine once it is bound.
#[derive(Default)]
pub(crate) struct MemoryTools {
    engine: OnceLock<Weak<Inner>>,
}

impl MemoryTools {
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
            .ok_or_else(|| ToolError::execution("memory is not available: the engine is gone"))
    }

    /// Every memory tool as the engine publishes it, for the tool reference.
    #[must_use]
    pub(crate) fn specs() -> Vec<ToolSpec> {
        vec![
            spec(SEARCH, None, SEARCH_DESCRIPTION, schema::<SearchArgs>()),
            spec(READ, Some("memory"), READ_DESCRIPTION, schema::<ReadArgs>()),
            spec(
                WRITE,
                Some("memory_write"),
                WRITE_DESCRIPTION,
                schema::<WriteArgs>(),
            ),
            spec(
                EDIT,
                Some("memory_write"),
                EDIT_DESCRIPTION,
                schema::<EditArgs>(),
            ),
            spec(
                DELETE,
                Some("memory_write"),
                DELETE_DESCRIPTION,
                schema::<DeleteArgs>(),
            ),
        ]
    }
}

/// Every memory tool as the `global` runtime publishes it, for the tool
/// reference.
#[must_use]
pub fn memory_tool_specs() -> Vec<ToolSpec> {
    MemoryTools::specs()
}

const SEARCH_DESCRIPTION: &str = "Search memory: notes saved in earlier sessions about the user, agents, machines, and projects. Terms are matched case-insensitively against each memory's keywords, name, description, and body; results are ranked by keyword hits first. Results show each memory's URL, description, keywords, and up to three matching lines; a result you need approval to read shows only its URL and description. Search for the project a task belongs to by repository name, remote URL, or directory before starting work. `layer` narrows the search to `user`, `agent`, `machine`, or `project` memories.";
const READ_DESCRIPTION: &str = "Read one memory whole by its URL (user://<category>, agent://<agent>, machine://<machine>/<name>, or project://<project>/<name>). Returns its description, keywords, body, and revision; send the revision to memory_write, memory_edit, or memory_delete to change it.";
const WRITE_DESCRIPTION: &str = "Create or replace one memory. Without `revision` the memory must not exist yet; to replace one, send the revision memory_read returned. Writing the first memory of a project (project://<project>/<name>) creates the project. user:// categories cannot be created, only changed; agent://<agent> may be created only for an existing agent. Give each memory a one-line description and the keywords a later search would use; for a project, include what identifies it across machines: repository names, remote URLs, and `<machine>:<path>` strings for each place the work lives. Never save secrets, credentials, or personal data the user did not ask you to keep. Prefer editing an existing memory over adding a near-duplicate.";
const EDIT_DESCRIPTION: &str = "Replace the one occurrence of `old` in a memory's body with `new`, at the revision memory_read returned. Fails when `old` occurs zero or several times; include more context to make it unique. Never save secrets.";
const DELETE_DESCRIPTION: &str = "Delete one memory at the revision memory_read returned. machine:// and project:// memories are deleted; a project disappears with its last memory. user:// and agent:// memories cannot be deleted: for those, the call clears the body and keeps the file with its description and keywords, so the category or agent file stays.";

fn spec(
    name: &str,
    permission_name: Option<&str>,
    description: &str,
    parameters: serde_json::Value,
) -> ToolSpec {
    let reads = matches!(name, SEARCH | READ);
    ToolSpec {
        background: false,
        background_default: false,
        collision: Default::default(),
        output: Default::default(),
        concurrency: if reads {
            ToolConcurrency::Parallel
        } else {
            ToolConcurrency::Exclusive
        },
        result_truncation: Default::default(),
        name: name.into(),
        permission_name: permission_name.map(str::to_owned),
        description: description.into(),
        parameters,
    }
}

/// The layer a search narrows to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LayerArg {
    User,
    Agent,
    Machine,
    Project,
}

impl LayerArg {
    const fn layer(self) -> MemoryLayer {
        match self {
            Self::User => MemoryLayer::User,
            Self::Agent => MemoryLayer::Agent,
            Self::Machine => MemoryLayer::Machine,
            Self::Project => MemoryLayer::Project,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    /// Search terms, separated by whitespace: 1 to 200 characters.
    #[schemars(length(min = 1, max = 200))]
    query: String,
    /// Only search this layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    layer: Option<LayerArg>,
    /// The most results to return: 1 to 25. Defaults to 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 25))]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    /// The memory URL.
    url: String,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    /// The memory URL.
    url: String,
    /// One line, 1 to 200 characters, shown in indexes and search results.
    description: String,
    /// Up to 32 keywords of 1 to 64 characters a later search would use.
    keywords: Vec<String>,
    /// The memory's Markdown body; may be empty.
    body: String,
    /// The revision memory_read returned, to replace an existing memory;
    /// leave it out to create one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    /// The memory URL.
    url: String,
    /// Text that occurs exactly once in the body.
    old: String,
    /// Its replacement.
    new: String,
    /// The revision memory_read returned.
    revision: String,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    /// The memory URL.
    url: String,
    /// The revision memory_read returned.
    revision: String,
}

/// One checked call.
enum MemoryCall {
    Search {
        query: String,
        layer: Option<MemoryLayer>,
        limit: usize,
    },
    Read {
        url: MemoryUrl,
    },
    Write {
        url: MemoryUrl,
        document: MemoryDocument,
        revision: Option<MemoryRevision>,
    },
    Edit {
        url: MemoryUrl,
        old: String,
        new: String,
        revision: MemoryRevision,
    },
    Delete {
        url: MemoryUrl,
        revision: MemoryRevision,
    },
}

impl MemoryCall {
    /// The call's URL; `None` for a search.
    fn url(&self) -> Option<&MemoryUrl> {
        match self {
            Self::Search { .. } => None,
            Self::Read { url }
            | Self::Write { url, .. }
            | Self::Edit { url, .. }
            | Self::Delete { url, .. } => Some(url),
        }
    }

    const fn writes(&self) -> bool {
        matches!(
            self,
            Self::Write { .. } | Self::Edit { .. } | Self::Delete { .. }
        )
    }
}

fn parse_url(value: &str) -> Result<MemoryUrl, ToolError> {
    MemoryUrl::parse(value).map_err(|error| ToolError::execution(error.to_string()))
}

fn parse_revision(value: &str) -> Result<MemoryRevision, ToolError> {
    MemoryRevision::parse(value).ok_or_else(|| {
        ToolError::execution(format!(
            "`{value}` is not a memory revision; send the 16 hex digits memory_read returned"
        ))
    })
}

/// Checks a call's arguments: strict members, a valid URL, revision, and
/// memory, and the bounds of a search.
fn check(name: &str, arguments: serde_json::Value) -> Result<MemoryCall, ToolError> {
    match name {
        SEARCH => {
            let args: SearchArgs = parse_args(SEARCH, arguments)?;
            let length = args.query.chars().count();
            if args.query.trim().is_empty() || length > MAX_QUERY_CHARS {
                return Err(ToolError::execution(format!(
                    "the query must have 1 to {MAX_QUERY_CHARS} characters and a search term; it has {length}"
                )));
            }
            let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
            if !(1..=MAX_LIMIT).contains(&limit) {
                return Err(ToolError::execution(format!(
                    "limit must be 1 to {MAX_LIMIT}; it is {limit}"
                )));
            }
            Ok(MemoryCall::Search {
                query: args.query,
                layer: args.layer.map(LayerArg::layer),
                limit: limit as usize,
            })
        }
        READ => {
            let args: ReadArgs = parse_args(READ, arguments)?;
            Ok(MemoryCall::Read {
                url: parse_url(&args.url)?,
            })
        }
        WRITE => {
            let args: WriteArgs = parse_args(WRITE, arguments)?;
            let url = parse_url(&args.url)?;
            let document = MemoryDocument::new(args.description, args.keywords, args.body)
                .map_err(|reason| {
                    ToolError::execution(format!("{url} would not be a valid memory: {reason}"))
                })?;
            Ok(MemoryCall::Write {
                url,
                document,
                revision: args.revision.as_deref().map(parse_revision).transpose()?,
            })
        }
        EDIT => {
            let args: EditArgs = parse_args(EDIT, arguments)?;
            Ok(MemoryCall::Edit {
                url: parse_url(&args.url)?,
                old: args.old,
                new: args.new,
                revision: parse_revision(&args.revision)?,
            })
        }
        DELETE => {
            let args: DeleteArgs = parse_args(DELETE, arguments)?;
            Ok(MemoryCall::Delete {
                url: parse_url(&args.url)?,
                revision: parse_revision(&args.revision)?,
            })
        }
        other => Err(ToolError::execution(format!(
            "the memory provider received another tool, `{other}`"
        ))),
    }
}

/// A delegated run writes only its own agent's file, whatever its rules
/// say (sections 6 and 9).
fn check_delegated_write(url: &MemoryUrl, agent: &AgentId) -> Result<(), ToolError> {
    match url {
        MemoryUrl::Agent { agent: target } if target == agent => Ok(()),
        _ => Err(ToolError::execution(format!(
            "a delegated run may write only its own agent's memory, agent://{agent}; {url} is read-only here"
        ))),
    }
}

fn permission_name(tool: &str) -> Result<Option<&'static str>, ToolError> {
    match tool {
        SEARCH => Ok(None),
        READ => Ok(Some("memory")),
        WRITE | EDIT | DELETE => Ok(Some("memory_write")),
        other => Err(ToolError::execution(format!(
            "the memory provider received another tool, `{other}`"
        ))),
    }
}

/// The URL or query a call names, for its header.
fn display_argument(name: &str, arguments: &serde_json::Value) -> Option<String> {
    let member = if name == SEARCH { "query" } else { "url" };
    arguments
        .get(member)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[async_trait]
impl ToolProvider for MemoryTools {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(Self::specs())
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        permission_name(tool_name)
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        let permission = permission_name(name)?;
        let call = check(name, arguments.clone())?;
        Ok((permission, call.url().map(ToString::to_string)))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        permission_name(name)?;
        display_argument(name, arguments)
            .ok_or_else(|| ToolError::execution("the call names no URL or query"))
    }

    fn presentation(&self, call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
        let Some(primary) = display_argument(&call.name, &call.arguments) else {
            return cookie_agent_runtime_api::tool_title_only(&call.name);
        };
        let text = |member: &str| {
            call.arguments
                .get(member)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let (title, hint, entries) = match call.name.as_str() {
            SEARCH => {
                let mut entries = vec![("query", primary.clone())];
                entries.extend(text("layer").map(|layer| ("layer", layer)));
                entries.extend(
                    call.arguments
                        .get("limit")
                        .and_then(serde_json::Value::as_u64)
                        .map(|limit| ("limit", limit.to_string())),
                );
                (
                    "Search memory".to_owned(),
                    cookie_agent_protocol::ToolHint::SEARCH,
                    entries,
                )
            }
            READ => (
                format!("Read memory {primary}"),
                cookie_agent_protocol::ToolHint::FILE_READ,
                Vec::new(),
            ),
            WRITE => {
                let mut entries = vec![("url", primary.clone())];
                entries.extend(text("description").map(|value| ("description", value)));
                entries.extend(
                    call.arguments
                        .get("keywords")
                        .and_then(serde_json::Value::as_array)
                        .map(|keywords| {
                            let keywords = keywords
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .collect::<Vec<_>>();
                            ("keywords", keywords.join(", "))
                        }),
                );
                entries.extend(text("body").map(|value| ("body", value)));
                entries.extend(text("revision").map(|value| ("revision", value)));
                (
                    format!("Write memory {primary}"),
                    cookie_agent_protocol::ToolHint::FILE_WRITE,
                    entries,
                )
            }
            EDIT => {
                let mut entries = vec![("url", primary.clone())];
                entries.extend(text("old").map(|value| ("old", value)));
                entries.extend(text("new").map(|value| ("new", value)));
                (
                    format!("Edit memory {primary}"),
                    cookie_agent_protocol::ToolHint::FILE_EDIT,
                    entries,
                )
            }
            DELETE => (
                format!("Delete memory {primary}"),
                cookie_agent_protocol::ToolHint::FILE_WRITE,
                Vec::new(),
            ),
            _ => return cookie_agent_runtime_api::tool_title_only(&call.name),
        };
        crate::base_tools::base_presentation(&title, hint, &primary, entries)
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let engine = self.engine()?;
        let memory_call = check(&call.name, call.arguments.clone())?;
        if memory_call.writes()
            && engine
                .session_is_delegated(ctx.session)
                .map_err(ToolError::execution)?
            && let Some(url) = memory_call.url()
        {
            check_delegated_write(url, &ctx.turn_context.agent)?;
        }
        let (action, operation_name) = match &memory_call {
            MemoryCall::Search { .. } => (PermissionAction::MEMORY, "search"),
            MemoryCall::Read { .. } => (PermissionAction::MEMORY, "read"),
            MemoryCall::Write { .. } => (PermissionAction::MEMORY_WRITE, "write"),
            MemoryCall::Edit { .. } => (PermissionAction::MEMORY_WRITE, "edit"),
            MemoryCall::Delete { .. } => (PermissionAction::MEMORY_WRITE, "delete"),
        };
        let label = memory_call
            .url()
            .map_or_else(|| "search".to_owned(), ToString::to_string);
        let resource = prepared_resource(
            action.clone(),
            "memory",
            label.as_bytes(),
            label.as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            &call.name,
            &call.arguments,
            vec![(action, operation_name)],
            vec![resource],
            &ctx.context_bytes(),
        )?;
        let prepared = PreparedTool::new(
            operation,
            call.arguments,
            None,
            Box::new(MemoryExecutor {
                engine,
                call: memory_call,
            }),
        )?;
        prepared.with_policy_labels(vec![label])
    }
}

struct MemoryExecutor {
    engine: Engine,
    call: MemoryCall,
}

/// Runs store work off the async workers: it reads files and may wait for
/// the memory lock.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ToolError::execution(format!("memory task failed: {error}")))
}

fn store_error(error: StoreError) -> ToolError {
    ToolError::execution(error.to_string())
}

fn result(
    title: String,
    output: String,
    display: Vec<Open<DisplayBlock>>,
    metadata: serde_json::Value,
) -> ToolCompletion {
    ToolCompletion::single(ToolResult {
        title: safe_title(title),
        output,
        display,
        background: None,
        retained_output: None,
        metadata,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    })
}

fn key_value(entries: Vec<(&str, String)>) -> Vec<Open<DisplayBlock>> {
    cookie_agent_protocol::fit_result_display(vec![Open::Known(DisplayBlock::key_value(entries))])
}

#[async_trait]
impl PreparedExecutor for MemoryExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let engine = self.engine;
        let root = engine.memory_root().clone();
        let max_file_bytes = engine.memory_limits().max_file_bytes;
        match self.call {
            MemoryCall::Search {
                query,
                layer,
                limit,
            } => {
                let caller = engine
                    .memory_caller(context.run)
                    .map_err(ToolError::execution)?;
                let hits = blocking(move || {
                    root.search(&query, layer, limit, max_file_bytes, |url| {
                        caller.visibility(url)
                    })
                })
                .await?
                .map_err(|error| ToolError::execution(error.to_string()))?;
                let output = search_output(&hits);
                let count = hits.len();
                Ok(result(
                    format!(
                        "Found {count} {}",
                        if count == 1 { "memory" } else { "memories" }
                    ),
                    output.clone(),
                    crate::base_tools::markdown_display(&output),
                    serde_json::json!({
                        "results": hits.iter().map(|hit| hit.url.to_string()).collect::<Vec<_>>(),
                    }),
                ))
            }
            MemoryCall::Read { url } => {
                let read = {
                    let url = url.clone();
                    blocking(move || root.read(&url, max_file_bytes))
                        .await?
                        .map_err(store_error)?
                };
                let revision = read.entry.revision.to_string();
                let mut output = format!("url: {url}\nrevision: {revision}\n");
                if let Err(reason) = &read.entry.document {
                    output.push_str(&format!(
                        "This memory is invalid ({reason}); search and the snapshot skip it until it is fixed.\n"
                    ));
                }
                output.push('\n');
                output.push_str(&read.text);
                let body = read
                    .entry
                    .document
                    .as_ref()
                    .map_or(read.text.as_str(), MemoryDocument::body);
                Ok(result(
                    format!("Read memory {url}"),
                    output,
                    crate::base_tools::markdown_display(body),
                    serde_json::json!({ "url": url.to_string(), "revision": revision }),
                ))
            }
            MemoryCall::Write {
                url,
                document,
                revision,
            } => {
                let created = revision.is_none();
                let agents = engine.clone();
                let target = url.clone();
                let new_revision = blocking(move || {
                    root.write(
                        &target,
                        &document.render(),
                        revision.as_ref(),
                        Origin::Model,
                        |agent| agents.agent_registered(agent),
                        max_file_bytes,
                    )
                })
                .await?
                .map_err(store_error)?
                .to_string();
                let verb = if created { "Created" } else { "Replaced" };
                Ok(result(
                    format!("{verb} memory {url}"),
                    format!("{verb} {url}; its revision is now {new_revision}."),
                    key_value(vec![
                        ("url", url.to_string()),
                        ("revision", new_revision.clone()),
                    ]),
                    serde_json::json!({ "url": url.to_string(), "revision": new_revision }),
                ))
            }
            MemoryCall::Edit {
                url,
                old,
                new,
                revision,
            } => {
                let target = url.clone();
                let new_revision =
                    blocking(move || root.edit(&target, &old, &new, &revision, max_file_bytes))
                        .await?
                        .map_err(store_error)?
                        .to_string();
                Ok(result(
                    format!("Edited memory {url}"),
                    format!("Edited {url}; its revision is now {new_revision}."),
                    key_value(vec![
                        ("url", url.to_string()),
                        ("revision", new_revision.clone()),
                    ]),
                    serde_json::json!({ "url": url.to_string(), "revision": new_revision }),
                ))
            }
            MemoryCall::Delete { url, revision } => {
                if matches!(url.layer(), MemoryLayer::User | MemoryLayer::Agent) {
                    let target = url.clone();
                    let cleared =
                        blocking(move || root.clear_body(&target, &revision, max_file_bytes))
                            .await?
                            .map_err(store_error)?;
                    let (title, output, revision) = cleared_result(&url, cleared);
                    return Ok(result(
                        title,
                        output,
                        key_value(vec![
                            ("url", url.to_string()),
                            ("revision", revision.clone()),
                        ]),
                        serde_json::json!({ "url": url.to_string(), "revision": revision }),
                    ));
                }
                let target = url.clone();
                blocking(move || root.delete(&target, &revision, Origin::Model))
                    .await?
                    .map_err(store_error)?;
                Ok(result(
                    format!("Deleted memory {url}"),
                    format!("Deleted {url}."),
                    key_value(vec![("url", url.to_string())]),
                    serde_json::json!({ "url": url.to_string() }),
                ))
            }
        }
    }
}

/// The title and text of a delete that cleared a `user://` or `agent://`
/// memory's body instead, and the revision after it. The text says plainly
/// that the memory still exists, so the model does not take it for gone.
fn cleared_result(url: &MemoryUrl, cleared: ClearedBody) -> (String, String, String) {
    match cleared {
        ClearedBody::Cleared(revision) => (
            format!("Cleared memory {url}"),
            format!(
                "{url} cannot be deleted, so its content was cleared; its description and keywords stay. New revision: {revision}."
            ),
            revision.to_string(),
        ),
        ClearedBody::AlreadyEmpty(revision) => (
            format!("Memory {url} already empty"),
            format!(
                "{url} cannot be deleted, and its content is already empty, so nothing changed. Its revision is still {revision}."
            ),
            revision.to_string(),
        ),
    }
}

/// A body line as a result shows it: cut after
/// [`MAX_HIT_LINE_CHARS`] characters.
fn hit_line(line: &str) -> String {
    match line.char_indices().nth(MAX_HIT_LINE_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}

/// The search results as the model reads them: one paragraph per memory,
/// in rank order.
fn search_output(hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return "No memories match.".to_owned();
    }
    hits.iter()
        .map(|hit| {
            let mut text = format!("{}: {}", hit.url, hit.description);
            match &hit.detail {
                Some(detail) => {
                    if !detail.keywords.is_empty() {
                        text.push_str(&format!(" [keywords: {}]", detail.keywords.join(", ")));
                    }
                    for line in &detail.lines {
                        text.push_str(&format!("\n  | {}", hit_line(line)));
                    }
                }
                None => text.push_str("\n  (reading it needs approval)"),
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests;

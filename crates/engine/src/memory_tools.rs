//! The memory tools of the `global` runtime (`docs/site/specs/memory.md`
//! section 8, as section 16.9 amends it): `memory_search` and
//! `memory_read` under the `memory` action, `memory_write`, `memory_edit`,
//! and `memory_delete` under `memory_write`, each with the memory URL as
//! resource. `memory_search` is checked per result instead (section 9.3),
//! so it declares no permission. Writes carry no revision: an edit applies
//! when its `old` text occurs once, and a replacing write needs the session
//! to have seen the memory as it is (section 16.9.8). A delete deletes.
//!
//! The provider also contributes the `<tool_instructions
//! provider="builtin.memory">` section (section 16.9.1): part A, what the
//! snapshot includes and when to search, for every run offered a memory
//! tool, and part B, the rubric, for a run also offered the write tools.
//!
//! The provider is built with the `global` runtime, before the engine
//! exists, and holds the engine through a handle bound once the engine is
//! made. Composition publishes its tools only to runs that have memory,
//! and its write tools only to runs that can both read and write some
//! memory ([`crate::runtime::memory_runs`]); the permission rules alone
//! decide what a run may change (section 16.9.11).

use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use cookie_agent_protocol::{
    ApprovalResourceSource, DiffAsOf, DisplayBlock, Open, PermissionAction,
    PersistedToolResult as ToolResult, PreparedBindingLifetime,
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolCompletion, ToolConcurrency,
    ToolError, ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec,
    authoring::{parse_args, prepared_operation, prepared_resource, safe_title, schema},
    display::{TextChange, fit_executed_block, text_diff},
};
use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Engine,
    memory::{
        file::MemoryDocument,
        format,
        search::SearchHit,
        similar::NearMemory,
        store::{StoreError, WriteMode},
        url::{MemoryLayer, MemoryUrl},
    },
    runtime::{
        Inner,
        memory_runs::{MemoryAccess, MemoryCaller},
    },
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
/// The longest `reason` a write that insists it is new may give.
const MAX_REASON_CHARS: usize = 200;

/// Part A of the memory section for a root run (section 16.9.1).
const PART_A_ROOT: &str = "Memory related to this session is included automatically at the start of each session: the user's memory, your own agent memory, the session's project, and the machines its runtimes run on. Use memory_search for anything else, such as another project or a machine this session doesn't use.";

/// Part A of the memory section for a delegated run.
const PART_A_DELEGATED: &str = "Your own agent memory is included automatically at the start of each run. Use memory_search for anything else.";

/// Part B of the memory section, the rubric, for a run offered the write
/// tools. The approval reviewer gets it too (section 16.9.12).
pub(crate) const PART_B: &str = concat!(
    "You manage your own memory. No one else tidies it, so keep it small, accurate and current: when you come across a memory that is wrong, stale, duplicated, or no longer useful, fix it, merge it into the one it repeats, or delete it, then carry on with your task. A memory earns its place only while a later session would miss it.\n",
    "\n",
    "Memory related to this session, the memory shown to you above, is yours to maintain. Do not write, edit or delete memory unrelated to it unless what it says is stale or wrong, or the user explicitly asks you to.\n",
    "\n",
    "Memory is for what a later session cannot recover by looking at the project or running a command, and that will still be true and useful across many sessions. Before saving, ask:\n",
    "1. Could a later session re-derive it by reading files, listing a directory, running a command, or checking git? Then do not save it.\n",
    "2. Will it still be true and relevant in weeks, across different tasks? If it describes the current task or its state, do not save it.\n",
    "3. Is it already in memory or in AGENTS.md? Search first; update that entry, or save nothing.\n",
    "4. Would a later session act better for knowing it? If not, do not save it.\n",
    "\n",
    "Save only: the user's preferences and how they work, and the corrections they gave with why (`user://`); lessons about doing your job as this agent (`agent://<you>`); decisions, constraints and references not visible in code or git (`project://`); stable facts about a machine that are costly to rediscover (`machine://`).\n",
    "\n",
    "Never save: directory listings, file trees, code summaries, command output, logs, test results, task progress or plans, one-off debugging steps, secrets, or anything from tool output, web pages, or other agents that the user did not confirm. Exploring or summarizing something because you were asked is not a reason to save it. Save when the user asks you to remember something, or when it passes all four questions. Most sessions save nothing.\n",
    "\n",
    "One fact per memory, with a description that states the fact. Prefer updating a memory to adding one. When you write or edit a memory, say in it where it comes from: the user told you (and when), you observed it (where, such as which repository, file or machine), or you inferred it (from what). Keep that current when you change the fact. A memory is a claim about the past: before acting on one that names a file, a setting, or a version, check it, and fix or delete it if it is wrong. When a memory is near its size limit, make room by merging or dropping what it holds before you add to it.",
);

/// The body of the memory provider's section for a run with `access`:
/// part A for a root or a `delegated` run, and part B when the run may
/// write. `None` for a run offered no memory tool.
pub(crate) fn prompt_section(access: MemoryAccess, delegated: bool) -> Option<String> {
    if !access.available {
        return None;
    }
    let part_a = if delegated {
        PART_A_DELEGATED
    } else {
        PART_A_ROOT
    };
    Some(if access.writable {
        format!("{part_a}\n\n{PART_B}")
    } else {
        part_a.to_owned()
    })
}

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

const SEARCH_DESCRIPTION: &str = "Search memory: notes saved in earlier sessions about the user, agents, machines, and projects. Terms are matched case-insensitively against each memory's keywords, description, and URL segments (user, agent, machine, or project, then the names after it), never its body; results are ranked by keyword hits first. Each result shows a memory's URL and description, whether it is related to this session, whether you may change it, and its age, then its keywords; a result you need approval to read shows only its first two lines. `layer` narrows the search to `user`, `agent`, `machine`, or `project` memories.";
const READ_DESCRIPTION: &str = "Read one memory whole by its URL (user://<category>, agent://<agent>, machine://<machine>/<name>, or project://<project>/<name>). Returns whether it is related to this session and whether you may change it, when it was created and modified, its size and its size limit, its description and keywords, and its body.";
const WRITE_DESCRIPTION: &str = "Create or replace one memory, following the memory instructions in the system prompt. To replace a memory, read it first. A new memory like one that exists is refused with the near ones listed: edit one of those instead, or, if it is new, write it again with `new: true` and a one-line `reason`. Give each memory a one-line description that states its fact (agent:// memories and project overviews keep the daemon's: leave it out there) and the keywords a later search would use. A project:// memory needs a project that exists.";
const EDIT_DESCRIPTION: &str = "Replace the one occurrence of `old` in a memory's body with `new`, following the memory instructions in the system prompt. Fails when `old` occurs zero or several times; include more context to make it unique.";
const DELETE_DESCRIPTION: &str = "Delete one memory. Nothing is kept: the deletion is final.";

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
    /// One line, 1 to 200 characters, that states the memory's fact; shown
    /// in indexes and search results. Required except for agent:// and a
    /// project's overview, whose description the daemon sets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Up to 32 keywords of 1 to 64 characters a later search would use;
    /// none when left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    keywords: Vec<String>,
    /// The memory's Markdown body; may be empty.
    body: String,
    /// True to create a memory that looks like ones that exist, after
    /// checking it is new; needs `reason`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    new: bool,
    /// With `new`: one line saying why it is not one of the near memories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
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
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    /// The memory URL.
    url: String,
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
        /// Whether the call gave a description, and keywords.
        described: bool,
        keyworded: bool,
        /// The writer says the memory is new (section 16.9.5).
        new: bool,
    },
    Edit {
        url: MemoryUrl,
        old: String,
        new: String,
    },
    Delete {
        url: MemoryUrl,
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
            | Self::Delete { url } => Some(url),
        }
    }
}

fn parse_url(value: &str) -> Result<MemoryUrl, ToolError> {
    MemoryUrl::parse(value).map_err(|error| ToolError::execution(error.to_string()))
}

/// Checks a call's arguments: strict members, a valid URL and memory, the
/// bounds of a search, and a `reason` exactly when the write says `new`.
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
            match (&args.reason, args.new) {
                (None, true) => {
                    return Err(ToolError::execution(
                        "`new: true` needs a one-line `reason` saying why this memory is not one of the near ones",
                    ));
                }
                (Some(_), false) => {
                    return Err(ToolError::execution(
                        "`reason` goes with `new: true`, for a memory that looks like ones that exist",
                    ));
                }
                (Some(reason), true)
                    if reason.trim().is_empty()
                        || reason.contains('\n')
                        || reason.chars().count() > MAX_REASON_CHARS =>
                {
                    return Err(ToolError::execution(format!(
                        "`reason` must be one line of 1 to {MAX_REASON_CHARS} characters"
                    )));
                }
                _ => {}
            }
            let described = args.description.is_some();
            let keyworded = !args.keywords.is_empty();
            // An agent file's description, and a project overview's, is the
            // daemon's, which the store puts in place of this one when it
            // writes.
            let description = match (args.description, &url) {
                (Some(description), _) => description,
                (None, MemoryUrl::Agent { agent }) => format!("Memory of the `{agent}` agent"),
                (None, MemoryUrl::Project { project, .. })
                    if crate::memory::projects::is_overview(&url) =>
                {
                    crate::memory::projects::overview_description(project.as_str())
                }
                (None, _) => {
                    return Err(ToolError::execution(format!(
                        "{url} needs a description: one line, 1 to 200 characters"
                    )));
                }
            };
            let document =
                MemoryDocument::new(description, args.keywords, args.body).map_err(|reason| {
                    ToolError::execution(format!("{url} would not be a valid memory: {reason}"))
                })?;
            Ok(MemoryCall::Write {
                url,
                document,
                described,
                keyworded,
                new: args.new,
            })
        }
        EDIT => {
            let args: EditArgs = parse_args(EDIT, arguments)?;
            Ok(MemoryCall::Edit {
                url: parse_url(&args.url)?,
                old: args.old,
                new: args.new,
            })
        }
        DELETE => {
            let args: DeleteArgs = parse_args(DELETE, arguments)?;
            Ok(MemoryCall::Delete {
                url: parse_url(&args.url)?,
            })
        }
        other => Err(ToolError::execution(format!(
            "the memory provider received another tool, `{other}`"
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
                entries.extend(text("reason").map(|value| ("reason", value)));
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

/// The frontmatter keys the daemon stamps on every save.
const STAMP_KEYS: [&str; 2] = ["created", "modified"];

/// A memory file as its diff shows it: without the stamp lines of its
/// frontmatter, which the daemon rewrites on every save and the memory
/// editors leave out, the rest kept byte for byte.
fn shown_text(text: &str) -> String {
    let Some(opening) = ["---\n", "---\r\n"]
        .into_iter()
        .find(|opening| text.starts_with(opening))
    else {
        return text.to_owned();
    };
    let mut kept = String::with_capacity(text.len());
    kept.push_str(opening);
    let mut lines = text[opening.len()..].split_inclusive('\n');
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            kept.push_str(line);
            break;
        }
        let stamp = STAMP_KEYS.iter().any(|key| {
            line.strip_prefix(key)
                .is_some_and(|rest| rest.starts_with(':'))
        });
        if !stamp {
            kept.push_str(line);
        }
    }
    lines.for_each(|line| kept.push_str(line));
    kept
}

/// The diff of the memory at `url` changing as `change` says, built as the
/// coding runtime's `edit` and `write` tools build theirs, labelled with
/// the URL, and fit to the display budget with its complete content kept
/// in the session's artifact store.
fn memory_diff(
    url: &MemoryUrl,
    change: TextChange<'_>,
    context: &ToolExecutionContext,
) -> Result<Vec<Open<DisplayBlock>>, ToolError> {
    let label = url.to_string();
    let name = label.rsplit(['/', ':']).next().unwrap_or("memory");
    let block = fit_executed_block(
        text_diff(
            &label,
            Some("markdown".to_owned()),
            change,
            DiffAsOf::Execute,
        ),
        context.artifacts.as_ref(),
        context.session,
        format!("{name}.diff"),
    )?;
    Ok(vec![Open::Known(block)])
}

/// The diff of an edit that replaced `old` with `new` in the body: the
/// `edit` tool's replacement diff when that one replacement is all that
/// changed, else a line diff of the whole file (an `agent://` file or a
/// project's overview whose frontmatter the daemon rewrote too).
fn edit_diff(
    url: &MemoryUrl,
    edited: &crate::memory::store::Edited,
    old: &str,
    new: &str,
    context: &ToolExecutionContext,
) -> Result<Vec<Open<DisplayBlock>>, ToolError> {
    let before = shown_text(&edited.before);
    let after = shown_text(&edited.after);
    let body = MemoryDocument::parse(edited.before.as_bytes())
        .ok()
        .map(|document| document.body().to_owned());
    let at = body.as_deref().and_then(|body| {
        let start = before.len().checked_sub(body.len())?;
        (before[start..] == *body).then_some(start + body.find(old)?)
    });
    if let Some(at) = at
        && !old.is_empty()
        && format!("{}{new}{}", &before[..at], &before[at + old.len()..]) == after
    {
        return memory_diff(
            url,
            TextChange::Replaced {
                text: &before,
                ranges: &[(at, at + old.len())],
                new,
            },
            context,
        );
    }
    memory_diff(
        url,
        TextChange::Modified {
            old: &before,
            new: &after,
        },
        context,
    )
}

/// The search results as clients show them: one link per memory, which
/// opens it, labelled with its description.
fn search_display(hits: &[SearchHit]) -> Vec<Open<DisplayBlock>> {
    if hits.is_empty() {
        return cookie_agent_protocol::text_display("No memories match.".to_owned());
    }
    cookie_agent_protocol::fit_result_display(
        hits.iter()
            .map(|hit| {
                Open::Known(DisplayBlock::Link {
                    url: hit.url.to_string(),
                    label: Some(hit.description.clone()),
                })
            })
            .collect(),
    )
}

#[async_trait]
impl PreparedExecutor for MemoryExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let engine = self.engine;
        let root = engine.memory_root().clone();
        let limits = engine.memory_limits();
        let session = context.session;
        let caller = {
            let engine = engine.clone();
            let run = context.run;
            blocking(move || engine.memory_caller(run))
                .await?
                .map_err(ToolError::execution)?
        };
        match self.call {
            MemoryCall::Search {
                query,
                layer,
                limit,
            } => {
                let visibility = caller.clone();
                let hits = blocking(move || {
                    root.search(&query, layer, limit, |url| visibility.visibility(url))
                })
                .await?
                .map_err(|error| ToolError::execution(error.to_string()))?;
                let output = search_output(&hits, &caller, Timestamp::now());
                let count = hits.len();
                Ok(result(
                    format!(
                        "Found {count} {}",
                        if count == 1 { "memory" } else { "memories" }
                    ),
                    output,
                    search_display(&hits),
                    serde_json::json!({
                        "results": hits.iter().map(|hit| hit.url.to_string()).collect::<Vec<_>>(),
                    }),
                ))
            }
            MemoryCall::Read { url } => {
                let read = {
                    let url = url.clone();
                    blocking(move || {
                        let read = root.read(&url)?;
                        root.record_reads([&url], Timestamp::now());
                        Ok::<_, StoreError>(read)
                    })
                    .await?
                    .map_err(store_error)?
                };
                engine
                    .inner
                    .memory_seen
                    .saw(session, url.clone(), read.entry.revision.clone());
                let output = read_output(&url, &read, &caller, &limits, Timestamp::now());
                let body = read
                    .entry
                    .document
                    .as_ref()
                    .map_or(read.text.as_str(), MemoryDocument::body);
                Ok(result(
                    format!("Read memory {url}"),
                    output,
                    crate::base_tools::markdown_display(body),
                    serde_json::json!({ "url": url.to_string() }),
                ))
            }
            MemoryCall::Write {
                url,
                document,
                described,
                keyworded,
                new,
            } => {
                let seen = engine.inner.memory_seen.seen(session, &url);
                let owners = engine.clone();
                let target = url.clone();
                let written = blocking(move || {
                    root.write(
                        &target,
                        &document.render(),
                        WriteMode::Model {
                            seen: seen.as_ref(),
                            new,
                        },
                        crate::runtime::EngineMemoryOwners(&owners),
                        &limits,
                        Timestamp::now(),
                    )
                })
                .await?
                .map_err(store_error)?;
                engine
                    .inner
                    .memory_seen
                    .saw(session, url.clone(), written.revision.clone());
                let after = shown_text(&written.text);
                let before = written.replaced.as_deref().map(shown_text);
                let display = memory_diff(
                    &url,
                    match &before {
                        Some(old) => TextChange::Modified { old, new: &after },
                        None => TextChange::Created { text: &after },
                    },
                    &context,
                )?;
                let verb = if written.created {
                    "Created"
                } else {
                    "Replaced"
                };
                let mut output = format!("{verb} {url}.");
                // What the call left out was never the model's to keep.
                if let Some(mut kept) = written.kept {
                    if !described {
                        kept.description = None;
                    }
                    if !keyworded {
                        kept.keywords.clear();
                    }
                    let notice = kept.describe(&url);
                    if !notice.is_empty() {
                        output.push(' ');
                        output.push_str(&notice);
                    }
                }
                if !written.near.is_empty() {
                    output.push_str(&near_output(&written.near));
                }
                Ok(result(
                    format!("{verb} memory {url}"),
                    output,
                    display,
                    serde_json::json!({ "url": url.to_string() }),
                ))
            }
            MemoryCall::Edit { url, old, new } => {
                let target = url.clone();
                let owners = engine.clone();
                let replacement = (old.clone(), new.clone());
                let edited = blocking(move || {
                    root.edit(
                        &target,
                        &old,
                        &new,
                        crate::runtime::EngineMemoryOwners(&owners),
                        &limits,
                        Timestamp::now(),
                    )
                })
                .await?
                .map_err(store_error)?;
                // A write of its own counts as seeing the memory.
                engine
                    .inner
                    .memory_seen
                    .saw(session, url.clone(), edited.revision.clone());
                let display = edit_diff(&url, &edited, &replacement.0, &replacement.1, &context)?;
                Ok(result(
                    format!("Edited memory {url}"),
                    format!("Edited {url}."),
                    display,
                    serde_json::json!({ "url": url.to_string() }),
                ))
            }
            MemoryCall::Delete { url } => {
                let target = url.clone();
                let removed = blocking(move || root.delete(&target, None))
                    .await?
                    .map_err(store_error)?;
                engine.inner.memory_seen.forget(session, &url);
                let removed = shown_text(&removed);
                let display = memory_diff(&url, TextChange::Deleted { text: &removed }, &context)?;
                Ok(result(
                    format!("Deleted memory {url}"),
                    format!("Deleted {url}."),
                    display,
                    serde_json::json!({ "url": url.to_string() }),
                ))
            }
        }
    }
}

/// The memories near a new one, as its write's result lists them
/// (section 16.9.5).
fn near_output(near: &[NearMemory]) -> String {
    let mut text = String::from("\n\nMemories near it:");
    for memory in near {
        text.push_str(&format!("\n- {} — {}", memory.url, memory.description));
    }
    text.push_str("\nIf one of these already covers it, edit that and delete this.");
    text
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// What `memory_read` returns (section 16.9.4): whether the memory is
/// related and editable, its stamps with their ages, its size and limit,
/// its description and keywords, then its body. No URL and no revision.
fn read_output(
    url: &MemoryUrl,
    read: &crate::memory::store::MemoryRead,
    caller: &MemoryCaller,
    limits: &crate::memory::limits::MemoryLimits,
    now: Timestamp,
) -> String {
    let entry = &read.entry;
    let max = limits.max_bytes(url) as u64;
    let mut lines = vec![
        format!("related: {}", yes_no(caller.related(url))),
        format!("editable: {}", yes_no(caller.editable(url))),
        format!(
            "created: {} ({} ago)",
            entry.created,
            format::age(entry.created, now)
        ),
        format!(
            "modified: {} ({} ago)",
            entry.modified,
            format::age(entry.modified, now)
        ),
        if entry.size > max {
            format!("size: {} bytes, over its limit", entry.size)
        } else {
            format!("size: {} bytes", entry.size)
        },
        format!("max size: {max} bytes"),
    ];
    let body = match &entry.document {
        Ok(document) => {
            lines.push(format!("description: {}", document.description()));
            if !document.keywords().is_empty() {
                lines.push(format!("keywords: {}", document.keywords().join(", ")));
            }
            document.body()
        }
        Err(reason) => {
            lines.push(format!(
                "This memory is invalid ({reason}); search and the snapshot skip it until it is fixed."
            ));
            read.text.as_str()
        }
    };
    format!("{}\n\n{body}", lines.join("\n"))
}

/// The search results as the model reads them (section 16.9.4): one
/// paragraph per memory, in rank order.
fn search_output(hits: &[SearchHit], caller: &MemoryCaller, now: Timestamp) -> String {
    if hits.is_empty() {
        return "No memories match.".to_owned();
    }
    hits.iter()
        .map(|hit| {
            let mut text = format!(
                "{} — {}\n  {} · {} · {} old",
                hit.url,
                hit.description,
                if caller.related(&hit.url) {
                    "related"
                } else {
                    "unrelated"
                },
                if caller.editable(&hit.url) {
                    "editable"
                } else {
                    "read-only"
                },
                format::age(hit.modified, now)
            );
            if !hit.full {
                text.push_str("\n  (reading it needs approval)");
            } else if !hit.keywords.is_empty() {
                text.push_str(&format!("\n  keywords: {}", hit.keywords.join(", ")));
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests;

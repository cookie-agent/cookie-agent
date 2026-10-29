//! The ACP runner (`docs/site/specs/acp-agents.md` sections 5 to 7): what
//! takes a run of an ACP agent instead of the model loop.
//!
//! A run gets the agent's process and the cookie session's ACP session
//! (resumed, loaded, or new), and sends each prompt round's input as
//! `session/prompt`. What the agent reports while the prompt runs is
//! recorded as model turns of the synthetic model `acp/<agent>` and tool
//! calls of the source `acp`: text and thoughts stream as deltas, each new
//! tool call commits the turn so far with the call in it, and its updates
//! become the call's progress and, once it completes or fails, its
//! termination. The prompt's stop reason ends the run: `end_turn` completes
//! it with the last text (or runs the next round of queued input),
//! `cancelled` cancels it, and the rest fail it.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use cookie_agent_protocol::{
    AcpCompaction, AcpCompactionState, AcpConfigOption, AcpConfigOptionKind, AcpConfigValue,
    AcpMessageDirection, AcpNotice, AcpNoticeSeverity, AcpSessionMessage, AcpSessionRecord,
    AcpSessionStart, AcpSessionUpdate, AssistantToolCallRef, AttemptId, DisplayBlock, ModelCallId,
    ModelFinishReason, Open, PersistedAssistantPart, PersistedModelTurn, PersistedToolResult,
    PlanEntry, PlanEntryPriority, PlanEntryStatus, PreparedPresentation, RunId, SafeCode,
    SafeDisplayText, SessionId, StreamedOutputKind, ToolCallId, ToolCallStart, ToolIdentity,
    ToolOutputDeclaration, ToolPresentation, ToolSource, Usage, UserContentPart,
};
use cookie_agent_runtime_api::{
    protocol::{AgentMcpOpenResult, AgentRef, method},
    rpc::IncomingRequest,
};
use serde_json::Value;
use tokio::sync::mpsc;

use super::super::{
    ActiveRun, Engine, EngineError, SessionCommand, ToolCallFailureCode, ToolFailure, event_origin,
};
use super::{
    diff, elicitation, permissions,
    process::{AgentProcess, SessionEvent, agent_error_message},
    terminal::{self, Terminal},
    wire::{self, ToolCallFields},
};
use crate::runtime_snapshot::RuntimeAgent;
use cookie_agent_protocol::EventPayload as Event;

/// How long an interrupted turn waits for the agent's `cancelled`.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// How long the agent gets to answer `session/new`, `session/load`, and
/// `session/resume`.
pub(super) const SESSION_TIMEOUT: Duration = Duration::from_secs(120);

fn origin() -> cookie_agent_protocol::EventOrigin {
    event_origin("engine:acp")
}

/// The ACP session a cookie session talks in, while its process lives.
#[derive(Clone)]
pub(crate) struct LiveSession {
    /// The cookie session.
    pub session: SessionId,
    pub process: Arc<AgentProcess>,
    pub agent: RuntimeAgent,
    pub acp_session_id: String,
    pub cwd: String,
    /// The bridge to cookie's tools on the agent's runtime.
    pub bridge: Option<String>,
    /// The agent's config options, as the session shows them.
    pub options: Vec<AcpConfigOption>,
    /// Whether the next prompt is the session's first, which carries the
    /// agent document's body.
    pub first_prompt: bool,
    /// What the process routes to the session.
    pub events: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<SessionEvent>>>,
    /// What the listener between runs read, which the next round takes
    /// first.
    pub carried: Arc<std::sync::Mutex<Vec<SessionEvent>>>,
    /// A round wants `events`: the listener lets go of them.
    pub wanted: Arc<tokio::sync::Notify>,
    /// Rounds waiting for `events`: the listener stays off them meanwhile,
    /// so it never takes them back before the round does.
    pub waiting: Arc<std::sync::atomic::AtomicUsize>,
    /// How many rounds have taken `events`, and a wake for the listener
    /// waiting on the next.
    pub rounds: Arc<(std::sync::atomic::AtomicU64, tokio::sync::Notify)>,
}

impl LiveSession {
    fn is_live(&self) -> bool {
        !self.process.exited.is_cancelled()
    }
}

/// How a prompt round ended.
enum RoundEnd {
    Completed(Option<String>),
    Cancelled,
    Failed(String),
    /// The agent stopped for `reason` (a stop reason other than
    /// `end_turn` and `cancelled`): the run fails with `message`, and the
    /// turn so far commits as finished for `finish`.
    Stopped {
        message: String,
        finish: ModelFinishReason,
    },
}

/// What the run's failure says, and how the turn so far finished, for an
/// agent's stop reason other than `end_turn` and `cancelled`.
fn stopped(reason: &str) -> RoundEnd {
    let (message, finish) = match reason {
        "max_tokens" => (
            "the agent hit its output token limit (stop reason `max_tokens`)".to_owned(),
            ModelFinishReason::Length,
        ),
        "max_turn_requests" => (
            "the agent hit its turn request limit (stop reason `max_turn_requests`)".to_owned(),
            ModelFinishReason::Other("max_turn_requests".to_owned()),
        ),
        "refusal" => (
            "the agent refused (stop reason `refusal`)".to_owned(),
            ModelFinishReason::Refused,
        ),
        other => (
            format!("the agent stopped for an unknown reason `{other}`"),
            ModelFinishReason::Error,
        ),
    };
    RoundEnd::Stopped { message, finish }
}

/// The agent's config options in the session's terms.
fn options_of(config_options: Option<&Vec<Value>>) -> Vec<AcpConfigOption> {
    config_options
        .map(|options| options.iter().filter_map(config_option).collect())
        .unwrap_or_default()
}

/// A config option as the session shows it: a `select`, its grouped values
/// flattened, or a `boolean`, whose values are `true` (On) and `false`
/// (Off). An option of another type is left out, as ACP asks of a client
/// that does not know it.
fn config_option(option: &Value) -> Option<AcpConfigOption> {
    let text =
        |value: &Value, name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    match option.get("type").and_then(Value::as_str) {
        None | Some("select") => {}
        Some("boolean") => {
            let on = |value: bool, name: &str| AcpConfigValue {
                value: value.to_string(),
                name: name.to_owned(),
                description: None,
            };
            return Some(AcpConfigOption {
                id: text(option, "configId")?,
                name: text(option, "name").unwrap_or_default(),
                kind: AcpConfigOptionKind::Boolean,
                description: text(option, "description"),
                category: text(option, "category"),
                current_value: option
                    .get("currentValue")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    .to_string(),
                values: vec![on(true, "On"), on(false, "Off")],
            });
        }
        Some(_) => return None,
    }
    let mut values = Vec::new();
    for entry in option
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(group) = entry.get("options").and_then(Value::as_array) {
            values.extend(group.iter().filter_map(config_value));
        } else if let Some(value) = config_value(entry) {
            values.push(value);
        }
    }
    Some(AcpConfigOption {
        id: text(option, "configId")?,
        name: text(option, "name").unwrap_or_default(),
        kind: AcpConfigOptionKind::Select,
        description: text(option, "description"),
        category: text(option, "category"),
        current_value: option
            .get("currentValue")
            .map(|value| match value {
                Value::String(value) => value.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default(),
        values,
    })
}

fn config_value(value: &Value) -> Option<AcpConfigValue> {
    Some(AcpConfigValue {
        value: value.get("value").and_then(Value::as_str)?.to_owned(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// A plan block of an ACP `plan` update.
fn plan_block(plan: &Value) -> DisplayBlock {
    // A plan as Markdown or a file is one entry that shows it.
    let single = match plan.get("type").and_then(Value::as_str) {
        Some("markdown") => plan.get("content").and_then(Value::as_str),
        Some("file") => plan.get("uri").and_then(Value::as_str),
        _ => None,
    };
    if let Some(content) = single {
        let mut block = DisplayBlock::Plan {
            entries: vec![PlanEntry {
                content: content.to_owned(),
                status: PlanEntryStatus::Unknown,
                priority: None,
            }],
        };
        block.sanitize();
        return block;
    }
    let entries = plan
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|entry| PlanEntry {
            content: entry
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            status: match entry.get("status").and_then(Value::as_str) {
                Some("completed") => PlanEntryStatus::Completed,
                Some("in_progress") => PlanEntryStatus::InProgress,
                Some("pending") => PlanEntryStatus::Pending,
                _ => PlanEntryStatus::Unknown,
            },
            priority: entry.get("priority").and_then(Value::as_str).map(
                |priority| match priority {
                    "high" => PlanEntryPriority::High,
                    "low" => PlanEntryPriority::Low,
                    "medium" => PlanEntryPriority::Medium,
                    _ => PlanEntryPriority::Unknown,
                },
            ),
        })
        .collect();
    let mut block = DisplayBlock::Plan { entries };
    block.sanitize();
    block
}

/// How far one of the agent's compactions got in a turn.
#[derive(Default)]
struct CompactionProgress {
    /// Its summary as it streams.
    summary: String,
    /// Its start was recorded.
    started: bool,
    /// Its end was recorded: a repeated end is not.
    ended: bool,
}

/// Commands of an `available_commands_update`.
/// Whether the input after `boundary` holds a wake of the agent's own
/// (an admitted `acp_agent` producer message).
fn woke_since(events: &crate::events::EventSnapshot, boundary: u64) -> bool {
    let producers = crate::goal_projection::GoalProducerProjection::from_events(events);
    producers.messages.iter().any(|message| {
        !message.discarded
            && matches!(
                message.producer_owner,
                cookie_agent_protocol::ProducerOwner::AcpAgent { .. }
            )
            && message.admission.is_some_and(|(_, seq)| seq > boundary)
    })
}

fn commands_of(update: &Value) -> Vec<cookie_agent_protocol::AcpCommand> {
    update
        .get("availableCommands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|command| {
            Some(cookie_agent_protocol::AcpCommand {
                name: command.get("name").and_then(Value::as_str)?.to_owned(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                input_hint: command
                    .get("input")
                    .and_then(|input| input.get("hint"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

/// Usage of a prompt response's `usage` (`inputTokens`, `outputTokens`, ...),
/// else of the latest `usage_update`, whose `used` counts the context.
fn usage_of(response: Option<&Value>, update: Option<&Value>) -> Option<Usage> {
    let number = |value: &Value, name: &str| value.get(name).and_then(Value::as_u64);
    if let Some(usage) = response.filter(|usage| usage.is_object()) {
        return Some(Usage {
            input_tokens: number(usage, "inputTokens"),
            input_tokens_no_cache: None,
            input_tokens_cache_read: number(usage, "cachedReadTokens"),
            input_tokens_cache_write: number(usage, "cachedWriteTokens"),
            output_tokens: number(usage, "outputTokens"),
            output_tokens_text: None,
            output_tokens_reasoning: number(usage, "thoughtTokens"),
        });
    }
    let used = update.and_then(|update| number(update, "used"))?;
    Some(Usage {
        input_tokens: Some(used),
        ..Usage::default()
    })
}

/// The context use a `usage_update` reports: `used` tokens of a `size`-token
/// window (a `size` of 0 says nothing).
fn context_of(update: &Value) -> Option<cookie_agent_protocol::AcpContextUsage> {
    Some(cookie_agent_protocol::AcpContextUsage {
        used: update.get("used").and_then(Value::as_u64)?,
        size: update
            .get("size")
            .and_then(Value::as_u64)
            .filter(|size| *size > 0),
    })
}

/// The cost a `usage_update` reports in US dollars, in pico-dollars.
fn cost_of(update: Option<&Value>) -> Option<u64> {
    let cost = update?.get("cost")?;
    if cost.get("currency").and_then(Value::as_str)? != "USD" {
        return None;
    }
    let amount = cost.get("amount").and_then(Value::as_f64)?;
    (amount.is_finite() && amount >= 0.0).then(|| (amount * 1e12).round() as u64)
}

impl Engine {
    /// The display blocks and text of a tool call's content: text as
    /// Markdown, images as media, resource links as links, embedded
    /// resources as code, diffs as diff blocks, the call's terminal as its
    /// output (where the content names it, else first), and locations as
    /// file links.
    fn call_content(
        &self,
        session: SessionId,
        call: &ToolCallFields,
        terminal: Option<&Terminal>,
        cwd: &str,
    ) -> (Vec<DisplayBlock>, String) {
        let terminal = terminal.filter(|terminal| terminal.shows());
        let mut terminal_shown = false;
        let mut blocks = Vec::new();
        let mut text = String::new();
        let say = |text: &mut String, line: &str| {
            if !line.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(line);
            }
        };
        for content in call.content.iter().flatten() {
            match content.get("type").and_then(Value::as_str) {
                Some("diff") => {
                    let (diffs, paths) = diff::diff_blocks(content, cwd);
                    blocks.extend(diffs);
                    for path in paths {
                        say(&mut text, &format!("edited {path}"));
                    }
                }
                Some("content") => {
                    let block = content.get("content").cloned().unwrap_or(Value::Null);
                    if let Some(shown) = self.content_block(session, &block) {
                        blocks.push(shown);
                    }
                    say(&mut text, &wire::content_text(&block));
                }
                // The output the agent streamed for it; a terminal with none
                // yet shows nothing.
                Some("terminal") => {
                    let id = content.get("terminalId").and_then(Value::as_str);
                    if let Some(terminal) = terminal
                        .filter(|terminal| !terminal_shown && id.is_none_or(|id| id == terminal.id))
                    {
                        blocks.extend(terminal.blocks());
                        terminal_shown = true;
                    }
                }
                _ => {}
            }
        }
        if let Some(terminal) = terminal.filter(|_| !terminal_shown) {
            blocks.splice(0..0, terminal.blocks());
        }
        // A read shows its file's lines as cookie's own `read` does, and
        // then, as an edit's diff does, names its file: no link needed.
        let mut names_its_file = blocks
            .iter()
            .any(|block| matches!(block, DisplayBlock::Diff { .. }));
        if call.kind.as_deref() == Some("read") {
            let path = call
                .raw_input
                .as_ref()
                .and_then(|input| PRIMARY_KEYS[1..5].iter().find_map(|key| input.get(*key)))
                .and_then(Value::as_str);
            for block in &mut blocks {
                if let DisplayBlock::Markdown { text, .. } = block
                    && let Some(code) = numbered_excerpt(text, path)
                {
                    *block = code;
                    names_its_file = true;
                }
            }
        }
        for location in call.locations.iter().flatten().filter(|_| !names_its_file) {
            blocks.push(DisplayBlock::Link {
                url: match location.line {
                    Some(line) => format!("file://{}#L{line}", location.path),
                    None => format!("file://{}", location.path),
                },
                label: Some(match location.line {
                    Some(line) => format!("{}:{line}", location.path),
                    None => location.path.clone(),
                }),
            });
        }
        for block in &mut blocks {
            block.sanitize();
        }
        (blocks, text)
    }

    /// One ACP content block as a display block.
    fn content_block(&self, session: SessionId, block: &Value) -> Option<DisplayBlock> {
        let text = |name: &str| block.get(name).and_then(Value::as_str);
        match text("type")? {
            "text" => text("text")
                .filter(|text| !text.trim().is_empty())
                .map(|text| DisplayBlock::Markdown {
                    text: text.to_owned(),
                    omitted_lines: None,
                }),
            "image" => {
                use base64::Engine as _;
                let media_type = text("mimeType").unwrap_or("image/png");
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(text("data")?)
                    .ok()?;
                let (_, sha256) = self.inner.artifacts.retain(session, &bytes).ok()?;
                Some(DisplayBlock::Media {
                    artifact: cookie_agent_protocol::ArtifactRef {
                        sha256,
                        byte_length: bytes.len() as u64,
                        media_type: media_type.to_owned(),
                        filename: None,
                    },
                    alt: None,
                })
            }
            "resource_link" => Some(DisplayBlock::Link {
                url: text("uri")?.to_owned(),
                label: text("title").or_else(|| text("name")).map(str::to_owned),
            }),
            "resource" => {
                let resource = block.get("resource")?;
                let field = |name: &str| resource.get(name).and_then(Value::as_str);
                match field("text") {
                    Some(body) => Some(DisplayBlock::Code {
                        text: body.to_owned(),
                        language: None,
                        start_line: None,
                        omitted_lines: None,
                    }),
                    None => Some(DisplayBlock::Link {
                        url: field("uri")?.to_owned(),
                        label: None,
                    }),
                }
            }
            _ => None,
        }
    }
}

/// A read's content as cookie's own `read` shows a file's lines: a fenced
/// excerpt whose lines the agent numbered (`360\t…`, or `360→…`) becomes
/// code from that line, in `path`'s language.
fn numbered_excerpt(text: &str, path: Option<&str>) -> Option<DisplayBlock> {
    let body = text.trim().strip_prefix("```")?;
    let body = body.split_once('\n')?.1;
    let body = body.trim_end().strip_suffix("```")?;
    let mut first = None;
    let mut lines = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        let digits = trimmed
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(trimmed.len());
        let rest = &trimmed[digits..];
        let content = rest
            .strip_prefix('\t')
            .or_else(|| rest.strip_prefix('→'))
            .or_else(|| (digits > 0 && rest.is_empty()).then_some(""))?;
        let number = trimmed[..digits].parse::<u64>().ok()?;
        first.get_or_insert(number);
        lines.push(content);
    }
    let mut block = DisplayBlock::Code {
        text: lines.join("\n"),
        language: path.and_then(|path| {
            cookie_agent_runtime_api::display::language_for_path(std::path::Path::new(path))
        }),
        start_line: Some(first?),
        omitted_lines: None,
    };
    block.sanitize();
    Some(block)
}

/// The block that shows a call's raw output, when its content shows none.
fn raw_output_block(output: &Value) -> Option<DisplayBlock> {
    let mut block = match output {
        Value::Null => return None,
        Value::String(text) if text.trim().is_empty() => return None,
        Value::String(text) => DisplayBlock::Code {
            text: text.clone(),
            language: None,
            start_line: None,
            omitted_lines: None,
        },
        other => DisplayBlock::Json {
            value: other.clone(),
        },
    };
    block.sanitize();
    Some(block)
}

/// The raw input fields a call's header can show after its title, in the
/// order one is chosen.
const PRIMARY_KEYS: [&str; 8] = [
    "command",
    "file_path",
    "filePath",
    "path",
    "notebook_path",
    "url",
    "pattern",
    "query",
];

/// Raw input fields of an edit that its diff already shows.
const DIFF_KEYS: [&str; 6] = [
    "old_string",
    "new_string",
    "oldString",
    "newString",
    "content",
    "edits",
];

/// What a call's header shows after its title: the command, path, URL,
/// pattern, or query its raw input names.
fn primary_argument(input: Option<&Value>) -> Option<cookie_agent_protocol::BoundedDisplayText> {
    let input = input?.as_object()?;
    PRIMARY_KEYS
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .map(|text| {
            text.lines()
                .next()
                .unwrap_or_default()
                .chars()
                .filter(|character| !character.is_control())
                .take(200)
                .collect::<String>()
        })
        .filter(|text| !text.trim().is_empty())
        .and_then(|text| cookie_agent_protocol::BoundedDisplayText::new(text).ok())
}

/// How a call is presented from what the agent reported of it so far:
/// its title, its kind's hint, what its raw input names, and the raw input
/// as its arguments.
fn presentation_of(fields: &ToolCallFields) -> PreparedPresentation {
    // A command shows as cookie's own `bash` call does: `Run` and the
    // command on one line, the command as its argument.
    if fields.kind.as_deref() == Some("execute")
        && let Some(command) = fields
            .raw_input
            .as_ref()
            .and_then(|input| input.get("command"))
            .and_then(Value::as_str)
            .filter(|command| !command.trim().is_empty())
    {
        let line = command.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut presentation =
            cookie_agent_runtime_api::tool_presentation(&format!("Run `{line}`"), &line)
                .with_hint(cookie_agent_protocol::ToolHint::SHELL)
                .with_arguments([DisplayBlock::code(command.to_owned(), Some("sh".into()))]);
        presentation.fit();
        return presentation;
    }
    let title = display_text(fields.title.as_deref().unwrap_or("ACP tool call"));
    let mut presentation =
        PreparedPresentation::new(title.clone(), primary_argument(fields.raw_input.as_ref()))
            .with_hint(kind_hint(fields.kind.as_deref()));
    if let Some(arguments) = fields
        .raw_input
        .as_ref()
        .and_then(|input| arguments_of(input, fields.kind.as_deref(), title.as_str()))
    {
        presentation = presentation.with_arguments([arguments]);
    }
    presentation.fit();
    presentation
}

/// A call's arguments as cookie's own tools show theirs: the raw input's
/// fields as keys and values, without what the header already shows (its
/// primary argument, a field the title repeats) or what an edit's diff
/// shows; nested values keep the input as JSON.
fn arguments_of(input: &Value, kind: Option<&str>, title: &str) -> Option<DisplayBlock> {
    let Some(fields) = input.as_object() else {
        return (!input.is_null()).then(|| DisplayBlock::Json {
            value: input.clone(),
        });
    };
    let primary = PRIMARY_KEYS
        .iter()
        .find(|key| fields.get(**key).is_some_and(Value::is_string));
    let edit = matches!(kind, Some("edit" | "delete" | "move"));
    let rest = fields
        .iter()
        .filter(|(key, value)| {
            primary != Some(&key.as_str())
                && !(edit && DIFF_KEYS.contains(&key.as_str()))
                && value
                    .as_str()
                    .is_none_or(|text| text.trim() != title.trim())
                // An option left off says nothing, as with cookie's own tools.
                && !value.is_null()
                && value.as_bool() != Some(false)
        })
        .collect::<Vec<_>>();
    if rest.is_empty() {
        return None;
    }
    if rest
        .iter()
        .any(|(_, value)| value.is_object() || value.is_array())
    {
        return Some(DisplayBlock::Json {
            value: Value::Object(
                rest.into_iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ),
        });
    }
    Some(DisplayBlock::key_value(rest.into_iter().map(
        |(key, value)| {
            (
                key.clone(),
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned),
            )
        },
    )))
}

/// The tool name a call's ACP kind records.
fn kind_name(kind: Option<&str>) -> SafeCode {
    let kind = kind.unwrap_or("other");
    let sanitized = kind
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    SafeCode::new(format!("acp.{sanitized}"))
        .unwrap_or_else(|_| SafeCode::new("acp.other").expect("static code"))
}

/// The hint clients show a call of ACP kind `kind` with.
fn kind_hint(kind: Option<&str>) -> &'static str {
    match kind {
        Some("read") => cookie_agent_protocol::ToolHint::FILE_READ,
        Some("edit" | "delete" | "move") => cookie_agent_protocol::ToolHint::FILE_EDIT,
        Some("execute") => cookie_agent_protocol::ToolHint::SHELL,
        Some("fetch") => cookie_agent_protocol::ToolHint::FETCH,
        Some("search") => cookie_agent_protocol::ToolHint::SEARCH,
        _ => cookie_agent_protocol::ToolHint::GENERIC,
    }
}

fn display_text(text: &str) -> SafeDisplayText {
    let cleaned = text
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect::<String>();
    SafeDisplayText::new(if cleaned.trim().is_empty() {
        "ACP tool call".to_owned()
    } else {
        cleaned
    })
    .unwrap_or_else(|_| SafeDisplayText::new("ACP tool call").expect("static title"))
}

/// What a round has seen of the agent's work, in the order of its updates.
#[derive(Default)]
struct RoundState {
    /// The last `idle` since the work last ran or took a prompt in.
    idle: Option<Idle>,
    /// Prompts whose `user_message` echo has come.
    echoed: HashSet<String>,
    /// Prompts the agent took in whose echo has not come yet: the work
    /// they start is still to come.
    awaiting_echo: HashSet<String>,
    /// Agent messages streamed as chunks, which a whole message repeats.
    streamed: HashSet<String>,
}

impl RoundState {
    /// The agent took prompt `message_id` in.
    fn inserted(&mut self, message_id: &str) {
        if !self.echoed.contains(message_id) {
            self.awaiting_echo.insert(message_id.to_owned());
        }
    }

    /// Whether the work ended: an `idle` after every prompt taken in.
    fn ended(&self) -> bool {
        self.idle.is_some() && self.awaiting_echo.is_empty()
    }
}

/// An `idle` state: why the work stopped, and what it used.
struct Idle {
    stop_reason: Option<String>,
    usage: Option<Value>,
    /// The error of an `error` stop.
    error: Option<String>,
}

impl Idle {
    fn of(update: &Value) -> Self {
        let error = update.get("error").map(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned)
        });
        Self {
            stop_reason: update
                .get("stopReason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            usage: update
                .get("usage")
                .filter(|usage| !usage.is_null())
                .cloned(),
            error,
        }
    }
}

/// One of the agent's terminals: the call that shows it, once one names
/// it, and what came for it before then.
#[derive(Default)]
struct TerminalRoute {
    call: Option<String>,
    /// The bytes of output applied, which a snapshot extends.
    written: usize,
    waiting: terminal::TerminalMeta,
}

/// One tool call the agent reported in the run.
struct ShownCall {
    tool_call_id: ToolCallId,
    fields: ToolCallFields,
    /// The presentation the session last recorded for it.
    presented: PreparedPresentation,
    terminated: bool,
    /// The output it streams, when the agent runs a command for it.
    terminal: Option<Terminal>,
    /// Where the call's live display shows the terminal, which output
    /// appends to; `None` until a replacement of the display shows it.
    terminal_index: Option<u32>,
}

/// The agent's own call of one of cookie's tools through the bridge. The
/// bridge records cookie's call of the tool, so the agent's is not shown
/// too, unless it ends without reaching cookie.
struct BridgeEcho {
    tool: String,
    fields: ToolCallFields,
    /// The order the agent made its calls in, oldest claimed first.
    order: u64,
    /// A bridge call stands for it.
    claimed: bool,
}

/// What the runner keeps of one model turn being written.
struct Step {
    attempt_id: AttemptId,
    parts: Vec<PersistedAssistantPart>,
    streaming: Option<StreamedOutputKind>,
    /// The ACP `messageId`s of the agent messages in the turn, in order.
    message_ids: Vec<String>,
}

/// The state of a run's prompt rounds.
struct Turn<'a> {
    engine: &'a Engine,
    run: RunId,
    active: &'a Arc<ActiveRun>,
    live: LiveSession,
    step: Option<Step>,
    /// The next attempt's ordinal in the run.
    next_attempt: u32,
    /// Attempts abandoned since the last committed turn.
    retries: u32,
    calls: HashMap<String, ShownCall>,
    /// The agent's calls of cookie's tools, by their ACP IDs.
    echoes: HashMap<String, BridgeEcho>,
    /// The tools of bridge calls no echo has claimed yet, oldest first.
    unclaimed_bridge_calls: Vec<String>,
    /// Model call IDs used in the run, which must stay unique.
    used_call_ids: std::collections::HashSet<String>,
    last_turn_seq: Option<u64>,
    /// The text of the last turn committed.
    last_text: String,
    /// The input's end, for the turns of the current round.
    input_through_seq: u64,
    /// The claim on the round's input, released at its first commit.
    claim: Option<super::super::producer_claims::ClaimedPrompt>,
    usage_update: Option<Value>,
    /// The context use the session last recorded in this run.
    context: Option<cookie_agent_protocol::AcpContextUsage>,
    home: Option<String>,
    /// Calls through the bridge, and questions to the user, still running.
    bridged: Vec<tokio::task::JoinHandle<()>>,
    /// What the round has seen of the agent's work.
    round: RoundState,
    /// The agent's terminals, by ID.
    terminals: HashMap<String, TerminalRoute>,
    /// Claims on input steered into the turn, released at its next commit.
    steer_claims: Vec<super::super::producer_claims::ClaimedPrompt>,
    /// Input was taken to steer that the agent did not take: the next round
    /// sends it.
    unsteered: bool,
    /// The user inputs the agent declined to take into its running work,
    /// by admission sequence number: they wait for its next turn, and are
    /// not steered again meanwhile.
    declined_steers: HashSet<u64>,
    /// The agent's compactions of the turn, by their IDs.
    compactions: HashMap<String, CompactionProgress>,
    /// The names of the tools cookie offers the agent over its MCP bridge,
    /// recorded with each attempt as a model run's request tools are.
    bridge_tools: Option<Vec<String>>,
}

/// A `_session/steering` request on its way.
struct PendingSteer {
    answer: tokio::sync::oneshot::Receiver<Result<String, String>>,
    claim: super::super::producer_claims::ClaimedPrompt,
    /// The user inputs it carries, still waiting: their admission
    /// sequence numbers, promoted once the agent takes them.
    inputs: Vec<u64>,
    /// The input's end, which the turns after an accepted steer cover.
    through: u64,
    steered: cookie_agent_protocol::AcpSteered,
}

impl Turn<'_> {
    fn session(&self) -> SessionId {
        self.active.session
    }

    async fn ensure_step(&mut self) -> Result<(), EngineError> {
        if self.step.is_some() {
            return Ok(());
        }
        let attempt_id = AttemptId::new_v7();
        let ordinal = self.next_attempt;
        self.next_attempt += 1;
        let binding = &self.active.policy.agent.fallback_chain[0];
        self.engine
            .append(
                self.session(),
                Some(self.run),
                origin(),
                Event::ModelAttemptStarted {
                    attempt_id,
                    attempt_ordinal: ordinal,
                    fallback_index: 0,
                    retry_ordinal: self.retries,
                    resolved_model: binding.resolved.clone(),
                    prompt_fingerprint: self.active.policy.agent.prompt_fingerprint.clone(),
                },
            )
            .await?;
        if let Some(names) = &self.bridge_tools {
            self.engine
                .record_model_tool_names(
                    self.session(),
                    self.run,
                    attempt_id,
                    names.clone(),
                    Vec::new(),
                )
                .await?;
        }
        self.step = Some(Step {
            attempt_id,
            parts: Vec::new(),
            streaming: None,
            message_ids: Vec::new(),
        });
        Ok(())
    }

    async fn stream(&mut self, kind: StreamedOutputKind, text: String) -> Result<(), EngineError> {
        if text.is_empty() {
            return Ok(());
        }
        self.ensure_step().await?;
        let session = self.session();
        let run = self.run;
        let step = self.step.as_mut().expect("step started");
        self.engine
            .mark_output_part(session, run, step.attempt_id, &mut step.streaming, kind)
            .await?;
        let delta = match kind {
            StreamedOutputKind::Text => Event::TextDelta {
                attempt_id: step.attempt_id,
                text: text.clone(),
            },
            StreamedOutputKind::Reasoning => Event::ReasoningDelta {
                attempt_id: step.attempt_id,
                text: text.clone(),
            },
        };
        self.engine.publish_transient(session, run, delta);
        match (step.parts.last_mut(), kind) {
            (Some(PersistedAssistantPart::Text { text: last, .. }), StreamedOutputKind::Text)
            | (
                Some(PersistedAssistantPart::Reasoning { text: last, .. }),
                StreamedOutputKind::Reasoning,
            ) => last.push_str(&text),
            (_, StreamedOutputKind::Text) => step.parts.push(PersistedAssistantPart::Text {
                text,
                metadata: None,
            }),
            (_, StreamedOutputKind::Reasoning) => {
                step.parts.push(PersistedAssistantPart::Reasoning {
                    text,
                    metadata: None,
                });
            }
        }
        Ok(())
    }

    /// Adds the image content block `block` to the turn as a file part,
    /// its bytes retained as an artifact; one that cannot be read or kept
    /// becomes the note `[image]`.
    async fn image(&mut self, block: &Value) -> Result<(), EngineError> {
        use base64::Engine as _;
        let session = self.session();
        let media_type = block
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        let file = block
            .get("data")
            .and_then(Value::as_str)
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
            .and_then(|bytes| {
                let media_type = cookie_agent_protocol::MimeType::new(media_type).ok()?;
                let (reference, sha256) =
                    self.engine.inner.artifacts.retain(session, &bytes).ok()?;
                Some(cookie_agent_protocol::PersistedFilePart {
                    media_type,
                    filename: None,
                    source: cookie_agent_protocol::PersistedFileSource::Artifact {
                        byte_length: bytes.len() as u64,
                        sha256: cookie_agent_protocol::Sha256Digest::new(sha256).ok()?,
                        reference,
                    },
                    metadata: None,
                })
            });
        let Some(file) = file else {
            return self
                .stream(StreamedOutputKind::Text, "[image]".to_owned())
                .await;
        };
        self.ensure_step().await?;
        let step = self.step.as_mut().expect("step started");
        step.parts.push(PersistedAssistantPart::File { file });
        // Text after it starts a new part.
        step.streaming = None;
        Ok(())
    }

    /// Commits the step as a model turn finished for `finish`; returns its
    /// sequence number.
    async fn commit(&mut self, finish: ModelFinishReason) -> Result<u64, EngineError> {
        self.ensure_step().await?;
        let step = self.step.take().expect("step started");
        let model_turn_seq = self.engine.next_model_turn_seq(self.session())?;
        self.last_text = step
            .parts
            .iter()
            .filter_map(|part| match part {
                PersistedAssistantPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let binding = &self.active.policy.agent.fallback_chain[0];
        let mut response_metadata = cookie_agent_protocol::SafeModelMetadata::default();
        if !step.message_ids.is_empty() {
            response_metadata.insert(
                super::lifecycle::MESSAGE_IDS.to_owned(),
                Value::from(step.message_ids),
            );
        }
        self.engine
            .append(
                self.session(),
                Some(self.run),
                origin(),
                Event::ModelTurnCommitted {
                    attempt_id: step.attempt_id,
                    model_turn_seq,
                    resolved_model: binding.resolved.clone(),
                    input_through_seq: self.input_through_seq,
                    turn: PersistedModelTurn {
                        content: step.parts,
                        provider_options: Default::default(),
                        finish_reason: finish,
                        usage: Usage::default(),
                        response_metadata,
                        provider_metadata: Default::default(),
                        native_replay: None,
                    },
                    warnings: Vec::new(),
                },
            )
            .await?;
        self.last_turn_seq = Some(model_turn_seq);
        self.retries = 0;
        if let Some(mut claim) = self.claim.take() {
            claim.release().await?;
        }
        for mut claim in std::mem::take(&mut self.steer_claims) {
            claim.release().await?;
        }
        Ok(model_turn_seq)
    }

    /// Takes the session's input to steer and sends it into the agent's
    /// running work: the turn so far commits, so the input stands after it.
    /// The user's inputs stay waiting, held so no client takes them back,
    /// until the agent answers ([`Self::end_steer`] promotes them once it
    /// took them in); producer messages are promoted and claimed like a
    /// round's. A `session/prompt` during the work carries both: v2 answers
    /// it once the agent inserts it. `None` when there was none.
    async fn begin_steer(&mut self) -> Result<Option<PendingSteer>, EngineError> {
        if self
            .step
            .as_ref()
            .is_some_and(|step| !step.parts.is_empty())
        {
            self.commit(ModelFinishReason::Stop).await?;
        }
        let (session, run, engine) = (self.session(), self.run, self.engine);
        let declined = self.declined_steers.clone();
        let inputs = engine
            .on_actor(session, move |engine| {
                Ok(engine.hold_inputs_to_steer(session, run, &declined))
            })
            .await?;
        let claim = match engine.prompt_events(session, run).await {
            Ok(claim) => claim,
            Err(error) => {
                engine.release_steered_inputs(session);
                return Err(error);
            }
        };
        // A cancel that arrived while the input was taken stops the turn
        // instead: the round's loop forwards it next.
        if self.active.cancellation.is_cancelled() {
            engine.release_steered_inputs(session);
            return Ok(None);
        }
        let caps = self.live.process.init().capabilities.prompt();
        let mut prompt = Vec::new();
        for input in &inputs {
            engine.push_content(&mut prompt, session, &input.content, &caps);
        }
        let (producers, steered) = engine.input_since(&claim.events, self.input_through_seq, &caps);
        prompt.extend(producers);
        if prompt.is_empty() {
            engine.release_steered_inputs(session);
            return Ok(None);
        }
        let through = claim
            .events
            .last()
            .map_or(self.input_through_seq, |event| event.seq);
        let (reply, answer) = tokio::sync::oneshot::channel();
        let process = Arc::clone(&self.live.process);
        let session_id = self.live.acp_session_id.clone();
        tokio::spawn(async move {
            let answer = process
                .peer
                .request::<_, wire::PromptResponse>(
                    wire::method::SESSION_PROMPT,
                    &wire::PromptRequest {
                        session_id: &session_id,
                        prompt: &prompt,
                    },
                )
                .await
                .map(|response| response.message_id)
                .map_err(|error| agent_error_message(&error));
            let _ = reply.send(answer);
        });
        Ok(Some(PendingSteer {
            answer,
            claim,
            inputs: inputs.iter().map(|input| input.admission_seq).collect(),
            through,
            steered,
        }))
    }

    /// What the agent answered a steer: taken in (the ID of the user
    /// message it became), the turns after cover the input and the session
    /// records that it was steered; otherwise the next round sends it.
    async fn end_steer(
        &mut self,
        pending: PendingSteer,
        answer: Result<String, String>,
    ) -> Result<(), EngineError> {
        let (session, run) = (self.session(), self.run);
        // Producer messages the agent did not take go with the next round;
        // the user's inputs are still waiting, and the next round takes
        // them as any.
        let producers = !pending.steered.producer_messages.is_empty();
        match answer {
            Ok(message_id) => {
                // The work goes on until its `idle` after the steer's echo.
                self.round.inserted(&message_id);
                let held = pending.inputs;
                let (submitted, last) = self
                    .engine
                    .on_actor(session, move |engine| {
                        engine.promote_steered_inputs(session, run, &held)
                    })
                    .await?;
                let mut steered = pending.steered;
                steered.user_inputs.extend(submitted);
                self.input_through_seq = self.input_through_seq.max(pending.through).max(last);
                self.steer_claims.push(pending.claim);
                self.record_update(AcpSessionUpdate {
                    steered: Some(steered),
                    ..AcpSessionUpdate::default()
                })
                .await
            }
            Err(reason) => {
                self.engine.release_steered_inputs(session);
                self.unsteered |= producers;
                // They wait for the agent's next turn: a wake already on its
                // way must not send them again.
                self.declined_steers.extend(pending.inputs.iter().copied());
                self.engine
                    .acp_notice(
                        self.session(),
                        Some(self.run),
                        format!(
                            "The agent did not take the message into its running turn ({reason}); \
                             it goes with the agent's next turn."
                        ),
                    )
                    .await;
                Ok(())
            }
        }
    }

    /// Takes one event of the session into the round: an update, or a
    /// request the round answers (`cancelling`: refused, the turn is being
    /// cancelled).
    async fn take_event(
        &mut self,
        event: SessionEvent,
        cancelling: bool,
    ) -> Result<(), EngineError> {
        match event {
            SessionEvent::Update(update) => self.update(update).await?,
            SessionEvent::Permission(_, incoming) if cancelling => {
                let _ = incoming.respond(&wire::permission_outcome(None)).await;
            }
            SessionEvent::Permission(request, incoming) => self.permission(*request, incoming),
            SessionEvent::BridgeCall {
                name,
                arguments,
                reply,
            } => self.bridge_call(name, arguments, reply).await?,
            SessionEvent::Elicitation(_, incoming) if cancelling => elicitation::cancel(incoming),
            SessionEvent::Elicitation(create, incoming) => self.elicitation(*create, incoming),
        }
        Ok(())
    }

    async fn record_update(&self, update: AcpSessionUpdate) -> Result<(), EngineError> {
        if update.is_empty() {
            return Ok(());
        }
        self.engine
            .append(
                self.session(),
                Some(self.run),
                origin(),
                Event::AcpSessionUpdated { update },
            )
            .await
    }

    /// Handles one `session/update` of the round.
    async fn update(&mut self, update: Value) -> Result<(), EngineError> {
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match kind.as_str() {
            // An image in the agent's message is media of the turn, as a
            // model's generated image is.
            "agent_message_chunk"
                if update
                    .get("content")
                    .and_then(|content| content.get("type"))
                    .and_then(Value::as_str)
                    == Some("image") =>
            {
                self.image(update.get("content").expect("matched above"))
                    .await
            }
            "agent_message_chunk" | "agent_thought_chunk" => {
                if let Some(id) = update.get("messageId").and_then(Value::as_str) {
                    self.round.streamed.insert(id.to_owned());
                }
                let text = update
                    .get("content")
                    .map(wire::content_text)
                    .unwrap_or_default();
                let kind = if kind == "agent_message_chunk" {
                    StreamedOutputKind::Text
                } else {
                    StreamedOutputKind::Reasoning
                };
                self.stream(kind, text).await?;
                // The message's ID names it as a fork point later.
                if kind == StreamedOutputKind::Text
                    && let Some(id) = update.get("messageId").and_then(Value::as_str)
                    && let Some(step) = self.step.as_mut()
                    && step.message_ids.last().map(String::as_str) != Some(id)
                {
                    step.message_ids.push(id.to_owned());
                }
                Ok(())
            }
            // A whole message: what its chunks did not already stream. Agents
            // that stream chunks repeat them here, which adds nothing.
            "agent_message" | "agent_thought" => {
                let streamed = update
                    .get("messageId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| self.round.streamed.contains(id));
                if streamed {
                    return Ok(());
                }
                let chunk = if kind == "agent_message" {
                    "agent_message_chunk"
                } else {
                    "agent_thought_chunk"
                };
                for block in update
                    .get("content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                {
                    let mut part = update.clone();
                    part["sessionUpdate"] = Value::from(chunk);
                    part["content"] = block;
                    Box::pin(self.update(part)).await?;
                }
                Ok(())
            }
            // The echo of a prompt the agent took in: the work it starts is
            // still to come. The prompt itself is already recorded.
            "user_message" | "user_message_chunk" => {
                if let Some(id) = update.get("messageId").and_then(Value::as_str) {
                    self.round.awaiting_echo.remove(id);
                    self.round.echoed.insert(id.to_owned());
                }
                self.round.idle = None;
                Ok(())
            }
            "state_update" => {
                match update.get("state").and_then(Value::as_str) {
                    Some("idle") => self.round.idle = Some(Idle::of(&update)),
                    Some("running" | "requires_action") => self.round.idle = None,
                    _ => {}
                }
                Ok(())
            }
            // The agent's own subagent: its cookie session, a child of this
            // one; its work arrives in that session.
            "subagent_update" => {
                self.engine
                    .acp_subagent_update(&self.live, Some(self.run), &update)
                    .await;
                Ok(())
            }
            // A message between the agent's sessions, such as the task a
            // subagent got or the report it gave back: shown where it
            // arrives, never sent anywhere.
            "session_message" => {
                let text = update
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(wire::content_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = text.trim();
                if text.is_empty() {
                    return Ok(());
                }
                let to_self = update.get("recipientSessionId").and_then(Value::as_str)
                    == Some(self.live.acp_session_id.as_str());
                let (direction, peer) = if to_self {
                    (AcpMessageDirection::Received, "senderSessionId")
                } else {
                    (AcpMessageDirection::Sent, "recipientSessionId")
                };
                let peer = update
                    .get(peer)
                    .and_then(Value::as_str)
                    .and_then(|peer| self.engine.acp_peer_label(self.session(), peer));
                self.record_update(AcpSessionUpdate {
                    session_message: Some(AcpSessionMessage {
                        direction,
                        peer: peer.map(|peer| truncate(&peer, 512)),
                        text: truncate(text, 64 * 1024),
                    }),
                    ..AcpSessionUpdate::default()
                })
                .await
            }
            "tool_call_update" => {
                let fields: ToolCallFields =
                    serde_json::from_value(update.clone()).unwrap_or_default();
                self.tool_call(fields).await
            }
            // One more content item of a call: its content so far, and it.
            "tool_call_content_chunk" => {
                let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                    return Ok(());
                };
                let Some(item) = update.get("content").cloned() else {
                    return Ok(());
                };
                let mut content = self
                    .calls
                    .get(id)
                    .map(|call| &call.fields)
                    .or_else(|| self.echoes.get(id).map(|echo| &echo.fields))
                    .and_then(|fields| fields.content.clone())
                    .unwrap_or_default();
                content.push(item);
                self.tool_call(ToolCallFields {
                    tool_call_id: id.to_owned(),
                    content: Some(content),
                    ..ToolCallFields::default()
                })
                .await
            }
            "terminal_update" | "terminal_output_chunk" => {
                let Some(read) = terminal::TerminalUpdate::read(&kind, &update) else {
                    return Ok(());
                };
                if let Some(exit) = read.exit.clone()
                    && let Some(ended) =
                        self.engine
                            .acp_terminal_exited(self.session(), &read.id, exit, false)
                {
                    self.record_update(AcpSessionUpdate {
                        background_ended: Some(ended),
                        ..AcpSessionUpdate::default()
                    })
                    .await?;
                }
                let route = self.terminals.entry(read.id.clone()).or_default();
                let meta = read.meta(route.written);
                route.written += meta.output.as_ref().map_or(0, String::len);
                match route.call.clone() {
                    Some(call) => {
                        self.show_call_with(
                            ToolCallFields {
                                tool_call_id: call,
                                ..ToolCallFields::default()
                            },
                            meta,
                        )
                        .await
                    }
                    None => {
                        route.waiting.absorb(meta);
                        Ok(())
                    }
                }
            }
            "plan_update" => {
                let plan = update.get("plan").cloned().unwrap_or(Value::Null);
                self.record_update(AcpSessionUpdate {
                    plan: Some(Open::Known(plan_block(&plan))),
                    ..AcpSessionUpdate::default()
                })
                .await
            }
            "plan_removed" => {
                self.record_update(AcpSessionUpdate {
                    plan: Some(Open::Known(plan_block(&Value::Null))),
                    ..AcpSessionUpdate::default()
                })
                .await
            }
            _ => self.metadata(&kind, &update).await,
        }
    }

    /// An update about the session rather than the turn.
    async fn metadata(&mut self, kind: &str, update: &Value) -> Result<(), EngineError> {
        match kind {
            "available_commands_update" => {
                self.record_update(AcpSessionUpdate {
                    commands: Some(commands_of(update)),
                    ..AcpSessionUpdate::default()
                })
                .await
            }
            "config_option_update" => {
                let options = update
                    .get("configOptions")
                    .and_then(Value::as_array)
                    .cloned();
                let options = options_of(options.as_ref());
                self.live.options = options.clone();
                self.engine.remember_live(&self.live);
                self.record_update(AcpSessionUpdate {
                    options: Some(options),
                    ..AcpSessionUpdate::default()
                })
                .await
            }
            "session_info_update" => {
                if let Some(title) = update.get("title").and_then(Value::as_str) {
                    self.engine
                        .set_acp_title(self.session(), self.run, title)
                        .await?;
                }
                Ok(())
            }
            // The agent's context use: what the clients show as the
            // session's context, recorded as it changes.
            "usage_update" => {
                self.usage_update = Some(update.clone());
                match context_of(update).filter(|context| self.context != Some(*context)) {
                    Some(context) => {
                        self.context = Some(context);
                        self.record_update(AcpSessionUpdate {
                            context: Some(context),
                            ..AcpSessionUpdate::default()
                        })
                        .await
                    }
                    None => Ok(()),
                }
            }
            // The session notices RFD: an advisory line for the user.
            "notice" => {
                let text = |name: &str| update.get(name).and_then(Value::as_str);
                let severity = match text("severity") {
                    Some("error") => AcpNoticeSeverity::Error,
                    Some("warning") => AcpNoticeSeverity::Warning,
                    _ => AcpNoticeSeverity::Info,
                };
                let Some(title) = text("title").filter(|title| !title.trim().is_empty()) else {
                    return Ok(());
                };
                self.notice(severity, title, text("description")).await
            }
            // The session compaction RFD: a compaction's summary streams,
            // then its terminal status says how it went.
            "compaction_summary_chunk" => {
                if let (Some(id), Some(content)) = (
                    update.get("compactionId").and_then(Value::as_str),
                    update.get("content"),
                ) {
                    self.compactions
                        .entry(id.to_owned())
                        .or_default()
                        .summary
                        .push_str(&wire::content_text(content));
                }
                Ok(())
            }
            // Recorded once as it starts and once as it ends: the agent may
            // repeat its end, such as Claude Code's adapter with the
            // compaction's token counts.
            "compaction_update" => {
                let id = update
                    .get("compactionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let state = match update.get("status").and_then(Value::as_str) {
                    Some("in_progress") => AcpCompactionState::InProgress,
                    Some("completed") => AcpCompactionState::Completed,
                    Some("failed") => AcpCompactionState::Failed,
                    Some("cancelled") => AcpCompactionState::Cancelled,
                    _ => return Ok(()),
                };
                let progress = self.compactions.entry(id.clone()).or_default();
                if progress.ended || (state == AcpCompactionState::InProgress && progress.started) {
                    return Ok(());
                }
                // An abandoned compaction that was never shown leaves nothing
                // to close.
                if state == AcpCompactionState::Cancelled && !progress.started {
                    progress.ended = true;
                    return Ok(());
                }
                if state == AcpCompactionState::InProgress {
                    progress.started = true;
                } else {
                    progress.ended = true;
                }
                let streamed = std::mem::take(&mut progress.summary);
                let summary = (state == AcpCompactionState::Completed)
                    .then(|| {
                        update
                            .get("summary")
                            .and_then(Value::as_array)
                            .map(|blocks| {
                                blocks
                                    .iter()
                                    .map(wire::content_text)
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .filter(|summary| !summary.trim().is_empty())
                            .unwrap_or(streamed)
                    })
                    .map(|summary| truncate(summary.trim(), 64 * 1024))
                    .filter(|summary| !summary.is_empty());
                let notice = (state == AcpCompactionState::Failed).then(|| {
                    let error = update.get("error").map(|error| match error {
                        Value::String(error) => error.clone(),
                        other => other
                            .get("message")
                            .and_then(Value::as_str)
                            .map_or_else(|| other.to_string(), str::to_owned),
                    });
                    AcpNotice {
                        severity: AcpNoticeSeverity::Warning,
                        title: "The agent could not compact its conversation".to_owned(),
                        description: error
                            .map(|error| truncate(error.trim(), 16 * 1024))
                            .filter(|error| !error.is_empty()),
                    }
                });
                self.record_update(AcpSessionUpdate {
                    compaction: Some(AcpCompaction {
                        compaction_id: truncate(&id, 256),
                        state,
                        summary,
                    }),
                    notice,
                    ..AcpSessionUpdate::default()
                })
                .await
            }
            _ => Ok(()),
        }
    }

    /// Records a notice of the agent's.
    async fn notice(
        &self,
        severity: AcpNoticeSeverity,
        title: &str,
        description: Option<&str>,
    ) -> Result<(), EngineError> {
        self.record_update(AcpSessionUpdate {
            notice: Some(AcpNotice {
                severity,
                title: truncate(title.trim(), 1024),
                description: description
                    .map(|description| truncate(description.trim(), 16 * 1024))
                    .filter(|description| !description.is_empty()),
            }),
            ..AcpSessionUpdate::default()
        })
        .await
    }

    async fn tool_call(&mut self, update: ToolCallFields) -> Result<(), EngineError> {
        if update.tool_call_id.is_empty() {
            return Ok(());
        }
        if let Some(echo) = self.echoes.get_mut(&update.tool_call_id) {
            echo.fields = permissions::merge(Some(&echo.fields), &update);
            let ended = matches!(echo.fields.status.as_deref(), Some("completed" | "failed"));
            if echo.claimed || !ended {
                return Ok(());
            }
            // It ended without reaching cookie, such as a call the agent
            // could not make: shown as the agent's call, with its error.
            let echo = self
                .echoes
                .remove(&update.tool_call_id)
                .expect("echo just found");
            return self.show_call(echo.fields).await;
        }
        if !self.calls.contains_key(&update.tool_call_id)
            && let Some(tool) = permissions::bridge_tool_of(&update)
        {
            let claimed = self
                .unclaimed_bridge_calls
                .iter()
                .position(|unclaimed| *unclaimed == tool)
                .map(|index| self.unclaimed_bridge_calls.remove(index))
                .is_some();
            let order = self.echoes.len() as u64;
            self.echoes.insert(
                update.tool_call_id.clone(),
                BridgeEcho {
                    tool,
                    fields: permissions::merge(None, &update),
                    order,
                    claimed,
                },
            );
            return Ok(());
        }
        self.show_call(update).await
    }

    /// Shows the agent's tool call `update`, or updates the one shown,
    /// with the output that came for a terminal its content names.
    async fn show_call(&mut self, update: ToolCallFields) -> Result<(), EngineError> {
        let mut terminal_meta = terminal::TerminalMeta::default();
        if let Some(id) = terminal::named(update.content.as_ref()) {
            let route = self.terminals.entry(id.clone()).or_default();
            if route.call.is_none() {
                route.call = Some(update.tool_call_id.clone());
                terminal_meta = std::mem::take(&mut route.waiting);
            }
            terminal_meta.id = Some(id);
        }
        self.show_call_with(update, terminal_meta).await
    }

    /// [`Self::show_call`] with `terminal_meta` to apply to its terminal.
    async fn show_call_with(
        &mut self,
        update: ToolCallFields,
        terminal_meta: terminal::TerminalMeta,
    ) -> Result<(), EngineError> {
        // Nothing but output: the live display grows by appends.
        let output_only = update.title.is_none()
            && update.kind.is_none()
            && update.status.is_none()
            && update.content.is_none()
            && update.locations.is_none()
            && update.raw_input.is_none()
            && update.raw_output.is_none();
        if let Some(shown) = self.calls.get_mut(&update.tool_call_id) {
            if shown.terminated {
                return Ok(());
            }
            shown.fields = permissions::merge(Some(&shown.fields), &update);
            let fields = shown.fields.clone();
            let tool_call_id = shown.tool_call_id;
            // Its title, kind, or raw input can arrive after it started
            // (Claude Code's adapter sends the raw input in an update).
            let presentation = presentation_of(&fields);
            if presentation != shown.presented {
                shown.presented = presentation.clone();
                self.record_update(AcpSessionUpdate {
                    tool_presentation: Some(cookie_agent_protocol::AcpToolPresentation {
                        tool_call_id,
                        presentation,
                    }),
                    ..AcpSessionUpdate::default()
                })
                .await?;
            }
            let appends = self
                .feed_terminal(&fields.tool_call_id, terminal_meta)
                .await;
            let index = self
                .calls
                .get(&fields.tool_call_id)
                .and_then(|shown| shown.terminal_index);
            if output_only && let Some(index) = index {
                self.append_output(tool_call_id, &fields, index, appends);
                return Ok(());
            }
            return self.progress_or_end(&fields.tool_call_id).await;
        }
        let fields = permissions::merge(None, &update);
        let mut call_id = fields.tool_call_id.clone();
        while !self.used_call_ids.insert(call_id.clone()) {
            call_id.push('\'');
        }
        let model_call_id = ModelCallId::new(call_id).map_err(|error| {
            EngineError::Acp(format!("the agent's tool call ID is unusable: {error}"))
        })?;
        self.ensure_step().await?;
        let step = self.step.as_mut().expect("step started");
        step.parts.push(PersistedAssistantPart::ToolCall {
            id: model_call_id.clone(),
            provider_item_id: None,
            name: kind_name(fields.kind.as_deref()),
            input: fields
                .raw_input
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
            raw_input: None,
            metadata: None,
        });
        let content_index = (step.parts.len() - 1) as u32;
        let model_turn_seq = self.commit(ModelFinishReason::ToolCalls).await?;
        let owner = AssistantToolCallRef {
            model_turn_seq,
            content_index,
            model_call_id,
            provider_item_id: None,
        };
        let tool_call_id = ToolCallId::new_v7();
        let name = kind_name(fields.kind.as_deref()).as_str().to_owned();
        let presented = presentation_of(&fields);
        let presentation = ToolPresentation::new(
            presented.clone(),
            ToolIdentity {
                published_name: fields
                    .tool_name()
                    .map(|name| name.chars().take(128).collect())
                    .unwrap_or_else(|| name.clone()),
                name,
                source: Some(Open::Known(ToolSource::Acp {
                    agent: self.live.agent.id.clone(),
                    runtime_id: self.live.agent.runtime.clone(),
                })),
            },
        );
        let fingerprint = super::super::tool_execution::fallback_operation_fingerprint(
            &cookie_agent_runtime_api::ToolCall {
                id: tool_call_id,
                name: format!("acp_{}", fields.kind.as_deref().unwrap_or("other")),
                arguments: fields.raw_input.clone().unwrap_or(Value::Null),
            },
            None,
        );
        self.engine
            .append(
                self.session(),
                Some(self.run),
                origin(),
                Event::ToolCallStarted {
                    start: ToolCallStart {
                        tool_call_id,
                        output: ToolOutputDeclaration::default(),
                        owner,
                        presentation,
                        operation_fingerprint: fingerprint,
                        background: false,
                    },
                },
            )
            .await?;
        self.calls.insert(
            fields.tool_call_id.clone(),
            ShownCall {
                tool_call_id,
                fields: fields.clone(),
                presented,
                terminated: false,
                terminal: None,
                terminal_index: None,
            },
        );
        self.feed_terminal(&fields.tool_call_id, terminal_meta)
            .await;
        self.progress_or_end(&fields.tool_call_id).await
    }

    /// Applies what an update says of call `acp_id`'s terminal, opening the
    /// terminal on its first mention; returns the appends its new output
    /// makes to the live display.
    async fn feed_terminal(&mut self, acp_id: &str, meta: terminal::TerminalMeta) -> Vec<String> {
        if meta.is_empty() {
            return Vec::new();
        }
        let session = self.active.session;
        let limits = self.active.policy.result_limits.tool_output;
        let Some(shown) = self.calls.get_mut(acp_id) else {
            return Vec::new();
        };
        if shown.terminal.is_none() {
            let id = meta.id.clone().unwrap_or_else(|| acp_id.to_owned());
            shown.terminal =
                Some(Terminal::open(self.engine, session, shown.tool_call_id, id, limits).await);
        }
        let terminal = shown.terminal.as_mut().expect("opened above");
        let appends = match &meta.output {
            Some(output) => terminal.write(output).await,
            None => Vec::new(),
        };
        if let Some(exit) = meta.exit {
            terminal.exit = Some(exit);
        }
        appends
    }

    /// Appends output to the terminal at `index` of the call's live display.
    fn append_output(
        &self,
        tool_call_id: ToolCallId,
        fields: &ToolCallFields,
        index: u32,
        appends: Vec<String>,
    ) {
        let message = display_text(fields.title.as_deref().unwrap_or("ACP tool call"));
        for text in appends {
            self.engine.publish_transient(
                self.session(),
                self.run,
                Event::ToolCallProgress {
                    tool_call_id,
                    message: message.clone(),
                    display: Some(Open::Known(cookie_agent_protocol::DisplayDelta::Append {
                        index,
                        text,
                        stream: Some(cookie_agent_protocol::TerminalStream::Stdout),
                    })),
                },
            );
        }
    }

    /// Shows the new state of the call the agent knows as `acp_id`, or ends
    /// it once it completed or failed.
    async fn progress_or_end(&mut self, acp_id: &str) -> Result<(), EngineError> {
        let session = self.session();
        let Some(shown) = self.calls.get_mut(acp_id) else {
            return Ok(());
        };
        match shown.fields.status.as_deref() {
            Some("completed") => self.end_call(acp_id, true).await,
            Some("failed") => self.end_call(acp_id, false).await,
            _ => {
                let fields = &shown.fields;
                let tool_call_id = shown.tool_call_id;
                let (blocks, _) = self.engine.call_content(
                    session,
                    fields,
                    shown.terminal.as_ref(),
                    &self.live.cwd,
                );
                shown.terminal_index = blocks
                    .iter()
                    .position(|block| matches!(block, DisplayBlock::Terminal { .. }))
                    .and_then(|index| u32::try_from(index).ok());
                let message = display_text(fields.title.as_deref().unwrap_or("ACP tool call"));
                self.engine.publish_transient(
                    self.session(),
                    self.run,
                    Event::ToolCallProgress {
                        tool_call_id,
                        message,
                        display: (!blocks.is_empty()).then(|| {
                            Open::Known(cookie_agent_protocol::DisplayDelta::Replace {
                                blocks: blocks.into_iter().map(Open::Known).collect(),
                            })
                        }),
                    },
                );
                Ok(())
            }
        }
    }

    /// Terminates the call the agent knows as `acp_id`: completed when
    /// `succeeded`, failed otherwise.
    async fn end_call(&mut self, acp_id: &str, succeeded: bool) -> Result<(), EngineError> {
        let Some(shown) = self.calls.get_mut(acp_id) else {
            return Ok(());
        };
        if shown.terminated {
            return Ok(());
        }
        shown.terminated = true;
        let session = self.active.session;
        let (mut blocks, mut text) = self.engine.call_content(
            session,
            &shown.fields,
            shown.terminal.as_ref(),
            &self.live.cwd,
        );
        // A command still running when its call ends runs on in the
        // background, as Claude Code's backgrounded commands do.
        let backgrounded = shown
            .terminal
            .as_ref()
            .filter(|terminal| terminal.exit.is_none())
            .map(|terminal| {
                let command = shown
                    .fields
                    .raw_input
                    .as_ref()
                    .and_then(|input| input.get("command"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                (
                    terminal.id.clone(),
                    shown.tool_call_id,
                    shown.fields.tool_name().map_or_else(
                        || kind_name(shown.fields.kind.as_deref()).as_str().to_owned(),
                        str::to_owned,
                    ),
                    command.or_else(|| shown.fields.title.clone()),
                )
            });
        let terminal = shown.terminal.take();
        if let Some(output) = &shown.fields.raw_output {
            if text.is_empty() {
                text = match output {
                    Value::String(output) => output.clone(),
                    other => other.to_string(),
                };
            }
            if shown.fields.content.as_ref().is_none_or(Vec::is_empty) {
                blocks.extend(raw_output_block(output));
            }
        }
        let title = display_text(shown.fields.title.as_deref().unwrap_or("ACP tool call"));
        let tool_call_id = shown.tool_call_id;
        let mut result = PersistedToolResult {
            title,
            output: truncate(&text, PersistedToolResult::MAX_OUTPUT_BYTES),
            display: cookie_agent_protocol::fit_result_display(
                blocks.into_iter().map(Open::Known).collect(),
            ),
            background: None,
            retained_output: None,
            metadata: serde_json::json!({
                "acp": {
                    "kind": shown.fields.kind,
                    "raw_input": shown.fields.raw_input,
                    "raw_output": shown.fields.raw_output,
                    "tool_name": shown.fields.tool_name(),
                }
            }),
            attachments: Vec::new(),
            additional_messages: Vec::new(),
            loaded_tools: Vec::new(),
        };
        // A command's output is the call's output, retained as a native
        // command's is; a failed call keeps it too.
        let mut failure = None;
        let mut partial = false;
        if let Some(terminal) = terminal {
            failure = terminal.failure();
            partial = true;
            // A failed call's output counts as incomplete, as the session
            // records every failed call's (and so keeps showing it).
            result = terminal.finish(self.engine, result, !succeeded).await;
            if result.output.is_empty() && !text.is_empty() {
                result.output = truncate(&text, PersistedToolResult::MAX_OUTPUT_BYTES);
            }
        }
        let outcome = if succeeded {
            Ok(result)
        } else {
            Err(ToolFailure {
                code: ToolCallFailureCode::ExecutionFailed,
                message: match failure {
                    _ if !text.is_empty() => truncate(&text, 4096),
                    Some(failure) => failure,
                    None => "the agent's tool call failed".to_owned(),
                },
                partial_output: partial.then(|| Box::new(result)),
            })
        };
        self.engine
            .submit_tool_result_status(self.session(), self.run, tool_call_id, outcome, false)
            .await?;
        if let Some((terminal, tool_call_id, tool, display)) = backgrounded {
            self.engine
                .acp_command_backgrounded(&self.live, terminal, tool_call_id, tool, display);
        }
        Ok(())
    }

    /// Ends every call the agent left open, as the round ends.
    async fn end_open_calls(&mut self, cancelled: bool) -> Result<(), EngineError> {
        let open = self
            .calls
            .iter()
            .filter(|(_, call)| !call.terminated)
            .map(|(id, call)| (id.clone(), call.tool_call_id))
            .collect::<Vec<_>>();
        for (acp_id, tool_call_id) in open {
            let mut partial_output = None;
            if let Some(call) = self.calls.get_mut(&acp_id) {
                call.terminated = true;
                // The output a command streamed before the round ended.
                if let Some(terminal) = call.terminal.take() {
                    let result = PersistedToolResult {
                        title: display_text(
                            call.fields.title.as_deref().unwrap_or("ACP tool call"),
                        ),
                        output: String::new(),
                        display: cookie_agent_protocol::fit_result_display(
                            terminal.blocks().into_iter().map(Open::Known).collect(),
                        ),
                        background: None,
                        retained_output: None,
                        metadata: serde_json::json!({"acp": {"kind": call.fields.kind}}),
                        attachments: Vec::new(),
                        additional_messages: Vec::new(),
                        loaded_tools: Vec::new(),
                    };
                    partial_output =
                        Some(Box::new(terminal.finish(self.engine, result, true).await));
                }
            }
            self.engine
                .submit_tool_result_status(
                    self.session(),
                    self.run,
                    tool_call_id,
                    Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: if cancelled {
                            "the turn was cancelled before the call finished".to_owned()
                        } else {
                            "the agent ended its turn without finishing the call".to_owned()
                        },
                        partial_output,
                    }),
                    cancelled,
                )
                .await?;
        }
        Ok(())
    }

    /// Runs a call of cookie's tool `name` that the agent made through the
    /// bridge as a call of this turn: the turn so far commits with the call
    /// in it, the call starts as cookie's tool calls do, and it runs through
    /// the rules, approvals, and execution of a model's call; `reply` gets
    /// its result.
    async fn bridge_call(
        &mut self,
        name: String,
        arguments: Value,
        reply: tokio::sync::oneshot::Sender<Value>,
    ) -> Result<(), EngineError> {
        // This call stands for the agent's oldest unclaimed call of the
        // tool, which is then not shown; one the agent reports later
        // claims it instead.
        let echo = self
            .echoes
            .values_mut()
            .filter(|echo| !echo.claimed && echo.tool == name)
            .min_by_key(|echo| echo.order);
        match echo {
            Some(echo) => echo.claimed = true,
            None => self.unclaimed_bridge_calls.push(name.clone()),
        }
        let selection = cookie_agent_protocol::acp_model_selection(&self.live.agent.id);
        let turn_context = Arc::new(cookie_agent_runtime_api::TurnAgentContext {
            agent: self.live.agent.id.clone(),
            model: selection.model,
            adapter: cookie_agent_protocol::AdaptorId::new(crate::policy::ACP_ADAPTER)
                .expect("static adapter"),
            input_media: Vec::new(),
        });
        let tool_call_id = ToolCallId::new_v7();
        let call = cookie_agent_runtime_api::ToolCall {
            id: tool_call_id,
            name: name.clone(),
            arguments: arguments.clone(),
        };
        let prepared = self
            .engine
            .prepare_tool_call(
                self.session(),
                self.run,
                call,
                &self.active.policy,
                Arc::clone(&turn_context),
            )
            .await;
        let model_call_id = ModelCallId::new(format!("cookie:{tool_call_id}"))
            .map_err(|error| EngineError::Acp(error.to_string()))?;
        let tool_name = SafeCode::new(
            name.to_ascii_lowercase()
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                        character
                    } else {
                        '_'
                    }
                })
                .collect::<String>(),
        )
        .unwrap_or_else(|_| SafeCode::new("tool").expect("static code"));
        self.ensure_step().await?;
        let step = self.step.as_mut().expect("step started");
        step.parts.push(PersistedAssistantPart::ToolCall {
            id: model_call_id.clone(),
            provider_item_id: None,
            name: tool_name,
            input: arguments,
            raw_input: None,
            metadata: None,
        });
        let content_index = (step.parts.len() - 1) as u32;
        let model_turn_seq = self.commit(ModelFinishReason::ToolCalls).await?;
        let operation_fingerprint = prepared.prepared.as_ref().map_or_else(
            |_| {
                super::super::tool_execution::fallback_operation_fingerprint(
                    &prepared.call,
                    prepared.permission_name.as_deref(),
                )
            },
            |ready| {
                cookie_agent_protocol::OperationFingerprint::from_prepared_operation(
                    ready.operation(),
                )
            },
        );
        self.engine
            .append(
                self.session(),
                Some(self.run),
                origin(),
                Event::ToolCallStarted {
                    start: ToolCallStart {
                        tool_call_id,
                        output: ToolOutputDeclaration::default(),
                        owner: AssistantToolCallRef {
                            model_turn_seq,
                            content_index,
                            model_call_id,
                            provider_item_id: None,
                        },
                        presentation: prepared.presentation.clone(),
                        operation_fingerprint,
                        background: false,
                    },
                },
            )
            .await?;
        let engine = self.engine.clone();
        let active = Arc::clone(self.active);
        let run = self.run;
        self.bridged.push(tokio::spawn(async move {
            let result = engine
                .execute_tool(Arc::clone(&active), run, prepared, turn_context)
                .await;
            let answer = match &result {
                Ok(result) => {
                    let mut text = crate::model_history::tool_result_text(result);
                    if text.is_empty() {
                        text = result.title.as_str().to_owned();
                    }
                    serde_json::json!({"content": [{"type": "text", "text": text}], "isError": false})
                }
                Err(failure) => serde_json::json!({
                    "content": [{"type": "text", "text": failure.message}],
                    "isError": true,
                }),
            };
            let cancelled = active.cancellation.is_cancelled();
            let _ = engine
                .submit_tool_result_status(active.session, run, tool_call_id, result, cancelled)
                .await;
            let _ = reply.send(answer);
        }));
        Ok(())
    }

    /// Asks the user an `elicitation/create` of the agent's.
    fn elicitation(&mut self, create: elicitation::Create, incoming: IncomingRequest) {
        let tool_call_id = create
            .tool_call_id()
            .and_then(|id| self.calls.get(id))
            .map(|call| call.tool_call_id);
        let asked = self.engine.ask_acp_question(
            Arc::clone(self.active),
            self.run,
            self.live.agent.id.clone(),
            tool_call_id,
            &self.live.process,
            create,
            incoming,
        );
        self.bridged.push(asked);
    }

    /// Answers a permission request, by the agent's rules and approvals.
    fn permission(
        &self,
        request: wire::PermissionRequest,
        incoming: cookie_agent_runtime_api::rpc::IncomingRequest,
    ) {
        let engine = self.engine.clone();
        let active = Arc::clone(self.active);
        let run = self.run;
        let agent = self.live.agent.clone();
        let cwd = self.live.cwd.clone();
        let home = self.home.clone();
        let known = self.calls.get(&request.tool_call.tool_call_id);
        // A call of cookie's tools that is not shown is known all the same.
        let known_fields = known.map(|call| call.fields.clone()).or_else(|| {
            self.echoes
                .get(&request.tool_call.tool_call_id)
                .map(|echo| echo.fields.clone())
        });
        let tool_call_id = known.map(|call| call.tool_call_id);
        // A call cookie has not seen is named by the request's own title.
        let mut request = request;
        if known_fields.is_none() && request.tool_call.title.is_none() {
            request.tool_call.title = request.title.clone();
        }
        tokio::spawn(async move {
            let answer = tokio::select! {
                answer = engine.decide_acp_permission(
                    &active,
                    run,
                    &agent,
                    &cwd,
                    home.as_deref(),
                    &request,
                    known_fields.as_ref(),
                    tool_call_id,
                ) => answer,
                () = active.cancellation.cancelled() => wire::permission_outcome(None),
            };
            let answer = if active.cancellation.is_cancelled() {
                wire::permission_outcome(None)
            } else {
                answer
            };
            let _ = incoming.respond(&answer).await;
        });
    }
}

/// The MCP result of a call through the bridge outside the agent's turn.
fn bridge_refusal() -> Value {
    serde_json::json!({
        "content": [{"type": "text", "text": "cookie's tools can be called only during the agent's turns"}],
        "isError": true,
    })
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

impl Engine {
    /// Runs `active`, a run of an ACP agent, to its end.
    pub(in crate::runtime) async fn run_acp_loop(
        &self,
        run_id: RunId,
        active: Arc<ActiveRun>,
    ) -> Result<(), EngineError> {
        let offers =
            active.policy.acp_agent().cloned().ok_or_else(|| {
                EngineError::Acp("the run's agent is not an ACP agent".to_owned())
            })?;
        let agent = self.place_acp_agent(active.session, &offers)?;
        let document = active
            .policy
            .registry
            .get(&agent.id)
            .map(|agent| agent.document.clone())
            .ok_or_else(|| EngineError::InvalidRuntimeAgent(agent.id.clone()))?;
        let (process, _use) = self
            .inner
            .acp
            .processes
            .acquire(&self.inner, &agent)
            .await
            .map_err(EngineError::Acp)?;
        let live = self
            .acp_session(active.session, Some(run_id), &agent, &document, &process)
            .await?;
        let home = process.client.home();
        let hide = document
            .frontmatter
            .acp
            .clone()
            .unwrap_or_default()
            .hidden_tools();
        let bridge_tools = self
            .bridged_tool_definitions(active.session, &active.policy, &hide)
            .ok()
            .map(|tools| tools.into_iter().map(|tool| tool.name).collect());
        let mut turn = Turn {
            engine: self,
            run: run_id,
            active: &active,
            live,
            step: None,
            next_attempt: 1,
            retries: 0,
            calls: HashMap::new(),
            echoes: HashMap::new(),
            unclaimed_bridge_calls: Vec::new(),
            used_call_ids: Default::default(),
            last_turn_seq: None,
            last_text: String::new(),
            input_through_seq: 0,
            claim: None,
            usage_update: None,
            context: None,
            home,
            bridged: Vec::new(),
            round: RoundState::default(),
            terminals: HashMap::new(),
            steer_claims: Vec::new(),
            unsteered: false,
            declined_steers: HashSet::new(),
            compactions: HashMap::new(),
            bridge_tools,
        };
        loop {
            if active.cancellation.is_cancelled() {
                // A stop before the round still reaches the agent: a wake's
                // work (a subagent's, say) is under way without a prompt.
                let _ = process
                    .peer
                    .notify(
                        wire::method::SESSION_CANCEL,
                        &wire::SessionRef {
                            session_id: &turn.live.acp_session_id,
                        },
                    )
                    .await;
                self.drop_stopped_acp_wakes(active.session).await?;
                self.append_run_cancelled_once(&active, run_id, None)?;
                return Ok(());
            }
            let claim = self.prompt_events(active.session, run_id).await?;
            let context = if turn.live.first_prompt {
                self.acp_first_prompt_context(
                    active.session,
                    &active.policy,
                    &claim.events,
                    &document,
                    &turn.live,
                )?
            } else {
                Vec::new()
            };
            // What the agent reads beyond the conversation is on the record,
            // so the user sees it.
            if !context.is_empty() {
                self.append(
                    active.session,
                    Some(run_id),
                    origin(),
                    Event::AcpSessionUpdated {
                        update: AcpSessionUpdate {
                            prompt_context: Some(context.clone()),
                            ..AcpSessionUpdate::default()
                        },
                    },
                )
                .await?;
            }
            let prompt = self.acp_prompt(&claim.events, &mut turn.live, context, &process);
            // The agent's own subagent works only on what its parent gives
            // it: cookie has no prompt to send it.
            if !prompt.is_empty() && super::is_acp_subagent(&claim.events) {
                return Err(EngineError::Acp(format!(
                    "this is `{}`'s own subagent, which takes no messages; send them to its parent",
                    turn.live.agent.id
                )));
            }
            turn.input_through_seq = claim.events.last().map_or(0, |event| event.seq);
            turn.claim = Some(claim);
            turn.last_text.clear();
            turn.last_turn_seq = None;
            turn.usage_update = None;
            let end = self.acp_round(&mut turn, prompt).await?;
            // A cancel accepted while the agent was finishing its turn ends
            // the run as cancelled, as it does a model run's, rather than
            // completing it or taking more input.
            let end = match end {
                RoundEnd::Completed(_) if active.cancellation.is_cancelled() => RoundEnd::Cancelled,
                end => end,
            };
            match end {
                RoundEnd::Completed(final_text) => {
                    // A title is a nicety: a failure leaves the session
                    // untitled, as a model run's does.
                    let _ = self
                        .acp_fallback_title(&active, run_id, turn.input_through_seq)
                        .await;
                    // Once the agent has replied in a session, its process
                    // can be asked whether its fork takes a fork point.
                    let engine = self.clone();
                    let live = turn.live.clone();
                    tokio::spawn(async move { engine.probe_fork_point(&live).await });
                    // Input the agent did not take into its turn goes with
                    // the next round.
                    let unsteered = std::mem::take(&mut turn.unsteered);
                    let promoted = self
                        .request(active.session, |reply| {
                            SessionCommand::PromotePendingOrComplete {
                                run: run_id,
                                final_text,
                                complete_if_empty: !unsteered,
                                reply,
                            }
                        })
                        .await?;
                    if !promoted && !unsteered {
                        return Ok(());
                    }
                }
                RoundEnd::Cancelled => {
                    if let Some(claim) = turn.claim.as_mut() {
                        claim.release().await?;
                    }
                    self.drop_stopped_acp_wakes(active.session).await?;
                    self.append_run_cancelled_once(&active, run_id, None)?;
                    return Ok(());
                }
                RoundEnd::Failed(reason)
                | RoundEnd::Stopped {
                    message: reason, ..
                } => return Err(EngineError::Acp(reason)),
            }
        }
    }

    /// One prompt round: sends `prompt` and records what the agent reports
    /// until its answer.
    async fn acp_round(
        &self,
        turn: &mut Turn<'_>,
        prompt: Vec<Value>,
    ) -> Result<RoundEnd, EngineError> {
        let events = Arc::clone(&turn.live.events);
        // The listener between runs lets go of the session's updates.
        turn.live
            .waiting
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        turn.live.wanted.notify_one();
        let mut events = events.lock().await;
        turn.live
            .waiting
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        let (rounds, round_taken) = &*turn.live.rounds;
        rounds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        round_taken.notify_waiters();
        let listening = prompt.is_empty();
        // Input to steer wakes the round: a prompt during the work. Watched
        // from here, so input that arrives while the round takes in what
        // came before is not missed; input already waiting is looked for
        // once the work can take it.
        let steer_wake = self.inner.acp.processes.watch_steering(turn.session());
        steer_wake.notify_one();
        // What arrived between runs: the listener's, then what is still
        // queued. A wake's work is this round's; for a prompt it is work the
        // agent did meanwhile, recorded before the prompt.
        let carried = std::mem::take(
            &mut *turn
                .live
                .carried
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for event in carried {
            turn.take_event(event, false).await?;
        }
        while let Ok(event) = events.try_recv() {
            turn.take_event(event, false).await?;
        }
        let process = Arc::clone(&turn.live.process);
        let session_id = turn.live.acp_session_id.clone();
        let prompt_request = wire::PromptRequest {
            session_id: &session_id,
            prompt: &prompt,
        };
        let request = process
            .peer
            .request::<_, wire::PromptResponse>(wire::method::SESSION_PROMPT, &prompt_request);
        tokio::pin!(request);
        let mut cancel_deadline: Option<tokio::time::Instant> = None;
        let mut pending_steer: Option<PendingSteer> = None;
        // The prompt's answer says the agent took it in; the round ends at
        // the `idle` after it, with no steer on its way. A listening round
        // (a wake's) sends no prompt and ends at its work's `idle`.
        if !listening {
            turn.round = RoundState::default();
        }
        let mut answered = listening;
        let answer = loop {
            if answered && pending_steer.is_none() && turn.round.ended() {
                break Some(Ok(()));
            }
            // Biased, cancellation first: an agent that streams without a
            // pause keeps the update branch ready, and a cancel accepted
            // meanwhile must still reach it at the next turn of the loop.
            tokio::select! {
                biased;
                () = turn.active.cancellation.cancelled(), if cancel_deadline.is_none() => {
                    cancel_deadline = Some(tokio::time::Instant::now() + CANCEL_GRACE);
                    let _ = process.peer.notify(
                        wire::method::SESSION_CANCEL,
                        &wire::SessionRef { session_id: &session_id },
                    ).await;
                }
                () = async {
                    match cancel_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => break None,
                () = process.exited.cancelled() => break None,
                answer = &mut request, if !answered => match answer {
                    Ok(response) => {
                        answered = true;
                        turn.round.inserted(&response.message_id);
                        if turn.round.ended() {
                            break Some(Ok(()));
                        }
                    }
                    Err(error) => break Some(Err(error)),
                },
                answer = async {
                    match pending_steer.as_mut() {
                        Some(pending) => (&mut pending.answer).await,
                        None => std::future::pending().await,
                    }
                }, if pending_steer.is_some() => {
                    let pending = pending_steer.take().expect("a steer is pending");
                    let answer = answer.unwrap_or_else(|_| {
                        Err("the agent's process ended".to_owned())
                    });
                    turn.end_steer(pending, answer).await?;
                    if answered && turn.round.ended() {
                        break Some(Ok(()));
                    }
                }
                event = events.recv() => match event {
                    Some(event) => turn.take_event(event, cancel_deadline.is_some()).await?,
                    None => break None,
                },
                () = steer_wake.notified(),
                    if answered && cancel_deadline.is_none() && pending_steer.is_none() => {
                    pending_steer = turn.begin_steer().await?;
                }
            }
        };
        // Updates the agent sent before its answer may still be on their way.
        process.flush().await;
        while let Ok(event) = events.try_recv() {
            match event {
                SessionEvent::Update(update) => turn.update(update).await?,
                SessionEvent::Permission(_, incoming) => {
                    let _ = incoming.respond(&wire::permission_outcome(None)).await;
                }
                SessionEvent::BridgeCall { reply, .. } => {
                    let _ = reply.send(bridge_refusal());
                }
                SessionEvent::Elicitation(_, incoming) => elicitation::cancel(incoming),
            }
        }
        drop(events);
        self.inner.acp.processes.unwatch_steering(turn.session());
        if let Some(mut pending) = pending_steer.take() {
            let answer = tokio::time::timeout(CANCEL_GRACE * 5, &mut pending.answer)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_else(|| Err("the agent did not take the message in".to_owned()));
            turn.end_steer(pending, answer).await?;
        }
        // A call through the bridge, or a question to the user, ends before
        // the turn does.
        self.end_acp_questions(turn.run);
        for call in std::mem::take(&mut turn.bridged) {
            let _ = call.await;
        }
        let cancelled = turn.active.cancellation.is_cancelled();
        let idle = turn.round.idle.take();
        let answered_usage = idle.as_ref().and_then(|idle| idle.usage.clone());
        let end = match answer {
            Some(Ok(())) => {
                let idle = idle.expect("the round ends at an idle");
                match idle.stop_reason.as_deref() {
                    Some("cancelled") => RoundEnd::Cancelled,
                    Some("error") => RoundEnd::Failed(
                        idle.error
                            .unwrap_or_else(|| "the agent's work failed".to_owned()),
                    ),
                    Some(reason @ ("max_tokens" | "max_turn_requests" | "refusal")) => {
                        stopped(reason)
                    }
                    // `end_turn`, none, or one this client does not know.
                    _ => {
                        if turn.step.is_some() || turn.last_turn_seq.is_none() {
                            turn.commit(ModelFinishReason::Stop).await?;
                        }
                        let usage = usage_of(idle.usage.as_ref(), turn.usage_update.as_ref());
                        self.record_acp_usage(turn, usage).await?;
                        RoundEnd::Completed(
                            (!turn.last_text.is_empty()).then(|| turn.last_text.clone()),
                        )
                    }
                }
            }
            Some(Err(error)) if cancelled => {
                let _ = error;
                RoundEnd::Cancelled
            }
            Some(Err(error)) => RoundEnd::Failed(match process.exit_reason() {
                Some(reason) => reason,
                None => agent_error_message(&error),
            }),
            None if cancelled => RoundEnd::Cancelled,
            None => RoundEnd::Failed(
                process
                    .exit_reason()
                    .unwrap_or_else(|| "the agent's process ended".to_owned()),
            ),
        };
        if !matches!(end, RoundEnd::Completed(_)) {
            if let Some(step) = turn.step.as_ref()
                && !step.parts.is_empty()
            {
                let finish = match &end {
                    RoundEnd::Cancelled => ModelFinishReason::Cancelled,
                    RoundEnd::Stopped { finish, .. } => finish.clone(),
                    _ => ModelFinishReason::Error,
                };
                turn.commit(finish).await?;
            } else if let Some(step) = turn.step.take() {
                turn.retries += 1;
                self.append(
                    turn.session(),
                    Some(turn.run),
                    origin(),
                    Event::AttemptAbandoned {
                        attempt_id: step.attempt_id,
                        model_error: None,
                    },
                )
                .await?;
            }
            // What the agent used before it stopped counts too.
            let usage = usage_of(answered_usage.as_ref(), turn.usage_update.as_ref());
            self.record_acp_usage(turn, usage).await?;
        }
        turn.end_open_calls(matches!(end, RoundEnd::Cancelled))
            .await?;
        if !turn.live.is_live() {
            self.forget_live(turn.session(), &turn.live);
        }
        Ok(end)
    }

    async fn record_acp_usage(
        &self,
        turn: &Turn<'_>,
        usage: Option<Usage>,
    ) -> Result<(), EngineError> {
        let (Some(usage), Some(model_turn_seq)) = (usage, turn.last_turn_seq) else {
            return Ok(());
        };
        self.append(
            turn.session(),
            Some(turn.run),
            origin(),
            Event::ModelUsageRecorded {
                model_turn_seq,
                agent_id: turn.live.agent.id.clone(),
                resolved_model: turn.active.policy.agent.fallback_chain[0].resolved.clone(),
                usage,
                estimated_cost_pico_usd: cost_of(turn.usage_update.as_ref()),
            },
        )
        .await
    }

    /// What the first prompt of an ACP session carries before the input
    /// (`docs/site/specs/acp-agents.md` section 15, 2026-10-23), each part
    /// left out when empty: the tool instruction sections cookie would
    /// compose for a model run, for exactly the tools the agent is offered
    /// over the bridge; the listing of the skills a model run would see,
    /// when the `skill` tool is offered; the block on the session's
    /// attachments a model run's system prompt carries, which with several
    /// also says where the harness's built-in tools run (2026-10-25); the
    /// session's memory snapshot as a model run sees it; and the agent
    /// document's body.
    fn acp_first_prompt_context(
        &self,
        session: SessionId,
        policy: &crate::policy::FrozenRunPolicy,
        events: &crate::events::EventSnapshot,
        document: &cookie_agent_config::AgentDocument,
        live: &LiveSession,
    ) -> Result<Vec<String>, EngineError> {
        let hide = document
            .frontmatter
            .acp
            .clone()
            .unwrap_or_default()
            .hidden_tools();
        let offered = self.offered_tools(session, policy, &hide)?;
        let mut context = Vec::new();
        context.extend(self.tool_prompt_block(policy, session, Some(&offered), &|_, _| true)?);
        if offered.skill {
            context.extend(self.skill_listing(
                session,
                policy,
                None,
                super::prompt::SKILL_LISTING_CONTEXT_TOKENS,
            )?);
        }
        context.extend(self.runtimes_block(
            session,
            Some(&super::super::runtime_notes::BuiltinTools {
                runtime: live.agent.runtime.as_str(),
                cwd: &live.cwd,
            }),
        ));
        context.extend(
            events
                .iter()
                .rev()
                .find_map(|event| match &event.payload {
                    Event::MemoryLoaded { snapshot } => Some(snapshot),
                    _ => None,
                })
                .and_then(crate::memory::render::snapshot_turn),
        );
        if !document.body.trim().is_empty() {
            context.push(document.body.clone());
        }
        Ok(context)
    }

    /// The prompt of a round: the input the session took that no committed
    /// turn covers yet (`input_through_seq` of the last one), after the
    /// first prompt's `context` on the session's first prompt.
    fn acp_prompt(
        &self,
        events: &crate::events::EventSnapshot,
        live: &mut LiveSession,
        context: Vec<String>,
        process: &AgentProcess,
    ) -> Vec<Value> {
        let boundary = events
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                Event::ModelTurnCommitted {
                    input_through_seq, ..
                } => Some(*input_through_seq),
                _ => None,
            })
            .unwrap_or(0);
        let mut prompt = Vec::new();
        if live.first_prompt {
            prompt.extend(context.iter().map(|part| wire::text_block(part)));
        }
        let caps = &process.init().capabilities.prompt();
        prompt.extend(self.input_since(events, boundary, caps).0);
        // Only the agent's own wake: nothing to send, the round listens.
        if prompt.is_empty() && !woke_since(events, boundary) {
            prompt.push(wire::text_block("Continue."));
        }
        live.first_prompt = false;
        self.remember_live(live);
        prompt
    }

    /// The user inputs of `run` waiting to steer, held while they go into
    /// the agent's running turn: no client takes them back meanwhile.
    /// Called on the session's actor.
    fn hold_inputs_to_steer(
        &self,
        session: SessionId,
        run: RunId,
        declined: &HashSet<u64>,
    ) -> Vec<super::super::PendingInput> {
        let Ok(log) = self.inner.store.log(session) else {
            return Vec::new();
        };
        let mut inputs = super::super::mailbox::pending_inputs(&log.event_snapshot(), run);
        inputs.retain(|input| !declined.contains(&input.admission_seq));
        let held = inputs
            .iter()
            .map(|input| input.admission_seq)
            .collect::<Vec<_>>();
        if held.is_empty() {
            self.release_steered_inputs(session);
        } else {
            self.inner
                .acp
                .steering
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(session, held);
        }
        inputs
    }

    /// The agent answered: `session`'s held inputs are no longer on their
    /// way.
    pub(crate) fn release_steered_inputs(&self, session: SessionId) {
        self.inner
            .acp
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&session);
    }

    /// Whether `seq` of `session` is on its way into the agent's turn.
    pub(crate) fn input_is_steering(&self, session: SessionId, seq: u64) -> bool {
        self.inner
            .acp
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session)
            .is_some_and(|held| held.contains(&seq))
    }

    /// The agent took the held inputs `held`: each is promoted where the
    /// turn stands now, as a model's steer is at its next request, and
    /// applied. They lead the pending lane, since nothing took them back
    /// while held. Returns their `user_input_submitted` sequence numbers
    /// and the last event's. Called on the session's actor.
    fn promote_steered_inputs(
        &self,
        session: SessionId,
        run: RunId,
        held: &[u64],
    ) -> Result<(Vec<u64>, u64), EngineError> {
        self.release_steered_inputs(session);
        let pending = super::super::mailbox::pending_inputs(
            &self.inner.store.log(session)?.event_snapshot(),
            run,
        );
        let mut submitted = Vec::new();
        let mut last = 0;
        for input in pending
            .into_iter()
            .take_while(|input| held.contains(&input.admission_seq))
        {
            let record = self.append_direct_record(
                session,
                Some(run),
                input.origin,
                Event::UserInputSubmitted {
                    content: input.content,
                },
            )?;
            submitted.push(record.seq);
            let applied = self.append_direct_record(
                session,
                Some(run),
                origin(),
                Event::UserInputApplied {
                    user_input_seq: record.seq,
                },
            )?;
            last = applied.seq;
        }
        Ok((submitted, last))
    }

    /// The prompt blocks of the input the session took after `boundary`
    /// (user input applied, producer messages admitted), and which input
    /// that was.
    fn input_since(
        &self,
        events: &crate::events::EventSnapshot,
        boundary: u64,
        caps: &wire::PromptCapabilities,
    ) -> (Vec<Value>, cookie_agent_protocol::AcpSteered) {
        let producers = crate::goal_projection::GoalProducerProjection::from_events(events);
        let admitted = producers
            .messages
            .iter()
            .filter(|message| !message.discarded)
            .filter_map(|message| {
                message.admission.map(|(_, seq)| {
                    let wake = matches!(
                        message.producer_owner,
                        cookie_agent_protocol::ProducerOwner::AcpAgent { .. }
                    );
                    (seq, (message.message_id, &message.body, wake))
                })
            })
            .collect::<HashMap<_, _>>();
        let mut submitted = HashMap::new();
        let mut prompt = Vec::new();
        let mut taken = cookie_agent_protocol::AcpSteered::default();
        for event in events.iter() {
            match &event.payload {
                Event::UserInputSubmitted { content } => {
                    submitted.insert(event.seq, content);
                }
                Event::UserInputApplied { user_input_seq } if event.seq > boundary => {
                    if let Some(content) = submitted.get(user_input_seq) {
                        self.push_content(&mut prompt, event.session_id, content, caps);
                        taken.user_inputs.push(*user_input_seq);
                    }
                }
                Event::ProducerMessageAdmitted { .. } if event.seq > boundary => {
                    if let Some((message_id, body, wake)) = admitted.get(&event.seq) {
                        // The agent made its wake's message: never sent to it.
                        if !wake {
                            self.push_content(&mut prompt, event.session_id, body, caps);
                        }
                        taken.producer_messages.push(*message_id);
                    }
                }
                _ => {}
            }
        }
        (prompt, taken)
    }

    /// Adds the blocks of `content` to `prompt`: text, and files as the
    /// agent's prompt capabilities `caps` allow ([`super::prompt`]).
    fn push_content(
        &self,
        prompt: &mut Vec<Value>,
        session: SessionId,
        content: &cookie_agent_protocol::UserContent,
        caps: &wire::PromptCapabilities,
    ) {
        for part in content.parts() {
            match part {
                Open::Known(UserContentPart::Text { text }) => prompt.push(wire::text_block(text)),
                Open::Known(UserContentPart::File { artifact }) => {
                    prompt.push(super::prompt::file_block(artifact, caps, || {
                        self.read_user_artifact(session, artifact)
                    }));
                }
                Open::Unknown { .. } => {}
            }
        }
    }

    /// The ACP session `session` talks to `agent` in: the live one, else the
    /// one its log records resumed or loaded, else, when a fork or a revert
    /// left that one behind, one branched off it
    /// ([`super::lifecycle::Plan`]), else a new one. Starting, resuming,
    /// loading, or forking one records it.
    pub(crate) async fn acp_session(
        &self,
        session: SessionId,
        run: Option<RunId>,
        agent: &RuntimeAgent,
        document: &cookie_agent_config::AgentDocument,
        process: &Arc<AgentProcess>,
    ) -> Result<LiveSession, EngineError> {
        let plan = super::lifecycle::plan(
            &self.inner.store.get(session)?.log.event_snapshot(),
            &agent.id,
        );
        // The live session serves while the log still makes it the
        // session's: a fork or a revert since detached it.
        if let Some(live) = self.live_session(session)
            && Arc::ptr_eq(&live.process, process)
            && live.is_live()
            && live.agent.id == agent.id
            && matches!(&plan, super::lifecycle::Plan::Resume(record)
                if record.acp_session_id == live.acp_session_id)
        {
            return Ok(live);
        }
        // An agent's own subagent lives only in the agent process that made
        // it; cookie cannot open it again.
        if let super::lifecycle::Plan::Resume(record) = &plan
            && record.how == AcpSessionStart::Subagent
        {
            return Err(EngineError::Acp(format!(
                "`{}`'s subagent ended with the agent's process; it takes no more turns",
                agent.id
            )));
        }
        self.remember_acp_input(agent, &process.init().capabilities.prompt());
        let cwd = self.acp_cwd(session, agent, process)?;
        let additional_directories = if process.init().capabilities.session("additionalDirectories")
        {
            self.acp_additional_directories(session, agent, &cwd)?
        } else {
            Vec::new()
        };
        let bridge = self.open_acp_bridge(session, agent, process).await;
        // Bound before the agent hears of it: an agent connects its MCP
        // servers while it answers `session/new`.
        if let Ok(opened) = &bridge {
            super::mcp::bind(
                &self.inner,
                agent,
                &process.key.runtime,
                &opened.bridge,
                session,
            );
        }
        let mcp_servers = match &bridge {
            Ok(opened) => {
                let (command, args) = opened
                    .command
                    .split_first()
                    .map(|(command, args)| (command.clone(), args.to_vec()))
                    .unwrap_or_default();
                vec![wire::McpServer {
                    transport: "stdio",
                    name: super::mcp::BRIDGE_SERVER.to_owned(),
                    command,
                    args,
                    env: Vec::new(),
                }]
            }
            Err(reason) => {
                self.acp_notice(
                    session,
                    run,
                    format!("cookie's tools are not offered to the agent: {reason}"),
                )
                .await;
                Vec::new()
            }
        };
        let (mut prior, mut branch) = match plan {
            super::lifecycle::Plan::New => (None, None),
            super::lifecycle::Plan::Resume(record) => (Some(record), None),
            super::lifecycle::Plan::Branch {
                from,
                message_id,
                delete,
            } => (None, Some((from, message_id, delete))),
        };
        // The agent keeps its sessions on the machine that runs it: one
        // recorded on another runtime, or of another declaration, cannot be
        // resumed or forked here.
        if let Some(elsewhere) = prior
            .as_ref()
            .or(branch.as_ref().map(|(from, _, _)| from))
            .filter(|record| {
                record.runtime_id != agent.runtime || record.runtime_agent != agent.agent
            })
            .cloned()
        {
            let notice = if elsewhere.runtime_id == agent.runtime {
                format!(
                    "The agent's earlier session is of ACP agent `{}`, and its document now runs \
                     `{}`; this turn starts a new session, which does not remember the \
                     conversation so far.",
                    elsewhere.runtime_agent, agent.agent
                )
            } else {
                format!(
                    "The agent's earlier session is on runtime `{}`, which does not run it for \
                     this session now; this turn starts a new session on runtime `{}`, which does \
                     not remember the conversation so far.",
                    elsewhere.runtime_id, agent.runtime
                )
            };
            self.acp_notice(session, run, notice).await;
            prior = None;
            branch = None;
        }
        let caps = &process.init().capabilities;
        let mut opened = None;
        let mut lost = false;
        let mut forked_from = None;
        if let Some((from, message_id, delete)) = &branch {
            let forked = self
                .branch_acp_session(
                    session,
                    process,
                    from,
                    message_id.as_deref(),
                    *delete,
                    &cwd,
                    &additional_directories,
                    &mcp_servers,
                )
                .await?;
            if let Some((id, response)) = forked {
                let receiver = process.route(&id);
                forked_from = Some(from.acp_session_id.clone());
                opened = Some((id, response, AcpSessionStart::Forked, receiver));
            }
        }
        if let Some(prior) = &prior {
            let receiver = process.route(&prior.acp_session_id);
            let request = wire::ExistingSessionRequest {
                session_id: &prior.acp_session_id,
                cwd: &cwd,
                additional_directories: &additional_directories,
                mcp_servers: &mcp_servers,
                meta: wire::session_meta(),
            };
            // Every v2 agent resumes; none replays (no `replayFrom`).
            let attempt = Some((wire::method::SESSION_RESUME, AcpSessionStart::Resumed));
            match attempt {
                Some((method, how)) => {
                    match tokio::time::timeout(
                        SESSION_TIMEOUT,
                        process
                            .peer
                            .request::<_, wire::SessionResponse>(method, &request),
                    )
                    .await
                    {
                        Ok(Ok(response)) => {
                            opened = Some((prior.acp_session_id.clone(), response, how, receiver));
                        }
                        _ => {
                            process.unroute(&prior.acp_session_id);
                            lost = true;
                        }
                    }
                }
                None => {
                    process.unroute(&prior.acp_session_id);
                    lost = true;
                }
            }
        }
        let (acp_session_id, response, how, receiver) = match opened {
            Some(opened) => opened,
            None => {
                let response = tokio::time::timeout(
                    SESSION_TIMEOUT,
                    process.peer.request::<_, wire::SessionResponse>(
                        wire::method::SESSION_NEW,
                        &wire::NewSessionRequest {
                            cwd: &cwd,
                            additional_directories: &additional_directories,
                            mcp_servers: &mcp_servers,
                            meta: wire::session_meta(),
                        },
                    ),
                )
                .await
                .map_err(|_| {
                    EngineError::Acp(format!(
                        "the agent did not answer `session/new` within {} s",
                        SESSION_TIMEOUT.as_secs()
                    ))
                })?
                .map_err(|error| {
                    EngineError::Acp(
                        process
                            .exit_reason()
                            .unwrap_or_else(|| agent_error_message(&error)),
                    )
                })?;
                let id = response
                    .session_id
                    .clone()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        EngineError::Acp(
                            "the agent answered `session/new` without a session ID".to_owned(),
                        )
                    })?;
                let receiver = process.route(&id);
                (id, response, AcpSessionStart::New, receiver)
            }
        };
        if lost {
            self.acp_notice(
                session,
                run,
                "The agent could not resume its earlier session, so this turn starts a new one; \
                 the agent does not remember the conversation so far."
                    .to_owned(),
            )
            .await;
        }
        self.append(
            session,
            run,
            origin(),
            Event::AcpSessionStarted {
                record: AcpSessionRecord {
                    agent: agent.id.clone(),
                    runtime_id: agent.runtime.clone(),
                    runtime_agent: agent.agent.clone(),
                    acp_session_id: acp_session_id.clone(),
                    cwd: cwd.clone(),
                    protocol_version: process.init().protocol_version,
                    agent_info: process.init().info.clone(),
                    how,
                    // A v2 agent takes a prompt during its work.
                    steering: true,
                    forked_from,
                    fork: Some(caps.session("fork")),
                    fork_point: process.fork_point.get().copied(),
                },
            },
        )
        .await?;
        // The session a revert left behind is the cookie session's own: gone
        // once this one replaced it.
        if let Some((from, _, true)) = &branch {
            self.retire_acp_session(session, process, &from.acp_session_id)
                .await;
        }
        let options = options_of(response.config_options.as_ref());
        self.remember_acp_options(agent, &options, matches!(how, AcpSessionStart::New));
        let mut live = LiveSession {
            session,
            process: Arc::clone(process),
            agent: agent.clone(),
            acp_session_id,
            cwd,
            bridge: bridge.ok().map(|opened| opened.bridge),
            options,
            first_prompt: matches!(how, AcpSessionStart::New),
            events: Arc::new(tokio::sync::Mutex::new(receiver)),
            carried: Arc::default(),
            wanted: Arc::default(),
            waiting: Arc::default(),
            rounds: Arc::default(),
        };
        if matches!(how, AcpSessionStart::New) {
            self.apply_starting_options(session, run, &mut live, document)
                .await;
            self.apply_session_starting_options(session, run, &mut live)
                .await;
        }
        // v2 gives the session's commands with it, not as a first update.
        let commands = response
            .available_commands
            .as_ref()
            .map(|commands| commands_of(&serde_json::json!({"availableCommands": commands})));
        if !live.options.is_empty() || commands.is_some() {
            self.append(
                session,
                run,
                origin(),
                Event::AcpSessionUpdated {
                    update: AcpSessionUpdate {
                        options: (!live.options.is_empty()).then(|| live.options.clone()),
                        commands,
                        ..AcpSessionUpdate::default()
                    },
                },
            )
            .await?;
        }
        self.remember_live(&live);
        self.listen_between_runs(&live);
        Ok(live)
    }

    /// The starting mode (section 5): the document's `acp.mode`, else the
    /// agent's `auto`, else its default; then the document's `acp.config`,
    /// except the model and effort options when the document names
    /// `models`; then the first `models` entry the agent offers, and its
    /// variant.
    async fn apply_starting_options(
        &self,
        session: SessionId,
        run: Option<RunId>,
        live: &mut LiveSession,
        document: &cookie_agent_config::AgentDocument,
    ) {
        let settings = document.frontmatter.acp.clone().unwrap_or_default();
        let mode_option = super::options::mode_option(&live.options).cloned();
        if let Some(option) = mode_option {
            let wanted = super::options::wanted_mode(&option, &settings);
            if let Some(wanted) = wanted
                && wanted != option.current_value
                && let Err(reason) = self.set_live_option(live, &option.id, &wanted).await
            {
                self.acp_notice(
                    session,
                    run,
                    format!("The agent's mode was not set to `{wanted}`: {reason}"),
                )
                .await;
            }
        }
        for (id, value) in &settings.config {
            if super::options::models_decide(&settings, &live.options, id) {
                continue;
            }
            let known = live
                .options
                .iter()
                .find(|option| &option.id == id)
                .map(|option| {
                    option
                        .values
                        .iter()
                        .any(|candidate| &candidate.value == value)
                });
            let problem = match known {
                None => Some(format!("the agent offers no option `{id}`")),
                Some(false) => Some(format!("option `{id}` takes no value `{value}`")),
                Some(true) => self.set_live_option(live, id, value).await.err(),
            };
            if let Some(problem) = problem {
                self.acp_notice(
                    session,
                    run,
                    format!("The agent document's `acp.config` was skipped there: {problem}"),
                )
                .await;
            }
        }
        self.apply_document_models(session, run, live, &settings)
            .await;
    }

    /// The first of the document's `models` whose model the agent offers,
    /// then its variant when it names one; an entry that cannot apply is
    /// reported in the session and the agent keeps its default.
    async fn apply_document_models(
        &self,
        session: SessionId,
        run: Option<RunId>,
        live: &mut LiveSession,
        settings: &cookie_agent_config::AcpDocumentSettings,
    ) {
        let pick = super::options::pick_model(&live.options, settings);
        let chosen = match &pick {
            super::options::ModelPick::Chosen { option, choice } => {
                Some((option.clone(), (*choice).clone()))
            }
            _ => None,
        };
        let mut problem = pick.problem(settings);
        if let Some((option, choice)) = chosen {
            let current = super::options::model_option(&live.options)
                .is_some_and(|model| model.current_value == choice.model);
            if !current {
                problem = self
                    .set_live_option(live, &option, &choice.model)
                    .await
                    .err();
            }
            if problem.is_none()
                && let Some(variant) = &choice.variant
            {
                problem = match super::options::pick_variant(&live.options, variant) {
                    Err(problem) => Some(problem),
                    Ok(option) => {
                        let current = super::options::effort_option(&live.options)
                            .is_some_and(|effort| &effort.current_value == variant);
                        if current {
                            None
                        } else {
                            self.set_live_option(live, &option, variant).await.err()
                        }
                    }
                };
            }
        }
        if let Some(problem) = problem {
            self.acp_notice(
                session,
                run,
                format!("The agent document's `models` was skipped there: {problem}"),
            )
            .await;
        }
    }

    /// Sets option `id` of `live` to `value` with the agent.
    pub(crate) async fn set_live_option(
        &self,
        live: &mut LiveSession,
        id: &str,
        value: &str,
    ) -> Result<(), String> {
        // A boolean option takes a boolean (`type: boolean`); a select the
        // value's ID (`type: id`).
        let boolean = live
            .options
            .iter()
            .any(|option| option.id == id && option.kind == AcpConfigOptionKind::Boolean);
        let request = wire::SetConfigOptionRequest {
            session_id: &live.acp_session_id,
            config_id: id,
            value_type: if boolean { "boolean" } else { "id" },
            value: if boolean {
                Value::Bool(value == "true")
            } else {
                Value::from(value)
            },
        };
        let response = live
            .process
            .peer
            .request::<_, wire::SetConfigOptionResponse>(
                wire::method::SESSION_SET_CONFIG_OPTION,
                &request,
            )
            .await
            .map_err(|error| agent_error_message(&error))?;
        match response.config_options {
            Some(options) => live.options = options_of(Some(&options)),
            None => {
                for option in &mut live.options {
                    if option.id == id {
                        value.clone_into(&mut option.current_value);
                    }
                }
            }
        }
        self.remember_live(live);
        Ok(())
    }

    /// Records a notice about the session's ACP agent.
    pub(super) async fn acp_notice(
        &self,
        session: SessionId,
        _run: Option<RunId>,
        message: String,
    ) {
        let _ = self
            .append(
                session,
                None,
                origin(),
                Event::Diagnostic {
                    source: cookie_agent_protocol::DiagnosticSource::engine("acp")
                        .expect("static diagnostic source is valid"),
                    kind: cookie_agent_protocol::DiagnosticKind::new("acp_agent")
                        .expect("static diagnostic kind is valid"),
                    message,
                    count: None,
                    detail: None,
                },
            )
            .await;
    }

    /// The working directory of `agent`'s sessions for `session`: its
    /// workspace attachment on the agent's runtime, else the runtime's home.
    fn acp_cwd(
        &self,
        session: SessionId,
        agent: &RuntimeAgent,
        process: &AgentProcess,
    ) -> Result<String, EngineError> {
        let projection = self.inner.store.get(session)?;
        let workspace = projection
            .meta
            .attachments
            .iter()
            .find(|attachment| {
                attachment.runtime_id == agent.runtime && attachment.workspace.is_some()
            })
            .and_then(|attachment| attachment.workspace.clone());
        workspace.or_else(|| process.client.home()).ok_or_else(|| {
            EngineError::Acp(format!(
                "the agent has no working directory: the session has no workspace on runtime \
                     `{}`, which reported no home directory",
                agent.runtime
            ))
        })
    }

    /// The workspace roots besides `cwd` that `session` attaches on
    /// `agent`'s runtime, each once, in attachment order: the agent's
    /// `additionalDirectories`. Workspaces on other runtimes are on other
    /// machines, and are left out.
    fn acp_additional_directories(
        &self,
        session: SessionId,
        agent: &RuntimeAgent,
        cwd: &str,
    ) -> Result<Vec<String>, EngineError> {
        let projection = self.inner.store.get(session)?;
        let mut directories = Vec::<String>::new();
        for workspace in projection
            .meta
            .attachments
            .iter()
            .filter(|attachment| attachment.runtime_id == agent.runtime)
            .filter_map(|attachment| attachment.workspace.as_ref())
        {
            if workspace != cwd && !directories.contains(workspace) {
                directories.push(workspace.clone());
            }
        }
        Ok(directories)
    }

    async fn open_acp_bridge(
        &self,
        _session: SessionId,
        agent: &RuntimeAgent,
        process: &AgentProcess,
    ) -> Result<AgentMcpOpenResult, String> {
        process
            .client
            .agent_request::<_, AgentMcpOpenResult>(
                method::AGENT_MCP_OPEN,
                &AgentRef {
                    agent: agent.agent.to_string(),
                },
                Duration::from_secs(30),
            )
            .await
    }

    /// The IDs of the runtimes `session` attaches, each once.
    pub(crate) fn session_runtime_ids(
        &self,
        session: SessionId,
    ) -> Result<Vec<cookie_agent_protocol::RuntimeId>, EngineError> {
        let projection = self.inner.store.get(session)?;
        let mut ids = Vec::new();
        for attachment in &projection.meta.attachments {
            if !ids.contains(&attachment.runtime_id) {
                ids.push(attachment.runtime_id.clone());
            }
        }
        Ok(ids)
    }

    /// The runtimes that offer the ACP agent `session` runs now, when it
    /// runs one.
    pub(crate) fn session_acp_offers(
        &self,
        session: SessionId,
    ) -> Result<Option<crate::runtime_snapshot::AcpOffers>, EngineError> {
        let projection = self.inner.store.get(session)?;
        let selection =
            cookie_agent_protocol::SessionModelState::from_events(&projection.log.event_snapshot())
                .selection
                .unwrap_or_else(|| projection.meta.creation_selection.clone());
        let runtime = self.current_runtime();
        let Ok(agents) = runtime.agents_for_preset(selection.preset.as_deref()) else {
            return Ok(None);
        };
        Ok(agents
            .get(&selection.agent)
            .and_then(|agent| agent.acp.clone()))
    }

    /// The ACP agent of `offers` on the one runtime `session` attaches
    /// that offers it; refused, saying why, when there is none or more
    /// than one, or it is away or disabled there.
    pub(crate) fn place_acp_agent(
        &self,
        session: SessionId,
        offers: &crate::runtime_snapshot::AcpOffers,
    ) -> Result<RuntimeAgent, EngineError> {
        offers
            .place(&self.session_runtime_ids(session)?)
            .map_err(EngineError::Acp)
    }

    /// The live ACP session of `session`, if any.
    /// Tells the agent of `session`'s live ACP session to stop, for a run
    /// cancelled before its runner started: a wake's work (the agent's own
    /// subagent's, say) is under way and would not hear otherwise. Starts no
    /// process and opens no session.
    pub(crate) async fn cancel_live_acp_session(&self, session: SessionId) {
        if let Some(live) = self.live_session(session).filter(LiveSession::is_live) {
            let _ = live
                .process
                .peer
                .notify(
                    wire::method::SESSION_CANCEL,
                    &wire::SessionRef {
                        session_id: &live.acp_session_id,
                    },
                )
                .await;
        }
        let _ = self.drop_stopped_acp_wakes(session).await;
    }

    pub(crate) fn live_session(&self, session: SessionId) -> Option<LiveSession> {
        self.inner
            .acp
            .processes
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .cloned()
    }

    /// Remembers `live` as the ACP session of its cookie session.
    pub(crate) fn remember_live(&self, live: &LiveSession) {
        self.inner
            .acp
            .processes
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(live.session, live.clone());
    }

    /// The bytes of a file the user attached, when the store has them.
    fn read_user_artifact(
        &self,
        session: SessionId,
        artifact: &cookie_agent_protocol::ArtifactRef,
    ) -> Option<bytes::Bytes> {
        let attachment = cookie_agent_protocol::ToolAttachment {
            mime_type: cookie_agent_protocol::MimeType::new(artifact.media_type.clone()).ok()?,
            filename: artifact.filename.clone(),
            byte_length: artifact.byte_length,
            sha256: cookie_agent_protocol::Sha256Digest::new(artifact.sha256.clone()).ok()?,
            reference: cookie_agent_protocol::ArtifactReference {
                uri: format!("artifact://sha256/{}", artifact.sha256),
            },
        };
        self.inner
            .artifacts
            .read_verified_attachment(session, &attachment)
            .ok()
    }

    fn forget_live(&self, session: SessionId, live: &LiveSession) {
        let mut sessions = self
            .inner
            .acp
            .processes
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if sessions
            .get(&session)
            .is_some_and(|known| known.acp_session_id == live.acp_session_id)
        {
            sessions.remove(&session);
        }
    }

    /// Ends the ACP session of `session`, as the session is unloaded:
    /// `session/close` when the agent offers it, and its bridge closes.
    pub(crate) async fn close_acp_session(&self, session: SessionId) {
        let live = self
            .inner
            .acp
            .processes
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session);
        if let Some(live) = live {
            self.end_live_session(live, false).await;
        }
    }

    /// Closes the bridge of `live`, which no longer serves its session,
    /// and ends it with the agent: `session/close`, and with `delete` also
    /// `session/delete` when the agent offers it.
    pub(super) async fn end_live_session(&self, live: LiveSession, delete: bool) {
        if let Some(bridge) = &live.bridge {
            self.inner
                .acp
                .processes
                .bridges
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&(live.process.key.runtime.clone(), bridge.clone()));
            let _ = live
                .process
                .client
                .agent_notify(
                    method::AGENT_MCP_CLOSE,
                    &cookie_agent_runtime_api::protocol::AgentMcpBridgeRef {
                        bridge: bridge.clone(),
                    },
                )
                .await;
        }
        if !live.is_live() {
            return;
        }
        super::lifecycle::end_agent_session(&live.process, &live.acp_session_id, true, delete)
            .await;
        live.process.unroute(&live.acp_session_id);
    }

    /// Names a session the agent has not named after its round: cookie's
    /// own title generation, on the title agent's models but never the
    /// synthetic `acp/<agent>` it would inherit as `${parent_model}`;
    /// nothing when that leaves no model (no `small_model`).
    async fn acp_fallback_title(
        &self,
        active: &ActiveRun,
        run: RunId,
        input_through_seq: u64,
    ) -> Result<(), EngineError> {
        if self.inner.store.get(active.session)?.meta.title.is_some()
            || matches!(
                self.inner.store.origin(active.session)?,
                cookie_agent_protocol::SessionOrigin::Delegated { .. }
            )
        {
            return Ok(());
        }
        let policy = self.internal_agent_policy(
            cookie_agent_protocol::InternalAgentKind::SessionTitle,
            &active.policy,
            None,
        )?;
        if policy.models.is_empty() {
            return Ok(());
        }
        self.maybe_generate_session_title(
            active.session,
            run,
            input_through_seq,
            &active.cancellation,
            &policy,
        )
        .await
    }

    /// Sets the session's title from the agent's, unless the user set one.
    async fn set_acp_title(
        &self,
        session: SessionId,
        run: RunId,
        title: &str,
    ) -> Result<(), EngineError> {
        // The agent's title applies, again and again, until the user names
        // the session, even over a title cookie generated.
        let projection = self.inner.store.get(session)?;
        let Ok(title) = cookie_agent_protocol::SessionTitle::new(truncate(title.trim(), 512))
        else {
            return Ok(());
        };
        if projection.fold.titled_by_user() || projection.meta.title.as_ref() == Some(&title) {
            return Ok(());
        }
        let input_through_seq = self.inner.store.get(session)?.meta.last_event_seq;
        self.append(
            session,
            Some(run),
            event_origin("engine:session-title"),
            Event::SessionTitleCommitted {
                change: cookie_agent_protocol::SessionTitleChange::FallbackSet { title },
                input_through_seq,
            },
        )
        .await
    }
}

impl Engine {
    /// `session.agent_option.set`: sets option `option_id` of the ACP agent
    /// of session `session_id` with the agent, resuming its ACP session when
    /// it is not live, and records the session's options.
    pub async fn set_session_agent_option(
        &self,
        params: cookie_agent_protocol::SessionAgentOptionSetParams,
    ) -> Result<cookie_agent_protocol::SessionAgentOptionSetResult, EngineError> {
        let projection = self.inner.store.get(params.session_id)?;
        let agent_id =
            cookie_agent_protocol::SessionModelState::from_events(&projection.log.event_snapshot())
                .selection
                .unwrap_or_else(|| projection.meta.creation_selection.clone())
                .agent;
        let runtime = self.current_runtime();
        let resolved = runtime
            .agents
            .get(&agent_id)
            .filter(|agent| agent.acp.is_some())
            .ok_or_else(|| {
                EngineError::Acp(format!(
                    "session {} does not run an ACP agent",
                    params.session_id
                ))
            })?;
        let offers = resolved.acp.clone().expect("filtered to ACP agents");
        let agent = self.place_acp_agent(params.session_id, &offers)?;
        let (process, _use) = self
            .inner
            .acp
            .processes
            .acquire(&self.inner, &agent)
            .await
            .map_err(EngineError::Acp)?;
        let mut live = self
            .acp_session(
                params.session_id,
                None,
                &agent,
                &resolved.document,
                &process,
            )
            .await?;
        let known = live
            .options
            .iter()
            .find(|option| option.id == params.option_id)
            .map(|option| {
                option
                    .values
                    .iter()
                    .any(|value| value.value == params.value)
            });
        match known {
            None => {
                return Err(EngineError::Acp(format!(
                    "the agent offers no option `{}`",
                    params.option_id
                )));
            }
            Some(false) => {
                return Err(EngineError::Acp(format!(
                    "option `{}` takes no value `{}`",
                    params.option_id, params.value
                )));
            }
            Some(true) => {}
        }
        self.set_live_option(&mut live, &params.option_id, &params.value)
            .await
            .map_err(EngineError::Acp)?;
        self.append(
            params.session_id,
            None,
            origin(),
            Event::AcpSessionUpdated {
                update: AcpSessionUpdate {
                    options: Some(live.options.clone()),
                    ..AcpSessionUpdate::default()
                },
            },
        )
        .await?;
        Ok(cookie_agent_protocol::SessionAgentOptionSetResult {
            options: live.options,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_reads_numbered_excerpt_is_code_from_its_first_line() {
        assert_eq!(
            numbered_excerpt("```\n360\t    {\n361\t        x\n362\n```", Some("/w/a.rs")),
            Some(DisplayBlock::Code {
                text: "    {\n        x\n".into(),
                language: Some("rust".into()),
                start_line: Some(360),
                omitted_lines: None,
            })
        );
        assert_eq!(numbered_excerpt("```\nplain text\n```", None), None);
        assert_eq!(numbered_excerpt("no fence", None), None);
    }

    #[test]
    fn a_calls_arguments_show_what_its_header_and_diff_do_not() {
        // An edit's strings are its diff's; its path is the header's.
        assert_eq!(
            arguments_of(
                &json!({"file_path": "/w/a.rs", "old_string": "a", "new_string": "b", "replace_all": true}),
                Some("edit"),
                "Edit a.rs",
            ),
            Some(DisplayBlock::key_value([("replace_all", "true")]))
        );
        assert_eq!(
            arguments_of(
                &json!({"file_path": "/w/a.rs", "old_string": "a", "new_string": "b", "replace_all": false}),
                Some("edit"),
                "Edit a.rs",
            ),
            None
        );
        // A field the title repeats is left out.
        assert_eq!(
            arguments_of(
                &json!({"description": "Map the panel", "prompt": "Look", "subagent_type": "explore"}),
                Some("think"),
                "Map the panel",
            ),
            Some(DisplayBlock::key_value([
                ("prompt", "Look"),
                ("subagent_type", "explore")
            ]))
        );
        // Nested values keep the input as JSON.
        assert_eq!(
            arguments_of(&json!({"questions": [{"q": "?"}]}), None, "Ask"),
            Some(DisplayBlock::Json {
                value: json!({"questions": [{"q": "?"}]})
            })
        );
        assert_eq!(
            arguments_of(&json!({"file_path": "/w/a.rs"}), Some("read"), "Read a.rs"),
            None
        );
    }
}

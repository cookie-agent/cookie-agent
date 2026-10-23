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

use std::{collections::HashMap, sync::Arc, time::Duration};

use cookie_agent_protocol::{
    AcpConfigOption, AcpConfigValue, AcpNotice, AcpNoticeSeverity, AcpSessionRecord,
    AcpSessionStart, AcpSessionUpdate, AssistantToolCallRef, AttemptId, DisplayBlock, ModelCallId,
    ModelFinishReason, Open, PersistedAssistantPart, PersistedModelTurn, PersistedToolResult,
    PlanEntry, PlanEntryPriority, PlanEntryStatus, PreparedPresentation, RunId, SafeCode,
    SafeDisplayText, SessionId, StreamedOutputKind, ToolCallId, ToolCallStart, ToolIdentity,
    ToolOutputDeclaration, ToolPresentation, ToolSource, Usage, UserContentPart,
};
use cookie_agent_runtime_api::protocol::{AgentMcpOpenResult, AgentRef, method};
use serde_json::Value;
use tokio::sync::mpsc;

use super::super::{
    ActiveRun, Engine, EngineError, SessionCommand, ToolCallFailureCode, ToolFailure, event_origin,
};
use super::{
    permissions,
    process::{AgentProcess, SessionEvent, agent_error_message},
    wire::{self, ToolCallFields},
};
use crate::runtime_snapshot::RuntimeAgent;
use cookie_agent_protocol::EventPayload as Event;

/// How long an interrupted turn waits for the agent's `cancelled`.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// How long the agent gets to answer `session/new`, `session/load`, and
/// `session/resume`.
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);

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
    /// Whether `options` shows the agent's modes as an option `mode`.
    pub modes_as_option: bool,
    /// Whether the next prompt is the session's first, which carries the
    /// agent document's body.
    pub first_prompt: bool,
    /// What the process routes to the session.
    pub events: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<SessionEvent>>>,
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
}

/// One ACP option or mode list in the session's terms.
fn options_of(
    config_options: Option<&Vec<Value>>,
    modes: Option<&wire::SessionModes>,
) -> (Vec<AcpConfigOption>, bool) {
    let mut options = config_options
        .map(|options| options.iter().filter_map(config_option).collect::<Vec<_>>())
        .unwrap_or_default();
    let has_mode_option = options
        .iter()
        .any(|option| option.category.as_deref() == Some("mode") || option.id == "mode");
    let modes_as_option =
        !has_mode_option && modes.is_some_and(|modes| !modes.available_modes.is_empty());
    if modes_as_option && let Some(modes) = modes {
        options.push(AcpConfigOption {
            id: "mode".to_owned(),
            name: "Mode".to_owned(),
            description: None,
            category: Some("mode".to_owned()),
            current_value: modes.current_mode_id.clone(),
            values: modes
                .available_modes
                .iter()
                .map(|mode| AcpConfigValue {
                    value: mode.id.clone(),
                    name: mode.name.clone(),
                    description: mode.description.clone(),
                })
                .collect(),
        });
    }
    (options, modes_as_option)
}

/// A `select` config option as the session shows it; grouped values are
/// flattened.
fn config_option(option: &Value) -> Option<AcpConfigOption> {
    let text =
        |value: &Value, name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
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
        id: text(option, "id")?,
        name: text(option, "name").unwrap_or_default(),
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
fn plan_block(update: &Value) -> DisplayBlock {
    let entries = update
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

/// Commands of an `available_commands_update`.
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

/// The cost a `usage_update` reports in US dollars, in pico-dollars.
fn cost_of(update: Option<&Value>) -> Option<u64> {
    let cost = update?.get("cost")?;
    if cost.get("currency").and_then(Value::as_str)? != "USD" {
        return None;
    }
    let amount = cost.get("amount").and_then(Value::as_f64)?;
    (amount.is_finite() && amount >= 0.0).then(|| (amount * 1e12).round() as u64)
}

/// A diff block of ACP diff content.
fn diff_block(path: &str, old: Option<&str>, new: &str) -> DisplayBlock {
    use cookie_agent_protocol::{DiffAsOf, DiffBlock, DiffChange, DiffHunk, DiffLine, DiffLineOp};
    let old_text = old.unwrap_or_default();
    let diff = similar::TextDiff::from_lines(old_text, new);
    let (mut added, mut removed) = (0_u64, 0_u64);
    let hunks = diff
        .grouped_ops(3)
        .into_iter()
        .map(|group| {
            let (first, last) = (
                group[0].as_tag_tuple(),
                group[group.len() - 1].as_tag_tuple(),
            );
            let old_range = first.1.start..last.1.end;
            let new_range = first.2.start..last.2.end;
            let mut lines = Vec::new();
            for op in &group {
                for change in diff.iter_changes(op) {
                    let op = match change.tag() {
                        similar::ChangeTag::Equal => DiffLineOp::Context,
                        similar::ChangeTag::Insert => {
                            added += 1;
                            DiffLineOp::Add
                        }
                        similar::ChangeTag::Delete => {
                            removed += 1;
                            DiffLineOp::Remove
                        }
                    };
                    lines.push(DiffLine {
                        op,
                        text: change.value().trim_end_matches(['\n', '\r']).to_owned(),
                    });
                }
            }
            let number = |start: usize, count: usize| start as u64 + u64::from(count > 0);
            DiffHunk {
                old_start: number(old_range.start, old_range.len()),
                old_lines: old_range.len() as u64,
                new_start: number(new_range.start, new_range.len()),
                new_lines: new_range.len() as u64,
                lines,
            }
        })
        .collect();
    let mut block = DisplayBlock::Diff(DiffBlock {
        path: path.to_owned(),
        old_path: None,
        language: None,
        change: if old.is_none() {
            DiffChange::Created
        } else {
            DiffChange::Modified
        },
        hunks,
        added,
        removed,
        omitted: None,
        as_of: DiffAsOf::Execute,
        full: None,
    });
    block.sanitize();
    block
}

impl Engine {
    /// The display blocks and text of a tool call's content: text as
    /// Markdown, images as media, resource links as links, embedded
    /// resources as code, diffs as diff blocks, and locations as file links.
    fn call_content(
        &self,
        session: SessionId,
        call: &ToolCallFields,
    ) -> (Vec<DisplayBlock>, String) {
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
                    let path = content
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let new = content
                        .get("newText")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let old = content.get("oldText").and_then(Value::as_str);
                    blocks.push(diff_block(path, old, new));
                    say(&mut text, &format!("edited {path}"));
                }
                Some("content") => {
                    let block = content.get("content").cloned().unwrap_or(Value::Null);
                    if let Some(shown) = self.content_block(session, &block) {
                        blocks.push(shown);
                    }
                    say(&mut text, &wire::content_text(&block));
                }
                Some("terminal") => {
                    let id = content
                        .get("terminalId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    blocks.push(DisplayBlock::text(format!("[terminal {id}]")));
                }
                _ => {}
            }
        }
        for location in call.locations.iter().flatten() {
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

/// What a call's header shows after its title: the command, path, URL,
/// pattern, or query its raw input names.
fn primary_argument(input: Option<&Value>) -> Option<cookie_agent_protocol::BoundedDisplayText> {
    let input = input?.as_object()?;
    [
        "command",
        "file_path",
        "filePath",
        "path",
        "notebook_path",
        "url",
        "pattern",
        "query",
    ]
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
    let title = display_text(fields.title.as_deref().unwrap_or("ACP tool call"));
    let mut presentation =
        PreparedPresentation::new(title, primary_argument(fields.raw_input.as_ref()))
            .with_hint(kind_hint(fields.kind.as_deref()));
    if let Some(input) = fields
        .raw_input
        .clone()
        .filter(|input| input.as_object().is_none_or(|input| !input.is_empty()))
    {
        presentation = presentation.with_arguments([DisplayBlock::Json { value: input }]);
    }
    presentation.fit();
    presentation
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

/// One tool call the agent reported in the run.
struct ShownCall {
    tool_call_id: ToolCallId,
    fields: ToolCallFields,
    /// The presentation the session last recorded for it.
    presented: PreparedPresentation,
    terminated: bool,
}

/// What the runner keeps of one model turn being written.
struct Step {
    attempt_id: AttemptId,
    parts: Vec<PersistedAssistantPart>,
    streaming: Option<StreamedOutputKind>,
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
    home: Option<String>,
    /// Calls through the bridge still running.
    bridged: Vec<tokio::task::JoinHandle<()>>,
    /// The agent takes input into its running turn (`_session/steering`).
    steering: bool,
    /// Claims on input steered into the turn, released at its next commit.
    steer_claims: Vec<super::super::producer_claims::ClaimedPrompt>,
    /// Input was taken to steer that the agent did not take: the next round
    /// sends it.
    unsteered: bool,
    /// The summary text of each compaction in progress, by its ID.
    compactions: HashMap<String, String>,
}

/// A `_session/steering` request on its way.
struct PendingSteer {
    answer: tokio::sync::oneshot::Receiver<Result<String, String>>,
    claim: super::super::producer_claims::ClaimedPrompt,
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
        self.step = Some(Step {
            attempt_id,
            parts: Vec::new(),
            streaming: None,
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
                        response_metadata: Default::default(),
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
    /// running turn: the turn so far commits, so the input stands after it,
    /// then the input is promoted and claimed like a round's, and
    /// `_session/steering` carries it. `None` when there was none.
    async fn begin_steer(&mut self) -> Result<Option<PendingSteer>, EngineError> {
        if self
            .step
            .as_ref()
            .is_some_and(|step| !step.parts.is_empty())
        {
            self.commit(ModelFinishReason::Stop).await?;
        }
        let (session, run, engine) = (self.session(), self.run, self.engine);
        engine
            .request(session, |reply| SessionCommand::PromotePendingOrComplete {
                run,
                final_text: None,
                complete_if_empty: false,
                reply,
            })
            .await?;
        let claim = engine.prompt_events(session, run).await?;
        let images = self
            .live
            .process
            .init()
            .agent_capabilities
            .prompt_capabilities
            .image;
        let (prompt, steered) = engine.input_since(&claim.events, self.input_through_seq, images);
        if prompt.is_empty() {
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
                .request::<_, wire::SteeringResponse>(
                    wire::method::SESSION_STEERING,
                    &wire::SteeringRequest {
                        session_id: &session_id,
                        prompt: &prompt,
                        meta: serde_json::json!({"steering": {"idleBehavior": "promptRequired"}}),
                    },
                )
                .await
                .map(|response| response.outcome)
                .map_err(|error| agent_error_message(&error));
            let _ = reply.send(answer);
        });
        Ok(Some(PendingSteer {
            answer,
            claim,
            through,
            steered,
        }))
    }

    /// What the agent answered a steer: taken (`injected`, or a turn it
    /// started for it), the turns after cover the input and the session
    /// records that it was steered; otherwise the next round sends it.
    async fn end_steer(
        &mut self,
        pending: PendingSteer,
        answer: Result<String, String>,
    ) -> Result<(), EngineError> {
        match answer.as_deref() {
            Ok("injected" | "startedNewTurn") => {
                self.input_through_seq = self.input_through_seq.max(pending.through);
                self.steer_claims.push(pending.claim);
                self.record_update(AcpSessionUpdate {
                    steered: Some(pending.steered),
                    ..AcpSessionUpdate::default()
                })
                .await
            }
            Ok("promptRequired") => {
                self.unsteered = true;
                Ok(())
            }
            other => {
                self.unsteered = true;
                let reason = match other {
                    Ok(outcome) => format!("it answered `{outcome}`"),
                    Err(error) => error.clone(),
                };
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
            "agent_message_chunk" | "agent_thought_chunk" => {
                let text = update
                    .get("content")
                    .map(wire::content_text)
                    .unwrap_or_default();
                let kind = if kind == "agent_message_chunk" {
                    StreamedOutputKind::Text
                } else {
                    StreamedOutputKind::Reasoning
                };
                self.stream(kind, text).await
            }
            "tool_call" | "tool_call_update" => {
                let fields: ToolCallFields =
                    serde_json::from_value(update.clone()).unwrap_or_default();
                self.tool_call(fields).await
            }
            "plan" => {
                self.record_update(AcpSessionUpdate {
                    plan: Some(Open::Known(plan_block(&update))),
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
                let (options, _) = options_of(options.as_ref(), None);
                self.live.options = options.clone();
                self.engine.remember_live(&self.live);
                self.record_update(AcpSessionUpdate {
                    options: Some(options),
                    ..AcpSessionUpdate::default()
                })
                .await
            }
            "current_mode_update" if self.live.modes_as_option => {
                let Some(mode) = update.get("currentModeId").and_then(Value::as_str) else {
                    return Ok(());
                };
                for option in &mut self.live.options {
                    if option.id == "mode" {
                        mode.clone_into(&mut option.current_value);
                    }
                }
                self.engine.remember_live(&self.live);
                self.record_update(AcpSessionUpdate {
                    options: Some(self.live.options.clone()),
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
            "usage_update" => {
                self.usage_update = Some(update.clone());
                Ok(())
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
                        .push_str(&wire::content_text(content));
                }
                Ok(())
            }
            "compaction_update" => {
                let id = update
                    .get("compactionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                match update.get("status").and_then(Value::as_str) {
                    Some("completed") => {
                        let streamed = self.compactions.remove(&id).unwrap_or_default();
                        let summary = update
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
                            .unwrap_or(streamed);
                        self.notice(
                            AcpNoticeSeverity::Info,
                            "The agent compacted its conversation",
                            Some(summary.trim()).filter(|summary| !summary.is_empty()),
                        )
                        .await
                    }
                    Some("failed") => {
                        self.compactions.remove(&id);
                        let error = update.get("error").map(|error| match error {
                            Value::String(error) => error.clone(),
                            other => other
                                .get("message")
                                .and_then(Value::as_str)
                                .map_or_else(|| other.to_string(), str::to_owned),
                        });
                        self.notice(
                            AcpNoticeSeverity::Warning,
                            "The agent could not compact its conversation",
                            error.as_deref(),
                        )
                        .await
                    }
                    Some("cancelled") => {
                        self.compactions.remove(&id);
                        Ok(())
                    }
                    _ => Ok(()),
                }
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
            return self.progress_or_end(tool_call_id, &fields).await;
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
            },
        );
        self.progress_or_end(tool_call_id, &fields).await
    }

    /// Shows a call's new state, or ends it once it completed or failed.
    async fn progress_or_end(
        &mut self,
        tool_call_id: ToolCallId,
        fields: &ToolCallFields,
    ) -> Result<(), EngineError> {
        match fields.status.as_deref() {
            Some("completed") => self.end_call(&fields.tool_call_id, true).await,
            Some("failed") => self.end_call(&fields.tool_call_id, false).await,
            _ => {
                let (blocks, _) = self.engine.call_content(self.session(), fields);
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
        let (mut blocks, mut text) = self.engine.call_content(session, &shown.fields);
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
        let result = PersistedToolResult {
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
        };
        let outcome = if succeeded {
            Ok(result)
        } else {
            Err(ToolFailure {
                code: ToolCallFailureCode::ExecutionFailed,
                message: if text.is_empty() {
                    "the agent's tool call failed".to_owned()
                } else {
                    truncate(&text, 4096)
                },
                partial_output: None,
            })
        };
        self.engine
            .submit_tool_result_status(self.session(), self.run, tool_call_id, outcome, false)
            .await?;
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
            if let Some(call) = self.calls.get_mut(&acp_id) {
                call.terminated = true;
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
                        partial_output: None,
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
        let known_fields = known.map(|call| call.fields.clone());
        let tool_call_id = known.map(|call| call.tool_call_id);
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
        let agent =
            active.policy.acp_agent().cloned().ok_or_else(|| {
                EngineError::Acp("the run's agent is not an ACP agent".to_owned())
            })?;
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
        let mut turn = Turn {
            engine: self,
            run: run_id,
            active: &active,
            live,
            step: None,
            next_attempt: 1,
            retries: 0,
            calls: HashMap::new(),
            used_call_ids: Default::default(),
            last_turn_seq: None,
            last_text: String::new(),
            input_through_seq: 0,
            claim: None,
            usage_update: None,
            home,
            bridged: Vec::new(),
            steering: process.init().steering(),
            steer_claims: Vec::new(),
            unsteered: false,
            compactions: HashMap::new(),
        };
        loop {
            if active.cancellation.is_cancelled() {
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
                )?
            } else {
                Vec::new()
            };
            let prompt = self.acp_prompt(&claim.events, &mut turn.live, context, &process);
            turn.input_through_seq = claim.events.last().map_or(0, |event| event.seq);
            turn.claim = Some(claim);
            turn.last_text.clear();
            turn.last_turn_seq = None;
            turn.usage_update = None;
            let end = self.acp_round(&mut turn, prompt).await?;
            match end {
                RoundEnd::Completed(final_text) => {
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
                    self.append_run_cancelled_once(&active, run_id, None)?;
                    return Ok(());
                }
                RoundEnd::Failed(reason) => return Err(EngineError::Acp(reason)),
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
        let mut events = events.lock().await;
        // What arrived between rounds, such as the commands an agent
        // announces after `session/new`; turn output from before the
        // prompt (a load's replay) is not this round's.
        while let Ok(event) = events.try_recv() {
            match event {
                SessionEvent::Update(update) => {
                    let kind = update
                        .get("sessionUpdate")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    turn.metadata(&kind, &update).await?;
                }
                SessionEvent::Permission(_, request) => {
                    let _ = request.respond(&wire::permission_outcome(None)).await;
                }
                SessionEvent::BridgeCall { reply, .. } => {
                    let _ = reply.send(bridge_refusal());
                }
            }
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
        // Input to steer wakes the round while the agent can take it.
        let steer_wake = turn
            .steering
            .then(|| self.inner.acp.processes.watch_steering(turn.session()));
        let mut pending_steer: Option<PendingSteer> = None;
        let answer = loop {
            tokio::select! {
                answer = &mut request => break Some(answer),
                () = async {
                    match &steer_wake {
                        Some(wake) => wake.notified().await,
                        None => std::future::pending().await,
                    }
                }, if cancel_deadline.is_none() && pending_steer.is_none() => {
                    pending_steer = turn.begin_steer().await?;
                }
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
                }
                event = events.recv() => match event {
                    Some(SessionEvent::Update(update)) => turn.update(update).await?,
                    Some(SessionEvent::Permission(request, incoming)) => {
                        if cancel_deadline.is_some() {
                            let _ = incoming.respond(&wire::permission_outcome(None)).await;
                        } else {
                            turn.permission(*request, incoming);
                        }
                    }
                    Some(SessionEvent::BridgeCall { name, arguments, reply }) => {
                        turn.bridge_call(name, arguments, reply).await?;
                    }
                    None => break None,
                },
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
            }
        }
        drop(events);
        if steer_wake.is_some() {
            self.inner.acp.processes.unwatch_steering(turn.session());
        }
        if let Some(mut pending) = pending_steer.take() {
            let answer = tokio::time::timeout(CANCEL_GRACE * 5, &mut pending.answer)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_else(|| Err("the agent did not answer the steering request".to_owned()));
            turn.end_steer(pending, answer).await?;
        }
        // A call through the bridge ends before the turn does.
        for call in std::mem::take(&mut turn.bridged) {
            let _ = call.await;
        }
        let cancelled = turn.active.cancellation.is_cancelled();
        let end = match answer {
            Some(Ok(response)) => match response.stop_reason.as_str() {
                "end_turn" => {
                    if turn.step.is_some() || turn.last_turn_seq.is_none() {
                        turn.commit(ModelFinishReason::Stop).await?;
                    }
                    let usage = usage_of(response.usage.as_ref(), turn.usage_update.as_ref());
                    self.record_acp_usage(turn, usage).await?;
                    RoundEnd::Completed((!turn.last_text.is_empty()).then(|| turn.last_text.clone()))
                }
                "cancelled" => RoundEnd::Cancelled,
                "max_tokens" => RoundEnd::Failed("the agent stopped: it reached its token limit (max_tokens)".to_owned()),
                "max_turn_requests" => RoundEnd::Failed(
                    "the agent stopped: it reached its limit of model requests in one turn (max_turn_requests)".to_owned(),
                ),
                "refusal" => RoundEnd::Failed("the agent refused to continue (refusal)".to_owned()),
                other => RoundEnd::Failed(format!("the agent stopped for an unknown reason `{other}`")),
            },
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
                let finish = if matches!(end, RoundEnd::Cancelled) {
                    ModelFinishReason::Cancelled
                } else {
                    ModelFinishReason::Error
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
    /// over the bridge; the session's memory snapshot as a model run sees
    /// it; and the agent document's body.
    fn acp_first_prompt_context(
        &self,
        session: SessionId,
        policy: &crate::policy::FrozenRunPolicy,
        events: &crate::events::EventSnapshot,
        document: &cookie_agent_config::AgentDocument,
    ) -> Result<Vec<String>, EngineError> {
        let hide = document
            .frontmatter
            .acp
            .clone()
            .unwrap_or_default()
            .hidden_tools();
        let offered = self.offered_tools(session, policy, &hide)?;
        let mut context = Vec::new();
        context.extend(self.tool_prompt_block(policy, session, Some(&offered))?);
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
        let images = process.init().agent_capabilities.prompt_capabilities.image;
        prompt.extend(self.input_since(events, boundary, images).0);
        if prompt.is_empty() {
            prompt.push(wire::text_block("Continue."));
        }
        live.first_prompt = false;
        self.remember_live(live);
        prompt
    }

    /// The prompt blocks of the input the session took after `boundary`
    /// (user input applied, producer messages admitted), and which input
    /// that was.
    fn input_since(
        &self,
        events: &crate::events::EventSnapshot,
        boundary: u64,
        images: bool,
    ) -> (Vec<Value>, cookie_agent_protocol::AcpSteered) {
        let producers = crate::goal_projection::GoalProducerProjection::from_events(events);
        let admitted = producers
            .messages
            .iter()
            .filter(|message| !message.discarded)
            .filter_map(|message| {
                message
                    .admission
                    .map(|(_, seq)| (seq, (message.message_id, &message.body)))
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
                        self.push_content(&mut prompt, event.session_id, content, images);
                        taken.user_inputs.push(*user_input_seq);
                    }
                }
                Event::ProducerMessageAdmitted { .. } if event.seq > boundary => {
                    if let Some((message_id, body)) = admitted.get(&event.seq) {
                        self.push_content(&mut prompt, event.session_id, body, images);
                        taken.producer_messages.push(*message_id);
                    }
                }
                _ => {}
            }
        }
        (prompt, taken)
    }

    /// Adds the blocks of `content` to `prompt`: text, and images when the
    /// agent takes them.
    fn push_content(
        &self,
        prompt: &mut Vec<Value>,
        session: SessionId,
        content: &cookie_agent_protocol::UserContent,
        images: bool,
    ) {
        for part in content.parts() {
            match part {
                Open::Known(UserContentPart::Text { text }) => prompt.push(wire::text_block(text)),
                Open::Known(UserContentPart::File { artifact }) => {
                    let image = images && artifact.media_type.starts_with("image/");
                    let bytes = image
                        .then(|| self.read_user_artifact(session, artifact))
                        .flatten();
                    match bytes {
                        Some(bytes) => {
                            use base64::Engine as _;
                            prompt.push(serde_json::json!({
                                "type": "image",
                                "mimeType": artifact.media_type,
                                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                            }));
                        }
                        None => prompt.push(wire::text_block(&format!(
                            "[attached file {} ({}), which this agent cannot read]",
                            artifact.filename.as_deref().unwrap_or("unnamed"),
                            artifact.media_type
                        ))),
                    }
                }
                Open::Unknown { .. } => {}
            }
        }
    }

    /// The ACP session `session` talks to `agent` in: the live one, else the
    /// one its log records resumed or loaded, else a new one. Starting,
    /// resuming, or loading one records it.
    pub(crate) async fn acp_session(
        &self,
        session: SessionId,
        run: Option<RunId>,
        agent: &RuntimeAgent,
        document: &cookie_agent_config::AgentDocument,
        process: &Arc<AgentProcess>,
    ) -> Result<LiveSession, EngineError> {
        if let Some(live) = self.live_session(session)
            && Arc::ptr_eq(&live.process, process)
            && live.is_live()
            && live.agent.id == agent.id
        {
            return Ok(live);
        }
        let cwd = self.acp_cwd(session, agent, process)?;
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
        let prior = self.recorded_acp_session(session, agent)?;
        let caps = &process.init().agent_capabilities;
        let mut opened = None;
        let mut lost = false;
        if let Some(prior) = &prior {
            let receiver = process.route(&prior.acp_session_id);
            let request = wire::ExistingSessionRequest {
                session_id: &prior.acp_session_id,
                cwd: &cwd,
                mcp_servers: &mcp_servers,
            };
            let attempt = if caps.session("resume") {
                Some((wire::method::SESSION_RESUME, AcpSessionStart::Resumed))
            } else if caps.load_session {
                Some((wire::method::SESSION_LOAD, AcpSessionStart::Loaded))
            } else {
                None
            };
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
                            mcp_servers: &mcp_servers,
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
                    agent_info: process.init().agent_info.clone(),
                    how,
                    steering: process.init().steering(),
                },
            },
        )
        .await?;
        let (options, modes_as_option) =
            options_of(response.config_options.as_ref(), response.modes.as_ref());
        let mut live = LiveSession {
            session,
            process: Arc::clone(process),
            agent: agent.clone(),
            acp_session_id,
            cwd,
            bridge: bridge.ok().map(|opened| opened.bridge),
            options,
            modes_as_option,
            first_prompt: matches!(how, AcpSessionStart::New),
            events: Arc::new(tokio::sync::Mutex::new(receiver)),
        };
        if matches!(how, AcpSessionStart::New) {
            self.apply_starting_options(session, run, &mut live, document)
                .await;
        }
        if !live.options.is_empty() {
            self.append(
                session,
                run,
                origin(),
                Event::AcpSessionUpdated {
                    update: AcpSessionUpdate {
                        options: Some(live.options.clone()),
                        ..AcpSessionUpdate::default()
                    },
                },
            )
            .await?;
        }
        self.remember_live(&live);
        Ok(live)
    }

    /// The starting mode (section 5): the document's `acp.mode`, else the
    /// agent's `auto`, else its default; then the document's `acp.config`.
    async fn apply_starting_options(
        &self,
        session: SessionId,
        run: Option<RunId>,
        live: &mut LiveSession,
        document: &cookie_agent_config::AgentDocument,
    ) {
        let settings = document.frontmatter.acp.clone().unwrap_or_default();
        let mode_option = live
            .options
            .iter()
            .find(|option| option.category.as_deref() == Some("mode") || option.id == "mode")
            .cloned();
        if let Some(option) = mode_option {
            let wanted = settings.mode.clone().or_else(|| {
                option
                    .values
                    .iter()
                    .any(|value| value.value == "auto")
                    .then(|| "auto".to_owned())
            });
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
    }

    /// Sets option `id` of `live` to `value` with the agent.
    pub(crate) async fn set_live_option(
        &self,
        live: &mut LiveSession,
        id: &str,
        value: &str,
    ) -> Result<(), String> {
        if live.modes_as_option && id == "mode" {
            live.process
                .peer
                .request::<_, Value>(
                    wire::method::SESSION_SET_MODE,
                    &wire::SetModeRequest {
                        session_id: &live.acp_session_id,
                        mode_id: value,
                    },
                )
                .await
                .map_err(|error| agent_error_message(&error))?;
            for option in &mut live.options {
                if option.id == "mode" {
                    value.clone_into(&mut option.current_value);
                }
            }
        } else {
            let response = live
                .process
                .peer
                .request::<_, wire::SetConfigOptionResponse>(
                    wire::method::SESSION_SET_CONFIG_OPTION,
                    &wire::SetConfigOptionRequest {
                        session_id: &live.acp_session_id,
                        config_id: id,
                        value,
                    },
                )
                .await
                .map_err(|error| agent_error_message(&error))?;
            match response.config_options {
                Some(options) => {
                    let (mut options, _) = options_of(Some(&options), None);
                    if live.modes_as_option
                        && let Some(mode) = live.options.iter().find(|option| option.id == "mode")
                    {
                        options.push(mode.clone());
                    }
                    live.options = options;
                }
                None => {
                    for option in &mut live.options {
                        if option.id == id {
                            value.clone_into(&mut option.current_value);
                        }
                    }
                }
            }
        }
        self.remember_live(live);
        Ok(())
    }

    /// Records a notice about the session's ACP agent.
    async fn acp_notice(&self, session: SessionId, _run: Option<RunId>, message: String) {
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

    /// The ACP session the log of `session` records last for `agent`.
    fn recorded_acp_session(
        &self,
        session: SessionId,
        agent: &RuntimeAgent,
    ) -> Result<Option<AcpSessionRecord>, EngineError> {
        let events = self.inner.store.get(session)?.log.event_snapshot();
        Ok(events.iter().rev().find_map(|event| match &event.payload {
            Event::AcpSessionStarted { record } if record.agent == agent.id => Some(record.clone()),
            _ => None,
        }))
    }

    /// The live ACP session of `session`, if any.
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

    /// Ends the ACP session of `session`, as the session is deleted:
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
        let Some(live) = live else {
            return;
        };
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
        if live.process.init().agent_capabilities.session("close") {
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                live.process.peer.request::<_, Value>(
                    wire::method::SESSION_CLOSE,
                    &wire::SessionRef {
                        session_id: &live.acp_session_id,
                    },
                ),
            )
            .await;
        }
        live.process.unroute(&live.acp_session_id);
    }

    /// Sets the session's title from the agent's, unless the user set one.
    async fn set_acp_title(
        &self,
        session: SessionId,
        run: RunId,
        title: &str,
    ) -> Result<(), EngineError> {
        let eligible = self.inner.store.get(session)?.automatic_title_eligible;
        let Ok(title) = cookie_agent_protocol::SessionTitle::new(truncate(title.trim(), 512))
        else {
            return Ok(());
        };
        if !eligible {
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
        let agent = resolved.acp.clone().expect("filtered to ACP agents");
        if !agent.available() {
            return Err(EngineError::Acp(format!(
                "ACP agent `{}` is unavailable: runtime `{}` is not connected",
                agent.id, agent.runtime
            )));
        }
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

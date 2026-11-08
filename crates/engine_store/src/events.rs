//! Buffered/durable session event logs and ephemeral tool-output hubs.

use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[cfg(unix)]
use std::fs::File;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    AssistantToolCallRef, AttemptId, DelegationStage, EventOrigin, EventPayload, ModelCallId, Open,
    OutputDelta, OutputGap, OutputSnapshot, OutputStream, ProviderItemId, RunId, SessionId,
    StoredEvent, StoredEventEnvelope, ToolCallId, deserialize_event_payload_best_effort,
    display_view::CacheStartFold,
};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::ownership::{WriteAuthority, WriteCapability};

pub mod blobs;
mod durability;
mod history;
pub mod model_context;
pub(crate) mod sorted;
pub mod state;
pub mod state_file;
#[cfg(any(test, feature = "test-support"))]
pub mod synthetic;
pub use durability::Commit;
pub use history::{History, NewestPass, OlderPage, ViewCacheStart};
pub use state::LogState;

#[derive(Debug, Error)]
pub enum EventLogError {
    #[error("event log IO failure at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid JSONL record in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("event log {0} has no SessionCreated record")]
    MissingCreation(PathBuf),
    #[error("corrupt event log at {path}: {message}")]
    Corrupt { path: PathBuf, message: String },
    #[error("event log {0} is read-only")]
    ReadOnly(PathBuf),
    /// A revert cut the file under a pass that had yet to read that far. The
    /// records it asked for are gone: a reader that still wants them asks again.
    #[error("a revert cut {} during a read of it", path.display())]
    HistoryCut { path: PathBuf },
    /// A newer build wrote this log: it holds the state event `event` this
    /// build does not know, so the session is render-only, or it has the
    /// newer `history_format`, so it does not open.
    #[error("{} was written by a newer cookie: {}", path.display(), match (event, history_format) {
        (Some(event), _) => format!("unknown event `{event}`"),
        (None, Some(format)) => format!("history format {format}"),
        (None, None) => "newer history".to_owned(),
    })]
    RequiresNewerEngine {
        path: PathBuf,
        event: Option<String>,
        history_format: Option<u32>,
    },
    /// The log's sequence stops increasing at the record `seq`: that record
    /// and everything after it is not read, and the session can only be
    /// viewed.
    #[error("{} breaks its sequence at {seq}; the session can only be viewed", path.display())]
    SequenceBroken { path: PathBuf, seq: u64 },
}

/// Why a log that opened takes no appends: it can be read and followed, but
/// nothing changes it.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
enum RenderOnly {
    /// The first state event of a type this build does not know: a newer
    /// build wrote it.
    UnknownStateEvent(String),
    /// The sequence stopped increasing at this record; it and what follows
    /// were not read.
    SequenceBroken(u64),
}

/// The largest window a view cache answers: every request's budget.
pub(crate) const VIEW_BUDGET: usize = cookie_agent_protocol::HISTORY_WINDOW_MAX_BYTES as usize;

#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
    session_id: SessionId,
    append: Mutex<()>,
    events: Mutex<EventStorage>,
    diagnostics: Vec<EventLoadDiagnostic>,
    /// Damage found at load. Truncation drops what it no longer reaches.
    initial_validation_taint: Mutex<ValidationTaint>,
    validation: Mutex<ValidationState>,
    next_seq: AtomicU64,
    persisted: AtomicBool,
    read_only: bool,
    /// A read-only open found a torn final record and left it in the file;
    /// an owned open cuts it.
    torn_tail_ignored: bool,
    /// A read-only open converted the log from an older history format in
    /// memory; the file still holds the old one.
    converted: bool,
    /// Why the log is render-only, when it is: it can be read and followed,
    /// but nothing appends to it.
    render_only: Option<RenderOnly>,
    write_capability: Option<WriteCapability>,
    #[cfg(any(test, feature = "test-support"))]
    _test_authority: Option<WriteAuthority>,
    #[cfg(any(test, feature = "test-support"))]
    append_authorization_hook: Mutex<Option<AppendAuthorizationHook>>,
    writer: Mutex<Option<EventLogWriter>>,
    /// What of the file is synced; shared with the writer.
    durability: Arc<durability::Durability>,
    /// How many cuts of the file began: a pass that reads the file sees it
    /// change under it.
    cuts: Arc<AtomicU64>,
    /// What the log's records folded to as it opened, at the tip it opened with,
    /// when it did not read itself whole: its first projection takes this.
    opened: Mutex<Option<(u64, crate::session::ProjectionFolds)>>,
    /// How much of the file `state.json` covered when it was last written or read.
    state_written: AtomicU64,
    /// Test-only: what asked for a pass over the log, and what took one that
    /// reads the file, each where it did so.
    #[cfg(any(test, feature = "test-support"))]
    history_calls: Mutex<Vec<String>>,
    /// Test-only: the log opened from its `state.json`.
    #[cfg(any(test, feature = "test-support"))]
    opened_from_state: bool,
    /// Test-only: what a native window would hold, as of the cuts counted
    /// then (`Self::assert_model_context_holds`).
    #[cfg(any(test, feature = "test-support"))]
    model_context_shadow: Mutex<Option<(u64, Vec<Arc<StoredEvent>>)>>,
}

#[derive(Clone, Copy)]
enum TornTail {
    Truncate,
    Ignore,
}

/// The open append handle of a persisted log. Every record is written before
/// it is published, so nothing is buffered here. Syncs run outside it, on
/// the log's [`durability::Durability`]; closing it syncs what no sync
/// covered yet.
#[derive(Debug)]
struct EventLogWriter {
    path: PathBuf,
    file: fs::File,
    durability: Arc<durability::Durability>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
struct AppendAuthorizationHook {
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct EventIndex {
    last_run_started: Option<(u64, RunId, cookie_agent_protocol::ModelSelection)>,
    last_checkpoint_seq: u64,
    last_checkpoint_input_through_seq: u64,
    last_recorded_usage: Option<(u64, u64)>,
    last_turn_usage: Option<(u64, u64)>,
    /// Visible `ModelAttemptStarted` records of each of the latest
    /// [`ATTEMPTED_RUNS_KEPT`] runs to try a model, oldest first: an attempt asks
    /// of its own run, the latest.
    run_attempts: VecDeque<(RunId, u32)>,
    /// Tool names of the latest visible `ModelToolsPublished` record.
    last_model_tools: Option<(Vec<String>, Vec<String>)>,
    /// The latest runtime revisions the log records.
    revisions: cookie_agent_protocol::RevisionFold,
    /// Deferred tools since the latest checkpoint, shared with the readers
    /// handed it: copied only when a record changes it while one holds it.
    deferral: Arc<DeferralFold>,
    /// The prompt blocks of the latest run and what the model knows of them.
    prompt_blocks: PromptBlockFold,
    /// The log records an ACP agent's own subagent session.
    acp_subagent: bool,
    /// The latest `MemoryLoaded` record, and whether a checkpoint was
    /// committed after it.
    last_memory_loaded: Option<(Arc<StoredEvent>, bool)>,
    /// What readers need of the whole log, shared with those handed it:
    /// copied only when a record changes it while one holds it.
    state: Arc<LogState>,
    /// Where the log's window starts, the latest committed turn of an ACP agent,
    /// while the session's agent is one (`docs/site/specs/forever-sessions.md`
    /// section 3.2). A native run ends that: its model reads the log.
    window_start: Option<u64>,
}

/// How many runs' attempt counts a log's index keeps.
const ATTEMPTED_RUNS_KEPT: usize = 16;

/// What a log records about prompt blocks (stable prefix spec, section 6):
/// the blocks the latest run's system prompt carries, and the blocks the
/// model knows, those folded with the announcements since.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PromptBlockFold {
    /// The latest run's composed prompt, until its blocks are recorded.
    prompt: Option<String>,
    /// What the model knew when the latest run started, until its blocks
    /// are recorded.
    known_at_start: Option<std::collections::BTreeMap<String, String>>,
    /// The latest run's blocks, in order; `None` when it recorded none.
    pub run_blocks: Option<Vec<(String, String)>>,
    /// A checkpoint lies after the latest run's start.
    pub checkpoint_since_run: bool,
    /// The blocks the model knows; `None` while the run's blocks are unknown.
    pub known: Option<std::collections::BTreeMap<String, String>>,
}

impl PromptBlockFold {
    fn observe(&mut self, payload: &EventPayload) {
        match payload {
            EventPayload::RunStarted { agent, .. } => {
                *self = Self {
                    prompt: Some(agent.composed_prompt.clone()),
                    known_at_start: self.known.take(),
                    ..Self::default()
                };
            }
            EventPayload::PromptBlocksRecorded {
                blocks,
                composed_live,
            } => {
                let Some(prompt) = self.prompt.take() else {
                    return;
                };
                let previous = self.known_at_start.take();
                self.run_blocks = prompt_block_texts(&prompt, blocks);
                // A run that reused the previous run's blocks carries the
                // same text, so what the model knew of them still holds.
                self.known = match (composed_live, previous) {
                    (false, Some(previous)) => Some(previous),
                    _ => self
                        .run_blocks
                        .as_ref()
                        .map(|blocks| blocks.iter().cloned().collect()),
                };
            }
            // A checkpoint inside the run composed its prompt afresh: its
            // blocks are the run's now, and the model knows them as they are.
            EventPayload::SystemPromptComposed {
                composed_prompt,
                blocks,
                ..
            } => {
                self.prompt = None;
                self.known_at_start = None;
                self.run_blocks = blocks
                    .as_ref()
                    .and_then(|blocks| prompt_block_texts(composed_prompt, blocks));
                self.known = self
                    .run_blocks
                    .as_ref()
                    .map(|blocks| blocks.iter().cloned().collect());
            }
            EventPayload::ContextCheckpointCommitted { .. } => {
                self.checkpoint_since_run = true;
                // Announcements before a checkpoint are left out of history.
                self.known = self
                    .run_blocks
                    .as_ref()
                    .map(|blocks| blocks.iter().cloned().collect());
            }
            EventPayload::PromptBlocksAnnounced { blocks } => {
                if let Some(known) = &mut self.known {
                    for block in blocks {
                        match &block.text {
                            Some(text) => {
                                known.insert(block.key.clone(), text.clone());
                            }
                            None => {
                                known.remove(&block.key);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// What a log records about deferred tools since its latest checkpoint
/// (deferred tools in cookie, sections 6 to 8): the current tool round, the
/// tools results loaded, and what announcements add up to. A checkpoint
/// starts all three over, keeping the tools it folded.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DeferralFold {
    /// The latest `tool_round_started` record.
    pub round: Option<ToolRound>,
    /// Tools a result loaded.
    pub loaded: std::collections::BTreeSet<String>,
    /// Announced, still-loadable tools by group.
    pub announced: std::collections::BTreeMap<String, AnnouncedGroupState>,
    /// Tools the latest checkpoint folded: the next round declares them up
    /// front (section 8.3).
    pub folded: std::collections::BTreeSet<String>,
    /// Round tools announced as no longer available, by tool and machine
    /// (`None`: every machine), since the round started (session tool
    /// groups spec, section 8.2).
    pub unavailable: std::collections::BTreeSet<(String, Option<String>)>,
}

/// The eager tools of a tool round and the groups whose prompt sections the
/// system prompt carries.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolRound {
    pub tools: Vec<cookie_agent_protocol::RoundToolDefinition>,
    pub prompt_groups: Vec<String>,
    /// The engine build that started the round, when it recorded one.
    pub engine_build: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct AnnouncedGroupState {
    pub tools: std::collections::BTreeSet<String>,
    /// The summary the latest announcement of the group carried.
    pub summary: Option<String>,
}

impl DeferralFold {
    /// Whether `payload` can change the fold.
    fn observes(payload: &EventPayload) -> bool {
        matches!(
            payload,
            EventPayload::ContextCheckpointCommitted { .. }
                | EventPayload::ContextRefreshed { .. }
                | EventPayload::ToolRoundStarted { .. }
                | EventPayload::DeferredToolsAnnounced { .. }
                | EventPayload::ToolsAvailabilityAnnounced { .. }
        ) || matches!(
            payload,
            EventPayload::ToolCallTerminated { termination }
                if termination
                    .result
                    .as_ref()
                    .is_some_and(|result| !result.loaded_tools.is_empty())
        )
    }

    fn observe(&mut self, payload: &EventPayload) {
        match payload {
            EventPayload::ContextCheckpointCommitted { commit } => {
                *self = Self {
                    folded: commit.folded_tools().iter().cloned().collect(),
                    ..Self::default()
                };
            }
            // A refresh starts a new round; what was loaded and announced
            // stays, since the conversation that loaded it does (frozen
            // context spec, section 6.1).
            EventPayload::ContextRefreshed { .. } => {
                self.round = None;
                self.unavailable.clear();
            }
            EventPayload::ToolsAvailabilityAnnounced {
                unavailable,
                available_again,
            } => {
                for group in available_again {
                    for tool in &group.tools {
                        self.unavailable
                            .remove(&(tool.clone(), group.machine.clone()));
                    }
                }
                for group in unavailable {
                    for tool in &group.tools {
                        self.unavailable
                            .insert((tool.clone(), group.machine.clone()));
                    }
                }
            }
            EventPayload::ToolRoundStarted {
                tools,
                prompt_groups,
                engine_build,
            } => {
                self.round = Some(ToolRound {
                    tools: tools.clone(),
                    prompt_groups: prompt_groups.clone(),
                    engine_build: engine_build.clone(),
                });
            }
            EventPayload::ToolCallTerminated { termination } => {
                if let Some(result) = &termination.result {
                    self.loaded.extend(result.loaded_tools.iter().cloned());
                }
            }
            EventPayload::DeferredToolsAnnounced { added, removed } => {
                for group in removed {
                    if let Some(state) = self.announced.get_mut(&group.name) {
                        for tool in &group.tools {
                            state.tools.remove(tool);
                        }
                        if state.tools.is_empty() {
                            self.announced.remove(&group.name);
                        }
                    }
                }
                for group in added {
                    let state = self.announced.entry(group.name.clone()).or_default();
                    state.tools.extend(group.tools.iter().cloned());
                    if group.summary.is_some() {
                        state.summary.clone_from(&group.summary);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Whether `model` is an ACP agent's synthetic model.
fn is_acp_model(model: &cookie_agent_protocol::ModelSelection) -> bool {
    model.model.provider_id().as_str() == cookie_agent_protocol::ACP_MODEL_PROVIDER
}

/// What `payload` makes of whether a log's window is an ACP agent's, the rule the
/// log's own window follows ([`EventIndex::observe`]), and `None` when it says
/// nothing of that: a committed turn sets it, and a native model's turn or run
/// ends it. What is settled is let go of only while it holds.
pub(crate) fn windowed_after(payload: &EventPayload) -> Option<bool> {
    match payload {
        EventPayload::RunStarted { selection, .. } if !is_acp_model(&selection.model) => {
            Some(false)
        }
        EventPayload::ModelTurnCommitted { resolved_model, .. } => {
            Some(is_acp_model(&resolved_model.selection))
        }
        _ => None,
    }
}

/// What a log's records say of what its folds may let go of
/// (`docs/site/specs/forever-sessions.md` sections 6 and 12): while its agent is
/// an ACP agent ([`windowed_after`]), and once a native model's log has a
/// checkpoint, as its context then starts there. A native log with none keeps
/// everything, as its model reads it all.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) struct LettingGo {
    acp: bool,
    /// Where a native model's context starts, as of the latest checkpoint
    /// ([`model_context::context_window_start`]), never moving back.
    context_from: Option<u64>,
    /// Where the latest summary's context starts: a provider window alone, which
    /// builds before 1.29.0 wrote, leaves the context starting there.
    summary_from: Option<u64>,
}

impl LettingGo {
    pub(crate) fn observe(&mut self, event: &StoredEvent) {
        let payload = &event.payload;
        if let Some(acp) = windowed_after(payload) {
            self.acp = acp;
        }
        // An approval reviewer's epoch starts its own log's context.
        if let EventPayload::ReviewerTurnRecorded { turn } = payload
            && turn.epoch.is_some()
        {
            self.context_from = Some(
                self.context_from
                    .map_or(event.seq, |from| from.max(event.seq)),
            );
        }
        if let EventPayload::ContextCheckpointCommitted { commit } = payload {
            let start = model_context::commit_window_start(commit);
            let start = match commit.checkpoint {
                cookie_agent_protocol::ContextCheckpoint::InternalSummary { .. } => {
                    self.summary_from = Some(start);
                    start
                }
                cookie_agent_protocol::ContextCheckpoint::NativeWindow { .. } => {
                    start.min(self.summary_from.unwrap_or(0))
                }
            };
            self.context_from = Some(self.context_from.map_or(start, |from| from.max(start)));
        }
    }

    /// Whether what settled may go.
    pub(crate) fn lets_go(self) -> bool {
        self.acp || self.context_from.is_some()
    }

    /// The earliest end of a call that a valid record can still name, a native
    /// model's compaction eliding its output: the context's start. An ACP agent's
    /// log is never compacted so.
    fn calls_in_view_from(self) -> Option<u64> {
        if self.acp { None } else { self.context_from }
    }
}

impl EventIndex {
    fn observe(&mut self, event: &Arc<StoredEvent>) {
        Arc::make_mut(&mut self.state).observe(event);
        self.revisions.observe(&event.payload);
        if DeferralFold::observes(&event.payload) {
            Arc::make_mut(&mut self.deferral).observe(&event.payload);
        }
        self.prompt_blocks.observe(&event.payload);
        match &event.payload {
            EventPayload::AcpSessionStarted { record }
                if record.how == cookie_agent_protocol::AcpSessionStart::Subagent =>
            {
                self.acp_subagent = true;
            }
            // A machine's memories added in place leave the memory turn
            // as it was.
            EventPayload::MemoryLoaded {
                in_place: false, ..
            } => {
                self.last_memory_loaded = Some((Arc::clone(event), false));
            }
            _ => {}
        }
        match &event.payload {
            EventPayload::RunStarted { selection, .. } => {
                if let Some(run) = event.run_id {
                    self.last_run_started = Some((event.seq, run, selection.model.clone()));
                }
                if !is_acp_model(&selection.model) {
                    self.window_start = None;
                }
            }
            EventPayload::ContextCheckpointCommitted { commit } => {
                if let Some((_, compacted)) = &mut self.last_memory_loaded {
                    *compacted = true;
                }
                self.last_checkpoint_seq = event.seq;
                self.last_checkpoint_input_through_seq = commit.boundaries.input_through_seq;
            }
            EventPayload::ModelUsageRecorded { usage, .. } => {
                if let Some(usage) = usage_total(event.seq, usage) {
                    self.last_recorded_usage = Some(usage);
                }
            }
            EventPayload::ModelTurnCommitted {
                turn,
                resolved_model,
                ..
            } => {
                if let Some(usage) = usage_total(event.seq, &turn.usage) {
                    self.last_turn_usage = Some(usage);
                }
                self.window_start = is_acp_model(&resolved_model.selection).then_some(event.seq);
            }
            EventPayload::ModelAttemptStarted { .. } => {
                if let Some(run) = event.run_id {
                    match self
                        .run_attempts
                        .iter_mut()
                        .find(|(attempted, _)| *attempted == run)
                    {
                        Some((_, attempts)) => *attempts += 1,
                        None => {
                            self.run_attempts.push_back((run, 1));
                            if self.run_attempts.len() > ATTEMPTED_RUNS_KEPT {
                                self.run_attempts.pop_front();
                            }
                        }
                    }
                }
            }
            EventPayload::ModelToolsPublished {
                tool_names,
                deferred_tool_names,
                ..
            } => {
                self.last_model_tools = Some((tool_names.clone(), deferred_tool_names.clone()));
            }
            _ => {}
        }
    }

    fn latest_real_usage(&self) -> Option<(u64, u64)> {
        self.last_recorded_usage.or(self.last_turn_usage)
    }
}

/// A shared, immutable view of a log's events. Taking one is a reference
/// count bump; the log copies the pointer list only when it appends while an
/// older view is still alive, and never deep-copies an event.
pub type EventSnapshot = Arc<Vec<Arc<StoredEvent>>>;

#[derive(Clone, Debug)]
struct EventStorage {
    /// The records the log holds, in log order: every one, or from `window` on.
    /// The list is rebuilt when the window moves, so a reader's view of it stays
    /// as it was, and only for as long as the reader holds it.
    all: EventSnapshot,
    /// The log's first record, whatever the window.
    creation: Option<Arc<StoredEvent>>,
    /// The sequence the log holds its records from, when it does not hold them
    /// all: the rest are in the file, for a pass to read
    /// (`docs/site/specs/forever-sessions.md` sections 3.2 and 3.3).
    window: Option<u64>,
    /// What the log holds is a native model's context: the window, which starts
    /// where the latest checkpoint's context does, and before it the records
    /// that what follows still names, which [`model_context::model_context`]
    /// chose (section 5). Without it, a window holds nothing before its start.
    context: bool,
    /// Where a view cache of a windowed log starts, which it cannot walk back
    /// from its end to find.
    view_start: Option<Box<CacheStartFold>>,
    /// The sequences, ascending, of the records in the file that validation left
    /// out at load: a pass leaves them out too.
    refused: Arc<Vec<u64>>,
    index: EventIndex,
    /// `ModelTurnCommitted` records.
    physical_model_turns: u64,
    /// Where each sequenced line of the persisted file starts, so a revert
    /// cuts the file without reading it. `None` when the file's layout is not
    /// known (a buffered log published without one, or after a failed
    /// write); a cut then scans the file.
    lines: Option<LineIndex>,
    /// The bytes of the lines the log was read from; empty for a log built
    /// in memory.
    raw: Arc<RawLines>,
}

/// Byte offsets of the sequenced lines of an event file, in file order: of each
/// line from where a window's log holds its records, and of a bounded number of
/// those before (`docs/site/specs/forever-sessions.md` section 6), so what the
/// file holds before the window is found by reading it from the nearest of them.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct LineIndex {
    /// `(seq, start offset)` of every line that carries a sequence, records
    /// and skipped lines alike, from the first the log holds its records of:
    /// every one until a window lets go of the rest. Sequences strictly
    /// increase down the file.
    starts: Vec<(u64, u64)>,
    /// Some of the lines before `starts`, spread over the file.
    marks: Marks,
    /// The sequences the file skips, each `(first, last)`, which a load's taint
    /// must explain.
    gaps: Vec<(u64, u64)>,
    /// Length of the file's complete lines.
    len: u64,
}

/// Where some of the lines a window let go of start: at least `spacing` bytes
/// apart, and never more than [`MARKS_KEPT`], so a read from the nearest reads
/// a bounded share of the file. When full, every other one goes and the spacing
/// doubles. Each line is noted in file order, so it holds the same whatever
/// moments a window let go at.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
struct Marks {
    points: Vec<(u64, u64)>,
    spacing: u64,
}

const MARKS_KEPT: usize = 512;
const FIRST_MARK_SPACING: u64 = 64 * 1024;

impl Marks {
    fn note(&mut self, seq: u64, start: u64) {
        let spacing = self.spacing.max(FIRST_MARK_SPACING);
        if self
            .points
            .last()
            .is_some_and(|(_, last)| start < last + spacing)
        {
            return;
        }
        self.points.push((seq, start));
        self.spacing = spacing;
        if self.points.len() > MARKS_KEPT {
            let mut index = 0;
            self.points.retain(|_| {
                index += 1;
                index % 2 == 1
            });
            self.spacing = spacing * 2;
        }
    }
}

impl LineIndex {
    /// Notes a line of `length` bytes, newline included, appended at the end.
    pub fn push(&mut self, seq: Option<u64>, length: u64) {
        if let Some(seq) = seq {
            self.note(seq, self.len);
        }
        self.len += length;
    }

    /// Notes the sequenced line at `start`, after every one noted.
    fn note(&mut self, seq: u64, start: u64) {
        if let Some((last, _)) = self.starts.last()
            && seq > last + 1
        {
            self.gaps.push((last + 1, seq - 1));
        }
        self.starts.push((seq, start));
    }

    /// The first sequence of `starts`, while the index let go of any line.
    fn held_from(&self) -> Option<u64> {
        (!self.marks.points.is_empty()).then(|| self.starts.first().map(|(seq, _)| *seq))?
    }

    /// Lets go of every line sequenced before `seq`, keeping marks of them.
    fn let_go_before(&mut self, seq: u64) {
        let gone = self.starts.partition_point(|(line, _)| *line < seq);
        if gone == 0 {
            return;
        }
        for (line, start) in self.starts.drain(..gone) {
            self.marks.note(line, start);
        }
    }

    /// Where a line at or before the one of `seq` starts: a line of the file, so a
    /// read from there meets `seq`'s first.
    fn start_at_or_before(&self, seq: u64) -> u64 {
        if self.starts.first().is_none_or(|(first, _)| seq < *first) {
            let marks = &self.marks.points;
            let index = marks.partition_point(|(mark, _)| *mark <= seq);
            return index.checked_sub(1).map_or(0, |index| marks[index].1);
        }
        let index = self.starts.partition_point(|(line, _)| *line < seq);
        self.starts.get(index).map_or(self.len, |(_, at)| *at)
    }

    /// Where the first line sequenced at or after `seq` starts, or a later line,
    /// or the file's end: what a read that stops before `seq` may stop at.
    fn start_at_or_after(&self, seq: u64) -> u64 {
        if self.starts.first().is_none_or(|(first, _)| seq < *first) {
            let marks = &self.marks.points;
            let index = marks.partition_point(|(mark, _)| *mark < seq);
            if let Some((_, at)) = marks.get(index) {
                return *at;
            }
        }
        let index = self.starts.partition_point(|(line, _)| *line < seq);
        self.starts.get(index).map_or(self.len, |(_, at)| *at)
    }

    /// The length that keeps every line up to the first one sequenced after
    /// `through_seq`; lines without a sequence before it stay. `None` when that
    /// line is one the index let go of: the file has to be read for it, from
    /// [`Self::start_at_or_before`].
    fn cut_after(&self, through_seq: u64) -> Option<u64> {
        if self.held_from().is_some_and(|first| through_seq < first) {
            return None;
        }
        let kept = self.starts.partition_point(|(seq, _)| *seq <= through_seq);
        Some(self.starts.get(kept).map_or(self.len, |(_, start)| *start))
    }

    /// Drops what follows `through_seq`, the file now `length` long.
    fn truncate(&mut self, through_seq: u64, length: u64) {
        let kept = self.starts.partition_point(|(seq, _)| *seq <= through_seq);
        self.starts.truncate(kept);
        self.marks.points.retain(|(seq, _)| *seq <= through_seq);
        self.gaps.retain(|(first, _)| *first <= through_seq);
        for (_, last) in &mut self.gaps {
            *last = (*last).min(through_seq);
        }
        self.len = length;
    }

    /// The index of the first `length` bytes of the log at `path`, read from it,
    /// holding every line from `held_from` on and marks of those before: what a
    /// revert into what a window let go of needs, read as the window's moves would
    /// have built it.
    fn scanned(path: &Path, length: u64, held_from: Option<u64>) -> Result<Self, EventLogError> {
        let mut index = Self::default();
        for (seq, start) in scan_line_starts(path, 0, length)? {
            index.note(seq, start);
            if held_from.is_some_and(|held_from| seq < held_from) {
                index.let_go_before(seq + 1);
            }
        }
        index.len = length;
        Ok(index)
    }

    /// Test-only: this index as one that let go where `other` did: what a log
    /// that read its file whole, and did not let go of anything, compares with.
    #[cfg(any(test, feature = "test-support"))]
    fn let_go_as(&self, other: &Self) -> Self {
        let mut index = self.clone();
        if let Some(first) = other.held_from() {
            index.let_go_before(first);
        }
        index
    }
}

/// `(seq, start)` of each line of `path` in `from..until` that carries a
/// sequence, read as [`scanned_cut_length`] reads one.
fn scan_line_starts(path: &Path, from: u64, until: u64) -> Result<Vec<(u64, u64)>, EventLogError> {
    use std::io::{BufRead as _, Read as _, Seek as _};
    #[derive(Deserialize)]
    struct Sequenced {
        seq: u64,
    }
    let io_error = |source| EventLogError::Io {
        path: path.to_owned(),
        source,
    };
    crate::blocking::off_worker(|| {
        let mut file = fs::File::open(path).map_err(io_error)?;
        file.seek(io::SeekFrom::Start(from)).map_err(io_error)?;
        let mut reader = io::BufReader::new(file.take(until.saturating_sub(from)));
        let mut line = Vec::new();
        let mut at = from;
        let mut starts = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line).map_err(io_error)?;
            if read == 0 {
                return Ok(starts);
            }
            if let Ok(sequenced) = serde_json::from_slice::<Sequenced>(&line) {
                starts.push((sequenced.seq, at));
            }
            at += read as u64;
        }
    })
}

/// The length a cut of `path` after `through_seq` keeps, reading it from `from`,
/// a line's start before the cut.
fn scanned_cut_from(path: &Path, from: u64, through_seq: u64) -> Result<u64, EventLogError> {
    let length = fs::metadata(path)
        .map_err(|source| EventLogError::Io {
            path: path.to_owned(),
            source,
        })?
        .len();
    Ok(scan_line_starts(path, from, length)?
        .into_iter()
        .find(|(seq, _)| *seq > through_seq)
        .map_or(length, |(_, start)| start))
}

/// The lines a log was read from: a rewrite of the log (a fork) copies them,
/// so what a newer build wrote survives it, whether this build skipped the
/// line or read it without some member, and artifact liveness sees every
/// member of every record, known or not.
///
/// A line read into its record, its values in blobs read back or not, stays in
/// the file only, which those rare readers read again (`Self::read`); a line no
/// record reproduces (one this build skipped, quarantined, or repaired, or one
/// of a log converted in memory) is kept here.
#[derive(Clone, Default)]
pub struct RawLines {
    /// The file the lines left in a file were read from.
    file: Option<Arc<Path>>,
    /// Every sequenced line read, ascending by sequence: of a log whose window
    /// let go of its records, only those kept, as the file holds every other
    /// line as its record ([`Self::partial`]).
    lines: Vec<RawLine>,
    /// Only the lines kept are listed: a log that holds only its window lets go
    /// of the rest (`docs/site/specs/forever-sessions.md` section 6).
    partial: bool,
}

#[derive(Clone, Debug)]
struct RawLine {
    seq: u64,
    bytes: RawBytes,
}

#[derive(Clone, Debug)]
enum RawBytes {
    /// A line read into the record of its sequence, at `start..end` of the
    /// file, newline excluded, whose values in blobs that record holds.
    InFile { start: u64, end: u64 },
    /// A line no record reproduces.
    Kept(Arc<[u8]>),
}

impl std::fmt::Debug for RawLines {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kept = self
            .lines
            .iter()
            .filter(|line| matches!(line.bytes, RawBytes::Kept(_)))
            .count();
        formatter
            .debug_struct("RawLines")
            .field("file", &self.file)
            .field("lines", &self.lines.len())
            .field("kept", &kept)
            .field("partial", &self.partial)
            .finish()
    }
}

/// The lines a load read, with the buffer they were read into, until the
/// log's records are validated and [`RawLines::from_loaded`] keeps what it
/// has to.
struct LoadedRaw {
    buffer: Vec<u8>,
    lines: Vec<LoadedLine>,
    /// The buffer is not the file's bytes: an older history format was
    /// converted in memory.
    converted: bool,
}

struct LoadedLine {
    seq: u64,
    start: usize,
    end: usize,
    /// The line decoded into its record as it is, with no repair.
    strict: bool,
}

/// The record `line`, which names values in the blobs of `blob_dir`, reads as
/// with them read back, as the loader reads it, typed as it is. `None` when one
/// is missing, or the loader would repair or skip the line.
fn read_restored(line: &[u8], blob_dir: &Path) -> Option<StoredEvent> {
    let mut value = serde_json::from_slice::<Value>(line).ok()?;
    let payload = value.get_mut("payload")?;
    blobs::internalize(payload, &mut |digest| blobs::read(blob_dir, digest)).ok()?;
    StoredEvent::decode_strict(&serde_json::to_vec(&value).ok()?)
}

impl RawLines {
    /// The lines of a load from `path`: a line that decoded as it is into a
    /// record the validation accepted stays in the file, every other line is
    /// kept.
    fn from_loaded(loaded: LoadedRaw, records: &[Arc<StoredEvent>], path: &Path) -> Self {
        let has_record = |seq: u64| {
            records
                .binary_search_by_key(&seq, |record| record.seq)
                .is_ok()
        };
        let lines = loaded
            .lines
            .into_iter()
            .map(|line| RawLine {
                seq: line.seq,
                bytes: if line.strict && !loaded.converted && has_record(line.seq) {
                    RawBytes::InFile {
                        start: line.start as u64,
                        end: line.end as u64,
                    }
                } else {
                    RawBytes::Kept(Arc::from(&loaded.buffer[line.start..line.end]))
                },
            })
            .collect::<Vec<_>>();
        let in_file = lines
            .iter()
            .any(|line| matches!(line.bytes, RawBytes::InFile { .. }));
        Self {
            file: in_file.then(|| Arc::from(path)),
            lines,
            partial: false,
        }
    }

    /// Lets go of the lines the file holds as their records: what a log holding
    /// only its window keeps of them.
    fn keep_only_kept(&mut self) {
        if !self.partial {
            self.lines
                .retain(|line| matches!(line.bytes, RawBytes::Kept(_)));
            self.partial = true;
        }
    }

    /// The lines of the log `path` through `through_seq`, from the file, with
    /// those of this, which kept only what no record reproduces, in their
    /// places: what a fork copies.
    fn read_whole(&self, path: &Path, through_seq: u64) -> Result<Self, EventLogError> {
        if !self.partial {
            return Ok(self.through(through_seq));
        }
        let length = fs::metadata(path)
            .map_err(|source| EventLogError::Io {
                path: path.to_owned(),
                source,
            })?
            .len();
        let starts = scan_line_starts(path, 0, length)?;
        let mut kept = self.lines.iter().peekable();
        let mut lines = Vec::new();
        for (index, (seq, start)) in starts.iter().enumerate() {
            if *seq > through_seq {
                break;
            }
            let bytes = match kept.next_if(|line| line.seq == *seq) {
                Some(line) => line.bytes.clone(),
                None => RawBytes::InFile {
                    start: *start,
                    end: starts
                        .get(index + 1)
                        .map_or(length, |(_, next)| *next)
                        .saturating_sub(1),
                },
            };
            lines.push(RawLine { seq: *seq, bytes });
        }
        Ok(Self {
            file: Some(Arc::from(path)),
            lines,
            partial: false,
        })
    }

    fn get(&self, seq: u64) -> Option<&RawLine> {
        self.lines
            .binary_search_by_key(&seq, |line| line.seq)
            .ok()
            .map(|index| &self.lines[index])
    }

    /// The lines through `through_seq`.
    fn through(&self, through_seq: u64) -> Self {
        let kept = self.lines.partition_point(|line| line.seq <= through_seq);
        Self {
            file: self.file.clone(),
            lines: self.lines[..kept].to_vec(),
            partial: self.partial,
        }
    }

    fn truncate(&mut self, through_seq: u64) {
        let kept = self.lines.partition_point(|line| line.seq <= through_seq);
        self.lines.truncate(kept);
    }

    /// Drops the lines of `seqs`, which ascend.
    fn remove(&mut self, seqs: &[u64]) {
        if !seqs.is_empty() {
            self.lines
                .retain(|line| seqs.binary_search(&line.seq).is_err());
        }
    }

    /// Points the lines left in a file at `file`, the log written whole
    /// with the layout `layout`, which holds a line for every one of them.
    fn rebase(&mut self, file: &Path, layout: &LineIndex) {
        let mut in_file = false;
        for line in &mut self.lines {
            let RawBytes::InFile { start, end } = &mut line.bytes else {
                continue;
            };
            let Ok(index) = layout
                .starts
                .binary_search_by_key(&line.seq, |(seq, _)| *seq)
            else {
                continue;
            };
            let next = layout
                .starts
                .get(index + 1)
                .map_or(layout.len, |(_, start)| *start);
            *start = layout.starts[index].1;
            *end = next.saturating_sub(1);
            in_file = true;
        }
        self.file = in_file.then(|| Arc::from(file));
    }

    /// The lines with a sequence in `seqs`, with the stretch of the file that
    /// holds those of them left there. A file that cannot be read leaves
    /// them without bytes ([`ReadRawLines::bytes`]).
    fn read(&self, seqs: &impl std::ops::RangeBounds<u64>) -> ReadRawLines<'_> {
        let (from, to) = self
            .lines
            .iter()
            .filter(|line| seqs.contains(&line.seq))
            .filter_map(|line| match line.bytes {
                RawBytes::InFile { start, end } => Some((start, end)),
                RawBytes::Kept(_) => None,
            })
            .fold((u64::MAX, 0), |(from, to), (start, end)| {
                (from.min(start), to.max(end))
            });
        let window = self
            .file
            .as_deref()
            .filter(|_| from <= to)
            .and_then(|file| FileWindow::read(file, from.saturating_sub(1), to + 1).ok());
        ReadRawLines { raw: self, window }
    }
}

/// A stretch of a file, from `offset`.
struct FileWindow {
    offset: u64,
    bytes: Vec<u8>,
}

impl FileWindow {
    /// Bytes `from..to` of `file`, or fewer when it is shorter.
    fn read(file: &Path, from: u64, to: u64) -> io::Result<Self> {
        use std::io::{Read as _, Seek as _};
        let mut opened = fs::File::open(file)?;
        opened.seek(io::SeekFrom::Start(from))?;
        let mut bytes = Vec::new();
        opened.take(to - from).read_to_end(&mut bytes)?;
        Ok(Self {
            offset: from,
            bytes,
        })
    }

    /// The line at `start..end`, when the file still has a whole line there.
    fn line(&self, start: u64, end: u64) -> Option<&[u8]> {
        let at = |offset: u64| {
            offset
                .checked_sub(self.offset)
                .and_then(|index| usize::try_from(index).ok())
        };
        let (first, last) = (at(start)?, at(end)?);
        let line = self.bytes.get(first..last)?;
        let opens = start == 0
            || first.checked_sub(1).and_then(|index| self.bytes.get(index)) == Some(&b'\n');
        (opens && self.bytes.get(last) == Some(&b'\n')).then_some(line)
    }
}

/// [`RawLines`] in a range of sequences, with the file read for them.
struct ReadRawLines<'a> {
    raw: &'a RawLines,
    window: Option<FileWindow>,
}

impl ReadRawLines<'_> {
    /// The bytes of `line`: those kept, or those the file holds, when they
    /// still decode into `record`, the line's record (a fork's under another
    /// session ID). `None` when the file no longer holds the line, so the
    /// record is written as it serializes.
    fn bytes<'b>(&'b self, line: &'b RawLine, record: Option<&StoredEvent>) -> Option<&'b [u8]> {
        match &line.bytes {
            RawBytes::Kept(bytes) => Some(bytes),
            RawBytes::InFile { start, end } => {
                let bytes = self.window.as_ref()?.line(*start, *end)?;
                let record = record?;
                let mut read = if blobs::has_references(bytes) {
                    let file = self.raw.file.as_deref()?;
                    read_restored(bytes, &blobs::tree_blob_dir(file))?
                } else {
                    StoredEvent::decode_strict(bytes)?
                };
                read.session_id = record.session_id;
                (read == *record).then_some(bytes)
            }
        }
    }
}

/// What a revert of a windowed log keeps, folded from the file before it is cut.
struct FoldedPrefix {
    index: EventIndex,
    physical_model_turns: u64,
    validation: ValidationState,
    view_start: Option<Box<CacheStartFold>>,
    window: Option<u64>,
    context: bool,
    held: Vec<Arc<StoredEvent>>,
}

/// What a fork copies of its source: [`EventLog::read_fork_prefix`].
pub struct ForkPrefix {
    session_id: SessionId,
    storage: EventStorage,
    taint: ValidationTaint,
    validation: ValidationState,
    next_seq: u64,
}

/// Whether a fork leaves the record behind: the delegation records name the
/// session that delegated, and the subagents they reserve, start, and finish
/// stay that session's.
fn left_behind_by_fork(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::DelegationReserved { .. }
            | EventPayload::DelegationProgressed { .. }
            | EventPayload::DelegationFinished { .. }
            | EventPayload::DelegatedAttachmentsAdded { .. }
    )
}

fn is_model_turn(event: &StoredEvent) -> bool {
    matches!(event.payload, EventPayload::ModelTurnCommitted { .. })
}

impl EventStorage {
    fn new(all: Vec<Arc<StoredEvent>>) -> Self {
        let physical_model_turns = all.iter().filter(|event| is_model_turn(event)).count() as u64;
        let mut storage = Self {
            creation: all.first().cloned(),
            all: Arc::new(all),
            window: None,
            context: false,
            view_start: None,
            refused: Arc::default(),
            index: EventIndex::default(),
            physical_model_turns,
            lines: None,
            raw: Arc::default(),
        };
        storage.rebuild_index();
        storage
    }

    fn with_lines(all: Vec<Arc<StoredEvent>>, lines: LineIndex, raw: RawLines) -> Self {
        let mut storage = Self::new(all);
        storage.lines = Some(lines);
        storage.raw = Arc::new(raw);
        storage
    }

    fn push(&mut self, event: impl Into<Arc<StoredEvent>>) {
        let event = event.into();
        self.physical_model_turns += u64::from(is_model_turn(&event));
        self.index.observe(&event);
        if let Some(view_start) = &mut self.view_start {
            view_start.observe(&event);
        }
        self.creation.get_or_insert_with(|| Arc::clone(&event));
        Arc::make_mut(&mut self.all).push(event);
    }

    /// Lets go of what precedes the window's start, the latest committed turn of
    /// an ACP agent, when there is one and the log's file holds what it lets go
    /// of. The first time, the log still holds what a view cache needs.
    fn narrow(&mut self) {
        let Some(start) = self.index.window_start else {
            return;
        };
        if self.lines.is_none()
            || (!self.context && self.window.is_some_and(|window| window >= start))
        {
            return;
        }
        if self.window.is_none() {
            self.view_start = Some(Box::new(CacheStartFold::of(&self.all, VIEW_BUDGET)));
        }
        let kept = self.all.partition_point(|event| event.seq < start);
        self.all = Arc::new(self.all[kept..].to_vec());
        self.window = Some(start);
        self.context = false;
        self.let_go_of_lines();
    }

    /// Lets go of what a native model's context no longer needs, when the log
    /// has a checkpoint and its file holds what it lets go of: the window then
    /// starts where the checkpoint's context does, and holds before it only the
    /// records [`model_context::model_context`] pins
    /// (`docs/site/specs/forever-sessions.md` section 5). It applies at any
    /// moment, so a log narrows so as a checkpoint commits and as it opens.
    ///
    /// A window an ACP agent's turn moved holds too little to choose from: the
    /// log reads itself whole for a native model first
    /// ([`EventLog::model_context_and_state`]). So does one whose context would
    /// now start before what it holds, which only a provider window alone, as
    /// builds before 1.29.0 wrote, can make it do.
    fn narrow_to_context(&mut self) {
        if self.index.window_start.is_some()
            || self.lines.is_none()
            || (self.window.is_some() && !self.context)
        {
            return;
        }
        let Some(start) = model_context::context_window_start(&self.all) else {
            return;
        };
        if self.window.is_some_and(|window| start < window) {
            self.context = false;
            return;
        }
        let Some(selected) = model_context::model_context(&self.all, &self.index.state) else {
            return;
        };
        if self.window.is_none() {
            self.view_start = Some(Box::new(CacheStartFold::of(&self.all, VIEW_BUDGET)));
        }
        self.all = Arc::new(selected);
        self.window = Some(start);
        self.context = true;
        self.let_go_of_lines();
    }

    /// Whether a native model's context must be read whole first: what an ACP
    /// agent's window let go of may be in it.
    fn reads_whole_for_its_model(&self) -> bool {
        self.window.is_some() && !self.context && self.index.window_start.is_none()
    }

    /// Test-only: whether the log's agent is a native model that reads what it
    /// holds without reading it whole first, so its validation keeps what that
    /// model's context names, as a whole read's does.
    #[cfg(any(test, feature = "test-support"))]
    fn validates_for_a_native_model(&self) -> bool {
        self.index.window_start.is_none() && !self.reads_whole_for_its_model()
    }

    /// Narrows as `record`, just pushed, moves the window: an ACP agent's
    /// committed turn, a checkpoint of a native model's, or an epoch of an
    /// approval reviewer's own log.
    fn narrow_after(&mut self, record: &StoredEvent) {
        match &record.payload {
            EventPayload::ModelTurnCommitted { .. } => self.narrow(),
            EventPayload::ContextCheckpointCommitted { .. } => self.narrow_to_context(),
            EventPayload::ReviewerTurnRecorded { turn } if turn.epoch.is_some() => {
                self.narrow_to_context();
            }
            _ => {}
        }
    }

    /// Where the window's records start in [`Self::all`], after what is pinned
    /// before it.
    fn window_position(&self) -> usize {
        self.window.map_or(0, |window| {
            self.all.partition_point(|event| event.seq < window)
        })
    }

    /// Where the line index must hold every line from: the window's start, or an
    /// earlier one a view cache starts at. `None` for a log that holds its records.
    fn lines_held_from(&self) -> Option<u64> {
        let window = self.window?;
        Some(
            self.view_start
                .as_ref()
                .and_then(|view_start| view_start.start())
                .map_or(window, |start| start.seq.min(window)),
        )
    }

    /// Whether the line index let go of lines of what the log holds: a log that
    /// read itself whole for a native model after an ACP agent's window, whose
    /// context starts before that window did. A view cache's start that comes to
    /// lie before them is not: a cache is then not written, as before.
    fn lines_fall_short(&self) -> bool {
        let Some(first) = self.lines.as_ref().and_then(LineIndex::held_from) else {
            return false;
        };
        self.window.is_none_or(|window| window < first)
    }

    /// Lets go of where the lines before what the log holds start, but for where a
    /// view cache can start, and of the lines the file holds as their records.
    fn let_go_of_lines(&mut self) {
        let Some(held_from) = self.lines_held_from() else {
            return;
        };
        if let Some(lines) = &mut self.lines {
            lines.let_go_before(held_from);
        }
        Arc::make_mut(&mut self.raw).keep_only_kept();
    }

    /// Drops every physical event after `through_seq` from what it holds of the
    /// file's lines.
    /// After a cut to `length` of a log whose index let go of lines, the index read
    /// from the file again, holding what the window it went back to holds: a cut
    /// cannot tell which lines the window would have let go of by then.
    fn read_lines_again(
        &mut self,
        path: &Path,
        length: Option<u64>,
        let_go: bool,
    ) -> Result<(), EventLogError> {
        let (Some(_), Some(length), true) = (&self.lines, length, let_go) else {
            return Ok(());
        };
        let held_from = self.lines_held_from();
        self.lines = Some(LineIndex::scanned(path, length, held_from)?);
        Ok(())
    }

    fn truncate_lines(&mut self, through_seq: u64, length: Option<u64>) {
        if let Some(lines) = &mut self.lines {
            match length {
                Some(length) => lines.truncate(through_seq, length),
                None => self.lines = None,
            }
        }
        Arc::make_mut(&mut self.raw).truncate(through_seq);
        Arc::make_mut(&mut self.refused).retain(|seq| *seq <= through_seq);
    }

    /// Drops every physical event after `through_seq`.
    fn truncate(&mut self, through_seq: u64, length: Option<u64>) {
        self.truncate_lines(through_seq, length);
        Arc::make_mut(&mut self.all).retain(|event| event.seq <= through_seq);
        self.physical_model_turns =
            self.all.iter().filter(|event| is_model_turn(event)).count() as u64;
        self.rebuild_index();
    }

    fn rebuild_index(&mut self) {
        self.index = EventIndex::default();
        for event in self.all.iter() {
            self.index.observe(event);
        }
    }

    fn snapshot(&self) -> EventSnapshot {
        self.all.clone()
    }

    /// The records from the window's start, without what is pinned before it.
    fn recent(&self) -> EventSnapshot {
        match self.window_position() {
            0 => self.snapshot(),
            pinned => Arc::new(self.all[pinned..].to_vec()),
        }
    }
}

/// Borrows each event of a slice of owned or shared events.
pub fn event_refs<E: Borrow<StoredEvent>>(events: &[E]) -> Vec<&StoredEvent> {
    events.iter().map(Borrow::borrow).collect()
}

fn usage_total(seq: u64, usage: &cookie_agent_protocol::Usage) -> Option<(u64, u64)> {
    let input = usage.input_tokens;
    let output = usage.output_tokens;
    (input.is_some() || output.is_some()).then(|| {
        (
            seq,
            input
                .unwrap_or_default()
                .saturating_add(output.unwrap_or_default()),
        )
    })
}

impl EventLogWriter {
    fn open(path: &Path, durability: &Arc<durability::Durability>) -> Result<Self, EventLogError> {
        let created = !path.exists();
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;

            const FILE_SHARE_READ: u32 = 0x1;
            const FILE_SHARE_WRITE: u32 = 0x2;
            const FILE_SHARE_DELETE: u32 = 0x4;
            options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        }
        let io_error = |source| EventLogError::Io {
            path: path.to_owned(),
            source,
        };
        let file = options.open(path).map_err(io_error)?;
        durability.attach(&file, created).map_err(io_error)?;
        Ok(Self {
            path: path.to_owned(),
            file,
            durability: Arc::clone(durability),
        })
    }

    /// Writes one newline-terminated record, unsynced. Returns the record's
    /// [`Commit`] when `commit` asks for a sync or the file is new, whose
    /// directory entry the sync makes durable too.
    fn append(&mut self, record: &[u8], commit: bool) -> Result<Option<Commit>, EventLogError> {
        self.file
            .write_all(record)
            .map_err(|source| self.io_error(source))?;
        let (written, new_file) = self.durability.written();
        Ok((commit || new_file).then_some(written))
    }

    fn io_error(&self, source: io::Error) -> EventLogError {
        EventLogError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

impl Drop for EventLogWriter {
    /// Syncs what no sync covered yet: records that ride along with the next
    /// commit point, and a commit point whose sync has not run yet.
    fn drop(&mut self) {
        self.durability.close(&self.file);
    }
}

/// Whether a record is a commit point, synced before it takes effect: before
/// its append returns and before it is published
/// (`docs/site/specs/store-locks-and-durability.md`, section 3). What
/// something outside the log acts on is one: a tool call's start (before
/// the tool runs) and end (before the next model request), what a request's
/// reply confirms, grants and approval decisions, delegation records,
/// session creation, a run's end, and every rare kind, whose sync costs
/// nothing. The rest rides along: written at once, synced by the next commit
/// point or when the log closes, so a crash can drop only such records after
/// the last commit point. Live-only kinds are never appended.
///
/// No catch-all: a new kind does not build until it is put in a class.
pub fn syncs_on_append(payload: &EventPayload) -> bool {
    match payload {
        // Per-attempt bookkeeping, the committed turn (its tool calls'
        // starts sync before they run), and the ACP agent's stored state.
        EventPayload::ModelAttemptStarted { .. }
        | EventPayload::ModelToolsPublished { .. }
        | EventPayload::ModelRequestPrepared { .. }
        | EventPayload::ModelReplayEvaluated { .. }
        | EventPayload::ModelOutputStarted { .. }
        | EventPayload::ModelUsageRecorded { .. }
        | EventPayload::ModelTurnCommitted { .. }
        | EventPayload::AcpSessionUpdated { .. } => false,
        // Live-only output: rejected before it is written.
        EventPayload::TextDelta { .. }
        | EventPayload::ReasoningDelta { .. }
        | EventPayload::InternalAgentTextDelta { .. }
        | EventPayload::NativeCompactionStarted { .. }
        | EventPayload::NativeCompactionFinished { .. }
        | EventPayload::ToolCallProgress { .. }
        | EventPayload::AcpLiveUpdate { .. } => false,
        // The approval reviewer's own log: one per review, and a lost one
        // only costs the next review a longer delta.
        EventPayload::ReviewerTurnRecorded { .. } => true,
        // Tool calls.
        EventPayload::ToolCallStarted { .. }
        | EventPayload::ToolCallTerminated { .. }
        | EventPayload::ToolOutputElided { .. }
        | EventPayload::ToolCallLinked { .. }
        | EventPayload::ToolRoundStarted { .. } => true,
        // Input and what a request's reply confirms.
        EventPayload::UserInputAdmitted { .. }
        | EventPayload::UserInputSubmitted { .. }
        | EventPayload::UserInputTransformed { .. }
        | EventPayload::UserInputRecalled { .. }
        | EventPayload::UserInputApplied { .. }
        | EventPayload::UserInputQueued { .. }
        | EventPayload::UserInputQueueRecalled { .. }
        | EventPayload::UserInputQueueVoided { .. }
        | EventPayload::MessageInjected { .. }
        | EventPayload::SessionModelSelected { .. }
        | EventPayload::SessionTitleCommitted { .. }
        | EventPayload::SessionProjectSet { .. }
        | EventPayload::SessionPermissionOverlaySet { .. }
        | EventPayload::SessionToolExposureSet { .. }
        | EventPayload::PermissionModeSet { .. }
        | EventPayload::RuntimeAttached { .. }
        | EventPayload::RuntimeDetached { .. }
        | EventPayload::ElicitationRequested { .. }
        | EventPayload::ElicitationResolved { .. }
        // An attachment's outcome, which the user may answer.
        | EventPayload::AssistantAttachment { .. } => true,
        // Grants and approval decisions.
        EventPayload::ApprovalRequested { .. }
        | EventPayload::ApprovalStep { .. }
        | EventPayload::ApprovalFinalized { .. }
        | EventPayload::TreeApprovalGrantCommitted { .. } => true,
        // Delegation.
        EventPayload::DelegationReserved { .. }
        | EventPayload::DelegationProgressed { .. }
        | EventPayload::DelegationFinished { .. }
        | EventPayload::DelegateChildTerminated { .. }
        | EventPayload::DelegatedContextSeeded { .. }
        | EventPayload::DelegatedAttachmentsAdded { .. } => true,
        // Goals and producers.
        EventPayload::GoalActivated { .. }
        | EventPayload::GoalChecklistRevised { .. }
        | EventPayload::GoalLifecycleChanged { .. }
        | EventPayload::ProducerMessageAccepted { .. }
        | EventPayload::ProducerMessageAdmitted { .. }
        | EventPayload::ProducerMessagesClaimed { .. }
        | EventPayload::ProducerMessagesReleased { .. }
        | EventPayload::ProducerMessageConsumed { .. }
        | EventPayload::ProducerMessageDiscarded { .. }
        | EventPayload::AutomaticRunsHeld { .. }
        | EventPayload::AutomaticRunsResumed { .. } => true,
        // The session and its runs.
        EventPayload::SessionCreated { .. }
        | EventPayload::RunStarted { .. }
        | EventPayload::RunFinished { .. }
        | EventPayload::RecordOmitted { .. }
        | EventPayload::AcpSessionStarted { .. } => true,
        // Once per run or rarer: prompt, memory, skills, checkpoints,
        // internal agents, fallbacks, abandoned attempts, diagnostics.
        EventPayload::SkillLoaded { .. }
        | EventPayload::SkillInvocationNoted { .. }
        | EventPayload::AgentMdLoaded { .. }
        | EventPayload::MemoryLoaded { .. }
        | EventPayload::MemoryChanged { .. }
        | EventPayload::Diagnostic { .. }
        | EventPayload::DeferredToolsAnnounced { .. }
        | EventPayload::ToolsAvailabilityAnnounced { .. }
        | EventPayload::PromptBlocksRecorded { .. }
        | EventPayload::PromptBlocksAnnounced { .. }
        | EventPayload::SystemPromptComposed { .. }
        | EventPayload::ContextRefreshed { .. }
        | EventPayload::AttemptAbandoned { .. }
        | EventPayload::NativeReplayCut { .. }
        | EventPayload::ModelFallback { .. }
        | EventPayload::InternalAgentStarted { .. }
        | EventPayload::InternalAgentUsageRecorded { .. }
        | EventPayload::InternalAgentFinished { .. }
        | EventPayload::InternalAgentFallback { .. }
        | EventPayload::ContextCheckpointCommitted { .. } => true,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct EventLoadDiagnostic {
    pub seq: u64,
    pub reason: String,
    pub engine_version: Option<String>,
    pub skipped: bool,
}

impl EventLog {
    #[cfg(any(test, feature = "test-support"))]
    pub fn create(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
    ) -> Result<Arc<Self>, EventLogError> {
        let authority = WriteAuthority::new();
        let capability = authority.capability();
        Self::create_owned(
            path,
            session_id,
            origin,
            creation,
            Vec::new(),
            capability,
            true,
            Some(authority),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_owned(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
        initial: Vec<EventPayload>,
        capability: WriteCapability,
        persisted: bool,
        _test_authority: Option<WriteAuthority>,
    ) -> Result<Arc<Self>, EventLogError> {
        let mut storage = EventStorage::new(Vec::new());
        if persisted {
            storage.lines = Some(LineIndex::default());
        }
        let durability = Arc::new(durability::Durability::new(&path));
        let log = Arc::new(Self {
            path,
            session_id,
            append: Mutex::new(()),
            events: Mutex::new(storage),
            diagnostics: Vec::new(),
            initial_validation_taint: Mutex::default(),
            validation: Mutex::new(ValidationState::default()),
            next_seq: AtomicU64::new(1),
            persisted: AtomicBool::new(persisted),
            read_only: false,
            torn_tail_ignored: false,
            converted: false,
            render_only: None,
            write_capability: Some(capability),
            #[cfg(any(test, feature = "test-support"))]
            _test_authority,
            #[cfg(any(test, feature = "test-support"))]
            append_authorization_hook: Mutex::new(None),
            writer: Mutex::new(None),
            durability,
            cuts: Arc::default(),
            opened: Mutex::default(),
            state_written: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-support"))]
            history_calls: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            model_context_shadow: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            opened_from_state: false,
        });
        if !matches!(creation, EventPayload::SessionCreated { .. }) {
            return Err(EventLogError::MissingCreation(log.path.clone()));
        }
        let mut commit = log.append_inner(None, None, origin.clone(), creation)?.1;
        for event in initial {
            commit = log
                .append_inner(None, None, origin.clone(), event)?
                .1
                .or(commit);
        }
        if let Some(commit) = commit {
            log.sync_through(commit)?;
        }
        Ok(log)
    }

    /// Creates a log holding `creation` followed by `records`, written to its
    /// file at once and synced: a log no session's publication waits for,
    /// as the approval reviewer's own is.
    pub fn create_persisted_owned(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
        records: Vec<EventPayload>,
        capability: WriteCapability,
    ) -> Result<Arc<Self>, EventLogError> {
        Self::create_owned(
            path, session_id, origin, creation, records, capability, true, None,
        )
    }

    /// The session whose log this is.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn create_buffered(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
    ) -> Result<Arc<Self>, EventLogError> {
        let authority = WriteAuthority::new();
        let capability = authority.capability();
        Self::create_owned(
            path,
            session_id,
            origin,
            creation,
            Vec::new(),
            capability,
            false,
            Some(authority),
        )
    }

    /// Creates a buffered log holding `creation` followed by `initial`, all
    /// published together with the session's first persistence.
    pub fn create_buffered_owned(
        path: PathBuf,
        session_id: SessionId,
        origin: EventOrigin,
        creation: EventPayload,
        initial: Vec<EventPayload>,
        capability: WriteCapability,
    ) -> Result<Arc<Self>, EventLogError> {
        Self::create_owned(
            path, session_id, origin, creation, initial, capability, false, None,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn open(path: PathBuf, session_id: SessionId) -> Result<Arc<Self>, EventLogError> {
        let authority = WriteAuthority::new();
        let capability = authority.capability();
        Self::open_with_mode(path, session_id, false, Some(capability), Some(authority))
    }

    pub fn open_owned(
        path: PathBuf,
        session_id: SessionId,
        capability: WriteCapability,
    ) -> Result<Arc<Self>, EventLogError> {
        Self::open_with_mode(path, session_id, false, Some(capability), None)
    }

    pub fn open_read_only(
        path: PathBuf,
        session_id: SessionId,
    ) -> Result<Arc<Self>, EventLogError> {
        Self::open_with_mode(path, session_id, true, None, None)
    }

    fn open_with_mode(
        path: PathBuf,
        session_id: SessionId,
        read_only: bool,
        write_capability: Option<WriteCapability>,
        _test_authority: Option<WriteAuthority>,
    ) -> Result<Arc<Self>, EventLogError> {
        let torn_tail = if read_only {
            TornTail::Ignore
        } else {
            TornTail::Truncate
        };
        let opened = crate::blocking::off_worker(|| {
            cookie_agent_protocol::read_stored_history(|| {
                state_file::open_from_state(&path, session_id, torn_tail)
                    .or_else(|| state_file::fold_file(&path, session_id, torn_tail))
            })
        });
        let Some(opened) = opened else {
            let log = Self::open_parsed(
                path,
                session_id,
                read_only,
                write_capability,
                _test_authority,
            )?;
            if !read_only {
                log.settle_state_file();
            }
            return Ok(log);
        };
        let state_file::OpenedLog {
            storage,
            validation,
            initial_taint,
            diagnostics,
            next_seq,
            torn_tail,
            projection,
            state_covers,
        } = opened;
        let durability = Arc::new(durability::Durability::new(&path));
        let log = Arc::new(Self {
            path,
            session_id,
            append: Mutex::new(()),
            events: Mutex::new(storage),
            diagnostics,
            initial_validation_taint: Mutex::new(initial_taint),
            validation: Mutex::new(validation),
            next_seq: AtomicU64::new(next_seq),
            persisted: AtomicBool::new(true),
            read_only,
            torn_tail_ignored: torn_tail && read_only,
            converted: false,
            render_only: None,
            write_capability,
            #[cfg(any(test, feature = "test-support"))]
            _test_authority,
            #[cfg(any(test, feature = "test-support"))]
            append_authorization_hook: Mutex::new(None),
            writer: Mutex::new(None),
            durability,
            cuts: Arc::default(),
            opened: Mutex::new(projection.map(|folds| (next_seq - 1, folds))),
            state_written: AtomicU64::new(state_covers.unwrap_or(0)),
            #[cfg(any(test, feature = "test-support"))]
            history_calls: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            model_context_shadow: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            opened_from_state: state_covers.is_some(),
        });
        #[cfg(any(test, feature = "test-support"))]
        log.assert_opened_as_parsed();
        if !read_only {
            log.settle_state_file();
        }
        Ok(log)
    }

    /// Opens the log by reading it whole, as a damaged one or one of an older
    /// history format has to be.
    fn open_parsed(
        path: PathBuf,
        session_id: SessionId,
        read_only: bool,
        write_capability: Option<WriteCapability>,
        _test_authority: Option<WriteAuthority>,
    ) -> Result<Arc<Self>, EventLogError> {
        let loaded = load_event_jsonl(
            &path,
            if read_only {
                TornTail::Ignore
            } else {
                TornTail::Truncate
            },
        )?;
        if loaded.converted && !read_only {
            // An owned open comes after its tree was converted on disk under
            // the tree's lock (`SessionStore::hold_tree`).
            return corrupt(
                &path,
                "an older history format is converted when its session tree is taken",
            );
        }
        let records = loaded.records.into_iter().map(Arc::new).collect::<Vec<_>>();
        if !matches!(
            records.first().map(|record| &record.payload),
            Some(EventPayload::SessionCreated { .. })
        ) {
            return Err(EventLogError::MissingCreation(path));
        }
        let mut diagnostics = loaded.diagnostics;
        let mut taint = loaded.validation_taint;
        let read = records.iter().map(|record| record.seq).collect::<Vec<_>>();
        let (records, validation) =
            validate_loaded(&path, session_id, records, &mut taint, &mut diagnostics)?;
        // The read's buffer goes here: only the lines no record reproduces
        // stay in memory.
        let raw = RawLines::from_loaded(loaded.raw, &records, &path);
        let durability = Arc::new(durability::Durability::new(&path));
        let mut storage = EventStorage::with_lines(records, loaded.lines, raw);
        if read.len() != storage.all.len() {
            storage.refused = Arc::new(
                read.into_iter()
                    .filter(|seq| {
                        storage
                            .all
                            .binary_search_by_key(seq, |record| record.seq)
                            .is_err()
                    })
                    .collect(),
            );
        }
        Ok(Arc::new(Self {
            path,
            session_id,
            append: Mutex::new(()),
            events: Mutex::new(storage),
            diagnostics,
            initial_validation_taint: Mutex::new(taint),
            validation: Mutex::new(validation),
            next_seq: AtomicU64::new(loaded.next_seq),
            persisted: AtomicBool::new(true),
            read_only,
            torn_tail_ignored: loaded.torn_tail,
            converted: loaded.converted,
            render_only: loaded.render_only,
            write_capability,
            #[cfg(any(test, feature = "test-support"))]
            _test_authority,
            #[cfg(any(test, feature = "test-support"))]
            append_authorization_hook: Mutex::new(None),
            writer: Mutex::new(None),
            durability,
            cuts: Arc::default(),
            opened: Mutex::default(),
            state_written: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-support"))]
            history_calls: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            model_context_shadow: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            opened_from_state: false,
        }))
    }

    /// What a log's records folded to as it opened, while that is still of its
    /// tip: an owned log's first projection takes it, and a read-only log, which
    /// the parsed-log cache hands out again, keeps it.
    pub(crate) fn opened_folds(&self) -> Option<crate::session::ProjectionFolds> {
        let mut opened = self
            .opened
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tip = self.physical_tip_seq();
        if opened.as_ref().is_none_or(|(at, _)| *at != tip) {
            *opened = None;
            return None;
        }
        if self.read_only {
            return opened.as_ref().map(|(_, folds)| folds.clone());
        }
        opened.take().map(|(_, folds)| folds)
    }

    /// What a fork of this log through `through_seq` under `session_id` copies,
    /// assembled and validated in memory. It reads the log, so it runs before
    /// any store lock, and [`Self::fork_buffered`] takes the result. The damage
    /// the load found in that prefix comes along, so records it excused stay
    /// excused.
    ///
    /// The fork leaves its source's subagents behind: each delegation record
    /// becomes a `record_omitted` at its sequence, run, and time, so the
    /// sequence stays contiguous. The conversation stays as it was: the
    /// delegate calls and their results, the links to the children, and
    /// the producer messages that brought their reports.
    pub fn read_fork_prefix(
        &self,
        session_id: SessionId,
        through_seq: u64,
    ) -> Result<ForkPrefix, EventLogError> {
        let mut omitted = Vec::new();
        let records = self
            .history_range(..=through_seq)?
            .map(|event| {
                let event = event?;
                let mut copied = if left_behind_by_fork(&event.payload) {
                    omitted.push(event.seq);
                    StoredEvent {
                        engine_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                        origin: event.origin.clone(),
                        session_id,
                        run_id: event.run_id,
                        seq: event.seq,
                        timestamp: event.timestamp,
                        payload: EventPayload::RecordOmitted {
                            omitted_type: cookie_agent_protocol::EventType::new(
                                cookie_agent_protocol::OpenTagged::tag(&event.payload),
                            )
                            .expect("event types are snake-case labels"),
                        },
                    }
                } else {
                    event.as_ref().clone()
                };
                copied.session_id = session_id;
                Ok(Arc::new(copied))
            })
            .collect::<Result<Vec<_>, EventLogError>>()?;
        if !matches!(
            records.first().map(|record| &record.payload),
            Some(EventPayload::SessionCreated { .. })
        ) {
            return Err(EventLogError::MissingCreation(self.path.clone()));
        }
        let mut taint = self.initial_taint();
        taint.truncate(through_seq);
        // A prefix of a log that validated, so its records hold no
        // duplicates and pass again under the same damage.
        let validation = rebuild_validation(&self.path, session_id, &records, &taint)?;
        // The lines the source read come along as bytes, except those of
        // the records left behind, which are written as their replacements.
        let held = Arc::clone(&self.storage().raw);
        let mut raw = held.read_whole(&self.path, through_seq)?;
        raw.remove(&omitted);
        let next_seq = records
            .last()
            .map(|record| record.seq)
            .max(raw.lines.last().map(|line| line.seq))
            .map_or(1, |seq| seq + 1);
        let mut storage = EventStorage::new(records);
        storage.raw = Arc::new(raw);
        Ok(ForkPrefix {
            session_id,
            storage,
            taint,
            validation,
            next_seq,
        })
    }

    /// A buffered log holding what [`Self::read_fork_prefix`] read, for a fork
    /// whose file is written whole once it is assembled. It writes nothing until
    /// [`Self::mark_written`].
    pub fn fork_buffered(
        path: PathBuf,
        prefix: ForkPrefix,
        capability: WriteCapability,
    ) -> Arc<Self> {
        let ForkPrefix {
            session_id,
            storage,
            taint,
            validation,
            next_seq,
        } = prefix;
        let durability = Arc::new(durability::Durability::new(&path));
        Arc::new(Self {
            path,
            session_id,
            append: Mutex::new(()),
            events: Mutex::new(storage),
            diagnostics: Vec::new(),
            initial_validation_taint: Mutex::new(taint),
            validation: Mutex::new(validation),
            next_seq: AtomicU64::new(next_seq),
            persisted: AtomicBool::new(false),
            read_only: false,
            torn_tail_ignored: false,
            converted: false,
            render_only: None,
            write_capability: Some(capability),
            #[cfg(any(test, feature = "test-support"))]
            _test_authority: None,
            #[cfg(any(test, feature = "test-support"))]
            append_authorization_hook: Mutex::new(None),
            writer: Mutex::new(None),
            durability,
            cuts: Arc::default(),
            opened: Mutex::default(),
            state_written: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-support"))]
            history_calls: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            model_context_shadow: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            opened_from_state: false,
        })
    }

    /// An owned log over the records this read-only log parsed, without
    /// reading the file again. `None` when the parse ignored a torn final
    /// record, which an owned open has to cut from the file, or converted an
    /// older history format in memory, which the tree's owner converts on
    /// disk. The caller
    /// proves the file is unchanged since the parse.
    pub fn owned_from_read_only(&self, capability: WriteCapability) -> Option<Arc<Self>> {
        if !self.read_only || self.torn_tail_ignored || self.converted {
            return None;
        }
        let storage = self.storage().clone();
        let validation = self
            .validation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let owned = Arc::new(Self {
            path: self.path.clone(),
            session_id: self.session_id,
            append: Mutex::new(()),
            events: Mutex::new(storage),
            diagnostics: self.diagnostics.clone(),
            initial_validation_taint: Mutex::new(self.initial_taint()),
            validation: Mutex::new(validation),
            next_seq: AtomicU64::new(self.next_seq.load(Ordering::Acquire)),
            persisted: AtomicBool::new(true),
            read_only: false,
            torn_tail_ignored: false,
            converted: false,
            render_only: self.render_only.clone(),
            write_capability: Some(capability),
            #[cfg(any(test, feature = "test-support"))]
            _test_authority: None,
            #[cfg(any(test, feature = "test-support"))]
            append_authorization_hook: Mutex::new(None),
            writer: Mutex::new(None),
            durability: Arc::new(durability::Durability::new(&self.path)),
            cuts: Arc::default(),
            opened: Mutex::new(
                self.opened
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            ),
            state_written: AtomicU64::new(self.state_written.load(Ordering::Acquire)),
            #[cfg(any(test, feature = "test-support"))]
            history_calls: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            model_context_shadow: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            opened_from_state: self.opened_from_state,
        });
        owned.settle_state_file();
        Some(owned)
    }

    /// Appends one record and, when it is a commit point, returns after a
    /// sync covers it.
    pub fn append_owned(
        &self,
        capability: &WriteCapability,
        run_id: Option<RunId>,
        origin: EventOrigin,
        payload: EventPayload,
    ) -> Result<Arc<StoredEvent>, EventLogError> {
        let (event, commit) = self.write_owned(capability, run_id, origin, payload)?;
        if let Some(commit) = commit {
            self.sync_through(commit)?;
        }
        Ok(event)
    }

    /// Appends one record without syncing it: it is in the file and in
    /// memory at once. A commit point comes back with its [`Commit`], which
    /// the caller passes to [`Self::sync_through`] before anything acts on
    /// the record, outside any lock it holds.
    pub fn write_owned(
        &self,
        capability: &WriteCapability,
        run_id: Option<RunId>,
        origin: EventOrigin,
        payload: EventPayload,
    ) -> Result<(Arc<StoredEvent>, Option<Commit>), EventLogError> {
        if !self
            .write_capability
            .as_ref()
            .is_some_and(|expected| capability.authorizes(expected))
        {
            return Err(EventLogError::ReadOnly(self.path.clone()));
        }
        #[cfg(any(test, feature = "test-support"))]
        if let Some(hook) = self
            .append_authorization_hook
            .lock()
            .expect("append authorization hook lock poisoned")
            .take()
        {
            let _ = hook.reached.send(());
            let _ = hook.release.recv();
        }
        self.append_inner(Some(capability), run_id, origin, payload)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn append(
        &self,
        run_id: Option<RunId>,
        origin: EventOrigin,
        payload: EventPayload,
    ) -> Result<Arc<StoredEvent>, EventLogError> {
        let capability = self
            .write_capability
            .as_ref()
            .ok_or_else(|| EventLogError::ReadOnly(self.path.clone()))?;
        self.append_owned(capability, run_id, origin, payload)
    }

    fn append_inner(
        &self,
        capability: Option<&WriteCapability>,
        run_id: Option<RunId>,
        origin: EventOrigin,
        payload: EventPayload,
    ) -> Result<(Arc<StoredEvent>, Option<Commit>), EventLogError> {
        if self.read_only {
            return Err(EventLogError::ReadOnly(self.path.clone()));
        }
        self.ensure_appendable()?;
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if capability.is_some_and(|capability| {
            !self
                .write_capability
                .as_ref()
                .is_some_and(|expected| capability.authorizes(expected))
        }) {
            return Err(EventLogError::ReadOnly(self.path.clone()));
        }
        let events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Revisions are recorded again only where they changed; holding the
        // events lock keeps the comparison and the append together.
        let mut payload = payload;
        events.index.revisions.elide_unchanged(&mut payload);
        let event = StoredEvent {
            engine_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            origin: Some(origin),
            session_id: self.session_id,
            run_id,
            seq: self.next_seq.load(Ordering::Acquire),
            timestamp: Timestamp::now(),
            payload,
        };
        event.validate().map_err(|error| EventLogError::Corrupt {
            path: self.path.clone(),
            message: error.to_string(),
        })?;
        // Readers take any format; this build writes its own.
        if matches!(event.payload, EventPayload::SessionCreated { .. })
            && !is_current_creation(&event.payload)
        {
            return Err(EventLogError::Corrupt {
                path: self.path.clone(),
                message: cookie_agent_protocol::EventSchemaError::UnsupportedHistoryFormat
                    .to_string(),
            });
        }
        let mut moved = Vec::new();
        let mut record = stored_line(&event, &mut |digest, bytes| {
            moved.push((digest.to_owned(), bytes));
        })
        .map_err(|source| EventLogError::Json {
            path: self.path.clone(),
            source,
        })?;
        record.push(b'\n');
        let mut validation = self
            .validation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) =
            validate_record_incremental(&self.path, self.session_id, &event, &mut validation, true)
        {
            drop(validation);
            drop(events);
            let rebuilt = self.rebuilt_validation()?;
            *self
                .validation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = rebuilt;
            return Err(error);
        }
        drop(validation);
        drop(events);
        let written = self.persisted.load(Ordering::Acquire);
        let write_result = if written {
            self.write_blobs(&moved)
                .and_then(|()| self.write_record(&record, syncs_on_append(&event.payload)))
        } else {
            Ok(None)
        };
        let commit = match write_result {
            Ok(commit) => commit,
            Err(error) => {
                let mut events = self
                    .events
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Part of the record may have reached the file.
                events.lines = None;
                drop(events);
                let rebuilt = self.rebuilt_validation()?;
                *self
                    .validation
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = rebuilt;
                return Err(error);
            }
        };
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let event = Arc::new(event);
        if written && let Some(lines) = &mut events.lines {
            lines.push(Some(event.seq), record.len() as u64);
        }
        events.push(event.clone());
        self.next_seq.store(event.seq + 1, Ordering::Release);
        if written {
            self.narrow(&mut events, |storage| storage.narrow_after(&event));
        }
        Ok((event, commit))
    }

    /// Moves the window to where the log's records say it starts, if this log
    /// can have one (`docs/site/specs/forever-sessions.md` section 3.2): the file
    /// holds what it lets go of, so a log still buffered, and one whose records
    /// are not the file's stay whole. A read-only log has what it opened with:
    /// one that read itself whole has a reason, damage or an older format, and
    /// no appends move its tip. An open that did not read a log whole holds only
    /// its window ([`state_file`]). `narrow` is how: at an ACP agent's committed
    /// turn, at a native model's checkpoint, or both as a log opens.
    fn narrow(&self, storage: &mut EventStorage, narrow: impl FnOnce(&mut EventStorage)) {
        if self.read_only
            || self.converted
            || self.render_only.is_some()
            || !self.persisted.load(Ordering::Acquire)
        {
            return;
        }
        #[cfg(any(test, feature = "test-support"))]
        let (held, before) = (storage.snapshot(), storage.window);
        narrow(storage);
        if storage.lines_fall_short() {
            // Read again from the file, as a revert into what a window let go of
            // does; without them, a pass reads from the start and no state file is
            // written.
            let length = storage.lines.as_ref().map(|lines| lines.len);
            if storage.read_lines_again(&self.path, length, true).is_err() {
                storage.lines = None;
            }
        }
        #[cfg(any(test, feature = "test-support"))]
        if storage.window != before || storage.all.len() != held.len() {
            self.assert_file_reads_as(storage, &held);
        }
    }

    /// Test-only: the records the window just let go of read from the file as
    /// they were held, as every later pass over them expects, and so do those it
    /// pins before its start.
    #[cfg(any(test, feature = "test-support"))]
    fn assert_file_reads_as(&self, storage: &EventStorage, held: &[Arc<StoredEvent>]) {
        let Some(window) = storage.window else {
            return;
        };
        let Some(first) = held.first().map(|event| event.seq) else {
            return;
        };
        let range = self.file_range(storage, first, window);
        let read = History::with_file(None, range, Arc::default(), 0, 0)
            .collect::<Result<Vec<_>, _>>()
            .expect("what the window lets go of reads from the file");
        let read = read
            .iter()
            .map(|event| (event.seq, event))
            .collect::<HashMap<_, _>>();
        for held in held
            .iter()
            .chain(&storage.all[..storage.window_position()])
            .filter(|event| event.seq < window)
        {
            assert!(
                read.get(&held.seq).is_some_and(|read| *read == held),
                "record {} reads from the file otherwise than it was held",
                held.seq
            );
        }
    }

    /// Moves the log's window up to its latest committed turn, or where its
    /// native model's context starts, for what opened the log once it has done
    /// with the whole list. A log that cannot have a window stays as it is.
    pub fn narrow_window(&self) {
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.narrow(&mut self.storage(), |storage| {
            storage.narrow();
            storage.narrow_to_context();
        });
    }

    /// Where the file holds the records of sequences `from..until`, before the
    /// window; `None` when it holds none.
    fn file_range(
        &self,
        storage: &EventStorage,
        from: u64,
        until: u64,
    ) -> Option<history::FileRange> {
        let (offset, end) = match &storage.lines {
            Some(lines) => (
                lines.start_at_or_before(from),
                Some(lines.start_at_or_after(until)),
            ),
            None => (0, None),
        };
        // A read-only log's window is read by offset too, and only its owner
        // knows when the file is cut.
        let held_from = storage
            .all
            .get(storage.window_position())
            .filter(|_| self.read_only && storage.window.is_some())
            .and_then(|first| {
                let lines = storage.lines.as_ref()?;
                let index = lines
                    .starts
                    .binary_search_by_key(&first.seq, |(seq, _)| *seq)
                    .ok()?;
                Some(history::HeldFrom {
                    offset: lines.starts[index].1,
                    seq: first.seq,
                    timestamp: first.timestamp,
                })
            });
        (from < until && end != Some(offset)).then(|| history::FileRange {
            path: self.path.clone(),
            offset,
            end,
            from_seq: from,
            until_seq: until,
            refused: Arc::clone(&storage.refused),
            cuts: Arc::clone(&self.cuts),
            seen_cuts: self.cuts.load(Ordering::Acquire),
            undamaged: !self.diagnostics.iter().any(|diagnostic| diagnostic.skipped),
            held_from,
        })
    }

    /// The validation state of what the log holds, wherever it holds it: the
    /// caller holds `append`, and an append that failed left it wrong.
    fn rebuilt_validation(&self) -> Result<ValidationState, EventLogError> {
        let whole = || -> Result<ValidationState, EventLogError> {
            let mut state = self.validation_pass(ValidationState::whole(self.initial_taint()))?;
            state.finish_whole();
            Ok(state)
        };
        match self.validation_pass(ValidationState::new(self.initial_taint())) {
            // A record names what a record-at-a-time validation lets go of,
            // which a whole read accepted: validate it whole too.
            Err(EventLogError::Corrupt { .. }) => whole(),
            // A native model's validation keeps every call that ended in its
            // context, which an ACP agent's window let go of: as
            // [`Self::read_whole`] validates such a log whole, so does this.
            Ok(state) if state.let_go && self.storage().index.window_start.is_none() => whole(),
            folded => folded,
        }
    }

    fn validation_pass(
        &self,
        mut state: ValidationState,
    ) -> Result<ValidationState, EventLogError> {
        for record in self.pass(..)? {
            validate_record_incremental(&self.path, self.session_id, &*record?, &mut state, false)?;
        }
        Ok(state)
    }

    /// Returns once a sync covers `commit`: one sync covers every record
    /// written before it starts. Run it outside every lock: a sync on a
    /// busy disk takes up to seconds.
    pub fn sync_through(&self, commit: Commit) -> Result<(), EventLogError> {
        self.durability.sync_through(commit)
    }

    /// The last record written to the file, for [`Self::sync_through`] to
    /// cover everything written so far.
    #[must_use]
    pub fn latest_commit(&self) -> Commit {
        self.durability.latest()
    }

    /// Whether `commit` waits on no sync any more: one covering it ended,
    /// successfully or not. Publication follows this, so a failed sync
    /// does not hold back the session's later records.
    #[must_use]
    pub fn is_settled(&self, commit: Commit) -> bool {
        self.durability.is_settled(commit)
    }

    /// Writes the log whole as the content of the new file at `path` (a
    /// fork, or a buffered session's first persistence) and syncs it once,
    /// returning its layout for [`Self::mark_written`]. Every line the log
    /// was read from is copied as it was read, only its `session_id` set to
    /// this log's; the records appended since are serialized. The caller
    /// syncs the directory.
    pub fn write_whole(&self, path: &Path) -> Result<LineIndex, EventLogError> {
        let (events, raw) = {
            let storage = self.storage();
            (storage.snapshot(), Arc::clone(&storage.raw))
        };
        debug_assert!(
            !raw.partial,
            "a log written whole holds all its records and lines"
        );
        crate::blocking::off_worker(|| {
            cookie_agent_protocol::read_stored_history(|| {
                write_event_jsonl_in_place(path, self.session_id, &events, &raw.read(&..))
            })
        })
    }

    /// Calls `visit` with the JSON of every line of the log whose sequence is
    /// in `seqs`, as the log stores it: a line read from disk as its bytes,
    /// which hold whatever this build did not decode (a skipped event, an
    /// unknown member), and a record written since as it serializes, with
    /// the references to the blobs its large values move to ([`blobs`]).
    pub fn visit_json(
        &self,
        seqs: impl std::ops::RangeBounds<u64>,
        mut visit: impl FnMut(&Value),
    ) -> Result<(), EventLogError> {
        let (events, raw, windowed) = {
            let storage = self.storage();
            let windowed = storage.window.is_some() || storage.raw.partial;
            (storage.snapshot(), Arc::clone(&storage.raw), windowed)
        };
        if windowed {
            return self.visit_file_json(seqs, visit);
        }
        let json_error = |source| EventLogError::Json {
            path: self.path.clone(),
            source,
        };
        let record = |seq: u64| {
            events
                .binary_search_by_key(&seq, |event| event.seq)
                .ok()
                .map(|index| events[index].as_ref())
        };
        let read = crate::blocking::off_worker(|| {
            cookie_agent_protocol::read_stored_history(|| {
                let read = raw.read(&seqs);
                let mut unread = HashSet::new();
                for line in raw.lines.iter().filter(|line| seqs.contains(&line.seq)) {
                    match read.bytes(line, record(line.seq)) {
                        Some(bytes) => visit(&serde_json::from_slice::<Value>(bytes)?),
                        None => {
                            unread.insert(line.seq);
                        }
                    }
                }
                Ok::<_, serde_json::Error>(unread)
            })
        });
        let unread = read.map_err(json_error)?;
        for event in events.iter().filter(|event| {
            seqs.contains(&event.seq)
                && (raw.get(event.seq).is_none() || unread.contains(&event.seq))
        }) {
            // As its line stores it.
            let mut value = cookie_agent_protocol::write_stored_history(|| {
                serde_json::to_value(event.as_ref())
            })
            .map_err(json_error)?;
            if blobs::may_move(&event.payload)
                && let Some(payload) = value.get_mut("payload")
            {
                blobs::externalize(payload, &mut |_, _| {});
            }
            visit(&value);
        }
        Ok(())
    }

    /// [`Self::visit_json`] of a log that no longer holds its records: every
    /// line it wrote is in the file, one at a time and keeping nothing, bounded
    /// at the file's complete lines as they were when asked.
    fn visit_file_json(
        &self,
        seqs: impl std::ops::RangeBounds<u64>,
        mut visit: impl FnMut(&Value),
    ) -> Result<(), EventLogError> {
        use std::io::{BufRead as _, Read as _, Seek as _};
        let first = match seqs.start_bound() {
            std::ops::Bound::Included(start) => *start,
            std::ops::Bound::Excluded(start) => start.saturating_add(1),
            std::ops::Bound::Unbounded => 0,
        };
        let (offset, end, seen_cuts) = {
            let storage = self.storage();
            let (offset, end) = storage.lines.as_ref().map_or((0, None), |lines| {
                (lines.start_at_or_before(first), Some(lines.len))
            });
            (offset, end, self.cuts.load(Ordering::Acquire))
        };
        let io_error = |source| EventLogError::Io {
            path: self.path.clone(),
            source,
        };
        crate::blocking::off_worker(|| {
            let mut file = fs::File::open(&self.path).map_err(io_error)?;
            file.seek(io::SeekFrom::Start(offset)).map_err(io_error)?;
            let length = end.map_or(u64::MAX, |end| end - offset);
            let mut reader = io::BufReader::new(file.take(length));
            let mut line = Vec::new();
            loop {
                line.clear();
                let read = reader.read_until(b'\n', &mut line).map_err(io_error)?;
                if self.cuts.load(Ordering::Acquire) != seen_cuts {
                    return Err(EventLogError::HistoryCut {
                        path: self.path.clone(),
                    });
                }
                // A failed write can leave part of a line at the end.
                if read == 0 || !line.ends_with(b"\n") {
                    return Ok(());
                }
                // A line the load found no sequence in is no line of the log's.
                let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                let Some(seq) = value.get("seq").and_then(Value::as_u64) else {
                    continue;
                };
                if seqs.contains(&seq) {
                    visit(&value);
                }
            }
        })
    }

    /// The first state event of a type this build does not know, if the log
    /// holds one: the session is render-only.
    #[must_use]
    pub fn unknown_state_event(&self) -> Option<&str> {
        match &self.render_only {
            Some(RenderOnly::UnknownStateEvent(event)) => Some(event),
            _ => None,
        }
    }

    /// Whether a read-only open converted the log from an older history
    /// format in memory: its records and lines are not the file's.
    #[must_use]
    pub fn is_converted(&self) -> bool {
        self.converted
    }

    /// Whether the log takes no appends: a newer build wrote a state event
    /// this one cannot continue from, or its sequence breaks.
    #[must_use]
    pub fn is_render_only(&self) -> bool {
        self.render_only.is_some()
    }

    /// Refuses to change a render-only log.
    pub fn ensure_appendable(&self) -> Result<(), EventLogError> {
        match &self.render_only {
            Some(RenderOnly::UnknownStateEvent(event)) => Err(EventLogError::RequiresNewerEngine {
                path: self.path.clone(),
                event: Some(event.clone()),
                history_format: None,
            }),
            Some(RenderOnly::SequenceBroken(seq)) => Err(EventLogError::SequenceBroken {
                path: self.path.clone(),
                seq: *seq,
            }),
            None => Ok(()),
        }
    }

    /// Owned copies of every event, for a test.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn events(&self) -> Vec<StoredEvent> {
        self.event_snapshot()
            .iter()
            .map(|event| event.as_ref().clone())
            .collect()
    }

    /// Every event at once, for a test: production code says what it needs
    /// (`docs/site/specs/forever-sessions.md` section 2).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn event_snapshot(&self) -> EventSnapshot {
        self.snapshot_and_state().0
    }

    /// The events since the log's window start. O(1): a shared view, not a
    /// copy. A log holds every event until it is windowed. A window starts at
    /// the latest committed turn (`docs/site/specs/forever-sessions.md` section
    /// 3.2), so a reader takes this only for what it knows lies after that:
    /// anything older is [`Self::state`] or [`Self::history`].
    #[must_use]
    pub fn recent(&self) -> EventSnapshot {
        self.storage().recent()
    }

    /// What readers need of the whole log, as of its latest record.
    #[must_use]
    pub fn state(&self) -> Arc<LogState> {
        Arc::clone(&self.storage().index.state)
    }

    /// [`Self::event_snapshot`] and [`Self::state`] of the same moment, for a test.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn snapshot_and_state(&self) -> (EventSnapshot, Arc<LogState>) {
        {
            let storage = self.storage();
            if storage.window.is_none() {
                return (storage.snapshot(), Arc::clone(&storage.index.state));
            }
        }
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.snapshot_under_append()
    }

    /// Test-only: panics unless what a windowed log answers is what a reopening
    /// of its file gives (`docs/site/specs/forever-sessions.md` section 7). A
    /// log holding its records answers from them, but for one whose validation
    /// let go of what settled, as a log gone native after an ACP agent's window
    /// does, which must then validate as a reopening's first model context does.
    #[cfg(any(test, feature = "test-support"))]
    pub fn assert_window_reads_as_reopened(&self) {
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let let_go = self
            .validation
            .lock()
            .expect("validation lock poisoned")
            .let_go;
        let reopens = !self.read_only
            && !self.converted
            && self.render_only.is_none()
            && self.persisted.load(Ordering::Acquire);
        if self.storage().window.is_none() && !(let_go && reopens) {
            return;
        }
        let reopened = Self::parsed_read_only(self.path.clone(), self.session_id)
            .expect("a windowed log reopens");
        let (whole, reopened_validation) = {
            let storage = reopened.storage();
            (
                storage.snapshot(),
                reopened
                    .validation
                    .lock()
                    .expect("validation lock poisoned")
                    .clone(),
            )
        };
        let (read, recent, state) = {
            let read = self
                .unnoted_pass(..)
                .and_then(Iterator::collect::<Result<Vec<_>, _>>)
                .expect("a windowed log reads whole");
            let (recent, state) = self.recent_and_state();
            (read, recent, state)
        };
        assert_eq!(read.len(), whole.len(), "the records read");
        for (read, whole) in read.iter().zip(whole.iter()) {
            assert!(
                read == whole,
                "record {} reads otherwise than a reopening reads it",
                whole.seq
            );
        }
        assert_eq!(
            self.creation().map(|creation| creation.seq),
            whole.first().map(|creation| creation.seq),
            "the creation"
        );
        let held = whole.len() - recent.len();
        assert!(
            recent.iter().zip(&whole[held..]).all(|(a, b)| a == b),
            "the window is not the log's tail"
        );
        {
            let storage = self.storage();
            let pinned = &storage.all[..storage.window_position()];
            assert!(
                storage.context || pinned.is_empty(),
                "only a native model's context pins records"
            );
            assert!(
                pinned.iter().all(|pinned| whole
                    .binary_search_by_key(&pinned.seq, |event| event.seq)
                    .is_ok_and(|index| whole[index] == *pinned)),
                "a pinned record is not the log's"
            );
        }
        state::reference::assert_matches_scans(&state, &whole);
        let native = self.storage().validates_for_a_native_model();
        self.validation
            .lock()
            .expect("validation lock poisoned")
            .assert_agrees_with_whole(&reopened_validation, "a reopening's", native);
        let reopened_storage = reopened.storage();
        let storage = self.storage();
        assert_eq!(
            storage.physical_model_turns, reopened_storage.physical_model_turns,
            "the model turns"
        );
        assert_eq!(
            storage.lines,
            reopened_storage
                .lines
                .as_ref()
                .zip(storage.lines.as_ref())
                .map(|(parsed, lines)| parsed.let_go_as(lines)),
            "the file's line index"
        );
        assert_eq!(
            storage.index.window_start, reopened_storage.index.window_start,
            "the window's start"
        );
        assert_eq!(
            storage.index.prompt_blocks, reopened_storage.index.prompt_blocks,
            "the prompt blocks"
        );
        assert_eq!(
            storage.index.deferral, reopened_storage.index.deferral,
            "the deferred tools"
        );
        assert_eq!(
            storage.index.last_run_started, reopened_storage.index.last_run_started,
            "the latest run"
        );
        assert_eq!(
            storage.index.run_attempts, reopened_storage.index.run_attempts,
            "the run attempts"
        );
        assert_eq!(
            storage.index.latest_real_usage(),
            reopened_storage.index.latest_real_usage(),
            "the latest usage"
        );
        if let Some(view_start) = &storage.view_start
            && let Some(placed) = view_start.start()
        {
            let at = whole
                .iter()
                .position(|event| event.seq == placed.seq)
                .expect("a view cache starts at a record of the log");
            assert_eq!(
                whole[at].timestamp, placed.timestamp,
                "where a view cache starts"
            );
            assert!(
                cookie_agent_protocol::display_view::cache_start(&whole, VIEW_BUDGET)
                    .is_some_and(|latest| at <= latest),
                "a view cache starts where a window can reach before it"
            );
            assert_eq!(
                view_start.header(),
                cookie_agent_protocol::display_view::state_events(&whole[..at]),
                "a view cache's header"
            );
        }
    }

    /// [`Self::snapshot_and_state`], for a caller that holds `append`.
    #[cfg(any(test, feature = "test-support"))]
    fn snapshot_under_append(&self) -> (EventSnapshot, Arc<LogState>) {
        let events = self
            .unnoted_pass(..)
            .and_then(Iterator::collect)
            .expect("a test's log reads whole");
        (Arc::new(events), self.state())
    }

    /// [`Self::recent`] and [`Self::state`] of the same moment.
    #[must_use]
    pub fn recent_and_state(&self) -> (EventSnapshot, Arc<LogState>) {
        let storage = self.storage();
        (storage.recent(), Arc::clone(&storage.index.state))
    }

    /// The events a native model's context is assembled from: pinned records
    /// and the window, in order (`docs/site/specs/forever-sessions.md` section
    /// 5.1, and [`model_context`] for what is pinned). A native log holds only
    /// that once a checkpoint commits, and every record until then; a log an
    /// ACP agent's window narrowed is read whole, which is why this is
    /// fallible. So only what serves a native model takes it.
    pub fn model_context_events(&self) -> Result<EventSnapshot, EventLogError> {
        Ok(self.model_context_and_state()?.0)
    }

    /// [`Self::model_context_events`] and [`Self::state`] of the same moment.
    pub fn model_context_and_state(&self) -> Result<(EventSnapshot, Arc<LogState>), EventLogError> {
        let held = {
            let storage = self.storage();
            (storage.window.is_none() || storage.context).then(|| {
                (
                    storage.snapshot(),
                    Arc::clone(&storage.index.state),
                    storage.context,
                )
            })
        };
        let (whole, state) = match held {
            Some((held, state, _context)) => {
                #[cfg(any(test, feature = "test-support"))]
                if _context {
                    self.assert_context_reads_as_whole(&held, &state);
                }
                (held, state)
            }
            None => self.read_whole()?,
        };
        let selected = model_context::model_context(&whole, &state).map(Arc::new);
        #[cfg(any(test, feature = "test-support"))]
        self.assert_model_context_holds(&whole, &state, selected.as_ref());
        Ok((selected.unwrap_or(whole), state))
    }

    /// Test-only: what a native window gives is what the whole file gives, read
    /// at the same moment (`docs/site/specs/forever-sessions.md` section 5.3).
    #[cfg(any(test, feature = "test-support"))]
    fn assert_context_reads_as_whole(&self, held: &[Arc<StoredEvent>], state: &LogState) {
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tip = held.last().map_or(0, |event| event.seq);
        let Ok(whole) = self
            .unnoted_pass(..=tip)
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        else {
            // A revert cut the file since.
            return;
        };
        let expected = model_context::model_context(&whole, state).unwrap_or(whole);
        let given = model_context::model_context(held, state).unwrap_or_else(|| held.to_vec());
        assert!(
            given.len() == expected.len()
                && given
                    .iter()
                    .zip(&expected)
                    .all(|(given, expected)| given == expected),
            "a native window gives {:?}, a whole read of its file {:?}",
            given.iter().map(|event| event.seq).collect::<Vec<_>>(),
            expected.iter().map(|event| event.seq).collect::<Vec<_>>(),
        );
    }

    /// Test-only: what a log that let go of everything [`model_context`] did
    /// not return, each time it was asked, would return now is what the
    /// whole log gives (`docs/site/specs/forever-sessions.md` section 5.3), so
    /// a native window can hold only that. A cut of the file starts it over.
    #[cfg(any(test, feature = "test-support"))]
    fn assert_model_context_holds(
        &self,
        whole: &[Arc<StoredEvent>],
        state: &LogState,
        selected: Option<&EventSnapshot>,
    ) {
        let selected = selected.map_or(whole, |selected| selected.as_slice());
        let cuts = self.cuts.load(Ordering::SeqCst);
        let mut shadow = self
            .model_context_shadow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut held = match shadow.take() {
            Some((at_cuts, held)) if at_cuts == cuts => held,
            _ => Vec::new(),
        };
        let tip = held.last().map_or(0, |event| event.seq);
        held.extend(whole.iter().filter(|event| event.seq > tip).cloned());
        let narrowed = model_context::model_context(&held, state).unwrap_or(held);
        assert!(
            narrowed.len() == selected.len()
                && narrowed
                    .iter()
                    .zip(selected)
                    .all(|(held, whole)| held.seq == whole.seq && held == whole),
            "a log holding only what its model's context needed would give {:?}, the whole log {:?}",
            narrowed.iter().map(|event| event.seq).collect::<Vec<_>>(),
            selected.iter().map(|event| event.seq).collect::<Vec<_>>(),
        );
        *shadow = Some((cuts, narrowed));
    }

    /// Reads a windowed log whole, holding appends off (section 5.4). A log whose
    /// agent is no longer an ACP agent then holds what its model's context needs:
    /// all of it until a checkpoint, or the context since the latest one, until an
    /// ACP agent's next committed turn; a reader asking of one whose agent still
    /// is, as `get_history` can, gets its answer and the log lets it go.
    fn read_whole(&self) -> Result<(EventSnapshot, Arc<LogState>), EventLogError> {
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let whole = Arc::new(self.pass(..)?.collect::<Result<Vec<_>, _>>()?);
        // A native model's validation keeps every call that ended in its context,
        // which a compaction's elision may name, and an ACP agent's window let go
        // of those: it validates whole again.
        let native = self.storage().index.window_start.is_none();
        let let_go = self
            .validation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .let_go;
        if native && let_go {
            let rebuilt =
                rebuild_validation(&self.path, self.session_id, &whole, &self.initial_taint())?;
            *self
                .validation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = rebuilt;
        }
        let mut storage = self.storage();
        if storage.index.window_start.is_none() {
            storage.all = Arc::clone(&whole);
            storage.window = None;
            storage.context = false;
            storage.view_start = None;
            // Holding then only what its model's context needs, when it has a
            // checkpoint.
            self.narrow(&mut storage, EventStorage::narrow_to_context);
        }
        Ok((whole, Arc::clone(&storage.index.state)))
    }

    /// A pass over the whole log, as it stands now, one record at a time.
    #[cfg_attr(any(test, feature = "test-support"), track_caller)]
    pub fn history(&self) -> Result<History, EventLogError> {
        self.history_range(..)
    }

    /// A pass over the log's records whose sequences are in `seqs`, which
    /// starts at the first of them rather than at the log's start.
    #[cfg_attr(any(test, feature = "test-support"), track_caller)]
    pub fn history_range(
        &self,
        seqs: impl std::ops::RangeBounds<u64>,
    ) -> Result<History, EventLogError> {
        #[cfg(any(test, feature = "test-support"))]
        self.note_history_call("history");
        self.pass(seqs)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[track_caller]
    fn note_history_call(&self, kind: &str) {
        self.history_calls
            .lock()
            .expect("history calls lock poisoned")
            .push(format!("{kind}: {}", std::panic::Location::caller()));
    }

    /// Test-only: the passes over this log asked for since the last time this
    /// was called, each named `history` when a reader asked for one by name and
    /// `file from <seq>` when it reads the file, whatever asked, with where it
    /// was asked.
    #[cfg(any(test, feature = "test-support"))]
    pub fn take_history_calls_for_test(&self) -> Vec<String> {
        std::mem::take(
            &mut self
                .history_calls
                .lock()
                .expect("history calls lock poisoned"),
        )
    }

    /// Test-only: the latest record whose committed turn starts the window.
    #[cfg(any(test, feature = "test-support"))]
    pub fn state_latest_turn_seq_for_test(&self) -> Option<u64> {
        self.storage().index.window_start
    }

    /// Test-only: [`Self::truncate_after`] under the log's own capability.
    #[cfg(any(test, feature = "test-support"))]
    pub fn truncate_after_for_test(&self, through_seq: u64) -> Result<(), EventLogError> {
        let capability = self
            .write_capability
            .as_ref()
            .ok_or_else(|| EventLogError::ReadOnly(self.path.clone()))?;
        self.truncate_after(capability, through_seq)
    }

    /// Test-only: how many records the log holds, pinned ones included.
    #[cfg(any(test, feature = "test-support"))]
    pub fn storage_len_for_test(&self) -> usize {
        self.storage().all.len()
    }

    /// Test-only: the sequence the log holds its records from, when it does not
    /// hold them all.
    #[cfg(any(test, feature = "test-support"))]
    pub fn window_for_test(&self) -> Option<u64> {
        self.storage().window
    }

    #[cfg_attr(any(test, feature = "test-support"), track_caller)]
    fn pass(&self, seqs: impl std::ops::RangeBounds<u64>) -> Result<History, EventLogError> {
        let pass = self.unnoted_pass(seqs)?;
        #[cfg(any(test, feature = "test-support"))]
        if let Some(from) = pass.reads_the_file_from() {
            self.note_history_call(&format!("file from {from}"));
        }
        Ok(pass)
    }

    fn unnoted_pass(
        &self,
        seqs: impl std::ops::RangeBounds<u64>,
    ) -> Result<History, EventLogError> {
        let storage = self.storage();
        let events = storage.snapshot();
        let start = events.partition_point(|event| match seqs.start_bound() {
            std::ops::Bound::Included(start) => event.seq < *start,
            std::ops::Bound::Excluded(start) => event.seq <= *start,
            std::ops::Bound::Unbounded => false,
        });
        let end = events.partition_point(|event| match seqs.end_bound() {
            std::ops::Bound::Included(end) => event.seq <= *end,
            std::ops::Bound::Excluded(end) => event.seq < *end,
            std::ops::Bound::Unbounded => true,
        });
        let Some(window) = storage.window else {
            return Ok(History::new(events, start, end));
        };
        // What is pinned before the window is read from the file with the rest.
        let start = start.max(storage.window_position());
        // What precedes the window: the creation, held, and then the file.
        let creation = storage
            .creation
            .clone()
            .filter(|creation| seqs.contains(&creation.seq));
        let after_creation = storage
            .creation
            .as_ref()
            .map_or(0, |creation| creation.seq + 1);
        let from = match seqs.start_bound() {
            std::ops::Bound::Included(start) => *start,
            std::ops::Bound::Excluded(start) => start.saturating_add(1),
            std::ops::Bound::Unbounded => 0,
        }
        .max(after_creation);
        let until = match seqs.end_bound() {
            std::ops::Bound::Included(end) => end.saturating_add(1),
            std::ops::Bound::Excluded(end) => *end,
            std::ops::Bound::Unbounded => u64::MAX,
        }
        .min(window);
        let file = self.file_range(&storage, from, until);
        Ok(History::with_file(creation, file, events, start, end))
    }

    /// A pass for the newest-first window of the whole log, as it stands now,
    /// which reads the log only from where its view cache starts when it knows
    /// that.
    #[cfg_attr(any(test, feature = "test-support"), track_caller)]
    pub fn newest_pass(&self) -> Result<NewestPass, EventLogError> {
        #[cfg(any(test, feature = "test-support"))]
        self.note_history_call("history");
        let start = self
            .storage()
            .view_start
            .as_ref()
            .and_then(|view_start| Some((view_start.start()?, view_start.header().to_vec())));
        let from_cache_start = match start {
            Some((start, header)) => Some((start, header, self.pass(start.seq..)?)),
            None => None,
        };
        Ok(NewestPass {
            from_cache_start,
            whole: self.pass(..)?,
        })
    }

    /// Where a view cache of this log starts and what precedes it, for a log
    /// that cannot walk back from its end to find that.
    #[must_use]
    pub fn view_start_fold(&self) -> Option<CacheStartFold> {
        self.storage().view_start.as_deref().cloned()
    }

    /// The log's first record, its creation, which it keeps whatever its
    /// window: `None` only for a log built empty.
    #[must_use]
    pub fn creation(&self) -> Option<Arc<StoredEvent>> {
        self.storage().creation.clone()
    }

    fn storage(&self) -> std::sync::MutexGuard<'_, EventStorage> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many records the log holds, wherever it holds them.
    #[must_use]
    pub fn record_count(&self) -> u64 {
        self.validation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_count as u64
    }

    /// How many times a revert cut the log, for a reader that read it before
    /// taking a lock to tell whether what it read still stands.
    #[must_use]
    pub fn cut_count(&self) -> u64 {
        self.cuts.load(Ordering::Acquire)
    }

    /// Physical `ModelTurnCommitted` records, including reverted ones.
    #[must_use]
    pub fn physical_model_turns(&self) -> u64 {
        self.storage().physical_model_turns
    }

    /// Visible `ModelAttemptStarted` records of `run`.
    #[must_use]
    pub fn visible_run_attempts(&self, run: RunId) -> u32 {
        self.storage()
            .index
            .run_attempts
            .iter()
            .find(|(attempted, _)| *attempted == run)
            .map_or(0, |(_, attempts)| *attempts)
    }

    /// Physical events after `cursor`, at most `limit` of them, read without
    /// holding the log's lock.
    ///
    /// A limited page also stops before `REPLAY_PAGE_MAX_BYTES` (1 MiB) of
    /// serialized events, since it goes out as one WebSocket frame; it always
    /// holds at least one event, so paging makes progress.
    pub fn events_after(
        &self,
        cursor: Option<u64>,
        limit: Option<std::num::NonZeroU32>,
    ) -> Result<cookie_agent_protocol::EventsSubscribeResult, EventLogError> {
        let max_bytes = limit.map_or(usize::MAX, |_| REPLAY_PAGE_MAX_BYTES);
        let limit = limit.map_or(usize::MAX, |limit| limit.get() as usize);
        let after = cursor.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        let mut page = Vec::new();
        let mut page_bytes = 0_usize;
        let mut has_more = false;
        for event in self.pass((after, std::ops::Bound::Unbounded))? {
            let event = event?;
            if page.len() == limit {
                has_more = true;
                break;
            }
            if max_bytes != usize::MAX {
                page_bytes = page_bytes.saturating_add(serialized_len(&event));
                if page_bytes > max_bytes && !page.is_empty() {
                    has_more = true;
                    break;
                }
            }
            page.push(event);
        }
        Ok(cookie_agent_protocol::EventsSubscribeResult::page(
            page, has_more,
        ))
    }

    /// Whether the event at `seq` is the one a reader saw at `timestamp`. A
    /// revert truncates the log and reuses sequences, so a cursor alone
    /// cannot tell the replaced event from its replacement.
    #[must_use]
    pub fn event_matches(&self, seq: u64, timestamp: Timestamp) -> bool {
        // A record that cannot be read is no match: the reader starts over.
        self.pass(seq..=seq)
            .ok()
            .and_then(|mut record| record.next())
            .and_then(Result::ok)
            .is_some_and(|event| event.timestamp == timestamp)
    }

    /// Where the line of the record at `seq` starts in the log's file, when
    /// the log knows the file's layout.
    #[must_use]
    pub fn line_start(&self, seq: u64) -> Option<u64> {
        let storage = self.storage();
        let starts = &storage.lines.as_ref()?.starts;
        starts
            .binary_search_by_key(&seq, |(line, _)| *line)
            .ok()
            .map(|index| starts[index].1)
    }

    #[must_use]
    pub fn last_event(&self) -> Option<Arc<StoredEvent>> {
        self.storage().all.last().cloned()
    }

    pub fn last_run_started(&self) -> Option<(u64, RunId, cookie_agent_protocol::ModelSelection)> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .last_run_started
            .clone()
    }

    /// Whether the latest run has not recorded its prompt blocks yet.
    pub fn prompt_blocks_awaiting_record(&self) -> bool {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .prompt_blocks
            .prompt
            .is_some()
    }

    /// The prompt blocks of the latest run and what the model knows of them.
    pub fn prompt_blocks(&self) -> PromptBlockFold {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .prompt_blocks
            .clone()
    }

    /// Whether the log is an ACP agent's own subagent session's.
    pub fn is_acp_subagent(&self) -> bool {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .acp_subagent
    }

    /// The tools the latest recorded model request was offered.
    pub fn last_model_tool_names(&self) -> Option<Vec<String>> {
        self.storage()
            .index
            .last_model_tools
            .as_ref()
            .map(|(names, _)| names.clone())
    }

    /// The latest `MemoryLoaded` record, and whether a checkpoint was
    /// committed after it.
    pub fn last_memory_loaded(&self) -> Option<(Arc<StoredEvent>, bool)> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .last_memory_loaded
            .clone()
    }

    /// Deferred tools since the latest checkpoint, as the log records them.
    pub fn deferral(&self) -> Arc<DeferralFold> {
        Arc::clone(
            &self
                .events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .index
                .deferral,
        )
    }

    /// The published names the current tool round pins (stable prefix
    /// spec, section 5); empty without a round.
    pub fn round_pins(&self) -> std::collections::HashMap<(String, String), String> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .deferral
            .round
            .as_ref()
            .map(|round| {
                round
                    .tools
                    .iter()
                    .filter_map(|tool| {
                        let source = tool.source.as_ref()?;
                        Some((
                            (source.label.clone(), source.tool.clone()),
                            tool.name.clone(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the latest recorded tool names sent to the model are `names`.
    pub fn last_model_tools_match(&self, names: &[String], deferred: &[String]) -> bool {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .last_model_tools
            .as_ref()
            .is_some_and(|(last, last_deferred)| last == names && last_deferred == deferred)
    }

    pub fn latest_checkpoint_seq(&self) -> u64 {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .last_checkpoint_seq
    }

    pub fn latest_real_usage(&self) -> Option<(u64, u64)> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .latest_real_usage()
    }

    pub fn checkpoint_covers_input(&self, input_through_seq: u64) -> bool {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .index
            .last_checkpoint_input_through_seq
            >= input_through_seq
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[EventLoadDiagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub fn delegation_event_tainted(&self, event: &StoredEvent) -> bool {
        self.delegation_record_tainted(&event.payload, event.seq)
    }

    /// [`Self::delegation_event_tainted`] of the record at `seq` holding `payload`.
    #[must_use]
    pub fn delegation_record_tainted(&self, payload: &EventPayload, seq: u64) -> bool {
        delegation_invocation_from_event(payload).is_some_and(|invocation_id| {
            self.validation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .taint
                .delegation_before(invocation_id, seq)
        })
    }

    #[must_use]
    pub fn physical_tip_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Acquire).saturating_sub(1)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn is_persisted(&self) -> bool {
        self.persisted.load(Ordering::Acquire)
    }

    pub fn mark_persisted(&self) {
        self.persisted.store(true, Ordering::Release);
    }

    /// Marks a buffered log persisted once its file was written whole with
    /// the layout `lines`.
    pub fn mark_written(&self, lines: LineIndex) {
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut storage = self.storage();
        Arc::make_mut(&mut storage.raw).rebase(&self.path, &lines);
        storage.lines = Some(lines);
        self.mark_persisted();
        self.narrow(&mut storage, |storage| {
            storage.narrow();
            storage.narrow_to_context();
        });
    }

    /// Test-only: the log opened without reading its file whole: from its
    /// `state.json`, or a record at a time.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn opened_without_parse_for_test(&self) -> bool {
        self.opened_from_state
            || self
                .opened
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
    }

    /// Test-only: the sequences of the lines the log keeps in memory.
    #[cfg(any(test, feature = "test-support"))]
    pub fn kept_raw_seqs_for_test(&self) -> Vec<u64> {
        self.storage()
            .raw
            .lines
            .iter()
            .filter(|line| matches!(line.bytes, RawBytes::Kept(_)))
            .map(|line| line.seq)
            .collect()
    }

    /// Closes the append handle; the next append reopens it.
    pub fn suspend_writer(&self) {
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }

    fn initial_taint(&self) -> ValidationTaint {
        self.initial_validation_taint
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Removes every event after `through_seq`, from the file (cut at the
    /// first later record and synced) and from memory. The removed sequence
    /// numbers are reused by the next appends. `SessionCreated` always stays.
    pub fn truncate_after(
        &self,
        capability: &WriteCapability,
        through_seq: u64,
    ) -> Result<(), EventLogError> {
        if self.read_only
            || !self
                .write_capability
                .as_ref()
                .is_some_and(|expected| capability.authorizes(expected))
        {
            return Err(EventLogError::ReadOnly(self.path.clone()));
        }
        self.ensure_appendable()?;
        let _append = self
            .append
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if through_seq == 0 {
            return Err(EventLogError::Corrupt {
                path: self.path.clone(),
                message: "truncation must keep SessionCreated".into(),
            });
        }
        if through_seq >= self.physical_tip_seq() {
            return Ok(());
        }
        // A windowed log folds what stays from the file, before it cuts anything.
        let mut kept_taint = self.initial_taint();
        kept_taint.truncate(through_seq);
        let folded = if self.storage().window.is_some() {
            Some(
                match self.fold_prefix(through_seq, ValidationState::new(kept_taint.clone())) {
                    // A record names what a record-at-a-time validation lets go
                    // of, which a whole read accepted: validate it whole too.
                    Err(EventLogError::Corrupt { .. }) => {
                        self.fold_prefix(through_seq, ValidationState::whole(kept_taint.clone()))?
                    }
                    folded => folded?,
                },
            )
        } else {
            None
        };
        let mut cut_length = None;
        if self.persisted.load(Ordering::Acquire) {
            // Before the cut, so a crash leaves no file naming what it cut. One
            // that a write in flight puts back no longer matches the log.
            if let Some(directory) = self.path.parent() {
                match fs::remove_file(directory.join(state_file::STATE_FILE)) {
                    Ok(()) => {}
                    Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(EventLogError::Io {
                            path: directory.join(state_file::STATE_FILE),
                            source,
                        });
                    }
                }
            }
            self.state_written.store(0, Ordering::Release);
            *self
                .opened
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            // A pass reading the file finds this after the read that could
            // see the cut, or finds the file ends early; and one asked for
            // between here and the new state takes the second count below.
            self.cuts.fetch_add(1, Ordering::AcqRel);
            // Close the append handle first so nothing writes past the cut.
            self.writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            let lines = self.storage().lines.clone();
            let length = match lines {
                Some(lines) => match lines.cut_after(through_seq) {
                    Some(length) => length,
                    // Before the lines it holds: read from the nearest mark.
                    None => scanned_cut_from(
                        &self.path,
                        lines.start_at_or_before(through_seq),
                        through_seq,
                    )?,
                },
                None => scanned_cut_from(&self.path, 0, through_seq)?,
            };
            cut_event_file(&self.path, length)?;
            cut_length = Some(length);
            self.durability.cut()?;
        }
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let let_go_lines = events
            .lines
            .as_ref()
            .is_some_and(|lines| lines.held_from().is_some());
        let validation = match folded {
            Some(folded) => {
                events.truncate_lines(through_seq, cut_length);
                events.index = folded.index;
                events.physical_model_turns = folded.physical_model_turns;
                events.view_start = folded.view_start;
                events.window = folded.window;
                events.context = folded.context;
                events.all = Arc::new(folded.held);
                events.read_lines_again(&self.path, cut_length, let_go_lines)?;
                folded.validation
            }
            None => {
                events.truncate(through_seq, cut_length);
                events.read_lines_again(&self.path, cut_length, let_go_lines)?;
                #[cfg(any(test, feature = "test-support"))]
                state::reference::assert_matches_scans(&events.index.state, &events.all);
                rebuild_validation(&self.path, self.session_id, &events.all, &kept_taint)?
            }
        };
        *self
            .initial_validation_taint
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = kept_taint;
        *self
            .validation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = validation;
        self.next_seq.store(through_seq + 1, Ordering::Release);
        // Under the storage's lock, as a pass reads this with its window: what
        // was asked for under the old window sees it, and what is asked for
        // from now on starts from here.
        self.cuts.fetch_add(1, Ordering::AcqRel);
        drop(events);
        #[cfg(any(test, feature = "test-support"))]
        {
            let (events, state) = self.snapshot_under_append();
            state::reference::assert_matches_scans(&state, &events);
        }
        Ok(())
    }

    /// Folds a windowed log's records through `through_seq` from a pass over it,
    /// holding what the window it ends with holds (section 3.5), as a live append
    /// of each would: the window moves at each ACP agent's turn and each native
    /// model's checkpoint. The caller holds `append`.
    fn fold_prefix(
        &self,
        through_seq: u64,
        mut validation: ValidationState,
    ) -> Result<FoldedPrefix, EventLogError> {
        let mut storage = EventStorage::new(Vec::new());
        // What the window lets go of, it lets go of from the file.
        storage.lines = Some(LineIndex::default());
        let taint = validation.taint.clone();
        for record in self.pass(..=through_seq)? {
            let record = record?;
            validate_record_incremental(
                &self.path,
                self.session_id,
                &record,
                &mut validation,
                false,
            )?;
            storage.push(Arc::clone(&record));
            storage.narrow_after(&record);
        }
        validation.finish_whole();
        // Native since an ACP agent's window, which held too little to choose a
        // context from: whole, and then what its model's context needs. Its
        // validation too, as [`Self::read_whole`] does: what the window let go
        // of, the model's context may still name, and with the window gone
        // nothing reads it whole again.
        if storage.reads_whole_for_its_model() {
            let whole = self.pass(..=through_seq)?.collect::<Result<Vec<_>, _>>()?;
            if validation.let_go {
                validation = rebuild_validation(&self.path, self.session_id, &whole, &taint)?;
            }
            storage.all = Arc::new(whole);
            storage.window = None;
            storage.view_start = None;
            storage.narrow_to_context();
        }
        let EventStorage {
            index,
            physical_model_turns,
            view_start,
            window,
            context,
            all,
            ..
        } = storage;
        Ok(FoldedPrefix {
            index,
            physical_model_turns,
            validation,
            view_start: window.and(view_start),
            window,
            context,
            held: Arc::unwrap_or_clone(all),
        })
    }

    /// Stores the blobs a record about to be written names, unsynced: the
    /// sync that covers the record syncs them first. The caller holds
    /// `append`.
    fn write_blobs(&self, moved: &[(String, Vec<u8>)]) -> Result<(), EventLogError> {
        if moved.is_empty() {
            return Ok(());
        }
        let directory = blobs::tree_blob_dir(&self.path);
        let mut named = Vec::with_capacity(moved.len());
        for (digest, bytes) in moved {
            let stored =
                blobs::write(&directory, digest, bytes).map_err(|source| EventLogError::Io {
                    path: directory.join(digest),
                    source,
                })?;
            named.push((directory.join(digest), stored.is_some()));
        }
        self.durability.names_blobs(&directory, named);
        Ok(())
    }

    /// Writes one record, unsynced, returning its [`Commit`] when it has to
    /// be synced. The caller holds `append`.
    fn write_record(&self, record: &[u8], commit: bool) -> Result<Option<Commit>, EventLogError> {
        let mut writer = self.open_writer()?;
        writer
            .as_mut()
            .expect("event log writer initialized")
            .append(record, commit)
    }

    fn open_writer(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Option<EventLogWriter>>, EventLogError> {
        if self.read_only {
            return Err(EventLogError::ReadOnly(self.path.clone()));
        }
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if writer.is_none() {
            *writer = Some(EventLogWriter::open(&self.path, &self.durability)?);
        }
        Ok(writer)
    }

    /// Blocks the next sync of a commit point, after its record is written,
    /// until the returned sender releases it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn install_sync_hook_for_test(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, reached_receiver) = std::sync::mpsc::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        self.durability.install_hook(durability::SyncHook {
            reached,
            release: release_receiver,
        });
        (reached_receiver, release)
    }

    /// Test-only: how many times this log's file was synced.
    #[cfg(any(test, feature = "test-support"))]
    pub fn syncs_for_test(&self) -> usize {
        self.durability.syncs()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn install_append_authorization_hook_for_test(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, reached_receiver) = std::sync::mpsc::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *self
            .append_authorization_hook
            .lock()
            .expect("append authorization hook lock poisoned") = Some(AppendAuthorizationHook {
            reached,
            release: release_receiver,
        });
        (reached_receiver, release)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn writer_is_open_for_test(&self) -> bool {
        self.writer
            .lock()
            .expect("event log writer lock poisoned")
            .is_some()
    }

    /// Test-only: the file length a revert to `through_seq` keeps, by the
    /// line index, or `None` without one.
    #[cfg(any(test, feature = "test-support"))]
    pub fn indexed_cut_for_test(&self, through_seq: u64) -> Option<u64> {
        self.storage()
            .lines
            .as_ref()
            .and_then(|lines| lines.cut_after(through_seq))
    }

    #[cfg(test)]
    fn snapshot_lock_available_for_test(&self) -> bool {
        self.events.try_lock().is_ok()
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct RunAttribution {
    start_seq: u64,
    agent_id: cookie_agent_protocol::AgentId,
    /// The agent each internal agent kind was frozen as at run start.
    internal_agents: Vec<(
        cookie_agent_protocol::InternalAgentKind,
        cookie_agent_protocol::AgentId,
    )>,
    prompt_fingerprint: cookie_agent_protocol::Sha256Digest,
    selected_suffix: Vec<cookie_agent_protocol::ResolvedModelRef>,
    active_fallback_index: usize,
    next_attempt_ordinal: u32,
    attempts_on_active: u32,
    active_attempt: Option<AttemptId>,
    ordering_tainted: bool,
    /// The record that ended it, when one did.
    ended_seq: Option<u64>,
    /// The attempt whose replay evaluation the run recorded last: the base its
    /// loop's next evaluation may name, however many attempts end after it
    /// without one.
    replay_base: Option<AttemptId>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct AttemptAttribution {
    started_seq: u64,
    run_id: RunId,
    resolved_model: cookie_agent_protocol::ResolvedModelRef,
    finished: bool,
    /// A `ModelTurnCommitted` closed this attempt.
    committed: bool,
    /// An `AttemptAbandoned` closed this attempt.
    abandoned: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct InternalRunAttribution {
    start_seq: u64,
    invocation_id: cookie_agent_protocol::InternalAgentInvocationId,
    kind: cookie_agent_protocol::InternalAgentKind,
    run_id: RunId,
    active_model: Option<cookie_agent_protocol::ResolvedModelRef>,
    usage_recorded_in_phase: bool,
    model_phase_taint_seen: u64,
    usage_phase_taint_seen: u64,
    finished_seq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct DelegationAttribution {
    parent_run_id: RunId,
    child_session_id: SessionId,
    resume: bool,
    started: bool,
    child_run_id: Option<RunId>,
    finished: bool,
    finished_seq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct ValidationState {
    taint: ValidationTaint,
    #[serde(with = "crate::events::sorted::map")]
    runs: HashMap<RunId, RunAttribution>,
    #[serde(with = "crate::events::sorted::map")]
    approval_owners: HashMap<cookie_agent_protocol::ApprovalId, ApprovalOwner>,
    #[serde(with = "crate::events::sorted::map")]
    attempts: HashMap<AttemptId, AttemptAttribution>,
    #[serde(with = "crate::events::sorted::map")]
    turns: HashMap<u64, CommittedTurn>,
    #[serde(with = "crate::events::sorted::map")]
    turn_models: HashMap<u64, cookie_agent_protocol::ResolvedModelRef>,
    #[serde(with = "crate::events::sorted::set")]
    usage_turns: HashSet<u64>,
    #[serde(with = "crate::events::sorted::map")]
    internal_runs: HashMap<cookie_agent_protocol::InternalAgentRunId, InternalRunAttribution>,
    #[serde(with = "crate::events::sorted::map")]
    delegations: HashMap<cookie_agent_protocol::InvocationId, DelegationAttribution>,
    #[serde(with = "crate::events::sorted::map")]
    model_call_owners: HashMap<(RunId, ModelCallId), AssistantToolCallRef>,
    #[serde(with = "crate::events::sorted::map")]
    provider_item_owners: HashMap<(RunId, ProviderItemId), AssistantToolCallRef>,
    #[serde(with = "crate::events::sorted::map")]
    tool_starts: HashMap<ToolCallId, (RunId, AssistantToolCallRef)>,
    /// The calls that ended, with the record that ended each.
    #[serde(with = "crate::events::sorted::map")]
    terminated_tools: HashMap<ToolCallId, u64>,
    #[serde(with = "crate::events::sorted::set")]
    elided_tools: HashSet<ToolCallId>,
    /// Admitted inputs by sequence with their run (`None` for a runless
    /// admission), kept as a digest of the text.
    #[serde(with = "crate::events::sorted::map")]
    admissions: HashMap<u64, (Option<RunId>, InputDigest)>,
    /// What holds a run open besides its own records: a producer claim until its
    /// release, by the claim's sequence, and a question until its answer.
    #[serde(with = "crate::events::sorted::map")]
    claims: HashMap<u64, RunId>,
    #[serde(with = "crate::events::sorted::map")]
    questions: HashMap<cookie_agent_protocol::ElicitationId, RunId>,
    next_model_turn_seq: u64,
    previous_seq: Option<u64>,
    previous_timestamp: Option<Timestamp>,
    active_run: Option<RunId>,
    record_count: usize,
    /// Whether what settled is let go of after each record, and which ended calls
    /// stay in view.
    letting_go: LettingGo,
    /// Something was let go of: a record naming what this does not hold may name
    /// what is older than its floor.
    let_go: bool,
    /// Validating a log whole, as its load does, keeping everything, until
    /// [`Self::finish_whole`] lets go of what settled.
    #[serde(skip)]
    keep_all: bool,
}

/// The run that asked for an approval, and the record that settled it.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct ApprovalOwner {
    run: RunId,
    settled_seq: Option<u64>,
}

/// How much of what settled a windowed log's validation keeps, newest first, so
/// that a record that follows its settling by a moment still validates: an
/// abandon after a commit, a grant after its approval's end, a title agent's
/// usage after its run's end (`docs/site/specs/forever-sessions.md` section 12).
const SETTLED_KEPT: usize = 16;
/// The committed turns it keeps: a call's start and a turn's usage name the
/// latest, in every log of the store.
const TURNS_KEPT: u64 = 16;
/// The admitted inputs it keeps, which a recall names while they wait.
const ADMISSIONS_KEPT: usize = 256;

/// The keys of `map` whose value `settled` gives a sequence for, but for the
/// newest `kept` of them.
fn settled_beyond<K: Copy + Eq + std::hash::Hash, V>(
    map: &HashMap<K, V>,
    kept: usize,
    settled: impl Fn(&K, &V) -> Option<u64>,
) -> Vec<K> {
    let mut settled = map
        .iter()
        .filter_map(|(key, value)| Some((settled(key, value)?, *key)))
        .collect::<Vec<_>>();
    if settled.len() <= kept {
        return Vec::new();
    }
    settled.sort_unstable_by_key(|(seq, _)| *seq);
    settled.truncate(settled.len() - kept);
    settled.into_iter().map(|(_, key)| key).collect()
}

/// What validation keeps of a committed model turn: its run, and the call
/// identity of each tool-call part by content index.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct CommittedTurn {
    run_id: RunId,
    tool_calls: Vec<Option<(ModelCallId, Option<ProviderItemId>)>>,
}

/// SHA-256 of an admitted input, which a later submission must match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InputDigest([u8; 32]);

impl Serialize for InputDigest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(self.0))
    }
}

impl<'de> Deserialize<'de> for InputDigest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)?;
        <[u8; 32]>::try_from(bytes)
            .map(Self)
            .map_err(|_| serde::de::Error::custom("a digest is 32 bytes"))
    }
}

fn input_digest(content: &cookie_agent_protocol::UserContent) -> InputDigest {
    use sha2::Digest as _;
    InputDigest(sha2::Sha256::digest(serde_json::to_vec(content).unwrap_or_default()).into())
}

impl ValidationState {
    fn new(taint: ValidationTaint) -> Self {
        Self {
            taint,
            runs: HashMap::new(),
            approval_owners: HashMap::new(),
            attempts: HashMap::new(),
            turns: HashMap::new(),
            turn_models: HashMap::new(),
            usage_turns: HashSet::new(),
            internal_runs: HashMap::new(),
            delegations: HashMap::new(),
            model_call_owners: HashMap::new(),
            provider_item_owners: HashMap::new(),
            tool_starts: HashMap::new(),
            terminated_tools: HashMap::new(),
            elided_tools: HashSet::new(),
            admissions: HashMap::new(),
            claims: HashMap::new(),
            questions: HashMap::new(),
            next_model_turn_seq: 1,
            previous_seq: None,
            previous_timestamp: None,
            active_run: None,
            record_count: 0,
            letting_go: LettingGo::default(),
            let_go: false,
            keep_all: false,
        }
    }

    /// A state that keeps everything until [`Self::finish_whole`]: a whole log's
    /// validation, where a record may name anything before it.
    fn whole(taint: ValidationTaint) -> Self {
        Self {
            keep_all: true,
            ..Self::new(taint)
        }
    }

    /// Ends a whole log's validation: from here on, what settled goes as it does
    /// for a log validated a record at a time.
    fn finish_whole(&mut self) {
        self.keep_all = false;
        if self.letting_go.lets_go() {
            self.let_go_settled();
        }
    }

    /// Lets go of what no valid record can still name, in a log that lets go
    /// ([`LettingGo`]) whose load found no damage
    /// (`docs/site/specs/forever-sessions.md` section 6): a damaged log's taint
    /// excuses records by what this holds, so it keeps all. A native model's log
    /// also keeps every call that ended since its context's start.
    ///
    /// What stays, and why each may still be named, is in section 12. In short:
    /// what is open stays, whatever its age: a run that has not ended or that a
    /// call, an approval, a delegation, an internal agent, a producer claim, or a
    /// question holds open; a call that has not ended; an approval, a delegation,
    /// or an internal agent that has not settled. Of what settled, the newest
    /// [`SETTLED_KEPT`] of each stay, the last [`TURNS_KEPT`] committed turns, and
    /// the last [`ADMISSIONS_KEPT`] admitted inputs. What goes is what settled
    /// before all of those, and a record naming it is refused: the floor.
    ///
    /// It depends only on what the state holds, and what it lets go of no later
    /// record can bring back, so letting go after every record, or once at a whole
    /// log's end, keeps the same.
    fn let_go_settled(&mut self) {
        self.let_go_settled_keeping(self.letting_go.calls_in_view_from());
    }

    /// [`Self::let_go_settled`], keeping the calls that ended at or after
    /// `in_view_from`, whatever their run, as a native model's compaction may
    /// still elide their output (section 12).
    fn let_go_settled_keeping(&mut self, in_view_from: Option<u64>) {
        if self.keep_all || self.taint != ValidationTaint::default() {
            return;
        }
        let in_view = |ended: u64| in_view_from.is_some_and(|from| ended >= from);
        let before = self.held();
        let floor = self.next_model_turn_seq.saturating_sub(TURNS_KEPT);
        self.turns.retain(|seq, _| *seq >= floor);
        self.turn_models.retain(|seq, _| *seq >= floor);
        self.usage_turns.retain(|seq| *seq >= floor);
        self.model_call_owners
            .retain(|_, owner| owner.model_turn_seq >= floor);
        self.provider_item_owners
            .retain(|_, owner| owner.model_turn_seq >= floor);
        for call in settled_beyond(&self.terminated_tools, SETTLED_KEPT, |_, seq| Some(*seq)) {
            if in_view(self.terminated_tools[&call]) {
                continue;
            }
            self.terminated_tools.remove(&call);
            self.tool_starts.remove(&call);
            self.elided_tools.remove(&call);
        }
        for approval in settled_beyond(&self.approval_owners, SETTLED_KEPT, |_, owner| {
            owner.settled_seq
        }) {
            self.approval_owners.remove(&approval);
        }
        for invocation in settled_beyond(&self.delegations, SETTLED_KEPT, |_, delegation| {
            delegation.finished_seq
        }) {
            self.delegations.remove(&invocation);
        }
        for internal in settled_beyond(&self.internal_runs, SETTLED_KEPT, |_, internal| {
            internal.finished_seq
        }) {
            self.internal_runs.remove(&internal);
        }
        // A run that ended is held open by what names it and has not settled.
        let mut held_open = HashSet::new();
        held_open.extend(
            self.tool_starts
                .iter()
                .filter(|(call, _)| !self.terminated_tools.contains_key(call))
                .map(|(_, (run, _))| *run),
        );
        held_open.extend(
            self.approval_owners
                .values()
                .filter(|owner| owner.settled_seq.is_none())
                .map(|owner| owner.run),
        );
        held_open.extend(
            self.delegations
                .values()
                .filter(|delegation| !delegation.finished)
                .map(|delegation| delegation.parent_run_id),
        );
        held_open.extend(
            self.internal_runs
                .values()
                .filter(|internal| internal.finished_seq.is_none())
                .map(|internal| internal.run_id),
        );
        held_open.extend(self.claims.values().copied());
        held_open.extend(self.questions.values().copied());
        held_open.extend(
            self.attempts
                .values()
                .filter(|attempt| !attempt.finished)
                .map(|attempt| attempt.run_id),
        );
        for run in settled_beyond(&self.runs, SETTLED_KEPT, |id, run| {
            run.ended_seq.filter(|_| !held_open.contains(id))
        }) {
            self.runs.remove(&run);
        }
        // An attempt goes with its run, and once it ended, as its run's do, but
        // for the one its run's next replay evaluation may name as its base.
        let runs = &self.runs;
        self.attempts
            .retain(|_, attempt| runs.contains_key(&attempt.run_id));
        let replay_base = |id: &AttemptId, run: &RunId| {
            runs.get(run).and_then(|run| run.replay_base) == Some(*id)
        };
        for attempt in settled_beyond(&self.attempts, SETTLED_KEPT, |id, attempt| {
            (attempt.finished && !replay_base(id, &attempt.run_id)).then_some(attempt.started_seq)
        }) {
            self.attempts.remove(&attempt);
        }
        // What names a run it no longer holds goes with it: what settled does, but
        // for a call still in view, which only an elision names, by its own run.
        let terminated = &self.terminated_tools;
        self.tool_starts.retain(|call, (run, _)| {
            runs.contains_key(run) || terminated.get(call).is_some_and(|ended| in_view(*ended))
        });
        self.terminated_tools
            .retain(|call, _| self.tool_starts.contains_key(call));
        self.elided_tools
            .retain(|call| self.tool_starts.contains_key(call));
        self.approval_owners
            .retain(|_, owner| runs.contains_key(&owner.run));
        self.internal_runs
            .retain(|_, internal| runs.contains_key(&internal.run_id));
        self.delegations
            .retain(|_, delegation| runs.contains_key(&delegation.parent_run_id));
        if self.admissions.len() > ADMISSIONS_KEPT {
            let mut seqs = self.admissions.keys().copied().collect::<Vec<_>>();
            seqs.sort_unstable();
            for seq in &seqs[..seqs.len() - ADMISSIONS_KEPT] {
                self.admissions.remove(seq);
            }
        }
        self.let_go |= self.held() != before;
    }

    /// Test-only: panics unless this state, which may have let go of what
    /// settled as it went, agrees with `whole`, the same log's validated whole,
    /// which let go only at its end, and only if the log was windowed then
    /// (`docs/site/specs/forever-sessions.md` section 12). What this holds, `whole`
    /// holds the same; and once each lets go of all that settled, they are equal,
    /// which says this let go of nothing it should have kept. For `native`, a log
    /// whose native model reads it without reading it whole first, they are equal
    /// once each lets go only of what that model's context no longer names.
    #[cfg(any(test, feature = "test-support"))]
    fn assert_agrees_with_whole(&self, whole: &Self, what: &str, native: bool) {
        fn within<K: Eq + std::hash::Hash, V: PartialEq>(
            held: &HashMap<K, V>,
            whole: &HashMap<K, V>,
        ) -> bool {
            held.iter()
                .all(|(key, value)| whole.get(key).is_some_and(|known| known == value))
        }
        fn set_within<K: Eq + std::hash::Hash>(held: &HashSet<K>, whole: &HashSet<K>) -> bool {
            held.iter().all(|key| whole.contains(key))
        }
        for (map, held) in [
            ("runs", within(&self.runs, &whole.runs)),
            (
                "approval owners",
                within(&self.approval_owners, &whole.approval_owners),
            ),
            ("attempts", within(&self.attempts, &whole.attempts)),
            ("turns", within(&self.turns, &whole.turns)),
            ("turn models", within(&self.turn_models, &whole.turn_models)),
            (
                "usage turns",
                set_within(&self.usage_turns, &whole.usage_turns),
            ),
            (
                "internal runs",
                within(&self.internal_runs, &whole.internal_runs),
            ),
            ("delegations", within(&self.delegations, &whole.delegations)),
            (
                "model call owners",
                within(&self.model_call_owners, &whole.model_call_owners),
            ),
            (
                "provider item owners",
                within(&self.provider_item_owners, &whole.provider_item_owners),
            ),
            ("tool starts", within(&self.tool_starts, &whole.tool_starts)),
            (
                "terminated tools",
                within(&self.terminated_tools, &whole.terminated_tools),
            ),
            (
                "elided tools",
                set_within(&self.elided_tools, &whole.elided_tools),
            ),
            ("admissions", within(&self.admissions, &whole.admissions)),
        ] {
            assert!(
                held,
                "the validation state's {map} hold what {what}'s do not, or otherwise"
            );
        }
        // What a native model's view keeps of calls an ACP agent's window already
        // let go of a whole read still holds, so the two compare without it.
        let settled = |state: &Self| {
            let mut state = state.clone();
            state.keep_all = false;
            state.let_go_settled_keeping(None);
            state
        };
        assert!(
            settled(self) == settled(whole),
            "the validation state differs from {what} once each lets go of what settled"
        );
        // The calls in view of a native model's context, which its compaction may
        // still elide, stay as a whole read keeps them.
        let in_view = |state: &Self| {
            let mut state = state.clone();
            state.keep_all = false;
            if state.letting_go.lets_go() {
                state.let_go_settled();
            }
            state
        };
        assert!(
            !native || in_view(self) == in_view(whole),
            "the validation state differs from {what} in what a native model's context names"
        );
    }

    /// Whether [`Self::let_go_settled`] could let go of anything: a cheap check,
    /// true whenever it could, on maps it keeps small.
    fn may_let_go(&self) -> bool {
        let settled = |count: usize| count > SETTLED_KEPT;
        let in_view_from = self.letting_go.calls_in_view_from();
        let in_view = |ended: u64| in_view_from.is_some_and(|from| ended >= from);
        // The calls in view are those that ended last, as are those kept anyway.
        let calls_in_view = match in_view_from {
            Some(_) => self
                .terminated_tools
                .values()
                .filter(|ended| in_view(**ended))
                .count(),
            None => 0,
        };
        self.turns.len() as u64 > TURNS_KEPT
            || self.admissions.len() > ADMISSIONS_KEPT
            || self.terminated_tools.len() > SETTLED_KEPT.max(calls_in_view)
            || settled(
                self.approval_owners
                    .values()
                    .filter(|owner| owner.settled_seq.is_some())
                    .count(),
            )
            || settled(
                self.delegations
                    .values()
                    .filter(|delegation| delegation.finished_seq.is_some())
                    .count(),
            )
            || settled(
                self.internal_runs
                    .values()
                    .filter(|internal| internal.finished_seq.is_some())
                    .count(),
            )
            || settled(
                self.runs
                    .values()
                    .filter(|run| run.ended_seq.is_some())
                    .count(),
            )
            || settled(
                self.attempts
                    .values()
                    .filter(|attempt| attempt.finished)
                    .count(),
            )
            // What a run that went leaves.
            || self
                .attempts
                .values()
                .any(|attempt| !self.runs.contains_key(&attempt.run_id))
            || self.tool_starts.iter().any(|(call, (run, _))| {
                !self.runs.contains_key(run)
                    && !self
                        .terminated_tools
                        .get(call)
                        .is_some_and(|ended| in_view(*ended))
            })
    }

    /// How many entries its maps hold, to tell whether a letting go let go.
    fn held(&self) -> usize {
        self.runs.len()
            + self.approval_owners.len()
            + self.attempts.len()
            + self.turns.len()
            + self.turn_models.len()
            + self.usage_turns.len()
            + self.internal_runs.len()
            + self.delegations.len()
            + self.model_call_owners.len()
            + self.provider_item_owners.len()
            + self.tool_starts.len()
            + self.terminated_tools.len()
            + self.elided_tools.len()
            + self.admissions.len()
    }

    /// Notes what `record`, which validated, settles or holds open.
    fn note_settling(&mut self, record: &StoredEvent) {
        match &record.payload {
            EventPayload::RunFinished { .. } => {
                if let Some(run) = record.run_id.and_then(|run| self.runs.get_mut(&run)) {
                    run.ended_seq.get_or_insert(record.seq);
                }
            }
            EventPayload::InternalAgentFinished {
                internal_run_id, ..
            } => {
                if let Some(internal) = self.internal_runs.get_mut(internal_run_id) {
                    internal.finished_seq.get_or_insert(record.seq);
                }
            }
            EventPayload::ApprovalFinalized { approval_id, .. } => {
                if let Some(owner) = self.approval_owners.get_mut(approval_id) {
                    owner.settled_seq.get_or_insert(record.seq);
                }
            }
            EventPayload::DelegationFinished { invocation_id, .. } => {
                if let Some(delegation) = self.delegations.get_mut(invocation_id) {
                    delegation.finished_seq.get_or_insert(record.seq);
                }
            }
            EventPayload::ProducerMessagesClaimed { .. } => {
                if let Some(run) = record.run_id {
                    self.claims.insert(record.seq, run);
                }
            }
            EventPayload::ProducerMessagesReleased { claim_seq } => {
                self.claims.remove(claim_seq);
            }
            EventPayload::ElicitationRequested { request } => {
                if let Some(run) = record.run_id {
                    self.questions.insert(request.elicitation_id, run);
                }
            }
            EventPayload::ElicitationResolved { resolution } => {
                self.questions.remove(&resolution.elicitation_id);
            }
            _ => {}
        }
    }

    fn finish_record(&mut self, record: &StoredEvent) {
        self.previous_seq = Some(record.seq);
        self.previous_timestamp = Some(record.timestamp);
        self.record_count += 1;
    }
}

impl Default for ValidationState {
    fn default() -> Self {
        Self::new(ValidationTaint::default())
    }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
struct ValidationTaint {
    broad: Vec<(u64, u64)>,
    #[serde(with = "crate::events::sorted::map")]
    runs: HashMap<RunId, u64>,
    #[serde(with = "crate::events::sorted::map")]
    attempts: HashMap<AttemptId, u64>,
    #[serde(with = "crate::events::sorted::map")]
    attempt_runs: HashMap<RunId, u64>,
    #[serde(with = "crate::events::sorted::map")]
    turns: HashMap<u64, u64>,
    #[serde(with = "crate::events::sorted::map")]
    tools: HashMap<ToolCallId, u64>,
    #[serde(with = "crate::events::sorted::map")]
    approvals: HashMap<cookie_agent_protocol::ApprovalId, u64>,
    #[serde(with = "crate::events::sorted::map")]
    internal_runs: HashMap<cookie_agent_protocol::InternalAgentRunId, u64>,
    #[serde(with = "crate::events::sorted::map")]
    internal_model_phases: HashMap<cookie_agent_protocol::InternalAgentRunId, Vec<u64>>,
    #[serde(with = "crate::events::sorted::map")]
    internal_usage_phases: HashMap<cookie_agent_protocol::InternalAgentRunId, Vec<u64>>,
    #[serde(with = "crate::events::sorted::map")]
    tool_terminals: HashMap<ToolCallId, u64>,
    #[serde(with = "crate::events::sorted::map")]
    admissions: HashMap<u64, u64>,
    #[serde(with = "crate::events::sorted::map")]
    delegations: HashMap<cookie_agent_protocol::InvocationId, u64>,
    #[serde(with = "crate::events::sorted::map")]
    delegation_repairs: HashMap<cookie_agent_protocol::InvocationId, u64>,
    #[serde(with = "crate::events::sorted::map")]
    run_ordering: HashMap<RunId, u64>,
    turn_ordering: Option<u64>,
    active_run_ordering: Option<u64>,
}

const MAX_INTERNAL_PHASE_TAINTS_PER_RUN: usize = 64;
const INTERNAL_PHASE_TAINT_LIMIT_MESSAGE: &str =
    "internal-agent phase taint history exceeds the 64-transition per-run limit";

impl ValidationTaint {
    /// Forgets damage recorded after `through_seq`, which a truncation removed.
    fn truncate(&mut self, through_seq: u64) {
        let kept = |seq: &u64| *seq <= through_seq;
        self.broad.retain(|(start, _)| kept(start));
        for (_, end) in &mut self.broad {
            *end = (*end).min(through_seq);
        }
        self.runs.retain(|_, seq| kept(seq));
        self.attempts.retain(|_, seq| kept(seq));
        self.attempt_runs.retain(|_, seq| kept(seq));
        self.turns.retain(|_, seq| kept(seq));
        self.tools.retain(|_, seq| kept(seq));
        self.approvals.retain(|_, seq| kept(seq));
        self.internal_runs.retain(|_, seq| kept(seq));
        for phases in self
            .internal_model_phases
            .values_mut()
            .chain(self.internal_usage_phases.values_mut())
        {
            phases.retain(kept);
        }
        self.internal_model_phases
            .retain(|_, phases| !phases.is_empty());
        self.internal_usage_phases
            .retain(|_, phases| !phases.is_empty());
        self.tool_terminals.retain(|_, seq| kept(seq));
        self.admissions.retain(|_, seq| kept(seq));
        self.delegations.retain(|_, seq| kept(seq));
        self.delegation_repairs.retain(|_, seq| kept(seq));
        self.run_ordering.retain(|_, seq| kept(seq));
        self.turn_ordering = self.turn_ordering.filter(kept);
        self.active_run_ordering = self.active_run_ordering.filter(kept);
    }

    fn broad_before(&self, seq: u64) -> bool {
        self.broad.iter().any(|(start, _)| *start < seq)
    }

    fn keyed_before<K: Eq + std::hash::Hash>(
        &self,
        taints: &HashMap<K, u64>,
        key: &K,
        seq: u64,
    ) -> bool {
        self.broad_before(seq) || taints.get(key).is_some_and(|tainted| *tainted < seq)
    }

    fn run_before(&self, run_id: RunId, seq: u64) -> bool {
        self.keyed_before(&self.runs, &run_id, seq)
    }

    fn attempt_before(&self, attempt_id: AttemptId, seq: u64) -> bool {
        self.keyed_before(&self.attempts, &attempt_id, seq)
    }

    fn turn_before(&self, model_turn_seq: u64, seq: u64) -> bool {
        self.keyed_before(&self.turns, &model_turn_seq, seq)
    }

    fn tool_before(&self, tool_call_id: ToolCallId, seq: u64) -> bool {
        self.keyed_before(&self.tools, &tool_call_id, seq)
    }

    fn approval_before(&self, approval_id: cookie_agent_protocol::ApprovalId, seq: u64) -> bool {
        self.keyed_before(&self.approvals, &approval_id, seq)
    }

    fn internal_run_before(
        &self,
        internal_run_id: cookie_agent_protocol::InternalAgentRunId,
        seq: u64,
    ) -> bool {
        self.keyed_before(&self.internal_runs, &internal_run_id, seq)
    }

    fn latest_broad_before(&self, seq: u64) -> Option<u64> {
        self.broad
            .iter()
            .filter(|(start, _)| *start < seq)
            .map(|(_, end)| *end)
            .max()
    }

    fn internal_model_phase_taint_before(
        &self,
        internal_run_id: cookie_agent_protocol::InternalAgentRunId,
        seq: u64,
    ) -> Option<u64> {
        self.latest_broad_before(seq).max(
            self.internal_model_phases
                .get(&internal_run_id)
                .into_iter()
                .flatten()
                .copied()
                .filter(|tainted| *tainted < seq)
                .max(),
        )
    }

    fn internal_usage_phase_taint_before(
        &self,
        internal_run_id: cookie_agent_protocol::InternalAgentRunId,
        seq: u64,
    ) -> Option<u64> {
        self.latest_broad_before(seq).max(
            self.internal_usage_phases
                .get(&internal_run_id)
                .into_iter()
                .flatten()
                .copied()
                .filter(|tainted| *tainted < seq)
                .max(),
        )
    }

    fn tool_terminal_before(&self, tool_call_id: ToolCallId, seq: u64) -> bool {
        self.keyed_before(&self.tool_terminals, &tool_call_id, seq)
    }

    fn admission_before(&self, admission_seq: u64, seq: u64) -> bool {
        self.keyed_before(&self.admissions, &admission_seq, seq)
    }

    fn delegation_before(
        &self,
        invocation_id: cookie_agent_protocol::InvocationId,
        seq: u64,
    ) -> bool {
        self.broad_before(seq)
            || self.delegations.get(&invocation_id).is_some_and(|tainted| {
                *tainted < seq
                    && !self
                        .delegation_repairs
                        .get(&invocation_id)
                        .is_some_and(|repair| *repair >= *tainted && *repair <= seq)
            })
    }

    fn delegation_unrepaired_before(
        &self,
        invocation_id: cookie_agent_protocol::InvocationId,
        seq: u64,
    ) -> bool {
        self.broad_before(seq)
            || self
                .delegations
                .get(&invocation_id)
                .is_some_and(|tainted| *tainted < seq)
    }

    fn run_ordering_between(&self, run_id: RunId, start_seq: u64, seq: u64) -> bool {
        self.broad
            .iter()
            .any(|(start, end)| *start < seq && *end > start_seq)
            || self
                .run_ordering
                .get(&run_id)
                .is_some_and(|tainted| *tainted > start_seq && *tainted < seq)
            || self
                .attempt_runs
                .get(&run_id)
                .is_some_and(|tainted| *tainted > start_seq && *tainted < seq)
    }

    fn turn_ordering_before(&self, seq: u64) -> bool {
        self.broad_before(seq) || self.turn_ordering.is_some_and(|tainted| tainted < seq)
    }

    fn active_run_ordering_before(&self, seq: u64) -> bool {
        self.broad_before(seq)
            || self
                .active_run_ordering
                .is_some_and(|tainted| tainted < seq)
    }

    fn mark_broad(&mut self, start: u64, end: u64) {
        self.broad.push((start, end));
    }

    fn mark_record(
        &mut self,
        seq: u64,
        run_id: Option<RunId>,
        payload: &Value,
    ) -> Result<(), &'static str> {
        match payload.get("type").and_then(Value::as_str) {
            Some("run_started") => {
                if let Some(run_id) = run_id {
                    self.runs.entry(run_id).or_insert(seq);
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
                self.active_run_ordering.get_or_insert(seq);
            }
            Some("model_attempt_started") => {
                if let Some(attempt_id) = json_field(payload, "attempt_id") {
                    self.attempts.entry(attempt_id).or_insert(seq);
                }
                if let Some(run_id) = run_id {
                    self.attempt_runs.entry(run_id).or_insert(seq);
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            Some("attempt_abandoned") | Some("model_fallback") => {
                if let Some(run_id) = run_id {
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            Some("model_turn_committed") => {
                if let Some(model_turn_seq) = payload.get("model_turn_seq").and_then(Value::as_u64)
                {
                    self.turns.entry(model_turn_seq).or_insert(seq);
                }
                self.turn_ordering.get_or_insert(seq);
                if let Some(run_id) = run_id {
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            Some("tool_call_started") => {
                if let Some(tool_call_id) = json_field(payload, "tool_call_id") {
                    self.tools.entry(tool_call_id).or_insert(seq);
                }
            }
            Some("tool_call_terminated") => {
                if let Some(tool_call_id) = json_field(payload, "tool_call_id") {
                    self.tool_terminals.entry(tool_call_id).or_insert(seq);
                }
            }
            Some("approval_requested") => {
                if let Some(approval_id) = payload
                    .get("request")
                    .and_then(|request| json_field(request, "approval_id"))
                {
                    self.approvals.entry(approval_id).or_insert(seq);
                }
            }
            Some("internal_agent_started") => {
                if let Some(internal_run_id) = json_field(payload, "internal_run_id") {
                    self.internal_runs.entry(internal_run_id).or_insert(seq);
                }
            }
            Some("internal_agent_fallback") => {
                if let Some(internal_run_id) = json_field(payload, "internal_run_id") {
                    self.mark_internal_fallback(internal_run_id, seq)?;
                }
            }
            Some("internal_agent_usage_recorded") => {
                if let Some(internal_run_id) = json_field(payload, "internal_run_id") {
                    self.mark_internal_usage(internal_run_id, seq)?;
                }
            }
            Some("user_input_admitted") => {
                self.admissions.entry(seq).or_insert(seq);
            }
            Some("delegation_reserved" | "delegation_progressed" | "delegation_finished") => {
                if let Some(invocation_id) = delegation_invocation_from_value(payload) {
                    self.delegations.entry(invocation_id).or_insert(seq);
                }
            }
            Some("run_finished") => {
                self.active_run_ordering.get_or_insert(seq);
            }
            _ => {}
        }
        Ok(())
    }

    fn mark_event(&mut self, event: &StoredEvent) -> Result<(), &'static str> {
        let seq = event.seq;
        match &event.payload {
            EventPayload::RunStarted { .. } => {
                if let Some(run_id) = event.run_id {
                    self.runs.entry(run_id).or_insert(seq);
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
                self.active_run_ordering.get_or_insert(seq);
            }
            EventPayload::ModelAttemptStarted { attempt_id, .. } => {
                self.attempts.entry(*attempt_id).or_insert(seq);
                if let Some(run_id) = event.run_id {
                    self.attempt_runs.entry(run_id).or_insert(seq);
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            EventPayload::AttemptAbandoned { .. } | EventPayload::ModelFallback { .. } => {
                if let Some(run_id) = event.run_id {
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            EventPayload::ModelTurnCommitted { model_turn_seq, .. } => {
                self.turns.entry(*model_turn_seq).or_insert(seq);
                self.turn_ordering.get_or_insert(seq);
                if let Some(run_id) = event.run_id {
                    self.run_ordering.entry(run_id).or_insert(seq);
                }
            }
            EventPayload::ToolCallStarted { start } => {
                self.tools.entry(start.tool_call_id).or_insert(seq);
            }
            EventPayload::ToolCallTerminated { termination } => {
                self.tool_terminals
                    .entry(termination.tool_call_id)
                    .or_insert(seq);
            }
            EventPayload::ApprovalRequested { request } => {
                self.approvals.entry(request.approval_id()).or_insert(seq);
            }
            EventPayload::InternalAgentStarted {
                internal_run_id, ..
            } => {
                self.internal_runs.entry(*internal_run_id).or_insert(seq);
            }
            EventPayload::InternalAgentFallback {
                internal_run_id, ..
            } => {
                self.mark_internal_fallback(*internal_run_id, seq)?;
            }
            EventPayload::InternalAgentUsageRecorded {
                internal_run_id, ..
            } => {
                self.mark_internal_usage(*internal_run_id, seq)?;
            }
            EventPayload::UserInputAdmitted { .. } => {
                self.admissions.entry(seq).or_insert(seq);
            }
            EventPayload::DelegationReserved { reservation, .. } => {
                self.delegations
                    .entry(reservation.invocation_id)
                    .or_insert(seq);
            }
            EventPayload::DelegationProgressed { invocation_id, .. }
            | EventPayload::DelegationFinished { invocation_id, .. } => {
                self.delegations.entry(*invocation_id).or_insert(seq);
            }
            EventPayload::RunFinished { .. } => {
                self.active_run_ordering.get_or_insert(seq);
            }
            _ => {}
        }
        Ok(())
    }

    fn mark_internal_fallback(
        &mut self,
        internal_run_id: cookie_agent_protocol::InternalAgentRunId,
        seq: u64,
    ) -> Result<(), &'static str> {
        let model_len = self
            .internal_model_phases
            .get(&internal_run_id)
            .map_or(0, Vec::len);
        let usage_len = self
            .internal_usage_phases
            .get(&internal_run_id)
            .map_or(0, Vec::len);
        if model_len >= MAX_INTERNAL_PHASE_TAINTS_PER_RUN
            || usage_len >= MAX_INTERNAL_PHASE_TAINTS_PER_RUN
        {
            return Err(INTERNAL_PHASE_TAINT_LIMIT_MESSAGE);
        }
        self.internal_model_phases
            .entry(internal_run_id)
            .or_default()
            .push(seq);
        self.internal_usage_phases
            .entry(internal_run_id)
            .or_default()
            .push(seq);
        Ok(())
    }

    fn mark_internal_usage(
        &mut self,
        internal_run_id: cookie_agent_protocol::InternalAgentRunId,
        seq: u64,
    ) -> Result<(), &'static str> {
        let usage = self
            .internal_usage_phases
            .entry(internal_run_id)
            .or_default();
        if usage.len() >= MAX_INTERNAL_PHASE_TAINTS_PER_RUN {
            return Err(INTERNAL_PHASE_TAINT_LIMIT_MESSAGE);
        }
        usage.push(seq);
        Ok(())
    }
}

fn json_field<T: for<'de> Deserialize<'de>>(value: &Value, field: &str) -> Option<T> {
    serde_json::from_value(value.get(field)?.clone()).ok()
}

fn delegation_invocation_from_value(
    payload: &Value,
) -> Option<cookie_agent_protocol::InvocationId> {
    match payload.get("type").and_then(Value::as_str) {
        Some("delegation_reserved") => payload
            .get("reservation")
            .and_then(|reservation| json_field(reservation, "invocation_id")),
        Some("delegation_progressed" | "delegation_finished") => {
            json_field(payload, "invocation_id")
        }
        _ => None,
    }
}

fn delegation_invocation_from_event(
    payload: &EventPayload,
) -> Option<cookie_agent_protocol::InvocationId> {
    match payload {
        EventPayload::DelegationReserved { reservation, .. } => Some(reservation.invocation_id),
        EventPayload::DelegationProgressed { invocation_id, .. }
        | EventPayload::DelegationFinished { invocation_id, .. } => Some(*invocation_id),
        _ => None,
    }
}

/// Each record that repeats an identity an earlier record holds, by index,
/// with what it repeats. Validation skips tainted records, so this pass looks
/// at every record regardless.
fn duplicate_records<E: Borrow<StoredEvent>>(records: &[E]) -> Vec<(usize, &'static str)> {
    let mut duplicates = Vec::new();
    let mut runs = HashSet::new();
    let mut attempts = HashSet::new();
    let mut turns = HashSet::new();
    let mut usage_turns = HashSet::new();
    let mut internal_runs = HashSet::new();
    let mut tool_starts = HashSet::new();
    let mut tool_terminations = HashSet::new();
    let mut approvals = HashSet::new();
    let mut model_calls = HashSet::new();
    let mut provider_items = HashSet::new();
    for (index, record) in records.iter().enumerate() {
        let record: &StoredEvent = record.borrow();
        let mut duplicate = |message| duplicates.push((index, message));
        match &record.payload {
            EventPayload::RunStarted { .. } => {
                let Some(run_id) = record.run_id else {
                    duplicate("RunStarted is missing run_id");
                    continue;
                };
                if !runs.insert(run_id) {
                    duplicate("run_id has more than one RunStarted event");
                }
            }
            EventPayload::ModelAttemptStarted { attempt_id, .. } => {
                if !attempts.insert(*attempt_id) {
                    duplicate("attempt_id has more than one ModelAttemptStarted");
                }
            }
            EventPayload::ModelTurnCommitted {
                model_turn_seq,
                turn,
                ..
            } => {
                if !turns.insert(*model_turn_seq) {
                    duplicate("model turn sequence is duplicated");
                }
                if let Some(run_id) = record.run_id {
                    for part in &turn.content {
                        if let cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                            id,
                            provider_item_id,
                            ..
                        } = part
                        {
                            if !model_calls.insert((run_id, id.clone())) {
                                duplicate("model call id is reused within a run");
                            }
                            if let Some(provider_item_id) = provider_item_id
                                && !provider_items.insert((run_id, provider_item_id.clone()))
                            {
                                duplicate("provider item id is reused within a run");
                            }
                        }
                    }
                }
            }
            EventPayload::ModelUsageRecorded { model_turn_seq, .. } => {
                if !usage_turns.insert(*model_turn_seq) {
                    duplicate("usage ownership does not match its model turn");
                }
            }
            EventPayload::InternalAgentStarted {
                internal_run_id, ..
            } => {
                if !internal_runs.insert(*internal_run_id) {
                    duplicate("internal_run_id has more than one start");
                }
            }
            EventPayload::ToolCallStarted { start } => {
                if !tool_starts.insert(start.tool_call_id) {
                    duplicate("tool_call_id has more than one start");
                }
            }
            EventPayload::ToolCallTerminated { termination } => {
                if !tool_terminations.insert(termination.tool_call_id) {
                    duplicate("tool call has more than one terminal event");
                }
            }
            EventPayload::ApprovalRequested { request }
                if !approvals.insert(request.approval_id()) =>
            {
                duplicate("approval_id has more than one ApprovalRequested event");
            }
            _ => {}
        }
    }
    duplicates
}

#[cfg(test)]
fn validate_observed_duplicates<E: Borrow<StoredEvent>>(
    path: &Path,
    records: &[E],
) -> Result<(), EventLogError> {
    match duplicate_records(records).first() {
        Some((_, message)) => corrupt(path, *message),
        None => Ok(()),
    }
}

fn validate_record_local(path: &Path, record: &StoredEvent) -> Result<(), EventLogError> {
    if record.payload.is_transient() {
        return corrupt(path, "live-only stream output is never stored");
    }
    match &record.payload {
        EventPayload::SessionPermissionOverlaySet { .. } if record.run_id.is_some() => {
            return corrupt(path, "SessionPermissionOverlaySet must not have run_id");
        }
        EventPayload::SessionToolExposureSet { .. } if record.run_id.is_some() => {
            return corrupt(path, "SessionToolExposureSet must not have run_id");
        }
        EventPayload::DelegateChildTerminated { .. } if record.run_id.is_some() => {
            return corrupt(path, "DelegateChildTerminated must not have run_id");
        }
        EventPayload::DelegatedContextSeeded { .. } if record.run_id.is_some() => {
            return corrupt(path, "DelegatedContextSeeded must be runless");
        }
        EventPayload::SessionTitleCommitted { change, .. } => {
            let runless = matches!(
                change,
                cookie_agent_protocol::SessionTitleChange::UserSet { .. }
                    | cookie_agent_protocol::SessionTitleChange::UserClear { .. }
                    | cookie_agent_protocol::SessionTitleChange::UserReset { .. }
                    | cookie_agent_protocol::SessionTitleChange::DelegatedSet { .. }
            );
            if runless != record.run_id.is_none() {
                return corrupt(path, "SessionTitleCommitted has inconsistent run ownership");
            }
        }
        _ => {}
    }
    Ok(())
}

/// The most records a load reports one by one as quarantined; the rest are
/// counted in one diagnostic.
const MAX_QUARANTINE_DIAGNOSTICS: usize = 64;

/// Validates a log as it loads. A record that fails a cross-event check is
/// *quarantined* instead of failing the open: it is left out, its identity
/// is tainted as a skipped record's is (so the records that depend on it are
/// excused the same way), and a diagnostic names it. Only `session_created`
/// cannot be quarantined. Returns the records kept and their validation
/// state, and adds the quarantined records' taint to `taint`, which later
/// validation starts from.
fn validate_loaded(
    path: &Path,
    session_id: SessionId,
    records: Vec<Arc<StoredEvent>>,
    taint: &mut ValidationTaint,
    diagnostics: &mut Vec<EventLoadDiagnostic>,
) -> Result<(Vec<Arc<StoredEvent>>, ValidationState), EventLogError> {
    let mut refused = duplicate_records(&records).into_iter().peekable();
    let mut accepted: Vec<Arc<StoredEvent>> = Vec::with_capacity(records.len());
    let mut state = ValidationState::whole(taint.clone());
    let mut quarantined = 0_usize;
    for (index, record) in records.into_iter().enumerate() {
        let mut duplicate = None;
        while let Some((_, message)) = refused.next_if(|(at, _)| *at == index) {
            duplicate.get_or_insert(message);
        }
        let result = match duplicate {
            Some(message) => corrupt(path, message),
            None => validate_record_incremental(path, session_id, &record, &mut state, false),
        };
        let error = match result {
            Ok(()) => {
                accepted.push(record);
                continue;
            }
            Err(error) if accepted.is_empty() => return Err(error),
            Err(error) => error,
        };
        quarantined += 1;
        if quarantined <= MAX_QUARANTINE_DIAGNOSTICS {
            diagnostics.push(EventLoadDiagnostic {
                seq: record.seq,
                reason: format!(
                    "quarantined: {}",
                    match &error {
                        EventLogError::Corrupt { message, .. } => message.clone(),
                        error => error.to_string(),
                    }
                ),
                engine_version: record.engine_version.clone(),
                skipped: true,
            });
        }
        // A record past the per-run phase-taint bound taints everything
        // after it instead.
        if taint.mark_event(&record).is_err() {
            taint.mark_broad(record.seq, record.seq);
        }
        // The failed check may have changed the state before it failed; the
        // records before it validate as they did, since taint only reaches
        // later records.
        state = rebuild_whole(path, session_id, &accepted, taint)?;
    }
    state.finish_whole();
    if quarantined > MAX_QUARANTINE_DIAGNOSTICS {
        diagnostics.push(EventLoadDiagnostic {
            seq: accepted.last().map_or(1, |record| record.seq),
            reason: format!(
                "{} more inconsistent records were quarantined",
                quarantined - MAX_QUARANTINE_DIAGNOSTICS
            ),
            engine_version: None,
            skipped: false,
        });
    }
    Ok((accepted, state))
}

/// The validation state of records that already passed [`validate_loaded`]
/// as a prefix of the log (what a revert keeps, or what was there before an
/// append that failed). Duplicates cannot appear in such a prefix, so only
/// the per-record pass that builds the state runs.
fn rebuild_validation<E: Borrow<StoredEvent>>(
    path: &Path,
    session_id: SessionId,
    records: &[E],
    initial_taint: &ValidationTaint,
) -> Result<ValidationState, EventLogError> {
    let mut state = rebuild_whole(path, session_id, records, initial_taint)?;
    state.finish_whole();
    Ok(state)
}

/// [`rebuild_validation`], still keeping everything.
fn rebuild_whole<E: Borrow<StoredEvent>>(
    path: &Path,
    session_id: SessionId,
    records: &[E],
    initial_taint: &ValidationTaint,
) -> Result<ValidationState, EventLogError> {
    let mut state = ValidationState::whole(initial_taint.clone());
    for record in records {
        validate_record_incremental(path, session_id, record.borrow(), &mut state, false)?;
    }
    Ok(state)
}

/// Validates `record` as the next of a log whose validation is `state`, and folds
/// it in. A windowed log lets go of what settled as it goes, so a record naming
/// what it let go of is refused: older than the floor
/// (`docs/site/specs/forever-sessions.md` sections 6 and 11).
fn validate_record_incremental(
    path: &Path,
    session_id: SessionId,
    record: &StoredEvent,
    state: &mut ValidationState,
    strict: bool,
) -> Result<(), EventLogError> {
    if let Err(error) = validate_record(path, session_id, record, state, strict) {
        return Err(match error {
            EventLogError::Corrupt { path, message } if state.let_go => EventLogError::Corrupt {
                path,
                message: format!(
                    "{message} (or it names what settled before the validation floor, \
                     which this log no longer holds)"
                ),
            },
            error => error,
        });
    }
    state.note_settling(record);
    state.letting_go.observe(record);
    // After every record, so what it holds is what letting go once at the end of
    // a whole validation keeps, wherever the log ends.
    if state.letting_go.lets_go() && state.may_let_go() {
        state.let_go_settled();
    }
    Ok(())
}

fn validate_record(
    path: &Path,
    session_id: SessionId,
    record: &StoredEvent,
    state: &mut ValidationState,
    strict: bool,
) -> Result<(), EventLogError> {
    if state
        .previous_seq
        .is_some_and(|previous| record.seq <= previous)
    {
        return corrupt(
            path,
            format!(
                "event sequence {} is not strictly greater than {}",
                record.seq,
                state.previous_seq.expect("checked previous sequence")
            ),
        );
    }
    if record.session_id != session_id {
        return corrupt(
            path,
            "event envelope session ID does not match its directory",
        );
    }
    if state
        .previous_timestamp
        .is_some_and(|timestamp| record.timestamp < timestamp)
    {
        return corrupt(path, "event timestamps are not monotonic");
    }
    if state.record_count == 0 {
        let EventPayload::SessionCreated { .. } = &record.payload else {
            return Err(EventLogError::MissingCreation(path.to_owned()));
        };
        if record.run_id.is_some() {
            return corrupt(path, "invalid initial SessionCreated record");
        }
        state.finish_record(record);
        return Ok(());
    }
    if matches!(record.payload, EventPayload::SessionCreated { .. }) {
        return corrupt(path, "SessionCreated appeared after sequence 1");
    }
    validate_record_local(path, record)?;
    let ValidationState {
        taint,
        runs,
        approval_owners,
        attempts,
        turns,
        turn_models,
        usage_turns,
        internal_runs,
        delegations,
        model_call_owners,
        provider_item_owners,
        tool_starts,
        terminated_tools,
        elided_tools,
        admissions,
        next_model_turn_seq,
        active_run,
        let_go,
        ..
    } = state;
    if let EventPayload::UserInputAdmitted { content } = &record.payload {
        admissions.insert(record.seq, (record.run_id, input_digest(content)));
    }
    let missing_admission =
        |user_input_seq: u64,
         run_id: Option<RunId>,
         content: &cookie_agent_protocol::UserContent| {
            !admissions
                .get(&user_input_seq)
                .is_some_and(|(owner, admitted)| {
                    *owner == run_id && *admitted == input_digest(content)
                })
        };
    let tainted_prerequisite = if strict {
        false
    } else {
        match &record.payload {
            EventPayload::SkillLoaded { .. } | EventPayload::SkillInvocationNoted { .. }
                if record.run_id.is_some() =>
            {
                record.run_id.is_some_and(|run_id| {
                    !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                })
            }
            EventPayload::SessionTitleCommitted { .. } if record.run_id.is_some() => {
                record.run_id.is_some_and(|run_id| {
                    !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                })
            }
            EventPayload::UserInputRecalled {
                user_input_seq,
                content,
            } => {
                missing_admission(*user_input_seq, record.run_id, content)
                    && taint.admission_before(*user_input_seq, record.seq)
            }
            EventPayload::ModelAttemptStarted { .. }
            | EventPayload::InternalAgentStarted { .. }
            | EventPayload::ModelFallback { .. }
            | EventPayload::ApprovalRequested { .. } => record.run_id.is_some_and(|run_id| {
                !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
            }),
            EventPayload::ModelRequestPrepared { attempt_id, .. }
            | EventPayload::ModelToolsPublished { attempt_id, .. }
            | EventPayload::ModelOutputStarted { attempt_id, .. }
            | EventPayload::AttemptAbandoned { attempt_id, .. }
            | EventPayload::ModelReplayEvaluated { attempt_id, .. }
            | EventPayload::ModelTurnCommitted { attempt_id, .. } => {
                (!attempts.contains_key(attempt_id)
                    && taint.attempt_before(*attempt_id, record.seq))
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::ModelUsageRecorded { model_turn_seq, .. } => {
                (!turns.contains_key(model_turn_seq)
                    && taint.turn_before(*model_turn_seq, record.seq))
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::InternalAgentFallback {
                internal_run_id, ..
            }
            | EventPayload::InternalAgentUsageRecorded {
                internal_run_id, ..
            } => {
                (!internal_runs.contains_key(internal_run_id)
                    && taint.internal_run_before(*internal_run_id, record.seq))
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::ToolCallStarted { start } => {
                (!turns.contains_key(&start.owner.model_turn_seq)
                    && taint.turn_before(start.owner.model_turn_seq, record.seq))
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::ToolCallTerminated { termination } => {
                !tool_starts.contains_key(&termination.tool_call_id)
                    && taint.tool_before(termination.tool_call_id, record.seq)
            }
            EventPayload::ToolOutputElided { tool_call_id, .. }
            | EventPayload::ToolCallLinked { tool_call_id, .. } => {
                !tool_starts.contains_key(tool_call_id)
                    && taint.tool_before(*tool_call_id, record.seq)
            }
            EventPayload::ApprovalStep { approval_id, .. }
            | EventPayload::ApprovalFinalized { approval_id, .. } => {
                !approval_owners.contains_key(approval_id)
                    && taint.approval_before(*approval_id, record.seq)
            }
            EventPayload::TreeApprovalGrantCommitted { grant } => {
                !approval_owners.contains_key(&grant.approval_id)
                    && taint.approval_before(grant.approval_id, record.seq)
            }
            EventPayload::DelegationReserved { reservation, .. } => {
                taint.delegation_before(reservation.invocation_id, record.seq)
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::DelegationProgressed { invocation_id, .. } => {
                taint.delegation_before(*invocation_id, record.seq)
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            EventPayload::DelegationFinished {
                invocation_id,
                child_session_id,
                child_run_id,
                ..
            } => {
                let repair_matches = delegations.get(invocation_id).is_some_and(|delegation| {
                    record.run_id == Some(delegation.parent_run_id)
                        && *child_session_id == delegation.child_session_id
                        && *child_run_id == delegation.child_run_id
                        && !delegation.finished
                });
                (taint.delegation_before(*invocation_id, record.seq) && !repair_matches)
                    || record.run_id.is_some_and(|run_id| {
                        !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
                    })
            }
            _ => record.run_id.is_some_and(|run_id| {
                !runs.contains_key(&run_id) && taint.run_before(run_id, record.seq)
            }),
        }
    };
    if tainted_prerequisite {
        taint
            .mark_event(record)
            .map_err(|message| EventLogError::Corrupt {
                path: path.to_owned(),
                message: message.into(),
            })?;
        state.finish_record(record);
        return Ok(());
    }
    match &record.payload {
        EventPayload::SessionPermissionOverlaySet { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "SessionPermissionOverlaySet must not have run_id");
            }
        }
        EventPayload::SessionToolExposureSet { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "SessionToolExposureSet must not have run_id");
            }
        }
        EventPayload::DelegationReserved {
            reservation,
            request,
            ..
        } => {
            if record.run_id != Some(reservation.parent_run_id)
                || record.session_id != reservation.parent_session_id
                || !runs.contains_key(&reservation.parent_run_id)
                || delegations
                    .insert(
                        reservation.invocation_id,
                        DelegationAttribution {
                            parent_run_id: reservation.parent_run_id,
                            child_session_id: reservation.child_session_id,
                            resume: request.resume_session_id.is_some(),
                            started: false,
                            child_run_id: None,
                            finished: false,
                            finished_seq: None,
                        },
                    )
                    .is_some()
            {
                return corrupt(path, "delegation reservation ownership is invalid");
            }
        }
        EventPayload::DelegationProgressed {
            invocation_id,
            stage: Open::Known(DelegationStage::Started { child_session_id }),
        } => {
            let Some(delegation) = delegations.get_mut(invocation_id) else {
                return corrupt(path, "delegation start appeared before its reservation");
            };
            if record.run_id != Some(delegation.parent_run_id)
                || *child_session_id != delegation.child_session_id
                || delegation.started
                || delegation.finished
            {
                return corrupt(path, "delegation start ownership is invalid");
            }
            delegation.started = true;
        }
        EventPayload::DelegationProgressed {
            invocation_id,
            stage:
                Open::Known(
                    stage @ (DelegationStage::RunStarted { child_run_id }
                    | DelegationStage::RunAttached { child_run_id }),
                ),
        } => {
            let attached = matches!(stage, DelegationStage::RunAttached { .. });
            let Some(delegation) = delegations.get_mut(invocation_id) else {
                return corrupt(path, "delegation run appeared before its reservation");
            };
            if record.run_id != Some(delegation.parent_run_id)
                || delegation.child_run_id.is_some()
                || delegation.finished
                || (attached && !delegation.resume)
            {
                return corrupt(path, "delegation run ownership is invalid");
            }
            delegation.child_run_id = Some(*child_run_id);
        }
        EventPayload::DelegationFinished {
            invocation_id,
            child_session_id,
            child_run_id,
            ..
        } => {
            let Some(delegation) = delegations.get_mut(invocation_id) else {
                return corrupt(path, "delegation finish appeared before its reservation");
            };
            if record.run_id != Some(delegation.parent_run_id)
                || *child_session_id != delegation.child_session_id
                || *child_run_id != delegation.child_run_id
                || delegation.finished
            {
                return corrupt(path, "delegation finish ownership is invalid");
            }
            if taint.delegation_unrepaired_before(*invocation_id, record.seq) {
                taint.delegation_repairs.insert(*invocation_id, record.seq);
            }
            delegation.finished = true;
        }
        EventPayload::SkillLoaded { .. } | EventPayload::SkillInvocationNoted { .. } => {
            if record.run_id.is_some() {
                require_started_run(path, runs, record.run_id)?;
            }
        }
        EventPayload::DelegateChildTerminated { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "DelegateChildTerminated must not have run_id");
            }
        }
        EventPayload::UserInputAdmitted { .. } if record.run_id.is_none() => {
            if active_run.is_some() && !taint.active_run_ordering_before(record.seq) {
                return corrupt(path, "runless UserInputAdmitted requires no active run");
            }
            if taint.active_run_ordering_before(record.seq) {
                *active_run = None;
            }
        }
        EventPayload::UserInputRecalled {
            user_input_seq,
            content,
        } => {
            if record.run_id.is_none()
                && active_run.is_some()
                && !taint.active_run_ordering_before(record.seq)
            {
                return corrupt(path, "runless UserInputRecalled requires no active run");
            }
            if missing_admission(*user_input_seq, record.run_id, content) {
                return corrupt(path, "UserInputRecalled target is not a prior admission");
            }
        }
        EventPayload::DelegatedContextSeeded { .. } => {
            if record.run_id.is_some() || !runs.is_empty() {
                return corrupt(
                    path,
                    "DelegatedContextSeeded must be runless and precede the first run",
                );
            }
        }
        EventPayload::RunStarted {
            agent,
            internal_agents,
            ..
        } => {
            let Some(run_id) = record.run_id else {
                return corrupt(path, "RunStarted is missing run_id");
            };
            let attribution = RunAttribution {
                start_seq: record.seq,
                agent_id: agent.agent.clone(),
                internal_agents: internal_agents
                    .iter()
                    .map(|definition| (definition.kind, definition.agent.clone()))
                    .collect(),
                prompt_fingerprint: agent.prompt_fingerprint.clone(),
                selected_suffix: agent
                    .selected_suffix()
                    .iter()
                    .map(|binding| binding.resolved.clone())
                    .collect(),
                active_fallback_index: 0,
                next_attempt_ordinal: 1,
                attempts_on_active: 0,
                active_attempt: None,
                ordering_tainted: false,
                ended_seq: None,
                replay_base: None,
            };
            if runs.insert(run_id, attribution).is_some() {
                return corrupt(path, "run_id has more than one RunStarted event");
            }
            *active_run = Some(run_id);
        }
        // A checkpoint inside a running run composed its system prompt
        // afresh: its later attempts carry the new prompt's fingerprint.
        EventPayload::SystemPromptComposed {
            prompt_fingerprint, ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let run = runs.get_mut(&run_id).expect("started run is indexed");
            if run.ended_seq.is_some() {
                return corrupt(path, "SystemPromptComposed after its run ended");
            }
            run.prompt_fingerprint = prompt_fingerprint.clone();
        }
        EventPayload::SessionTitleCommitted { change, .. } => {
            let user = matches!(
                change,
                cookie_agent_protocol::SessionTitleChange::UserSet { .. }
                    | cookie_agent_protocol::SessionTitleChange::UserClear { .. }
                    | cookie_agent_protocol::SessionTitleChange::UserReset { .. }
                    | cookie_agent_protocol::SessionTitleChange::DelegatedSet { .. }
            );
            if user != record.run_id.is_none() {
                return corrupt(path, "SessionTitleCommitted has inconsistent run ownership");
            }
            if let Some(run_id) = record.run_id
                && !runs.contains_key(&run_id)
            {
                return corrupt(path, "session title references a run before RunStarted");
            }
        }
        EventPayload::ModelAttemptStarted {
            attempt_id,
            attempt_ordinal,
            fallback_index,
            retry_ordinal,
            resolved_model,
            prompt_fingerprint,
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            if attempts.contains_key(attempt_id) {
                return corrupt(path, "attempt_id has more than one ModelAttemptStarted");
            }
            let run = runs.get_mut(&run_id).expect("started run is indexed");
            run.ordering_tainted |= taint.run_ordering_between(run_id, run.start_seq, record.seq);
            if strict && run.ordering_tainted {
                return corrupt(
                    path,
                    "cannot strictly append an attempt after missing run-order prerequisites",
                );
            }
            if !run.ordering_tainted && run.active_attempt.is_some() {
                return corrupt(
                    path,
                    "ModelAttemptStarted appeared before the prior attempt ended",
                );
            }
            if !run.ordering_tainted && *attempt_ordinal != run.next_attempt_ordinal {
                return corrupt(path, "attempt_ordinal is not contiguous within its run");
            }
            let Ok(fallback_index) = usize::try_from(*fallback_index) else {
                return corrupt(path, "fallback_index does not index the frozen suffix");
            };
            if !run.ordering_tainted && fallback_index != run.active_fallback_index {
                return corrupt(
                    path,
                    "attempt fallback_index is not the active frozen suffix entry",
                );
            }
            let Some(expected_model) = run.selected_suffix.get(fallback_index) else {
                return corrupt(path, "fallback_index does not index the frozen suffix");
            };
            if resolved_model != expected_model {
                return corrupt(
                    path,
                    "attempt resolved model does not match its frozen suffix entry",
                );
            }
            if prompt_fingerprint != &run.prompt_fingerprint {
                return corrupt(path, "attempt prompt fingerprint does not match RunStarted");
            }
            if !run.ordering_tainted && *retry_ordinal != run.attempts_on_active {
                return corrupt(
                    path,
                    "retry_ordinal is not contiguous for the active fallback entry",
                );
            }
            run.next_attempt_ordinal = attempt_ordinal.saturating_add(1);
            run.active_fallback_index = fallback_index;
            run.attempts_on_active = retry_ordinal.saturating_add(1);
            run.active_attempt = Some(*attempt_id);
            attempts.insert(
                *attempt_id,
                AttemptAttribution {
                    started_seq: record.seq,
                    run_id,
                    resolved_model: resolved_model.clone(),
                    finished: false,
                    committed: false,
                    abandoned: false,
                },
            );
        }
        EventPayload::ModelRequestPrepared { attempt_id, .. }
        | EventPayload::ModelToolsPublished { attempt_id, .. }
        | EventPayload::ModelOutputStarted { attempt_id, .. } => {
            validate_attempt_owner(path, attempts, *attempt_id, record.run_id)?;
        }
        EventPayload::AttemptAbandoned { attempt_id, .. } => {
            let run_id =
                validate_abandoned_attempt_owner(path, attempts, *attempt_id, record.run_id)?;
            let run = runs.get_mut(&run_id).expect("started run is indexed");
            run.ordering_tainted |= taint.run_ordering_between(run_id, run.start_seq, record.seq);
            if strict && run.ordering_tainted {
                return corrupt(
                    path,
                    "cannot strictly append an attempt terminal after missing prerequisites",
                );
            }
            finish_attempt(path, runs, attempts, run_id, *attempt_id, false)?;
        }
        EventPayload::ModelReplayEvaluated {
            attempt_id,
            resolved_model,
            base_attempt_id,
            ..
        } => {
            let run =
                validate_attempt_model(path, attempts, *attempt_id, record.run_id, resolved_model)?;
            if let Some(base) = base_attempt_id {
                validate_replay_base(path, attempts, *attempt_id, *base, run, resolved_model)?;
            }
            if let Some(run) = runs.get_mut(&run) {
                run.replay_base = Some(*attempt_id);
            }
        }
        EventPayload::ModelTurnCommitted {
            attempt_id,
            model_turn_seq,
            resolved_model,
            turn,
            ..
        } => {
            let run_id =
                validate_attempt_model(path, attempts, *attempt_id, record.run_id, resolved_model)?;
            let run = runs.get_mut(&run_id).expect("started run is indexed");
            run.ordering_tainted |= taint.run_ordering_between(run_id, run.start_seq, record.seq);
            if strict && run.ordering_tainted {
                return corrupt(
                    path,
                    "cannot strictly append a model turn after missing prerequisites",
                );
            }
            finish_attempt(path, runs, attempts, run_id, *attempt_id, true)?;
            let turn_ordering_tainted = taint.turn_ordering_before(record.seq);
            if *model_turn_seq != *next_model_turn_seq && (strict || !turn_ordering_tainted) {
                return corrupt(path, "model_turn_seq is not contiguous");
            }
            *next_model_turn_seq = model_turn_seq.saturating_add(1);
            for (content_index, part) in turn.content.iter().enumerate() {
                if let cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                    id,
                    provider_item_id,
                    ..
                } = part
                {
                    let owner = AssistantToolCallRef {
                        model_turn_seq: *model_turn_seq,
                        content_index: content_index as u32,
                        model_call_id: id.clone(),
                        provider_item_id: provider_item_id.clone(),
                    };
                    if model_call_owners
                        .insert((run_id, id.clone()), owner.clone())
                        .is_some()
                    {
                        return corrupt(path, "model call id is reused within a run");
                    }
                    if let Some(provider_item_id) = provider_item_id
                        && provider_item_owners
                            .insert((run_id, provider_item_id.clone()), owner)
                            .is_some()
                    {
                        return corrupt(path, "provider item id is reused within a run");
                    }
                }
            }
            let tool_calls = turn
                .content
                .iter()
                .map(|part| match part {
                    cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                        id,
                        provider_item_id,
                        ..
                    } => Some((id.clone(), provider_item_id.clone())),
                    _ => None,
                })
                .collect();
            if turns
                .insert(*model_turn_seq, CommittedTurn { run_id, tool_calls })
                .is_some()
            {
                return corrupt(path, "model turn sequence is duplicated");
            }
            turn_models.insert(*model_turn_seq, resolved_model.clone());
        }
        EventPayload::ModelUsageRecorded {
            model_turn_seq,
            agent_id,
            resolved_model,
            ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let Some(CommittedTurn {
                run_id: turn_run, ..
            }) = turns.get(model_turn_seq)
            else {
                return corrupt(path, "usage references an unknown committed model turn");
            };
            if *turn_run != run_id
                || turn_models.get(model_turn_seq) != Some(resolved_model)
                || runs.get(&run_id).map(|run| &run.agent_id) != Some(agent_id)
                || !usage_turns.insert(*model_turn_seq)
            {
                return corrupt(path, "usage ownership does not match its model turn");
            }
        }
        EventPayload::InternalAgentStarted {
            invocation_id,
            internal_run_id,
            kind,
            backend,
            ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let active_model = match backend {
                cookie_agent_protocol::InternalAgentBackend::Model { resolved_model } => {
                    Some(resolved_model.clone())
                }
                cookie_agent_protocol::InternalAgentBackend::Builtin { .. } => None,
            };
            if internal_runs
                .insert(
                    *internal_run_id,
                    InternalRunAttribution {
                        start_seq: record.seq,
                        invocation_id: *invocation_id,
                        kind: *kind,
                        run_id,
                        active_model,
                        usage_recorded_in_phase: false,
                        model_phase_taint_seen: 0,
                        usage_phase_taint_seen: 0,
                        finished_seq: None,
                    },
                )
                .is_some()
            {
                return corrupt(path, "internal_run_id has more than one start");
            }
        }
        EventPayload::InternalAgentFallback {
            invocation_id,
            internal_run_id,
            kind,
            from,
            to,
            ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let Some(internal) = internal_runs.get_mut(internal_run_id) else {
                return corrupt(path, "internal fallback appeared before its start");
            };
            let from_model = match from {
                cookie_agent_protocol::InternalAgentBackend::Model { resolved_model } => {
                    Some(resolved_model)
                }
                cookie_agent_protocol::InternalAgentBackend::Builtin { .. } => None,
            };
            let model_phase_taint = taint
                .internal_model_phase_taint_before(*internal_run_id, record.seq)
                .filter(|tainted| {
                    *tainted > internal.start_seq && *tainted > internal.model_phase_taint_seen
                });
            if strict && model_phase_taint.is_some() {
                return corrupt(
                    path,
                    "cannot strictly append internal fallback after a missing phase transition",
                );
            }
            if internal.invocation_id != *invocation_id
                || internal.kind != *kind
                || internal.run_id != run_id
                || (model_phase_taint.is_none() && internal.active_model.as_ref() != from_model)
            {
                return corrupt(path, "internal fallback ownership does not match its run");
            }
            internal.active_model = match to {
                cookie_agent_protocol::InternalAgentBackend::Model { resolved_model } => {
                    Some(resolved_model.clone())
                }
                cookie_agent_protocol::InternalAgentBackend::Builtin { .. } => None,
            };
            internal.usage_recorded_in_phase = false;
            if let Some(tainted) = model_phase_taint {
                internal.model_phase_taint_seen = tainted;
            }
            if let Some(tainted) = taint
                .internal_usage_phase_taint_before(*internal_run_id, record.seq)
                .filter(|tainted| *tainted > internal.start_seq)
            {
                internal.usage_phase_taint_seen = tainted;
            }
        }
        EventPayload::InternalAgentUsageRecorded {
            internal_run_id,
            kind,
            agent_id,
            resolved_model,
            ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let Some(internal) = internal_runs.get_mut(internal_run_id) else {
                return corrupt(path, "internal usage appeared before its start");
            };
            let model_phase_taint = taint
                .internal_model_phase_taint_before(*internal_run_id, record.seq)
                .filter(|tainted| {
                    *tainted > internal.start_seq && *tainted > internal.model_phase_taint_seen
                });
            let usage_phase_taint = taint
                .internal_usage_phase_taint_before(*internal_run_id, record.seq)
                .filter(|tainted| {
                    *tainted > internal.start_seq && *tainted > internal.usage_phase_taint_seen
                });
            if strict && (model_phase_taint.is_some() || usage_phase_taint.is_some()) {
                return corrupt(
                    path,
                    "cannot strictly append internal usage after a missing phase transition",
                );
            }
            if internal.kind != *kind
                || internal.run_id != run_id
                || (model_phase_taint.is_none()
                    && internal.active_model.as_ref() != Some(resolved_model))
                || runs[&run_id]
                    .internal_agents
                    .iter()
                    .any(|(frozen, agent)| frozen == kind && agent != agent_id)
                || (usage_phase_taint.is_none() && internal.usage_recorded_in_phase)
            {
                return corrupt(path, "internal usage ownership does not match its run");
            }
            internal.active_model = Some(resolved_model.clone());
            internal.usage_recorded_in_phase = true;
            if let Some(tainted) = model_phase_taint {
                internal.model_phase_taint_seen = tainted;
            }
            if let Some(tainted) = usage_phase_taint {
                internal.usage_phase_taint_seen = tainted;
            }
        }
        EventPayload::ModelFallback {
            from,
            to,
            from_fallback_index,
            to_fallback_index,
            attempts_on_from,
            ..
        } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            let run = runs.get_mut(&run_id).expect("started run is indexed");
            run.ordering_tainted |= taint.run_ordering_between(run_id, run.start_seq, record.seq);
            if strict && run.ordering_tainted {
                return corrupt(
                    path,
                    "cannot strictly append fallback after missing run-order prerequisites",
                );
            }
            if !run.ordering_tainted && run.active_attempt.is_some() {
                return corrupt(
                    path,
                    "ModelFallback appeared before the active attempt ended",
                );
            }
            let Ok(from_index) = usize::try_from(*from_fallback_index) else {
                return corrupt(path, "ModelFallback index does not index the frozen suffix");
            };
            let Ok(to_index) = usize::try_from(*to_fallback_index) else {
                return corrupt(path, "ModelFallback index does not index the frozen suffix");
            };
            let Some(adjacent_index) = from_index.checked_add(1) else {
                return corrupt(path, "ModelFallback source index cannot advance");
            };
            if (!run.ordering_tainted && from_index != run.active_fallback_index)
                || to_index != adjacent_index
            {
                return corrupt(
                    path,
                    "ModelFallback transition is not adjacent from the active entry",
                );
            }
            let Some(expected_from) = run.selected_suffix.get(from_index) else {
                return corrupt(
                    path,
                    "ModelFallback source does not index the frozen suffix",
                );
            };
            let Some(expected_to) = run.selected_suffix.get(to_index) else {
                return corrupt(
                    path,
                    "ModelFallback target does not index the frozen suffix",
                );
            };
            if from != expected_from || to != expected_to {
                return corrupt(
                    path,
                    "ModelFallback models do not match the frozen suffix transition",
                );
            }
            if *attempts_on_from == 0
                || (!run.ordering_tainted && *attempts_on_from != run.attempts_on_active)
            {
                return corrupt(
                    path,
                    "ModelFallback attempt count does not match started attempts",
                );
            }
            run.active_fallback_index = to_index;
            run.attempts_on_active = 0;
        }
        EventPayload::ToolCallStarted { start } => {
            let run_id = require_started_run(path, runs, record.run_id)?;
            validate_tool_owner(
                path,
                run_id,
                turns,
                model_call_owners,
                provider_item_owners,
                &start.owner,
            )?;
            if tool_starts
                .insert(start.tool_call_id, (run_id, start.owner.clone()))
                .is_some()
            {
                return corrupt(path, "tool_call_id has more than one start");
            }
        }
        EventPayload::ToolCallTerminated { termination } => {
            // Keyed by the call id, so the owner is all left to match.
            let Some((run_id, owner)) = tool_starts.get(&termination.tool_call_id) else {
                return corrupt(path, "tool termination appeared before its start");
            };
            if record.run_id != Some(*run_id) || termination.owner != *owner {
                return corrupt(path, "tool termination ownership does not match its start");
            }
            if strict && taint.tool_terminal_before(termination.tool_call_id, record.seq) {
                return corrupt(
                    path,
                    "cannot strictly append tool termination after a missing terminal transition",
                );
            }
            if terminated_tools
                .insert(termination.tool_call_id, record.seq)
                .is_some()
            {
                return corrupt(path, "tool call has more than one terminal event");
            }
        }
        EventPayload::ToolOutputElided { tool_call_id, .. } => {
            let Some((run_id, _)) = tool_starts.get(tool_call_id) else {
                return corrupt(path, "tool elision appeared before its start");
            };
            let terminal_tainted = taint.tool_terminal_before(*tool_call_id, record.seq);
            if record.run_id != Some(*run_id)
                || (!terminated_tools.contains_key(tool_call_id) && !terminal_tainted)
                || (strict && terminal_tainted)
                || !elided_tools.insert(*tool_call_id)
            {
                return corrupt(path, "tool elision ownership or ordering is invalid");
            }
        }
        EventPayload::ToolCallLinked { tool_call_id, .. } => {
            let Some((run_id, _)) = tool_starts.get(tool_call_id) else {
                return corrupt(path, "tool lifecycle event appeared before its start");
            };
            let terminal_tainted = taint.tool_terminal_before(*tool_call_id, record.seq);
            if record.run_id != Some(*run_id)
                || terminated_tools.contains_key(tool_call_id)
                || (strict && terminal_tainted)
            {
                return corrupt(
                    path,
                    "tool lifecycle event has invalid ownership or ordering",
                );
            }
        }
        EventPayload::ApprovalRequested { request } => {
            let Some(run_id) = record.run_id else {
                return corrupt(path, "ApprovalRequested is missing run_id");
            };
            if !runs.contains_key(&run_id) {
                return corrupt(path, "approval references a run before RunStarted");
            }
            if approval_owners
                .insert(
                    request.approval_id(),
                    ApprovalOwner {
                        run: run_id,
                        settled_seq: None,
                    },
                )
                .is_some()
            {
                return corrupt(
                    path,
                    "approval_id has more than one ApprovalRequested event",
                );
            }
        }
        EventPayload::ApprovalStep { approval_id, .. }
        | EventPayload::ApprovalFinalized { approval_id, .. } => {
            validate_approval_owner(path, approval_owners, *approval_id, record.run_id)?;
        }
        EventPayload::TreeApprovalGrantCommitted { grant } => {
            validate_approval_owner(path, approval_owners, grant.approval_id, record.run_id)?;
        }
        // A diagnostic about a run belongs to it; any other is runless. An
        // omitted record keeps the run of the record it stands for. An
        // attachment resolves after its run, so it is runless.
        EventPayload::Diagnostic { .. } | EventPayload::AssistantAttachment { .. } => {
            if record.run_id.is_some() {
                require_started_run(path, runs, record.run_id)?;
            }
        }
        // A fork's stand-in for a delegation record keeps that record's run,
        // which nothing in the fork holds open: once runs were let go, it may
        // name one older than the floor.
        EventPayload::RecordOmitted { .. } => {
            if record.run_id.is_some() && !*let_go {
                require_started_run(path, runs, record.run_id)?;
            }
        }
        // An ACP agent's session is opened and its options are set within a
        // run or, by a client, outside one.
        EventPayload::AcpSessionStarted { .. } | EventPayload::AcpSessionUpdated { .. } => {
            if record.run_id.is_some() {
                require_started_run(path, runs, record.run_id)?;
            }
        }
        // The queue belongs to the session: entries outlive the run they
        // wait behind and start their own.
        EventPayload::UserInputQueued { .. }
        | EventPayload::UserInputQueueRecalled { .. }
        | EventPayload::UserInputQueueVoided {} => {
            if record.run_id.is_some() {
                return corrupt(path, "queued-input events must be runless");
            }
        }
        EventPayload::SessionModelSelected { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "SessionModelSelected must be runless");
            }
        }
        EventPayload::RuntimeAttached { .. } | EventPayload::RuntimeDetached { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "runtime attachment events must be runless");
            }
        }
        // A hold and its resume happen between runs.
        EventPayload::AutomaticRunsHeld { .. } | EventPayload::AutomaticRunsResumed {} => {
            if record.run_id.is_some() {
                return corrupt(path, "a hold or a resume must be runless");
            }
        }
        // A session moves between projects outside its runs.
        EventPayload::SessionProjectSet { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "SessionProjectSet must be runless");
            }
        }
        // The approval reviewer's own log has no runs.
        EventPayload::ReviewerTurnRecorded { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "ReviewerTurnRecorded must be runless");
            }
        }
        // A tree's permission mode changes outside its root's runs.
        EventPayload::PermissionModeSet { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "PermissionModeSet must be runless");
            }
        }
        // A root records its subagents' attachments as they are created.
        EventPayload::DelegatedAttachmentsAdded { .. } => {
            if record.run_id.is_some() {
                return corrupt(path, "DelegatedAttachmentsAdded must be runless");
            }
        }
        EventPayload::GoalActivated { .. }
        | EventPayload::GoalChecklistRevised { .. }
        | EventPayload::GoalLifecycleChanged { .. }
        | EventPayload::ProducerMessageAccepted { .. }
        | EventPayload::ProducerMessageDiscarded { .. } => {
            if record.run_id.is_some() {
                require_started_run(path, runs, record.run_id)?;
            }
        }
        _ => {
            require_started_run(path, runs, record.run_id)?;
        }
    }
    if matches!(record.payload, EventPayload::RunFinished { .. }) && record.run_id == *active_run {
        *active_run = None;
    }
    state.finish_record(record);
    Ok(())
}

struct LoadedEvents {
    records: Vec<StoredEvent>,
    /// Why the log is render-only, if it is.
    render_only: Option<RenderOnly>,
    raw: LoadedRaw,
    lines: LineIndex,
    /// The file ended in a partial record, cut or ignored per [`TornTail`].
    torn_tail: bool,
    /// The log was converted from an older history format in memory.
    converted: bool,
    diagnostics: Vec<EventLoadDiagnostic>,
    validation_taint: ValidationTaint,
    next_seq: u64,
}

/// Whether `payload` is a `session_created` of this build's history format.
fn is_current_creation(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::SessionCreated { history_format, .. }
            if *history_format == cookie_agent_protocol::HISTORY_FORMAT
    )
}

/// Checks the history format `session_created` declares. A log without one
/// predates history formats; a newer one needs a newer build. An older one
/// was converted before it got here, unless no conversion from it exists.
fn check_history_format(path: &Path, creation: Option<&Value>) -> Result<(), EventLogError> {
    let Some(format) = creation
        .and_then(|creation| creation.get("history_format"))
        .and_then(Value::as_u64)
    else {
        return corrupt_value(
            path,
            "SessionCreated is unsupported or corrupt: the log has no history format",
        );
    };
    let format = u32::try_from(format).unwrap_or(u32::MAX);
    match format.cmp(&cookie_agent_protocol::HISTORY_FORMAT) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => Err(EventLogError::RequiresNewerEngine {
            path: path.to_owned(),
            event: None,
            history_format: Some(format),
        }),
        std::cmp::Ordering::Less => corrupt_value(
            path,
            format!(
                "SessionCreated is unsupported or corrupt: history format {format} has no \
                 conversion to {}",
                cookie_agent_protocol::HISTORY_FORMAT
            ),
        ),
    }
}

/// The bytes of `bytes`'s log in this build's history format: converted when
/// its first line declares an older format a conversion exists from, else
/// as they are, for the loader to judge. A conversion's blobs stay in
/// memory, where the loader reads them back from.
fn in_current_format(
    path: &Path,
    bytes: Vec<u8>,
) -> Result<(Vec<u8>, Option<blobs::Blobs>), EventLogError> {
    let first = bytes
        .split(|byte| *byte == b'\n')
        .find(|line| !line.is_empty())
        .and_then(|line| serde_json::from_slice::<Value>(line).ok());
    match first.as_ref().and_then(convert::history_format) {
        Some(format) if convert::convertible(format) => {
            let folder = blobs::tree_blob_dir(path);
            let converted =
                convert::convert_bytes(format, &bytes, Some(&folder)).map_err(|error| {
                    EventLogError::Corrupt {
                        path: path.to_owned(),
                        message: error.to_string(),
                    }
                })?;
            Ok((converted.log, Some(converted.moved)))
        }
        _ => Ok((bytes, None)),
    }
}

/// Whether a record's sequence advances the log, recording any gap before
/// it. A sequence that does not advance ends the readable log there.
fn observe_sequence(
    seq: u64,
    last_observed_seq: &mut u64,
    diagnostics: &mut Vec<EventLoadDiagnostic>,
    validation_taint: &mut ValidationTaint,
) -> bool {
    if seq <= *last_observed_seq {
        diagnostics.push(EventLoadDiagnostic {
            seq,
            reason: format!(
                "event sequence {seq} does not follow {last_observed_seq}; the log is read up \
                 to it and the session can only be viewed"
            ),
            engine_version: None,
            skipped: true,
        });
        return false;
    }
    if *last_observed_seq > 0 && seq > *last_observed_seq + 1 {
        let start = *last_observed_seq + 1;
        let end = seq - 1;
        diagnostics.push(EventLoadDiagnostic {
            seq: start,
            reason: if start == end {
                "event sequence is absent from the physical log".into()
            } else {
                format!("event sequences {start}..={end} are absent from the physical log")
            },
            engine_version: None,
            skipped: true,
        });
        validation_taint.mark_broad(start, end);
    }
    *last_observed_seq = seq;
    true
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Test-only: event logs this thread parsed from disk.
    pub static EVENT_LOG_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Test-only: log syncs this thread ran.
    pub static EVENT_LOG_SYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Reads and decodes the whole log at `path`. A large log takes a while, so
/// the work leaves the async worker. Records read as their writer bounded
/// them: the writer-only size caps are off
/// ([`cookie_agent_protocol::read_stored_history`]).
fn load_event_jsonl(path: &Path, torn_tail: TornTail) -> Result<LoadedEvents, EventLogError> {
    crate::blocking::off_worker(|| {
        cookie_agent_protocol::read_stored_history(|| load_event_jsonl_in_place(path, torn_tail))
    })
}

fn load_event_jsonl_in_place(
    path: &Path,
    torn_tail: TornTail,
) -> Result<LoadedEvents, EventLogError> {
    #[cfg(any(test, feature = "test-support"))]
    EVENT_LOG_PARSES.with(|parses| parses.set(parses.get() + 1));
    let (bytes, torn) = read_complete_jsonl(path, torn_tail)?;
    let (bytes, converted_blobs) = in_current_format(path, bytes)?;
    let converted = converted_blobs.is_some();
    let blob_dir = blobs::tree_blob_dir(path);
    let mut fetch = |digest: &str| match converted_blobs
        .as_ref()
        .and_then(|converted| converted.get(digest))
    {
        Some(bytes) => Ok(bytes.clone()),
        None => blobs::read(&blob_dir, digest),
    };
    let mut lines = LineIndex {
        len: bytes.len() as u64,
        ..LineIndex::default()
    };
    let line_start = |line: &[u8]| (line.as_ptr() as usize - bytes.as_ptr() as usize) as u64;
    let raw_line = |seq: u64, line: &[u8], strict: bool| {
        let start = line.as_ptr() as usize - bytes.as_ptr() as usize;
        LoadedLine {
            seq,
            start,
            end: start + line.len(),
            strict,
        }
    };
    let mut raw = Vec::new();
    let mut records = Vec::new();
    let mut reader = LineReader::new(path);
    for (index, line) in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let read = reader.read(index, line, &mut fetch)?;
        if let Some((seq, strict)) = read.sequenced {
            lines.note(seq, line_start(line));
            raw.push(raw_line(seq, line, strict));
        }
        if read.stop {
            break;
        }
        records.extend(read.record);
    }
    let observed_tip = reader.last_observed_seq.max(
        reader
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.skipped)
            .map(|diagnostic| diagnostic.seq)
            .max()
            .unwrap_or(0),
    );
    Ok(LoadedEvents {
        records,
        render_only: reader.render_only,
        raw: LoadedRaw {
            buffer: bytes,
            lines: raw,
            converted,
        },
        lines,
        torn_tail: torn,
        converted,
        diagnostics: reader.diagnostics,
        validation_taint: reader.validation_taint,
        next_seq: observed_tip.saturating_add(1).max(1),
    })
}

/// What one line of a log reads as.
#[derive(Default)]
struct LineRead {
    /// The line's sequence, when it carries one that advances the log, and
    /// whether it decoded into its record as it is, with no repair but its
    /// values in blobs read back.
    sequenced: Option<(u64, bool)>,
    /// The line's record; `None` when it holds none this build can read.
    record: Option<StoredEvent>,
    /// The sequence stopped increasing here: nothing from this line on is read.
    stop: bool,
}

/// Reads a log's lines in order, one at a time, what [`EventLog`]'s load and a
/// pass over its file ([`History`]) both go through, so that they read the same
/// records. It repairs what it can read and notes what it cannot.
struct LineReader<'a> {
    path: &'a Path,
    diagnostics: Vec<EventLoadDiagnostic>,
    validation_taint: ValidationTaint,
    last_observed_seq: u64,
    render_only: Option<RenderOnly>,
}

impl<'a> LineReader<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            diagnostics: Vec::new(),
            validation_taint: ValidationTaint::default(),
            last_observed_seq: 0,
            render_only: None,
        }
    }

    /// Reads `line`, the log's line at `index` among those that are not empty,
    /// taking the values it keeps in blobs from `fetch`.
    fn read(
        &mut self,
        index: usize,
        line: &[u8],
        fetch: &mut dyn FnMut(&str) -> io::Result<Vec<u8>>,
    ) -> Result<LineRead, EventLogError> {
        let path = self.path;
        let line_number = index as u64 + 1;
        // Nearly every record is intact: type it straight from the bytes and
        // leave the untyped, repairing reader to the rest, and to a record
        // whose large values it reads back from blobs.
        let references = blobs::has_references(line);
        // The first record also has to be of this build's history format,
        // which the slower reader explains when it is not.
        if !references
            && let Some(event) = StoredEvent::decode_strict(line)
            && (index > 0 || is_current_creation(&event.payload))
        {
            if !observe_sequence(
                event.seq,
                &mut self.last_observed_seq,
                &mut self.diagnostics,
                &mut self.validation_taint,
            ) {
                self.render_only
                    .get_or_insert(RenderOnly::SequenceBroken(event.seq));
                return Ok(LineRead {
                    stop: true,
                    ..LineRead::default()
                });
            }
            return Ok(LineRead {
                sequenced: Some((event.seq, true)),
                record: Some(event),
                stop: false,
            });
        }
        let mut value = match serde_json::from_slice::<serde_json::Value>(line) {
            Ok(value) => value,
            Err(error) => {
                if index == 0 {
                    return corrupt_value(
                        path,
                        format!("SessionCreated line is unreadable: {error}"),
                    );
                }
                self.diagnostics.push(EventLoadDiagnostic {
                    seq: line_number,
                    reason: format!("corrupt JSON line: {error}"),
                    engine_version: None,
                    skipped: true,
                });
                self.validation_taint.mark_broad(line_number, line_number);
                return Ok(LineRead::default());
            }
        };
        let Some(object) = value.as_object_mut() else {
            if index == 0 {
                return corrupt_value(path, "SessionCreated line is not a JSON object");
            }
            self.diagnostics.push(EventLoadDiagnostic {
                seq: line_number,
                reason: "event envelope is not a JSON object".into(),
                engine_version: None,
                skipped: true,
            });
            self.validation_taint.mark_broad(line_number, line_number);
            return Ok(LineRead::default());
        };
        let seq = object
            .get("seq")
            .and_then(Value::as_u64)
            .unwrap_or(line_number);
        let engine_version = object
            .get("engine_version")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let envelope_run_id = object
            .get("run_id")
            .and_then(|value| RunId::deserialize(value).ok());
        let mut read = LineRead::default();
        if object.get("seq").and_then(Value::as_u64).is_some() {
            if !observe_sequence(
                seq,
                &mut self.last_observed_seq,
                &mut self.diagnostics,
                &mut self.validation_taint,
            ) {
                self.render_only
                    .get_or_insert(RenderOnly::SequenceBroken(seq));
                read.stop = true;
                return Ok(read);
            }
            read.sequenced = Some((seq, false));
        }
        // Envelope members this build does not know are ignored, on the
        // first line too: they carry what a newer build added.
        if index == 0 {
            check_history_format(path, object.get("payload"))?;
        }
        if let Some(tag) = object
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            .filter(|tag| index > 0 && !EventPayload::is_known_tag(tag))
        {
            // An event from a newer build: an audit event is skipped without
            // a trace, a state event leaves the session render-only.
            if object.get("skippable") != Some(&Value::Bool(true)) {
                self.diagnostics.push(EventLoadDiagnostic {
                    seq,
                    reason: format!(
                        "event `{tag}` is unknown to this build; the session can only be viewed"
                    ),
                    engine_version,
                    skipped: true,
                });
                self.render_only
                    .get_or_insert_with(|| RenderOnly::UnknownStateEvent(tag.to_owned()));
                if let Some(payload) = object.get("payload") {
                    self.validation_taint
                        .mark_record(seq, envelope_run_id, payload)
                        .map_err(|message| EventLogError::Corrupt {
                            path: path.to_owned(),
                            message: message.into(),
                        })?;
                }
            }
            return Ok(read);
        }
        let mut degraded = Vec::new();
        if object
            .get("engine_version")
            .is_some_and(|version| !version.is_null() && !version.is_string())
        {
            object.remove("engine_version");
            degraded.push("engine_version".to_owned());
        }
        let Some(mut payload_value) = object.remove("payload") else {
            if index == 0 {
                return corrupt_value(path, "SessionCreated payload is absent");
            }
            self.diagnostics.push(EventLoadDiagnostic {
                seq,
                reason: "required payload is absent".into(),
                engine_version,
                skipped: true,
            });
            self.validation_taint.mark_broad(seq, seq);
            return Ok(read);
        };
        if references && let Err(reason) = blobs::internalize(&mut payload_value, fetch) {
            // A blob a crash lost before the sync that covers its record, or
            // one removed by hand: the record cannot be read whole.
            if index == 0 {
                return corrupt_value(
                    path,
                    format!("SessionCreated is unsupported or corrupt: {reason}"),
                );
            }
            self.diagnostics.push(EventLoadDiagnostic {
                seq,
                reason,
                engine_version,
                skipped: true,
            });
            self.validation_taint
                .mark_record(seq, envelope_run_id, &payload_value)
                .map_err(|message| EventLogError::Corrupt {
                    path: path.to_owned(),
                    message: message.into(),
                })?;
            return Ok(read);
        }
        let payload = match deserialize_event_payload_best_effort(&payload_value) {
            Ok(read) => {
                degraded.extend(
                    read.degraded_fields
                        .into_iter()
                        .map(|field| format!("payload.{field}")),
                );
                read.payload
            }
            Err(reason) => {
                if index == 0 {
                    return corrupt_value(
                        path,
                        format!("SessionCreated is unsupported or corrupt: {reason}"),
                    );
                }
                self.diagnostics.push(EventLoadDiagnostic {
                    seq,
                    reason,
                    engine_version,
                    skipped: true,
                });
                self.validation_taint
                    .mark_record(seq, envelope_run_id, &payload_value)
                    .map_err(|message| EventLogError::Corrupt {
                        path: path.to_owned(),
                        message: message.into(),
                    })?;
                return Ok(read);
            }
        };
        drop(payload_value);
        // The envelope is decoded without its payload, which is already typed:
        // no line is re-serialized or decoded a second time.
        let event = match StoredEventEnvelope::deserialize(&value) {
            Ok(envelope) if !payload.skippable_matches(envelope.skippable()) => Err((
                cookie_agent_protocol::EventSchemaError::SkippableMismatch.to_string(),
                payload,
            )),
            Ok(envelope) => {
                let event = envelope.with_payload(payload);
                match event.validate() {
                    Ok(()) => Ok(event),
                    Err(error) => Err((error.to_string(), event.payload)),
                }
            }
            Err(error) => Err((error.to_string(), payload)),
        };
        match event {
            Ok(event) => {
                if !degraded.is_empty() {
                    self.diagnostics.push(EventLoadDiagnostic {
                        seq,
                        reason: format!("degraded optional fields: {}", degraded.join(", ")),
                        engine_version: event.engine_version.clone(),
                        skipped: false,
                    });
                } else if references && let Some((_, strict)) = &mut read.sequenced {
                    *strict = true;
                }
                read.record = Some(event);
            }
            Err((error, payload)) => {
                if index == 0 {
                    return corrupt_value(
                        path,
                        format!("SessionCreated is unsupported or corrupt: {error}"),
                    );
                }
                self.diagnostics.push(EventLoadDiagnostic {
                    seq,
                    reason: error,
                    engine_version,
                    skipped: true,
                });
                let payload = serde_json::to_value(payload).expect("event payload serializes");
                self.validation_taint
                    .mark_record(seq, envelope_run_id, &payload)
                    .map_err(|message| EventLogError::Corrupt {
                        path: path.to_owned(),
                        message: message.into(),
                    })?;
            }
        }
        Ok(read)
    }
}

fn require_started_run(
    path: &Path,
    runs: &HashMap<RunId, RunAttribution>,
    run_id: Option<RunId>,
) -> Result<RunId, EventLogError> {
    let Some(run_id) = run_id else {
        return corrupt_value(path, "run-owned event is missing run_id");
    };
    if !runs.contains_key(&run_id) {
        return corrupt_value(path, "event references a run before RunStarted");
    }
    Ok(run_id)
}

fn validate_attempt_owner(
    path: &Path,
    attempts: &HashMap<AttemptId, AttemptAttribution>,
    attempt_id: AttemptId,
    run_id: Option<RunId>,
) -> Result<RunId, EventLogError> {
    let Some(attempt) = attempts.get(&attempt_id) else {
        return corrupt_value(path, "attempt event appeared before ModelAttemptStarted");
    };
    if run_id != Some(attempt.run_id) {
        return corrupt_value(path, "attempt event uses a non-owning run_id");
    }
    if attempt.finished {
        return corrupt_value(
            path,
            "attempt lifecycle event appeared after its terminal event",
        );
    }
    Ok(attempt.run_id)
}

/// `AttemptAbandoned` may follow the commit that already closed its attempt.
///
/// An interrupted model attempt commits its partial turn (`ModelTurnCommitted`,
/// which closes the attempt) and then records the abandonment, so the abandon
/// of an already-committed attempt is terminal-isolation for the commit rather
/// than a second terminal event. Every other lifecycle event stays strict.
fn validate_abandoned_attempt_owner(
    path: &Path,
    attempts: &HashMap<AttemptId, AttemptAttribution>,
    attempt_id: AttemptId,
    run_id: Option<RunId>,
) -> Result<RunId, EventLogError> {
    let Some(attempt) = attempts.get(&attempt_id) else {
        return corrupt_value(path, "attempt event appeared before ModelAttemptStarted");
    };
    if run_id != Some(attempt.run_id) {
        return corrupt_value(path, "attempt event uses a non-owning run_id");
    }
    if attempt.finished && (attempt.abandoned || !attempt.committed) {
        return corrupt_value(
            path,
            "attempt lifecycle event appeared after its terminal event",
        );
    }
    Ok(attempt.run_id)
}

fn validate_attempt_model(
    path: &Path,
    attempts: &HashMap<AttemptId, AttemptAttribution>,
    attempt_id: AttemptId,
    run_id: Option<RunId>,
    resolved_model: &cookie_agent_protocol::ResolvedModelRef,
) -> Result<RunId, EventLogError> {
    let owner = validate_attempt_owner(path, attempts, attempt_id, run_id)?;
    if attempts
        .get(&attempt_id)
        .is_none_or(|attempt| &attempt.resolved_model != resolved_model)
    {
        return corrupt_value(path, "attempt resolved model changed within its lifecycle");
    }
    Ok(owner)
}

/// A delta replay evaluation must extend an earlier attempt of its own run
/// that used the same model.
fn validate_replay_base(
    path: &Path,
    attempts: &HashMap<AttemptId, AttemptAttribution>,
    attempt_id: AttemptId,
    base: AttemptId,
    run_id: RunId,
    resolved_model: &cookie_agent_protocol::ResolvedModelRef,
) -> Result<(), EventLogError> {
    if base == attempt_id
        || attempts
            .get(&base)
            .is_none_or(|base| base.run_id != run_id || &base.resolved_model != resolved_model)
    {
        return corrupt_value(
            path,
            "replay evaluation base is not an earlier attempt of its run and model",
        );
    }
    Ok(())
}

fn finish_attempt(
    path: &Path,
    runs: &mut HashMap<RunId, RunAttribution>,
    attempts: &mut HashMap<AttemptId, AttemptAttribution>,
    run_id: RunId,
    attempt_id: AttemptId,
    committed: bool,
) -> Result<(), EventLogError> {
    let attempt = attempts
        .get_mut(&attempt_id)
        .expect("validated attempt is indexed");
    let already_committed = attempt.committed;
    attempt.finished = true;
    attempt.committed |= committed;
    attempt.abandoned |= !committed;
    let run = runs.get_mut(&run_id).expect("started run is indexed");
    if !run.ordering_tainted && run.active_attempt != Some(attempt_id) {
        // `ModelTurnCommitted` already closed a committed attempt and cleared
        // `active_attempt`, so the abandon an interrupt writes afterwards is
        // expected rather than an ordering violation.
        let abandoned_after_commit =
            !committed && already_committed && run.active_attempt.is_none();
        if !abandoned_after_commit {
            return corrupt(
                path,
                "attempt terminal event is inconsistent with the active attempt",
            );
        }
    }
    run.active_attempt = None;
    if committed {
        run.attempts_on_active = 0;
    }
    Ok(())
}

fn validate_tool_owner(
    path: &Path,
    run_id: RunId,
    turns: &HashMap<u64, CommittedTurn>,
    model_call_owners: &HashMap<(RunId, ModelCallId), AssistantToolCallRef>,
    provider_item_owners: &HashMap<(RunId, ProviderItemId), AssistantToolCallRef>,
    owner: &AssistantToolCallRef,
) -> Result<(), EventLogError> {
    let Some(CommittedTurn {
        run_id: turn_run,
        tool_calls,
    }) = turns.get(&owner.model_turn_seq)
    else {
        return corrupt(
            path,
            "tool owner references an unknown committed model turn",
        );
    };
    let Some(Some((id, provider_item_id))) = tool_calls.get(owner.content_index as usize) else {
        return corrupt(
            path,
            "tool owner content index is not a committed tool call",
        );
    };
    if *turn_run != run_id
        || id != &owner.model_call_id
        || provider_item_id != &owner.provider_item_id
        || model_call_owners.get(&(run_id, id.clone())) != Some(owner)
        || provider_item_id.as_ref().is_some_and(|provider_id| {
            provider_item_owners.get(&(run_id, provider_id.clone())) != Some(owner)
        })
    {
        return corrupt(
            path,
            "tool owner does not match the committed model content",
        );
    }
    Ok(())
}

fn validate_approval_owner(
    path: &Path,
    owners: &HashMap<cookie_agent_protocol::ApprovalId, ApprovalOwner>,
    approval_id: cookie_agent_protocol::ApprovalId,
    event_run_id: Option<cookie_agent_protocol::RunId>,
) -> Result<(), EventLogError> {
    let Some(owner) = owners.get(&approval_id) else {
        return corrupt(
            path,
            "approval lifecycle event appeared before ApprovalRequested",
        );
    };
    if event_run_id != Some(owner.run) {
        return corrupt(path, "approval lifecycle event uses a non-owning run_id");
    }
    Ok(())
}

fn corrupt<T>(path: &Path, message: impl Into<String>) -> Result<T, EventLogError> {
    Err(EventLogError::Corrupt {
        path: path.to_owned(),
        message: message.into(),
    })
}

fn corrupt_value<T>(path: &Path, message: impl Into<String>) -> Result<T, EventLogError> {
    Err(EventLogError::Corrupt {
        path: path.to_owned(),
        message: message.into(),
    })
}

/// The records of the session log at `path`, each read whole, the values
/// history format 2 keeps in the tree's blobs read back: for tools and
/// tests that look at a log's lines without opening it, which validates
/// them.
pub fn load_whole_records(path: &Path) -> Result<Vec<StoredEvent>, EventLogError> {
    let blob_dir = blobs::tree_blob_dir(path);
    let json_error = |source| EventLogError::Json {
        path: path.to_owned(),
        source,
    };
    let bytes = fs::read(path).map_err(|source| EventLogError::Io {
        path: path.to_owned(),
        source,
    })?;
    cookie_agent_protocol::read_stored_history(|| {
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut value = serde_json::from_slice::<Value>(line).map_err(json_error)?;
                if let Some(payload) = value.get_mut("payload") {
                    blobs::internalize(payload, &mut |digest| blobs::read(&blob_dir, digest))
                        .map_err(|message| EventLogError::Corrupt {
                            path: path.to_owned(),
                            message,
                        })?;
                }
                serde_json::from_value(value).map_err(json_error)
            })
            .collect()
    })
}

/// Reads a JSONL file after removing a crash-torn final record.  A malformed
/// complete record is corruption, not a torn tail, and is rejected.
pub fn load_jsonl<T>(path: &Path) -> Result<Vec<T>, EventLogError>
where
    T: for<'de> Deserialize<'de>,
{
    load_jsonl_with_policy(path, TornTail::Truncate)
}

pub fn load_jsonl_shared<T>(path: &Path) -> Result<Vec<T>, EventLogError>
where
    T: for<'de> Deserialize<'de>,
{
    load_jsonl_with_policy(path, TornTail::Ignore)
}

fn load_jsonl_with_policy<T>(path: &Path, torn_tail: TornTail) -> Result<Vec<T>, EventLogError>
where
    T: for<'de> Deserialize<'de>,
{
    let (bytes, _) = read_complete_jsonl(path, torn_tail)?;
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line).map_err(|source| EventLogError::Json {
                path: path.to_owned(),
                source,
            })
        })
        .collect()
}

/// The complete records of `path`, and whether a torn final record was cut
/// from them.
fn read_complete_jsonl(path: &Path, torn_tail: TornTail) -> Result<(Vec<u8>, bool), EventLogError> {
    let mut bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
        Err(source) => {
            return Err(EventLogError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let torn = !bytes.is_empty() && !bytes.ends_with(b"\n");
    if torn {
        let length = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        bytes.truncate(length);
        if matches!(torn_tail, TornTail::Truncate) {
            let file = OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|source| EventLogError::Io {
                    path: path.to_owned(),
                    source,
                })?;
            file.set_len(length as u64)
                .and_then(|()| file.sync_all())
                .map_err(|source| EventLogError::Io {
                    path: path.to_owned(),
                    source,
                })?;
        }
    }
    Ok((bytes, torn))
}

/// The records of the event file at `path` from byte `offset` on, through
/// its last complete line, each read as the loader reads an intact record:
/// its values history format 2 keeps in the tree's `blobs/` read back
/// ([`blobs::internalize`]), then typed as it is
/// ([`StoredEvent::decode_strict`]). A torn final record is left out, as a
/// read-only open leaves it. `None` when `offset` does not start a line, a
/// line there does not read that way (a blob is missing, or the loader would
/// repair or skip it), or the sequences do not increase: such a file is for
/// the loader, which repairs, skips, and reports.
pub fn read_records_from(
    path: &Path,
    offset: u64,
) -> Result<Option<Vec<StoredEvent>>, EventLogError> {
    use std::io::{Read as _, Seek as _};
    let io_error = |source| EventLogError::Io {
        path: path.to_owned(),
        source,
    };
    let mut file = fs::File::open(path).map_err(io_error)?;
    // The byte before `offset` ends the previous line.
    let from = offset.saturating_sub(1);
    file.seek(io::SeekFrom::Start(from)).map_err(io_error)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(io_error)?;
    drop(file);
    let bytes = if offset == 0 {
        &bytes[..]
    } else {
        match bytes.split_first() {
            Some((b'\n', rest)) => rest,
            _ => return Ok(None),
        }
    };
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    let blob_dir = blobs::tree_blob_dir(path);
    cookie_agent_protocol::read_stored_history(|| {
        let mut records: Vec<StoredEvent> = Vec::new();
        for line in bytes[..complete]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record = if blobs::has_references(line) {
                read_restored(line, &blob_dir)
            } else {
                StoredEvent::decode_strict(line)
            };
            let Some(record) = record else {
                return Ok(None);
            };
            if records.last().is_some_and(|last| last.seq >= record.seq) {
                return Ok(None);
            }
            records.push(record);
        }
        Ok(Some(records))
    })
}

/// The length `truncate_event_file` keeps of `bytes`, found by reading the
/// sequence of each line.
pub fn scanned_cut_length(bytes: &[u8], through_seq: u64) -> u64 {
    #[derive(Deserialize)]
    struct Sequenced {
        seq: u64,
    }
    let mut length = 0_usize;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if serde_json::from_slice::<Sequenced>(line).is_ok_and(|record| record.seq > through_seq) {
            break;
        }
        length += line.len();
    }
    length as u64
}

/// Cuts `path` to its first `length` bytes and syncs it.
fn cut_event_file(path: &Path, length: u64) -> Result<(), EventLogError> {
    let io_error = |source| EventLogError::Io {
        path: path.to_owned(),
        source,
    };
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io_error)?;
    file.set_len(length)
        .and_then(|()| file.sync_all())
        .map_err(io_error)
}

pub fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<(), EventLogError> {
    let bytes = serde_json::to_vec(value).map_err(|source| EventLogError::Json {
        path: path.to_owned(),
        source,
    })?;
    let created = !path.exists();
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| EventLogError::Io {
        path: path.to_owned(),
        source,
    })?;
    file.write_all(&bytes)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_data())
        .map_err(|source| EventLogError::Io {
            path: path.to_owned(),
            source,
        })?;
    if created && let Some(parent) = path.parent() {
        fsync_directory(parent)?;
    }
    Ok(())
}

/// The line `event` is stored as, newline excluded: serialized in the
/// storage encoding, its large values moved to blobs ([`blobs`]), each
/// handed to `keep` with its digest.
fn stored_line(
    event: &StoredEvent,
    keep: &mut dyn FnMut(&str, Vec<u8>),
) -> Result<Vec<u8>, serde_json::Error> {
    cookie_agent_protocol::write_stored_history(|| {
        if !blobs::may_move(&event.payload) {
            return serde_json::to_vec(event);
        }
        let mut value = serde_json::to_value(event)?;
        if let Some(payload) = value.get_mut("payload") {
            blobs::externalize(payload, keep);
        }
        serde_json::to_vec(&value)
    })
}

/// Writes `events` as the content of the new log at `path` through one
/// buffered writer and syncs the file once. Every line `raw` holds (each
/// record read from disk, and each line the read skipped or quarantined) is
/// copied as it was read, under `session_id`, so members this build does not
/// know survive; only the records appended since the read are serialized.
/// The caller syncs the directory that holds it.
fn write_event_jsonl_in_place(
    path: &Path,
    session_id: SessionId,
    events: &[Arc<StoredEvent>],
    raw: &ReadRawLines<'_>,
) -> Result<LineIndex, EventLogError> {
    let io_error = |source| EventLogError::Io {
        path: path.to_owned(),
        source,
    };
    let json_error = |source| EventLogError::Json {
        path: path.to_owned(),
        source,
    };
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut writer = io::BufWriter::new(options.open(path).map_err(io_error)?);
    let mut lines = LineIndex::default();
    let mut record = Vec::new();
    let mut moved = blobs::Blobs::new();
    let mut copied = raw.raw.lines.iter().peekable();
    let mut events = events.iter().peekable();
    loop {
        let next_copied = copied.peek().map(|line| line.seq);
        let next_event = events.peek().map(|event| event.seq);
        let seq = match (next_copied, next_event) {
            (None, None) => break,
            (Some(copied), Some(event)) => copied.min(event),
            (Some(seq), None) | (None, Some(seq)) => seq,
        };
        record.clear();
        let line = copied.next_if(|line| line.seq == seq);
        // The record read from this line, if any, is written as read.
        let read = line.and_then(|line| {
            let event = events.peek().filter(|event| event.seq == seq);
            raw.bytes(line, event.map(AsRef::as_ref))
        });
        if let Some(bytes) = read {
            copy_raw_line(bytes, session_id, &mut record).map_err(json_error)?;
            events.next_if(|event| event.seq == seq);
        } else if line.is_some() && events.peek().is_none_or(|event| event.seq != seq) {
            // A line with neither its bytes nor a record: nothing to write.
            continue;
        } else {
            let event = events.next().expect("peeked event");
            record = stored_line(event, &mut |digest, bytes| {
                moved.insert(digest.to_owned(), bytes);
            })
            .map_err(json_error)?;
        }
        record.push(b'\n');
        writer.write_all(&record).map_err(io_error)?;
        lines.push(Some(seq), record.len() as u64);
    }
    let file = writer
        .into_inner()
        .map_err(|error| io_error(error.into_error()))?;
    let directory = blobs::tree_blob_dir(path);
    blobs::write_synced(&directory, &moved).map_err(|source| EventLogError::Io {
        path: directory,
        source,
    })?;
    file.sync_all().map_err(io_error)?;
    Ok(lines)
}

/// Appends `line` to `record`, with its envelope's `session_id` set to
/// `session_id` when it names another session (a fork's copy).
fn copy_raw_line(
    line: &[u8],
    session_id: SessionId,
    record: &mut Vec<u8>,
) -> Result<(), serde_json::Error> {
    let mut value = serde_json::from_slice::<Value>(line)?;
    let session = serde_json::to_value(session_id)?;
    match value.get_mut("session_id") {
        Some(current) if *current == session => record.extend_from_slice(line),
        Some(current) => {
            *current = session;
            serde_json::to_writer(record, &value)?;
        }
        None => record.extend_from_slice(line),
    }
    Ok(())
}

#[cfg(unix)]
pub fn fsync_directory(path: &Path) -> Result<(), EventLogError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| EventLogError::Io {
            path: path.to_owned(),
            source,
        })
}

#[cfg(windows)]
pub fn fsync_directory(path: &Path) -> Result<(), EventLogError> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .and_then(|directory| match directory.sync_all() {
            Ok(()) => Ok(()),
            Err(error) if matches!(error.raw_os_error(), Some(1 | 5 | 6 | 50)) => Ok(()),
            Err(error) => Err(error),
        })
        .map_err(|source| EventLogError::Io {
            path: path.to_owned(),
            source,
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputMessage {
    Delta(OutputDelta),
    Gap(OutputGap),
}

#[derive(Clone, Debug)]
struct Chunk {
    offset: u64,
    data: Vec<u8>,
}

#[derive(Debug, Default)]
struct StreamBuffer {
    start: u64,
    end: u64,
    chunks: VecDeque<Chunk>,
}

#[derive(Debug)]
struct Subscriber {
    sender: mpsc::Sender<OutputMessage>,
    gap: Option<u64>,
}

#[derive(Debug)]
struct HubState {
    streams: std::collections::HashMap<String, StreamBuffer>,
    subscribers: std::collections::HashMap<String, Vec<Subscriber>>,
    declaration: Vec<OutputStream>,
    finalized: bool,
}

/// Bounded retained output with atomic snapshot-to-live subscription handoff.
#[derive(Clone, Debug)]
pub struct OutputHub {
    call_id: ToolCallId,
    limit: usize,
    state: Arc<Mutex<HubState>>,
}

impl OutputHub {
    #[must_use]
    pub fn new(call_id: ToolCallId, retention_bytes: usize) -> Self {
        Self {
            call_id,
            limit: retention_bytes,
            state: Arc::new(Mutex::new(HubState {
                streams: std::collections::HashMap::new(),
                subscribers: std::collections::HashMap::new(),
                declaration: vec![OutputStream::Stdout, OutputStream::Stderr],
                finalized: false,
            })),
        }
    }

    pub fn declare(&self, declaration: &cookie_agent_protocol::ToolOutputDeclaration) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.declaration = declaration
            .channels()
            .iter()
            .map(|name| OutputStream::from_channel(name.as_deref()))
            .collect();
    }

    pub fn streams(&self) -> Vec<OutputStream> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .declaration
            .clone()
    }

    pub fn emit(&self, stream: OutputStream, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.finalized {
            return;
        }
        let end = {
            let buffer = state.streams.entry(stream.name().to_owned()).or_default();
            buffer.chunks.push_back(Chunk {
                offset: buffer.end,
                data: data.to_vec(),
            });
            buffer.end += data.len() as u64;
            // The retained chunks are contiguous, from `start` to `end`.
            while buffer.end - buffer.start > self.limit as u64 {
                let Some(chunk) = buffer.chunks.pop_front() else {
                    break;
                };
                buffer.start = chunk.offset + chunk.data.len() as u64;
            }
            buffer.end
        };
        // Only a live subscriber takes the delta, so output nobody watches is
        // never encoded.
        let Some(subscribers) = state
            .subscribers
            .get_mut(stream.name())
            .filter(|subscribers| !subscribers.is_empty())
        else {
            return;
        };
        let delta = OutputDelta {
            call_id: self.call_id,
            stream: stream.clone(),
            byte_offset: end - data.len() as u64,
            data: STANDARD.encode(data),
        };
        subscribers.retain_mut(|subscriber| {
            if let Some(next_offset) = subscriber.gap {
                match subscriber.sender.try_send(OutputMessage::Gap(OutputGap {
                    call_id: self.call_id,
                    stream: stream.clone(),
                    next_offset,
                })) {
                    Ok(()) => subscriber.gap = None,
                    Err(mpsc::error::TrySendError::Full(_)) => return true,
                    Err(mpsc::error::TrySendError::Closed(_)) => return false,
                }
            }
            match subscriber
                .sender
                .try_send(OutputMessage::Delta(delta.clone()))
            {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    subscriber.gap = Some(end);
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }

    /// Holds the same lock while taking the snapshot and registering the
    /// receiver, so subsequent deltas cannot fall into a handoff gap.
    #[must_use]
    pub fn subscribe(
        &self,
        stream: OutputStream,
        queue: usize,
    ) -> (OutputSnapshot, mpsc::Receiver<OutputMessage>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = stream.name().to_owned();
        let buffer = state.streams.entry(index.clone()).or_default();
        let snapshot = OutputSnapshot {
            call_id: self.call_id,
            start_offset: buffer.start,
            end_offset: buffer.end,
            chunks: buffer
                .chunks
                .iter()
                .map(|chunk| OutputDelta {
                    call_id: self.call_id,
                    stream: stream.clone(),
                    byte_offset: chunk.offset,
                    data: STANDARD.encode(&chunk.data),
                })
                .collect(),
        };
        let (sender, receiver) = mpsc::channel(queue);
        // A nonzero retained start is an explicit loss boundary for a new
        // snapshot consumer. Queue the same marker used for lagging live
        // subscribers before any later deltas can be registered.
        let gap = (buffer.start > 0).then_some(buffer.start);
        let gap = match gap {
            Some(next_offset) => match sender.try_send(OutputMessage::Gap(OutputGap {
                call_id: self.call_id,
                stream: stream.clone(),
                next_offset,
            })) {
                Ok(()) => None,
                Err(mpsc::error::TrySendError::Full(_)) => Some(next_offset),
                Err(mpsc::error::TrySendError::Closed(_)) => None,
            },
            None => None,
        };
        // A retained finalized hub is a snapshot-only resource. Do not retain a
        // sender for a receiver which can never receive another delta.
        if !state.finalized {
            state
                .subscribers
                .entry(index)
                .or_default()
                .push(Subscriber { sender, gap });
        }
        (snapshot, receiver)
    }

    /// The output bytes the hub holds, across its streams.
    pub fn retained_bytes(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .streams
            .values()
            .map(|buffer| buffer.end - buffer.start)
            .sum()
    }

    /// [`Self::finalize`], answering whether this call finalized the hub
    /// rather than an earlier one.
    pub fn finalize_once(&self) -> bool {
        let finalized = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .finalized;
        !finalized && {
            self.finalize();
            true
        }
    }

    /// Prevents late producer clones from publishing after the owning call has
    /// been committed as complete and closes every live subscription.
    pub fn finalize(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.finalized = true;
        state.subscribers.clear();
    }
}

/// The serialized size past which a limited replay page ends. Small
/// enough that a page crosses a slow link (a tunnel, a phone) well inside
/// the client's page timeout, large enough that a long replay is not
/// mostly round trips.
pub const REPLAY_PAGE_MAX_BYTES: usize = 1024 * 1024;

/// An event's serialized size, counted without building its JSON.
fn serialized_len(event: &StoredEvent) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, event).expect("a stored event serializes");
    counter.0
}

/// The prompt blocks `spans` locate in `prompt`, in order; `None` when a
/// span does not fit the prompt.
pub fn prompt_block_texts(
    prompt: &str,
    spans: &[cookie_agent_protocol::PromptBlockSpan],
) -> Option<Vec<(String, String)>> {
    spans
        .iter()
        .map(|span| {
            let start = usize::try_from(span.start).ok()?;
            let end = start.checked_add(usize::try_from(span.len).ok()?)?;
            Some((span.key.clone(), prompt.get(start..end)?.to_owned()))
        })
        .collect()
}

pub mod convert;

#[cfg(test)]
mod tests;

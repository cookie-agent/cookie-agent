//! A session's transcript items as HTML, from the shared reducer's
//! projection. Each item renders on its own, so the page re-renders only
//! the items whose [`item_key`] changed; an assistant reply renders part by
//! part ([`transcript_rows`]), so a streaming reply draws again only the
//! parts that changed, and its text only its open tail.

use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasher as _, Hash, Hasher as _},
    sync::Arc,
};

use cookie_agent_client_state::transcript_mode::{
    self, BlockFold, ChildFold, FoldPlan, ItemFold, TranscriptMode, WorkKey,
};
use cookie_agent_client_state::{
    AssistantChild, CompactingSource, EventLevel, ModelDisplayNames, ProducerMessageStatus,
    SessionState, ToolCallState, ToolStatus, TranscriptItem,
    rows::{
        AGENT_COMPACTION_HEADER, MAX_SYSTEM_PROMPT_BODY_BYTES, MAX_SYSTEM_PROMPT_BODY_LINES,
        agent_background_ended_details, agent_background_ended_summary,
        agent_session_message_summary, bounded_text, bounded_tools, display_line_count,
        goal_status_label, model_tool_groups, model_tools_summary, producer_mode_label,
        producer_owner_label, producer_summary, system_prompt_source, system_prompt_summary,
    },
};
use cookie_agent_protocol::{
    AcpMessageDirection, ContextCheckpoint, ExtensionMessageRole, PersistedFileSource, ToolCallId,
};

use super::{
    blocks::{BlockPlace, display_html, media_html},
    budget::{ALL_OUTPUT_LINES, OUTPUT_LINES, within_budget},
    escape,
    markdown::{block_html, document_html},
};
use crate::icons::{self, Icon, svg};

/// What the page shows besides the projection itself.
pub struct TranscriptView<'a> {
    /// The session the projection is of: item IDs repeat across sessions.
    pub session_id: cookie_agent_protocol::SessionId,
    pub state: &'a SessionState,
    pub names: &'a ModelDisplayNames,
    /// Diagnostic rows below this level are hidden (`/events`).
    pub minimum_level: EventLevel,
    /// What the user opened.
    pub expanded: &'a Expanded,
    /// The subagents this session's delegate calls started, by call.
    pub delegates: &'a HashMap<ToolCallId, Delegate>,
    /// Every runtime's home: a tool row shows its runtime's paths as
    /// `~/…`.
    pub homes: &'a cookie_agent_client_state::paths::RuntimeHomes,
    /// How the transcript shows agent work.
    pub transcript: TranscriptMode,
    /// The time live work rows count their elapsed time to.
    pub now: jiff::Timestamp,
}

/// A fingerprint of `homes`, which every cached row depends on.
pub fn homes_key(homes: &cookie_agent_client_state::paths::RuntimeHomes) -> u64 {
    foldhash::fast::FixedState::default().hash_one(format!("{homes:?}"))
}

/// What the user opened in one session's transcript: tool calls, the full
/// output of calls, thinking blocks, and other rows' blocks, each under its
/// own kind of key, so a pass looks a call up by its ID.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Expanded {
    tools: FastSet<ToolCallId>,
    all_output: FastSet<ToolCallId>,
    thinking: FastSet<u64>,
    blocks: FastSet<String>,
}

impl Expanded {
    /// Opens or closes what `key`, a row's `data-call` or
    /// `data-block-key`, names: a call, a call's full output (its ID and
    /// `:all`), a thinking block (`thinking:` and its segment), or another
    /// row's block.
    pub fn toggle(&mut self, key: &str) {
        fn flip<T: Eq + Hash>(set: &mut FastSet<T>, key: T) {
            if !set.remove(&key) {
                set.insert(key);
            }
        }
        if let Some(id) = key.strip_prefix("thinking:").and_then(|id| id.parse().ok()) {
            flip(&mut self.thinking, id);
        } else if let Some(call) = key.strip_suffix(":all").and_then(|call| call.parse().ok()) {
            flip(&mut self.all_output, call);
        } else if let Ok(call) = key.parse() {
            flip(&mut self.tools, call);
        } else {
            flip(&mut self.blocks, key.to_owned());
        }
    }

    /// Whether the call is open.
    pub fn tool(&self, call: &ToolCallId) -> bool {
        self.tools.contains(call)
    }

    /// Whether the call shows its output past the row budget.
    pub fn all_output(&self, call: &ToolCallId) -> bool {
        self.all_output.contains(call)
    }

    /// Whether the thinking segment that starts at `id` is open.
    pub fn thinking(&self, id: u64) -> bool {
        self.thinking.contains(&id)
    }

    /// Whether the row whose block key is `key` is open.
    pub fn block(&self, key: &str) -> bool {
        self.blocks.contains(key)
    }

    /// Whether the reduced mode's work row of the fold `key` is open.
    pub fn work(&self, key: WorkKey) -> bool {
        self.blocks.contains(&work_key(key))
    }

    /// Folds every work row, as a switch to the reduced mode does.
    pub fn fold_work(&mut self) {
        self.blocks.retain(|key| !key.starts_with(WORK_KEY_PREFIX));
    }
}

const WORK_KEY_PREFIX: &str = "work:";

/// The block key of the work row of the fold `key`.
pub fn work_key(key: WorkKey) -> String {
    format!("{WORK_KEY_PREFIX}{}:{}", key.item, key.child)
}

/// The characters a live work row's line has before a streaming one shows
/// unfinished: about what a desktop window's row fits.
const WORK_LINE_ROOM: usize = 96;

/// The keys opened, each as [`Expanded::toggle`] takes it.
impl<S: AsRef<str>> FromIterator<S> for Expanded {
    fn from_iter<I: IntoIterator<Item = S>>(keys: I) -> Self {
        let mut expanded = Self::default();
        for key in keys {
            expanded.toggle(key.as_ref());
        }
        expanded
    }
}

/// What every key of one pass over the transcript depends on, worked out
/// once for the pass.
struct Pass<'a, 'v> {
    view: &'a TranscriptView<'v>,
    homes: u64,
    /// The calls waiting for the user's approval.
    waiting: FastSet<ToolCallId>,
    /// The reduced mode's folds; none in the full mode.
    folds: FoldPlan,
}

impl<'a, 'v> Pass<'a, 'v> {
    fn new(view: &'a TranscriptView<'v>) -> Self {
        Self {
            view,
            homes: homes_key(view.homes),
            waiting: view
                .state
                .approvals
                .iter()
                .filter(|approval| approval.is_visible_user_escalation())
                .map(|approval| approval.tool_call_id)
                .collect(),
            folds: match view.transcript {
                TranscriptMode::Reduced => FoldPlan::of(view.state),
                TranscriptMode::Full => FoldPlan::default(),
            },
        }
    }

    /// Whether the work row of the fold `key` is open.
    fn open(&self, key: WorkKey) -> bool {
        self.view.expanded.work(key)
    }

    /// Where the assistant block `item_id` stands in a folded run.
    fn block_fold(&self, item_id: u64) -> Option<&BlockFold> {
        match self.folds.get(item_id)? {
            ItemFold::Block(fold) => Some(fold),
            ItemFold::Row { .. } => None,
        }
    }

    /// Whether a folded run's work hides all of `item`: a background row
    /// inside it, or a block between the run's first and last whose every
    /// child is folded work, no call waiting.
    fn hidden(&self, item: &TranscriptItem) -> bool {
        match (self.folds.get(item.id()), item) {
            (Some(ItemFold::Row { key }), _) => !self.open(*key),
            (Some(ItemFold::Block(fold)), TranscriptItem::Assistant { children, .. }) => {
                !fold.first
                    && !fold.last
                    && fold.children.iter().zip(children).all(|(place, child)| {
                        matches!(place, ChildFold::Work { key, starts: false } if !self.open(*key))
                            && !matches!(child, AssistantChild::Tool { call_id } if self.waiting.contains(call_id))
                    })
            }
            _ => false,
        }
    }

    /// The HTML of the work row of the fold `key`.
    fn work_row(&self, key: WorkKey) -> Option<String> {
        let view = self.view;
        let row = self
            .folds
            .work_row(view.state, key, view.now, WORK_LINE_ROOM)?;
        Some(work_row_html(key, &row, self.open(key)))
    }

    /// Hashes what a call's row shows besides the call's own revision.
    fn hash_tool(&self, call_id: &ToolCallId, hasher: &mut impl std::hash::Hasher) {
        let view = self.view;
        call_id.hash(hasher);
        view.expanded.tool(call_id).hash(hasher);
        view.expanded.all_output(call_id).hash(hasher);
        // Raw arguments show only at `info` and below.
        (view.minimum_level <= EventLevel::Info).hash(hasher);
        view.state
            .tools
            .get(call_id)
            .map(|tool| tool.revision)
            .hash(hasher);
        self.waiting.contains(call_id).hash(hasher);
        view.delegates.get(call_id).hash(hasher);
    }
}

impl TranscriptView<'_> {
    /// Whether `tool` waits for the user's approval.
    fn waiting_approval(&self, tool: &ToolCallState) -> bool {
        self.state.approvals.iter().any(|approval| {
            approval.tool_call_id == tool.id && approval.is_visible_user_escalation()
        })
    }
}

/// The subagent a delegate call started, as its row shows it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Delegate {
    pub session_id: cookie_agent_protocol::SessionId,
    pub agent: String,
    pub handle: Option<String>,
    /// What it does while it runs.
    pub activity: Option<crate::tree::Activity>,
}

const NOTE: &str = "text-xs text-ink dark:text-sand";

/// The maps and sets a pass fills, under a fast fixed hash: their keys are
/// hashes and IDs already, and a pass looks up every part of every row.
pub type FastMap<K, V> = HashMap<K, V, foldhash::fast::FixedState>;
type FastSet<T> = HashSet<T, foldhash::fast::FixedState>;

fn fast_map<K, V>(capacity: usize) -> FastMap<K, V> {
    HashMap::with_capacity_and_hasher(capacity, foldhash::fast::FixedState::default())
}

/// A key that changes whenever the item's HTML would: its version, the
/// revisions of the tool calls it shows, and their expansion.
pub fn item_key(view: &TranscriptView<'_>, item: &TranscriptItem) -> u64 {
    item_key_in(&Pass::new(view), item)
}

fn item_key_in(pass: &Pass<'_, '_>, item: &TranscriptItem) -> u64 {
    let view = pass.view;
    let mut hasher = foldhash::fast::FixedState::default().build_hasher();
    view.session_id.hash(&mut hasher);
    pass.homes.hash(&mut hasher);
    item.id().hash(&mut hasher);
    item.version().hash(&mut hasher);
    std::mem::discriminant(item).hash(&mut hasher);
    // Where it stands in a fold, whether the fold is open, and on its first
    // block the work row, which says what the work does while it runs.
    if let Some(fold) = pass.folds.get(item.id()) {
        fold.hash(&mut hasher);
        let keys = match fold {
            ItemFold::Row { key } => vec![(*key, false)],
            ItemFold::Block(block) => block
                .children
                .iter()
                .filter_map(|child| match child {
                    ChildFold::Work { key, starts } => Some((*key, *starts)),
                    ChildFold::Shown => None,
                })
                .collect(),
        };
        for (key, starts) in keys {
            pass.open(key).hash(&mut hasher);
            if starts {
                pass.work_row(key).hash(&mut hasher);
            }
        }
    }
    if let TranscriptItem::Assistant { id, children, .. } = item {
        // The footer's inputs: metrics can land after the reply settles.
        view.state.assistant_metrics.get(id).hash(&mut hasher);
        view.state
            .interrupted_assistant_items
            .contains(id)
            .hash(&mut hasher);
        children.len().hash(&mut hasher);
        for child in children {
            child.version().hash(&mut hasher);
            match child {
                AssistantChild::Thinking { id: part, .. } => {
                    view.expanded.thinking(*part).hash(&mut hasher);
                    hash_thinking_status(view.state, *id, *part, &mut hasher);
                }
                AssistantChild::Tool { call_id } => pass.hash_tool(call_id, &mut hasher),
                _ => {}
            }
        }
    }
    if let Some(key) = block_key(item) {
        view.expanded.block(&key).hash(&mut hasher);
    }
    if let TranscriptItem::ProducerMessage { .. } = item {
        (view.minimum_level == EventLevel::Debug).hash(&mut hasher);
    }
    if let TranscriptItem::Event { level, .. } = item {
        (*level >= view.minimum_level).hash(&mut hasher);
    }
    hasher.finish()
}

/// The item as HTML; `None` for a diagnostic row the level filter hides.
pub fn item_html(view: &TranscriptView<'_>, item: &TranscriptItem) -> Option<String> {
    Some(match item {
        TranscriptItem::User { markdown, seq, .. } => {
            let files = view
                .state
                .user_files
                .get(seq)
                .map(|files| {
                    files
                        .iter()
                        .map(|file| {
                            media_html(
                                &file.sha256,
                                &file.media_type,
                                file.byte_length,
                                file.filename.as_deref(),
                            )
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            format!(
                // The row is the whole band at the bubble's height: a mouse
                // over it shows the ⋯ (style/main.css), whose room stays;
                // a touch screen long-presses the bubble instead.
                "<div class=\"user-row flex justify-end\" data-seq=\"{seq}\"><div class=\"user-message group ml-auto flex w-fit max-w-[80%] items-start justify-end gap-1\" data-seq=\"{seq}\"><button type=\"button\" data-action=\"user-menu\" data-seq=\"{seq}\" aria-label=\"Message actions\" title=\"Message actions\" class=\"inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-md text-ink hover:bg-toasted active:bg-crust dark:text-sand dark:hover:bg-ember dark:active:bg-smoke\">{}</button><div class=\"min-w-0 rounded-2xl rounded-tr-sm bg-toasted px-3 py-2 dark:bg-ember\" data-bubble><div class=\"markdown\">{}</div>{files}</div></div></div>",
                svg(icondata::FaEllipsisSolid, ""),
                document_html(markdown)
            )
        }
        TranscriptItem::Assistant {
            id,
            attribution,
            children,
            ..
        } => {
            let mut html = format!(
                "<div class=\"{NOTE} font-mono\">{}</div>",
                escape(&attribution.header(view.names))
            );
            for child in children {
                html.push_str(&child_html(view, *id, child));
            }
            html.push_str(&reply_footer_html(view.state, *id).unwrap_or_default());
            format!("<div class=\"flex flex-col gap-1\">{html}</div>")
        }
        TranscriptItem::Event {
            level,
            text,
            repeat,
            ..
        } => {
            if *level < view.minimum_level {
                return None;
            }
            let class = match level {
                EventLevel::Error => "text-red-700 dark:text-red-400",
                EventLevel::Warning => "text-caramel dark:text-maple",
                EventLevel::Info | EventLevel::Debug => "text-ink dark:text-sand",
            };
            let repeat = if *repeat > 1 {
                format!(" ×{repeat}")
            } else {
                String::new()
            };
            format!(
                "<div class=\"font-mono text-xs whitespace-pre-wrap {class}\">{} {}{repeat}</div>",
                level.badge(),
                escape(text)
            )
        }
        TranscriptItem::Compacting { source, .. } => {
            let key = block_key(item).unwrap_or_default();
            let body = match source {
                CompactingSource::Summary { text, .. } if text.is_empty() => {
                    "waiting for the summary".to_owned()
                }
                CompactingSource::Summary { text, .. } => text.clone(),
                CompactingSource::Native { model } => {
                    format!("calling native compaction endpoint ({model})")
                }
                CompactingSource::Agent { .. } => {
                    "the agent is compacting its conversation".to_owned()
                }
            };
            collapsible(
                view.expanded.block(&key),
                &key,
                icondata::FaBroomSolid,
                "compacting context…",
                None,
                &body,
            )
        }
        TranscriptItem::Compaction { commit, .. } => {
            let key = block_key(item).unwrap_or_default();
            let (kind, body) = match &commit.checkpoint {
                // The summary as the model reads it, in its frame.
                ContextCheckpoint::InternalSummary { checkpoint, .. } => (
                    "internal summary",
                    cookie_agent_protocol::model_text::framed_compaction_summary(
                        checkpoint.summary(),
                    ),
                ),
                ContextCheckpoint::NativeWindow { window } => {
                    let scope = window.scope();
                    (
                        "native window",
                        format!(
                            "adapter: {}\nmodel: {}/{}\nresource: {}",
                            window.adapter_id(),
                            scope.provider_id,
                            scope.model_id,
                            scope.resource_id
                        ),
                    )
                }
            };
            let header = format!(
                "context compacted ({kind}, {}→{} tokens)",
                commit.budgets.input_tokens_before, commit.budgets.input_tokens_after
            );
            collapsible(
                view.expanded.block(&key),
                &key,
                icondata::FaBroomSolid,
                &header,
                None,
                &body,
            )
        }
        TranscriptItem::AgentCompaction { summary, .. } => {
            let key = block_key(item).unwrap_or_default();
            collapsible(
                view.expanded.block(&key),
                &key,
                icondata::FaBroomSolid,
                AGENT_COMPACTION_HEADER,
                None,
                summary.as_deref().unwrap_or("the agent kept no summary"),
            )
        }
        TranscriptItem::AgentSessionMessage { message, .. } => {
            let key = block_key(item).unwrap_or_default();
            let icon = if message.direction == AcpMessageDirection::Sent {
                icondata::FaPaperPlaneSolid
            } else {
                icondata::FaInboxSolid
            };
            collapsible(
                view.expanded.block(&key),
                &key,
                icon,
                &agent_session_message_summary(message),
                None,
                &message.text,
            )
        }
        TranscriptItem::AgentBackgroundEnded {
            ended_at, ended, ..
        } => {
            let key = block_key(item).unwrap_or_default();
            collapsible(
                view.expanded.block(&key),
                &key,
                icondata::FaTerminalSolid,
                &agent_background_ended_summary(ended),
                Some(&agent_background_ended_details(ended, *ended_at)),
                ended.display.as_deref().unwrap_or_default(),
            )
        }
        TranscriptItem::PluginMessage { role, input, .. } => {
            let key = block_key(item).unwrap_or_default();
            let role = match role {
                ExtensionMessageRole::System => "system",
                ExtensionMessageRole::User => "user",
                ExtensionMessageRole::Assistant => "assistant",
                ExtensionMessageRole::Tool => "tool",
            };
            let header = format!(
                "plugin message ({role}, {} lines)",
                display_line_count(input)
            );
            collapsible(
                view.expanded.block(&key),
                &key,
                icondata::FaPuzzlePieceSolid,
                &header,
                None,
                input,
            )
        }
        // The files' header, and opened, the AGENTS.md turn as the model
        // reads it, its framing included.
        TranscriptItem::AgentMd { entries, text, .. } => {
            let key = block_key(item).unwrap_or_default();
            let lines: usize = entries
                .iter()
                .map(|entry| display_line_count(&entry.content))
                .sum();
            let files = match entries.as_slice() {
                [entry] => entry.source.as_str().to_owned(),
                _ => format!("{} files", entries.len()),
            };
            let header = format!("AGENTS.md · {files} ({lines} lines)");
            collapsible(
                view.expanded.block(&key),
                &key,
                icondata::FaBookSolid,
                &header,
                None,
                text,
            )
        }
        // Input the engine added to the model's: collapsed to its header,
        // expanded to the text the model reads.
        TranscriptItem::ModelInput {
            kind, header, text, ..
        } => {
            use cookie_agent_client_state::model_input::ModelInputKind;
            let key = block_key(item).unwrap_or_default();
            let icon = match kind {
                ModelInputKind::Memory => icondata::FaBrainSolid,
                ModelInputKind::PromptBlocks => icondata::FaPenToSquareSolid,
                ModelInputKind::InheritedContext => icondata::FaTurnDownSolid,
                ModelInputKind::AgentContext => icondata::FaScrollSolid,
            };
            let row = collapsible(view.expanded.block(&key), &key, icon, header, None, text);
            format!("<div data-model-input=\"{}\">{row}</div>", kind.as_str())
        }
        // A model's memory change: one line that opens the Memory view at
        // the memory (memory spec section 16.9.10).
        TranscriptItem::MemoryChange { change, url, .. } => {
            let url = escape(url);
            format!(
                "<div class=\"{NOTE}\" data-memory-change><button type=\"button\" data-action=\"open-memory\" data-memory-url=\"{url}\" title=\"Open it in Memory\" class=\"inline-flex min-h-11 items-center gap-1.5 rounded-md px-2 text-left hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke\">{}<span>{}</span><span class=\"font-mono underline decoration-dotted underline-offset-2\">{url}</span></button></div>",
                svg(icondata::FaBrainSolid, "opacity-70"),
                change.verb()
            )
        }
        TranscriptItem::SkillLoaded {
            name,
            source_path,
            args,
            rendered_body,
            ..
        } => {
            let key = block_key(item).unwrap_or_default();
            let header = format!(
                "skill loaded · {name} ({} lines)",
                display_line_count(rendered_body)
            );
            let mut detail = format!("source: {source_path}");
            if !args.is_empty() {
                detail.push_str(&format!("\narguments: {args}"));
            }
            collapsible(
                view.expanded.block(&key),
                &key,
                icondata::FaWandMagicSparklesSolid,
                &header,
                Some(&detail),
                rendered_body,
            )
        }
        TranscriptItem::AcpNotice { notice, .. } => {
            use cookie_agent_protocol::AcpNoticeSeverity;
            let (class, icon) = match notice.severity {
                AcpNoticeSeverity::Error => (
                    "text-red-700 dark:text-red-400",
                    icondata::FaCircleExclamationSolid,
                ),
                AcpNoticeSeverity::Warning => (
                    "text-caramel dark:text-maple",
                    icondata::FaTriangleExclamationSolid,
                ),
                AcpNoticeSeverity::Info | AcpNoticeSeverity::Unknown => {
                    ("text-ink dark:text-sand", icondata::FaCircleInfoSolid)
                }
            };
            let description = notice
                .description
                .as_deref()
                .map(|description| {
                    format!(
                        "<div class=\"{NOTE} whitespace-pre-wrap\">{}</div>",
                        escape(description)
                    )
                })
                .unwrap_or_default();
            format!(
                "<div class=\"agent-notice rounded border-l-4 border-crust px-3 py-1 dark:border-smoke\" data-severity=\"{}\"><div class=\"flex items-center gap-2 text-sm {class}\">{}<span>{}</span></div>{description}</div>",
                match notice.severity {
                    AcpNoticeSeverity::Error => "error",
                    AcpNoticeSeverity::Warning => "warning",
                    _ => "info",
                },
                svg(icon, ""),
                escape(&notice.title)
            )
        }
        TranscriptItem::Plan { entries, .. } => format!(
            "<div class=\"agent-plan rounded border-l-4 border-caramel px-3 py-1 dark:border-maple\"><div class=\"mb-1 flex items-center gap-2 text-sm font-bold\">{}<span>Plan</span></div>{}</div>",
            svg(icondata::FaListCheckSolid, "text-caramel dark:text-maple"),
            super::blocks::plan_html(entries)
        ),
        TranscriptItem::Goal {
            goal, activation, ..
        } => {
            if *activation {
                // The goal the user set, rebuilt from the activation event:
                // shown like a message, with no message menu.
                format!(
                    "<div class=\"goal-activation ml-auto max-w-[90%] rounded-lg border border-caramel px-3 py-2 text-sm whitespace-pre-wrap dark:border-maple\">{}<span class=\"font-bold\">Goal:</span> {}</div>",
                    svg(
                        icondata::FaBullseyeSolid,
                        "mr-1.5 text-caramel dark:text-maple"
                    ),
                    escape(&goal.objective)
                )
            } else {
                let finished = goal.items.iter().filter(|item| item.finished).count();
                let checklist = if goal.items.is_empty() {
                    format!("<div class=\"{NOTE}\">checklist: empty</div>")
                } else {
                    goal.items
                        .iter()
                        .map(|item| {
                            let (icon, state) = if item.finished {
                                (icondata::FaSquareCheckRegular, "finished")
                            } else {
                                (icondata::FaSquareRegular, "open")
                            };
                            format!(
                                "<div class=\"flex items-start gap-2 {NOTE}\">{}<span class=\"sr-only\">{state}:</span><span>{}</span></div>",
                                svg(icon, "mt-0.5"),
                                escape(&item.description)
                            )
                        })
                        .collect()
                };
                format!(
                    "<div class=\"goal-state rounded border-l-4 border-caramel px-3 py-1 dark:border-maple\"><div class=\"flex items-start gap-2 text-sm whitespace-pre-wrap\">{}<span>{}</span></div><div class=\"{NOTE}\">status: {} · {finished}/{} finished</div>{checklist}</div>",
                    svg(
                        icondata::FaBullseyeSolid,
                        "mt-1 text-caramel dark:text-maple"
                    ),
                    escape(&goal.objective),
                    goal_status_label(goal.status),
                    goal.items.len()
                )
            }
        }
        TranscriptItem::ProducerMessage {
            producer_owner,
            mode,
            body,
            summary,
            status,
            ..
        } => match status {
            // A claimed message is already in the running request, so it
            // shows before the response; pending and admitted ones do not
            // show yet, as in the TUI.
            ProducerMessageStatus::Pending | ProducerMessageStatus::Admitted => return None,
            ProducerMessageStatus::Claimed | ProducerMessageStatus::Consumed => {
                let key = block_key(item).unwrap_or_default();
                let state = if *status == ProducerMessageStatus::Claimed {
                    "claimed"
                } else {
                    "consumed"
                };
                let detail = format!(
                    "{} · {} · {state}",
                    producer_owner_label(producer_owner),
                    producer_mode_label(*mode)
                );
                collapsible(
                    view.expanded.block(&key),
                    &key,
                    icondata::FaInboxSolid,
                    &producer_summary(producer_owner, *mode, summary.as_deref()),
                    Some(&detail),
                    body,
                )
            }
            ProducerMessageStatus::Discarded if view.minimum_level == EventLevel::Debug => {
                format!(
                    "<div class=\"{NOTE} font-mono\">{}</div>",
                    escape(&format!(
                        "producer message discarded · {} · {}",
                        producer_owner_label(producer_owner),
                        producer_mode_label(*mode)
                    ))
                )
            }
            ProducerMessageStatus::Discarded => return None,
        },
    })
}

/// One part of an assistant reply as the page draws it, under the part's
/// own node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PartHtml {
    /// The part's HTML, whole.
    Html(Arc<str>),
    /// Text: the HTML of each of its markdown blocks, in order. A settled
    /// block keeps its HTML (and the page its nodes) while the open tail
    /// after it streams.
    Markdown(Arc<[Arc<str>]>),
}

/// One transcript row as the page draws it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RowHtml {
    /// The row's HTML, whole.
    Html(Arc<str>),
    /// An assistant reply: its header, then each part under its ID, so a
    /// part that did not change keeps its node while a later one streams.
    Reply {
        header: Arc<str>,
        parts: Arc<[(u64, PartHtml)]>,
    },
}

/// The transcript's rows: their IDs in order and each one's HTML. A row's
/// ID stays the same while its content changes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Rows {
    pub order: Arc<[u64]>,
    pub html: Arc<FastMap<u64, RowHtml>>,
}

/// The DOM ID of the transcript row whose ID is `id`.
pub fn row_element_id(id: u64) -> String {
    format!("row-{id:x}")
}

/// The DOM ID of the part `part` of the reply in row `row`.
pub fn part_element_id(row: u64, part: u64) -> String {
    format!("part-{row:x}-{part:x}")
}

/// Whether HTML holds what the page fills in after drawing it: a code
/// block to highlight, or media to read.
pub fn fills_in(html: &str) -> bool {
    html.contains("language-") || html.contains("data-media")
}

/// The DOM IDs of the rows, and of the parts of replies, whose HTML changed
/// since `old` and holds something the page fills in after drawing it:
/// only those need looking into. What kept its HTML kept its nodes, filled
/// in already (the page compares HTML as it draws: by content, the same
/// text first by pointer). Text is left out: the page fills in each of its
/// markdown blocks as it adds it.
pub fn to_fill_in(old: &Rows, new: &Rows) -> Vec<String> {
    let mut found = Vec::new();
    for id in new.order.iter().copied() {
        match (old.html.get(&id), new.html.get(&id)) {
            (Some(old), Some(new)) if old == new => {}
            (_, Some(RowHtml::Html(html))) => {
                if fills_in(html) {
                    found.push(row_element_id(id));
                }
            }
            (old, Some(RowHtml::Reply { parts, .. })) => {
                let old = match old {
                    Some(RowHtml::Reply { parts, .. }) => parts
                        .iter()
                        .map(|(part, html)| (*part, html))
                        .collect::<FastMap<_, _>>(),
                    _ => FastMap::default(),
                };
                for (part, html) in parts.iter() {
                    if let PartHtml::Html(html) = html
                        && !matches!(old.get(part), Some(PartHtml::Html(old)) if old == html)
                        && fills_in(html)
                    {
                        found.push(part_element_id(id, *part));
                    }
                }
            }
            (_, None) => {}
        }
    }
    found
}

/// What the last pass drew, by content key, so a row or part whose key did
/// not change is not drawn again. Each pass keeps only what it used.
#[derive(Debug, Default)]
pub struct RowCache {
    rows: FastMap<u64, CachedRow>,
    parts: FastMap<u64, PartHtml>,
    /// Rows and parts drawn anew since the cache was made, for tests that
    /// check what a change draws again.
    pub drawn: u64,
}

#[derive(Debug)]
struct CachedRow {
    html: RowHtml,
    /// The content keys of a reply's parts, kept while the reply is.
    parts: Vec<u64>,
}

/// The ID of the row the system prompt and tools are pinned in.
fn pinned_row_id(session_id: cookie_agent_protocol::SessionId) -> u64 {
    foldhash::fast::FixedState::default().hash_one(("pinned", session_id))
}

/// A row's ID: its item's, which item kinds share, in its session.
fn row_id(view: &TranscriptView<'_>, item: &TranscriptItem) -> u64 {
    foldhash::fast::FixedState::default().hash_one((
        view.session_id,
        std::mem::discriminant(item),
        item.id(),
    ))
}

/// A reply part's ID within its reply: its segment, call, or committed
/// content index; the rest by position.
fn part_id(child: &AssistantChild, index: usize) -> u64 {
    let state = foldhash::fast::FixedState::default();
    match child {
        AssistantChild::Text { id, .. } => state.hash_one((0u8, id)),
        AssistantChild::Thinking { id, .. } => state.hash_one((1u8, id)),
        AssistantChild::Tool { call_id } => state.hash_one((2u8, call_id)),
        AssistantChild::CommittedTool {
            turn_seq,
            content_index,
            ..
        } => state.hash_one((3u8, turn_seq, content_index)),
        AssistantChild::MediaFile {
            turn_seq,
            content_index,
            ..
        } => state.hash_one((4u8, turn_seq, content_index)),
        AssistantChild::Attribution { .. } => state.hash_one((5u8, index)),
        AssistantChild::Notice { .. } => state.hash_one((6u8, index)),
    }
}

/// A key that changes whenever the part's HTML would.
fn part_key(pass: &Pass<'_, '_>, item_id: u64, child: &AssistantChild) -> u64 {
    let view = pass.view;
    let mut hasher = foldhash::fast::FixedState::default().build_hasher();
    // Segment and turn numbers repeat across sessions.
    view.session_id.hash(&mut hasher);
    pass.homes.hash(&mut hasher);
    std::mem::discriminant(child).hash(&mut hasher);
    match child {
        // A folded thinking block shows only its header, whatever streams
        // into it.
        AssistantChild::Thinking { id, version, .. } => {
            let open = view.expanded.thinking(*id);
            (id, open).hash(&mut hasher);
            hash_thinking_status(view.state, item_id, *id, &mut hasher);
            if open {
                version.hash(&mut hasher);
            }
        }
        AssistantChild::Tool { call_id } => pass.hash_tool(call_id, &mut hasher),
        // The rest show what they hold, which does not change.
        AssistantChild::Text { id, version, .. } => (id, version).hash(&mut hasher),
        AssistantChild::CommittedTool {
            turn_seq,
            content_index,
            name,
        } => (turn_seq, content_index, name.as_str()).hash(&mut hasher),
        AssistantChild::MediaFile {
            turn_seq,
            content_index,
            ..
        } => (turn_seq, content_index).hash(&mut hasher),
        AssistantChild::Attribution { resolved_model } => {
            view.names
                .selection_label(&resolved_model.selection)
                .hash(&mut hasher);
        }
        AssistantChild::Notice { text } => text.hash(&mut hasher),
    }
    hasher.finish()
}

/// The transcript's rows: the pinned row (`pinned`, from [`pinned_rows`])
/// first, then every item the view shows. Rows and parts whose content
/// key is in `cache` reuse its HTML.
pub fn transcript_rows(
    view: &TranscriptView<'_>,
    pinned: Option<String>,
    cache: &mut RowCache,
) -> Rows {
    let pass = Pass::new(view);
    let mut rows = fast_map(cache.rows.len());
    let mut parts = fast_map(cache.parts.len());
    let mut order = Vec::with_capacity(view.state.transcript.len() + 1);
    let mut html = fast_map(view.state.transcript.len() + 1);
    if let Some(pinned) = pinned {
        let id = pinned_row_id(view.session_id);
        order.push(id);
        html.insert(id, RowHtml::Html(Arc::from(pinned)));
    }
    for item in &view.state.transcript {
        if pass.hidden(item) {
            continue;
        }
        let key = item_key_in(&pass, item);
        let row = match cache.rows.remove(&key) {
            Some(row) => {
                for key in &row.parts {
                    if let Some(part) = cache.parts.get(key) {
                        parts.insert(*key, part.clone());
                    }
                }
                row
            }
            None => {
                let row = match item {
                    TranscriptItem::Assistant {
                        id,
                        attribution,
                        children,
                        ..
                    } => {
                        let (reply, keys) = reply_parts(&pass, *id, children, cache, &mut parts);
                        // A folded run's later blocks go on under its first
                        // block's header.
                        let headless = pass.block_fold(*id).is_some_and(|fold| !fold.first);
                        CachedRow {
                            html: RowHtml::Reply {
                                header: Arc::from(if headless {
                                    String::new()
                                } else {
                                    format!(
                                        "<div class=\"{NOTE} font-mono\">{}</div>",
                                        escape(&attribution.header(view.names))
                                    )
                                }),
                                parts: reply,
                            },
                            parts: keys,
                        }
                    }
                    _ => match item_html(view, item) {
                        Some(item) => CachedRow {
                            html: RowHtml::Html(Arc::from(item)),
                            parts: Vec::new(),
                        },
                        None => continue,
                    },
                };
                cache.drawn += 1;
                row
            }
        };
        let id = row_id(view, item);
        if html.insert(id, row.html.clone()).is_none() {
            order.push(id);
        }
        rows.insert(key, row);
    }
    cache.rows = rows;
    cache.parts = parts;
    Rows {
        order: order.into(),
        html: Arc::new(html),
    }
}

/// A reply's parts, each under its ID, and their content keys, then its
/// footer when it has one. Parts whose key is in `cache` reuse its HTML;
/// every part drawn lands in `used`.
fn reply_parts(
    pass: &Pass<'_, '_>,
    item_id: u64,
    children: &[AssistantChild],
    cache: &mut RowCache,
    used: &mut FastMap<u64, PartHtml>,
) -> (Arc<[(u64, PartHtml)]>, Vec<u64>) {
    let mut seen: FastSet<u64> =
        HashSet::with_capacity_and_hasher(children.len(), foldhash::fast::FixedState::default());
    let mut keys = Vec::with_capacity(children.len());
    let fold = pass.block_fold(item_id);
    let mut parts = Vec::with_capacity(children.len() + 2);
    // Each fold's work row draws where its work starts.
    let place = |index: usize| {
        fold.and_then(|fold| fold.children.get(index).copied())
            .unwrap_or(ChildFold::Shown)
    };
    let shown = |index: usize, child: &AssistantChild| match place(index) {
        ChildFold::Shown => true,
        ChildFold::Work { key, .. } => {
            pass.open(key)
                || matches!(child, AssistantChild::Tool { call_id } if pass.waiting.contains(call_id))
        }
    };
    for (index, child) in children.iter().enumerate() {
        if let ChildFold::Work { key, starts: true } = place(index)
            && let Some(row) = pass.work_row(key)
        {
            parts.push((
                foldhash::fast::FixedState::default().hash_one((8u8, key)),
                PartHtml::Html(Arc::from(row)),
            ));
        }
        if !shown(index, child) {
            continue;
        }
        parts.push({
            let mut id = part_id(child, index);
            if !seen.insert(id) {
                id = foldhash::fast::FixedState::default().hash_one((id, index));
                seen.insert(id);
            }
            let key = part_key(pass, item_id, child);
            let html = match used.get(&key).or_else(|| cache.parts.get(&key)) {
                Some(html) => html.clone(),
                None => match child {
                    // Each block is drawn once, and kept with its block.
                    AssistantChild::Text { markdown, .. } => {
                        PartHtml::Markdown(markdown.blocks().map(block_html).collect())
                    }
                    _ => {
                        cache.drawn += 1;
                        PartHtml::Html(Arc::from(child_html(pass.view, item_id, child)))
                    }
                },
            };
            used.insert(key, html.clone());
            keys.push(key);
            (id, html)
        });
    }
    // Drawn with its row, whose key holds what it shows; folded, only on a
    // fold's last block.
    if let Some(footer) = fold
        .is_none_or(|fold| fold.last)
        .then(|| reply_footer_html(pass.view.state, item_id))
        .flatten()
    {
        parts.push((
            foldhash::fast::FixedState::default().hash_one(7u8),
            PartHtml::Html(Arc::from(footer)),
        ));
    }
    (parts.into(), keys)
}

/// The reduced mode's row for a block's work: `Worked for 1m 23s · 7 tool
/// calls`, a failed count in the warning colour, and while it works what
/// it does now. A click opens and folds the work.
fn work_row_html(key: WorkKey, row: &transcript_mode::WorkRow, open: bool) -> String {
    let summary = &row.summary;
    let block = work_key(key);
    let chevron = if open {
        icondata::FaChevronDownSolid
    } else {
        icondata::FaChevronRightSolid
    };
    let failed = summary
        .failed_label()
        .map(|failed| {
            format!(
                "<span class=\"text-caramel dark:text-maple\" data-work-failed>{}</span>",
                escape(&failed)
            )
        })
        .unwrap_or_default();
    // What it last said, then its last call, a little smaller. Each line is
    // keyed by its text: a new one is a new node, which settles in
    // (`.work-line`), while the label's ticking time keeps the nodes.
    let line_key = |text: &str| foldhash::fast::FixedState::default().hash_one(text);
    let lines = row
        .lines
        .as_ref()
        .map(|lines| {
            let text = lines
                .text
                .as_deref()
                .map(|text| {
                    format!(
                        "<span class=\"work-line block min-w-0 truncate pl-5 text-sm text-ink/85 dark:text-sand/85\" data-key=\"t{:x}\" data-work-text>{}</span>",
                        line_key(text),
                        escape(text)
                    )
                })
                .unwrap_or_default();
            let tool = lines
                .tool
                .as_deref()
                .map(|tool| {
                    format!(
                        "<span class=\"work-line flex min-w-0 items-center gap-1.5 pl-5 font-mono {NOTE} opacity-75\" data-key=\"c{:x}\" data-work-tool><span aria-hidden=\"true\">↳</span><span class=\"min-w-0 truncate\">{}</span></span>",
                        line_key(tool),
                        escape(tool)
                    )
                })
                .unwrap_or_default();
            format!("<span class=\"flex min-w-0 flex-col gap-0.5\" aria-live=\"polite\" data-work-now>{text}{tool}</span>")
        })
        .unwrap_or_default();
    // A live row spins in place of the trailing working row, which the page
    // hides while the run's latest work is this row's.
    let spinner = if summary.live {
        svg(
            icondata::FaCircleNotchSolid,
            "animate-spin text-caramel dark:text-maple",
        )
    } else {
        String::new()
    };
    format!(
        "<div class=\"collapsible-row\" data-work=\"{}\" data-block-key=\"{block}\"><button type=\"button\" data-action=\"toggle-block\" data-block-key=\"{block}\" aria-expanded=\"{open}\" class=\"flex min-h-11 w-full min-w-0 flex-col justify-center gap-0.5 rounded-md px-2 py-1 text-left text-sm text-ink hover:bg-toasted active:bg-crust dark:text-sand dark:hover:bg-ember dark:active:bg-smoke\"><span class=\"flex min-w-0 items-center gap-2\">{}{spinner}<span class=\"min-w-0 truncate\" data-work-label>{}</span>{failed}</span>{lines}</button></div>",
        if summary.live { "live" } else { "done" },
        svg(chevron, "text-[0.625rem] opacity-70"),
        escape(&summary.label()),
    )
}

/// The line that closes a reply, as the TUI's footer reads: its generation
/// speed, the context it left, its cost, and whether it was interrupted.
fn reply_footer_html(state: &SessionState, item_id: u64) -> Option<String> {
    cookie_agent_client_state::reply::assistant_footer(state, item_id).map(|footer| {
        format!(
            "<div class=\"{NOTE} font-mono\" data-reply-footer>{}</div>",
            escape(&footer)
        )
    })
}

/// What a thinking block's header says: `thinking…` while it streams,
/// then how long it thought.
fn thinking_status(state: &SessionState, item_id: u64, id: u64) -> String {
    if state.is_open_thinking(item_id, id) {
        "thinking…".to_owned()
    } else {
        cookie_agent_client_state::reply::thought_label(state.thinking_duration(item_id, id))
    }
}

/// Hashes what [`thinking_status`] reads.
fn hash_thinking_status(
    state: &SessionState,
    item_id: u64,
    id: u64,
    hasher: &mut impl std::hash::Hasher,
) {
    state.is_open_thinking(item_id, id).hash(hasher);
    state.thinking_duration(item_id, id).hash(hasher);
}

/// The key under which a collapsible row's expansion is kept, shared with
/// the tool calls' in the page's expanded set.
fn block_key(item: &TranscriptItem) -> Option<String> {
    Some(match item {
        TranscriptItem::Compacting { seq, .. } => format!("compacting:{seq}"),
        TranscriptItem::Compaction { seq, .. } | TranscriptItem::AgentCompaction { seq, .. } => {
            format!("compaction:{seq}")
        }
        TranscriptItem::AgentSessionMessage { seq, .. } => format!("session-message:{seq}"),
        TranscriptItem::AgentBackgroundEnded { seq, .. } => format!("background-ended:{seq}"),
        TranscriptItem::PluginMessage { seq, .. } => format!("plugin:{seq}"),
        TranscriptItem::AgentMd { seq, .. } => format!("agent-md:{seq}"),
        TranscriptItem::SkillLoaded { seq, .. } => format!("skill:{seq}"),
        TranscriptItem::ModelInput { seq, .. } => format!("model-input:{seq}"),
        TranscriptItem::ProducerMessage { message_id, .. } => format!("producer:{message_id}"),
        _ => return None,
    })
}

/// The block keys of the rows pinned at the transcript's top.
pub const SYSTEM_PROMPT_KEY: &str = "system-prompt";
pub const MODEL_TOOLS_KEY: &str = "model-tools";

/// The rows pinned at the top of the transcript, as the TUI pins them:
/// the system prompt the latest run used (else the one the session was
/// created with), and the tools the latest model request offered. Both
/// start folded, side by side on one compact line; an opened one takes a
/// line of its own with its body under it. They are one item, keyed by
/// its HTML, which changes with what it shows.
pub fn pinned_rows(
    state: &SessionState,
    expanded: &Expanded,
    draft_agent: Option<&cookie_agent_protocol::AgentId>,
) -> Option<(u64, String)> {
    let (snapshot, from_run) = system_prompt_source(state)?;
    let mut rows = Vec::new();
    if cookie_agent_client_state::rows::shows_system_prompt(state) {
        rows.push(pinned_row(
        SYSTEM_PROMPT_KEY,
        icondata::FaScrollSolid,
        &system_prompt_summary(snapshot, from_run, draft_agent),
        expanded,
        || {
            let body = bounded_text(
                &snapshot.composed_prompt,
                MAX_SYSTEM_PROMPT_BODY_LINES,
                MAX_SYSTEM_PROMPT_BODY_BYTES,
            );
            let mut html = format!(
                "<pre class=\"max-h-[60vh] overflow-auto whitespace-pre-wrap break-words rounded-md bg-parchment p-2 font-mono text-xs dark:bg-char\" data-system-prompt>{}</pre>",
                escape(&body.lines.join("\n"))
            );
            if body.omitted_lines > 0 {
                html.push_str(&format!(
                    "<p class=\"{NOTE}\">… truncated ({} more lines)</p>",
                    body.omitted_lines
                ));
            }
            html
        },
    ));
    }
    if let Some(tools) = state.model_tools.as_deref() {
        rows.push(pinned_row(
            MODEL_TOOLS_KEY,
            icondata::FaToolboxSolid,
            &model_tools_summary(state, tools),
            expanded,
            || {
                // Searchable tools take their group's hue, so one group's
                // tools read alike; the others the plain chip.
                let list = |tools: &[&str], group: &str, hues: Option<&[usize]>| {
                    let (shown, omitted) = bounded_tools(tools);
                    let mut html = format!(
                        "<ul class=\"flex flex-wrap gap-1\" data-model-tools{group}>"
                    );
                    for (index, tool) in shown.iter().enumerate() {
                        let slot = hues.and_then(|hues| hues.get(index)).copied();
                        let colour = slot.map_or(
                            "border-toasted dark:border-ember",
                            |slot| {
                                crate::badges::hue_class(Some(
                                    u8::try_from(slot % 8).unwrap_or_default(),
                                ))
                            },
                        );
                        let title = state
                            .deferred_tool_groups
                            .get(*tool)
                            .filter(|_| slot.is_some())
                            .map(|group| format!(" title=\"{}\" data-tool-group=\"{}\"", escape(group), escape(group)))
                            .unwrap_or_default();
                        html.push_str(&format!(
                            "<li class=\"rounded border {colour} px-1.5 font-mono text-xs leading-5\"{title}>{}</li>",
                            escape(tool)
                        ));
                    }
                    html.push_str("</ul>");
                    if omitted > 0 {
                        html.push_str(&format!("<p class=\"{NOTE}\">… and {omitted} more</p>"));
                    }
                    html
                };
                let (provided, searchable) = model_tool_groups(state, tools);
                if searchable.is_empty() {
                    list(&provided, "", None)
                } else {
                    // A request that deferred tools lists them apart, the
                    // searchable ones after a link to the prompt that
                    // announced them.
                    let link = if cookie_agent_client_state::rows::deferred_tools_prompt(state)
                        .is_some()
                    {
                        " <button type=\"button\" data-action=\"deferred-prompt\" class=\"rounded px-1 underline decoration-dotted underline-offset-2 hover:bg-crust active:bg-parchment dark:hover:bg-smoke dark:active:bg-char\" data-deferred-prompt-link>[see full prompt]</button>"
                    } else {
                        ""
                    };
                    let slots = cookie_agent_client_state::rows::tool_group_slots(state, &searchable);
                    format!(
                        "<p class=\"{NOTE}\">Provided</p>{}<p class=\"{NOTE}\">Searchable{link}</p>{}",
                        list(&provided, "=\"provided\"", None),
                        list(&searchable, "=\"searchable\"", Some(&slots))
                    )
                }
            },
        ));
    }
    if rows.is_empty() {
        return None;
    }
    let html = format!(
        "<div class=\"flex flex-wrap items-start gap-x-3 gap-y-1\" data-pinned-rows>{}</div>",
        rows.concat()
    );
    Some((foldhash::fast::FixedState::default().hash_one(&html), html))
}

/// A pinned row: its folded title, a compact line that a finger still
/// reaches at 44 px (`hit-area`), and once opened, `body`'s HTML.
fn pinned_row(
    key: &str,
    icon: Icon,
    header: &str,
    expanded: &Expanded,
    body: impl FnOnce() -> String,
) -> String {
    let open = expanded.block(key);
    let chevron = if open {
        icondata::FaChevronDownSolid
    } else {
        icondata::FaChevronRightSolid
    };
    let width = if open { "w-full" } else { "min-w-0 max-w-full" };
    let mut html = format!(
        "<div class=\"collapsible-row {width}\" data-pinned=\"{key}\"><button type=\"button\" data-action=\"toggle-block\" data-block-key=\"{key}\" aria-expanded=\"{open}\" class=\"hit-area flex h-7 max-w-full items-center gap-1.5 rounded-md px-2 text-left {NOTE} hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke\">{}{}<span class=\"min-w-0 truncate\">{}</span></button>",
        svg(chevron, "text-[0.625rem] opacity-70"),
        svg(icon, ""),
        escape(header)
    );
    if open {
        html.push_str(
            "<div class=\"mt-1 flex flex-col gap-1 border-l border-toasted pl-3 dark:border-ember\">",
        );
        html.push_str(&body());
        html.push_str("</div>");
    }
    html.push_str("</div>");
    html
}

/// The expanded-set key of the thinking segment that starts at `id`.
fn thinking_key(id: u64) -> String {
    format!("thinking:{id}")
}

/// A row that shows `header` and, once clicked open (`expanded`), `detail`
/// and `body`; `key` is its block key.
fn collapsible(
    expanded: bool,
    key: &str,
    icon: Icon,
    header: &str,
    detail: Option<&str>,
    body: &str,
) -> String {
    let chevron = if expanded {
        icondata::FaChevronDownSolid
    } else {
        icondata::FaChevronRightSolid
    };
    let mut html = format!(
        "<div class=\"collapsible-row\" data-block-key=\"{}\"><button type=\"button\" data-action=\"toggle-block\" data-block-key=\"{}\" aria-expanded=\"{expanded}\" class=\"flex min-h-11 w-full items-center gap-2 rounded-md px-2 text-left {NOTE} hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke\">{}{}<span class=\"min-w-0 flex-1 truncate\">{}</span></button>",
        escape(key),
        escape(key),
        svg(chevron, "text-[0.625rem] opacity-70"),
        svg(icon, ""),
        escape(header)
    );
    if expanded {
        html.push_str("<div class=\"border-l border-toasted pl-3 dark:border-ember\">");
        if let Some(detail) = detail {
            html.push_str(&format!(
                "<div class=\"{NOTE} whitespace-pre-wrap\">{}</div>",
                escape(detail)
            ));
        }
        html.push_str(&format!(
            "<div class=\"{NOTE} whitespace-pre-wrap break-words\">{}</div>",
            escape(body)
        ));
        html.push_str("</div>");
    }
    html.push_str("</div>");
    html
}

fn child_html(view: &TranscriptView<'_>, item_id: u64, child: &AssistantChild) -> String {
    match child {
        AssistantChild::Text { markdown, .. } => {
            format!("<div class=\"markdown\">{}</div>", document_html(markdown))
        }
        // Open or folded like any other block, in the page's expanded set:
        // a native <details> forgot it whenever the item drew again.
        AssistantChild::Thinking { id, text, .. } => collapsible(
            view.expanded.thinking(*id),
            &thinking_key(*id),
            icondata::FaLightbulbRegular,
            &thinking_status(view.state, item_id, *id),
            None,
            text,
        ),
        AssistantChild::Tool { call_id } => match view.state.tools.get(call_id) {
            Some(tool) => tool_html(view, tool),
            None => String::new(),
        },
        AssistantChild::CommittedTool { name, .. } => format!(
            "<div class=\"flex items-center gap-2 {NOTE} font-mono\">{}{} (not started)</div>",
            svg(icondata::FaHammerSolid, ""),
            escape(name.as_str())
        ),
        AssistantChild::MediaFile { file, .. } => match &file.source {
            PersistedFileSource::Artifact {
                byte_length,
                sha256,
                ..
            } => media_html(
                sha256.as_str(),
                file.media_type.as_str(),
                *byte_length,
                file.filename.as_deref(),
            ),
            PersistedFileSource::Url { url } if super::safe_link(url) => format!(
                "<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-caramel underline hover:text-espresso dark:text-maple dark:hover:text-latte\">{}</a>",
                escape(url),
                escape(file.filename.as_deref().unwrap_or(url))
            ),
            _ => format!(
                "<div class=\"{NOTE}\">{}</div>",
                escape(&format!("media: {}", file.media_type))
            ),
        },
        AssistantChild::Attribution { resolved_model } => format!(
            "<div class=\"{NOTE} font-mono\">→ {}</div>",
            escape(&view.names.selection_label(&resolved_model.selection))
        ),
        AssistantChild::Notice { text } => {
            format!("<div class=\"{NOTE} italic\">{}</div>", escape(text))
        }
    }
}

/// The expansion key that shows a call's output past the row budget.
pub fn all_output_key(call: &str) -> String {
    format!("{call}:all")
}

/// A tool row's header text, with the paths of the runtime it runs on
/// shown under that runtime's home.
fn tool_header_text(view: &TranscriptView<'_>, tool: &ToolCallState) -> String {
    let home = match tool
        .presentation
        .tool
        .source
        .as_ref()
        .and_then(|source| source.known())
    {
        Some(
            cookie_agent_protocol::ToolSource::Runtime { runtime_id, .. }
            | cookie_agent_protocol::ToolSource::Acp { runtime_id, .. },
        ) => view.homes.get(runtime_id),
        _ => None,
    };
    crate::places::paths_under_home(&tool.header_text(), home)
}

/// A tool call: one header line, collapsed until clicked, as in the TUI.
/// The header has the hint's icon, the title and the primary argument it
/// does not name, the latest progress while it runs, and the status; the
/// open row adds the arguments and the output, within the row budget.
fn tool_html(view: &TranscriptView<'_>, tool: &ToolCallState) -> String {
    let id = tool.id.to_string();
    let mark = icons::tool_status(&tool.status, tool.backgrounded);
    // A finished or running call says so with its icon alone; one that
    // stopped short says why in words too.
    let label_class = match tool.status {
        ToolStatus::Completed | ToolStatus::Running if !tool.backgrounded => "sr-only",
        _ => "",
    };
    let status = format!(
        "<span class=\"inline-flex shrink-0 items-center gap-1 text-xs {}\" title=\"{}\">{}<span class=\"{label_class}\">{}</span></span>",
        mark.tone,
        mark.label,
        svg(mark.icon, if mark.spins { "animate-spin" } else { "" }),
        mark.label
    );
    let expanded = view.expanded.tool(&tool.id);
    let progress = tool
        .shown_progress()
        .map(|progress| {
            format!(
                "<span class=\"min-w-0 shrink truncate {NOTE}\" data-progress>· {}</span>",
                escape(&cookie_agent_client_state::tool_header::flatten_header_argument(progress))
            )
        })
        .unwrap_or_default();
    // A call waiting for an approval says so on its header, and opens the
    // approvals from there.
    let approval = if view.waiting_approval(tool) {
        format!(
            "<button type=\"button\" data-action=\"approvals\" class=\"inline-flex min-h-11 shrink-0 items-center gap-1 rounded-full bg-caramel px-3 text-xs font-bold text-cream hover:bg-espresso active:bg-ink dark:bg-maple dark:text-night dark:hover:bg-latte dark:active:bg-sand\">{}approve?</button>",
            svg(icondata::FaBellSolid, "")
        )
    } else {
        String::new()
    };
    // A running call the daemon can move to the background offers it, as
    // the TUI's title-row button does.
    let background = if tool.status == ToolStatus::Running
        && tool.backgroundable
        && !tool.backgrounded
    {
        format!(
            "<button type=\"button\" data-action=\"background\" data-call=\"{}\" title=\"Move to the background\" class=\"inline-flex min-h-11 shrink-0 items-center gap-1 rounded-md px-2 text-xs hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke\">{}background</button>",
            escape(&id),
            svg(icondata::FaLayerGroupSolid, "")
        )
    } else {
        String::new()
    };
    let chevron = svg(
        if expanded {
            icondata::FaChevronDownSolid
        } else {
            icondata::FaChevronRightSolid
        },
        "text-[0.625rem] opacity-60",
    );
    let icon = svg(
        icons::hint_icon(tool.presentation.hint.as_ref().map(|hint| hint.rendered())),
        "text-caramel dark:text-maple",
    );
    let header = tool_header_text(view, tool);
    // An open call's header wraps to show its whole title and argument, its
    // chevron, icon, and status on the first line; a folded one is one line,
    // cut to fit.
    let (align, fit) = if expanded {
        ("items-start", "whitespace-normal wrap-anywhere")
    } else {
        ("items-center", "truncate")
    };
    // A memory the call names is a link to the Memory view inside the
    // header, a click target of its own between the two halves that toggle.
    let link = tool
        .memory_url()
        .and_then(|url| header.find(url).map(|at| (at, url)));
    let toggle = match link {
        None => format!(
            "<button type=\"button\" data-action=\"toggle-tool\" data-call=\"{}\" aria-expanded=\"{expanded}\" class=\"flex min-h-11 min-w-0 flex-1 {align} gap-2 rounded-lg px-2 py-1 text-left font-mono text-sm hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke\">{chevron}{icon}<span class=\"min-w-0 shrink {fit}\" data-header>{}</span>{progress}<span class=\"flex-1\"></span>{status}</button>",
            escape(&id),
            escape(&header),
        ),
        Some((at, url)) => {
            let suffix = header[at + url.len()..].trim();
            let suffix = if suffix.is_empty() {
                String::new()
            } else {
                format!(
                    "<span class=\"min-w-0 shrink {fit}\">{}</span>",
                    escape(suffix)
                )
            };
            format!(
                "<div class=\"flex min-w-0 flex-1 {align} rounded-lg hover:bg-toasted dark:hover:bg-ember\" data-header-row><button type=\"button\" data-action=\"toggle-tool\" data-call=\"{id}\" aria-expanded=\"{expanded}\" class=\"flex min-h-11 min-w-0 shrink {align} gap-2 rounded-l-lg py-1 pl-2 text-left font-mono text-sm active:bg-crust dark:active:bg-smoke\">{chevron}{icon}<span class=\"min-w-0 shrink {fit}\" data-header>{}</span></button><button type=\"button\" data-action=\"open-memory\" data-memory-url=\"{url}\" title=\"Open it in Memory\" class=\"ml-2 min-h-11 min-w-0 shrink {fit} rounded-md px-1 text-left font-mono text-sm underline decoration-dotted underline-offset-2 hover:bg-crust active:bg-parchment dark:hover:bg-smoke dark:active:bg-char\" data-memory-link>{url}</button><button type=\"button\" data-action=\"toggle-tool\" data-call=\"{id}\" aria-expanded=\"{expanded}\" tabindex=\"-1\" class=\"flex min-h-11 min-w-0 flex-1 {align} gap-2 rounded-r-lg py-1 pr-2 pl-2 text-left font-mono text-sm active:bg-crust dark:active:bg-smoke\">{suffix}{progress}<span class=\"flex-1\"></span>{status}</button></div>",
                escape(header[..at].trim_end()),
                id = escape(&id),
                url = escape(url),
            )
        }
    };
    let mut html = format!(
        "<div class=\"rounded-lg border border-toasted dark:border-ember\" data-tool=\"{}\" data-expanded=\"{expanded}\"><div class=\"flex {align}\">{toggle}{approval}{background}</div>",
        escape(&id),
    );
    if let Some(delegate) = view.delegates.get(&tool.id) {
        html.push_str(&delegate_html(delegate, tool.status == ToolStatus::Running));
    }
    if expanded {
        html.push_str("<div class=\"flex flex-col gap-1 border-t border-toasted px-2 pt-1 pb-2 dark:border-ember\" data-tool-body>");
        let arguments = tool.shown_arguments();
        if arguments.is_empty() {
            // As in the TUI: a presentation with argument blocks or a
            // preview is complete as given, and raw arguments are
            // diagnostic detail at `info` and below.
            if !tool.arguments.is_empty()
                && tool.presentation.arguments.is_empty()
                && tool.presentation.preview.is_empty()
                && view.minimum_level <= EventLevel::Info
            {
                html.push_str(&format!(
                    "<div class=\"{NOTE}\">arguments</div><pre class=\"overflow-x-auto whitespace-pre-wrap font-mono text-xs\">{}</pre>",
                    escape(&tool.arguments)
                ));
            }
        } else {
            html.push_str(&format!(
                "<div class=\"{NOTE}\" data-arguments>arguments</div>{}",
                display_html(&arguments, BlockPlace::plain)
            ));
        }
        if let Some(failure) = &tool.failure {
            html.push_str(&format!(
                "<div class=\"text-sm text-red-700 dark:text-red-400\">{}</div>",
                escape(failure)
            ));
        }
        let all_key = all_output_key(&id);
        let all = view.expanded.all_output(&tool.id);
        let budget = if all { ALL_OUTPUT_LINES } else { OUTPUT_LINES };
        let (shown, omitted) = within_budget(tool.shown_display(), budget);
        html.push_str(&display_html(&shown, |index| BlockPlace {
            call_id: Some(&id),
            index,
            full: tool.full_content(index),
        }));
        let notice = match (omitted, all) {
            (0, false) => None,
            (0, true) => Some("click to collapse".to_owned()),
            (omitted, false) => Some(format!("… {omitted} more lines (click to expand)")),
            (omitted, true) => Some(format!(
                "… {omitted} more lines (maximum shown; click to collapse)"
            )),
        };
        if let Some(notice) = notice {
            html.push_str(&format!(
                "<button type=\"button\" data-action=\"toggle-block\" data-block-key=\"{}\" data-more-lines class=\"min-h-11 rounded-md px-2 text-left text-xs text-ink hover:bg-toasted active:bg-crust dark:text-sand dark:hover:bg-ember dark:active:bg-smoke\">{}</button>",
                escape(&all_key),
                escape(&notice)
            ));
        }
        html.push_str("</div>");
    }
    html.push_str("</div>");
    html
}

/// A delegate call's subagent: its agent and handle, what it does while it
/// runs, and the way to open it.
fn delegate_html(delegate: &Delegate, running: bool) -> String {
    let activity = delegate
        .activity
        .filter(|_| running)
        .map(|activity| {
            format!(
                "<span class=\"inline-flex items-center gap-1 text-caramel dark:text-maple\" data-delegate-activity>{}{}</span>",
                svg(icondata::FaCircleNotchSolid, "animate-spin"),
                activity.label()
            )
        })
        .unwrap_or_default();
    format!(
        "<div class=\"flex flex-wrap items-center gap-2 px-2 pb-1 {NOTE}\" data-delegate=\"{}\"><span class=\"font-mono\">{}{}</span>{activity}<button type=\"button\" data-action=\"open-session\" data-session=\"{}\" class=\"inline-flex min-h-11 items-center gap-1 rounded-md px-2 hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke\">{}open</button></div>",
        delegate.session_id,
        escape(&delegate.agent),
        delegate
            .handle
            .as_ref()
            .map(|handle| format!(" {}", escape(handle)))
            .unwrap_or_default(),
        delegate.session_id,
        svg(icondata::FaArrowUpRightFromSquareSolid, ""),
    )
}

#[cfg(test)]
mod tests {
    use cookie_agent_client_state::transcript_mode::TranscriptMode;
    use cookie_agent_client_state::{
        EventLevel, ModelDisplayNames, ProducerMessageStatus, SessionState, TranscriptItem,
    };

    use super::{Delegate, Expanded, TranscriptView, child_html, item_html, item_key, tool_html};

    /// The fold that starts the test replies' first block.
    const FIRST_FOLD: cookie_agent_client_state::transcript_mode::WorkKey =
        cookie_agent_client_state::transcript_mode::WorkKey { item: 1, child: 0 };

    static NO_HOMES: std::sync::LazyLock<cookie_agent_client_state::paths::RuntimeHomes> =
        std::sync::LazyLock::new(Default::default);

    static NO_DELEGATES: std::sync::LazyLock<
        std::collections::HashMap<cookie_agent_protocol::ToolCallId, Delegate>,
    > = std::sync::LazyLock::new(Default::default);

    /// A completed call with raw `arguments`, presented with `presentation`.
    fn tool(
        presentation: cookie_agent_protocol::PreparedPresentation,
        arguments: &str,
    ) -> cookie_agent_client_state::ToolCallState {
        let name = presentation.title.to_string();
        let presentation = cookie_agent_protocol::ToolPresentation::new(
            presentation,
            cookie_agent_protocol::ToolIdentity {
                name: name.clone(),
                published_name: name,
                source: None,
            },
        );
        let owner = cookie_agent_protocol::AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: cookie_agent_protocol::ModelCallId::new("call-1").expect("call ID"),
            provider_item_id: None,
        };
        let mut tool = cookie_agent_client_state::ToolCallState::started(
            cookie_agent_protocol::ToolCallId::new_v7(),
            owner,
            presentation,
            arguments.into(),
        );
        tool.status = cookie_agent_client_state::ToolStatus::Completed;
        tool
    }

    /// A `Run` call of `command`, with `command` as its primary argument and
    /// a key/value argument block that repeats it beside one that does not.
    fn run(title: &str, command: &str) -> cookie_agent_client_state::ToolCallState {
        let presentation = cookie_agent_protocol::PreparedPresentation::new(
            cookie_agent_protocol::SafeDisplayText::new(title).expect("title"),
            Some(cookie_agent_protocol::BoundedDisplayText::new(command).expect("argument")),
        )
        .with_hint("shell")
        .with_arguments([cookie_agent_protocol::DisplayBlock::KeyValue {
            entries: vec![
                cookie_agent_protocol::KeyValueEntry {
                    key: "command".into(),
                    value: command.into(),
                },
                cookie_agent_protocol::KeyValueEntry {
                    key: "timeout".into(),
                    value: "30s".into(),
                },
            ],
            omitted_entries: None,
        }]);
        tool(presentation, "{}")
    }

    fn rendered(tool: &cookie_agent_client_state::ToolCallState, expanded: &Expanded) -> String {
        let state = SessionState::default();
        let names = ModelDisplayNames::default();
        let view = TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state: &state,
            names: &names,
            minimum_level: EventLevel::Warning,
            expanded,
            delegates: &NO_DELEGATES,
            homes: &NO_HOMES,
            transcript: TranscriptMode::Full,
            now: jiff::Timestamp::UNIX_EPOCH,
        };
        tool_html(&view, tool)
    }

    /// The text of the header's title span.
    fn header(html: &str) -> &str {
        let start = html.find("data-header>").expect("a header") + "data-header>".len();
        &html[start..start + html[start..].find('<').expect("its end")]
    }

    #[test]
    fn thinking_stays_open_from_the_expanded_set_whatever_draws_again() {
        let state = SessionState::default();
        let names = ModelDisplayNames::default();
        let thinking = cookie_agent_client_state::AssistantChild::Thinking {
            id: 7,
            version: 0,
            text: "weighing the options".into(),
        };
        let draw = |expanded: &Expanded| {
            let view = TranscriptView {
                session_id: cookie_agent_protocol::SessionId::new_v7(),
                state: &state,
                names: &names,
                minimum_level: EventLevel::Warning,
                expanded,
                delegates: &NO_DELEGATES,
                homes: &NO_HOMES,
                transcript: TranscriptMode::Full,
                now: jiff::Timestamp::UNIX_EPOCH,
            };
            child_html(&view, 1, &thinking)
        };
        let folded = draw(&Expanded::default());
        assert!(folded.contains("data-block-key=\"thinking:7\""), "{folded}");
        assert!(folded.contains("aria-expanded=\"false\""), "{folded}");
        assert!(!folded.contains("weighing the options"), "{folded}");
        let open = draw(&Expanded::from_iter(["thinking:7".to_owned()]));
        assert!(open.contains("aria-expanded=\"true\""), "{open}");
        assert!(open.contains("weighing the options"), "{open}");
    }

    #[test]
    fn an_open_header_holds_the_whole_title_and_argument() {
        let command = format!("printf {}", "a-long-argument ".repeat(20));
        let call = run("Run", &command);
        let folded = rendered(&call, &Expanded::default());
        let open = rendered(&call, &Expanded::from_iter([call.id.to_string()]));
        // The header holds the whole argument folded or open.
        assert_eq!(header(&open), header(&folded));
        assert!(
            header(&open).ends_with(command.trim_end()),
            "{}",
            header(&open)
        );
    }

    #[test]
    fn the_collapsed_header_shows_the_title_and_the_argument_it_does_not_name() {
        let named = run("Run `cargo test`", "cargo test");
        let html = rendered(&named, &Expanded::default());
        assert_eq!(header(&html), "Run `cargo test`");
        let unnamed = run("Run a command", "cargo test");
        assert_eq!(
            header(&rendered(&unnamed, &Expanded::default())),
            "Run a command cargo test"
        );
        // Collapsed: no arguments and no output.
        assert!(html.contains("aria-expanded=\"false\""), "{html}");
        assert!(!html.contains("data-tool-body"), "{html}");
        assert!(!html.contains("timeout"), "{html}");
    }

    /// A memory tool call is one row whose header links to the memory, a
    /// target of its own beside the halves that toggle, and whose open
    /// body shows the change as a diff.
    #[test]
    fn a_memory_call_links_its_url_and_opens_to_its_diff() {
        let mut call = tool(
            cookie_agent_protocol::PreparedPresentation::new(
                cookie_agent_protocol::SafeDisplayText::new("Edit memory agent://builder")
                    .expect("title"),
                Some(
                    cookie_agent_protocol::BoundedDisplayText::new("agent://builder")
                        .expect("argument"),
                ),
            )
            .with_hint("file_edit"),
            "{}",
        );
        call.display = vec![cookie_agent_protocol::Open::Known(
            cookie_agent_protocol::DisplayBlock::Diff(cookie_agent_protocol::DiffBlock {
                path: "agent://builder".into(),
                old_path: None,
                language: Some("markdown".into()),
                change: cookie_agent_protocol::DiffChange::Modified,
                hunks: vec![cookie_agent_protocol::DiffHunk {
                    old_start: 4,
                    old_lines: 2,
                    new_start: 4,
                    new_lines: 2,
                    lines: vec![
                        cookie_agent_protocol::DiffLine {
                            op: cookie_agent_protocol::DiffLineOp::Context,
                            text: "---".into(),
                        },
                        cookie_agent_protocol::DiffLine {
                            op: cookie_agent_protocol::DiffLineOp::Remove,
                            text: "- old lesson".into(),
                        },
                        cookie_agent_protocol::DiffLine {
                            op: cookie_agent_protocol::DiffLineOp::Add,
                            text: "- new lesson".into(),
                        },
                    ],
                }],
                added: 1,
                removed: 1,
                omitted: None,
                as_of: cookie_agent_protocol::DiffAsOf::Execute,
                full: None,
            }),
        )];
        let folded = rendered(&call, &Expanded::default());
        assert_eq!(header(&folded), "Edit memory");
        let link = "<button type=\"button\" data-action=\"open-memory\" data-memory-url=\"agent://builder\"";
        assert_eq!(folded.matches(link).count(), 1, "{folded}");
        // The link is no part of a toggle: it sits between the two.
        let at = folded.find(link).expect("the link");
        let before = &folded[..at];
        assert!(before.ends_with("</button>"), "{folded}");
        assert_eq!(folded.matches("data-action=\"toggle-tool\"").count(), 2);
        assert!(!folded.contains("data-tool-body"), "{folded}");
        let open = rendered(&call, &Expanded::from_iter([call.id.to_string()]));
        assert!(open.contains("data-tool-body"), "{open}");
        assert!(open.contains("data-kind=\"diff\""), "{open}");
        assert!(open.contains("agent://builder · modified"), "{open}");
        assert!(open.contains("- new lesson"), "{open}");
        // A call whose argument is no memory URL keeps one toggle.
        let plain = rendered(&run("Run a command", "user:// x"), &Expanded::default());
        assert!(!plain.contains("open-memory"), "{plain}");
        assert_eq!(plain.matches("data-action=\"toggle-tool\"").count(), 1);
    }

    #[test]
    fn the_open_row_shows_arguments_without_the_one_the_header_shows() {
        let call = run("Run a command", "cargo test");
        let html = rendered(&call, &Expanded::from_iter([call.id.to_string()]));
        assert!(html.contains("aria-expanded=\"true\""), "{html}");
        assert!(html.contains("data-arguments>arguments<"), "{html}");
        assert!(html.contains("timeout"), "{html}");
        assert!(!html.contains(">command</dt>"), "{html}");
        assert_eq!(call.shown_arguments().len(), 1);
    }

    #[test]
    fn a_running_call_shows_its_progress_while_collapsed() {
        let mut call = run("Run a command", "sleep 8");
        call.status = cookie_agent_client_state::ToolStatus::Running;
        call.progress = Some("compiling\n12 of 40".into());
        let html = rendered(&call, &Expanded::default());
        assert!(
            html.contains("data-progress>· compiling 12 of 40<"),
            "{html}"
        );
    }

    #[test]
    fn long_output_stops_at_the_row_budget_until_expanded() {
        let mut call = run("Run a command", "seq 100");
        let lines = (1..=100)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        call.display = vec![cookie_agent_protocol::Open::Known(
            cookie_agent_protocol::DisplayBlock::text(lines),
        )];
        let id = call.id.to_string();
        let open = rendered(&call, &Expanded::from_iter([id.clone()]));
        assert!(open.contains("… 36 more lines (click to expand)"), "{open}");
        assert!(!open.contains("\n65"), "{open}");
        let all = rendered(
            &call,
            &Expanded::from_iter([id.clone(), super::all_output_key(&id)]),
        );
        assert!(all.contains("\n100"), "{all}");
        assert!(all.contains("click to collapse"), "{all}");
    }

    #[test]
    fn raw_arguments_show_only_without_blocks_or_a_preview_at_info() {
        let state = SessionState::default();
        let names = ModelDisplayNames::default();
        let plain = tool(
            cookie_agent_protocol::PreparedPresentation::new(
                cookie_agent_protocol::SafeDisplayText::new("Lookup").expect("title"),
                None,
            ),
            r#"{"query":"raw-marker"}"#,
        );
        let previewed = tool(
            cookie_agent_protocol::PreparedPresentation::new(
                cookie_agent_protocol::SafeDisplayText::new("Edit a.rs").expect("title"),
                None,
            )
            .with_preview([cookie_agent_protocol::DisplayBlock::text("the preview")]),
            r#"{"filePath":"a.rs","newString":"raw-marker"}"#,
        );
        let expanded = Expanded::from_iter([plain.id.to_string(), previewed.id.to_string()]);
        let view = |minimum_level| TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state: &state,
            names: &names,
            minimum_level,
            expanded: &expanded,
            delegates: &NO_DELEGATES,
            homes: &NO_HOMES,
            transcript: TranscriptMode::Full,
            now: jiff::Timestamp::UNIX_EPOCH,
        };
        assert!(tool_html(&view(EventLevel::Info), &plain).contains("raw-marker"));
        assert!(!tool_html(&view(EventLevel::Warning), &plain).contains("raw-marker"));
        let previewed_html = tool_html(&view(EventLevel::Debug), &previewed);
        assert!(!previewed_html.contains("raw-marker"), "{previewed_html}");
    }

    #[test]
    fn rows_below_the_event_level_are_hidden_and_text_is_escaped() {
        let state = SessionState::default();
        let names = ModelDisplayNames::default();
        let expanded = Expanded::default();
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let view = |minimum_level| TranscriptView {
            session_id,
            state: &state,
            names: &names,
            minimum_level,
            expanded: &expanded,
            delegates: &NO_DELEGATES,
            homes: &NO_HOMES,
            transcript: TranscriptMode::Full,
            now: jiff::Timestamp::UNIX_EPOCH,
        };
        let row = TranscriptItem::internal("<careful>");
        let shown = item_html(&view(EventLevel::Info), &row).expect("shown");
        assert!(shown.contains("[I] &lt;careful&gt;"), "{shown}");
        assert_eq!(item_html(&view(EventLevel::Warning), &row), None);
        assert_ne!(
            item_key(&view(EventLevel::Info), &row),
            item_key(&view(EventLevel::Warning), &row)
        );
        let user =
            item_html(&view(EventLevel::Info), &TranscriptItem::user("hi <b>")).expect("user");
        assert!(user.contains("hi &lt;b&gt;"), "{user}");
        // Item IDs repeat across sessions: the same item of another session
        // keys apart, so switching sessions never shows a stale row.
        let other = TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            ..view(EventLevel::Info)
        };
        assert_ne!(
            item_key(&view(EventLevel::Info), &row),
            item_key(&other, &row)
        );
    }

    #[test]
    fn producer_messages_fold_under_their_summary_and_open_on_a_click() {
        let state = SessionState::default();
        let names = ModelDisplayNames::default();
        let message_id = cookie_agent_protocol::ProducerMessageId::new_v7();
        let row = |status| TranscriptItem::ProducerMessage {
            id: 1,
            seq: 4,
            accepted_at: jiff::Timestamp::UNIX_EPOCH,
            message_id,
            producer_owner: cookie_agent_protocol::ProducerOwner::GoalControl {
                goal_id: cookie_agent_protocol::GoalId::new_v7(),
            },
            mode: cookie_agent_protocol::ProducerDeliveryMode::Steer,
            body: "[engine goal control]\nThe goal <x> was paused.".into(),
            summary: None,
            reminder: None,
            status,
        };
        let collapsed = Expanded::default();
        let opened = Expanded::from_iter([format!("producer:{message_id}")]);
        let view = |expanded| TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state: &state,
            names: &names,
            minimum_level: EventLevel::Warning,
            expanded,
            delegates: &NO_DELEGATES,
            homes: &NO_HOMES,
            transcript: TranscriptMode::Full,
            now: jiff::Timestamp::UNIX_EPOCH,
        };
        let consumed = row(ProducerMessageStatus::Consumed);
        let folded = item_html(&view(&collapsed), &consumed).expect("shown");
        assert!(folded.contains("goal control · steer"), "{folded}");
        assert!(!folded.contains("engine goal control"), "{folded}");
        let open = item_html(&view(&opened), &consumed).expect("shown");
        assert!(open.contains("The goal &lt;x&gt; was paused."), "{open}");
        assert_eq!(
            item_html(&view(&collapsed), &row(ProducerMessageStatus::Pending)),
            None
        );
        assert_eq!(
            item_html(&view(&collapsed), &row(ProducerMessageStatus::Discarded)),
            None
        );
    }

    #[test]
    fn model_input_folds_to_its_header_and_opens_to_the_text_the_model_reads() {
        let state = SessionState::default();
        let names = ModelDisplayNames::default();
        let row = TranscriptItem::ModelInput {
            id: 1,
            version: 0,
            seq: 7,
            kind: cookie_agent_client_state::model_input::ModelInputKind::Memory,
            header: "memory · 2 lines".into(),
            text: "<system-reminder>\n# Memory".into(),
        };
        let collapsed = Expanded::default();
        let opened = Expanded::from_iter(["model-input:7".to_owned()]);
        let view = |expanded| TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state: &state,
            names: &names,
            minimum_level: EventLevel::Error,
            expanded,
            delegates: &NO_DELEGATES,
            homes: &NO_HOMES,
            transcript: TranscriptMode::Full,
            now: jiff::Timestamp::UNIX_EPOCH,
        };
        let folded = item_html(&view(&collapsed), &row).expect("shown at any level");
        assert!(folded.contains("data-model-input=\"memory\""), "{folded}");
        assert!(folded.contains("memory · 2 lines"), "{folded}");
        assert!(!folded.contains("# Memory"), "{folded}");
        let open = item_html(&view(&opened), &row).expect("shown");
        assert!(open.contains("&lt;system-reminder&gt;\n# Memory"), "{open}");
    }

    #[test]
    fn a_memory_change_is_a_line_that_opens_the_memory_whatever_the_level() {
        let state = SessionState::default();
        let names = ModelDisplayNames::default();
        let expanded = Expanded::default();
        let view = TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state: &state,
            names: &names,
            minimum_level: EventLevel::Error,
            expanded: &expanded,
            delegates: &NO_DELEGATES,
            homes: &NO_HOMES,
            transcript: TranscriptMode::Full,
            now: jiff::Timestamp::UNIX_EPOCH,
        };
        let html = item_html(
            &view,
            &TranscriptItem::MemoryChange {
                id: 1,
                seq: 7,
                change: cookie_agent_protocol::MemoryChangeKind::Saved,
                url: "project://web/<build>".into(),
            },
        )
        .expect("shown at any level");
        assert!(
            html.contains(
                "data-action=\"open-memory\" data-memory-url=\"project://web/&lt;build&gt;\""
            ),
            "{html}"
        );
        assert!(html.contains("<span>Saved</span>"), "{html}");
        assert!(
            html.contains(">project://web/&lt;build&gt;</span>"),
            "{html}"
        );
    }

    #[test]
    fn the_system_prompt_and_tools_rows_fold_and_open() {
        use super::{MODEL_TOOLS_KEY, SYSTEM_PROMPT_KEY, pinned_rows};
        let mut state = SessionState::default();
        let mut expanded = Expanded::default();
        // Nothing to pin before the session's agent is known.
        assert!(pinned_rows(&state, &expanded, None).is_none());
        let primary = cookie_agent_protocol::AgentId::new("primary").unwrap();
        state.run_snapshot = Some(Box::new(cookie_agent_protocol::AgentSnapshot {
            agent: primary.clone(),
            mode: cookie_agent_protocol::AgentMode::Primary,
            description: "Test primary agent".into(),
            document_source: cookie_agent_protocol::AgentDocumentSource::User,
            document_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"document"),
            composed_prompt: "You help.\n  Indented <b>line</b>".into(),
            prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
            max_output_tokens: 0,
            permissions: Vec::new(),
            delegation: None,
            fallback_chain: Vec::new(),
            selected_suffix_start: 0,
        }));
        state.model_tools = Some(std::sync::Arc::from(vec![
            "read".to_owned(),
            "bash".to_owned(),
        ]));
        let fast = cookie_agent_protocol::AgentId::new("fast").unwrap();
        let (folded_key, folded) = pinned_rows(&state, &expanded, Some(&fast)).unwrap();
        assert!(folded.contains("system prompt · primary (last run) · next: fast (2 lines)"));
        assert!(folded.contains("tools · 2 available (last request)"));
        assert!(!folded.contains("data-system-prompt") && !folded.contains("data-model-tools"));
        // Folded, both share one line.
        assert_eq!(folded.matches("min-w-0 max-w-full").count(), 2);
        let (_, same) = pinned_rows(&state, &expanded, Some(&primary)).unwrap();
        assert!(!same.contains("next:"));
        expanded.toggle(SYSTEM_PROMPT_KEY);
        expanded.toggle(MODEL_TOOLS_KEY);
        let (open_key, open) = pinned_rows(&state, &expanded, None).unwrap();
        // The prompt keeps its whitespace, escaped; the tools are chips.
        assert!(
            open.contains("You help.\n  Indented &lt;b&gt;line&lt;/b&gt;</pre>"),
            "{open}"
        );
        assert_eq!(open.matches("<li ").count(), 2);
        assert_ne!(open_key, folded_key, "opening changes its key");
        // A request that deferred a tool lists it apart.
        state.model_deferred_tools = vec!["bash".to_owned()].into();
        let (_, split) = pinned_rows(&state, &expanded, None).unwrap();
        assert!(split.contains("tools · 1 provided, 1 searchable (last request)"));
        assert!(split.contains("data-model-tools=\"provided\""), "{split}");
        assert!(split.contains("data-model-tools=\"searchable\""), "{split}");
        assert!(
            !split.contains("data-deferred-prompt-link"),
            "no prompt yet"
        );
        // Once announced, the searchable tools take their group's hue, after
        // a link to the prompt.
        state.model_tools = Some(std::sync::Arc::from(vec![
            "read".to_owned(),
            "bash".to_owned(),
            "chrome_click".to_owned(),
        ]));
        state.model_deferred_tools = vec!["bash".to_owned(), "chrome_click".to_owned()].into();
        state.deferred_announcements = vec!["The following deferred tool groups …".to_owned()];
        state.deferred_tool_groups = [("bash", "shell"), ("chrome_click", "chrome")]
            .into_iter()
            .map(|(tool, group)| (tool.to_owned(), group.to_owned()))
            .collect();
        let (_, linked) = pinned_rows(&state, &expanded, None).unwrap();
        assert!(
            linked.contains("data-action=\"deferred-prompt\""),
            "{linked}"
        );
        assert!(linked.contains(">[see full prompt]</button>"), "{linked}");
        assert!(linked.contains("data-tool-group=\"shell\""), "{linked}");
    }

    /// A session whose one reply has a folded thinking block, a finished
    /// call, and text still streaming, and the call's ID.
    fn streaming_reply() -> (SessionState, cookie_agent_protocol::ToolCallId) {
        use cookie_agent_client_state::{AssistantChild, FrozenAssistantAttribution};
        use cookie_agent_protocol::{
            AdaptorId, AgentId, ModelSelection, ProviderId, ProviderModelId, ResolvedModelRef,
            Sha256Digest,
        };

        let call = run("Run `ls`", "ls");
        let call_id = call.id;
        let selection = ModelSelection {
            model: "gateway/model".parse().expect("model key"),
            variant: None,
        };
        let attribution = FrozenAssistantAttribution {
            agent: AgentId::new("primary").expect("agent"),
            acp_model: None,
            resolved_model: ResolvedModelRef {
                provider_id: ProviderId::new("gateway").expect("provider"),
                model_id: ProviderModelId::new("model").expect("model"),
                adapter_id: AdaptorId::OPENAI_COMPATIBLE,
                selection_fingerprint: Sha256Digest::of_bytes(b"selection"),
                selection,
            },
        };
        let mut markdown = cookie_agent_client_state::markdown::MarkdownDocument::default();
        markdown.append("# Settled\n\nA settled paragraph.\n\nthe open tail");
        let state = SessionState {
            transcript: vec![TranscriptItem::Assistant {
                id: 1,
                version: 0,
                attribution,
                committed_turn_seq: None,
                child_times: Vec::new(),
                children: vec![
                    AssistantChild::Thinking {
                        id: 2,
                        version: 0,
                        text: "weighing".into(),
                    },
                    AssistantChild::Tool { call_id },
                    AssistantChild::Text {
                        id: 3,
                        version: 0,
                        markdown,
                    },
                ],
            }],
            tools: [(call_id, call)].into_iter().collect(),
            ..SessionState::default()
        };
        (state, call_id)
    }

    /// The reply row of `state` as the reduced mode draws it with
    /// `expanded`, its parts' HTML joined.
    fn reduced_reply(state: &SessionState, expanded: &Expanded) -> String {
        let names = ModelDisplayNames::default();
        let view = TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state,
            names: &names,
            minimum_level: EventLevel::Warning,
            expanded,
            delegates: &NO_DELEGATES,
            homes: &NO_HOMES,
            transcript: TranscriptMode::Reduced,
            now: jiff::Timestamp::UNIX_EPOCH,
        };
        let rows = super::transcript_rows(&view, None, &mut super::RowCache::default());
        reply_parts(&rows)
            .into_iter()
            .map(|(_, part)| match part {
                super::PartHtml::Html(html) => html.to_string(),
                super::PartHtml::Markdown(blocks) => blocks.concat(),
            })
            .collect()
    }

    #[test]
    fn a_reduced_reply_folds_its_work_into_one_row_that_opens_it() {
        let (state, _) = streaming_reply();
        let folded = reduced_reply(&state, &Expanded::default());
        assert!(folded.contains("data-work=\"done\""), "{folded}");
        assert!(folded.contains("Worked for 0s · 1 tool call"), "{folded}");
        assert!(folded.contains("aria-expanded=\"false\""), "{folded}");
        assert!(folded.contains("the open tail"), "{folded}");
        assert!(!folded.contains("data-tool"), "{folded}");
        assert!(!folded.contains("thought"), "{folded}");

        let opened = reduced_reply(&state, &Expanded::from_iter([super::work_key(FIRST_FOLD)]));
        assert!(opened.contains("aria-expanded=\"true\""), "{opened}");
        assert!(opened.contains("data-tool"), "{opened}");
        assert!(opened.contains("thought"), "{opened}");
        assert!(opened.contains("the open tail"), "{opened}");

        // Folding every work row leaves other opened blocks open.
        let mut expanded =
            Expanded::from_iter([super::work_key(FIRST_FOLD), "thinking:2".to_owned()]);
        expanded.fold_work();
        assert!(!expanded.work(FIRST_FOLD));
        assert!(expanded.thinking(2));
    }

    #[test]
    fn a_run_split_by_a_background_command_folds_into_one_work_row() {
        let (mut state, _) = streaming_reply();
        // What the first block writes between its call and the next block's
        // work is filler, which folds with the work.
        let TranscriptItem::Assistant { children, .. } = &mut state.transcript[0] else {
            unreachable!()
        };
        let mut note = cookie_agent_client_state::markdown::MarkdownDocument::default();
        note.append("…");
        children[2] = cookie_agent_client_state::AssistantChild::Text {
            id: 3,
            version: 0,
            markdown: note,
        };
        let TranscriptItem::Assistant { attribution, .. } = &state.transcript[0] else {
            unreachable!()
        };
        let attribution = attribution.clone();
        let mut answer = cookie_agent_client_state::markdown::MarkdownDocument::default();
        answer.append("All done.");
        state.transcript.extend([
            TranscriptItem::AgentBackgroundEnded {
                id: 5,
                seq: 5,
                ended_at: jiff::Timestamp::UNIX_EPOCH,
                ended: serde_json::from_value(serde_json::json!({
                    "tool_call_id": cookie_agent_protocol::ToolCallId::new_v7(),
                    "tool": "Bash",
                    "display": "npm run dev",
                    "started_at": "2026-10-29T10:00:00Z"
                }))
                .expect("background ended"),
            },
            TranscriptItem::Assistant {
                id: 6,
                version: 0,
                attribution,
                committed_turn_seq: None,
                child_times: Vec::new(),
                children: vec![
                    cookie_agent_client_state::AssistantChild::Thinking {
                        id: 7,
                        version: 0,
                        text: "checking".into(),
                    },
                    cookie_agent_client_state::AssistantChild::Text {
                        id: 8,
                        version: 0,
                        markdown: answer,
                    },
                ],
            },
        ]);
        let run = cookie_agent_protocol::RunId::new_v7();
        state.assistant_runs = [(1, run), (6, run)].into_iter().collect();
        let names = ModelDisplayNames::default();
        let draw = |expanded: &Expanded| {
            let view = TranscriptView {
                session_id: cookie_agent_protocol::SessionId::new_v7(),
                state: &state,
                names: &names,
                minimum_level: EventLevel::Warning,
                expanded,
                delegates: &NO_DELEGATES,
                homes: &NO_HOMES,
                transcript: TranscriptMode::Reduced,
                now: jiff::Timestamp::UNIX_EPOCH,
            };
            let rows = super::transcript_rows(&view, None, &mut super::RowCache::default());
            rows.order
                .iter()
                .map(|id| match &rows.html[id] {
                    super::RowHtml::Html(html) => html.to_string(),
                    super::RowHtml::Reply { header, parts } => {
                        let mut html = header.to_string();
                        for (_, part) in parts.iter() {
                            match part {
                                super::PartHtml::Html(part) => html.push_str(part),
                                super::PartHtml::Markdown(blocks) => {
                                    html.push_str(&blocks.concat())
                                }
                            }
                        }
                        html
                    }
                })
                .collect::<Vec<_>>()
        };
        let folded = draw(&Expanded::default());
        // The two blocks, the background row hidden between them.
        assert_eq!(folded.len(), 2, "{folded:?}");
        let all = folded.concat();
        assert_eq!(all.matches("data-work=").count(), 1, "{all}");
        assert!(all.contains("Worked for 0s · 1 tool call"), "{all}");
        assert!(!all.contains("npm run dev"), "{all}");
        assert!(
            !all.contains("…"),
            "the first block's filler is work: {all}"
        );
        assert!(folded[1].contains("All done."), "{}", folded[1]);
        assert!(
            !folded[1].contains("primary"),
            "the later block has no header: {}",
            folded[1]
        );

        let opened = draw(&Expanded::from_iter([super::work_key(FIRST_FOLD)]));
        assert_eq!(opened.len(), 3, "{opened:?}");
        let all = opened.concat();
        assert!(all.contains("npm run dev"), "{all}");
        assert!(all.contains("…"), "{all}");
    }

    #[test]
    fn a_text_between_calls_shows_between_two_work_rows() {
        let (mut state, _) = streaming_reply();
        let TranscriptItem::Assistant { children, .. } = &mut state.transcript[0] else {
            unreachable!()
        };
        // Its text runs over more lines: an answer, which the next work
        // follows.
        children.push(cookie_agent_client_state::AssistantChild::Thinking {
            id: 9,
            version: 0,
            text: "checking".into(),
        });
        let mut done = cookie_agent_client_state::markdown::MarkdownDocument::default();
        done.append("All done.");
        children.push(cookie_agent_client_state::AssistantChild::Text {
            id: 10,
            version: 0,
            markdown: done,
        });
        let html = reduced_reply(&state, &Expanded::default());
        assert_eq!(html.matches("data-work=").count(), 2, "{html}");
        let (first, second) = (
            html.find("data-work=").expect("a row"),
            html.rfind("data-work=").expect("a row"),
        );
        let answer = html.find("the open tail").expect("the text shows");
        assert!(first < answer && answer < second, "{html}");
        assert!(html.contains("All done."), "{html}");
    }

    #[test]
    fn a_reply_keys_apart_in_each_mode() {
        let (state, _) = streaming_reply();
        let names = ModelDisplayNames::default();
        let expanded = Expanded::default();
        let view = |transcript| TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state: &state,
            names: &names,
            minimum_level: EventLevel::Warning,
            expanded: &expanded,
            delegates: &NO_DELEGATES,
            homes: &NO_HOMES,
            transcript,
            now: jiff::Timestamp::UNIX_EPOCH,
        };
        let reduced = view(TranscriptMode::Reduced);
        let full = TranscriptView {
            session_id: reduced.session_id,
            ..view(TranscriptMode::Full)
        };
        let item = &state.transcript[0];
        assert_ne!(item_key(&reduced, item), item_key(&full, item));
    }

    /// The reply row's parts.
    fn reply_parts(rows: &super::Rows) -> Vec<(u64, super::PartHtml)> {
        assert_eq!(rows.order.len(), 1);
        match &rows.html[&rows.order[0]] {
            super::RowHtml::Reply { parts, .. } => parts.to_vec(),
            super::RowHtml::Html(html) => panic!("not a reply: {html}"),
        }
    }

    /// Streams `text` into the reply's text and `thinking` into its folded
    /// thinking block, as deltas do.
    fn stream(state: &mut SessionState, text: &str, thinking: &str) {
        use cookie_agent_client_state::AssistantChild;
        let TranscriptItem::Assistant {
            version, children, ..
        } = &mut state.transcript[0]
        else {
            unreachable!()
        };
        *version += 1;
        for child in children {
            match child {
                AssistantChild::Text {
                    version, markdown, ..
                } => {
                    *version += 1;
                    markdown.append(text);
                }
                AssistantChild::Thinking {
                    version, text: own, ..
                } => {
                    *version += 1;
                    own.push_str(thinking);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn a_streaming_reply_draws_again_only_its_open_tail() {
        let (mut state, _) = streaming_reply();
        let names = ModelDisplayNames::default();
        let expanded = Expanded::default();
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let mut cache = super::RowCache::default();
        let draw = |state: &SessionState, cache: &mut super::RowCache| {
            let view = TranscriptView {
                session_id,
                state,
                names: &names,
                minimum_level: EventLevel::Warning,
                expanded: &expanded,
                delegates: &NO_DELEGATES,
                homes: &NO_HOMES,
                transcript: TranscriptMode::Full,
                now: jiff::Timestamp::UNIX_EPOCH,
            };
            super::transcript_rows(&view, None, cache)
        };
        let first = draw(&state, &mut cache);
        let drawn = cache.drawn;
        stream(&mut state, " grows\n\nand settles", " further");
        let second = draw(&state, &mut cache);
        // The reply drew again; its folded thinking and its call did not.
        assert_eq!(cache.drawn - drawn, 1);
        assert_eq!(first.order, second.order, "the row keeps its ID");
        let (before, after) = (reply_parts(&first), reply_parts(&second));
        assert_eq!(before.len(), after.len());
        for ((id, old), (new_id, new)) in before.iter().zip(&after) {
            assert_eq!(id, new_id, "a part keeps its ID");
            match (old, new) {
                (super::PartHtml::Html(old), super::PartHtml::Html(new)) => {
                    assert!(std::sync::Arc::ptr_eq(old, new), "{new}");
                }
                (super::PartHtml::Markdown(old), super::PartHtml::Markdown(new)) => {
                    // The settled blocks keep their HTML; the tail moved on.
                    assert!(std::sync::Arc::ptr_eq(&old[0], &new[0]));
                    assert!(std::sync::Arc::ptr_eq(&old[1], &new[1]));
                    assert_ne!(old[2], new[2]);
                    assert!(new[2].contains("the open tail grows"), "{}", new[2]);
                    assert!(new.last().expect("a tail").contains("and settles"));
                }
                _ => panic!("a part changed its kind"),
            }
        }
        // Nothing changed: nothing draws, and the rows compare equal.
        let drawn = cache.drawn;
        assert_eq!(draw(&state, &mut cache), second);
        assert_eq!(cache.drawn, drawn);
    }

    #[test]
    fn opening_a_call_in_a_streaming_reply_draws_only_that_call() {
        let (mut state, call_id) = streaming_reply();
        let names = ModelDisplayNames::default();
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let mut cache = super::RowCache::default();
        let draw = |state: &SessionState, expanded: &Expanded, cache: &mut super::RowCache| {
            let view = TranscriptView {
                session_id,
                state,
                names: &names,
                minimum_level: EventLevel::Warning,
                expanded,
                delegates: &NO_DELEGATES,
                homes: &NO_HOMES,
                transcript: TranscriptMode::Full,
                now: jiff::Timestamp::UNIX_EPOCH,
            };
            super::transcript_rows(&view, None, cache)
        };
        let folded = Expanded::default();
        let open = Expanded::from_iter([call_id.to_string()]);
        let first = draw(&state, &folded, &mut cache);
        stream(&mut state, " more", " more");
        draw(&state, &folded, &mut cache);
        let drawn = cache.drawn;
        let opened = draw(&state, &open, &mut cache);
        // The reply and its call drew again, nothing else.
        assert_eq!(cache.drawn - drawn, 2);
        let (before, after) = (reply_parts(&first), reply_parts(&opened));
        let super::PartHtml::Html(call) = &after[1].1 else {
            panic!("the call is not HTML")
        };
        assert!(call.contains("aria-expanded=\"true\""), "{call}");
        let (super::PartHtml::Html(old), super::PartHtml::Html(new)) = (&before[0].1, &after[0].1)
        else {
            panic!("the thinking block is not HTML")
        };
        assert!(
            std::sync::Arc::ptr_eq(old, new),
            "the folded thinking drew again"
        );
    }

    #[test]
    fn an_acp_notice_and_a_steered_message_render() {
        let mut state = SessionState::default();
        state.acp.steered.insert(7);
        let names = ModelDisplayNames::default();
        let expanded = Expanded::default();
        let view = TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state: &state,
            names: &names,
            minimum_level: EventLevel::Warning,
            expanded: &expanded,
            delegates: &NO_DELEGATES,
            homes: &NO_HOMES,
            transcript: TranscriptMode::Full,
            now: jiff::Timestamp::UNIX_EPOCH,
        };
        let user = item_html(
            &view,
            &TranscriptItem::User {
                id: 1,
                version: 0,
                markdown: cookie_agent_client_state::markdown::MarkdownDocument::new(
                    "look at notes.txt".into(),
                )
                .finished(),
                seq: 7,
            },
        )
        .unwrap();
        // A message steered into an ACP agent's turn is an ordinary bubble,
        // as a model run's steered input is: it reaches the transcript only
        // once the agent took it.
        assert!(!user.contains("data-steered"), "{user}");
        assert!(!user.contains("steered into"), "{user}");
        let notice = item_html(
            &view,
            &TranscriptItem::AcpNotice {
                id: 2,
                version: 0,
                notice: cookie_agent_protocol::AcpNotice {
                    severity: cookie_agent_protocol::AcpNoticeSeverity::Warning,
                    title: "Rate limited".into(),
                    description: Some("Retrying <soon>.".into()),
                },
            },
        )
        .expect("no level filter hides a notice");
        assert!(notice.contains("data-severity=\"warning\""), "{notice}");
        assert!(notice.contains("<span>Rate limited</span>"), "{notice}");
        assert!(notice.contains("Retrying &lt;soon&gt;."), "{notice}");
    }

    #[test]
    fn a_late_presentation_draws_its_call_again_with_the_new_title() {
        let (mut state, call_id) = streaming_reply();
        let names = ModelDisplayNames::default();
        let expanded = Expanded::default();
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let mut cache = super::RowCache::default();
        let draw = |state: &SessionState, cache: &mut super::RowCache| {
            let view = TranscriptView {
                session_id,
                state,
                names: &names,
                minimum_level: EventLevel::Warning,
                expanded: &expanded,
                delegates: &NO_DELEGATES,
                homes: &NO_HOMES,
                transcript: TranscriptMode::Full,
                now: jiff::Timestamp::UNIX_EPOCH,
            };
            super::transcript_rows(&view, None, cache)
        };
        let first = draw(&state, &mut cache);
        let drawn = cache.drawn;
        // The agent's raw input arrived after the call started.
        let mut store = cookie_agent_client_state::StateStore::default();
        store.sessions.insert(session_id, state);
        store.apply_event(cookie_agent_protocol::StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id: None,
            seq: 1,
            timestamp: jiff::Timestamp::now(),
            payload: cookie_agent_protocol::EventPayload::AcpSessionUpdated {
                update: cookie_agent_protocol::AcpSessionUpdate {
                    tool_presentation: Some(cookie_agent_protocol::AcpToolPresentation {
                        tool_call_id: call_id,
                        presentation: cookie_agent_protocol::PreparedPresentation::new(
                            cookie_agent_protocol::SafeDisplayText::new("Read notes.txt")
                                .expect("title"),
                            None,
                        ),
                    }),
                    ..cookie_agent_protocol::AcpSessionUpdate::default()
                },
            },
        });
        state = store.sessions.remove(&session_id).expect("the session");
        let second = draw(&state, &mut cache);
        // The reply and its call drew again; the folded thinking did not.
        assert_eq!(cache.drawn - drawn, 2);
        assert_eq!(first.order, second.order);
        let (before, after) = (reply_parts(&first), reply_parts(&second));
        let super::PartHtml::Html(call) = &after[1].1 else {
            panic!("the call is not HTML")
        };
        assert!(call.contains("Read notes.txt"), "{call}");
        let (super::PartHtml::Html(old), super::PartHtml::Html(new)) = (&before[0].1, &after[0].1)
        else {
            panic!("the thinking block is not HTML")
        };
        assert!(std::sync::Arc::ptr_eq(old, new));
    }

    #[test]
    fn a_reply_footer_that_lands_after_the_reply_settles_draws_it_again() {
        let (mut state, _) = streaming_reply();
        let names = ModelDisplayNames::default();
        let expanded = Expanded::default();
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let mut cache = super::RowCache::default();
        let draw = |state: &SessionState, cache: &mut super::RowCache| {
            let view = TranscriptView {
                session_id,
                state,
                names: &names,
                minimum_level: EventLevel::Warning,
                expanded: &expanded,
                delegates: &NO_DELEGATES,
                homes: &NO_HOMES,
                transcript: TranscriptMode::Full,
                now: jiff::Timestamp::UNIX_EPOCH,
            };
            (
                item_key(&view, &state.transcript[0]),
                super::transcript_rows(&view, None, cache),
            )
        };
        let (before_key, before) = draw(&state, &mut cache);
        assert_eq!(reply_parts(&before).len(), 3, "no footer without metrics");
        // The usage arrives without the reply's version moving.
        let metrics = state.assistant_metrics.entry(1).or_default();
        metrics.timed_output_tokens = 421;
        metrics.generation = std::time::Duration::from_secs(10);
        metrics.timed_turns = 1;
        metrics.context_tokens = Some(12_500);
        metrics.estimated_cost_pico_usd = Some(4_000_000_000);
        let (metrics_key, metrics) = draw(&state, &mut cache);
        assert_ne!(metrics_key, before_key, "the metrics change the row's key");
        let footer = |rows: &super::Rows| match reply_parts(rows).last() {
            Some((_, super::PartHtml::Html(html))) => html.to_string(),
            other => panic!("no footer: {other:?}"),
        };
        assert!(
            footer(&metrics).ends_with("data-reply-footer>⚡ 42.1 tps · 12.5K ctx · $0.0040</div>"),
            "{}",
            footer(&metrics)
        );
        state.interrupted_assistant_items.insert(1);
        let (interrupted_key, interrupted) = draw(&state, &mut cache);
        assert_ne!(interrupted_key, metrics_key);
        assert!(
            footer(&interrupted).contains("$0.0040 · interrupted</div>"),
            "{}",
            footer(&interrupted)
        );
    }

    #[test]
    fn a_sealed_thinking_block_says_how_long_it_thought() {
        let (mut state, _) = streaming_reply();
        let names = ModelDisplayNames::default();
        let expanded = Expanded::default();
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let mut cache = super::RowCache::default();
        let draw = |state: &SessionState, cache: &mut super::RowCache| {
            let view = TranscriptView {
                session_id,
                state,
                names: &names,
                minimum_level: EventLevel::Warning,
                expanded: &expanded,
                delegates: &NO_DELEGATES,
                homes: &NO_HOMES,
                transcript: TranscriptMode::Full,
                now: jiff::Timestamp::UNIX_EPOCH,
            };
            super::transcript_rows(&view, None, cache)
        };
        let thinking = |rows: &super::Rows| match &reply_parts(rows)[0].1 {
            super::PartHtml::Html(html) => html.to_string(),
            super::PartHtml::Markdown(_) => panic!("the thinking block is not HTML"),
        };
        let untimed = thinking(&draw(&state, &mut cache));
        assert!(untimed.contains(">thought</span>"), "{untimed}");
        state
            .thinking_durations
            .insert((1, 2), std::time::Duration::from_secs(65));
        let timed = thinking(&draw(&state, &mut cache));
        assert!(timed.contains(">thought for 1m 5s</span>"), "{timed}");
    }

    #[test]
    fn only_rows_and_parts_whose_new_html_holds_code_or_media_are_filled_in() {
        let (mut state, call_id) = streaming_reply();
        let names = ModelDisplayNames::default();
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let mut cache = super::RowCache::default();
        let draw = |state: &SessionState, expanded: &Expanded, cache: &mut super::RowCache| {
            let view = TranscriptView {
                session_id,
                state,
                names: &names,
                minimum_level: EventLevel::Warning,
                expanded,
                delegates: &NO_DELEGATES,
                homes: &NO_HOMES,
                transcript: TranscriptMode::Full,
                now: jiff::Timestamp::UNIX_EPOCH,
            };
            super::transcript_rows(&view, None, cache)
        };
        let folded = Expanded::default();
        let first = draw(&state, &folded, &mut cache);
        let row = first.order[0];
        let call = reply_parts(&first)[1].0;
        // Nothing in it to fill in, drawn or not; text fills itself in.
        assert!(super::to_fill_in(&super::Rows::default(), &first).is_empty());
        stream(&mut state, "\n\n```rust\nfn main() {}\n```\n\n", "");
        let coded = draw(&state, &folded, &mut cache);
        assert!(super::to_fill_in(&first, &coded).is_empty());
        // A call whose output shows media, opened.
        let tool = state.tools.get_mut(&call_id).expect("the call");
        tool.display = vec![cookie_agent_protocol::Open::Known(
            cookie_agent_protocol::DisplayBlock::Media {
                artifact: serde_json::from_value(serde_json::json!({
                    "sha256": "c".repeat(64),
                    "byte_length": 3,
                    "media_type": "image/png",
                }))
                .expect("artifact"),
                alt: None,
            },
        )];
        tool.revision += 1;
        let opened = Expanded::from_iter([call_id.to_string()]);
        let shown = draw(&state, &opened, &mut cache);
        assert_eq!(
            super::to_fill_in(&coded, &shown),
            [super::part_element_id(row, call)]
        );
        // Nothing changed: nothing to fill in.
        assert!(super::to_fill_in(&shown, &draw(&state, &opened, &mut cache)).is_empty());
    }

    /// The cost of one delta at the end of a reply of 4000 parts (half of
    /// them calls, half text), the pass a streaming frame makes. Run with
    /// `cargo test -p cookie_agent_web --release -- --ignored --nocapture
    /// a_delta_in_a_long_reply`.
    #[test]
    #[ignore = "a timing, not a check"]
    fn a_delta_in_a_long_reply() {
        use cookie_agent_client_state::AssistantChild;
        let (mut state, _) = streaming_reply();
        let mut calls = Vec::new();
        let TranscriptItem::Assistant { children, .. } = &mut state.transcript[0] else {
            unreachable!()
        };
        let tail = children.pop().expect("the text");
        for index in 0..2000u64 {
            let call = run("Run `ls`", "ls");
            children.push(AssistantChild::Tool { call_id: call.id });
            calls.push(call);
            let mut markdown = cookie_agent_client_state::markdown::MarkdownDocument::default();
            markdown.append(&format!("Step {index} looked at the **files**.\n\n"));
            children.push(AssistantChild::Text {
                id: 10 + index,
                version: 0,
                markdown: markdown.finished(),
            });
        }
        children.push(tail);
        state
            .tools
            .extend(calls.into_iter().map(|call| (call.id, call)));
        let names = ModelDisplayNames::default();
        let expanded = Expanded::default();
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let mut cache = super::RowCache::default();
        let draw = |state: &SessionState, cache: &mut super::RowCache| {
            let view = TranscriptView {
                session_id,
                state,
                names: &names,
                minimum_level: EventLevel::Warning,
                expanded: &expanded,
                delegates: &NO_DELEGATES,
                homes: &NO_HOMES,
                transcript: TranscriptMode::Full,
                now: jiff::Timestamp::UNIX_EPOCH,
            };
            super::transcript_rows(&view, None, cache)
        };
        draw(&state, &mut cache);
        let rounds = 200;
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            let TranscriptItem::Assistant {
                version, children, ..
            } = &mut state.transcript[0]
            else {
                unreachable!()
            };
            *version += 1;
            if let Some(AssistantChild::Text {
                version, markdown, ..
            }) = children.last_mut()
            {
                *version += 1;
                markdown.append(" more");
            }
            std::hint::black_box(draw(&state, &mut cache));
        }
        let per_delta = started.elapsed() / rounds;
        println!("one delta in a 4000-part reply: {per_delta:?}");
    }
}

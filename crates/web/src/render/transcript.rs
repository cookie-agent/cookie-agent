//! A session's transcript items as HTML, from the shared reducer's
//! projection. Each item renders on its own, so the page re-renders only
//! the items whose [`item_key`] changed.

use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasher as _, Hash, Hasher as _},
};

use cookie_agent_client_state::{
    AssistantChild, CompactingSource, EventLevel, ModelDisplayNames, ProducerMessageStatus,
    SessionState, ToolCallState, ToolStatus, TranscriptItem,
    rows::{
        display_line_count, goal_status_label, producer_mode_label, producer_owner_label,
        producer_summary,
    },
};
use cookie_agent_protocol::{ContextCheckpoint, ExtensionMessageRole, PersistedFileSource};

use super::{
    blocks::{BlockPlace, display_html, media_html},
    budget::{ALL_OUTPUT_LINES, OUTPUT_LINES, within_budget},
    escape,
    markdown::document_html,
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
    /// Tool calls the user expanded.
    pub expanded: &'a HashSet<String>,
    /// The subagents this session's delegate calls started, by call.
    pub delegates: &'a HashMap<cookie_agent_protocol::ToolCallId, Delegate>,
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

/// A key that changes whenever the item's HTML would: its version, the
/// revisions of the tool calls it shows, and their expansion.
pub fn item_key(view: &TranscriptView<'_>, item: &TranscriptItem) -> u64 {
    let mut hasher = foldhash::fast::FixedState::default().build_hasher();
    view.session_id.hash(&mut hasher);
    item.id().hash(&mut hasher);
    item.version().hash(&mut hasher);
    std::mem::discriminant(item).hash(&mut hasher);
    if let TranscriptItem::Assistant { children, .. } = item {
        children.len().hash(&mut hasher);
        for child in children {
            child.version().hash(&mut hasher);
            if let AssistantChild::Tool { call_id } = child {
                let id = call_id.to_string();
                view.expanded.contains(&id).hash(&mut hasher);
                view.expanded
                    .contains(&all_output_key(&id))
                    .hash(&mut hasher);
                // Raw arguments show only at `info` and below.
                (view.minimum_level <= EventLevel::Info).hash(&mut hasher);
                if let Some(tool) = view.state.tools.get(call_id) {
                    tool.revision.hash(&mut hasher);
                    view.waiting_approval(tool).hash(&mut hasher);
                }
                view.delegates.get(call_id).hash(&mut hasher);
            }
        }
    }
    if let Some(key) = block_key(item) {
        view.expanded.contains(&key).hash(&mut hasher);
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
            attribution,
            children,
            ..
        } => {
            let mut html = format!(
                "<div class=\"{NOTE} font-mono\">{}</div>",
                escape(&attribution.header(view.names))
            );
            for child in children {
                html.push_str(&child_html(view, child));
            }
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
            };
            collapsible(
                view,
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
                ContextCheckpoint::InternalSummary { checkpoint } => {
                    ("internal summary", checkpoint.summary().to_owned())
                }
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
            collapsible(view, &key, icondata::FaBroomSolid, &header, None, &body)
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
                view,
                &key,
                icondata::FaPuzzlePieceSolid,
                &header,
                None,
                input,
            )
        }
        TranscriptItem::AgentMd { entries, .. } => {
            let key = block_key(item).unwrap_or_default();
            let lines: usize = entries
                .iter()
                .map(|entry| display_line_count(&entry.content))
                .sum();
            let files = match entries.as_slice() {
                [entry] => entry.source.as_str().to_owned(),
                _ => format!("{} files", entries.len()),
            };
            let body = entries
                .iter()
                .map(|entry| {
                    if entries.len() > 1 {
                        format!("── {}\n{}", entry.source.as_str(), entry.content)
                    } else {
                        entry.content.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let header = format!("AGENTS.md · {files} ({lines} lines)");
            collapsible(view, &key, icondata::FaBookSolid, &header, None, &body)
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
                view,
                &key,
                icondata::FaWandMagicSparklesSolid,
                &header,
                Some(&detail),
                rendered_body,
            )
        }
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
                    view,
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

/// The key under which a collapsible row's expansion is kept, shared with
/// the tool calls' in the page's expanded set.
fn block_key(item: &TranscriptItem) -> Option<String> {
    Some(match item {
        TranscriptItem::Compacting { seq, .. } => format!("compacting:{seq}"),
        TranscriptItem::Compaction { seq, .. } => format!("compaction:{seq}"),
        TranscriptItem::PluginMessage { seq, .. } => format!("plugin:{seq}"),
        TranscriptItem::AgentMd { seq, .. } => format!("agent-md:{seq}"),
        TranscriptItem::SkillLoaded { seq, .. } => format!("skill:{seq}"),
        TranscriptItem::ProducerMessage { message_id, .. } => format!("producer:{message_id}"),
        _ => return None,
    })
}

/// A row that shows `header` and, once clicked open, `detail` and `body`.
fn collapsible(
    view: &TranscriptView<'_>,
    key: &str,
    icon: Icon,
    header: &str,
    detail: Option<&str>,
    body: &str,
) -> String {
    let expanded = view.expanded.contains(key);
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

fn child_html(view: &TranscriptView<'_>, child: &AssistantChild) -> String {
    match child {
        AssistantChild::Text { markdown, .. } => {
            format!("<div class=\"markdown\">{}</div>", document_html(markdown))
        }
        AssistantChild::Thinking { text, .. } => format!(
            "<details class=\"{NOTE}\"><summary class=\"inline-flex min-h-11 cursor-pointer items-center gap-2 rounded-md px-2 hover:bg-toasted hover:text-espresso active:bg-crust dark:hover:bg-ember dark:hover:text-latte dark:active:bg-smoke\">{}thinking</summary><div class=\"border-l border-toasted pl-3 whitespace-pre-wrap dark:border-ember\">{}</div></details>",
            svg(icondata::FaLightbulbRegular, ""),
            escape(text)
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
    let expanded = view.expanded.contains(&id);
    let progress = tool
        .progress
        .as_ref()
        .filter(|_| tool.status == ToolStatus::Running)
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
    let mut html = format!(
        "<div class=\"rounded-lg border border-toasted dark:border-ember\" data-tool=\"{}\" data-expanded=\"{expanded}\"><div class=\"flex items-center\"><button type=\"button\" data-action=\"toggle-tool\" data-call=\"{}\" aria-expanded=\"{expanded}\" class=\"flex min-h-11 min-w-0 flex-1 items-center gap-2 rounded-lg px-2 py-1 text-left font-mono text-sm hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke\">{chevron}{}<span class=\"min-w-0 shrink truncate\" data-header>{}</span>{progress}<span class=\"flex-1\"></span>{status}</button>{approval}{background}</div>",
        escape(&id),
        escape(&id),
        svg(
            icons::hint_icon(tool.presentation.hint.as_ref().map(|hint| hint.rendered())),
            "text-caramel dark:text-maple"
        ),
        escape(&tool.header_text()),
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
        let all = view.expanded.contains(&all_key);
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
    use std::collections::HashSet;

    use cookie_agent_client_state::{
        EventLevel, ModelDisplayNames, ProducerMessageStatus, SessionState, TranscriptItem,
    };

    use super::{Delegate, TranscriptView, item_html, item_key, tool_html};

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

    fn rendered(
        tool: &cookie_agent_client_state::ToolCallState,
        expanded: &HashSet<String>,
    ) -> String {
        let state = SessionState::default();
        let names = ModelDisplayNames::default();
        let view = TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state: &state,
            names: &names,
            minimum_level: EventLevel::Warning,
            expanded,
            delegates: &NO_DELEGATES,
        };
        tool_html(&view, tool)
    }

    /// The text of the header's title span.
    fn header(html: &str) -> &str {
        let start = html.find("data-header>").expect("a header") + "data-header>".len();
        &html[start..start + html[start..].find('<').expect("its end")]
    }

    #[test]
    fn the_collapsed_header_shows_the_title_and_the_argument_it_does_not_name() {
        let named = run("Run `cargo test`", "cargo test");
        let html = rendered(&named, &HashSet::new());
        assert_eq!(header(&html), "Run `cargo test`");
        let unnamed = run("Run a command", "cargo test");
        assert_eq!(
            header(&rendered(&unnamed, &HashSet::new())),
            "Run a command cargo test"
        );
        // Collapsed: no arguments and no output.
        assert!(html.contains("aria-expanded=\"false\""), "{html}");
        assert!(!html.contains("data-tool-body"), "{html}");
        assert!(!html.contains("timeout"), "{html}");
    }

    #[test]
    fn the_open_row_shows_arguments_without_the_one_the_header_shows() {
        let call = run("Run a command", "cargo test");
        let html = rendered(&call, &HashSet::from([call.id.to_string()]));
        assert!(html.contains("aria-expanded=\"true\""), "{html}");
        assert!(html.contains("data-arguments>arguments<"), "{html}");
        assert!(html.contains("timeout"), "{html}");
        assert!(
            !html.contains("<dt class=\"text-ink dark:text-sand\">command"),
            "{html}"
        );
        assert_eq!(call.shown_arguments().len(), 1);
    }

    #[test]
    fn a_running_call_shows_its_progress_while_collapsed() {
        let mut call = run("Run a command", "sleep 8");
        call.status = cookie_agent_client_state::ToolStatus::Running;
        call.progress = Some("compiling\n12 of 40".into());
        let html = rendered(&call, &HashSet::new());
        assert!(
            html.contains("data-progress>· compiling 12 of 40<"),
            "{html}"
        );
        assert!(html.contains("animate-spin"), "{html}");
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
        let open = rendered(&call, &HashSet::from([id.clone()]));
        assert!(open.contains("… 36 more lines (click to expand)"), "{open}");
        assert!(!open.contains("\n65"), "{open}");
        let all = rendered(
            &call,
            &HashSet::from([id.clone(), super::all_output_key(&id)]),
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
        let expanded = HashSet::from([plain.id.to_string(), previewed.id.to_string()]);
        let view = |minimum_level| TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state: &state,
            names: &names,
            minimum_level,
            expanded: &expanded,
            delegates: &NO_DELEGATES,
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
        let expanded = HashSet::new();
        let session_id = cookie_agent_protocol::SessionId::new_v7();
        let view = |minimum_level| TranscriptView {
            session_id,
            state: &state,
            names: &names,
            minimum_level,
            expanded: &expanded,
            delegates: &NO_DELEGATES,
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
        let collapsed = HashSet::new();
        let opened = HashSet::from([format!("producer:{message_id}")]);
        let view = |expanded| TranscriptView {
            session_id: cookie_agent_protocol::SessionId::new_v7(),
            state: &state,
            names: &names,
            minimum_level: EventLevel::Warning,
            expanded,
            delegates: &NO_DELEGATES,
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
}

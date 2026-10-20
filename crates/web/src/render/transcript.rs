//! A session's transcript items as HTML, from the shared reducer's
//! projection. Each item renders on its own, so the page re-renders only
//! the items whose [`item_key`] changed.

use std::{
    collections::HashSet,
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
                // Raw arguments show only at `info` and below.
                (view.minimum_level <= EventLevel::Info).hash(&mut hasher);
                if let Some(tool) = view.state.tools.get(call_id) {
                    tool.revision.hash(&mut hasher);
                }
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
                "<div class=\"user-message group ml-auto flex max-w-[90%] items-start gap-1\" data-seq=\"{seq}\"><button type=\"button\" data-action=\"user-menu\" data-seq=\"{seq}\" aria-label=\"Message actions\" title=\"Message actions\" class=\"inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-md text-ink hover:bg-toasted active:bg-crust dark:text-sand dark:hover:bg-ember dark:active:bg-smoke\">{}</button><div class=\"min-w-0 rounded-2xl rounded-tr-sm bg-toasted px-3 py-2 dark:bg-ember\"><div class=\"markdown\">{}</div>{files}</div></div>",
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
                // The canonical `/goal` action, rebuilt from the activation
                // event: shown like a message, with no message menu.
                format!(
                    "<div class=\"goal-activation ml-auto max-w-[90%] rounded-lg border border-caramel px-3 py-2 font-mono text-sm whitespace-pre-wrap dark:border-maple\">{}</div>",
                    escape(&format!("/goal {}", goal.objective))
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
    let expanded = view.expanded.contains(&id) || tool.status == ToolStatus::Running;
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
    let mut html = format!(
        "<div class=\"rounded-lg border border-toasted dark:border-ember\" data-tool=\"{}\"><div class=\"flex items-center\"><button type=\"button\" data-action=\"toggle-tool\" data-call=\"{}\" aria-expanded=\"{expanded}\" class=\"flex min-h-11 min-w-0 flex-1 items-center gap-2 rounded-lg px-2 py-1 text-left font-mono text-sm hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke\">{}<span class=\"flex-1 truncate\">{}</span>{status}</button>{background}</div>",
        escape(&id),
        escape(&id),
        svg(
            icons::hint_icon(tool.presentation.hint.as_ref().map(|hint| hint.rendered())),
            "text-caramel dark:text-maple"
        ),
        escape(&tool.header_text()),
    );
    if let Some(progress) = tool
        .progress
        .as_ref()
        .filter(|_| tool.status == ToolStatus::Running)
    {
        html.push_str(&format!(
            "<div class=\"px-2 {NOTE}\">{}</div>",
            escape(progress)
        ));
    }
    if expanded {
        html.push_str("<div class=\"flex flex-col gap-1 px-2 pb-2\">");
        if tool.presentation.arguments.is_empty() {
            // As in the TUI: a presentation with a preview is complete as
            // given, and raw arguments are diagnostic detail at `info` and
            // below.
            if !tool.arguments.is_empty()
                && tool.presentation.preview.is_empty()
                && view.minimum_level <= EventLevel::Info
            {
                html.push_str(&format!(
                    "<pre class=\"overflow-x-auto whitespace-pre-wrap font-mono text-xs\">{}</pre>",
                    escape(&tool.arguments)
                ));
            }
        } else {
            html.push_str(&display_html(
                &tool.presentation.arguments,
                BlockPlace::plain,
            ));
        }
        if let Some(failure) = &tool.failure {
            html.push_str(&format!(
                "<div class=\"text-sm text-red-700 dark:text-red-400\">{}</div>",
                escape(failure)
            ));
        }
        html.push_str(&display_html(tool.shown_display(), |index| BlockPlace {
            call_id: Some(&id),
            index,
            full: tool.full_content(index),
        }));
        html.push_str("</div>");
    }
    html.push_str("</div>");
    html
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use cookie_agent_client_state::{
        EventLevel, ModelDisplayNames, ProducerMessageStatus, SessionState, TranscriptItem,
    };

    use super::{TranscriptView, item_html, item_key, tool_html};

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

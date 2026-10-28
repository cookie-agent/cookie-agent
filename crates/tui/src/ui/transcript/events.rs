//! Goal, producer, compaction, plugin, and media event layouts.

use super::*;

pub(super) use cookie_agent_client_state::rows::goal_status_label;
pub(in crate::ui) use cookie_agent_client_state::rows::producer_summary;
use cookie_agent_client_state::rows::{
    AGENT_COMPACTION_HEADER, agent_session_message_summary, producer_mode_label,
    producer_owner_label,
};

pub(super) fn goal_activation_layout(
    goal: &GoalState,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // Canonical action reconstructed from GoalActivated, not a local command
    // echo or UserInputSubmitted. It has no prompt recall/revert hit target.
    role_block(
        Role::Action,
        format!("/goal {}", goal.objective)
            .lines()
            .map(|line| Line::from(line.to_owned()))
            .collect(),
        width,
        theme,
    )
}

pub(super) fn goal_layout(goal: &GoalState, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let finished = goal.items.iter().filter(|item| item.finished).count();
    let mut body = vec![
        Line::styled(goal.objective.clone(), theme.assistant()),
        Line::styled(
            format!(
                "status: {} · {finished}/{} finished",
                goal_status_label(goal.status),
                goal.items.len()
            ),
            theme.internal(),
        ),
    ];
    if goal.items.is_empty() {
        body.push(Line::styled("checklist: empty", theme.muted()));
    } else {
        body.extend(goal.items.iter().map(|item| {
            let marker = if item.finished { "[x]" } else { "[ ]" };
            Line::styled(format!("{marker} {}", item.description), theme.internal())
        }));
    }
    role_block(Role::Goal, body, width, theme)
}

/// An ACP agent's plan, as a checklist with each step's status.
pub(super) fn plan_layout(
    entries: &[cookie_agent_protocol::PlanEntry],
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    role_block(Role::Plan, plan_lines(entries, theme), width, theme)
}

/// A notice of an ACP agent: its title, colored by its severity, and its
/// description.
pub(super) fn acp_notice_layout(
    notice: &cookie_agent_protocol::AcpNotice,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    use cookie_agent_protocol::AcpNoticeSeverity;
    let style = match notice.severity {
        AcpNoticeSeverity::Error => theme.error(),
        AcpNoticeSeverity::Warning => theme.warning(),
        AcpNoticeSeverity::Info | AcpNoticeSeverity::Unknown => theme.assistant(),
    };
    let mut body = vec![Line::styled(notice.title.clone(), style)];
    body.extend(
        notice
            .description
            .iter()
            .flat_map(|description| description.lines())
            .map(|line| Line::styled(line.to_owned(), theme.internal())),
    );
    role_block(Role::Notice, body, width, theme)
}

/// The lines of a plan: `[x]` done, `[~]` in progress, `[ ]` pending, and a
/// high or low priority after the step.
pub(super) fn plan_lines(
    entries: &[cookie_agent_protocol::PlanEntry],
    theme: &Theme,
) -> Vec<Line<'static>> {
    use cookie_agent_protocol::{PlanEntryPriority, PlanEntryStatus};
    if entries.is_empty() {
        return vec![Line::styled("no steps", theme.muted())];
    }
    entries
        .iter()
        .map(|entry| {
            let (marker, marker_style, text_style) = match entry.status {
                PlanEntryStatus::Completed => (
                    "[x]",
                    theme.tool_success(),
                    theme
                        .muted()
                        .add_modifier(ratatui::style::Modifier::CROSSED_OUT),
                ),
                PlanEntryStatus::InProgress => ("[~]", theme.tool_running(), theme.assistant()),
                PlanEntryStatus::Pending | PlanEntryStatus::Unknown => {
                    ("[ ]", theme.muted(), theme.internal())
                }
            };
            let mut spans = vec![
                Span::styled(marker, marker_style),
                Span::raw(" "),
                Span::styled(entry.content.clone(), text_style),
            ];
            match entry.priority {
                Some(PlanEntryPriority::High) => {
                    spans.push(Span::styled(" · high", theme.warning()));
                }
                Some(PlanEntryPriority::Low) => {
                    spans.push(Span::styled(" · low", theme.muted()));
                }
                Some(PlanEntryPriority::Medium | PlanEntryPriority::Unknown) | None => {}
            }
            Line::from(spans)
        })
        .collect()
}

pub(super) fn producer_message_layout(
    message_id: ProducerMessageId,
    owner: &ProducerOwner,
    mode: ProducerDeliveryMode,
    body: &str,
    summary: &str,
    status: ProducerMessageStatus,
    context: &TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = BlockId::ProducerMessage(message_id);
    let expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if expanded { '▾' } else { '▸' };
    let header = if context.width < 8 {
        format!("[P] {chevron} …")
    } else {
        format!("◇ {chevron} {summary}")
    };
    let mut lines = vec![Line::styled(
        crate::ui::app::truncate_with_ellipsis(&header, usize::from(context.width)),
        context.theme.internal(),
    )];
    let status = match status {
        ProducerMessageStatus::Claimed => "claimed",
        ProducerMessageStatus::Consumed => "consumed",
        _ => unreachable!("only claimed or consumed messages enter the transcript"),
    };
    if expanded {
        let mut body_lines = vec![Line::styled(
            format!(
                "{} · {} · {status}",
                producer_owner_label(owner),
                producer_mode_label(mode)
            ),
            context.theme.internal(),
        )];
        body_lines.extend(bounded_safe_display_text(
            body,
            context.theme.internal(),
            MAX_EXPANDED_BODY_LINES,
            MAX_EXPANDED_BODY_BYTES,
        ));
        for line in body_lines {
            lines.extend(repeated_prefixed_wrapped_line(
                vec![Span::styled("· ", context.theme.internal())],
                line,
                context.width,
            ));
        }
    }
    ItemLayout {
        regions: vec![BlockRegion {
            id: block_id,
            start_line: 0,
            end_line: lines.len(),
            header_lines: Some(1),
            header_gutter: None,
        }],
        lines,
        user_seq: None,
    }
}

pub(super) fn discarded_producer_message_layout(
    owner: &ProducerOwner,
    mode: ProducerDeliveryMode,
    width: u16,
    theme: &Theme,
) -> ItemLayout {
    let owner = producer_owner_label(owner);
    ItemLayout {
        lines: role_block(
            Role::Debug,
            vec![Line::from(format!(
                "producer message discarded · {owner} · {}",
                producer_mode_label(mode)
            ))],
            width,
            theme,
        ),
        regions: Vec::new(),
        user_seq: None,
    }
}

pub(super) fn system_prompt_layout(
    snapshot: &cookie_agent_protocol::AgentSnapshot,
    draft_agent: Option<&AgentId>,
    expanded: Option<&HashSet<BlockId>>,
    width: u16,
    theme: &Theme,
) -> ItemLayout {
    let block_id = BlockId::SystemPrompt;
    let is_expanded = expanded.is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if is_expanded { '▾' } else { '▸' };
    let mut body = vec![Line::styled(
        format!(
            "📜 {chevron} {}",
            cookie_agent_client_state::rows::system_prompt_summary(snapshot, true, draft_agent)
        ),
        theme.internal(),
    )];
    if is_expanded {
        body.extend(bounded_safe_display_text(
            &snapshot.composed_prompt,
            theme.internal(),
            MAX_SYSTEM_PROMPT_BODY_LINES,
            MAX_SYSTEM_PROMPT_BODY_BYTES,
        ));
    }
    collapsible_event_block(block_id, body, width, theme)
}

/// The tools sent with the latest model request, pinned under the system prompt.
pub(super) fn model_tools_layout(
    state: &cookie_agent_client_state::SessionState,
    tools: &[String],
    expanded: Option<&HashSet<BlockId>>,
    width: u16,
    theme: &Theme,
) -> ItemLayout {
    let block_id = BlockId::ModelTools;
    let is_expanded = expanded.is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if is_expanded { '▾' } else { '▸' };
    let mut body = vec![Line::styled(
        format!(
            "🧰 {chevron} {}",
            cookie_agent_client_state::rows::model_tools_summary(state, tools)
        ),
        theme.internal(),
    )];
    if is_expanded {
        // A request that deferred tools lists them apart, under headings.
        let text = if state.model_deferred_tools.is_empty() {
            tools.join(", ")
        } else {
            let (provided, searchable) =
                cookie_agent_client_state::rows::model_tool_groups(state, tools);
            format!(
                "Provided: {}\nSearchable: {}",
                provided.join(", "),
                searchable.join(", ")
            )
        };
        body.extend(bounded_safe_display_text(
            &text,
            theme.internal(),
            MAX_EXPANDED_BODY_LINES,
            MAX_EXPANDED_BODY_BYTES,
        ));
    }
    collapsible_event_block(block_id, body, width, theme)
}

/// A leveled diagnostic row. Its first line is the title, followed by the
/// repeat count when the same row happened several times; any
/// further lines (a provider's response body, say) fold behind the title.
/// Warnings and errors lead the title with their marker instead of a badge
/// row, like the system prompt row.
pub(super) fn event_row_layout(
    block_id: BlockId,
    level: crate::state::EventLevel,
    text: &str,
    repeat: u32,
    expanded: Option<&HashSet<BlockId>>,
    width: u16,
    theme: &Theme,
) -> ItemLayout {
    let (role, marker) = match level {
        crate::state::EventLevel::Debug => (Role::Debug, None),
        crate::state::EventLevel::Info => (Role::Internal, None),
        // Both are emoji-presentation by default: two cells everywhere,
        // with no VS16 for the frame pass to strip.
        crate::state::EventLevel::Warning => (Role::Warning, Some("\u{1F6A8}")),
        crate::state::EventLevel::Error => (Role::Error, Some("\u{274C}")),
    };
    let block = |body: Vec<Line<'static>>| {
        if marker.is_some() {
            headerless_role_block(role, body, width, theme)
        } else {
            role_block(role, body, width, theme)
        }
    };
    let mut lines = text.lines();
    let mut title = lines.next().unwrap_or_default().to_owned();
    if repeat > 1 {
        title.push_str(&format!(" · repeated {repeat}×"));
    }
    let details = lines.map(|line| Line::from(line.to_owned()));
    let marker = marker
        .map(|marker| format!("{marker} "))
        .unwrap_or_default();
    if text.lines().nth(1).is_none() {
        return ItemLayout {
            lines: block(vec![Line::from(format!("{marker}{title}"))]),
            regions: Vec::new(),
            user_seq: None,
        };
    }
    let is_expanded = expanded.is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if is_expanded { '▾' } else { '▸' };
    let header = Line::from(format!("{marker}{chevron} {title}"));
    let header_lines = block(vec![header.clone()]).len();
    let mut body = vec![header];
    if is_expanded {
        body.extend(details);
    }
    let lines = block(body);
    ItemLayout {
        regions: vec![BlockRegion {
            id: block_id,
            start_line: 0,
            end_line: lines.len(),
            header_lines: Some(header_lines),
            header_gutter: None,
        }],
        lines,
        user_seq: None,
    }
}

/// A compaction in progress: an animated header and, when expanded, the
/// tail of the summary streamed so far or the native call being made.
pub(super) fn compacting_layout(
    seq: u64,
    source: &crate::state::CompactingSource,
    context: &TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = BlockId::Compacting(seq);
    let is_expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if is_expanded { '▾' } else { '▸' };
    let dots = ".".repeat(usize::from(context.clock_bucket));
    let mut body = vec![Line::styled(
        format!("🧹 {chevron} compacting context{dots}"),
        context.theme.internal(),
    )];
    let text = match source {
        crate::state::CompactingSource::Summary { text, .. } => text,
        crate::state::CompactingSource::Agent { .. } => {
            if is_expanded {
                body.push(Line::styled(
                    "the agent is compacting its conversation",
                    context.theme.muted(),
                ));
            }
            return collapsible_event_block(block_id, body, context.width, context.theme);
        }
        crate::state::CompactingSource::Native { model } => {
            if is_expanded {
                body.extend(bounded_safe_display_text(
                    &format!("calling native compaction endpoint ({model})"),
                    context.theme.muted(),
                    MAX_EXPANDED_BODY_LINES,
                    MAX_EXPANDED_BODY_BYTES,
                ));
            }
            return collapsible_event_block(block_id, body, context.width, context.theme);
        }
    };
    if is_expanded {
        if text.is_empty() {
            body.push(Line::styled(
                "waiting for the summary",
                context.theme.muted(),
            ));
        } else {
            // Streaming output reads at its newest end, so keep the tail.
            let lines = text.split('\n').collect::<Vec<_>>();
            let mut start = lines.len();
            let mut bytes = 0;
            while start > 0 && lines.len() - start < MAX_EXPANDED_BODY_LINES {
                bytes += lines[start - 1].len();
                if bytes > MAX_EXPANDED_BODY_BYTES && start < lines.len() {
                    break;
                }
                start -= 1;
            }
            if start > 0 {
                body.push(Line::styled(
                    format!("… {start} earlier lines"),
                    context.theme.muted(),
                ));
            }
            body.extend(bounded_safe_display_lines(
                lines[start..].iter().copied(),
                context.theme.internal(),
                MAX_EXPANDED_BODY_LINES,
                MAX_EXPANDED_BODY_BYTES,
            ));
        }
    }
    collapsible_event_block(block_id, body, context.width, context.theme)
}

pub(super) fn compaction_layout(
    seq: u64,
    commit: &cookie_agent_protocol::ContextCheckpointCommit,
    context: &TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = BlockId::Compaction(seq);
    let is_expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if is_expanded { '▾' } else { '▸' };
    let kind = match &commit.checkpoint {
        cookie_agent_protocol::ContextCheckpoint::InternalSummary { .. } => "internal summary",
        cookie_agent_protocol::ContextCheckpoint::NativeWindow { .. } => "native window",
    };
    let mut body = vec![Line::styled(
        format!(
            "🧹 {chevron} context compacted ({kind}, {}→{} tokens)",
            commit.budgets.input_tokens_before, commit.budgets.input_tokens_after
        ),
        context.theme.internal(),
    )];
    if is_expanded {
        match &commit.checkpoint {
            cookie_agent_protocol::ContextCheckpoint::InternalSummary { checkpoint, .. } => {
                body.extend(bounded_safe_display_text(
                    checkpoint.summary(),
                    context.theme.internal(),
                    MAX_EXPANDED_BODY_LINES,
                    MAX_EXPANDED_BODY_BYTES,
                ));
            }
            cookie_agent_protocol::ContextCheckpoint::NativeWindow { window } => {
                let scope = window.scope();
                let details = [
                    format!("adapter: {}", window.adapter_id()),
                    format!("model: {}/{}", scope.provider_id, scope.model_id),
                    format!("resource: {}", scope.resource_id),
                    format!("selection fingerprint: {}", window.selection_fingerprint()),
                ];
                body.extend(bounded_safe_display_lines(
                    details.iter().map(String::as_str),
                    context.theme.internal(),
                    MAX_EXPANDED_BODY_LINES,
                    MAX_EXPANDED_BODY_BYTES,
                ));
            }
        }
        body.push(Line::styled(
            format!(
                "retained range: {}..{}",
                commit.boundaries.source_from_seq, commit.boundaries.input_through_seq
            ),
            context.theme.muted(),
        ));
    }
    collapsible_event_block(block_id, body, context.width, context.theme)
}

/// A compaction of the ACP agent's own, as the session's own compaction
/// row shows: its summary when opened.
pub(super) fn agent_compaction_layout(
    seq: u64,
    summary: Option<&str>,
    context: &TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = BlockId::Compaction(seq);
    let is_expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if is_expanded { '▾' } else { '▸' };
    let mut body = vec![Line::styled(
        format!("🧹 {chevron} {AGENT_COMPACTION_HEADER}"),
        context.theme.internal(),
    )];
    if is_expanded {
        match summary {
            Some(summary) => body.extend(bounded_safe_display_text(
                summary,
                context.theme.internal(),
                MAX_EXPANDED_BODY_LINES,
                MAX_EXPANDED_BODY_BYTES,
            )),
            None => body.push(Line::styled(
                "the agent kept no summary",
                context.theme.muted(),
            )),
        }
    }
    collapsible_event_block(block_id, body, context.width, context.theme)
}

/// A message between the ACP agent's sessions, as a producer message row
/// shows: which way it went and the other session, its text when opened.
pub(super) fn agent_session_message_layout(
    seq: u64,
    message: &cookie_agent_protocol::AcpSessionMessage,
    context: &TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = BlockId::AgentSessionMessage(seq);
    let expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if expanded { '▾' } else { '▸' };
    let header = if context.width < 8 {
        format!("[M] {chevron} …")
    } else {
        format!("◇ {chevron} {}", agent_session_message_summary(message))
    };
    let mut lines = vec![Line::styled(
        crate::ui::app::truncate_with_ellipsis(&header, usize::from(context.width)),
        context.theme.internal(),
    )];
    if expanded {
        for line in bounded_safe_display_text(
            &message.text,
            context.theme.internal(),
            MAX_EXPANDED_BODY_LINES,
            MAX_EXPANDED_BODY_BYTES,
        ) {
            lines.extend(repeated_prefixed_wrapped_line(
                vec![Span::styled("· ", context.theme.internal())],
                line,
                context.width,
            ));
        }
    }
    ItemLayout {
        regions: vec![BlockRegion {
            id: block_id,
            start_line: 0,
            end_line: lines.len(),
            header_lines: Some(1),
            header_gutter: None,
        }],
        lines,
        user_seq: None,
    }
}

pub(super) fn plugin_message_layout(
    seq: u64,
    role: cookie_agent_protocol::ExtensionMessageRole,
    input: &str,
    context: &TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = BlockId::PluginMessage(seq);
    let is_expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if is_expanded { '▾' } else { '▸' };
    let role = match role {
        cookie_agent_protocol::ExtensionMessageRole::System => "system",
        cookie_agent_protocol::ExtensionMessageRole::User => "user",
        cookie_agent_protocol::ExtensionMessageRole::Assistant => "assistant",
        cookie_agent_protocol::ExtensionMessageRole::Tool => "tool",
    };
    let mut body = vec![Line::styled(
        format!(
            "🧩 {chevron} plugin message ({role}, {} lines)",
            display_line_count(input)
        ),
        context.theme.internal(),
    )];
    if is_expanded {
        body.extend(bounded_safe_display_text(
            input,
            context.theme.internal(),
            MAX_EXPANDED_BODY_LINES,
            MAX_EXPANDED_BODY_BYTES,
        ));
    }
    collapsible_event_block(block_id, body, context.width, context.theme)
}

pub(super) fn agent_md_layout(
    seq: u64,
    entries: &[cookie_agent_protocol::AgentMdEntry],
    context: &TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = BlockId::AgentMd(seq);
    let is_expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if is_expanded { '▾' } else { '▸' };
    let line_count: usize = entries
        .iter()
        .map(|entry| display_line_count(&entry.content))
        .sum();
    let files = match entries {
        [entry] => entry.source.as_str().to_owned(),
        _ => format!("{} files", entries.len()),
    };
    let mut body = vec![Line::styled(
        format!("📘 {chevron} AGENTS.md · {files} ({line_count} lines)"),
        context.theme.internal(),
    )];
    if is_expanded {
        for entry in entries {
            if entries.len() > 1 {
                body.push(Line::styled(
                    format!("── {}", entry.source.as_str()),
                    context.theme.muted(),
                ));
            }
            body.extend(bounded_safe_display_text(
                &entry.content,
                context.theme.internal(),
                MAX_SYSTEM_PROMPT_BODY_LINES,
                MAX_SYSTEM_PROMPT_BODY_BYTES,
            ));
        }
    }
    collapsible_event_block(block_id, body, context.width, context.theme)
}

pub(super) fn skill_loaded_layout(
    seq: u64,
    name: &str,
    source_path: &str,
    args: &str,
    rendered_body: &str,
    context: &TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = BlockId::SkillLoaded(seq);
    let is_expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if is_expanded { '▾' } else { '▸' };
    let mut body = vec![Line::styled(
        format!(
            "📚 {chevron} skill loaded · {} ({} lines)",
            safe_display_text(name),
            display_line_count(rendered_body)
        ),
        context.theme.internal(),
    )];
    if is_expanded {
        body.push(Line::styled(
            format!("source: {}", safe_display_text(source_path)),
            context.theme.muted(),
        ));
        if !args.trim().is_empty() {
            body.extend(bounded_safe_display_text(
                &format!("args: {args}"),
                context.theme.muted(),
                MAX_EXPANDED_BODY_LINES,
                MAX_EXPANDED_BODY_BYTES,
            ));
        }
        body.extend(bounded_safe_display_text(
            rendered_body,
            context.theme.internal(),
            MAX_SYSTEM_PROMPT_BODY_LINES,
            MAX_SYSTEM_PROMPT_BODY_BYTES,
        ));
    }
    collapsible_event_block(block_id, body, context.width, context.theme)
}

/// A model's memory change: one line, `🧠 Saved project://web/build`,
/// whose click opens the Memory panel at the memory (memory spec section
/// 16.9.10).
pub(super) fn memory_change_layout(
    seq: u64,
    change: cookie_agent_protocol::MemoryChangeKind,
    url: &str,
    context: &TranscriptRenderContext<'_>,
) -> ItemLayout {
    let line = Line::from(vec![
        Span::styled(format!("🧠 {} ", change.verb()), context.theme.internal()),
        Span::styled(safe_display_text(url), context.theme.link()),
    ]);
    collapsible_event_block(
        BlockId::MemoryChange(seq),
        vec![line],
        context.width,
        context.theme,
    )
}

pub(super) fn collapsible_event_block(
    block_id: BlockId,
    body: Vec<Line<'static>>,
    width: u16,
    theme: &Theme,
) -> ItemLayout {
    let header_lines = body.first().map_or(0, |header| {
        role_block(Role::Internal, vec![header.clone()], width, theme).len()
    });
    let lines = role_block(Role::Internal, body, width, theme);
    ItemLayout {
        regions: vec![BlockRegion {
            id: block_id,
            start_line: 0,
            end_line: lines.len(),
            header_lines: Some(header_lines),
            header_gutter: None,
        }],
        lines,
        user_seq: None,
    }
}

pub(super) fn media_file_layout(
    turn_seq: u64,
    content_index: u32,
    file: &cookie_agent_protocol::PersistedFilePart,
    context: &TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = BlockId::MediaFile {
        turn_seq,
        content_index,
    };
    let is_expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let chevron = if is_expanded { '▾' } else { '▸' };
    let filename = safe_display_text(file.filename.as_deref().unwrap_or("unnamed"));
    let mut body = vec![Line::styled(
        format!("📎 {chevron} {} · {filename}", file.media_type),
        context.theme.internal(),
    )];
    if is_expanded {
        let mut details = vec![
            format!("media type: {}", file.media_type),
            format!("filename: {filename}"),
        ];
        match &file.source {
            cookie_agent_protocol::PersistedFileSource::Artifact {
                byte_length,
                sha256,
                reference,
            } => details.extend([
                format!("byte size: {byte_length}"),
                format!("sha256: {sha256}"),
                format!("uri: {}", reference.uri),
            ]),
            cookie_agent_protocol::PersistedFileSource::Url { url } => {
                details.push(format!("uri: {url}"));
            }
            cookie_agent_protocol::PersistedFileSource::ProviderReference { provider_id, id } => {
                details.extend([
                    format!("provider: {provider_id}"),
                    format!("reference: {id}"),
                ]);
            }
        }
        body.extend(bounded_safe_display_lines(
            details.iter().map(String::as_str),
            context.theme.internal(),
            MAX_EXPANDED_BODY_LINES,
            MAX_EXPANDED_BODY_BYTES,
        ));
    }
    let header_lines = body.first().map_or(0, |header| {
        assistant_body_line(header.clone(), context.width, context.theme).len()
    });
    let lines = body
        .into_iter()
        .flat_map(|line| assistant_body_line(line, context.width, context.theme))
        .collect::<Vec<_>>();
    ItemLayout {
        regions: vec![BlockRegion {
            id: block_id,
            start_line: 0,
            end_line: lines.len(),
            header_lines: Some(header_lines),
            header_gutter: None,
        }],
        lines,
        user_seq: None,
    }
}

/// Warm, actionable empty states: what the pane says before there is
/// anything to show. The no-session variant points at session commands; the
/// fresh-session variant invites the first message. Both stay muted so the
/// guidance never competes with real content, and both wrap to the pane.
pub(super) fn empty_conversation_lines(
    has_session: bool,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let (headline, hint) = if has_session {
        (
            "🍪 Fresh session, warm out of the oven.",
            "Type a message below to start · `/` or `ctrl+p` lists commands",
        )
    } else {
        (
            "No session selected.",
            "`/sessions` chooses one · `/new` starts one · `ctrl+p` lists commands",
        )
    };
    let wrap = |text: &str, style: Style| {
        wrapped_line(
            Line::from(Span::styled(text.to_owned(), style)),
            width.max(1),
        )
    };
    let mut lines = wrap(headline, theme.muted());
    lines.extend(wrap(hint, theme.internal()));
    lines
}

//! Goal, producer, compaction, plugin, and media event layouts.

use super::*;

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

pub(in crate::ui) fn producer_summary(
    owner: &ProducerOwner,
    mode: ProducerDeliveryMode,
    description: Option<&str>,
) -> String {
    if let Some(description) = description.filter(|text| !text.trim().is_empty()) {
        return description.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    format!(
        "{} · {}",
        producer_owner_label(owner),
        producer_mode_label(mode)
    )
}

pub(super) fn producer_owner_label(owner: &ProducerOwner) -> String {
    match owner {
        ProducerOwner::Plugin { plugin } => format!("plugin {plugin}"),
        ProducerOwner::Delegation { invocation_id } => format!("delegation {invocation_id}"),
        ProducerOwner::Goal { .. } => "goal controller".to_owned(),
        ProducerOwner::GoalControl { .. } => "goal control".to_owned(),
        ProducerOwner::Agent { session_id } => format!("agent {session_id}"),
    }
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
    let owner = match owner {
        ProducerOwner::Plugin { plugin } => format!("plugin {plugin}"),
        ProducerOwner::Delegation { invocation_id } => format!("delegation {invocation_id}"),
        ProducerOwner::Goal { .. } => "goal controller".to_owned(),
        ProducerOwner::GoalControl { .. } => "goal control".to_owned(),
        ProducerOwner::Agent { session_id } => format!("agent {session_id}"),
    };
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

pub(super) fn producer_mode_label(mode: ProducerDeliveryMode) -> &'static str {
    match mode {
        ProducerDeliveryMode::Steer => "steer",
        ProducerDeliveryMode::Queue => "queue",
    }
}

pub(super) fn goal_status_label(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Completed => "completed",
        GoalStatus::Cancelled => "cancelled",
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
    let line_count = display_line_count(&snapshot.composed_prompt);
    let chevron = if is_expanded { '▾' } else { '▸' };
    let next_agent = draft_agent
        .filter(|draft_agent| *draft_agent != &snapshot.agent)
        .map(|draft_agent| format!(" · next: {draft_agent}"))
        .unwrap_or_default();
    let mut body = vec![Line::styled(
        format!(
            "📜 {chevron} system prompt · {} (last run){next_agent} ({line_count} lines)",
            snapshot.agent
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
            cookie_agent_protocol::ContextCheckpoint::InternalSummary { checkpoint } => {
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

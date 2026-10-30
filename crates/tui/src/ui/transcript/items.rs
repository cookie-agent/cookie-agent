//! Per-transcript-item layout, block identity, and interaction probes.

use cookie_agent_client_state::transcript_mode::{ChildFold, ItemFold};

use super::*;

pub(super) fn item_layout_key(
    state: &SessionState,
    item: &TranscriptItem,
    expanded: Option<&HashSet<BlockId>>,
    clock_bucket: u8,
    model_names: &ModelDisplayNames,
    transcript: TranscriptMode,
    fold: u64,
) -> ItemLayoutKey {
    ItemLayoutKey {
        id: item.id(),
        version: item.version(),
        fold,
        interaction: item_interaction(item, expanded),
        clock: if item_is_live(state, item, transcript) {
            clock_bucket
        } else {
            0
        },
        model_names: item_model_names_revision(item, model_names),
    }
}

// Each argument is an independent input of the cached layout, as for
// `ensure_cached_transcript_layout_in`.
#[allow(clippy::too_many_arguments)]
pub(super) fn item_layout_key_matches(
    key: &ItemLayoutKey,
    state: &SessionState,
    item: &TranscriptItem,
    expanded: Option<&HashSet<BlockId>>,
    clock_bucket: u8,
    model_names: &ModelDisplayNames,
    transcript: TranscriptMode,
    fold: u64,
) -> bool {
    key.id == item.id()
        && key.version == item.version()
        && key.fold == fold
        && key.model_names == item_model_names_revision(item, model_names)
        && key.clock
            == if item_is_live(state, item, transcript) {
                clock_bucket
            } else {
                0
            }
        && item_interaction_matches(&key.interaction, item, expanded)
}

/// What an item's layout reads of the reduced mode's folds: where it stands
/// in one, whether the fold is open, and on a fold's first block what its
/// work row says. Zero outside the folds.
pub(super) fn item_fold_key(
    folds: &FoldPlan,
    work_rows: &WorkRows,
    item: &TranscriptItem,
    expanded: Option<&HashSet<BlockId>>,
) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let Some(fold) = folds.get(item.id()) else {
        return 0;
    };
    let mut hasher = std::hash::DefaultHasher::new();
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
        expanded
            .is_some_and(|blocks| blocks.contains(&BlockId::work(key)))
            .hash(&mut hasher);
        if starts && let Some((row, fresh)) = work_rows.get(&key) {
            row.summary.label().hash(&mut hasher);
            row.summary.tool_calls.hash(&mut hasher);
            row.summary.failed.hash(&mut hasher);
            row.lines.hash(&mut hasher);
            fresh.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Only assistant items render model labels (their headers and "now using"
/// rows), so only they relayout when display names change; their cached
/// prose and thinking parts are reused.
fn item_model_names_revision(item: &TranscriptItem, model_names: &ModelDisplayNames) -> u64 {
    if matches!(item, TranscriptItem::Assistant { .. }) {
        model_names.revision()
    } else {
        0
    }
}

#[derive(Clone, Copy)]
pub(super) enum Role {
    User,
    Action,
    Goal,
    /// An ACP agent's plan.
    Plan,
    /// An ACP agent's notice.
    Notice,
    ToolRunning,
    ToolSuccess,
    ToolFailure,
    Debug,
    Warning,
    Error,
    Internal,
}

pub(super) fn for_each_item_block_id(
    item: &TranscriptItem,
    mut visit: impl FnMut(BlockId) -> bool,
) -> bool {
    match item {
        TranscriptItem::Assistant { children, .. } => {
            for child in children {
                let id = match child {
                    AssistantChild::Thinking { id, .. } => Some(BlockId::Thinking(*id)),
                    AssistantChild::Tool { call_id } => Some(BlockId::Tool(*call_id)),
                    AssistantChild::MediaFile {
                        turn_seq,
                        content_index,
                        ..
                    } => Some(BlockId::MediaFile {
                        turn_seq: *turn_seq,
                        content_index: *content_index,
                    }),
                    AssistantChild::Text { .. }
                    | AssistantChild::Attribution { .. }
                    | AssistantChild::CommittedTool { .. }
                    | AssistantChild::Notice { .. } => None,
                };
                let Some(id) = id else {
                    continue;
                };
                if !visit(id) {
                    return false;
                }
                if let BlockId::Tool(call_id) = id {
                    for section in [
                        ToolOutputSection::Detail,
                        ToolOutputSection::Stdout,
                        ToolOutputSection::Stderr,
                    ] {
                        if !visit(BlockId::ToolOutput { call_id, section }) {
                            return false;
                        }
                    }
                }
            }
            true
        }
        TranscriptItem::Compacting { seq, .. } => visit(BlockId::Compacting(*seq)),
        TranscriptItem::Compaction { seq, .. } | TranscriptItem::AgentCompaction { seq, .. } => {
            visit(BlockId::Compaction(*seq))
        }
        TranscriptItem::AgentSessionMessage { seq, .. } => {
            visit(BlockId::AgentSessionMessage(*seq))
        }
        TranscriptItem::AgentBackgroundEnded { seq, .. } => {
            visit(BlockId::AgentBackgroundEnded(*seq))
        }
        TranscriptItem::PluginMessage { seq, .. } => visit(BlockId::PluginMessage(*seq)),
        TranscriptItem::AgentMd { seq, .. } => visit(BlockId::AgentMd(*seq)),
        TranscriptItem::SkillLoaded { seq, .. } => visit(BlockId::SkillLoaded(*seq)),
        TranscriptItem::ModelInput { seq, .. } => visit(BlockId::ModelInput(*seq)),
        TranscriptItem::MemoryChange { seq, .. } => visit(BlockId::MemoryChange(*seq)),
        TranscriptItem::ProducerMessage { message_id, .. } => {
            visit(BlockId::ProducerMessage(*message_id))
        }
        TranscriptItem::Event { id, .. } => visit(BlockId::Event(*id)),
        TranscriptItem::User { .. }
        | TranscriptItem::Goal { .. }
        | TranscriptItem::Plan { .. }
        | TranscriptItem::AcpNotice { .. } => true,
    }
}

pub(super) fn item_interaction_matches(
    expected: &[(BlockId, bool)],
    item: &TranscriptItem,
    expanded: Option<&HashSet<BlockId>>,
) -> bool {
    let mut index = 0;
    let complete = for_each_item_block_id(item, |id| {
        let matches = expected.get(index).is_some_and(|entry| {
            *entry == (id, expanded.is_some_and(|blocks| blocks.contains(&id)))
        });
        index += 1;
        matches
    });
    complete && index == expected.len()
}

#[cfg(test)]
pub(super) fn item_block_ids(item: &TranscriptItem) -> Vec<BlockId> {
    let mut ids = Vec::new();
    for_each_item_block_id(item, |id| {
        ids.push(id);
        true
    });
    ids
}

pub(super) fn item_interaction(
    item: &TranscriptItem,
    expanded: Option<&HashSet<BlockId>>,
) -> Vec<(BlockId, bool)> {
    let mut interaction = Vec::new();
    for_each_item_block_id(item, |id| {
        interaction.push((id, expanded.is_some_and(|blocks| blocks.contains(&id))));
        true
    });
    interaction
}

/// Whether one transcript item owns live content — a still-streaming
/// thinking part, a running tool row, or in the reduced mode a work row
/// whose time ticks — and so re-renders on each animation clock bucket.
pub(super) fn item_is_live(
    state: &SessionState,
    item: &TranscriptItem,
    transcript: TranscriptMode,
) -> bool {
    match item {
        TranscriptItem::Assistant { id, .. }
            if transcript == TranscriptMode::Reduced && state.assistant_live(*id) =>
        {
            true
        }
        TranscriptItem::Assistant { id, children, .. } => {
            children.iter().any(|child| match child {
                AssistantChild::Thinking { id: part_id, .. } => {
                    state.is_open_thinking(*id, *part_id)
                }
                AssistantChild::Tool { call_id } => state
                    .tools
                    .get(call_id)
                    .is_some_and(|tool| tool.status == ToolStatus::Running),
                AssistantChild::Text { .. }
                | AssistantChild::Attribution { .. }
                | AssistantChild::CommittedTool { .. }
                | AssistantChild::MediaFile { .. }
                | AssistantChild::Notice { .. } => false,
            })
        }
        TranscriptItem::Compacting { .. } => true,
        TranscriptItem::User { .. }
        | TranscriptItem::Event { .. }
        | TranscriptItem::Compaction { .. }
        | TranscriptItem::PluginMessage { .. }
        | TranscriptItem::AgentMd { .. }
        | TranscriptItem::SkillLoaded { .. }
        | TranscriptItem::ModelInput { .. }
        | TranscriptItem::MemoryChange { .. }
        | TranscriptItem::Goal { .. }
        | TranscriptItem::Plan { .. }
        | TranscriptItem::AcpNotice { .. }
        | TranscriptItem::AgentCompaction { .. }
        | TranscriptItem::AgentSessionMessage { .. }
        | TranscriptItem::AgentBackgroundEnded { .. }
        | TranscriptItem::ProducerMessage { .. } => false,
    }
}

/// One item's layout over shared chunks, for the layout cache: an assistant
/// item reuses its unchanged parts and tool rows by reference.
pub(super) fn chunked_item_layout(
    state: &SessionState,
    item: &TranscriptItem,
    context: &mut TranscriptRenderContext<'_>,
) -> ChunkedItem {
    match item {
        TranscriptItem::Assistant {
            id,
            attribution,
            children,
            ..
        } => assistant_item_layout(state, *id, attribution, children, context),
        _ => {
            let mut layout = transcript_item_layout(state, item, context);
            // A background row an open fold shows is part of its work.
            if let Some(ItemFold::Row { key }) = context.folds.get(item.id())
                && !layout.lines.is_empty()
            {
                layout.regions.push(BlockRegion {
                    id: BlockId::work_span(*key),
                    start_line: 0,
                    end_line: layout.lines.len(),
                    header_lines: Some(0),
                    header_gutter: None,
                });
            }
            layout.into()
        }
    }
}

pub(super) fn transcript_item_layout(
    state: &SessionState,
    item: &TranscriptItem,
    context: &mut TranscriptRenderContext<'_>,
) -> ItemLayout {
    // A background row inside a folded run's work shows when it opens.
    if let Some(ItemFold::Row { key }) = context.folds.get(item.id())
        && !context
            .expanded
            .is_some_and(|blocks| blocks.contains(&BlockId::work(*key)))
    {
        return ItemLayout::default();
    }
    match item {
        TranscriptItem::User { markdown, seq, .. } => ItemLayout {
            lines: {
                let width = context.width;
                let theme = context.theme;
                let mut lines = role_block(Role::User, Vec::new(), width, theme);
                lines.extend(
                    crate::markdown::render_markdown_lines_width(
                        markdown,
                        theme,
                        context.highlighter,
                        markdown_body_width(width),
                    )
                    .into_iter()
                    .flat_map(|line| markdown_body_line(line, width, theme.user())),
                );
                lines
            },
            regions: Vec::new(),
            user_seq: Some(*seq),
        },
        TranscriptItem::Assistant {
            id,
            attribution,
            children,
            ..
        } => assistant_item_layout(state, *id, attribution, children, context).flatten(),
        TranscriptItem::Event {
            id,
            level,
            text,
            repeat,
            ..
        } => {
            // Level filtering is a pure view concern: the row stays in the
            // session projection and reappears when the threshold is lowered.
            if *level < context.minimum_event_level {
                return ItemLayout::default();
            }
            event_row_layout(
                BlockId::Event(*id),
                *level,
                text,
                *repeat,
                context.expanded,
                context.width,
                context.theme,
            )
        }
        TranscriptItem::Compacting { seq, source, .. } => compacting_layout(*seq, source, context),
        TranscriptItem::Compaction { seq, commit, .. } => compaction_layout(*seq, commit, context),
        TranscriptItem::AgentCompaction { seq, summary, .. } => {
            agent_compaction_layout(*seq, summary.as_deref(), context)
        }
        TranscriptItem::AgentSessionMessage { seq, message, .. } => {
            agent_session_message_layout(*seq, message, context)
        }
        TranscriptItem::AgentBackgroundEnded {
            seq,
            ended_at,
            ended,
            ..
        } => agent_background_ended_layout(*seq, *ended_at, ended, context),
        TranscriptItem::PluginMessage {
            seq, role, input, ..
        } => plugin_message_layout(*seq, *role, input, context),
        TranscriptItem::AgentMd {
            seq, entries, text, ..
        } => agent_md_layout(*seq, entries, text, context),
        TranscriptItem::SkillLoaded {
            seq,
            name,
            source_path,
            args,
            rendered_body,
            ..
        } => skill_loaded_layout(*seq, name, source_path, args, rendered_body, context),
        TranscriptItem::ModelInput {
            seq,
            kind,
            header,
            text,
            ..
        } => model_input_layout(*seq, *kind, header, text, context),
        TranscriptItem::MemoryChange {
            seq, change, url, ..
        } => memory_change_layout(*seq, *change, url, context),
        TranscriptItem::Plan { entries, .. } => ItemLayout {
            lines: plan_layout(entries, context.width, context.theme),
            regions: Vec::new(),
            user_seq: None,
        },
        TranscriptItem::AcpNotice { notice, .. } => ItemLayout {
            lines: acp_notice_layout(notice, context.width, context.theme),
            regions: Vec::new(),
            user_seq: None,
        },
        TranscriptItem::Goal {
            goal, activation, ..
        } => ItemLayout {
            lines: if *activation {
                goal_activation_layout(goal, context.width, context.theme)
            } else {
                goal_layout(goal, context.width, context.theme)
            },
            regions: Vec::new(),
            user_seq: None,
        },
        TranscriptItem::ProducerMessage {
            message_id,
            producer_owner,
            mode,
            body,
            summary,
            status,
            ..
        } => match status {
            // A claim means the running request already carries the message,
            // so it shows before the response streams in, not after the turn
            // commits. Claims leave the queue before the request starts, and
            // the reducer anchored the row at admission, so it lands above
            // the response and never shows twice.
            crate::state::ProducerMessageStatus::Pending
            | crate::state::ProducerMessageStatus::Admitted => ItemLayout::default(),
            crate::state::ProducerMessageStatus::Claimed
            | crate::state::ProducerMessageStatus::Consumed => producer_message_layout(
                *message_id,
                producer_owner,
                *mode,
                body,
                &producer_summary(producer_owner, *mode, summary.as_deref()),
                *status,
                context,
            ),
            crate::state::ProducerMessageStatus::Discarded
                if context.minimum_event_level == crate::state::EventLevel::Debug =>
            {
                discarded_producer_message_layout(
                    producer_owner,
                    *mode,
                    context.width,
                    context.theme,
                )
            }
            crate::state::ProducerMessageStatus::Discarded => ItemLayout::default(),
        },
    }
}

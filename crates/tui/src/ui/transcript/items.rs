//! Per-transcript-item layout, block identity, and interaction probes.

use super::*;

pub(super) fn item_layout_key(
    state: &SessionState,
    item: &TranscriptItem,
    expanded: Option<&HashSet<BlockId>>,
    clock_bucket: u8,
) -> ItemLayoutKey {
    ItemLayoutKey {
        id: item.id(),
        version: item.version(),
        interaction: item_interaction(item, expanded),
        clock: if item_is_live(state, item) {
            clock_bucket
        } else {
            0
        },
    }
}

pub(super) fn item_layout_key_matches(
    key: &ItemLayoutKey,
    state: &SessionState,
    item: &TranscriptItem,
    expanded: Option<&HashSet<BlockId>>,
    clock_bucket: u8,
) -> bool {
    key.id == item.id()
        && key.version == item.version()
        && key.clock
            == if item_is_live(state, item) {
                clock_bucket
            } else {
                0
            }
        && item_interaction_matches(&key.interaction, item, expanded)
}

#[derive(Clone, Copy)]
pub(super) enum Role {
    User,
    Action,
    Goal,
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
                    | AssistantChild::CommittedTool { .. } => None,
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
        TranscriptItem::Compaction { seq, .. } => visit(BlockId::Compaction(*seq)),
        TranscriptItem::PluginMessage { seq, .. } => visit(BlockId::PluginMessage(*seq)),
        TranscriptItem::ProducerMessage { message_id, .. } => {
            visit(BlockId::ProducerMessage(*message_id))
        }
        TranscriptItem::User { .. }
        | TranscriptItem::Event { .. }
        | TranscriptItem::Goal { .. } => true,
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
    item_block_ids(item)
        .into_iter()
        .map(|id| (id, expanded.is_some_and(|blocks| blocks.contains(&id))))
        .collect()
}

/// Whether one transcript item owns live content — a still-streaming
/// thinking part or a running tool row — and so re-renders on each
/// animation clock bucket.
pub(super) fn item_is_live(state: &SessionState, item: &TranscriptItem) -> bool {
    match item {
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
                | AssistantChild::MediaFile { .. } => false,
            })
        }
        TranscriptItem::User { .. }
        | TranscriptItem::Event { .. }
        | TranscriptItem::Compaction { .. }
        | TranscriptItem::PluginMessage { .. }
        | TranscriptItem::Goal { .. }
        | TranscriptItem::ProducerMessage { .. } => false,
    }
}

pub(super) fn transcript_item_layout(
    state: &SessionState,
    item: &TranscriptItem,
    context: &mut TranscriptRenderContext<'_>,
) -> ItemLayout {
    match item {
        TranscriptItem::User { text, seq, .. } => ItemLayout {
            lines: role_block(
                Role::User,
                text.lines()
                    .map(|line| Line::from(line.to_owned()))
                    .collect(),
                context.width,
                context.theme,
            ),
            regions: Vec::new(),
            user_seq: Some(*seq),
        },
        TranscriptItem::Assistant {
            id,
            attribution,
            children,
            ..
        } => assistant_item_layout(state, *id, attribution, children, context),
        TranscriptItem::Event { level, text, .. } => {
            // Level filtering is a pure view concern: the row stays in the
            // session projection and reappears when the threshold is lowered.
            if *level < context.minimum_event_level {
                return ItemLayout::default();
            }
            let badge_role = match level {
                crate::state::EventLevel::Debug => Role::Debug,
                crate::state::EventLevel::Info => Role::Internal,
                crate::state::EventLevel::Warning => Role::Warning,
                crate::state::EventLevel::Error => Role::Error,
            };
            ItemLayout {
                lines: role_block(
                    badge_role,
                    text.lines()
                        .map(|line| Line::from(line.to_owned()))
                        .collect(),
                    context.width,
                    context.theme,
                ),
                regions: Vec::new(),
                user_seq: None,
            }
        }
        TranscriptItem::Compaction { seq, commit, .. } => compaction_layout(*seq, commit, context),
        TranscriptItem::PluginMessage {
            seq, role, input, ..
        } => plugin_message_layout(*seq, *role, input, context),
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

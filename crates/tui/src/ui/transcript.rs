//! Transcript layout, collapse state, wrapping, scrolling, and hit testing.

use std::collections::{HashMap, HashSet};

use cookie_agent_protocol::SessionId;
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    markdown::Highlighter,
    state::{AssistantChild, SessionState, ToolStatus, TranscriptItem},
    theme::{Theme, ThemeKey},
};

use super::app::App;
use super::slash::BlockCommand;

/// Scrollbar geometry over the total rendered line height.
///
/// Thumb **height** is strictly a function of the total content height and
/// the viewport (track) height — `ceil(viewport² / content)`, clamped to
/// `[1, track]` — and never of the scroll offset, position, or follow state.
/// Thumb **top** is the only position-dependent value: offset 0 maps to the
/// first track row and the maximum valid top offset maps the thumb flush
/// against the last track row, so top/bottom are exact. One helper is shared
/// by render, hit testing, and drag math; no ratatui `ScrollbarState`
/// position/content-length folding is involved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ScrollbarGeometry {
    pub(super) track: Rect,
    pub(super) thumb: Rect,
    pub(super) content_height: usize,
    pub(super) viewport_height: usize,
    pub(super) max_offset: usize,
}

impl ScrollbarGeometry {
    fn resolve(track: Rect, content_height: usize) -> Option<Self> {
        if track.height == 0 || content_height <= usize::from(track.height) {
            return None;
        }
        let viewport_height = usize::from(track.height);
        let max_offset = content_height - viewport_height;
        Some(Self {
            track,
            thumb: Rect::default(),
            content_height,
            viewport_height,
            max_offset,
        })
    }

    /// Clamp any top offset into the valid range for this geometry.
    pub(super) fn clamp_offset(&self, offset: usize) -> usize {
        offset.min(self.max_offset)
    }

    /// Thumb height in rows: the exact visible fraction of the track,
    /// independent of scroll offset/position/following.
    pub(super) fn thumb_size(&self) -> usize {
        let track = usize::from(self.track.height);
        (self.viewport_height * track)
            .div_ceil(self.content_height)
            .clamp(1, track)
    }

    /// Thumb top row within the track for a top offset. Linear over the
    /// available travel `track − thumb_size`, so the thumb sits flush at the
    /// track top at offset 0 and flush at the track bottom at max offset,
    /// always at full height.
    pub(super) fn thumb_top(&self, offset: usize) -> usize {
        let travel = usize::from(self.track.height) - self.thumb_size();
        if travel == 0 || self.max_offset == 0 {
            return 0;
        }
        (self.clamp_offset(offset) * travel + self.max_offset / 2) / self.max_offset
    }

    fn with_thumb(mut self, offset: usize) -> Self {
        let top = self.thumb_top(offset);
        let size = self.thumb_size();
        self.thumb = Rect::new(
            self.track.x,
            self.track.y + u16::try_from(top).unwrap_or(u16::MAX),
            self.track.width,
            u16::try_from(size).unwrap_or(u16::MAX),
        );
        self
    }

    /// Offset for a thumb dragged so its grab anchor sits at `row`. Exact
    /// inverse of `thumb_top`, clamped to the valid range.
    pub(super) fn offset_for_thumb_anchor(&self, row: u16, grab: u16) -> usize {
        let travel = usize::from(self.track.height) - self.thumb_size();
        if travel == 0 {
            return 0;
        }
        let row = usize::from(row.saturating_sub(self.track.y).min(self.track.height - 1));
        let top = row.saturating_sub(usize::from(grab));
        (top * self.max_offset + travel / 2) / travel
    }

    /// Offset whose viewport is centered on the track position of `row`.
    pub(super) fn offset_for_track_row(&self, row: u16) -> usize {
        self.offset_for_thumb_anchor(row, (self.thumb_size() / 2) as u16)
    }
}

#[derive(Debug)]
pub(super) struct ConversationScroll {
    pub(super) offset: usize,
    pub(super) following: bool,
}

impl Default for ConversationScroll {
    fn default() -> Self {
        Self {
            offset: 0,
            following: true,
        }
    }
}

impl ConversationScroll {
    pub(super) fn max_offset(total_lines: usize, viewport_height: u16) -> usize {
        total_lines.saturating_sub(usize::from(viewport_height))
    }

    pub(super) fn clamp(&mut self, total_lines: usize, viewport_height: u16) {
        let max_offset = Self::max_offset(total_lines, viewport_height);
        self.offset = if self.following {
            max_offset
        } else {
            self.offset.min(max_offset)
        };
        // A non-following view resting exactly on the last valid top offset is
        // the live bottom; wheel/track input re-engages following from there.
        if self.offset == max_offset {
            self.following = true;
        }
    }

    pub(super) fn up(&mut self, lines: usize) {
        self.following = false;
        self.offset = self.offset.saturating_sub(lines);
    }
    pub(super) fn down(&mut self, lines: usize) {
        self.following = false;
        self.offset = self.offset.saturating_add(lines);
    }
    pub(super) fn top(&mut self) {
        self.following = false;
        self.offset = 0;
    }
    pub(super) fn bottom(&mut self) {
        self.following = true;
    }

    /// Absolute top offset from a scrollbar thumb/track gesture.
    pub(super) fn scroll_to(&mut self, offset: usize) {
        self.following = false;
        self.offset = offset;
    }

    fn reveal(&mut self, region: BlockRegion, viewport_height: u16) {
        let height = usize::from(viewport_height.max(1));
        self.following = false;
        if region.start_line < self.offset {
            self.offset = region.start_line;
        } else if region.end_line > self.offset.saturating_add(height) {
            self.offset = region.end_line.saturating_sub(height);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum BlockId {
    Thinking(u64),
    Tool(cookie_agent_protocol::ToolCallId),
    CommittedTool(u32),
}

/// A contiguous logical-line range owned by one collapsible transcript block.
/// Stage 4 mouse handling can translate a y coordinate to a logical line by
/// adding the conversation scroll offset, then find the containing region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BlockRegion {
    pub(super) id: BlockId,
    pub(super) start_line: usize,
    pub(super) end_line: usize,
}

/// Width-resolved transcript output and its stage-4 block hit map.
#[derive(Clone, Default)]
pub(super) struct TranscriptLayout {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) regions: Vec<BlockRegion>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LayoutCacheKey {
    pub(super) session_id: SessionId,
    pub(super) session_generation: u64,
    pub(super) width: u16,
    pub(super) theme: ThemeKey,
    pub(super) minimum_event_level: crate::state::EventLevel,
}

#[derive(Default)]
pub(super) struct LayoutCache {
    pub(super) key: Option<LayoutCacheKey>,
    pub(super) layout: TranscriptLayout,
    items: Vec<CachedItemLayout>,
    assistant_parts: HashMap<u64, CachedAssistantPartLayout>,
    #[cfg(test)]
    pub(super) item_layout_passes: u64,
    pub(super) assistant_part_layout_passes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ItemLayoutKey {
    id: u64,
    version: u64,
    interaction: Vec<(BlockId, bool, bool)>,
}

#[derive(Clone, Default)]
struct ItemLayout {
    lines: Vec<Line<'static>>,
    regions: Vec<BlockRegion>,
}

#[derive(Clone)]
struct CachedItemLayout {
    key: ItemLayoutKey,
    layout: ItemLayout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AssistantPartLayoutKey {
    version: u64,
    expanded: bool,
    selected: bool,
    streaming: bool,
}

#[derive(Clone)]
struct CachedAssistantPartLayout {
    key: AssistantPartLayoutKey,
    layout: ItemLayout,
}

struct TranscriptRenderContext<'a> {
    expanded: Option<&'a HashSet<BlockId>>,
    selected_block: Option<BlockId>,
    width: u16,
    theme: &'a Theme,
    highlighter: &'a dyn Highlighter,
    minimum_event_level: crate::state::EventLevel,
    assistant_part_cache: &'a mut HashMap<u64, CachedAssistantPartLayout>,
    assistant_part_layout_passes: &'a mut u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BlockHit {
    pub(super) rect: Rect,
    pub(super) id: BlockId,
}

// Layout cache validity depends on each independent render input; grouping them
// would obscure invalidation semantics without reducing call-site complexity.
#[allow(clippy::too_many_arguments)]
pub(super) fn ensure_cached_transcript_layout(
    cache: &mut LayoutCache,
    session_id: SessionId,
    state: &SessionState,
    expanded: Option<&HashSet<BlockId>>,
    selected_block: Option<BlockId>,
    width: u16,
    theme: &Theme,
    highlighter: &dyn Highlighter,
    minimum_event_level: crate::state::EventLevel,
) -> bool {
    let key = LayoutCacheKey {
        session_id,
        session_generation: state.generation,
        width,
        theme: theme.key(),
        minimum_event_level,
    };
    if cache.key != Some(key) {
        cache.key = Some(key);
        cache.items.clear();
        cache.assistant_parts.clear();
    }
    let mut all_cached = cache.items.len() >= state.transcript.len();
    let mut assembled = TranscriptLayout::default();
    for (index, item) in state.transcript.iter().enumerate() {
        let item_key = ItemLayoutKey {
            id: item.id(),
            version: item.version(),
            interaction: item_interaction(item, expanded, selected_block),
        };
        let layout = if cache
            .items
            .get(index)
            .is_some_and(|cached| cached.key == item_key)
        {
            cache.items[index].layout.clone()
        } else {
            all_cached = false;
            let layout = transcript_item_layout(
                state,
                item,
                &mut TranscriptRenderContext {
                    expanded,
                    selected_block,
                    width,
                    theme,
                    highlighter,
                    minimum_event_level,
                    assistant_part_cache: &mut cache.assistant_parts,
                    assistant_part_layout_passes: &mut cache.assistant_part_layout_passes,
                },
            );
            let cached = CachedItemLayout {
                key: item_key,
                layout: layout.clone(),
            };
            if index < cache.items.len() {
                cache.items[index] = cached;
            } else {
                cache.items.push(cached);
            }
            #[cfg(test)]
            {
                cache.item_layout_passes = cache.item_layout_passes.wrapping_add(1);
            }
            layout
        };
        let start_line = assembled.lines.len();
        assembled.lines.extend(layout.lines);
        for region in layout.regions {
            assembled.regions.push(BlockRegion {
                id: region.id,
                start_line: start_line + region.start_line,
                end_line: start_line + region.end_line,
            });
        }
    }
    cache.items.truncate(state.transcript.len());
    cache.layout = assembled;
    all_cached
}

impl App {
    pub(super) fn render_conversation(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        // The rightmost inner column is reserved for the scrollbar whenever
        // the content can overflow; content layout and block hit regions never
        // extend into it, so the track can be grabbed without hitting blocks.
        let scrollable = self.selected.is_some_and(|session_id| {
            self.store
                .sessions
                .get(&session_id)
                .is_some_and(|state| !state.transcript.is_empty())
        }) || !self.transient_notices.is_empty()
            || (self.tui_config.minimum_event_level <= crate::state::EventLevel::Warning
                && self
                    .selected
                    .is_some_and(|selected| !self.descendant_warnings(selected).is_empty()));
        let width = area
            .width
            .saturating_sub(2)
            .saturating_sub(u16::from(scrollable));
        let empty_layout = TranscriptLayout {
            lines: vec![Line::from("Select or create a session")],
            regions: Vec::new(),
        };
        let layout = if let Some((session_id, state)) = self.selected.and_then(|session_id| {
            self.store
                .sessions
                .get(&session_id)
                .map(|state| (session_id, state))
        }) {
            ensure_cached_transcript_layout(
                &mut self.layout_cache,
                session_id,
                state,
                self.expanded_blocks.get(&session_id),
                self.selected_block,
                width,
                &self.theme,
                self.highlighter.as_ref(),
                self.tui_config.minimum_event_level,
            );
            &self.layout_cache.layout
        } else {
            &empty_layout
        };
        let mut notice_lines = Vec::new();
        for notice in &self.transient_notices {
            notice_lines.extend(role_block(
                Role::Internal,
                vec![Line::from(format!("NOTICE: {notice}"))],
                width,
                &self.theme,
            ));
        }
        // Descendant warnings aggregate into the viewed session's pane with
        // their owning session's attribution. The viewed session's own
        // warnings already render locally inside the transcript; only strict
        // descendants are appended here, so a warning never appears twice in
        // one view.
        if self.tui_config.minimum_event_level <= crate::state::EventLevel::Warning {
            let descendant_warnings = self
                .selected
                .map(|selected| self.descendant_warnings(selected))
                .unwrap_or_default();
            for warning in &descendant_warnings {
                notice_lines.extend(role_block(
                    Role::Warning,
                    vec![Line::from(warning.clone())],
                    width,
                    &self.theme,
                ));
            }
        }
        let viewport = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width
                .saturating_sub(2)
                .saturating_sub(u16::from(scrollable)),
            area.height.saturating_sub(2),
        );
        let scrollbar_track = scrollable.then(|| {
            Rect::new(
                viewport.x.saturating_add(viewport.width),
                viewport.y,
                area.width
                    .saturating_sub(2)
                    .saturating_sub(viewport.width)
                    .min(1),
                viewport.height,
            )
        });
        let content_height = layout.lines.len() + notice_lines.len();
        if self
            .selected_block
            .is_some_and(|selected| !layout.regions.iter().any(|region| region.id == selected))
        {
            self.selected_block = None;
            self.reveal_selected_block = false;
        }
        if self.reveal_selected_block {
            if let Some(region) = self.selected_block.and_then(|selected| {
                layout
                    .regions
                    .iter()
                    .find(|region| region.id == selected)
                    .copied()
            }) {
                self.conversation_scroll.reveal(region, viewport.height);
            }
            self.reveal_selected_block = false;
        }
        self.conversation_scroll
            .clamp(content_height, viewport.height);
        self.hit_map.conversation = Some(viewport);
        self.hit_map.scrollbar = scrollbar_track.filter(|track| track.width > 0);
        self.hit_map.blocks = layout
            .regions
            .iter()
            .filter_map(|region| block_hit(*region, viewport, self.conversation_scroll.offset))
            .collect();
        let visible_lines = layout
            .lines
            .iter()
            .chain(notice_lines.iter())
            .skip(self.conversation_scroll.offset)
            .take(usize::from(viewport.height))
            .cloned()
            .collect::<Vec<_>>();
        let filter = self.tui_config.minimum_event_level.name();
        // Conversation and Message border titles carry no instructional
        // drag/hotkey prose.
        let title = format!("Conversation · events ≥ {filter}");
        frame.render_widget(
            Paragraph::new(Text::from(visible_lines))
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
        self.scrollbar_geometry = scrollbar_track.and_then(|track| {
            ScrollbarGeometry::resolve(track, content_height)
                .map(|geometry| geometry.with_thumb(self.conversation_scroll.offset))
        });
        if let Some(geometry) = self.scrollbar_geometry {
            render_scrollbar_track(frame, geometry, &self.theme);
        }
    }

    pub(super) fn toggle_block(&mut self, block_id: BlockId) {
        let Some(session_id) = self.selected else {
            return;
        };
        let expanded = self.expanded_blocks.entry(session_id).or_default();
        if !expanded.insert(block_id) {
            expanded.remove(&block_id);
        }
    }

    pub(super) fn run_block_command(&mut self, command: BlockCommand) {
        match command {
            BlockCommand::Next => self.select_block(true),
            BlockCommand::Previous => self.select_block(false),
            BlockCommand::Toggle => {
                if let Some(block_id) = self.selected_block {
                    self.toggle_block(block_id);
                    self.status = format!("Toggled {}", block_label(block_id));
                } else {
                    self.status = "no thinking or tool block selected".into();
                }
            }
            BlockCommand::Clear => {
                self.selected_block = None;
                self.reveal_selected_block = false;
                self.status = "transcript block selection cleared".into();
            }
        }
    }

    fn select_block(&mut self, forward: bool) {
        let Some(state) = self
            .selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
        else {
            self.selected_block = None;
            self.status = "no transcript blocks available".into();
            return;
        };
        let blocks = state
            .transcript
            .iter()
            .flat_map(item_block_ids)
            .collect::<Vec<_>>();
        if blocks.is_empty() {
            self.selected_block = None;
            self.status = "no thinking or tool blocks available".into();
            return;
        }
        let selected_index = self
            .selected_block
            .and_then(|selected| blocks.iter().position(|block| *block == selected));
        let next_index = match (selected_index, forward) {
            (Some(index), true) => (index + 1) % blocks.len(),
            (Some(index), false) => (index + blocks.len() - 1) % blocks.len(),
            (None, true) => 0,
            (None, false) => blocks.len() - 1,
        };
        let block_id = blocks[next_index];
        self.selected_block = Some(block_id);
        self.reveal_selected_block = true;
        self.status = format!(
            "Selected {} ({}/{}) · click or /block toggle to expand",
            block_label(block_id),
            next_index + 1,
            blocks.len()
        );
    }
}

/// Render the reserved scrollbar column: a subdued track with a distinct
/// thumb covering the exact visible fraction of the total rendered height.
fn render_scrollbar_track(frame: &mut ratatui::Frame, geometry: ScrollbarGeometry, theme: &Theme) {
    for row in 0..geometry.track.height {
        let y = geometry.track.y + row;
        if y >= geometry.track.y + geometry.track.height {
            break;
        }
        let cell = &mut frame.buffer_mut()[(geometry.track.x, y)];
        cell.set_symbol("│");
        cell.set_style(theme.muted());
    }
    for row in 0..geometry.thumb.height {
        let y = geometry.thumb.y + row;
        if y >= geometry.track.y + geometry.track.height {
            break;
        }
        let cell = &mut frame.buffer_mut()[(geometry.thumb.x, y)];
        cell.set_symbol("█");
        cell.set_style(theme.assistant());
    }
}

fn block_label(block_id: BlockId) -> &'static str {
    match block_id {
        BlockId::Thinking(_) => "thinking block",
        BlockId::Tool(_) | BlockId::CommittedTool(_) => "tool block",
    }
}

#[cfg(test)]
pub(super) fn transcript_layout(
    state: &SessionState,
    expanded: Option<&HashSet<BlockId>>,
    width: u16,
) -> TranscriptLayout {
    transcript_layout_with(
        state,
        expanded,
        width,
        &Theme::default(),
        &crate::markdown::SyntectHighlighter::default(),
    )
}

#[cfg(test)]
fn transcript_layout_with(
    state: &SessionState,
    expanded: Option<&HashSet<BlockId>>,
    width: u16,
    theme: &Theme,
    highlighter: &dyn Highlighter,
) -> TranscriptLayout {
    transcript_layout_with_level(
        state,
        expanded,
        width,
        theme,
        highlighter,
        crate::state::EventLevel::Debug,
    )
}

#[cfg(test)]
fn transcript_layout_with_level(
    state: &SessionState,
    expanded: Option<&HashSet<BlockId>>,
    width: u16,
    theme: &Theme,
    highlighter: &dyn Highlighter,
    minimum_event_level: crate::state::EventLevel,
) -> TranscriptLayout {
    let mut layout = TranscriptLayout::default();
    let mut assistant_parts = HashMap::new();
    let mut assistant_part_layout_passes = 0;
    for item in &state.transcript {
        let item_layout = transcript_item_layout(
            state,
            item,
            &mut TranscriptRenderContext {
                expanded,
                selected_block: None,
                width,
                theme,
                highlighter,
                minimum_event_level,
                assistant_part_cache: &mut assistant_parts,
                assistant_part_layout_passes: &mut assistant_part_layout_passes,
            },
        );
        let start_line = layout.lines.len();
        layout.lines.extend(item_layout.lines);
        for region in item_layout.regions {
            layout.regions.push(BlockRegion {
                id: region.id,
                start_line: start_line + region.start_line,
                end_line: start_line + region.end_line,
            });
        }
    }
    layout
}

#[derive(Clone, Copy)]
enum Role {
    User,
    ToolRunning,
    ToolSuccess,
    ToolFailure,
    Debug,
    Warning,
    Error,
    Internal,
}

fn item_block_ids(item: &TranscriptItem) -> Vec<BlockId> {
    match item {
        TranscriptItem::Assistant { children, .. } => children
            .iter()
            .filter_map(|child| match child {
                AssistantChild::Thinking { id, .. } => Some(BlockId::Thinking(*id)),
                AssistantChild::Tool { call_id } => Some(BlockId::Tool(*call_id)),
                AssistantChild::Text { .. } | AssistantChild::CommittedTool { .. } => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn item_interaction(
    item: &TranscriptItem,
    expanded: Option<&HashSet<BlockId>>,
    selected_block: Option<BlockId>,
) -> Vec<(BlockId, bool, bool)> {
    item_block_ids(item)
        .into_iter()
        .map(|id| {
            (
                id,
                expanded.is_some_and(|blocks| blocks.contains(&id)),
                selected_block == Some(id),
            )
        })
        .collect()
}

fn transcript_item_layout(
    state: &SessionState,
    item: &TranscriptItem,
    context: &mut TranscriptRenderContext<'_>,
) -> ItemLayout {
    match item {
        TranscriptItem::User { text, .. } => ItemLayout {
            lines: role_block(
                Role::User,
                text.lines()
                    .map(|line| Line::from(line.to_owned()))
                    .collect(),
                context.width,
                context.theme,
            ),
            regions: Vec::new(),
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
                    vec![Line::from(text.clone())],
                    context.width,
                    context.theme,
                ),
                regions: Vec::new(),
            }
        }
    }
}

fn assistant_item_layout(
    state: &SessionState,
    item_id: u64,
    attribution: &crate::state::FrozenAssistantAttribution,
    children: &[AssistantChild],
    context: &mut TranscriptRenderContext<'_>,
) -> ItemLayout {
    let mut layout = ItemLayout {
        lines: assistant_header(attribution.header().as_str(), context.width, context.theme),
        regions: Vec::new(),
    };
    for child in children {
        match child {
            AssistantChild::Text { .. } | AssistantChild::Thinking { .. } => {
                let block_id = match child {
                    AssistantChild::Thinking { id, .. } => Some(BlockId::Thinking(*id)),
                    AssistantChild::Text { .. } => None,
                    AssistantChild::Tool { .. } | AssistantChild::CommittedTool { .. } => {
                        unreachable!()
                    }
                };
                let key = AssistantPartLayoutKey {
                    version: child.version(),
                    expanded: block_id.is_some_and(|id| {
                        context.expanded.is_some_and(|blocks| blocks.contains(&id))
                    }),
                    selected: block_id.is_some_and(|id| context.selected_block == Some(id)),
                    streaming: matches!(child, AssistantChild::Thinking { id, .. } if state.is_open_thinking(item_id, *id)),
                };
                let part_layout = if context
                    .assistant_part_cache
                    .get(&child.id())
                    .is_some_and(|cached| cached.key == key)
                {
                    context.assistant_part_cache[&child.id()].layout.clone()
                } else {
                    let part_layout = assistant_child_layout(
                        child,
                        key,
                        context.width,
                        context.theme,
                        context.highlighter,
                    );
                    context.assistant_part_cache.insert(
                        child.id(),
                        CachedAssistantPartLayout {
                            key,
                            layout: part_layout.clone(),
                        },
                    );
                    *context.assistant_part_layout_passes =
                        context.assistant_part_layout_passes.wrapping_add(1);
                    part_layout
                };
                let start_line = layout.lines.len();
                layout.lines.extend(part_layout.lines);
                layout
                    .regions
                    .extend(part_layout.regions.into_iter().map(|region| BlockRegion {
                        id: region.id,
                        start_line: start_line + region.start_line,
                        end_line: start_line + region.end_line,
                    }));
            }
            AssistantChild::Tool { call_id } => {
                let child_layout = tool_child_layout(state, Some(*call_id), *call_id, context);
                let start_line = layout.lines.len();
                layout.lines.extend(child_layout.lines);
                layout
                    .regions
                    .extend(child_layout.regions.into_iter().map(|region| BlockRegion {
                        id: region.id,
                        start_line: start_line + region.start_line,
                        end_line: start_line + region.end_line,
                    }));
            }
            AssistantChild::CommittedTool { content_index } => {
                let child_layout = tool_child_layout(state, None, *content_index, context);
                let start_line = layout.lines.len();
                layout.lines.extend(child_layout.lines);
                layout
                    .regions
                    .extend(child_layout.regions.into_iter().map(|region| BlockRegion {
                        id: region.id,
                        start_line: start_line + region.start_line,
                        end_line: start_line + region.end_line,
                    }));
            }
        }
    }
    layout
}

fn assistant_child_layout(
    child: &AssistantChild,
    key: AssistantPartLayoutKey,
    width: u16,
    theme: &Theme,
    highlighter: &dyn Highlighter,
) -> ItemLayout {
    match child {
        AssistantChild::Text { markdown, .. } => ItemLayout {
            lines: crate::markdown::render_markdown_width(markdown, theme, highlighter, width)
                .into_iter()
                .flat_map(|line| assistant_body_line(line, width, theme))
                .collect(),
            regions: Vec::new(),
        },
        AssistantChild::Thinking { id, text, .. } => {
            let block_id = BlockId::Thinking(*id);
            let body = thinking_body_lines(text, width, theme);
            let hidden_lines = body.len().max(1);
            // Exactly one chevron per thinking row: `▸` collapsed, `▾`
            // expanded. Selection is conveyed through style, never a second
            // triangle.
            let mut label = if key.expanded {
                "▾ thinking".to_owned()
            } else {
                format!("▸ thinking ({hidden_lines} lines hidden)")
            };
            if key.streaming {
                label.push_str(" …");
            }
            let label_style = if key.selected {
                theme.thinking_selected()
            } else {
                theme.thinking()
            };
            let mut lines =
                assistant_body_line(Line::from(Span::styled(label, label_style)), width, theme);
            if key.expanded {
                lines.extend(body);
            }
            ItemLayout {
                regions: vec![BlockRegion {
                    id: block_id,
                    start_line: 0,
                    end_line: lines.len(),
                }],
                lines,
            }
        }
        AssistantChild::Tool { .. } | AssistantChild::CommittedTool { .. } => {
            unreachable!("tool children use tool_child_layout")
        }
    }
}

/// A compact or expanded tool row inside its owning assistant item. Compact
/// rows render the persisted sanitized title and primary argument: running
/// adds `…`, success adds no suffix, and failed/cancelled/interrupted use
/// their exact concise markers. `COMPLETED` is never rendered. Exactly one
/// chevron per row.
fn tool_child_layout(
    state: &SessionState,
    call_id: Option<cookie_agent_protocol::ToolCallId>,
    block_key: impl Into<BlockKey>,
    context: &mut TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = match block_key.into() {
        BlockKey::Call(call) => BlockId::Tool(call),
        BlockKey::ContentIndex(index) => BlockId::CommittedTool(index),
    };
    let selected = context.selected_block == Some(block_id);
    let is_expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let tool = call_id.and_then(|call_id| state.tools.get(&call_id));
    let Some(tool) = tool else {
        let lines = role_block_selected(
            Role::Error,
            vec![Line::from("tool: unavailable payload".to_owned())],
            context.width,
            context.theme,
            selected,
        );
        return ItemLayout {
            regions: vec![BlockRegion {
                id: block_id,
                start_line: 0,
                end_line: lines.len(),
            }],
            lines,
        };
    };
    let (suffix, role) = match tool.status {
        ToolStatus::Running => (" …", Role::ToolRunning),
        ToolStatus::Completed => ("", Role::ToolSuccess),
        ToolStatus::Failed => (" failed", Role::ToolFailure),
        ToolStatus::Cancelled => (" cancelled", Role::ToolFailure),
        ToolStatus::Interrupted => (" interrupted", Role::ToolFailure),
    };
    let title = tool.compact_title();
    let mut body = if is_expanded {
        vec![
            Line::from(format!("▾ {title}{suffix}")),
            Line::from(format!("arguments: {}", tool.arguments)),
        ]
    } else {
        vec![Line::from(format!("▸ {title}{suffix}"))]
    };
    if is_expanded {
        if !tool.detail.is_empty() {
            body.extend(tool_body_lines(tool, context));
        }
        if let Some(call_id) = call_id {
            for (stderr, label) in [(false, "STDOUT"), (true, "STDERR")] {
                if let Some(output) = state.output.get(&(call_id, stderr)) {
                    let gap = if output.has_gap { " [OUTPUT GAP]" } else { "" };
                    body.push(Line::from(format!("{label}{gap}:")));
                    body.extend(
                        output
                            .text()
                            .lines()
                            .map(|line| Line::from(line.to_owned())),
                    );
                }
            }
        }
    }
    let lines = tool_block_lines(role, body, context.width, context.theme, selected);
    ItemLayout {
        regions: vec![BlockRegion {
            id: block_id,
            start_line: 0,
            end_line: lines.len(),
        }],
        lines,
    }
}

/// Identity for a tool row: a started call or a committed placeholder index.
enum BlockKey {
    Call(cookie_agent_protocol::ToolCallId),
    ContentIndex(u32),
}

impl From<cookie_agent_protocol::ToolCallId> for BlockKey {
    fn from(call_id: cookie_agent_protocol::ToolCallId) -> Self {
        Self::Call(call_id)
    }
}

impl From<u32> for BlockKey {
    fn from(index: u32) -> Self {
        Self::ContentIndex(index)
    }
}

/// Expanded tool detail lines. A successful `read` of a validated textual
/// file is syntax-highlighted with the language inferred deterministically
/// from the read path's extension (the same path the tool was invoked with).
/// Binary/image/PDF summaries, failed calls, truncation/attachment metadata,
/// and unknown extensions stay plain. The reduced detail is free of secrets:
/// it contains only safe tool output and engine-authored metadata.
fn tool_body_lines(
    tool: &crate::state::ToolCallState,
    context: &TranscriptRenderContext<'_>,
) -> Vec<Line<'static>> {
    if tool.presentation.title.as_str() == "read" && tool.status == ToolStatus::Completed {
        let language =
            read_path_extension(&tool.arguments).map(crate::markdown::normalized_language);
        let (content, metadata) = split_read_detail(&tool.detail);
        let highlighted = language.and_then(|language| {
            (!content.is_empty()).then(|| {
                context
                    .highlighter
                    .highlight(&language, content, context.theme)
            })
        });
        if let Some(mut lines) = highlighted {
            lines.extend(metadata.iter().map(|line| Line::from((*line).to_owned())));
            return lines;
        }
    }
    tool.detail
        .lines()
        .map(|line| Line::from(line.to_owned()))
        .collect()
}

/// The extension of the `path` argument in a reduced `read` call's arguments
/// JSON, located structurally without a JSON parser: the argument string is
/// produced by the engine's own serialization, so the `"path"` key and its
/// quoted value are exact. Anything unexpected yields `None` and plain text.
fn read_path_extension(arguments: &str) -> Option<&str> {
    let key = arguments.find("\"path\"")?;
    let after_key = arguments.get(key + "\"path\"".len()..)?;
    let colon = after_key.find(':')?;
    let after_colon = after_key.get(colon + 1..)?.trim_start();
    let quoted = after_colon.strip_prefix('"')?;
    let end = quoted.find('"')?;
    let path = &quoted[..end];
    let name = path.rsplit(['/', '\\']).next()?;
    name.rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
        .map(|(_, extension)| extension)
}

/// Split reduced `read` detail into the file content and trailing
/// engine-authored metadata lines (truncation retention references,
/// attachment descriptors). Only the content is highlighted; metadata lines
/// always stay plain.
fn split_read_detail(detail: &str) -> (&str, Vec<&str>) {
    let mut content_len = detail.len();
    let mut metadata = Vec::new();
    loop {
        let content = detail[..content_len].trim_end_matches('\n');
        let Some(last) = content.rsplit('\n').next() else {
            break;
        };
        let trimmed = last.trim_start();
        if trimmed.starts_with("attachment: ") || trimmed.starts_with("retained output: ") {
            metadata.insert(0, last);
            content_len = content.len() - last.len();
        } else {
            break;
        }
    }
    (detail[..content_len].trim_end_matches('\n'), metadata)
}

fn assistant_header(attribution: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    // The frozen `Agent • Model` attribution wraps at tiny widths and is
    // never reduced to a tag: it is the sole producer identity.
    let text = if width >= 8 {
        format!("╭─ {attribution}")
    } else {
        attribution.to_owned()
    };
    let gutter = (width >= 4).then_some("│ ");
    let gutter_width = gutter.map_or(0, unicode_width::UnicodeWidthStr::width);
    // The continuation gutter's width is reserved before wrapping, so every
    // rendered row including its prefix fits the panel width.
    let wrap_width = u16::try_from(
        usize::from(width.max(1))
            .saturating_sub(gutter_width)
            .max(1),
    )
    .unwrap_or(u16::MAX);
    wrapped_line(
        Line::from(vec![
            Span::styled(text.clone(), theme.assistant()),
            Span::raw(" "),
        ]),
        wrap_width,
    )
    .into_iter()
    .enumerate()
    .map(|(index, mut line)| {
        if index > 0
            && let Some(gutter) = gutter
        {
            line.spans
                .insert(0, Span::styled(gutter, theme.assistant()));
        }
        line
    })
    .collect()
}

fn assistant_body_line(line: Line<'static>, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let prefix = (width >= 3).then(|| vec![Span::styled("│ ", theme.assistant())]);
    repeated_prefixed_wrapped_line(prefix.unwrap_or_default(), line, width)
}

fn thinking_body_lines(text: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    text.split('\n')
        .flat_map(|text| {
            let prefix = if width >= 5 {
                vec![
                    Span::styled("│ ", theme.assistant()),
                    Span::styled("┆ ", theme.thinking()),
                ]
            } else if width >= 3 {
                vec![Span::styled("┆ ", theme.thinking())]
            } else {
                Vec::new()
            };
            repeated_prefixed_wrapped_line(
                prefix,
                Line::styled(text.to_owned(), theme.thinking()),
                width,
            )
        })
        .collect()
}

fn role_block(
    role: Role,
    body: Vec<Line<'static>>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    role_block_selected(role, body, width, theme, false)
}

/// Tool children render inside the assistant item without a standalone
/// `TOOL` header: the compact/expanded rows keep the assistant gutter and
/// take only their status style.
fn tool_block_lines(
    role: Role,
    body: Vec<Line<'static>>,
    width: u16,
    theme: &Theme,
    selected: bool,
) -> Vec<Line<'static>> {
    let style = match role {
        Role::ToolRunning => theme.tool_running(),
        Role::ToolSuccess => theme.tool_success(),
        Role::ToolFailure => theme.tool_failure(),
        _ => theme.tool(),
    };
    let style = if selected {
        style.add_modifier(ratatui::style::Modifier::UNDERLINED)
    } else {
        style
    };
    if width < 8 {
        let short = match role {
            Role::ToolRunning => "T…",
            Role::ToolSuccess => "T✓",
            Role::ToolFailure => "T!",
            _ => "T",
        };
        let mut lines = Vec::new();
        for (index, line) in body.into_iter().enumerate() {
            let prefix = if index == 0 {
                format!("[{short}] ")
            } else {
                "    ".into()
            };
            lines.extend(prefixed_wrapped_line(prefix, style, line, width));
        }
        return lines;
    }
    let mut lines = Vec::new();
    for line in body {
        let mut spans = vec![Span::styled("│ ", theme.assistant())];
        spans.extend(line.spans.into_iter().map(|mut span| {
            span.style = style.patch(span.style);
            span
        }));
        lines.extend(repeated_prefixed_wrapped_line(
            Vec::new(),
            Line::from(spans),
            width,
        ));
    }
    lines
}

fn role_block_selected(
    role: Role,
    body: Vec<Line<'static>>,
    width: u16,
    theme: &Theme,
    selected: bool,
) -> Vec<Line<'static>> {
    let (label, marker, gutter, style) = match role {
        Role::User => ("USER", "┌─", "│ ", theme.user()),
        Role::ToolRunning => ("TOOL RUNNING", "┏…", "┃ ", theme.tool_running()),
        Role::ToolSuccess => ("TOOL SUCCESS", "┏✓", "┃ ", theme.tool_success()),
        Role::ToolFailure => ("TOOL FAILURE", "┏!", "┃ ", theme.tool_failure()),
        Role::Debug => ("DEBUG [D]", "··", "· ", theme.muted()),
        Role::Warning => ("WARNING [W]", "⚠─", "│ ", theme.warning()),
        Role::Error => ("ERROR [E]", "!!", "! ", theme.error()),
        Role::Internal => ("EVENT [I]", "--", "· ", theme.internal()),
    };
    // Selection adds an underline/reversed emphasis on top of the role style;
    // it never adds a glyph, so toggle rows keep exactly one chevron.
    let style = if selected {
        style.add_modifier(ratatui::style::Modifier::UNDERLINED)
    } else {
        style
    };
    if width < 8 {
        let short = match role {
            Role::User => "U",
            Role::ToolRunning => "T…",
            Role::ToolSuccess => "T✓",
            Role::ToolFailure => "T!",
            Role::Debug => "D",
            Role::Warning => "W",
            Role::Error => "E",
            Role::Internal => "I",
        };
        let mut lines = Vec::new();
        for (index, line) in body.into_iter().enumerate() {
            let prefix = if index == 0 {
                format!("[{short}] ")
            } else {
                "    ".into()
            };
            lines.extend(prefixed_wrapped_line(prefix, style, line, width));
        }
        return lines;
    }
    let mut lines = wrapped_line(
        Line::from(vec![
            Span::styled(format!("{marker} {label}"), style),
            Span::raw(" "),
        ]),
        width,
    );
    for line in body {
        lines.extend(prefixed_wrapped_line(gutter.into(), style, line, width));
    }
    lines
}

fn prefixed_wrapped_line(
    prefix: String,
    prefix_style: Style,
    line: Line<'static>,
    width: u16,
) -> Vec<Line<'static>> {
    let continuation = " ".repeat(UnicodeWidthStr::width(prefix.as_str()));
    let mut spans = vec![Span::styled(prefix, prefix_style)];
    spans.extend(line.spans);
    let mut wrapped = wrapped_line(Line::from(spans), width);
    for line in wrapped.iter_mut().skip(1) {
        line.spans.insert(0, Span::raw(continuation.clone()));
    }
    wrapped
}

fn repeated_prefixed_wrapped_line(
    mut prefix: Vec<Span<'static>>,
    line: Line<'static>,
    width: u16,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let prefix_width = prefix
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    if prefix_width >= width {
        prefix.clear();
    }
    let prefix_width = prefix
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    wrapped_line(
        line,
        u16::try_from(width.saturating_sub(prefix_width).max(1)).unwrap_or(u16::MAX),
    )
    .into_iter()
    .map(|line| {
        let mut spans = prefix.clone();
        spans.extend(line.spans);
        Line::from(spans)
    })
    .collect()
}

/// Word-wrap a styled line using the same word-boundary behavior as the
/// paragraph renderer. Long individual words fall back to grapheme wrapping.
pub(super) fn wrapped_line(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    // A line-level style (`Line::styled`) applies to every span it contains;
    // preserve it on each wrapped output line so underline/bold emphasis
    // survives wrapping.
    let line_style = line.style;
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0;
    let mut pending_whitespace = Vec::new();
    let mut pending_whitespace_width = 0;

    for (content, style, whitespace) in line_tokens(line) {
        let token_width = UnicodeWidthStr::width(content.as_str());
        if whitespace {
            append_span(&mut pending_whitespace, content, style);
            pending_whitespace_width += token_width;
            continue;
        }
        if current_width > 0 && current_width + pending_whitespace_width + token_width > width {
            lines.push(line_from_spans(std::mem::take(&mut current)));
            current_width = 0;
            pending_whitespace.clear();
            pending_whitespace_width = 0;
        }
        if !pending_whitespace.is_empty() {
            if current_width + pending_whitespace_width <= width {
                current.append(&mut pending_whitespace);
                current_width += pending_whitespace_width;
            }
            pending_whitespace_width = 0;
        }
        append_word(
            &mut lines,
            &mut current,
            &mut current_width,
            content,
            style,
            width,
        );
    }
    if !current.is_empty() || lines.is_empty() {
        if !pending_whitespace.is_empty() && current_width + pending_whitespace_width <= width {
            current.append(&mut pending_whitespace);
        }
        lines.push(line_from_spans(current));
    }
    if line_style != Style::default() {
        for line in &mut lines {
            line.style = line.style.patch(line_style);
        }
    }
    lines
}

pub(super) fn line_tokens(line: Line<'static>) -> Vec<(String, Style, bool)> {
    let mut tokens = Vec::new();
    for span in line.spans {
        let mut token = String::new();
        let mut whitespace = None;
        for character in span.content.chars() {
            let is_whitespace = character.is_whitespace();
            if let Some(previous) = whitespace
                && previous != is_whitespace
            {
                tokens.push((std::mem::take(&mut token), span.style, previous));
            }
            token.push(character);
            whitespace = Some(is_whitespace);
        }
        if let Some(whitespace) = whitespace {
            tokens.push((token, span.style, whitespace));
        }
    }
    tokens
}

pub(super) fn append_word(
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    word: String,
    style: Style,
    width: usize,
) {
    let word_width = UnicodeWidthStr::width(word.as_str());
    if word_width <= width {
        append_span(current, word, style);
        *current_width += word_width;
        return;
    }
    for grapheme in word.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if *current_width > 0 && *current_width + grapheme_width > width {
            lines.push(line_from_spans(std::mem::take(current)));
            *current_width = 0;
        }
        append_span(current, grapheme.to_owned(), style);
        *current_width += grapheme_width;
    }
}

pub(super) fn append_span(spans: &mut Vec<Span<'static>>, content: String, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(&content);
    } else {
        spans.push(Span::styled(content, style));
    }
}

pub(super) fn line_from_spans(spans: Vec<Span<'static>>) -> Line<'static> {
    let mut merged = Vec::new();
    for span in spans {
        append_span(&mut merged, span.content.into_owned(), span.style);
    }
    Line::from(merged)
}

pub(super) fn block_hit(
    region: BlockRegion,
    viewport: Rect,
    scroll_offset: usize,
) -> Option<BlockHit> {
    let viewport_end = scroll_offset.saturating_add(usize::from(viewport.height));
    let start = region.start_line.max(scroll_offset);
    let end = region.end_line.min(viewport_end);
    (start < end).then(|| BlockHit {
        rect: Rect::new(
            viewport.x,
            viewport.y + u16::try_from(start - scroll_offset).unwrap_or(u16::MAX),
            viewport.width,
            u16::try_from(end - start).unwrap_or(u16::MAX),
        ),
        id: region.id,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use cookie_agent_config::{
        ApprovalConfig, ConfigSchemaVersion, ContextCompactionConfig, LoadedConfiguration,
        RuntimeConfig, ServerConfig, SessionTitleConfig, ToolOutputConfig,
    };
    use cookie_agent_engine::{Engine, EngineOptions};
    use cookie_agent_models::{
        ModelManager,
        catalog::{
            CatalogAgeState, CatalogAvailability, CatalogLimits, CatalogModalities,
            CatalogModelEntry, CatalogModelRecord, CatalogModelStatus, CatalogProviderEntry,
            CatalogProviderRecord, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
        },
        provider_store::{
            ClientConnectId as StoreClientConnectId, ConnectMutation, ConnectProposal,
            ProviderAuthValues, ProviderStore, SafePolicyString, StoredProviderPolicyProjection,
        },
    };
    use cookie_agent_protocol::{
        AgentId, ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation,
        ApprovalId, ApprovalRecord, ApprovalRequest, ApprovalResourceSource, ApprovalStatus,
        ApprovalTrigger, AssistantToolCallRef, AttemptId, DecisionTrace, EventPayload,
        EventSchemaVersion, ModelCallId, ModelKey, ModelSelection, OperationFingerprint,
        OutputDelta, OutputStream, PermissionAction, PermissionEffect, PreparedApprovalResource,
        PreparedBindingLifetime, PreparedCapabilityOperation, PreparedOperationIdentity,
        PreparedResourceDigest, PreparedResourceIdentity, ProviderId, RunId, RunSelection,
        SafeCode, SafeDisplayText, SafeErrorMessage, SessionId, SessionMeta,
        SessionMetaSchemaVersion, SessionOrigin, SessionStatus, SessionTitle, SessionTree,
        Sha256Digest, StoredEvent, ToolCallId, ToolCallStart, Usage,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use jiff::Timestamp;
    use ratatui::{Terminal, backend::TestBackend, text::Line};

    use crate::Client;
    use crate::markdown::{MarkdownDocument, PlainHighlighter};
    use crate::state::{
        ApprovalState, AssistantChild, FrozenAssistantAttribution, SessionState, StateStore,
        ToolCallState,
    };
    use crate::theme::{ColorLevel, ThemeKind};
    use crate::ui::app::*;
    use crate::ui::events::{RenderScheduler, TerminalCleanup, TerminalRestore};
    use crate::ui::input::credential_wipe_count;
    use crate::ui::provider::{ProviderAction, ProviderForm, ProviderOperation};
    use crate::ui::slash::{
        BlockCommand, InputMode, ScrollCommand, SlashCommand, Submission, command_allowed_in_mode,
        command_help, command_spec, parse_submission,
    };
    use crate::ui::terminal_layout_with_tree_rows;

    use async_trait::async_trait;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use cookie_agent_server::{MessageFrame, MessageStream, Server, TransportError};
    use serde_json::Value;

    // ------------------------------------------------------------------
    // Fixtures
    // ------------------------------------------------------------------

    const AGENT: &str = "primary";
    const MODEL: &str = "gateway/arbitrary-model";

    struct ProductionProviderHarness {
        _directory: tempfile::TempDir,
        engine: Engine,
        server: Arc<Server>,
    }

    fn production_provider_harness(
        catalog: Arc<CatalogSnapshot>,
        prepare_store: impl FnOnce(&ProviderStore),
    ) -> ProductionProviderHarness {
        let directory = tempfile::tempdir().expect("production provider test directory");
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private test directory");
        let provider_store_path = directory.path().join("provider-store");
        fs::create_dir(&provider_store_path).expect("provider store directory");
        #[cfg(unix)]
        fs::set_permissions(&provider_store_path, fs::Permissions::from_mode(0o700))
            .expect("private provider store");
        let store = ProviderStore::open(&provider_store_path).expect("provider store");
        prepare_store(&store);
        let manager = Arc::new(
            ModelManager::new(BTreeMap::new(), catalog, store).expect("production model manager"),
        );
        let config = LoadedConfiguration {
            runtime: RuntimeConfig {
                schema_version: ConfigSchemaVersion,
                server: ServerConfig::default(),
                tool_output: ToolOutputConfig::default(),
                approval: ApprovalConfig::default(),
                context_compaction: ContextCompactionConfig::default(),
                session_title: SessionTitleConfig::default(),
                providers: BTreeMap::new(),
            },
            agents: BTreeMap::new(),
        };
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config,
            model_manager: manager,
            tools: Vec::new(),
        })
        .expect("production engine");
        let server = Arc::new(Server::new(engine.clone()));
        ProductionProviderHarness {
            _directory: directory,
            engine,
            server,
        }
    }

    fn production_openai_catalog(label: char, quarantined: bool) -> Arc<CatalogSnapshot> {
        let provider_id = ProviderId::new("openai").expect("provider ID");
        let model_id = cookie_agent_protocol::ProviderModelId::new("gpt-5-mini").expect("model ID");
        let environment = vec!["OPENAI_API_KEY".to_owned()];
        let model = CatalogModelRecord {
            id: model_id.clone(),
            name: "GPT-5 mini".to_owned(),
            description: "TUI production projection test".to_owned(),
            family: None,
            attachment: false,
            reasoning: false,
            tool_call: true,
            structured_output: Some(true),
            temperature: Some(true),
            open_weights: false,
            status: CatalogModelStatus::Stable,
            release_date: "2026-01-01".to_owned(),
            last_updated: "2026-01-01".to_owned(),
            modalities: CatalogModalities {
                input: vec!["text".to_owned()],
                output: vec!["text".to_owned()],
            },
            limits: CatalogLimits {
                context: 128_000,
                input: None,
                output: 16_384,
            },
            shape: None,
            provider: None,
            reasoning_options: Vec::new(),
            interleaved: None,
            canonical_provenance: None,
        };
        let shape = quarantined.then(|| "unexpected".to_owned());
        let record = CatalogProviderRecord {
            id: provider_id.clone(),
            name: "OpenAI".to_owned(),
            environment: environment.clone(),
            npm: "@ai-sdk/openai".to_owned(),
            api: None,
            shape: shape.clone(),
            documentation_url: "https://example.test/openai".to_owned(),
            models: BTreeMap::from([(
                model_id.clone(),
                CatalogModelEntry {
                    id: model_id,
                    record: Some(model),
                    quarantine: None,
                },
            )]),
        };
        let now = Timestamp::now();
        Arc::new(CatalogSnapshot {
            revision: cookie_agent_protocol::CatalogRevision::new(format!(
                "sha256:{}",
                label.to_string().repeat(64)
            ))
            .expect("catalog revision"),
            source: CatalogSource::Network,
            state: CatalogRuntimeState {
                availability: CatalogAvailability::Ready,
                age: CatalogAgeState::Current,
                last_error: None,
            },
            validated_at: now,
            last_checked_at: now,
            etag: None,
            providers: BTreeMap::from([(
                provider_id.clone(),
                CatalogProviderEntry {
                    id: provider_id,
                    record: Some(record),
                    quarantine: None,
                },
            )]),
            canonical_models: BTreeMap::new(),
            quarantine: Vec::new(),
        })
    }

    fn production_empty_catalog(label: char) -> Arc<CatalogSnapshot> {
        let now = Timestamp::now();
        Arc::new(CatalogSnapshot {
            revision: cookie_agent_protocol::CatalogRevision::new(format!(
                "sha256:{}",
                label.to_string().repeat(64)
            ))
            .expect("catalog revision"),
            source: CatalogSource::Network,
            state: CatalogRuntimeState {
                availability: CatalogAvailability::Ready,
                age: CatalogAgeState::Current,
                last_error: None,
            },
            validated_at: now,
            last_checked_at: now,
            etag: None,
            providers: BTreeMap::new(),
            canonical_models: BTreeMap::new(),
            quarantine: Vec::new(),
        })
    }

    fn install_unmatched_openai_connection(store: &ProviderStore, catalog: &CatalogSnapshot) {
        let transaction = store.begin_transaction().expect("provider transaction");
        let snapshot = transaction.snapshot();
        let mutation = ConnectMutation {
            client_connect_id: StoreClientConnectId::new("tui-unmatched-retained")
                .expect("connect ID"),
            provider_id: ProviderId::new("openai").expect("provider ID"),
            expected_catalog_revision: catalog.revision.clone(),
            expectation: snapshot.expectation(),
            setup_values: BTreeMap::new(),
            auth_method: cookie_agent_protocol::AuthMethodId::new("bearer-api-key-v1")
                .expect("auth method"),
            auth_values: ProviderAuthValues::new(BTreeMap::from([(
                cookie_agent_protocol::AuthFieldName::new("api_key").expect("auth field"),
                "stored-secret".to_owned(),
            )]))
            .expect("auth values"),
            policy: StoredProviderPolicyProjection {
                catalog_revision: catalog.revision.clone(),
                family_id: SafePolicyString::new("openai").expect("family ID"),
                setup_recipe: cookie_agent_protocol::ProviderSetupRecipeId::new("no-setup-v1")
                    .expect("setup recipe"),
                adapter_id: SafePolicyString::new("openai").expect("adapter ID"),
                compiler_version: cookie_agent_protocol::RecipeCompilerVersion::new(
                    "family-registry-compiler-v1",
                )
                .expect("compiler version"),
                default_endpoint_identity: SafePolicyString::new("https://api.openai.com/v1")
                    .expect("endpoint"),
                package_claim: SafePolicyString::new("@ai-sdk/openai-forged")
                    .expect("mismatched package"),
                source_record_digest: cookie_agent_models::Sha256Digest::new("d".repeat(64))
                    .expect("source digest"),
                recipe_fingerprint: cookie_agent_models::Sha256Digest::new("e".repeat(64))
                    .expect("recipe fingerprint"),
                model_overrides: BTreeMap::new(),
            },
        };
        let ConnectProposal::Proposed(proposal) = transaction
            .propose_connect(&mutation, &catalog.revision)
            .expect("connect proposal")
        else {
            panic!("unmatched retained policy unexpectedly replayed")
        };
        transaction.commit(*proposal).expect("connect commit");
    }

    fn agent_id() -> AgentId {
        AgentId::new(AGENT).expect("agent id")
    }

    fn model_key() -> ModelKey {
        MODEL.parse::<ModelKey>().expect("model key")
    }

    fn resolved_model(variant: Option<&str>) -> cookie_agent_protocol::ResolvedModelRef {
        let selection = ModelSelection {
            model: model_key(),
            variant: variant
                .map(|id| cookie_agent_protocol::VariantId::new(id).expect("variant id")),
        };
        cookie_agent_protocol::ResolvedModelRef {
            provider_id: ProviderId::new("gateway").expect("provider id"),
            model_id: cookie_agent_protocol::ProviderModelId::new("arbitrary-model")
                .expect("model id"),
            adapter_id: cookie_agent_protocol::AdaptorId::OpenaiCompatible,
            selection_fingerprint: Sha256Digest::of_bytes(
                format!("selection:{selection:?}").as_bytes(),
            ),
            selection,
        }
    }

    fn protocol_revision<T>(digit: &str) -> T
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(serde_json::json!(format!(
            "sha256:{}",
            digit.repeat(64 / digit.len())
        )))
        .expect("protocol revision")
    }

    fn frozen_binding(
        resolved: cookie_agent_protocol::ResolvedModelRef,
    ) -> cookie_agent_protocol::FrozenModelBinding {
        cookie_agent_protocol::FrozenModelBinding {
            manifest_revision: protocol_revision("a"),
            blueprint_fingerprint: Sha256Digest::of_bytes(b"blueprint"),
            selection: resolved.selection.clone(),
            source: cookie_agent_protocol::FrozenProviderSource::Custom {
                safe_definition_fingerprint: Sha256Digest::of_bytes(b"definition"),
            },
            config_override_fingerprint: Sha256Digest::of_bytes(b"override"),
            credential_binding: cookie_agent_protocol::FrozenCredentialBinding {
                source: cookie_agent_protocol::FrozenCredentialSource::NoAuth,
                auth_method: cookie_agent_protocol::AuthMethodId::new("no-auth")
                    .expect("auth method"),
                fields: Vec::new(),
                parameters: BTreeMap::new(),
                owned_headers: Vec::new(),
            },
            setup_binding: cookie_agent_protocol::FrozenSetupBinding {
                setup_recipe: cookie_agent_protocol::ProviderSetupRecipeId::new("custom-setup")
                    .expect("setup recipe"),
                values: BTreeMap::new(),
                setup_fingerprint: Sha256Digest::of_bytes(b"setup"),
            },
            endpoint_identity: cookie_agent_protocol::SafeEndpointIdentity::new(
                "https://example.test/v1",
            )
            .expect("endpoint"),
            provider_recipe: cookie_agent_protocol::ProviderRecipeId::new("custom-provider")
                .expect("provider recipe"),
            protocol_recipe: cookie_agent_protocol::ProtocolRecipeId::new("custom-protocol")
                .expect("protocol recipe"),
            setup_recipe: cookie_agent_protocol::ProviderSetupRecipeId::new("custom-setup")
                .expect("setup recipe"),
            compiler_version: cookie_agent_protocol::RecipeCompilerVersion::new("compiler-v1")
                .expect("compiler version"),
            descriptor: serde_json::from_value(serde_json::json!({
                "identity": {"provider_id": "gateway", "model_id": "arbitrary-model"},
                "adapter_id": "openai-compatible",
                "capabilities": {
                    "features": [],
                    "limits": {"context": 8192, "input": null, "output": 2048},
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "media": {"input": {}},
                    "cancellation": "local_only",
                    "compaction": "unsupported",
                    "replay": {"policy": "never", "capability": "unsupported", "reasoning": false}
                },
                "provider_metadata": {}
            }))
            .expect("test descriptor"),
            defaults: cookie_agent_protocol::FrozenResolvedRequestDefaults {
                request: cookie_agent_protocol::FrozenRequestDefaults::default(),
                reasoning: None,
            },
            options: cookie_agent_protocol::ProviderOptions::OpenAiCompatible { api_path: None },
            static_headers: BTreeMap::new(),
            behavior_fingerprint: resolved.selection_fingerprint.clone(),
            selection_fingerprint: resolved.selection_fingerprint,
        }
    }

    fn attribution(variant: Option<&str>) -> FrozenAssistantAttribution {
        FrozenAssistantAttribution {
            agent: agent_id(),
            resolved_model: resolved_model(variant),
        }
    }

    fn run_id() -> RunId {
        RunId::new_v7()
    }

    fn event(session_id: SessionId, seq: u64, run: RunId, payload: EventPayload) -> StoredEvent {
        StoredEvent {
            event_schema_version: EventSchemaVersion::current(),
            session_id,
            run_id: Some(run),
            seq,
            timestamp: Timestamp::now(),
            payload,
        }
    }

    fn attempt_started(
        session_id: SessionId,
        seq: u64,
        run: RunId,
        attempt: AttemptId,
        variant: Option<&str>,
    ) -> StoredEvent {
        event(
            session_id,
            seq,
            run,
            EventPayload::ModelAttemptStarted {
                attempt_id: attempt,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model(variant),
                prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
            },
        )
    }

    fn session_created(session_id: SessionId, seq: u64) -> StoredEvent {
        session_created_with(session_id, seq, AGENT, vec![resolved_model(None)], 0)
    }

    /// A `SessionCreated` for one agent with an exact frozen fallback chain
    /// and selected suffix start.
    fn session_created_with(
        session_id: SessionId,
        seq: u64,
        agent: &str,
        chain: Vec<cookie_agent_protocol::ResolvedModelRef>,
        suffix_start: u32,
    ) -> StoredEvent {
        let selection = RunSelection {
            agent: AgentId::new(agent).expect("agent id"),
            model: chain[suffix_start as usize].selection.clone(),
        };
        let chain = chain.into_iter().map(frozen_binding).collect::<Vec<_>>();
        session_created_from_bindings(session_id, seq, selection, chain, suffix_start)
    }

    fn session_created_from_bindings(
        session_id: SessionId,
        seq: u64,
        selection: RunSelection,
        chain: Vec<cookie_agent_protocol::FrozenModelBinding>,
        suffix_start: u32,
    ) -> StoredEvent {
        StoredEvent {
            event_schema_version: EventSchemaVersion::current(),
            session_id,
            run_id: None,
            seq,
            timestamp: Timestamp::now(),
            payload: EventPayload::SessionCreated {
                origin: SessionOrigin::Root,
                cwd_identity: cookie_agent_protocol::CwdIdentity::new("/workspace").expect("cwd"),
                creation_selection: selection.clone(),
                creation_agent: Box::new(cookie_agent_protocol::AgentSnapshot {
                    agent: selection.agent.clone(),
                    schema: cookie_agent_protocol::AgentSchemaVersion::current(),
                    mode: cookie_agent_protocol::AgentMode::Primary,
                    description: "Test primary agent".into(),
                    document_source: cookie_agent_protocol::AgentDocumentSource::Workspace,
                    document_fingerprint: Sha256Digest::of_bytes(b"document"),
                    composed_prompt: "You are the primary test agent.\n".into(),
                    prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
                    tools: Vec::new(),
                    permissions: Vec::new(),
                    delegation: None,
                    fallback_chain: chain,
                    selected_suffix_start: suffix_start,
                }),
                runtime_revision: protocol_revision("1"),
                catalog_revision: protocol_revision("2"),
                provider_state_revision: protocol_revision("3"),
                model_revision: protocol_revision("4"),
                agent_revision: protocol_revision("5"),
                recipe_registry_revision: protocol_revision("6"),
                manifest_revision: protocol_revision("7"),
            },
        }
    }

    fn text_delta(
        session_id: SessionId,
        seq: u64,
        run: RunId,
        attempt: AttemptId,
        text: &str,
    ) -> StoredEvent {
        event(
            session_id,
            seq,
            run,
            EventPayload::TextDelta {
                attempt_id: attempt,
                text: text.into(),
            },
        )
    }

    fn reasoning_delta(
        session_id: SessionId,
        seq: u64,
        run: RunId,
        attempt: AttemptId,
        text: &str,
    ) -> StoredEvent {
        event(
            session_id,
            seq,
            run,
            EventPayload::ReasoningDelta {
                attempt_id: attempt,
                text: text.into(),
            },
        )
    }

    // Test fixtures stay explicit about each protocol-8 turn field; grouping them
    // would obscure the event shape without reducing call-site complexity.
    #[allow(clippy::too_many_arguments)]
    fn turn_committed(
        session_id: SessionId,
        seq: u64,
        run: RunId,
        attempt: AttemptId,
        turn_seq: u64,
        content: Vec<cookie_agent_protocol::PersistedAssistantPart>,
        warnings: Vec<&str>,
        variant: Option<&str>,
    ) -> StoredEvent {
        event(
            session_id,
            seq,
            run,
            EventPayload::ModelTurnCommitted {
                attempt_id: attempt,
                model_turn_seq: turn_seq,
                resolved_model: resolved_model(variant),
                input_through_seq: seq,
                turn: cookie_agent_protocol::PersistedModelTurn {
                    content,
                    provider_options: BTreeMap::new(),
                    finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
                    usage: Usage {
                        input_tokens: Some(10),
                        input_tokens_no_cache: Some(8),
                        input_tokens_cache_read: Some(2),
                        input_tokens_cache_write: Some(0),
                        output_tokens: Some(4),
                        output_tokens_text: Some(3),
                        output_tokens_reasoning: Some(1),
                    },
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: warnings
                    .into_iter()
                    .map(|warning| SafeErrorMessage::new(warning).expect("warning"))
                    .collect(),
            },
        )
    }

    fn presentation(
        title: &str,
        primary: Option<&str>,
    ) -> cookie_agent_protocol::ToolCallPresentation {
        cookie_agent_protocol::ToolCallPresentation {
            title: SafeDisplayText::new(title).expect("presentation title"),
            primary_argument: primary
                .map(|argument| SafeDisplayText::new(argument).expect("primary argument")),
        }
    }

    fn owner(turn_seq: u64, call: &str) -> AssistantToolCallRef {
        AssistantToolCallRef {
            model_turn_seq: turn_seq,
            content_index: 0,
            model_call_id: ModelCallId::new(call).expect("model call id"),
            provider_item_id: None,
        }
    }

    fn operation_fingerprint() -> OperationFingerprint {
        OperationFingerprint::from_prepared_operation(
            &PreparedOperationIdentity::new(
                Sha256Digest::of_bytes(b"arguments"),
                vec![ApprovalCapability {
                    action: PermissionAction::Bash,
                    operation: PreparedCapabilityOperation::new("execute")
                        .expect("capability operation"),
                }],
                Vec::new(),
                Sha256Digest::of_bytes(b"context"),
            )
            .expect("prepared operation"),
        )
    }

    // Fixture mirrors the exact protocol-8 ownership/presentation fields; grouping
    // them would obscure the event shape.
    #[allow(clippy::too_many_arguments)]
    fn tool_started_at(
        session_id: SessionId,
        seq: u64,
        run: RunId,
        call_id: ToolCallId,
        turn_seq: u64,
        call: &str,
        content_index: u32,
        title: &str,
        primary: Option<&str>,
    ) -> StoredEvent {
        let mut owner = owner(turn_seq, call);
        owner.content_index = content_index;
        event(
            session_id,
            seq,
            run,
            EventPayload::ToolCallStarted {
                start: ToolCallStart {
                    tool_call_id: call_id,
                    owner,
                    presentation: presentation(title, primary),
                    operation_fingerprint: operation_fingerprint(),
                },
            },
        )
    }

    fn tool_started(
        session_id: SessionId,
        seq: u64,
        run: RunId,
        call_id: ToolCallId,
        turn_seq: u64,
        call: &str,
    ) -> StoredEvent {
        tool_started_at(session_id, seq, run, call_id, turn_seq, call, 0, call, None)
    }

    fn tool_terminated(
        session_id: SessionId,
        seq: u64,
        run: RunId,
        call_id: ToolCallId,
        turn_seq: u64,
        call: &str,
        outcome: cookie_agent_protocol::ToolTerminationOutcome,
    ) -> StoredEvent {
        let completed = matches!(
            outcome,
            cookie_agent_protocol::ToolTerminationOutcome::Completed
        );
        event(
            session_id,
            seq,
            run,
            EventPayload::ToolCallTerminated {
                termination: cookie_agent_protocol::ToolCallTermination {
                    tool_call_id: call_id,
                    owner: owner(turn_seq, call),
                    outcome,
                    result: completed.then(|| cookie_agent_protocol::PersistedToolResult {
                        title: SafeDisplayText::new("ran true").expect("result title"),
                        output: "done".into(),
                        metadata: serde_json::Value::Null,
                        truncation: None,
                        attachments: Vec::new(),
                    }),
                    error: (!completed).then(|| cookie_agent_protocol::SafeToolError {
                        code: SafeCode::new("exit_failure").expect("error code"),
                        message: SafeErrorMessage::new("command failed").expect("error message"),
                    }),
                },
            },
        )
    }

    fn session_meta(id: SessionId) -> SessionMeta {
        SessionMeta {
            meta_schema_version: SessionMetaSchemaVersion::current(),
            session_id: id,
            origin: SessionOrigin::Root,
            cwd_identity: cookie_agent_protocol::CwdIdentity::new("/workspace").expect("cwd"),
            creation_selection: RunSelection {
                agent: agent_id(),
                model: ModelSelection {
                    model: model_key(),
                    variant: None,
                },
            },
            runtime_revision: protocol_revision("1"),
            catalog_revision: protocol_revision("2"),
            provider_state_revision: protocol_revision("3"),
            model_revision: protocol_revision("4"),
            agent_revision: protocol_revision("5"),
            recipe_registry_revision: protocol_revision("6"),
            manifest_revision: protocol_revision("7"),
            title: None,
            title_updated_seq: 0,
            last_event_seq: 1,
            status: SessionStatus::Idle,
        }
    }

    fn titled_meta(session_id: SessionId, title: &str, title_seq: u64) -> SessionMeta {
        SessionMeta {
            title: Some(SessionTitle::new(title).expect("title")),
            title_updated_seq: title_seq,
            ..session_meta(session_id)
        }
    }

    fn approval_request(trigger: ApprovalTrigger) -> ApprovalRequest {
        let resource = PreparedApprovalResource {
            capability: PermissionAction::Bash,
            canonical: PreparedResourceIdentity::new("command:git-status")
                .expect("prepared resource identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::CommandPrefix {
                prefix: "git status".into(),
            },
            source: ApprovalResourceSource::ModelRequest,
        };
        let resource_digest = resource.binding_digest.clone();
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"normalized arguments"),
            vec![ApprovalCapability {
                action: PermissionAction::Bash,
                operation: PreparedCapabilityOperation::new("execute")
                    .expect("prepared capability operation"),
            }],
            vec![resource],
            Sha256Digest::of_bytes(b"execution context"),
        )
        .expect("prepared operation");
        ApprovalRequest::new(
            ApprovalId::new_v7(),
            3,
            trigger,
            operation,
            vec![ApprovalEvaluation {
                resource_digest,
                effect: PermissionEffect::Ask,
                trace: DecisionTrace {
                    action: PermissionAction::Bash,
                    normalized_resource: "git status".into(),
                    candidates: Vec::new(),
                    effect: PermissionEffect::Ask,
                    precedence_reason: "model requested approval".into(),
                },
            }],
            ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: false,
                cancellable: true,
                expires_at: None,
            },
        )
        .expect("approval request")
    }

    fn approval(session_id: SessionId) -> ApprovalState {
        crate::state::approval_state_from_record(ApprovalRecord {
            session_id,
            request: approval_request(ApprovalTrigger::ModelToolApproval),
            status: ApprovalStatus::Escalated,
            internal_decision: None,
            user_decision: None,
            final_decision: None,
        })
        .expect("escalated approval state")
    }

    fn descriptor(agent: &str, runnable: bool) -> cookie_agent_protocol::AgentDescriptor {
        cookie_agent_protocol::AgentDescriptor {
            id: AgentId::new(agent).expect("agent id"),
            description: format!("Test {agent} agent"),
            mode: cookie_agent_protocol::AgentMode::Primary,
            enabled: runnable,
            runnable_as_root: runnable,
            resolved_fallback: vec![ModelSelection {
                model: model_key(),
                variant: None,
            }],
            tools: Vec::new(),
            delegation_targets: Vec::new(),
        }
    }

    fn model_descriptor() -> cookie_agent_protocol::AvailableModelDescriptor {
        cookie_agent_protocol::AvailableModelDescriptor {
            key: model_key(),
            display_name: "Arbitrary Model".into(),
            capabilities: cookie_agent_protocol::ModelCapabilities {
                input: [cookie_agent_protocol::Modality::Text]
                    .into_iter()
                    .collect(),
                output: [cookie_agent_protocol::Modality::Text]
                    .into_iter()
                    .collect(),
                context_tokens: 8192,
                output_tokens: 2048,
                tool_calling: true,
                parallel_tool_calls: true,
                structured_output: false,
                reasoning: true,
                temperature: true,
                top_p: true,
                seed: true,
                native_replay: cookie_agent_protocol::ReplayCapability::Optional,
                native_compaction: cookie_agent_protocol::CompactionCapability::Unsupported,
                cancellation: cookie_agent_protocol::CancellationCapability::LocalOnly,
                media: BTreeMap::new(),
            },
            variants: vec![
                cookie_agent_protocol::AvailableVariantDescriptor {
                    id: cookie_agent_protocol::VariantId::new("fast").expect("variant"),
                    display_name: "Fast".into(),
                    origin: cookie_agent_protocol::VariantOrigin::Explicit,
                    behavior_fingerprint: Sha256Digest::of_bytes(b"fast"),
                },
                cookie_agent_protocol::AvailableVariantDescriptor {
                    id: cookie_agent_protocol::VariantId::new("high").expect("variant"),
                    display_name: "High".into(),
                    origin: cookie_agent_protocol::VariantOrigin::ModelsDevEffort,
                    behavior_fingerprint: Sha256Digest::of_bytes(b"high"),
                },
            ],
            default_variant: None,
            behavior_fingerprint: Sha256Digest::of_bytes(b"model"),
        }
    }

    fn provider_descriptor(
        id: &str,
        support: &str,
        presence: &str,
        connected: bool,
    ) -> cookie_agent_protocol::ProviderDescriptor {
        let reason = (support != "supported").then_some("unsupported_environment");
        let durable = connected.then(|| {
            serde_json::json!({
                "provider_id": id,
                "setup_values": {"region": "us-east-1"},
                "setup_fingerprint": Sha256Digest::of_bytes(b"setup"),
                "recipe_fingerprint": Sha256Digest::of_bytes(b"recipe"),
                "auth_method": "api-key",
                "credential_fields": ["api_key"],
                "connection_generation": 1,
                "connected_at": Timestamp::now()
            })
        });
        serde_json::from_value(serde_json::json!({
            "id": id,
            "display_name": format!("{id} provider"),
            "presence": presence,
            "support": {"state": support, "reason": reason},
            "setup_fields": [{
                "id": "region",
                "display_name": "Region",
                "help": "Public service region",
                "required": true,
                "default": "us-west-2",
                "validation": {"value_type": "string", "min_length": 2, "max_length": 32, "minimum": null, "maximum": null},
                "safe_to_project": true
            }],
            "auth_methods": [{
                "id": "api-key",
                "display_name": "API key",
                "credentials": [{
                    "id": "api_key",
                    "display_name": "API key",
                    "help": "Secret API credential",
                    "required": true,
                    "credential_type": "api_key"
                }]
            }],
            "configuration": if connected {"stored"} else {"unconfigured"},
            "effective_auth_state": if connected {"provider_store"} else {"unavailable"},
            "durable_connection": durable,
            "quarantine": null
        }))
        .expect("provider descriptor")
    }

    fn runtime_snapshot(
        digit: &str,
        providers: Vec<cookie_agent_protocol::ProviderDescriptor>,
        models: Vec<cookie_agent_protocol::AvailableModelDescriptor>,
        agents: Vec<cookie_agent_protocol::AgentDescriptor>,
    ) -> cookie_agent_protocol::RuntimeSnapshotV1 {
        cookie_agent_protocol::RuntimeSnapshotV1 {
            snapshot_schema_version: cookie_agent_protocol::RuntimeSnapshotSchemaVersion::current(),
            recipe_registry_revision: protocol_revision(digit),
            catalog_revision: protocol_revision(digit),
            catalog_source: cookie_agent_protocol::CatalogSource::Network,
            catalog_state: cookie_agent_protocol::CatalogRuntimeState {
                stale: false,
                provider_quarantine_count: 0,
                model_quarantine_count: 0,
                quarantine_digest: Sha256Digest::of_bytes(b"quarantine"),
                last_error: None,
            },
            provider_state_revision: protocol_revision(digit),
            provider_store_generation: cookie_agent_protocol::ProviderStoreGeneration::new(1)
                .expect("store generation"),
            model_revision: protocol_revision(digit),
            agent_revision: protocol_revision(digit),
            runtime_revision: protocol_revision(digit),
            providers,
            models,
            agents,
        }
    }

    fn catalog_model(
        key: &str,
        variants: &[&str],
        default_variant: Option<&str>,
    ) -> cookie_agent_protocol::AvailableModelDescriptor {
        let mut descriptor = model_descriptor();
        descriptor.key = key.parse().expect("model key");
        descriptor.display_name = format!("Catalog {key}");
        descriptor.variants = variants
            .iter()
            .map(|id| cookie_agent_protocol::AvailableVariantDescriptor {
                id: cookie_agent_protocol::VariantId::new(*id).expect("variant"),
                display_name: format!("Variant {id}"),
                origin: cookie_agent_protocol::VariantOrigin::Explicit,
                behavior_fingerprint: Sha256Digest::of_bytes(id.as_bytes()),
            })
            .collect();
        descriptor.default_variant = default_variant
            .map(|id| cookie_agent_protocol::VariantId::new(id).expect("default variant"));
        descriptor
    }

    fn frame_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn rect_text(rows: &[String], rect: Rect) -> String {
        rows[usize::from(rect.y)]
            .chars()
            .skip(usize::from(rect.x))
            .take(usize::from(rect.width))
            .collect()
    }

    async fn test_app() -> App {
        let (client, _requests) = recording_client();
        let mut app = App::new(client).await.expect("test app");
        app.install_initial_runtime(runtime_snapshot(
            "1",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        ));
        app
    }

    // Recording client plumbing (no server): records outbound requests and
    // replays scripted inbound frames.
    struct ScriptedStream {
        incoming: tokio::sync::mpsc::UnboundedReceiver<MessageFrame>,
        sent: tokio::sync::mpsc::UnboundedSender<MessageFrame>,
    }

    #[async_trait]
    impl MessageStream for ScriptedStream {
        async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
            self.sent.send(frame).map_err(|_| TransportError::Closed)
        }

        async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
            Ok(self.incoming.recv().await)
        }
    }

    fn recording_client() -> (Client, Arc<Mutex<Vec<Value>>>) {
        let (_incoming, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
        let (sent, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = recorded.clone();
        tokio::spawn(async move {
            while let Some(frame) = sent_rx.recv().await {
                let value = match frame {
                    MessageFrame::Value(value) => value,
                    MessageFrame::Text(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
                };
                sink.lock().expect("recorded lock").push(value);
            }
        });
        (
            Client::connect_stream(ScriptedStream {
                incoming: incoming_rx,
                sent,
            }),
            recorded,
        )
    }

    fn live_recording_client() -> (
        Client,
        Arc<Mutex<Vec<Value>>>,
        tokio::sync::mpsc::UnboundedSender<MessageFrame>,
    ) {
        let (incoming, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
        let (sent, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = recorded.clone();
        tokio::spawn(async move {
            while let Some(frame) = sent_rx.recv().await {
                let value = match frame {
                    MessageFrame::Value(value) => value,
                    MessageFrame::Text(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
                };
                sink.lock().expect("recorded lock").push(value);
            }
        });
        (
            Client::connect_stream(ScriptedStream {
                incoming: incoming_rx,
                sent,
            }),
            recorded,
            incoming,
        )
    }

    fn recorded_method_count(recorded: &Arc<Mutex<Vec<Value>>>, method: &str) -> usize {
        recorded
            .lock()
            .expect("recorded")
            .iter()
            .filter(|value| value["method"].as_str() == Some(method))
            .count()
    }

    fn assistant_state(children: Vec<AssistantChild>) -> SessionState {
        SessionState {
            transcript: vec![TranscriptItem::Assistant {
                id: 1,
                version: 0,
                attribution: attribution(None),
                committed_turn_seq: Some(1),
                children,
            }],
            ..SessionState::default()
        }
    }

    fn rendered_frame(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect::<String>()
    }

    fn rendered_agent_rows(app: &mut App, width: u16) -> Vec<String> {
        let backend = TestBackend::new(width, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        let buffer = terminal.backend().buffer();
        (1..=3)
            .map(|y| {
                (1..width.saturating_sub(1))
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn text_column(row: &str, text: &str) -> usize {
        let byte = row.find(text).expect("row text");
        row[..byte].chars().count()
    }

    fn chevron_counts(rendered: &str) -> (usize, usize) {
        (rendered.matches('▸').count(), rendered.matches('▾').count())
    }

    // ------------------------------------------------------------------
    // Layout and terminal geometry
    // ------------------------------------------------------------------

    #[test]
    fn terminal_layout_has_exact_rects_for_wide_square_tall_and_tiny_terminals() {
        for (width, height) in [(160, 50), (80, 24), (40, 12), (20, 8)] {
            let layout = terminal_layout_with_tree_rows(Rect::new(0, 0, width, height), 3);
            assert_eq!(layout.agent.y, 0);
            assert_eq!(layout.conversation.y, layout.agent.height);
            assert_eq!(layout.input.height, 5.min(height));
            assert_eq!(layout.input.y + layout.input.height, height);
        }
    }

    #[tokio::test]
    async fn agent_panel_text_rows_are_clamped_1_to_3_with_borders_outside() {
        let mut app = test_app().await;
        for (sessions, expected_rows) in [(0usize, 3u16), (1, 3), (2, 4), (3, 5), (9, 5)] {
            let layout = terminal_layout_with_tree_rows(Rect::new(0, 0, 80, 24), sessions.max(1));
            app.tree = (sessions > 0).then(|| SessionTree {
                session: session_meta(SessionId::new_v7()),
                children: (1..sessions)
                    .map(|_| SessionTree {
                        session: session_meta(SessionId::new_v7()),
                        children: Vec::new(),
                    })
                    .collect(),
            });
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| app.render_tree(frame, layout.agent))
                .expect("render");
            let buffer = terminal.backend().buffer().clone();
            let top = buffer[(0, 0)].symbol() == "┌";
            let bottom = buffer[(0, expected_rows - 1)].symbol() == "└";
            let below = buffer[(0, expected_rows)].symbol() == "└";
            assert!(top, "sessions {sessions}");
            assert!(bottom, "sessions {sessions}");
            assert!(!below, "sessions {sessions}");
        }
    }

    // ------------------------------------------------------------------
    // Transcript rendering: headers, children, tools, chevrons
    // ------------------------------------------------------------------

    #[test]
    fn assistant_header_projects_exact_agent_model_and_variant() {
        assert_eq!(
            attribution(None).header(),
            "primary • gateway/arbitrary-model[base]"
        );
        assert_eq!(
            attribution(Some("high")).header(),
            "primary • gateway/arbitrary-model[high]"
        );
        assert_eq!(
            attribution(Some("default")).header(),
            "primary • gateway/arbitrary-model[default]"
        );
        assert_eq!(attribution(Some("high")).variant_label(), "high");
        assert_eq!(attribution(None).variant_label(), "base");
    }

    #[test]
    fn tiny_header_wraps_frozen_attribution_never_reduces_to_a_tag() {
        let state = assistant_state(vec![AssistantChild::Text {
            id: 1,
            version: 0,
            markdown: MarkdownDocument::new("answer".into()),
        }]);
        for width in [3u16, 6, 12, 24] {
            let layout = transcript_layout(&state, None, width);
            // Render into a real terminal buffer at the exact panel width:
            // every row, including continuation prefixes, must fit.
            let backend =
                TestBackend::new(width, u16::try_from(layout.lines.len().max(1)).unwrap_or(1));
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new(ratatui::text::Text::from(
                            layout.lines.clone(),
                        )),
                        area,
                    );
                })
                .expect("render");
            let buffer = terminal.backend().buffer();
            let mut visible = String::new();
            for y in 0..buffer.area.height {
                let row = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>();
                visible.push_str(row.trim_end());
            }
            let squashed = visible.replace(['│', ' '], "");
            assert!(
                squashed.contains("primary•gateway/arbitrary-model"),
                "width {width}: {visible}"
            );
            assert!(!visible.contains("[A]"), "width {width}");
        }
        let rendered = snapshot_lines(&transcript_layout(&state, None, 80).lines);
        assert!(rendered.contains("╭─ primary • gateway/arbitrary-model[base]"));
    }

    #[test]
    fn assistant_item_renders_one_header_with_inline_children() {
        let state = assistant_state(vec![
            AssistantChild::Thinking {
                id: 1,
                version: 0,
                text: "thinking".into(),
            },
            AssistantChild::Text {
                id: 2,
                version: 0,
                markdown: MarkdownDocument::new("answer".into()),
            },
        ]);
        let layout = transcript_layout(&state, None, 60);
        let rendered = layout
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            rendered
                .matches("╭─ primary • gateway/arbitrary-model[base]")
                .count(),
            1
        );
        assert!(!rendered.contains("ASSISTANT"));
        assert!(!rendered.contains("REASONING"));
        assert!(!rendered.contains("TOOL"));
        let (collapsed, expanded) = chevron_counts(&rendered);
        assert_eq!(expanded, 0);
        assert_eq!(collapsed, 1);
    }

    #[test]
    fn tool_children_render_compact_titles_with_status_semantics() {
        let call_id = ToolCallId::new_v7();
        let mut state = assistant_state(vec![AssistantChild::Tool { call_id }]);
        state.tools.insert(
            call_id,
            ToolCallState {
                id: call_id,
                owner: owner(1, "call-1"),
                presentation: presentation("bash", Some("touch README.md")),
                arguments: r#"{"command": "touch README.md"}"#.into(),
                status: ToolStatus::Running,
                detail: String::new(),
            },
        );
        let rendered = transcript_layout(&state, None, 60)
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered
                .lines()
                .any(|line| line.trim_end().ends_with("▸ bash touch README.md …"))
        );
        assert!(!rendered.contains("COMPLETED"));

        state.tools.get_mut(&call_id).expect("tool").status = ToolStatus::Completed;
        let rendered = transcript_layout(&state, None, 60)
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered
                .lines()
                .any(|line| line.trim_end() == "▸ bash touch README.md"
                    || line.trim_end() == "│ ▸ bash touch README.md")
        );
        assert!(!rendered.contains('…'));
        assert!(!rendered.contains("failed"));

        state.tools.get_mut(&call_id).expect("tool").status = ToolStatus::Failed;
        let rendered = transcript_layout(&state, None, 60)
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("▸ bash touch README.md failed"));
    }

    #[test]
    fn parallel_tool_children_stay_in_committed_order_not_completion_order() {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let first = ToolCallId::new_v7();
        let second = ToolCallId::new_v7();
        let mut store = StateStore::default();
        let events = [
            attempt_started(session, 1, run, attempt, None),
            turn_committed(
                session,
                2,
                run,
                attempt,
                7,
                vec![
                    cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                        id: ModelCallId::new("call-a").expect("call"),
                        provider_item_id: None,
                        name: SafeCode::new("bash").expect("tool"),
                        input: serde_json::json!({"command": "sleep 2"}),
                        raw_input: None,
                        metadata: None,
                    },
                    cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                        id: ModelCallId::new("call-b").expect("call"),
                        provider_item_id: None,
                        name: SafeCode::new("bash").expect("tool"),
                        input: serde_json::json!({"command": "true"}),
                        raw_input: None,
                        metadata: None,
                    },
                ],
                Vec::new(),
                None,
            ),
            tool_started_at(session, 3, run, first, 7, "call-a", 0, "bash", None),
            tool_started_at(session, 4, run, second, 7, "call-b", 1, "bash", None),
            // The second tool terminates first; order must not change.
            tool_terminated(
                session,
                5,
                run,
                second,
                7,
                "call-b",
                cookie_agent_protocol::ToolTerminationOutcome::Completed,
            ),
        ];
        for event in events {
            assert!(store.apply_event(event));
        }
        let state = &store.sessions[&session];
        let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
            panic!("assistant item");
        };
        let tool_order = children
            .iter()
            .filter_map(|child| match child {
                AssistantChild::Tool { call_id } => Some(*call_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_order, vec![first, second]);
    }

    #[test]
    fn tool_rows_project_from_committed_turn_ownership() {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let call_id = ToolCallId::new_v7();
        let mut store = StateStore::default();
        let events = [
            session_created(session, 1),
            attempt_started(session, 2, run, attempt, Some("high")),
            turn_committed(
                session,
                3,
                run,
                attempt,
                3,
                vec![cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                    id: ModelCallId::new("call-1").expect("call"),
                    provider_item_id: None,
                    name: SafeCode::new("bash").expect("tool"),
                    input: serde_json::json!({"command": "git status"}),
                    raw_input: None,
                    metadata: None,
                }],
                Vec::new(),
                Some("high"),
            ),
            tool_started(session, 4, run, call_id, 3, "call-1"),
        ];
        for event in events {
            assert!(store.apply_event(event));
        }
        let state = &store.sessions[&session];
        let tool = &state.tools[&call_id];
        assert_eq!(tool.presentation.title.as_str(), "call-1");
        assert_eq!(tool.arguments, r#"{"command":"git status"}"#);
        let TranscriptItem::Assistant {
            attribution,
            committed_turn_seq,
            ..
        } = &state.transcript[0]
        else {
            panic!("assistant item");
        };
        assert_eq!(*committed_turn_seq, Some(3));
        assert_eq!(
            attribution.header(),
            "primary • gateway/arbitrary-model[high]"
        );
        assert_eq!(attribution.variant_label(), "high");
        assert!(children_has_tool(&state.transcript[0], call_id));
    }

    fn children_has_tool(item: &TranscriptItem, call_id: ToolCallId) -> bool {
        match item {
            TranscriptItem::Assistant { children, .. } => children.iter().any(
                |child| matches!(child, AssistantChild::Tool { call_id: id } if *id == call_id),
            ),
            _ => false,
        }
    }

    // ------------------------------------------------------------------
    // Streaming reduction against protocol-8 events
    // ------------------------------------------------------------------

    #[test]
    fn streaming_deltas_group_under_the_attempt_header() {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let mut store = StateStore::default();
        for event in [
            attempt_started(session, 1, run, attempt, None),
            reasoning_delta(session, 2, run, attempt, "r1"),
            reasoning_delta(session, 3, run, attempt, "+r2"),
            text_delta(session, 4, run, attempt, "t1"),
            reasoning_delta(session, 5, run, attempt, "r3"),
        ] {
            assert!(store.apply_event(event));
        }
        let state = &store.sessions[&session];
        let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
            panic!("assistant item");
        };
        assert!(matches!(
            children.as_slice(),
            [
                AssistantChild::Thinking { text, .. },
                AssistantChild::Text { markdown, .. },
                AssistantChild::Thinking { text: second, .. },
            ] if text == "r1+r2" && markdown.as_str() == "t1" && second == "r3"
        ));
    }

    #[test]
    fn committed_turn_appends_unstreamed_content_in_model_order() {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let mut store = StateStore::default();
        for event in [
            attempt_started(session, 1, run, attempt, None),
            turn_committed(
                session,
                2,
                run,
                attempt,
                1,
                vec![
                    cookie_agent_protocol::PersistedAssistantPart::Text {
                        text: "durable text".into(),
                        metadata: None,
                    },
                    cookie_agent_protocol::PersistedAssistantPart::Reasoning {
                        text: "durable thinking".into(),
                        metadata: None,
                    },
                ],
                vec!["context near limit"],
                None,
            ),
        ] {
            assert!(store.apply_event(event));
        }
        let state = &store.sessions[&session];
        let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
            panic!("assistant item");
        };
        assert!(matches!(
            children.as_slice(),
            [
                AssistantChild::Text { markdown, .. },
                AssistantChild::Thinking { text, .. },
            ] if markdown.as_str() == "durable text" && text == "durable thinking"
        ));
        assert!(state.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Event {
                level: crate::state::EventLevel::Warning,
                text,
                ..
            } if text.contains("context near limit")
        )));
    }

    #[test]
    fn attempts_after_abandonment_start_a_new_item() {
        let session = SessionId::new_v7();
        let run = run_id();
        let first = AttemptId::new_v7();
        let second = AttemptId::new_v7();
        let mut store = StateStore::default();
        for event in [
            attempt_started(session, 1, run, first, None),
            text_delta(session, 2, run, first, "partial"),
            event(
                session,
                3,
                run,
                EventPayload::AttemptAbandoned { attempt_id: first },
            ),
            attempt_started(session, 4, run, second, None),
            text_delta(session, 5, run, second, "final"),
        ] {
            assert!(store.apply_event(event));
        }
        let state = &store.sessions[&session];
        assert!(matches!(
            state.transcript.as_slice(),
            [
                TranscriptItem::Assistant { .. },
                TranscriptItem::Event { .. },
                TranscriptItem::Assistant { .. },
            ]
        ));
    }

    #[test]
    fn variant_changes_between_attempts_reheader_each_item() {
        let session = SessionId::new_v7();
        let run = run_id();
        let base = AttemptId::new_v7();
        let high = AttemptId::new_v7();
        let mut store = StateStore::default();
        for event in [
            session_created(session, 1),
            attempt_started(session, 2, run, base, None),
            text_delta(session, 3, run, base, "base answer"),
            event(
                session,
                4,
                run,
                EventPayload::AttemptAbandoned { attempt_id: base },
            ),
            attempt_started(session, 5, run, high, Some("high")),
            text_delta(session, 6, run, high, "high answer"),
        ] {
            assert!(store.apply_event(event));
        }
        let rendered = transcript_layout(&store.sessions[&session], None, 60)
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            rendered
                .matches("primary • gateway/arbitrary-model[base]")
                .count(),
            1
        );
        assert_eq!(
            rendered
                .matches("primary • gateway/arbitrary-model[high]")
                .count(),
            1
        );
    }

    // ------------------------------------------------------------------
    // Titles, trees, and panels
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn title_events_patch_tree_rows_immediately_and_stale_tree_cannot_overwrite() {
        let mut app = test_app().await;
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        app.tree_root = Some(root);
        app.selected = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: session_meta(child),
                children: Vec::new(),
            }],
        });
        // Immediate patch from the title event.
        app.apply_title_patch(
            child,
            Some(SessionTitle::new("worker done").expect("title")),
            7,
        );
        let entries = app.tree_entries();
        assert!(entries.iter().any(|(id, meta, _)| {
            *id == child
                && meta
                    .title
                    .as_ref()
                    .is_some_and(|t| t.as_str() == "worker done")
        }));
        assert_eq!(app.title_sequences[&child], 7);

        // A stale tree response (title seq 3 < 7) must not overwrite.
        let mut stale = SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: titled_meta(child, "old title", 3),
                children: Vec::new(),
            }],
        };
        app.patch_tree_titles(&mut stale);
        assert_eq!(stale.children[0].session.title_updated_seq, 7);
        assert_eq!(
            stale.children[0]
                .session
                .title
                .as_ref()
                .map(|title| title.as_str()),
            Some("worker done")
        );

        // An older event never patches over a newer one.
        app.apply_title_patch(
            child,
            Some(SessionTitle::new("regression").expect("title")),
            5,
        );
        let entries = app.tree_entries();
        assert!(entries.iter().any(|(id, meta, _)| {
            *id == child
                && meta
                    .title
                    .as_ref()
                    .is_some_and(|t| t.as_str() == "worker done")
        }));

        // A user reset clears the title at a newer sequence.
        app.apply_title_patch(child, None, 9);
        let entries = app.tree_entries();
        assert!(entries.iter().any(|(id, meta, _)| *id == child
            && meta.title.is_none()
            && meta.title_updated_seq == 9));
        let _ = root;
    }

    #[tokio::test]
    async fn agent_tree_rows_are_agent_colon_title() {
        let mut app = test_app().await;
        let root = SessionId::new_v7();
        app.tree = Some(SessionTree {
            session: titled_meta(root, "fix the flaky test", 2),
            children: Vec::new(),
        });
        app.selected = Some(root);
        let entries = app.tree_entries();
        // Primary text is exactly `agent-id:session-title` with no session
        // ID; cursor/watch markers live in prefix cells only.
        let label = app.tree_row_label(&entries[0], false);
        assert_eq!(label, "  ● primary:fix the flaky test");
        let label = app.tree_row_label(&entries[0], true);
        assert_eq!(label, "> ● primary:fix the flaky test");
        let root_id = root.to_string();
        assert!(!label.contains(&root_id));
        assert!(!label.contains(&root_id[..8]));
        // Untitled sessions render the exact untitled placeholder.
        let untitled = SessionId::new_v7();
        app.tree = Some(SessionTree {
            session: session_meta(untitled),
            children: Vec::new(),
        });
        let entries = app.tree_entries();
        assert_eq!(
            app.tree_row_label(&entries[0], false),
            "    primary:untitled"
        );
    }

    #[tokio::test]
    async fn sessions_picker_shows_titles_with_filtering_and_no_results_state() {
        let mut app = test_app().await;
        let first = titled_meta(SessionId::new_v7(), "quarterly report", 1);
        let second = session_meta(SessionId::new_v7());
        app.sessions = vec![first, second];
        assert_eq!(app.filtered_sessions().len(), 2);
        app.picker_query = "quarterly".into();
        assert_eq!(app.filtered_sessions().len(), 1);
        app.picker_query = "primary".into();
        assert_eq!(app.filtered_sessions().len(), 2);
        app.picker_query = "no-such-session".into();
        assert!(app.filtered_sessions().is_empty());
    }

    #[tokio::test]
    async fn watching_a_descendant_keeps_the_stable_root() {
        let mut app = test_app().await;
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        app.tree_root = Some(root);
        app.selected = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: session_meta(child),
                children: Vec::new(),
            }],
        });
        app.watch_session(child);
        assert_eq!(app.tree_root, Some(root));
        assert_eq!(app.selected, Some(child));
        assert!(app.tree.is_some());
        // Watching a session outside the tree is the intentional reroot.
        let outside = SessionId::new_v7();
        app.watch_session(outside);
        assert_eq!(app.tree_root, Some(outside));
        assert!(app.tree.is_none());
    }

    #[tokio::test]
    async fn clicking_child_then_root_preserves_multilevel_tree_depth_and_hit_regions() {
        for width in [16, 40] {
            let mut app = test_app().await;
            let root = SessionId::new_v7();
            let child = SessionId::new_v7();
            let grandchild = SessionId::new_v7();
            app.tree_root = Some(root);
            app.selected = Some(root);
            app.tree_cursor = Some(root);
            app.tree = Some(SessionTree {
                session: titled_meta(root, "root", 1),
                children: vec![SessionTree {
                    session: titled_meta(child, "child", 1),
                    children: vec![SessionTree {
                        session: titled_meta(grandchild, "grandchild", 1),
                        children: Vec::new(),
                    }],
                }],
            });

            let expected = vec![(root, 0), (child, 1), (grandchild, 2)];
            let depths = |app: &App| {
                app.tree_entries()
                    .into_iter()
                    .map(|(session_id, _, depth)| (session_id, depth))
                    .collect::<Vec<_>>()
            };
            let root_selected = rendered_agent_rows(&mut app, width);
            assert_eq!(depths(&app), expected, "width {width}");
            let child_hit = app
                .hit_map
                .tree_rows
                .iter()
                .find(|hit| hit.session_id == child)
                .copied()
                .expect("child row hit");
            let child_expand = child_hit.expand_rect.expect("child expand hit");

            app.handle_click(
                child_hit.rect.x + child_hit.rect.width - 1,
                child_hit.rect.y,
            )
            .await;
            let child_selected = rendered_agent_rows(&mut app, width);
            assert_eq!(app.selected, Some(child));
            assert_eq!(app.tree_root, Some(root));
            assert_eq!(depths(&app), expected, "width {width}");
            let selected_child_expand = app
                .hit_map
                .tree_rows
                .iter()
                .find(|hit| hit.session_id == child)
                .and_then(|hit| hit.expand_rect)
                .expect("selected child expand hit");
            assert_eq!(selected_child_expand, child_expand, "width {width}");

            app.apply_title_patch(
                grandchild,
                Some(SessionTitle::new("updated").expect("title")),
                2,
            );
            let root_hit = app
                .hit_map
                .tree_rows
                .iter()
                .find(|hit| hit.session_id == root)
                .copied()
                .expect("root row hit");
            app.handle_click(root_hit.rect.x + root_hit.rect.width - 1, root_hit.rect.y)
                .await;
            let root_selected_again = rendered_agent_rows(&mut app, width);

            assert_eq!(app.selected, Some(root));
            assert_eq!(app.tree_root, Some(root));
            assert_eq!(depths(&app), expected, "width {width}");
            assert!(
                root_selected[0].starts_with("> ● primary:"),
                "width {width}: {root_selected:?}"
            );
            assert!(root_selected[1].starts_with("  -   primary"));
            assert!(root_selected[2].starts_with("        p"));
            assert!(child_selected[0].starts_with("    primary:"));
            assert!(child_selected[1].starts_with("> - ● primary"));
            assert!(child_selected[2].starts_with("        p"));
            assert!(root_selected_again[1].starts_with("  -   primary"));
            assert!(root_selected_again[2].starts_with("        p"));

            // These columns come from the actual rendered buffer. Selection
            // changes cursor/watch cells only; agent text retains depth 0/1/2.
            assert_eq!(text_column(&root_selected[0], "p"), 4);
            assert_eq!(text_column(&root_selected[1], "p"), 6);
            assert_eq!(text_column(&root_selected[2], "p"), 8);
            assert_eq!(text_column(&child_selected[0], "p"), 4);
            assert_eq!(text_column(&child_selected[1], "p"), 6);
            assert_eq!(text_column(&child_selected[2], "p"), 8);
            assert_eq!(text_column(&root_selected_again[0], "p"), 4);
            assert_eq!(text_column(&root_selected_again[1], "p"), 6);
            assert_eq!(text_column(&root_selected_again[2], "p"), 8);
        }
    }

    // ------------------------------------------------------------------
    // Message title and draft selection
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn message_title_is_exact_agent_model_variant_with_hit_regions() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![model_descriptor()];
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: Some(cookie_agent_protocol::VariantId::new("high").expect("variant")),
            },
        });
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("primary • gateway/arbitrary-model[high]"));
        let segments = &app.hit_map.title_segments;
        assert_eq!(segments.len(), 3);
        let agent_rect = segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Agent)
            .expect("agent segment")
            .rect;
        let model_rect = segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Model)
            .expect("model segment")
            .rect;
        let variant_rect = segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Variant)
            .expect("variant segment")
            .rect;
        assert_eq!(agent_rect.width, 7);
        assert_eq!(model_rect.width, 23);
        assert_eq!(variant_rect.width, 6);
        assert_eq!(model_rect.x, agent_rect.x + agent_rect.width + 3);
        assert_eq!(variant_rect.x, model_rect.x + model_rect.width);
        let bullet_x = agent_rect.x + agent_rect.width + 1;
        assert!(
            segments
                .iter()
                .all(|hit| !hit.rect.contains((bullet_x, agent_rect.y).into())),
            "the bullet must remain decoration"
        );

        let narrow = rendered_frame(&mut app, 28, 12);
        assert!(narrow.contains("primary • gateway/arbit"));
        assert_eq!(app.hit_map.title_segments.len(), 2);
        let narrow_model = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Model)
            .expect("clipped model segment");
        assert_eq!(narrow_model.rect.x, agent_rect.x + agent_rect.width + 3);
        assert_eq!(narrow_model.rect.width, 16);
        assert!(
            app.hit_map
                .title_segments
                .iter()
                .all(|hit| hit.segment != TitleSegment::Variant)
        );
    }

    #[tokio::test]
    async fn scrolled_message_title_hits_follow_visible_cells_or_disappear() {
        async fn app_at_scroll_position(position: usize, width: u16) -> (App, Vec<String>) {
            let mut app = test_app().await;
            app.agents = vec![descriptor("primary", true)];
            app.models = vec![model_descriptor()];
            app.draft = Some(RunSelection {
                agent: agent_id(),
                model: ModelSelection {
                    model: model_key(),
                    variant: None,
                },
            });
            app.input
                .set_buffer("zero\none\ntwo\nthree\nfour\nfive\nsix".into());
            frame_rows(&mut app, width, 24);
            match position {
                0 => app.input.move_buffer_home(),
                1 => {
                    app.input.move_buffer_home();
                    app.input.move_page_down();
                }
                2 => {}
                _ => unreachable!(),
            }
            assert_eq!(app.input.viewport_row(), [0, 1, 4][position]);
            let rows = frame_rows(&mut app, width, 24);
            (app, rows)
        }

        for position in 0..3 {
            let (mut app, rows) = app_at_scroll_position(position, 100).await;
            let agent = app
                .hit_map
                .title_segments
                .iter()
                .find(|hit| hit.segment == TitleSegment::Agent)
                .copied()
                .expect("visible agent");
            let model = app
                .hit_map
                .title_segments
                .iter()
                .find(|hit| hit.segment == TitleSegment::Model)
                .copied()
                .expect("visible model");
            let variant = app
                .hit_map
                .title_segments
                .iter()
                .find(|hit| hit.segment == TitleSegment::Variant)
                .copied()
                .expect("visible variant");
            assert_eq!(rect_text(&rows, agent.rect), "primary");
            assert_eq!(rect_text(&rows, model.rect), "gateway/arbitrary-model");
            assert_eq!(rect_text(&rows, variant.rect), "[base]");

            app.handle_click(agent.rect.x, agent.rect.y).await;
            assert_eq!(app.modal, Modal::Agents);
            assert!(app.draft.as_ref().expect("draft").model.variant.is_none());
            app.modal = Modal::None;
            frame_rows(&mut app, 100, 24);

            app.handle_click(model.rect.x, model.rect.y).await;
            assert_eq!(app.modal, Modal::Models);
            assert!(app.draft.as_ref().expect("draft").model.variant.is_none());
            app.modal = Modal::None;
            frame_rows(&mut app, 100, 24);

            app.handle_click(variant.rect.x, variant.rect.y).await;
            assert_eq!(app.modal, Modal::None);
            assert_eq!(
                app.draft
                    .as_ref()
                    .and_then(|draft| draft.model.variant.as_ref())
                    .map(|variant| variant.as_str()),
                Some("fast")
            );

            let rows = frame_rows(&mut app, 100, 24);
            let agent = app
                .hit_map
                .title_segments
                .iter()
                .find(|hit| hit.segment == TitleSegment::Agent)
                .copied()
                .expect("visible agent");
            let model = app
                .hit_map
                .title_segments
                .iter()
                .find(|hit| hit.segment == TitleSegment::Model)
                .copied()
                .expect("visible model");
            let bullet_x = agent.rect.x + agent.rect.width + 1;
            assert_eq!(
                rect_text(&rows, Rect::new(bullet_x, agent.rect.y, 1, 1)),
                "•"
            );
            app.handle_click(bullet_x, agent.rect.y).await;
            assert_eq!(app.modal, Modal::None);
            assert_eq!(model.rect.x, bullet_x + 2);
            assert_eq!(
                app.draft
                    .as_ref()
                    .and_then(|draft| draft.model.variant.as_ref())
                    .map(|variant| variant.as_str()),
                Some("fast")
            );

            let marker_x = rows[usize::from(agent.rect.y)]
                .chars()
                .position(|character| matches!(character, '↑' | '↓'))
                .and_then(|column| u16::try_from(column).ok())
                .expect("scroll marker");
            app.handle_click(marker_x, agent.rect.y).await;
            assert_eq!(app.modal, Modal::None);
            assert_eq!(
                app.draft
                    .as_ref()
                    .and_then(|draft| draft.model.variant.as_ref())
                    .map(|variant| variant.as_str()),
                Some("fast")
            );
        }

        for position in 0..3 {
            let (mut app, rows) = app_at_scroll_position(position, 28).await;
            let title_y =
                terminal_layout_with_tree_rows(Rect::new(0, 0, 28, 24), app.tree_entries().len())
                    .input
                    .y;
            assert!(app.hit_map.title_segments.is_empty());
            assert!(!rows[usize::from(title_y)].contains("primary"));
            let original_variant = app.draft.as_ref().expect("draft").model.variant.clone();
            for column in [1, 9, 11] {
                app.handle_click(column, title_y).await;
                assert_eq!(app.modal, Modal::None);
                assert_eq!(
                    app.draft.as_ref().expect("draft").model.variant,
                    original_variant
                );
            }
            let marker_x = rows[usize::from(title_y)]
                .chars()
                .position(|character| matches!(character, '↑' | '↓'))
                .and_then(|column| u16::try_from(column).ok())
                .expect("scroll marker");
            app.handle_click(marker_x, title_y).await;
            assert_eq!(app.modal, Modal::None);
            assert_eq!(
                app.draft.as_ref().expect("draft").model.variant,
                original_variant
            );
        }
    }

    #[tokio::test]
    async fn title_segments_open_pickers_and_cycle_variant_by_mouse() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![model_descriptor()];
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
        });
        rendered_frame(&mut app, 100, 30);
        let agent = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Agent)
            .expect("agent segment")
            .rect;
        app.handle_click(agent.x + 1, agent.y).await;
        assert_eq!(app.modal, Modal::Agents);
        app.modal = Modal::None;
        rendered_frame(&mut app, 100, 30);
        let variant = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Variant)
            .expect("variant segment")
            .rect;
        app.handle_click(variant.x + 1, variant.y).await;
        assert_eq!(app.modal, Modal::None);
        assert_eq!(
            app.draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map(|variant| variant.as_str()),
            Some("fast")
        );
    }

    #[tokio::test]
    async fn draft_model_picker_uses_global_catalog_and_variant_cycle_is_inline() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![model_descriptor()];
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
        });
        assert_eq!(app.draft_models().len(), app.models.len());
        let variants = app.draft_variants();
        assert_eq!(variants.len(), 3);
        assert!(variants[0].is_none());
        assert_eq!(variants[1].as_ref().map(|v| v.as_str()), Some("fast"));
        assert_eq!(variants[2].as_ref().map(|v| v.as_str()), Some("high"));

        // Cycling a variant changes only the draft; active runs are frozen.
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.store.sessions.entry(session).or_default().active_run = Some(run_id());
        app.store
            .sessions
            .get_mut(&session)
            .expect("session")
            .run_agent = Some(agent_id());
        frame_rows(&mut app, 80, 24);
        let variant_hit = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Variant)
            .copied()
            .expect("variant hit");
        app.handle_click(variant_hit.rect.x, variant_hit.rect.y)
            .await;
        assert!(app.status.contains("the active run is unchanged"));
        assert_eq!(
            app.active_run_agent().map(|agent| agent.as_str()),
            Some("primary")
        );
    }

    #[tokio::test]
    async fn global_out_of_chain_models_render_and_select_at_normal_and_narrow_widths() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        let mut outside =
            catalog_model("other/catalog-model", &["default", "high"], Some("default"));
        outside.display_name = "Outside".into();
        let mut base = model_descriptor();
        base.display_name = "Base".into();
        app.models = vec![base, outside.clone()];
        app.draft = app.default_draft_selection();
        assert_eq!(
            app.draft_model_labels(),
            vec![
                "gateway/arbitrary-model[base] — Base",
                "other/catalog-model[default] — Outside",
            ]
        );

        for (width, theme) in [
            (100, Theme::new(ThemeKind::Default, ColorLevel::TrueColor)),
            (48, Theme::new(ThemeKind::Mono, ColorLevel::None)),
        ] {
            app.theme = theme;
            app.modal = Modal::Models;
            let rows = frame_rows(&mut app, width, 24);
            if width == 100 {
                assert!(
                    rows.iter()
                        .any(|row| row.contains("other/catalog-model[default] — Outside")),
                    "width {width}: {rows:?}"
                );
                assert!(
                    rows.iter()
                        .any(|row| row.contains("gateway/arbitrary-model[base] — Base")),
                    "width {width}: {rows:?}"
                );
            } else {
                assert!(
                    rows.iter()
                        .any(|row| row.contains("other/catalog-model[de"))
                );
                assert!(
                    rows.iter()
                        .any(|row| row.contains("gateway/arbitrary-mode"))
                );
            }
            assert_eq!(app.hit_map.picker_rows.len(), 2);
        }

        app.choose_picker_entry(1).await;
        let draft = app.draft.as_ref().expect("draft");
        assert_eq!(draft.model.model, outside.key);
        assert_eq!(
            draft.model.variant.as_ref().map(|variant| variant.as_str()),
            Some("default")
        );
    }

    #[tokio::test]
    async fn composer_variant_hit_cycles_lexically_wraps_and_one_entry_is_noop() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![catalog_model(MODEL, &["high", "default", "fast"], None)];
        app.draft = app.default_draft_selection();

        for expected in [Some("default"), Some("fast"), Some("high"), None] {
            let rows = frame_rows(&mut app, 48, 24);
            let hit = app
                .hit_map
                .title_segments
                .iter()
                .find(|hit| hit.segment == TitleSegment::Variant)
                .copied()
                .expect("visible bracketed variant hit");
            let before = app
                .draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map_or("base", |variant| variant.as_str());
            assert!(
                rows.iter().any(|row| row.contains(&format!("[{before}]"))),
                "{rows:?}"
            );
            assert_eq!(hit.rect.width, u16::try_from(before.len() + 2).unwrap());
            app.handle_click(hit.rect.x, hit.rect.y).await;
            assert_eq!(
                app.draft
                    .as_ref()
                    .and_then(|draft| draft.model.variant.as_ref())
                    .map(|variant| variant.as_str()),
                expected
            );
        }

        app.models = vec![catalog_model(MODEL, &[], None)];
        app.revalidate_draft();
        let before = app.draft.clone();
        app.cycle_draft_variant();
        assert_eq!(app.draft, before);
    }

    #[tokio::test]
    async fn model_agent_and_refresh_normalization_preserve_only_valid_draft_parts() {
        let mut app = test_app().await;
        let first = model_descriptor();
        let second = catalog_model("other/catalog-model", &["default", "high"], Some("default"));
        let mut primary = descriptor("primary", true);
        primary.resolved_fallback = vec![ModelSelection {
            model: second.key.clone(),
            variant: Some(cookie_agent_protocol::VariantId::new("high").expect("variant")),
        }];
        let reviewer = descriptor("reviewer", true);
        app.agents = vec![primary, reviewer];
        app.models = vec![first.clone(), second.clone()];
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: first.key.clone(),
                variant: Some(cookie_agent_protocol::VariantId::new("high").expect("variant")),
            },
        });

        app.set_draft_model(first.key.clone());
        assert_eq!(
            app.draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map(|variant| variant.as_str()),
            Some("high"),
            "reselecting the current model preserves its variant"
        );
        app.set_draft_agent(AgentId::new("reviewer").expect("agent"));
        assert_eq!(app.draft.as_ref().expect("draft").model.model, first.key);
        assert_eq!(
            app.draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map(|variant| variant.as_str()),
            Some("high"),
            "agent changes preserve a valid global model selection"
        );

        app.set_draft_model(second.key.clone());
        assert_eq!(
            app.draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map(|variant| variant.as_str()),
            Some("default"),
            "model changes select the model default"
        );
        app.draft.as_mut().expect("draft").model.variant =
            Some(cookie_agent_protocol::VariantId::new("removed").expect("variant"));
        app.revalidate_draft();
        assert_eq!(
            app.draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map(|variant| variant.as_str()),
            Some("default"),
            "a missing variant resets to the model default"
        );

        app.draft.as_mut().expect("draft").agent = agent_id();
        app.draft.as_mut().expect("draft").model.model = "missing/model".parse().expect("model");
        app.revalidate_draft();
        let draft = app.draft.as_ref().expect("draft");
        assert_eq!(draft.model.model, second.key);
        assert_eq!(
            draft.model.variant.as_ref().map(|variant| variant.as_str()),
            Some("high"),
            "missing models prefer the agent's authored available fallback"
        );
    }

    #[tokio::test]
    async fn draft_clicks_do_not_mutate_active_or_committed_frozen_attribution() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![model_descriptor()];
        app.draft = app.default_draft_selection();
        let session = SessionId::new_v7();
        app.selected = Some(session);
        let state = app.store.sessions.entry(session).or_default();
        state.active_run = Some(run_id());
        state.run_agent = Some(agent_id());
        state.transcript = vec![TranscriptItem::Assistant {
            id: 1,
            version: 0,
            attribution: attribution(Some("default")),
            committed_turn_seq: Some(1),
            children: vec![AssistantChild::Text {
                id: 2,
                version: 0,
                markdown: MarkdownDocument::new("frozen".into()),
            }],
        }];

        frame_rows(&mut app, 80, 24);
        let variant_hit = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Variant)
            .copied()
            .expect("variant hit");
        app.handle_click(variant_hit.rect.x, variant_hit.rect.y)
            .await;
        let TranscriptItem::Assistant { attribution, .. } =
            &app.store.sessions[&session].transcript[0]
        else {
            panic!("assistant")
        };
        assert_eq!(
            attribution.header(),
            "primary • gateway/arbitrary-model[default]"
        );
        assert_eq!(app.active_run_agent().map(AgentId::as_str), Some("primary"));
        let rendered = frame_rows(&mut app, 80, 24).join("\n");
        assert!(rendered.contains("primary • gateway/arbitrary-model[default]"));
        assert!(rendered.contains("primary • gateway/arbitrary-model[fast]"));
    }

    #[tokio::test]
    async fn agent_picker_lists_only_root_runnable_agents() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true), descriptor("worker", false)];
        app.models = vec![model_descriptor()];
        assert_eq!(app.selectable_agents().len(), 1);
        let draft = app.default_draft_selection().expect("default draft");
        assert_eq!(draft.agent.as_str(), "primary");
    }

    // ------------------------------------------------------------------
    // Root vs delegated draft-agent selection semantics
    // ------------------------------------------------------------------

    fn delegated_meta(session_id: SessionId, root: SessionId, agent: &str) -> SessionMeta {
        SessionMeta {
            origin: SessionOrigin::Delegated {
                root_session_id: root,
                parent_session_id: root,
                parent_run_id: RunId::new_v7(),
                parent_tool_call_id: ToolCallId::new_v7(),
                invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
                depth: 1,
            },
            creation_selection: RunSelection {
                agent: AgentId::new(agent).expect("agent id"),
                model: ModelSelection {
                    model: model_key(),
                    variant: None,
                },
            },
            ..session_meta(session_id)
        }
    }

    #[tokio::test]
    async fn root_sessions_may_switch_draft_agents_between_runs() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true), descriptor("reviewer", true)];
        app.models = vec![model_descriptor()];
        let root = SessionId::new_v7();
        app.selected = Some(root);
        app.tree_root = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: Vec::new(),
        });
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
        });
        assert!(app.watching_root_session());
        assert!(app.agent_switching_allowed());
        assert!(app.delegated_pin_reason().is_none());
        app.cycle_agent(false);
        assert_eq!(
            app.draft.as_ref().map(|draft| draft.agent.as_str()),
            Some("reviewer")
        );
        // Opening the agent selector works for root sessions.
        app.open_selection_modal(Modal::Agents);
        assert_eq!(app.modal, Modal::Agents);
        app.modal = Modal::None;
        // An active run does not gate drafts: agent switching stays allowed
        // for root sessions and affects the next run only; the producing
        // attribution stays frozen.
        let state = app.store.sessions.entry(root).or_default();
        state.active_run = Some(run_id());
        state.run_agent = Some(agent_id());
        assert!(app.agent_switching_allowed());
        app.open_selection_modal(Modal::Agents);
        assert_eq!(app.modal, Modal::Agents);
        app.modal = Modal::None;
        app.set_draft_agent(AgentId::new("reviewer").expect("agent"));
        assert!(app.status.contains("next run"));
        assert_eq!(
            app.active_run_agent().map(|agent| agent.as_str()),
            Some("primary")
        );
        // Model selection and inline variant cycling stay available during the run too.
        app.open_selection_modal(Modal::Models);
        assert_eq!(app.modal, Modal::Models);
        app.modal = Modal::None;
        app.cycle_draft_variant();
        assert!(app.status.contains("active run is unchanged"));
    }

    #[tokio::test]
    async fn delegated_sessions_pin_the_frozen_child_agent_with_textual_reason() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true), descriptor("worker", false)];
        app.models = vec![model_descriptor()];
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        app.tree_root = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: delegated_meta(child, root, "worker"),
                children: Vec::new(),
            }],
        });
        // The persisted `SessionCreated` carries the exact frozen chain the
        // delegated pickers must derive from.
        assert!(app.store.apply_event(session_created_with(
            child,
            1,
            "worker",
            vec![resolved_model(None)],
            0,
        )));
        app.set_selected_session(child);
        assert!(!app.watching_root_session());
        assert!(!app.agent_switching_allowed());
        assert_eq!(
            app.delegated_pin_reason().as_deref(),
            Some("delegated session pinned to frozen child agent worker")
        );
        // The agent selector is disabled with a clear non-color reason.
        app.open_selection_modal(Modal::Agents);
        assert_eq!(app.modal, Modal::None);
        assert!(app.status.contains("pinned to frozen child agent worker"));
        // Tab cycling is refused the same way.
        app.cycle_agent(false);
        assert!(app.status.contains("pinned to frozen child agent worker"));
        // Choosing an entry from a stale open modal is rejected too.
        app.modal = Modal::Agents;
        app.choose_picker_entry(0).await;
        assert!(app.status.contains("pinned to frozen child agent worker"));
        app.modal = Modal::None;
        // Model selection stays available for the delegated session within
        // its frozen suffix; inline variant cycling is a fixed no-op.
        app.open_selection_modal(Modal::Models);
        assert_eq!(app.modal, Modal::Models);
        app.choose_picker_entry(0).await;
        assert_eq!(app.modal, Modal::None);
        assert_eq!(app.draft_variants(), vec![None]);
        app.cycle_draft_variant();
        assert!(
            app.draft
                .as_ref()
                .is_some_and(|draft| draft.model.variant.is_none())
        );
        // The Agents modal presents the pinned agent as fixed when rendered.
        app.modal = Modal::Agents;
        let rendered = rendered_frame(&mut app, 140, 30);
        assert!(rendered.contains("fixed (delegated session)"));
        assert!(rendered.contains("pinned to frozen child agent worker"));
    }

    #[tokio::test]
    async fn descriptor_revisions_are_coherent_and_refresh_revalidates_root_drafts_only() {
        let mut app = test_app().await;
        // Revision coherence: agent and model snapshot revisions travel
        // together in selector presentation.
        app.agent_revision = Some(protocol_revision("1"));
        app.model_revision = Some(protocol_revision("2"));
        let label = app.descriptor_revisions_label();
        assert!(label.contains("agent revision sha256:1111"));
        assert!(label.contains("model revision sha256:2222"));

        // A root draft pointing at a now-unrunnable agent resets to the
        // default; a delegated session's pin is untouched.
        app.agents = vec![descriptor("reviewer", true)];
        app.models = vec![model_descriptor()];
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: delegated_meta(child, root, "worker"),
                children: Vec::new(),
            }],
        });
        app.selected = Some(root);
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
        });
        app.revalidate_draft();
        assert_eq!(
            app.draft.as_ref().map(|draft| draft.agent.as_str()),
            Some("reviewer")
        );
        app.selected = Some(child);
        app.draft = Some(RunSelection {
            agent: AgentId::new("worker").expect("agent"),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
        });
        app.revalidate_draft();
        assert_eq!(
            app.draft.as_ref().map(|draft| draft.agent.as_str()),
            Some("worker")
        );
    }

    #[tokio::test]
    async fn watched_session_change_rebinds_the_draft_without_carrying_root_into_child() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true), descriptor("reviewer", true)];
        app.models = vec![model_descriptor()];
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: delegated_meta(child, root, "reviewer"),
                children: Vec::new(),
            }],
        });
        // Root: draft rebinds to the root's own creation selection.
        app.set_selected_session(root);
        assert_eq!(
            app.draft.as_ref().map(|draft| draft.agent.as_str()),
            Some("primary")
        );
        // Change the root draft, then watch the child: the child draft is
        // pinned to its frozen agent — the root draft never carries down.
        app.set_draft_agent(AgentId::new("reviewer").expect("agent"));
        assert!(app.store.apply_event(session_created_with(
            child,
            1,
            "reviewer",
            vec![resolved_model(None)],
            0,
        )));
        app.set_selected_session(child);
        let draft = app.draft.as_ref().expect("child draft");
        assert_eq!(draft.agent.as_str(), "reviewer");
        assert_eq!(draft.model.model, model_key());
        // The exact selection is valid against the frozen agent's chain, so
        // run.start would be accepted.
        assert!(app.agents.iter().any(|agent| {
            agent.id == draft.agent
                && agent
                    .resolved_fallback
                    .iter()
                    .any(|selection| selection == &draft.model)
        }));
        // Back at the root, the draft rebinds to the root creation selection.
        app.set_selected_session(root);
        assert_eq!(
            app.draft.as_ref().map(|draft| draft.agent.as_str()),
            Some("primary")
        );
    }

    #[tokio::test]
    async fn empty_chain_inherited_child_uses_the_persisted_frozen_suffix() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![model_descriptor()];
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: delegated_meta(child, root, "worker"),
                children: Vec::new(),
            }],
        });
        // An empty-chain child inherits the invoking parent's active frozen
        // suffix at admission: here the suffix begins at the second chain
        // entry, so only the suffix head is selectable.
        let inherited_head = resolved_model(Some("high"));
        let inherited_next = resolved_model(Some("fast"));
        assert!(app.store.apply_event(session_created_with(
            child,
            1,
            "worker",
            vec![resolved_model(None), inherited_head, inherited_next],
            1,
        )));
        app.set_selected_session(child);
        let models = app.draft_models();
        assert_eq!(models.len(), 2, "only the inherited frozen suffix");
        assert_eq!(
            models[0].variant.as_ref().map(|variant| variant.as_str()),
            Some("high")
        );
        assert_eq!(
            models[1].variant.as_ref().map(|variant| variant.as_str()),
            Some("fast")
        );
        // The draft is pinned to the suffix head even though the creation
        // selection names an earlier chain entry.
        let draft = app.draft.as_ref().expect("delegated draft");
        assert_eq!(draft.agent.as_str(), "worker");
        assert_eq!(
            draft.model.variant.as_ref().map(|variant| variant.as_str()),
            Some("high")
        );
        // Model picking stays within the persisted chain: an out-of-chain
        // model is rejected, a chain member is accepted exactly.
        app.set_draft_model(model_key());
        assert!(
            app.status.contains("not in agent worker's fallback chain")
                || app
                    .draft
                    .as_ref()
                    .is_some_and(|draft| draft.model.variant.is_some())
        );
        // Live descriptor changes never reinterpret the persisted chain.
        app.agents.clear();
        let models = app.draft_models();
        assert_eq!(models.len(), 2);
        // A full replay keeps the persisted projection authoritative.
        let mut store = StateStore::default();
        assert!(store.apply_event(session_created_with(
            child,
            1,
            "worker",
            vec![resolved_model(None)],
            0,
        )));
        let state = &store.sessions[&child];
        assert_eq!(
            state
                .creation_agent
                .as_ref()
                .map(|snapshot| snapshot.fallback_chain.len()),
            Some(1)
        );
    }

    fn run_started_with_suffix(
        session_id: SessionId,
        seq: u64,
        run: RunId,
        suffix: Vec<cookie_agent_protocol::ResolvedModelRef>,
    ) -> StoredEvent {
        let snapshot_chain = suffix
            .iter()
            .cloned()
            .map(frozen_binding)
            .collect::<Vec<_>>();
        let selection = RunSelection {
            agent: agent_id(),
            model: suffix[0].selection.clone(),
        };
        event(
            session_id,
            seq,
            run,
            EventPayload::RunStarted {
                client_run_id: cookie_agent_protocol::ClientRunId::new("run-1").expect("run id"),
                selection: selection.clone(),
                agent: Box::new(cookie_agent_protocol::AgentSnapshot {
                    agent: agent_id(),
                    schema: cookie_agent_protocol::AgentSchemaVersion::current(),
                    mode: cookie_agent_protocol::AgentMode::Primary,
                    description: "Test primary agent".into(),
                    document_source: cookie_agent_protocol::AgentDocumentSource::Workspace,
                    document_fingerprint: Sha256Digest::of_bytes(b"document"),
                    composed_prompt: "You are the primary test agent.\n".into(),
                    prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
                    tools: Vec::new(),
                    permissions: Vec::new(),
                    delegation: None,
                    fallback_chain: snapshot_chain.clone(),
                    selected_suffix_start: 0,
                }),
                runtime_revision: protocol_revision("1"),
                catalog_revision: protocol_revision("2"),
                provider_state_revision: protocol_revision("3"),
                model_revision: protocol_revision("4"),
                agent_revision: protocol_revision("5"),
                recipe_registry_revision: protocol_revision("6"),
                manifest_revision: protocol_revision("7"),
                selected_suffix: snapshot_chain,
                input_through_seq: seq,
            },
        )
    }

    #[tokio::test]
    async fn selected_suffix_head_variant_override_persists_through_replay() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![model_descriptor()];
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: delegated_meta(child, root, "worker"),
                children: Vec::new(),
            }],
        });
        let run = run_id();
        // The creation chain resolves the head to base; the run selection
        // overrode the head to the exact `high` variant. The authoritative
        // suffix carries the override directly.
        assert!(app.store.apply_event(session_created_with(
            child,
            1,
            "worker",
            vec![resolved_model(None), resolved_model(Some("fast"))],
            0,
        )));
        assert!(app.store.apply_event(run_started_with_suffix(
            child,
            2,
            run,
            vec![resolved_model(Some("high")), resolved_model(Some("fast"))],
        )));
        app.set_selected_session(child);
        // The delegated pickers derive from the exact persisted suffix: the
        // overridden head variant, never the reconstructed base.
        let models = app.draft_models();
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0].variant.as_ref().map(|variant| variant.as_str()),
            Some("high")
        );
        let draft = app.draft.as_ref().expect("delegated draft");
        assert_eq!(
            draft.model.variant.as_ref().map(|variant| variant.as_str()),
            Some("high")
        );
        // Variant cycling for the selected head is fixed to its one exact
        // persisted selection, not live descriptor variants.
        assert_eq!(
            app.draft_variants()
                .iter()
                .map(|variant| variant.as_ref().map(|id| id.as_str()))
                .collect::<Vec<_>>(),
            vec![Some("high")]
        );
        // A full replay keeps the exact suffix authoritative.
        let mut store = StateStore::default();
        assert!(store.apply_event(session_created_with(
            child,
            1,
            "worker",
            vec![resolved_model(None), resolved_model(Some("fast"))],
            0,
        )));
        assert!(store.apply_event(run_started_with_suffix(
            child,
            2,
            run,
            vec![resolved_model(Some("high")), resolved_model(Some("fast"))],
        )));
        let suffix = store.sessions[&child]
            .run_selected_suffix
            .as_ref()
            .expect("persisted selected suffix");
        assert_eq!(
            suffix[0]
                .selection
                .variant
                .as_ref()
                .map(|variant| variant.as_str()),
            Some("high")
        );
    }

    #[tokio::test]
    async fn delegated_variant_cycle_is_immune_to_live_provider_refresh() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![model_descriptor()];
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: delegated_meta(child, root, "worker"),
                children: Vec::new(),
            }],
        });
        let run = run_id();
        assert!(app.store.apply_event(session_created_with(
            child,
            1,
            "worker",
            vec![resolved_model(Some("high"))],
            0,
        )));
        assert!(app.store.apply_event(run_started_with_suffix(
            child,
            2,
            run,
            vec![resolved_model(Some("high"))],
        )));
        app.set_selected_session(child);
        let before = app.draft_variants();
        assert_eq!(
            before
                .iter()
                .map(|variant| variant.as_ref().map(|id| id.as_str()))
                .collect::<Vec<_>>(),
            vec![Some("high")]
        );
        // A provider refresh adds brand-new live variants; the delegated
        // cycle still exposes only the persisted exact selection.
        let mut refreshed = model_descriptor();
        refreshed
            .variants
            .push(cookie_agent_protocol::AvailableVariantDescriptor {
                id: cookie_agent_protocol::VariantId::new("ultra").expect("variant"),
                display_name: "Ultra".into(),
                origin: cookie_agent_protocol::VariantOrigin::Explicit,
                behavior_fingerprint: Sha256Digest::of_bytes(b"ultra"),
            });
        app.models = vec![refreshed];
        let after = app.draft_variants();
        assert_eq!(after, before);
        app.cycle_draft_variant();
        assert_eq!(app.draft_variants(), before);
    }

    // ------------------------------------------------------------------
    // Approvals
    // ------------------------------------------------------------------

    #[test]
    fn approval_content_shows_identity_resources_and_constraints() {
        let session = SessionId::new_v7();
        let state = approval(session);
        let content = approval_content(&state);
        for needle in [
            "PERMISSION REQUIRED · ESCALATED",
            "git status",
            "operation fingerprint",
            "CAPABILITIES (1)",
            "RESOURCES (1)",
            "EVALUATIONS (1)",
            "RESPONSE CONSTRAINTS",
        ] {
            assert!(content.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn escalated_only_visibility_and_optimistic_response_identity() {
        let session = SessionId::new_v7();
        let mut state = approval(session);
        assert!(state.is_visible_user_escalation());
        state.escalated = false;
        assert!(!state.is_visible_user_escalation());
    }

    // ------------------------------------------------------------------
    // Scrollbar geometry and scrolling
    // ------------------------------------------------------------------

    fn tall_transcript_state(lines: usize) -> SessionState {
        SessionState {
            transcript: (0..lines)
                .map(|index| TranscriptItem::Event {
                    id: index as u64 + 1,
                    version: 0,
                    level: crate::state::EventLevel::Warning,
                    text: format!("line {index}"),
                })
                .collect(),
            ..SessionState::default()
        }
    }

    #[test]
    fn scrollbar_geometry_maps_track_ends_to_the_exact_offset_range() {
        let track = Rect::new(10, 2, 1, 10);
        let geometry = ScrollbarGeometry::resolve(track, 100).expect("geometry");
        assert_eq!(geometry.max_offset, 90);
        assert_eq!(geometry.thumb_top(0), 0);
        assert_eq!(
            geometry.thumb_top(90) + geometry.thumb_size(),
            usize::from(track.height)
        );
        assert_eq!(geometry.offset_for_track_row(track.y), 0);
        assert_eq!(
            geometry.clamp_offset(geometry.offset_for_track_row(track.y + track.height - 1)),
            90
        );
    }

    #[test]
    fn thumb_height_is_constant_across_top_middle_and_bottom() {
        let track = Rect::new(0, 0, 1, 12);
        let geometry = ScrollbarGeometry::resolve(track, 120).expect("geometry");
        let size = geometry.thumb_size();
        for offset in [0, 27, 54, 108] {
            assert_eq!(geometry.thumb_size(), size);
            let _ = geometry.with_thumb(offset);
        }
    }

    #[test]
    fn thumb_drag_round_trips_offsets_at_constant_height() {
        let track = Rect::new(0, 0, 1, 12);
        let geometry = ScrollbarGeometry::resolve(track, 120).expect("geometry");
        let tolerance = (geometry.max_offset / usize::from(track.height)) + 2;
        for offset in [0, 13, 54, 108] {
            let top = geometry.thumb_top(offset);
            let round_trip = geometry.clamp_offset(
                geometry.offset_for_thumb_anchor(u16::try_from(top).expect("row"), 0),
            );
            assert!(
                (round_trip as i64 - offset as i64).unsigned_abs() as usize <= tolerance,
                "offset {offset} round-tripped to {round_trip}"
            );
        }
    }

    #[test]
    fn conversation_scroll_reengages_following_at_the_exact_bottom() {
        let mut scroll = ConversationScroll::default();
        scroll.up(5);
        scroll.clamp(200, 10);
        assert!(!scroll.following);
        scroll.scroll_to(ConversationScroll::max_offset(200, 10));
        scroll.clamp(200, 10);
        assert!(scroll.following);
    }

    #[tokio::test]
    async fn scrollbar_is_reserved_drawn_and_pages_on_track_press() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.tree_root = Some(session);
        app.store
            .sessions
            .insert(session, tall_transcript_state(300));
        rendered_frame(&mut app, 80, 50);
        let track = app.hit_map.scrollbar.expect("scrollbar reserved");
        assert_eq!(track.width, 1);
        let geometry = app.scrollbar_geometry.expect("geometry");
        assert!((1..=track.height).contains(&geometry.thumb.height));
        app.handle_click(track.x, track.y + track.height - 1).await;
        assert!(app.conversation_scroll.offset > 0);
    }

    // ------------------------------------------------------------------
    // Input, palette, and commands
    // ------------------------------------------------------------------

    #[test]
    fn slash_commands_parse_and_escape_prompts() {
        assert_eq!(
            parse_submission("/quit").expect("quit"),
            Submission::Command(SlashCommand::Quit)
        );
        assert_eq!(
            parse_submission("//literal /quit").expect("escaped"),
            Submission::Prompt("/literal /quit".into())
        );
        assert_eq!(
            parse_submission("line one\n/quit").expect("multiline"),
            Submission::Prompt("line one\n/quit".into())
        );
        assert!(parse_submission("/nope").is_err());
    }

    #[test]
    fn newline_keys_insert_and_bare_enter_submits() {
        let newline = |key: KeyEvent| {
            matches!(
                (key.code, key.modifiers),
                (
                    KeyCode::Enter,
                    KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT
                ) | (KeyCode::Char('j'), KeyModifiers::CONTROL)
            )
        };
        assert!(newline(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)));
        assert!(newline(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
        assert!(newline(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL
        )));
        assert!(!newline(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    }

    #[test]
    fn no_ctrl_p_b_n_block_shortcuts_are_registered() {
        // Block navigation lives behind /block only; Ctrl-P/B/N remain plain
        // text input characters handled by the input editor.
        assert!(command_spec("block").is_some());
        assert_eq!(
            parse_submission("/block next").expect("block next"),
            Submission::Command(SlashCommand::Block(BlockCommand::Next))
        );
        for key in ['p', 'b', 'n'] {
            let event = KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL);
            assert!(matches!(event.code, KeyCode::Char(_)));
        }
    }

    #[test]
    fn command_registry_drives_help_and_parser() {
        let help = command_help();
        assert!(help.contains("/new"));
        assert!(help.contains("/connect"));
        assert!(help.contains("/events"));
        assert!(help.contains("/block"));
        assert!(command_allowed_in_mode(
            SlashCommand::Scroll(ScrollCommand::Top),
            InputMode::Message
        ));
        assert!(!command_allowed_in_mode(
            SlashCommand::Eof,
            InputMode::Message
        ));
    }

    async fn submit_direct_command(app: &mut App, command: &str) {
        type_input(app, command).await;
        if app.command_palette_visible() {
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .await;
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
    }

    async fn settle_recording() {
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
    }

    async fn wait_for_method(recorded: &Arc<Mutex<Vec<Value>>>, method: &str, count: usize) {
        for _ in 0..100 {
            if recorded_method_count(recorded, method) == count {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(recorded_method_count(recorded, method), count, "{method}");
    }

    #[tokio::test]
    async fn command_palette_no_matches_renders_empty_state_then_reports_unknown_command() {
        let mut app = test_app().await;
        type_input(&mut app, "/definitely-not-a-command").await;
        assert!(app.command_palette_visible());

        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Commands"));
        assert!(rendered.contains("No matching commands"));
        assert!(app.hit_map.palette.is_some());
        assert!(app.hit_map.palette_rows.is_empty());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(app.input.as_str().is_empty());
        assert!(app.status.contains("unknown command"));
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("unknown command"));
    }

    #[tokio::test]
    async fn empty_agent_and_model_selectors_are_truthful_and_safe() {
        let mut agents = test_app().await;
        agents.agents.clear();
        submit_direct_command(&mut agents, "/new").await;
        assert_eq!(agents.modal, Modal::Agents);
        let rendered = rendered_frame(&mut agents, 100, 30);
        assert!(rendered.contains("No root-runnable agents are available."));
        assert!(!rendered.contains("Backspace or Ctrl-U clears the filter"));
        assert!(agents.hit_map.picker_rows.is_empty());
        agents
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(agents.modal, Modal::Agents);
        agents
            .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert_eq!(agents.modal, Modal::None);

        let mut models = test_app().await;
        models.agents = vec![descriptor("primary", true)];
        models.models.clear();
        models.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
        });
        rendered_frame(&mut models, 100, 30);
        let model_hit = models
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Model)
            .copied()
            .expect("model title hit");
        models
            .handle_click(model_hit.rect.x, model_hit.rect.y)
            .await;
        assert_eq!(models.modal, Modal::Models);
        let rendered = rendered_frame(&mut models, 100, 30);
        assert!(rendered.contains("No models are available for this draft."));
        assert!(!rendered.contains("Backspace or Ctrl-U clears the filter"));
        assert!(models.hit_map.picker_rows.is_empty());
        models
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(models.modal, Modal::Models);
        models
            .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert_eq!(models.modal, Modal::None);
    }

    #[tokio::test]
    async fn every_slash_command_variant_dispatches_from_key_events_without_starting_a_run() {
        let cases = [
            ("/quit", InputMode::Message, None),
            ("/new", InputMode::Message, Some("Agent")),
            ("/connect", InputMode::Message, Some("Connect provider")),
            ("/sessions", InputMode::Message, Some("Sessions")),
            ("/cancel", InputMode::Message, Some("no active run")),
            (
                "/stdin",
                InputMode::Message,
                Some("no running interactive tool"),
            ),
            (
                "/stdin next",
                InputMode::Message,
                Some("no running interactive tool"),
            ),
            ("/eof", InputMode::ToolStdin, None),
            ("/message", InputMode::ToolStdin, Some("message mode")),
            ("/watch", InputMode::Message, Some("no session selected")),
            ("/tree up", InputMode::Message, None),
            ("/tree down", InputMode::Message, None),
            ("/tree toggle", InputMode::Message, None),
            ("/approve once", InputMode::Message, None),
            ("/approve tree", InputMode::Message, None),
            ("/approve reject", InputMode::Message, None),
            ("/approve cancel", InputMode::Message, None),
            ("/scroll up", InputMode::Message, None),
            ("/scroll down 2", InputMode::Message, None),
            ("/scroll top", InputMode::Message, None),
            ("/scroll bottom", InputMode::Message, None),
            ("/block next", InputMode::Message, None),
            ("/block previous", InputMode::Message, None),
            ("/block toggle", InputMode::Message, None),
            ("/block clear", InputMode::Message, None),
            (
                "/events debug",
                InputMode::Message,
                Some("diagnostic event filter"),
            ),
            (
                "/events info",
                InputMode::Message,
                Some("diagnostic event filter"),
            ),
            (
                "/events warning",
                InputMode::Message,
                Some("diagnostic event filter"),
            ),
            (
                "/events error",
                InputMode::Message,
                Some("diagnostic event filter"),
            ),
            ("/help", InputMode::Message, Some("Commands:")),
        ];

        for (command, mode, expected) in cases {
            let mut app = test_app().await;
            let (client, recorded, incoming_guard) = live_recording_client();
            app.client = client;
            app.input_mode = mode;
            submit_direct_command(&mut app, command).await;
            settle_recording().await;
            let rendered = rendered_frame(&mut app, 100, 30);
            assert!(!rendered.is_empty(), "{command}");
            if let Some(expected) = expected {
                assert!(rendered.contains(expected), "{command}: {rendered}");
            }
            assert_eq!(
                recorded_method_count(&recorded, "run.start"),
                0,
                "{command}"
            );
            assert_eq!(
                recorded_method_count(&recorded, "run.steer"),
                0,
                "{command}"
            );
            drop(incoming_guard);
        }
    }

    #[tokio::test]
    async fn command_palette_mouse_activation_uses_the_same_local_dispatch() {
        let mut app = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        app.agents.clear();
        type_input(&mut app, "/ne").await;
        rendered_frame(&mut app, 100, 30);
        let row = app
            .hit_map
            .palette_rows
            .first()
            .copied()
            .expect("new palette row");
        app.handle_click(row.rect.x, row.rect.y).await;
        assert_eq!(app.modal, Modal::Agents);
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("No root-runnable agents are available."));
        settle_recording().await;
        assert_eq!(recorded_method_count(&recorded, "run.start"), 0);
        assert_eq!(recorded_method_count(&recorded, "run.steer"), 0);
        drop(incoming_guard);
    }

    #[tokio::test]
    async fn rpc_slash_commands_issue_only_their_intended_methods() {
        let mut cancel = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        cancel.client = client;
        let session = SessionId::new_v7();
        cancel.selected = Some(session);
        cancel.store.sessions.entry(session).or_default().active_run = Some(run_id());
        submit_direct_command(&mut cancel, "/cancel").await;
        wait_for_method(&recorded, "run.cancel", 1).await;
        assert_eq!(recorded_method_count(&recorded, "run.cancel"), 1);
        assert_eq!(recorded_method_count(&recorded, "run.start"), 0);
        assert_eq!(recorded_method_count(&recorded, "run.steer"), 0);
        drop(incoming_guard);

        let mut watch = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        watch.client = client;
        let session = SessionId::new_v7();
        watch.tree = Some(SessionTree {
            session: session_meta(session),
            children: Vec::new(),
        });
        watch.tree_cursor = Some(session);
        submit_direct_command(&mut watch, "/watch").await;
        wait_for_method(&recorded, "events.subscribe", 1).await;
        assert_eq!(watch.selected, Some(session));
        assert_eq!(recorded_method_count(&recorded, "run.start"), 0);
        assert_eq!(recorded_method_count(&recorded, "run.steer"), 0);
        drop(incoming_guard);

        let mut eof = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        eof.client = client;
        let session = SessionId::new_v7();
        let call_id = ToolCallId::new_v7();
        let mut state = SessionState {
            active_run: Some(run_id()),
            ..SessionState::default()
        };
        state.tools.insert(
            call_id,
            ToolCallState {
                id: call_id,
                owner: owner(1, "call-1"),
                presentation: presentation("bash", None),
                arguments: "{}".into(),
                status: ToolStatus::Running,
                detail: String::new(),
            },
        );
        eof.selected = Some(session);
        eof.store.sessions.insert(session, state);
        eof.input_mode = InputMode::ToolStdin;
        submit_direct_command(&mut eof, "/eof").await;
        wait_for_method(&recorded, "run.tool_stdin", 1).await;
        assert_eq!(recorded_method_count(&recorded, "run.tool_stdin"), 1);
        assert_eq!(recorded_method_count(&recorded, "run.start"), 0);
        assert_eq!(recorded_method_count(&recorded, "run.steer"), 0);
        drop(incoming_guard);

        for command in [
            "/approve once",
            "/approve tree",
            "/approve reject",
            "/approve cancel",
        ] {
            let mut app = test_app().await;
            let (client, recorded, incoming_guard) = live_recording_client();
            app.client = client;
            let approval = bash_approval_state();
            app.selected = Some(approval.session_id);
            app.store
                .sessions
                .entry(approval.session_id)
                .or_default()
                .approvals
                .push(approval);
            submit_direct_command(&mut app, command).await;
            wait_for_method(&recorded, "approval.respond", 1).await;
            assert_eq!(
                recorded_method_count(&recorded, "approval.respond"),
                1,
                "{command}"
            );
            assert_eq!(
                recorded_method_count(&recorded, "run.start"),
                0,
                "{command}"
            );
            assert_eq!(
                recorded_method_count(&recorded, "run.steer"),
                0,
                "{command}"
            );
            drop(incoming_guard);
        }
    }

    // ------------------------------------------------------------------
    // Markdown, themes, and diagnostics
    // ------------------------------------------------------------------

    #[test]
    fn event_badges_render_textually_for_every_level_and_theme() {
        for level in [
            crate::state::EventLevel::Debug,
            crate::state::EventLevel::Info,
            crate::state::EventLevel::Warning,
            crate::state::EventLevel::Error,
        ] {
            let state = SessionState {
                transcript: vec![TranscriptItem::Event {
                    id: 1,
                    version: 0,
                    level,
                    text: "diagnostic".into(),
                }],
                ..SessionState::default()
            };
            let rendered = transcript_layout_with_level(
                &state,
                None,
                60,
                &Theme::default(),
                &PlainHighlighter,
                crate::state::EventLevel::Debug,
            )
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
            assert!(rendered.contains(level.badge()), "{}", level.name());
        }
    }

    #[test]
    fn event_threshold_hides_lower_levels_without_removing_them_from_state() {
        let state = SessionState {
            transcript: vec![
                TranscriptItem::Event {
                    id: 1,
                    version: 0,
                    level: crate::state::EventLevel::Debug,
                    text: "debug row".into(),
                },
                TranscriptItem::Event {
                    id: 2,
                    version: 0,
                    level: crate::state::EventLevel::Error,
                    text: "error row".into(),
                },
            ],
            ..SessionState::default()
        };
        let rendered = transcript_layout_with_level(
            &state,
            None,
            60,
            &Theme::default(),
            &PlainHighlighter,
            crate::state::EventLevel::Warning,
        )
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert!(!rendered.contains("debug row"));
        assert!(rendered.contains("error row"));
        assert_eq!(state.transcript.len(), 2);
    }

    #[test]
    fn markdown_tables_and_inline_code_render_inside_the_gutter() {
        let state = assistant_state(vec![AssistantChild::Text {
            id: 1,
            version: 0,
            markdown: MarkdownDocument::new(
                "text with `inline code`\n\n| a | b |\n|---|---|\n| 1 | 2 |".to_owned(),
            ),
        }]);
        let layout = transcript_layout(&state, None, 50);
        let rendered = layout
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("inline code"));
        assert!(rendered.contains("a"));
        assert!(rendered.contains("b"));
        assert!(layout.lines.iter().all(|line| {
            unicode_width::UnicodeWidthStr::width(line.to_string().as_str()) <= 50
        }));
    }

    #[test]
    fn inline_code_spans_have_a_background_highlight() {
        let state = assistant_state(vec![AssistantChild::Text {
            id: 1,
            version: 0,
            markdown: MarkdownDocument::new("use `cargo test` here".to_owned()),
        }]);
        let layout = transcript_layout_with(&state, None, 60, &Theme::default(), &PlainHighlighter);
        let code_span = layout
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("cargo test"))
            .expect("inline code span");
        assert!(code_span.style.bg.is_some());
    }

    // ------------------------------------------------------------------
    // Read highlighting
    // ------------------------------------------------------------------

    fn read_tool_state(path: &str, status: ToolStatus, detail: &str) -> SessionState {
        let call_id = ToolCallId::new_v7();
        let mut state = assistant_state(vec![AssistantChild::Tool { call_id }]);
        state.tools.insert(
            call_id,
            ToolCallState {
                id: call_id,
                owner: owner(1, "call-1"),
                presentation: presentation("read", None),
                arguments: format!(r#"{{"path": "{path}"}}"#),
                status,
                detail: detail.into(),
            },
        );
        state
    }

    fn expanded_read_layout(state: &SessionState, theme: &Theme) -> Vec<Line<'static>> {
        let call_id = read_tool_id(state);
        let expanded = std::collections::HashSet::from([BlockId::Tool(call_id)]);
        transcript_layout_with(
            state,
            Some(&expanded),
            80,
            theme,
            &crate::markdown::SyntectHighlighter::default(),
        )
        .lines
    }

    fn read_tool_id(state: &SessionState) -> ToolCallId {
        state
            .tools
            .keys()
            .next()
            .copied()
            .expect("read tool present")
    }

    #[test]
    fn read_rust_output_is_syntax_highlighted_with_tool_gutter_preserved() {
        let state = read_tool_state(
            "src/main.rs",
            ToolStatus::Completed,
            "Read 2 lines\nfn main() {\n    let x = 1;\n}",
        );
        let lines = expanded_read_layout(&state, &Theme::default());
        let rendered = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| line.contains("fn main() {")));
        // Highlighting produces more than one distinct foreground color.
        let colors = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter_map(|span| span.style.fg)
            .collect::<std::collections::HashSet<_>>();
        assert!(colors.len() > 1);
    }

    #[test]
    fn read_errors_and_non_read_tools_stay_plain() {
        let state = read_tool_state("src/main.rs", ToolStatus::Failed, "permission denied");
        let lines = expanded_read_layout(&state, &Theme::default());
        let failure_colors = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains("permission denied"))
            .filter_map(|span| span.style.fg)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(failure_colors.len(), 1, "{failure_colors:?}");
        let mut bash = read_tool_state("src/main.rs", ToolStatus::Completed, "fn main() {}");
        bash.tools.values_mut().next().expect("tool").presentation = presentation("bash", None);
        let lines = expanded_read_layout(&bash, &Theme::default());
        // A non-read tool never gets read highlighting: content spans share
        // the single tool-success foreground.
        let content_colors = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains("fn main"))
            .filter_map(|span| span.style.fg)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(content_colors.len(), 1);
    }

    #[test]
    fn read_path_extension_parsing_is_deterministic() {
        assert_eq!(
            read_path_extension(r#"{"path": "src/main.rs"}"#),
            Some("rs")
        );
        assert_eq!(read_path_extension(r#"{"path": "README"}"#), None);
        assert_eq!(read_path_extension("not json"), None);
    }

    // ------------------------------------------------------------------
    // Mouse, hit regions, and block navigation
    // ------------------------------------------------------------------

    #[test]
    fn block_hit_rects_are_clipped_and_shifted_by_scroll_offset() {
        let region = BlockRegion {
            id: BlockId::Thinking(1),
            start_line: 10,
            end_line: 20,
        };
        let viewport = Rect::new(0, 0, 40, 5);
        let hit = block_hit(region, viewport, 8).expect("hit");
        assert_eq!(hit.rect.y, 2);
        assert_eq!(hit.rect.height, 3);
        assert!(block_hit(region, viewport, 25).is_none());
    }

    #[tokio::test]
    async fn mouse_clicks_focus_blur_and_toggle_blocks() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.tree_root = Some(session);
        app.store.sessions.insert(
            session,
            assistant_state(vec![AssistantChild::Thinking {
                id: 1,
                version: 0,
                text: "thought".into(),
            }]),
        );
        rendered_frame(&mut app, 80, 24);
        let block = app.hit_map.blocks.first().copied().expect("block hit");
        app.handle_click(block.rect.x, block.rect.y).await;
        assert!(
            app.expanded_blocks
                .get(&session)
                .is_some_and(|set| set.contains(&block.id))
        );
        assert!(!app.input_focused);
    }

    #[tokio::test]
    async fn block_navigation_flattens_children_in_render_order() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        app.selected = Some(session);
        let call = ToolCallId::new_v7();
        let mut state = assistant_state(vec![
            AssistantChild::Thinking {
                id: 1,
                version: 0,
                text: "one".into(),
            },
            AssistantChild::Tool { call_id: call },
            AssistantChild::Thinking {
                id: 2,
                version: 0,
                text: "two".into(),
            },
        ]);
        state.tools.insert(
            call,
            ToolCallState {
                id: call,
                owner: owner(1, "call-1"),
                presentation: presentation("bash", None),
                arguments: "{}".into(),
                status: ToolStatus::Running,
                detail: String::new(),
            },
        );
        app.store.sessions.insert(session, state);
        app.run_block_command(BlockCommand::Next);
        assert_eq!(app.selected_block, Some(BlockId::Thinking(1)));
        app.run_block_command(BlockCommand::Next);
        assert_eq!(app.selected_block, Some(BlockId::Tool(call)));
        app.run_block_command(BlockCommand::Next);
        assert_eq!(app.selected_block, Some(BlockId::Thinking(2)));
        app.run_block_command(BlockCommand::Previous);
        assert_eq!(app.selected_block, Some(BlockId::Tool(call)));
        app.run_block_command(BlockCommand::Clear);
        assert_eq!(app.selected_block, None);
    }

    // ------------------------------------------------------------------
    // Connect flow
    // ------------------------------------------------------------------

    fn catalog_provider() -> cookie_agent_protocol::ProviderDescriptor {
        provider_descriptor("acme-ai", "supported", "current", false)
    }

    async fn type_input(app: &mut App, text: &str) {
        for character in text.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .await;
        }
    }

    #[tokio::test]
    async fn connect_submission_renders_the_provider_panel_with_an_empty_catalog() {
        let mut app = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        app.providers.clear();
        app.selected = Some(SessionId::new_v7());
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
        });

        type_input(&mut app, "/connect").await;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;

        assert_eq!(app.modal, Modal::ConnectProviders);
        assert!(app.input.as_str().is_empty());
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Connect provider — type to filter"));
        assert!(rendered.contains("No providers are available in the runtime snapshot."));
        assert!(app.hit_map.picker.is_some());
        assert!(app.hit_map.picker_rows.is_empty());
        tokio::task::yield_now().await;
        assert_eq!(recorded_method_count(&recorded, "run.start"), 0);
        assert_eq!(recorded_method_count(&recorded, "run.steer"), 0);
        drop(incoming_guard);

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.picker_query, "x");
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Connect provider — filter: x"));
    }

    #[tokio::test]
    async fn connect_palette_submission_opens_and_focuses_provider_search() {
        let mut app = test_app().await;
        app.providers = vec![catalog_provider()];

        type_input(&mut app, "/connect").await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;

        assert_eq!(app.modal, Modal::ConnectProviders);
        type_input(&mut app, "api_key").await;
        assert_eq!(app.picker_query, "api_key");
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Connect provider — filter: api_key"));
        assert!(rendered.contains("acme-ai provider (acme-ai)"));
        assert!(rendered.contains("credential: API key"));
        assert!(app.hit_map.picker.is_some());
        assert_eq!(app.hit_map.picker_rows.len(), 1);

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .await;
        type_input(&mut app, "missing").await;
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Connect provider — filter: missing"));
        assert!(rendered.contains("No matches."));
        assert!(app.hit_map.picker.is_some());
        assert!(app.hit_map.picker_rows.is_empty());
    }

    #[tokio::test]
    async fn credential_inputs_wipe_on_cancel_and_app_drop() {
        let before = credential_wipe_count();
        {
            let mut app = test_app().await;
            app.begin_provider_form(catalog_provider());
            app.modal = Modal::ConnectCredentials;
            app.provider_form.as_mut().expect("provider form").secrets[0]
                .input
                .insert_owned("sentinel-secret".to_owned());
            app.clear_connect_secrets();
            assert!(app.provider_form.is_none());
        }
        assert!(credential_wipe_count() > before);
    }

    #[tokio::test]
    async fn provider_picker_filter_matches_credential_labels() {
        let mut app = test_app().await;
        app.providers = vec![catalog_provider()];
        app.picker_query = "api_key".into();
        assert_eq!(app.filtered_providers().len(), 1);
        app.picker_query = "unknown".into();
        assert!(app.filtered_providers().is_empty());
    }

    #[tokio::test]
    async fn empty_runtime_uses_exact_message_buffer_has_no_title_hits_and_blocks_rpcs() {
        let mut app = test_app().await;
        app.runtime = crate::state::RuntimeState::default();
        app.install_initial_runtime(runtime_snapshot(
            "2",
            vec![catalog_provider()],
            Vec::new(),
            Vec::new(),
        ));
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        app.selected = Some(SessionId::new_v7());

        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains(crate::state::EMPTY_RUNTIME_GUIDANCE));
        assert!(app.hit_map.title_segments.is_empty());

        type_input(&mut app, "ordinary text").await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.status, crate::state::EMPTY_RUNTIME_GUIDANCE);
        settle_recording().await;
        for method in ["session.create", "run.start", "run.steer"] {
            assert_eq!(recorded_method_count(&recorded, method), 0, "{method}");
        }

        type_input(&mut app, "/connect").await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectProviders);
        let rendered = rendered_frame(&mut app, 160, 30);
        assert!(rendered.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
        drop(incoming_guard);
    }

    #[tokio::test]
    async fn provider_picker_renders_all_required_row_states() {
        let mut app = test_app().await;
        let unsupported = provider_descriptor("a-unsupported", "unsupported", "current", false);
        let disconnected = provider_descriptor("b-disconnected", "supported", "current", false);
        let connected = provider_descriptor("c-connected", "supported", "current", true);
        let removed = provider_descriptor("d-removed", "supported", "removed", true);
        let error = provider_descriptor("e-error", "supported", "current", false);
        let progress = provider_descriptor("f-progress", "supported", "current", false);
        let mut authored = provider_descriptor("g-authored", "supported", "current", false);
        authored.configuration = cookie_agent_protocol::ProviderConfigurationState::Authored;
        authored.effective_auth_state = cookie_agent_protocol::EffectiveAuthState::AuthoredApiKey;
        let mut quarantined = provider_descriptor("h-quarantined", "supported", "current", false);
        quarantined.support.state = cookie_agent_protocol::ProviderSupportState::Quarantined;
        quarantined.support.reason = Some(SafeCode::new("invalid_recipe").expect("reason"));
        quarantined.quarantine = Some(cookie_agent_protocol::QuarantineDiagnostic {
            code: SafeCode::new("invalid_recipe").expect("code"),
            message: SafeErrorMessage::new("recipe was quarantined").expect("message"),
        });
        app.providers = vec![
            unsupported,
            disconnected,
            connected,
            removed,
            error.clone(),
            progress.clone(),
            authored,
            quarantined,
        ];
        app.provider_operations.insert(
            error.id.clone(),
            ProviderOperation::Error {
                action: ProviderAction::Connect,
                message: "retryable".into(),
            },
        );
        app.provider_operations.insert(
            progress.id.clone(),
            ProviderOperation::InProgress(ProviderAction::Connect),
        );
        app.modal = Modal::ConnectProviders;
        let rendered = rendered_frame(&mut app, 220, 40);
        for text in [
            "unsupported: unsupported_environment",
            "disconnected",
            "connected · Enter: reconnect/update",
            "removed from current catalog",
            "error · Enter: retry · retryable",
            "connect in progress",
            "g-authored provider (g-authored) — disconnected",
            "quarantined: invalid_recipe",
        ] {
            assert!(rendered.contains(text), "missing {text}: {rendered}");
        }
        assert!(rendered.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
    }

    #[tokio::test]
    async fn reconnect_prefills_public_setup_but_keeps_secret_fields_blank() {
        let mut app = test_app().await;
        app.begin_provider_form(provider_descriptor(
            "connected-provider",
            "supported",
            "current",
            true,
        ));
        let form = app.provider_form.as_ref().expect("provider form");
        assert!(form.reconnect);
        assert_eq!(form.setup[0].input.as_str(), "us-east-1");
        assert!(form.secrets[0].input.as_str().is_empty());

        let public = rendered_frame(&mut app, 100, 30);
        assert!(public.contains("PUBLIC SETUP"));
        assert!(public.contains("us-east-1"));
        app.modal = Modal::ConnectCredentials;
        let secret = rendered_frame(&mut app, 100, 30);
        assert!(secret.contains("SECRET CREDENTIALS"));
        assert!(secret.contains("reconnect fields are always blank"));
        assert!(!secret.contains("us-east-1"));
    }

    #[tokio::test]
    async fn setup_and_secret_validation_fail_before_provider_rpc() {
        let mut app = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        app.begin_provider_form(catalog_provider());
        let form = app.provider_form.as_mut().expect("provider form");
        form.setup[0].input.set_buffer("x".into());
        form.secrets[0].input.insert_owned("secret".into());
        app.dispatch_provider_connect();
        assert!(app.status.contains("Invalid public setup"));
        settle_recording().await;
        assert_eq!(recorded_method_count(&recorded, "provider.connect"), 0);

        app.begin_provider_form(catalog_provider());
        app.dispatch_provider_connect();
        assert!(app.status.contains("Invalid credentials"));
        settle_recording().await;
        assert_eq!(recorded_method_count(&recorded, "provider.connect"), 0);
        drop(incoming_guard);
    }

    #[tokio::test]
    async fn unsupported_enter_is_details_only_and_never_connects() {
        let mut app = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        app.providers = vec![provider_descriptor(
            "unsupported-provider",
            "unsupported",
            "current",
            false,
        )];
        app.modal = Modal::ConnectProviders;
        app.picker_state.select(Some(0));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectDetails);
        let details = rendered_frame(&mut app, 100, 30);
        assert!(details.contains("Typed reason: unsupported_environment"));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectDetails);
        settle_recording().await;
        assert_eq!(recorded_method_count(&recorded, "provider.connect"), 0);
        drop(incoming_guard);
    }

    #[tokio::test]
    async fn supported_removed_provider_reconnects_through_setup_and_secret_workflow() {
        let current_catalog = production_openai_catalog('a', false);
        let harness = production_provider_harness(Arc::clone(&current_catalog), |_| {});
        let client = Client::connect_in_process(Arc::clone(&harness.server));
        client.handshake().await.expect("production handshake");
        let current = client.runtime_snapshot().await.expect("current runtime");
        let mut initial_form = ProviderForm::new(current.snapshot.providers[0].clone(), false)
            .expect("initial provider form");
        initial_form.secrets[0]
            .input
            .insert_owned("stored-secret".to_owned());
        client
            .connect_provider(cookie_agent_protocol::ProviderConnectParams {
                provider_id: ProviderId::new("openai").expect("provider ID"),
                expected_catalog_revision: current.snapshot.catalog_revision,
                setup_values: initial_form.setup_values().expect("initial setup"),
                auth_method: initial_form.auth_method.clone(),
                auth_values: initial_form.auth_values().expect("initial credentials"),
                client_connect_id: cookie_agent_protocol::ClientConnectId::new(
                    "tui-connect-before-removal",
                )
                .expect("connect ID"),
            })
            .await
            .expect("initial provider connect");
        initial_form.wipe_secrets();
        let removed_catalog = production_empty_catalog('b');
        harness
            .engine
            .refresh_catalog(Arc::clone(&removed_catalog))
            .expect("catalog churn");
        let removed = client.runtime_snapshot().await.expect("removed runtime");
        let projected = &removed.snapshot.providers[0];
        assert_eq!(
            projected.presence,
            cookie_agent_protocol::ProviderPresence::Removed
        );
        assert_eq!(
            projected.support.state,
            cookie_agent_protocol::ProviderSupportState::Supported
        );
        assert!(projected.durable_connection.is_some());
        let generation_before = removed.snapshot.provider_store_generation;

        let mut app = test_app().await;
        app.client = client.clone();
        app.runtime = crate::state::RuntimeState::default();
        app.install_initial_runtime(removed.snapshot);
        type_input(&mut app, "/connect").await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectProviders);
        app.picker_state.select(Some(0));

        let picker = rendered_frame(&mut app, 160, 36);
        assert!(picker.contains("removed from current catalog · Enter: reconnect/update"));
        assert!(picker.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectCredentials);
        let credentials = rendered_frame(&mut app, 160, 36);
        assert!(credentials.contains("SECRET CREDENTIALS"));
        assert!(credentials.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
        type_input(&mut app, "removed-secret").await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectConfirm);
        let confirm = rendered_frame(&mut app, 140, 32);
        assert!(confirm.contains("Action: reconnect/update"));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("reconnect update timeout")
            .expect("reconnect update");
        app.handle_rpc_update(update);
        let reconnected = client
            .runtime_snapshot()
            .await
            .expect("reconnected runtime");
        assert!(reconnected.snapshot.provider_store_generation > generation_before);
        let projected = &reconnected.snapshot.providers[0];
        assert_eq!(
            projected.presence,
            cookie_agent_protocol::ProviderPresence::Removed
        );
        assert_eq!(
            projected.support.state,
            cookie_agent_protocol::ProviderSupportState::Supported
        );
        assert_eq!(
            projected.effective_auth_state,
            cookie_agent_protocol::EffectiveAuthState::ProviderStore
        );
        app.abort_connect_work();
    }

    #[tokio::test]
    async fn unsupported_removed_provider_is_typed_details_only() {
        let catalog = production_empty_catalog('c');
        let store_catalog = Arc::clone(&catalog);
        let harness = production_provider_harness(Arc::clone(&catalog), move |store| {
            install_unmatched_openai_connection(store, &store_catalog);
        });
        let client = Client::connect_in_process(Arc::clone(&harness.server));
        client.handshake().await.expect("production handshake");
        let runtime = client.runtime_snapshot().await.expect("unmatched runtime");
        let projected = &runtime.snapshot.providers[0];
        assert_eq!(
            projected.presence,
            cookie_agent_protocol::ProviderPresence::Removed
        );
        assert_eq!(
            projected.support.state,
            cookie_agent_protocol::ProviderSupportState::Unsupported
        );
        assert_eq!(
            projected.support.reason.as_ref().map(SafeCode::as_str),
            Some("removed_without_retained_recipe_match")
        );
        let generation_before = runtime.snapshot.provider_store_generation;

        let mut app = test_app().await;
        app.client = client.clone();
        app.runtime = crate::state::RuntimeState::default();
        app.install_initial_runtime(runtime.snapshot);
        app.modal = Modal::ConnectProviders;
        app.picker_state.select(Some(0));
        let picker = rendered_frame(&mut app, 140, 32);
        assert!(picker.contains("removed · unsupported: removed_without_retained_recipe_match"));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectDetails);
        let details = rendered_frame(&mut app, 140, 32);
        assert!(details.contains("Presence: Removed"));
        assert!(details.contains("Typed reason: removed_without_retained_recipe_match"));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectDetails);
        let after = client.runtime_snapshot().await.expect("unchanged runtime");
        assert_eq!(after.snapshot.provider_store_generation, generation_before);
    }

    #[tokio::test]
    async fn catalog_shape_does_not_quarantine_provider() {
        let catalog = production_openai_catalog('e', true);
        let harness = production_provider_harness(catalog, |_| {});
        let client = Client::connect_in_process(Arc::clone(&harness.server));
        client.handshake().await.expect("production handshake");
        let runtime = client.runtime_snapshot().await.expect("family runtime");
        let projected = &runtime.snapshot.providers[0];
        assert_eq!(
            projected.support.state,
            cookie_agent_protocol::ProviderSupportState::Supported
        );
        assert_eq!(
            projected.support.reason.as_ref().map(SafeCode::as_str),
            None
        );

        let mut app = test_app().await;
        app.client = client.clone();
        app.runtime = crate::state::RuntimeState::default();
        app.install_initial_runtime(runtime.snapshot);
        app.modal = Modal::ConnectProviders;
        app.picker_state.select(Some(0));
        let picker = rendered_frame(&mut app, 140, 32);
        assert!(picker.contains("disconnected"));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectCredentials);
        let after = client.runtime_snapshot().await.expect("unchanged runtime");
        assert_eq!(
            after.snapshot.provider_store_generation,
            app.runtime
                .snapshot()
                .expect("installed runtime")
                .provider_store_generation
        );
    }

    #[tokio::test]
    async fn authored_incomplete_provider_is_disconnected_and_opens_public_setup() {
        let mut app = test_app().await;
        let mut provider =
            provider_descriptor("authored-incomplete", "supported", "current", false);
        provider.configuration = cookie_agent_protocol::ProviderConfigurationState::Authored;
        provider.effective_auth_state = cookie_agent_protocol::EffectiveAuthState::AuthoredApiKey;
        provider.setup_fields[0].default = None;
        app.providers = vec![provider];
        app.modal = Modal::ConnectProviders;
        app.picker_state.select(Some(0));
        let picker = rendered_frame(&mut app, 140, 32);
        assert!(
            picker.contains("authored-incomplete provider (authored-incomplete) — disconnected")
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectSetup);
        let setup = rendered_frame(&mut app, 160, 32);
        assert!(setup.contains("Provider public setup"));
        assert!(setup.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
        assert!(
            app.provider_form
                .as_ref()
                .is_some_and(|form| form.setup[0].input.as_str().is_empty())
        );
    }

    #[tokio::test]
    async fn complete_authored_override_is_disconnected_and_can_create_global_connection() {
        let mut app = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        let mut provider = provider_descriptor("gateway", "supported", "current", false);
        provider.configuration = cookie_agent_protocol::ProviderConfigurationState::Authored;
        provider.effective_auth_state = cookie_agent_protocol::EffectiveAuthState::AuthoredOverride;
        provider.setup_fields[0].default = None;
        app.providers = vec![provider];
        app.modal = Modal::ConnectProviders;
        app.picker_state.select(Some(0));
        let picker = rendered_frame(&mut app, 220, 32);
        assert!(picker.contains(
            "gateway provider (gateway) — disconnected · config override active · Enter: create global stored connection"
        ));
        assert!(!picker.contains("https://"));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectSetup);
        let form = app.provider_form.as_ref().expect("form");
        assert!(!form.reconnect);
        assert!(!form.can_disconnect);
        let setup = rendered_frame(&mut app, 140, 32);
        assert!(!setup.contains("Ctrl-D disconnect"));
        assert!(!setup.contains("https://"));
        type_input(&mut app, "us-east-1").await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectCredentials);
        type_input(&mut app, "rotated-authored-secret").await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectConfirm);
        let confirm = rendered_frame(&mut app, 140, 32);
        assert!(confirm.contains("Action: connect"));
        assert!(!confirm.contains("Action: reconnect/update"));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        settle_recording().await;
        assert_eq!(recorded_method_count(&recorded, "provider.connect"), 1);
        assert_eq!(recorded_method_count(&recorded, "provider.disconnect"), 0);
        let request = recorded
            .lock()
            .expect("recorded")
            .iter()
            .find(|value| value["method"] == "provider.connect")
            .cloned()
            .expect("connect request");
        assert_eq!(request["params"]["provider_id"], "gateway");
        assert_eq!(request["params"]["setup_values"]["region"], "us-east-1");
        assert_eq!(
            request["params"]["auth_values"]["api_key"],
            "rotated-authored-secret"
        );
        app.abort_connect_work();
        drop(incoming_guard);
    }

    #[tokio::test]
    async fn reconnect_and_disconnect_emit_only_protocol8_provider_methods() {
        let mut app = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        let provider = provider_descriptor("connected-provider", "supported", "current", true);
        app.begin_provider_form(provider.clone());
        app.provider_form.as_mut().expect("form").secrets[0]
            .input
            .insert_owned("sentinel-secret".into());
        app.dispatch_provider_connect();
        settle_recording().await;
        assert_eq!(recorded_method_count(&recorded, "provider.connect"), 1);
        let connect = recorded
            .lock()
            .expect("recorded")
            .iter()
            .find(|value| value["method"] == "provider.connect")
            .cloned()
            .expect("connect request");
        assert_eq!(connect["params"]["setup_values"]["region"], "us-east-1");
        assert_eq!(
            connect["params"]["auth_values"]["api_key"],
            "sentinel-secret"
        );
        app.abort_connect_work();

        app.provider_operations.remove(&provider.id);
        app.providers = vec![provider];
        app.modal = Modal::ConnectProviders;
        app.picker_state.select(Some(0));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectSetup);
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.modal, Modal::DisconnectConfirm);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        settle_recording().await;
        assert_eq!(recorded_method_count(&recorded, "provider.disconnect"), 1);
        for legacy in [
            "model.list",
            "agent.list",
            "catalog.provider.list",
            "catalog.model.list",
        ] {
            assert_eq!(recorded_method_count(&recorded, legacy), 0, "{legacy}");
        }
        app.abort_connect_work();
        drop(incoming_guard);
    }

    #[tokio::test]
    async fn runtime_notifications_are_predecessor_monotonic_and_keep_active_runs_frozen() {
        let mut app = test_app().await;
        let initial = app.runtime.revision().cloned().expect("initial runtime");
        let session = SessionId::new_v7();
        let run = run_id();
        app.selected = Some(session);
        app.store.sessions.insert(
            session,
            SessionState {
                active_run: Some(run),
                run_agent: Some(agent_id()),
                ..SessionState::default()
            },
        );
        let newer = runtime_snapshot(
            "2",
            vec![catalog_provider()],
            vec![model_descriptor()],
            vec![descriptor("reviewer", true)],
        );
        assert!(app.install_runtime_notification(
            cookie_agent_protocol::RuntimeChangedNotification {
                previous_revision: Some(initial),
                snapshot: newer.clone(),
                reasons: vec![cookie_agent_protocol::RuntimeChangeReason::ConfigReloaded],
            }
        ));
        let installed = app.runtime.revision().cloned().expect("new runtime");
        let stale = runtime_snapshot("3", Vec::new(), Vec::new(), Vec::new());
        assert!(!app.install_runtime_notification(
            cookie_agent_protocol::RuntimeChangedNotification {
                previous_revision: None,
                snapshot: stale,
                reasons: vec![cookie_agent_protocol::RuntimeChangeReason::Startup],
            }
        ));
        assert_eq!(app.runtime.revision(), Some(&installed));
        assert_eq!(app.active_run_agent().map(AgentId::as_str), Some("primary"));
    }

    #[tokio::test]
    async fn catalog_refresh_and_fallback_notifications_update_durable_state_monotonically() {
        let mut app = test_app().await;
        let ready_revision = app.runtime.revision().cloned().expect("ready runtime");
        let mut stale = runtime_snapshot(
            "9",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        );
        stale.catalog_source = cookie_agent_protocol::CatalogSource::Cache;
        stale.catalog_state.stale = true;
        stale.catalog_state.last_error = Some(cookie_agent_protocol::CatalogSafeErrorMeta {
            code: SafeCode::new("network_unavailable").expect("code"),
            message: SafeErrorMessage::new("catalog refresh failed").expect("message"),
            time: Timestamp::now(),
        });
        assert!(app.install_runtime_notification(
            cookie_agent_protocol::RuntimeChangedNotification {
                previous_revision: Some(ready_revision.clone()),
                snapshot: stale,
                reasons: vec![cookie_agent_protocol::RuntimeChangeReason::CatalogFallback],
            }
        ));
        assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Stale);
        let stale_revision = app.runtime.revision().cloned().expect("stale runtime");
        let stale_buffer = rendered_frame(&mut app, 160, 30);
        assert!(stale_buffer.contains("Using stale catalog cache"));

        let refreshed = runtime_snapshot(
            "a",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        );
        assert!(app.install_runtime_notification(
            cookie_agent_protocol::RuntimeChangedNotification {
                previous_revision: Some(stale_revision),
                snapshot: refreshed,
                reasons: vec![cookie_agent_protocol::RuntimeChangeReason::CatalogRefreshed],
            }
        ));
        assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Ready);
        assert!(app.runtime.durable_explanation().is_none());
        let refreshed_revision = app.runtime.revision().cloned().expect("refreshed runtime");

        let mut old_fallback = runtime_snapshot(
            "b",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        );
        old_fallback.catalog_source = cookie_agent_protocol::CatalogSource::Cache;
        old_fallback.catalog_state.stale = true;
        assert!(!app.install_runtime_notification(
            cookie_agent_protocol::RuntimeChangedNotification {
                previous_revision: Some(ready_revision),
                snapshot: old_fallback,
                reasons: vec![cookie_agent_protocol::RuntimeChangeReason::CatalogFallback],
            }
        ));
        assert_eq!(app.runtime.revision(), Some(&refreshed_revision));
        assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Ready);

        let mut bootstrap = runtime_snapshot(
            "c",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        );
        bootstrap.catalog_source = cookie_agent_protocol::CatalogSource::Bootstrap;
        bootstrap.catalog_state.stale = true;
        assert!(app.install_runtime_notification(
            cookie_agent_protocol::RuntimeChangedNotification {
                previous_revision: Some(refreshed_revision),
                snapshot: bootstrap,
                reasons: vec![cookie_agent_protocol::RuntimeChangeReason::CatalogFallback],
            }
        ));
        assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Bootstrap);
        let bootstrap_buffer = rendered_frame(&mut app, 160, 30);
        assert!(bootstrap_buffer.contains("Using bundled bootstrap catalog"));
    }

    #[tokio::test]
    async fn coherent_connect_runtime_restores_draft_or_preserves_empty_exactly() {
        let mut app = test_app().await;
        app.runtime = crate::state::RuntimeState::default();
        app.install_initial_runtime(runtime_snapshot(
            "4",
            vec![catalog_provider()],
            Vec::new(),
            Vec::new(),
        ));
        let baseline = app.runtime.revision().cloned();
        app.apply_provider_mutation_outcome(ProviderMutationOutcome::Connected {
            provider_id: cookie_agent_protocol::ProviderId::new("acme-ai").expect("provider"),
            baseline,
            runtime: Box::new(runtime_snapshot(
                "5",
                vec![provider_descriptor("acme-ai", "supported", "current", true)],
                vec![model_descriptor()],
                vec![descriptor("primary", true)],
            )),
        });
        assert!(app.draft.is_some());
        let ready = rendered_frame(&mut app, 100, 30);
        assert!(ready.contains("primary • gateway/arbitrary-model[base]"));
        assert_eq!(app.hit_map.title_segments.len(), 3);

        let baseline = app.runtime.revision().cloned();
        app.apply_provider_mutation_outcome(ProviderMutationOutcome::Connected {
            provider_id: cookie_agent_protocol::ProviderId::new("acme-ai").expect("provider"),
            baseline,
            runtime: Box::new(runtime_snapshot(
                "6",
                vec![provider_descriptor("acme-ai", "supported", "current", true)],
                Vec::new(),
                Vec::new(),
            )),
        });
        assert!(app.runtime.is_empty());
        assert!(app.draft.is_none());
        assert_eq!(app.status, crate::state::EMPTY_RUNTIME_GUIDANCE);
        let empty = rendered_frame(&mut app, 100, 30);
        assert!(empty.contains(crate::state::EMPTY_RUNTIME_GUIDANCE));
        assert!(app.hit_map.title_segments.is_empty());
    }

    #[tokio::test]
    async fn loading_stale_bootstrap_and_error_retry_have_distinct_durable_ui() {
        let mut app = test_app().await;
        app.runtime = crate::state::RuntimeState::default();
        app.draft = None;
        let loading = rendered_frame(&mut app, 100, 30);
        assert!(loading.contains("loading runtime snapshot"));
        assert!(app.hit_map.title_segments.is_empty());

        let mut stale = runtime_snapshot(
            "7",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        );
        stale.catalog_source = cookie_agent_protocol::CatalogSource::Cache;
        stale.catalog_state.stale = true;
        stale.catalog_state.last_error = Some(cookie_agent_protocol::CatalogSafeErrorMeta {
            code: SafeCode::new("network_unavailable").expect("code"),
            message: SafeErrorMessage::new("catalog refresh failed").expect("message"),
            time: Timestamp::now(),
        });
        app.install_initial_runtime(stale);
        assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Stale);
        let stale_ui = rendered_frame(&mut app, 160, 30);
        assert!(stale_ui.contains("Using stale catalog cache"));
        app.modal = Modal::Models;
        let stale_modal = rendered_frame(&mut app, 160, 30);
        assert!(stale_modal.contains("Using stale catalog cache"));

        app.runtime = crate::state::RuntimeState::default();
        let mut bootstrap = runtime_snapshot(
            "8",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        );
        bootstrap.catalog_source = cookie_agent_protocol::CatalogSource::Bootstrap;
        bootstrap.catalog_state.stale = true;
        app.install_initial_runtime(bootstrap);
        assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Bootstrap);
        let bootstrap_ui = rendered_frame(&mut app, 160, 30);
        assert!(bootstrap_ui.contains("Using bundled bootstrap catalog"));

        app.runtime = crate::state::RuntimeState::default();
        app.runtime.set_error("runtime unavailable");
        app.draft = None;
        app.modal = Modal::None;
        let error = rendered_frame(&mut app, 100, 30);
        assert!(error.contains("runtime error — retry"));
        assert!(error.contains("runtime unavailable"));
    }

    // ------------------------------------------------------------------
    // Rendering safety at all widths/themes
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn full_app_degrades_safely_on_tiny_terminals() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.tree_root = Some(session);
        app.store.sessions.insert(
            session,
            assistant_state(vec![
                AssistantChild::Thinking {
                    id: 1,
                    version: 0,
                    text: "thought".into(),
                },
                AssistantChild::Text {
                    id: 2,
                    version: 0,
                    markdown: MarkdownDocument::new("answer".into()),
                },
            ]),
        );
        for (width, height) in [(8, 4), (12, 6), (20, 8), (40, 12)] {
            rendered_frame(&mut app, width, height);
        }
    }

    #[test]
    fn mono_and_tiny_transcripts_keep_one_header_and_no_standalone_blocks() {
        let state = assistant_state(vec![
            AssistantChild::Thinking {
                id: 1,
                version: 0,
                text: "thought".into(),
            },
            AssistantChild::Text {
                id: 2,
                version: 0,
                markdown: MarkdownDocument::new("answer".into()),
            },
        ]);
        for width in [4, 6, 12, 80] {
            let rendered = transcript_layout_with(
                &state,
                None,
                width,
                &Theme::new(
                    crate::theme::ThemeKind::Mono,
                    crate::theme::ColorLevel::None,
                ),
                &PlainHighlighter,
            )
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
            assert!(!rendered.contains("REASONING"));
            assert!(!rendered.contains("TOOL"));
            assert!(!rendered.contains("ASSISTANT"));
        }
    }

    // ------------------------------------------------------------------
    // Terminal restore / scheduler
    // ------------------------------------------------------------------

    #[test]
    fn terminal_restore_disables_mouse_capture_during_cleanup() {
        let restore = TerminalRestore {
            mouse_capture: true,
            bracketed_paste: true,
            ..TerminalRestore::default()
        };
        let steps = restore.cleanup_steps();
        assert!(steps.contains(&TerminalCleanup::DisableMouseCapture));
        assert!(steps.contains(&TerminalCleanup::DisableBracketedPaste));
    }

    #[test]
    fn render_scheduler_coalesces_streams_and_prioritizes_input() {
        let mut scheduler = RenderScheduler::default();
        let now = std::time::Instant::now();
        assert!(scheduler.should_draw(now));
        scheduler.drew(now);
        scheduler.mark_stream();
        assert!(!scheduler.should_draw(now));
        scheduler.mark_immediate();
        assert!(scheduler.should_draw(now));
    }

    // ------------------------------------------------------------------
    // Replay/cache projections
    // ------------------------------------------------------------------

    #[test]
    fn replay_evaluations_render_variant_scoped_discards() {
        let session = SessionId::new_v7();
        let run = run_id();
        let mut store = StateStore::default();
        let event = event(
            session,
            1,
            run,
            EventPayload::ModelReplayEvaluated {
                attempt_id: AttemptId::new_v7(),
                resolved_model: resolved_model(Some("high")),
                ordered_decisions: vec![
                    cookie_agent_protocol::ReplayDecision {
                        history_index: 0,
                        disposition: cookie_agent_protocol::ReplayDisposition::Replayed,
                    },
                    cookie_agent_protocol::ReplayDecision {
                        history_index: 1,
                        disposition:
                            cookie_agent_protocol::ReplayDisposition::DiscardedForeignVariant {
                                found: None,
                                expected: Some(
                                    cookie_agent_protocol::VariantId::new("high").expect("variant"),
                                ),
                            },
                    },
                ],
            },
        );
        assert!(store.apply_event(event));
        let rendered = store.sessions[&session]
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Event { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("discarded foreign variant base (expected high)"));
        assert!(rendered.contains("gateway/arbitrary-model (high, openai-compatible)"));
    }

    #[test]
    fn replay_projection_deduplicates_logical_transitions_without_losing_evidence() {
        let session = SessionId::new_v7();
        let first_run = run_id();
        let second_run = run_id();
        let resolved = resolved_model(None);
        let adapter_discard = cookie_agent_protocol::ReplayDisposition::DiscardedForeignAdapter {
            found: SafeCode::new("anthropic").expect("adapter"),
            expected: SafeCode::new(resolved.adapter_id.as_str()).expect("adapter"),
        };
        let adapter_evidence = vec![
            cookie_agent_protocol::ReplayDecision {
                history_index: 1,
                disposition: adapter_discard.clone(),
            },
            cookie_agent_protocol::ReplayDecision {
                history_index: 1,
                disposition:
                    cookie_agent_protocol::ReplayDisposition::ReconstructedNormalizedHistory,
            },
            cookie_agent_protocol::ReplayDecision {
                history_index: 3,
                disposition: adapter_discard.clone(),
            },
            cookie_agent_protocol::ReplayDecision {
                history_index: 3,
                disposition:
                    cookie_agent_protocol::ReplayDisposition::ReconstructedNormalizedHistory,
            },
        ];
        let other_model = ModelSelection {
            model: "other/model".parse().expect("model key"),
            variant: None,
        };
        let events = vec![
            event(
                session,
                1,
                first_run,
                EventPayload::ModelReplayEvaluated {
                    attempt_id: AttemptId::new_v7(),
                    resolved_model: resolved.clone(),
                    ordered_decisions: adapter_evidence.clone(),
                },
            ),
            event(
                session,
                2,
                first_run,
                EventPayload::ModelReplayEvaluated {
                    attempt_id: AttemptId::new_v7(),
                    resolved_model: resolved.clone(),
                    ordered_decisions: adapter_evidence,
                },
            ),
            event(
                session,
                3,
                first_run,
                EventPayload::ModelReplayEvaluated {
                    attempt_id: AttemptId::new_v7(),
                    resolved_model: resolved.clone(),
                    ordered_decisions: vec![cookie_agent_protocol::ReplayDecision {
                        history_index: 5,
                        disposition: cookie_agent_protocol::ReplayDisposition::DiscardedForeignModelSelection {
                            found: other_model,
                            expected: resolved.selection.clone(),
                        },
                    }],
                },
            ),
            event(
                session,
                4,
                first_run,
                EventPayload::ModelReplayEvaluated {
                    attempt_id: AttemptId::new_v7(),
                    resolved_model: resolved.clone(),
                    ordered_decisions: vec![cookie_agent_protocol::ReplayDecision {
                        history_index: 7,
                        disposition: cookie_agent_protocol::ReplayDisposition::DiscardedForeignVariant {
                            found: Some(cookie_agent_protocol::VariantId::new("foreign").expect("variant")),
                            expected: None,
                        },
                    }],
                },
            ),
            event(
                session,
                5,
                second_run,
                EventPayload::ModelReplayEvaluated {
                    attempt_id: AttemptId::new_v7(),
                    resolved_model: resolved,
                    ordered_decisions: vec![cookie_agent_protocol::ReplayDecision {
                        history_index: 1,
                        disposition: adapter_discard,
                    }],
                },
            ),
        ];

        let assert_projection = |store: &StateStore| {
            let projected = &store.sessions[&session].transcript;
            let warnings = projected
                .iter()
                .filter_map(|item| match item {
                    TranscriptItem::Event {
                        level: crate::state::EventLevel::Warning,
                        text,
                        ..
                    } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(warnings.len(), 4);
            assert_eq!(
                warnings
                    .iter()
                    .filter(|warning| warning.contains("discarded foreign adapter"))
                    .count(),
                2
            );
            assert_eq!(
                warnings
                    .iter()
                    .filter(|warning| warning.contains("discarded foreign model selection"))
                    .count(),
                1
            );
            assert_eq!(
                warnings
                    .iter()
                    .filter(|warning| warning.contains("discarded foreign variant"))
                    .count(),
                1
            );
            let reconstructions = projected
                .iter()
                .filter(|item| {
                    matches!(
                        item,
                        TranscriptItem::Event {
                            level: crate::state::EventLevel::Debug,
                            text,
                            ..
                        } if text.contains("reconstructed normalized history")
                    )
                })
                .count();
            assert_eq!(reconstructions, 4);
        };

        let mut live = StateStore::default();
        for event in events.clone() {
            assert!(live.apply_event(event));
        }
        assert_projection(&live);

        let mut reopened = StateStore::default();
        assert!(reopened.rebuild_session(session, 0, events));
        assert_projection(&reopened);
    }

    #[test]
    fn output_snapshot_handoff_and_gaps_stay_ordered() {
        let session = SessionId::new_v7();
        let call = ToolCallId::new_v7();
        let mut store = StateStore::default();
        store.sessions.entry(session).or_default().tools.insert(
            call,
            ToolCallState {
                id: call,
                owner: owner(1, "call-1"),
                presentation: presentation("bash", None),
                arguments: String::new(),
                status: ToolStatus::Running,
                detail: String::new(),
            },
        );
        store.apply_output_gap(cookie_agent_protocol::OutputGap {
            call_id: call,
            stream: OutputStream::Stdout,
            next_offset: 3,
        });
        store.apply_output_delta(OutputDelta {
            call_id: call,
            stream: OutputStream::Stdout,
            byte_offset: 3,
            data: STANDARD.encode(b"two"),
        });
        let output = &store.sessions[&session].output[&(call, false)];
        assert!(output.has_gap);
        assert_eq!(output.text(), "two");
    }

    // ------------------------------------------------------------------
    // Descendant warnings
    // ------------------------------------------------------------------

    fn push_model_warning(store: &mut StateStore, session_id: SessionId, text: &str) {
        let run = run_id();
        let attempt = AttemptId::new_v7();
        assert!(store.apply_event(attempt_started(session_id, 1, run, attempt, None)));
        assert!(store.apply_event(turn_committed(
            session_id,
            2,
            run,
            attempt,
            1,
            Vec::new(),
            vec![text],
            None,
        )));
    }

    #[tokio::test]
    async fn descendant_warnings_aggregate_with_attribution_without_duplication() {
        let mut app = test_app().await;
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        app.tree = Some(SessionTree {
            session: titled_meta(root, "root session", 1),
            children: vec![SessionTree {
                session: titled_meta(child, "child session", 1),
                children: Vec::new(),
            }],
        });
        app.tree_root = Some(root);
        push_model_warning(&mut app.store, root, "root warning");
        push_model_warning(&mut app.store, child, "child warning");
        let warnings = app.descendant_warnings(root);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("child warning"));
        assert!(warnings[0].contains("child session"));
        assert!(warnings[0].contains(&crate::ui::pickers::short_id(child)));
    }

    // ------------------------------------------------------------------
    // Layout cache
    // ------------------------------------------------------------------

    #[test]
    fn child_layout_cache_recomputes_only_the_changed_assistant_segment() {
        let state = assistant_state(vec![
            AssistantChild::Thinking {
                id: 1,
                version: 0,
                text: "stable thought".into(),
            },
            AssistantChild::Text {
                id: 2,
                version: 0,
                markdown: MarkdownDocument::new("stable text".into()),
            },
        ]);
        let mut cache = LayoutCache::default();
        let session = SessionId::new_v7();
        ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            None,
            None,
            60,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Debug,
        );
        let passes = cache.assistant_part_layout_passes;
        assert_eq!(passes, 2);
        let item_passes = cache.item_layout_passes;
        // A cache hit for the identical projection recomputes nothing.
        ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            None,
            None,
            60,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Debug,
        );
        assert_eq!(cache.assistant_part_layout_passes, passes);
        assert_eq!(cache.item_layout_passes, item_passes);
        // A version bump on one child (and its owning item, exactly as the
        // reducer maintains) recomputes only that child.
        let mut changed = state;
        if let TranscriptItem::Assistant {
            version: item_version,
            children,
            ..
        } = &mut changed.transcript[0]
        {
            *item_version = 1;
            if let AssistantChild::Text { version, .. } = &mut children[1] {
                *version = 1;
            }
        }
        ensure_cached_transcript_layout(
            &mut cache,
            session,
            &changed,
            None,
            None,
            60,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Debug,
        );
        assert_eq!(cache.assistant_part_layout_passes, passes + 1);
    }

    // ------------------------------------------------------------------
    // Canonical parallel ownership snapshots
    // ------------------------------------------------------------------

    /// A canonical parallel-tools projection: two committed tool parts at
    /// distinct content indices, out-of-order starts, and a completion of
    /// the second tool before the first starts.
    fn parallel_tools_state() -> (StateStore, SessionId, ToolCallId, ToolCallId) {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let first = ToolCallId::new_v7();
        let second = ToolCallId::new_v7();
        let mut store = StateStore::default();
        let events = [
            session_created(session, 1),
            attempt_started(session, 2, run, attempt, None),
            text_delta(session, 3, run, attempt, "launching both"),
            turn_committed(
                session,
                4,
                run,
                attempt,
                5,
                vec![
                    cookie_agent_protocol::PersistedAssistantPart::Text {
                        text: "launching both".into(),
                        metadata: None,
                    },
                    cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                        id: ModelCallId::new("call-a").expect("call"),
                        provider_item_id: None,
                        name: SafeCode::new("bash").expect("tool"),
                        input: serde_json::json!({"command": "sleep 2"}),
                        raw_input: None,
                        metadata: None,
                    },
                    cookie_agent_protocol::PersistedAssistantPart::Reasoning {
                        text: "waiting on both".into(),
                        metadata: None,
                    },
                    cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                        id: ModelCallId::new("call-b").expect("call"),
                        provider_item_id: None,
                        name: SafeCode::new("read").expect("tool"),
                        input: serde_json::json!({"path": "src/lib.rs"}),
                        raw_input: None,
                        metadata: None,
                    },
                ],
                Vec::new(),
                None,
            ),
            // Out-of-order: call-b starts first and completes before call-a
            // even starts.
            tool_started_at(
                session,
                5,
                run,
                second,
                5,
                "call-b",
                3,
                "read",
                Some("src/lib.rs"),
            ),
            tool_terminated(
                session,
                6,
                run,
                second,
                5,
                "call-b",
                cookie_agent_protocol::ToolTerminationOutcome::Completed,
            ),
            tool_started_at(
                session,
                7,
                run,
                first,
                5,
                "call-a",
                1,
                "bash",
                Some("sleep 2"),
            ),
        ];
        for event in events {
            assert!(store.apply_event(event));
        }
        (store, session, first, second)
    }

    #[test]
    fn canonical_parallel_ownership_preserves_content_index_order() {
        let (store, session, first, second) = parallel_tools_state();
        let state = &store.sessions[&session];
        let TranscriptItem::Assistant { children, .. } = state
            .transcript
            .iter()
            .find(|item| matches!(item, TranscriptItem::Assistant { .. }))
            .expect("assistant item")
        else {
            panic!("assistant item")
        };
        let rendered_kinds = children
            .iter()
            .map(|child| match child {
                AssistantChild::Text { .. } => "text",
                AssistantChild::Thinking { .. } => "thinking",
                AssistantChild::Tool { call_id } if *call_id == first => "tool-a",
                AssistantChild::Tool { call_id } if *call_id == second => "tool-b",
                AssistantChild::Tool { .. } => "tool",
                AssistantChild::CommittedTool { .. } => "placeholder",
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rendered_kinds,
            vec!["text", "tool-a", "thinking", "tool-b"],
            "children stay in committed content order"
        );
        let tool_a = &state.tools[&first];
        let tool_b = &state.tools[&second];
        assert_eq!(tool_a.compact_title(), "bash sleep 2");
        assert_eq!(tool_a.status, ToolStatus::Running);
        assert_eq!(tool_b.compact_title(), "read src/lib.rs");
        assert_eq!(tool_b.status, ToolStatus::Completed);
    }

    fn snapshot_lines(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn parallel_tools_render_snapshot_across_widths_and_themes() {
        let (store, session, _, _) = parallel_tools_state();
        let state = &store.sessions[&session];
        let mut snapshots = Vec::new();
        for width in [24u16, 48, 96] {
            let layout = transcript_layout_with_level(
                state,
                None,
                width,
                &Theme::default(),
                &crate::markdown::SyntectHighlighter::default(),
                crate::state::EventLevel::Error,
            );
            snapshots.push(format!(
                "== width {width} ==\n{}",
                snapshot_lines(&layout.lines)
            ));
        }
        let mono = transcript_layout_with_level(
            state,
            None,
            48,
            &Theme::new(
                crate::theme::ThemeKind::Mono,
                crate::theme::ColorLevel::None,
            ),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Error,
        );
        snapshots.push(format!("== mono 48 ==\n{}", snapshot_lines(&mono.lines)));
        insta::assert_snapshot!(snapshots.join("\n"));
    }

    #[test]
    fn parallel_tools_expanded_hit_regions_track_each_tool() {
        let (store, session, first, second) = parallel_tools_state();
        let state = &store.sessions[&session];
        let expanded = std::collections::HashSet::from([
            BlockId::Tool(first),
            BlockId::Tool(second),
            BlockId::Thinking(6),
        ]);
        let layout = transcript_layout_with_level(
            state,
            Some(&expanded),
            60,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Error,
        );
        // One chevron per row, expanded markers only.
        let rendered = snapshot_lines(&layout.lines);
        assert!(!rendered.contains('▸'));
        assert_eq!(rendered.matches('▾').count(), 3);
        assert!(
            rendered.contains("arguments: {\"command\":\"sleep 2\"}")
                || rendered.contains("sleep 2")
        );
        let tool_regions = layout
            .regions
            .iter()
            .filter(|region| matches!(region.id, BlockId::Tool(_)))
            .count();
        assert_eq!(tool_regions, 2);
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn cancelled_and_interrupted_tools_render_distinct_concise_markers() {
        let call_cancelled = ToolCallId::new_v7();
        let call_interrupted = ToolCallId::new_v7();
        let mut state = assistant_state(vec![
            AssistantChild::Tool {
                call_id: call_cancelled,
            },
            AssistantChild::Tool {
                call_id: call_interrupted,
            },
        ]);
        for (call_id, status) in [
            (call_cancelled, ToolStatus::Cancelled),
            (call_interrupted, ToolStatus::Interrupted),
        ] {
            state.tools.insert(
                call_id,
                ToolCallState {
                    id: call_id,
                    owner: owner(1, "call-1"),
                    presentation: presentation("bash", Some("make")),
                    arguments: "{}".into(),
                    status,
                    detail: String::new(),
                },
            );
        }
        let rendered = snapshot_lines(&transcript_layout(&state, None, 60).lines);
        assert!(rendered.contains("▸ bash make cancelled"));
        assert!(rendered.contains("▸ bash make interrupted"));
        assert!(!rendered.contains("failed"));
        assert!(!rendered.contains("COMPLETED"));
    }

    // ------------------------------------------------------------------
    // Approval identity snapshots
    // ------------------------------------------------------------------

    fn stable_approval_snapshot(approval: &ApprovalState, mut snapshot: String) -> String {
        snapshot = snapshot.replace(&approval.approval_id.to_string(), "<approval-id>");
        snapshot = snapshot.replace(
            approval.operation_fingerprint.digest().as_str(),
            "<operation-fingerprint>",
        );
        snapshot = snapshot.replace(
            approval.normalized_arguments_digest.as_str(),
            "<normalized-arguments-digest>",
        );
        snapshot = snapshot.replace(
            approval.execution_context_digest.as_str(),
            "<execution-context-digest>",
        );
        for (index, resource) in approval.resources.iter().enumerate() {
            snapshot = snapshot.replace(
                resource.binding_digest.digest().as_str(),
                &format!("<resource-{}-binding-digest>", index + 1),
            );
        }
        snapshot
    }

    fn bash_approval_state() -> ApprovalState {
        approval(SessionId::new_v7())
    }

    #[test]
    fn bash_prepared_approval_identity_snapshot_is_complete() {
        let approval = bash_approval_state();
        insta::assert_snapshot!(stable_approval_snapshot(
            &approval,
            approval_content(&approval)
        ));
    }

    #[test]
    fn approval_modal_no_color_snapshot_remains_textually_complete_and_scrollable() {
        let approval = bash_approval_state();
        let content = approval_content(&approval);
        assert!(content.contains("PERMISSION REQUIRED · ESCALATED"));
        assert!(content.contains("git status"));
        let lines = content.lines().count();
        assert!(lines > 20);
    }
}

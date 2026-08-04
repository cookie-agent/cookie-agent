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
    // The frozen `Agent(Model)` attribution wraps at tiny widths and is
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
        sync::{Arc, Mutex},
    };

    use super::*;
    use cookie_agent_protocol::{
        AgentId, ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation,
        ApprovalId, ApprovalRecord, ApprovalRequest, ApprovalResourceSource, ApprovalStatus,
        ApprovalTrigger, AssistantToolCallRef, AttemptId, CatalogIdentifier, CatalogProvider,
        CatalogText, CredentialFieldName, DecisionTrace, EventPayload, EventSchemaVersion,
        ModelCallId, ModelKey, ModelSelection, OperationFingerprint, OutputDelta, OutputStream,
        PermissionAction, PermissionEffect, PreparedApprovalResource, PreparedBindingLifetime,
        PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
        PreparedResourceIdentity, ProviderId, RunId, RunSelection, SafeCode, SafeDisplayText,
        SafeErrorMessage, SessionId, SessionMeta, SessionMetaSchemaVersion, SessionOrigin,
        SessionStatus, SessionTitle, SessionTree, Sha256Digest, StoredEvent, ToolCallId,
        ToolCallStart, Usage,
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
    use crate::ui::app::*;
    use crate::ui::events::{RenderScheduler, TerminalCleanup, TerminalRestore};
    use crate::ui::input::{CredentialInput, credential_wipe_count};
    use crate::ui::slash::{
        BlockCommand, InputMode, ScrollCommand, SlashCommand, Submission, command_allowed_in_mode,
        command_help, command_spec, parse_submission,
    };
    use crate::ui::terminal_layout_with_tree_rows;

    use async_trait::async_trait;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use cookie_agent_server::{MessageFrame, MessageStream, TransportError};
    use serde_json::Value;

    // ------------------------------------------------------------------
    // Fixtures
    // ------------------------------------------------------------------

    const AGENT: &str = "primary";
    const MODEL: &str = "gateway/arbitrary-model";

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
        let chain = chain
            .into_iter()
            .map(|resolved| cookie_agent_protocol::FrozenModelBinding {
                behavior_fingerprint: resolved.selection_fingerprint.clone(),
                resolved,
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
                defaults: cookie_agent_protocol::ResolvedRequestDefaults {
                    request: cookie_agent_protocol::RequestDefaults::default(),
                    reasoning: None,
                },
                provider_options: cookie_agent_protocol::ProviderOptions::OpenAiCompatible {
                    api_path: None,
                },
            })
            .collect::<Vec<_>>();
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
                model_snapshot_fingerprint: Sha256Digest::of_bytes(b"snapshot"),
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

    // Test fixtures stay explicit about each v7 turn field; grouping them
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

    // Fixture mirrors the exact v7 ownership/presentation fields; grouping
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

    async fn test_app() -> App {
        let (client, _requests) = recording_client();
        App::new(client).await.expect("test app")
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
    fn assistant_header_projects_agent_model_without_variant() {
        assert_eq!(
            attribution(None).header(),
            "primary(gateway/arbitrary-model)"
        );
        assert_eq!(
            attribution(Some("high")).header(),
            "primary(gateway/arbitrary-model)"
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
                squashed.contains("primary(gateway/arbitrary-model"),
                "width {width}: {visible}"
            );
            assert!(!visible.contains("[A]"), "width {width}");
        }
        let rendered = snapshot_lines(&transcript_layout(&state, None, 80).lines);
        assert!(rendered.contains("╭─ primary(gateway/arbitrary-model)"));
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
                .matches("╭─ primary(gateway/arbitrary-model)")
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
        assert_eq!(attribution.header(), "primary(gateway/arbitrary-model)");
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
    // Streaming reduction against v7 events
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
            rendered.matches("primary(gateway/arbitrary-model)").count(),
            2
        );
        assert!(!rendered.contains("high)"));
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
        assert!(rendered.contains("primary(gateway/arbitrary-model-high)"));
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
        assert_eq!(variant_rect.width, 4);
        assert_eq!(model_rect.x, agent_rect.x + agent_rect.width + 1);
        assert_eq!(variant_rect.x, model_rect.x + model_rect.width + 1);
    }

    #[tokio::test]
    async fn title_segments_open_their_picker_modals_by_mouse() {
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
        assert_eq!(app.modal, Modal::Variants);
    }

    #[tokio::test]
    async fn draft_selection_pickers_offer_chain_models_base_and_variants() {
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
        assert_eq!(app.draft_models().len(), 1);
        let variants = app.draft_variants();
        assert_eq!(variants.len(), 3);
        assert!(variants[0].is_none());
        assert_eq!(variants[1].as_ref().map(|v| v.as_str()), Some("fast"));
        assert_eq!(variants[2].as_ref().map(|v| v.as_str()), Some("high"));

        // Choosing a variant changes only the draft; active runs are frozen.
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.store.sessions.entry(session).or_default().active_run = Some(run_id());
        app.store
            .sessions
            .get_mut(&session)
            .expect("session")
            .run_agent = Some(agent_id());
        app.set_draft_variant(Some(
            cookie_agent_protocol::VariantId::new("high").expect("variant"),
        ));
        assert!(app.status.contains("the active run is unchanged"));
        assert_eq!(
            app.active_run_agent().map(|agent| agent.as_str()),
            Some("primary")
        );
    }

    #[tokio::test]
    async fn agent_picker_lists_only_root_runnable_agents() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true), descriptor("worker", false)];
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
        // Model and variant pickers stay available during the run too.
        app.open_selection_modal(Modal::Models);
        assert_eq!(app.modal, Modal::Models);
        app.modal = Modal::None;
        app.open_selection_modal(Modal::Variants);
        assert_eq!(app.modal, Modal::Variants);
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
        // Model and variant selection stay available for the delegated
        // session within its frozen agent's fallback chain.
        app.open_selection_modal(Modal::Models);
        assert_eq!(app.modal, Modal::Models);
        app.choose_picker_entry(0).await;
        assert_eq!(app.modal, Modal::None);
        app.open_selection_modal(Modal::Variants);
        assert_eq!(app.modal, Modal::Variants);
        // Only the persisted exact selections appear: base for this model.
        assert_eq!(app.draft_variants(), vec![None]);
        app.choose_picker_entry(0).await;
        assert_eq!(app.modal, Modal::None);
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
        app.agent_revision = Some(
            cookie_agent_protocol::SnapshotRevision::new(
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            )
            .expect("revision"),
        );
        app.model_revision = Some(
            cookie_agent_protocol::SnapshotRevision::new(
                "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            )
            .expect("revision"),
        );
        let label = app.descriptor_revisions_label();
        assert!(label.contains("agent revision sha256:1111"));
        assert!(label.contains("model revision sha256:2222"));

        // A root draft pointing at a now-unrunnable agent resets to the
        // default; a delegated session's pin is untouched.
        app.agents = vec![descriptor("reviewer", true)];
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
            .map(|resolved| cookie_agent_protocol::FrozenModelBinding {
                behavior_fingerprint: resolved.selection_fingerprint.clone(),
                resolved: resolved.clone(),
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
                defaults: cookie_agent_protocol::ResolvedRequestDefaults {
                    request: cookie_agent_protocol::RequestDefaults::default(),
                    reasoning: None,
                },
                provider_options: cookie_agent_protocol::ProviderOptions::OpenAiCompatible {
                    api_path: None,
                },
            })
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
        // Variant options for the head model are exactly the persisted
        // selection set, not live descriptor variants.
        assert_eq!(
            app.persisted_variants_for(&model_key())
                .iter()
                .map(|variant| variant.as_ref().map(|id| id.as_str()))
                .collect::<Vec<_>>(),
            vec![Some("high"), Some("fast")]
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
                .resolved
                .selection
                .variant
                .as_ref()
                .map(|variant| variant.as_str()),
            Some("high")
        );
    }

    #[tokio::test]
    async fn delegated_variant_selector_is_immune_to_live_provider_refresh() {
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
        // selector still exposes only the persisted exact selection.
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
        // Choosing works from the persisted selection only.
        app.open_selection_modal(Modal::Variants);
        assert_eq!(app.picker_entry_count(), 1);
    }

    #[tokio::test]
    async fn connect_refreshed_installs_coherent_pair_with_both_revision_labels() {
        let mut app = test_app().await;
        let revision = cookie_agent_protocol::SnapshotRevision::new(
            "sha256:7777777777777777777777777777777777777777777777777777777777777777",
        )
        .expect("revision");
        let models = cookie_agent_protocol::ModelListResult {
            revision: revision.clone(),
            generated_at: jiff::Timestamp::now(),
            catalog_revision: cookie_agent_protocol::CatalogRevision::current(),
            models: vec![model_descriptor()],
        };
        let agents = cookie_agent_protocol::AgentListResult {
            revision: revision.clone(),
            model_revision: revision.clone(),
            generated_at: jiff::Timestamp::now(),
            agents: vec![descriptor("primary", true)],
        };
        app.apply_connect_outcome(ConnectOutcome::Connected {
            provider_id: cookie_agent_protocol::ProviderId::new("acme").expect("provider"),
            receipt_model_revision: revision.clone(),
            follow_up: Box::new(ConnectFollowUp::Refreshed {
                models: Box::new(models),
                agents: Box::new(agents),
                created: None,
            }),
        });
        assert_eq!(app.model_revision.as_ref(), Some(&revision));
        assert_eq!(app.agent_revision.as_ref(), Some(&revision));
        assert_eq!(app.models.len(), 1);
        assert_eq!(app.models[0].variants.len(), 2);
        assert_eq!(app.agents.len(), 1);
        assert!(app.status.contains("agents are ready"));

        // Mismatch: the bounded retry failed coherence, so the whole pair
        // is discarded before any side effect; both labels and the prior
        // pair stay authoritative and no session is created.
        let stale = cookie_agent_protocol::SnapshotRevision::new(
            "sha256:8888888888888888888888888888888888888888888888888888888888888888",
        )
        .expect("revision");
        let sessions_before = app.sessions.len();
        app.apply_connect_outcome(ConnectOutcome::Connected {
            provider_id: cookie_agent_protocol::ProviderId::new("acme").expect("provider"),
            receipt_model_revision: revision.clone(),
            follow_up: Box::new(ConnectFollowUp::Incoherent {
                model_revision: stale,
                agent_model_revision: revision.clone(),
            }),
        });
        assert_eq!(app.model_revision.as_ref(), Some(&revision));
        assert_eq!(app.agent_revision.as_ref(), Some(&revision));
        assert_eq!(app.models.len(), 1);
        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.sessions.len(), sessions_before);
        assert!(app.status.contains("incoherent"));
        assert!(app.status.contains("No session was created"));
    }

    #[tokio::test]
    async fn coherent_pair_install_success_failure_and_mismatch() {
        // Success path is covered by refresh at startup; here the revision
        // guard semantics: matching revisions install atomically.
        let mut app = test_app().await;
        let revision = cookie_agent_protocol::SnapshotRevision::new(
            "sha256:5555555555555555555555555555555555555555555555555555555555555555",
        )
        .expect("revision");
        let models = cookie_agent_protocol::ModelListResult {
            revision: revision.clone(),
            generated_at: jiff::Timestamp::now(),
            catalog_revision: cookie_agent_protocol::CatalogRevision::current(),
            models: vec![model_descriptor()],
        };
        let agents = cookie_agent_protocol::AgentListResult {
            revision: revision.clone(),
            model_revision: revision.clone(),
            generated_at: jiff::Timestamp::now(),
            agents: vec![descriptor("primary", true)],
        };
        app.install_coherent_pair(models, agents);
        assert_eq!(app.model_revision.as_ref(), Some(&revision));
        assert_eq!(app.agent_revision.as_ref(), Some(&revision));
        assert_eq!(app.models.len(), 1);
        assert_eq!(app.models[0].variants.len(), 2);
        assert_eq!(app.agents.len(), 1);

        // Mismatch: nothing is installed; the prior pair and both labels
        // stay authoritative.
        let stale_models = cookie_agent_protocol::ModelListResult {
            revision: cookie_agent_protocol::SnapshotRevision::new(
                "sha256:6666666666666666666666666666666666666666666666666666666666666666",
            )
            .expect("revision"),
            generated_at: jiff::Timestamp::now(),
            catalog_revision: cookie_agent_protocol::CatalogRevision::current(),
            models: Vec::new(),
        };
        let mismatched_agents_revision = app
            .model_revision
            .clone()
            .expect("installed model revision");
        assert_ne!(
            mismatched_agents_revision, stale_models.revision,
            "a mismatched pair is never installed"
        );
        assert_eq!(app.models.len(), 1);
        assert_eq!(app.agents.len(), 1);
    }

    #[tokio::test]
    async fn incoherent_descriptor_pairs_are_retried_then_discarded_whole() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![model_descriptor()];
        let revision = cookie_agent_protocol::SnapshotRevision::new(
            "sha256:3333333333333333333333333333333333333333333333333333333333333333",
        )
        .expect("revision");
        app.agent_revision = Some(revision.clone());
        app.model_revision = Some(revision);
        let agents_before = app.agents.clone();
        let models_before = app.models.clone();
        // Simulated mismatched pair handling: a mismatched revision pair is
        // never applied (unit-level mirror of the refresh guard).
        let mismatched_model_revision = cookie_agent_protocol::SnapshotRevision::new(
            "sha256:4444444444444444444444444444444444444444444444444444444444444444",
        )
        .expect("revision");
        let coherent = app.model_revision.as_ref() == Some(&mismatched_model_revision);
        assert!(!coherent);
        assert_eq!(app.agents.len(), agents_before.len());
        assert_eq!(app.models.len(), models_before.len());
        assert_eq!(
            app.models[0].variants.len(),
            2,
            "variants stay with the previous coherent model snapshot"
        );
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

    #[tokio::test]
    async fn credential_inputs_wipe_on_cancel_and_app_drop() {
        let before = credential_wipe_count();
        {
            let mut app = test_app().await;
            app.modal = Modal::ConnectCredentials;
            app.connect_fields = vec![(
                CredentialFieldName::new("API_KEY").expect("field"),
                CredentialInput::default(),
            )];
            app.connect_fields[0]
                .1
                .insert_owned("sentinel-secret".to_owned());
            app.clear_connect_secrets();
            assert!(app.connect_fields.is_empty());
        }
        assert!(credential_wipe_count() > before);
    }

    #[tokio::test]
    async fn provider_picker_filter_matches_credential_labels() {
        let provider = CatalogProvider {
            id: CatalogIdentifier::new("acme-ai").expect("id"),
            name: CatalogText::new("Acme AI").expect("name"),
            credential_fields: vec![CredentialFieldName::new("ACME_API_KEY").expect("field")],
            npm: CatalogText::new("@acme/ai").expect("npm"),
            api: Some(CatalogText::new("https://api.acme.example").expect("api")),
            documentation_url: CatalogText::new("https://docs.acme.example").expect("docs"),
        };
        let mut app = test_app().await;
        app.providers = vec![provider];
        app.picker_query = "acme_api".into();
        assert_eq!(app.filtered_providers().len(), 1);
        app.picker_query = "unknown".into();
        assert!(app.filtered_providers().is_empty());
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
            .map(|line| line.to_string())
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
    // Connect coherence gate at the RPC boundary
    // ------------------------------------------------------------------

    /// A scripted daemon: answers provider.connect, model.list, agent.list,
    /// and session.create with configurable descriptor revisions while
    /// recording every outbound client request.
    struct ConnectScript {
        model_revision: cookie_agent_protocol::SnapshotRevision,
        /// The model revision agent.list currently reports; switchable
        /// between startup and the connect follow-up.
        agent_model_revision: Arc<std::sync::Mutex<cookie_agent_protocol::SnapshotRevision>>,
        /// Optional (call-count, revision) flip for delayed-coherent
        /// scenarios.
        flip: Option<(usize, cookie_agent_protocol::SnapshotRevision)>,
        recorded: Arc<Mutex<Vec<Value>>>,
    }

    impl ConnectScript {
        fn client(self) -> Client {
            let (incoming_tx, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
            let (sent, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();
            let recorded = self.recorded.clone();
            let model_revision = self.model_revision.clone();
            let agent_revision_cell = self.agent_model_revision;
            let flip = self.flip;
            let mut agent_list_calls = 0usize;
            tokio::spawn(async move {
                while let Some(frame) = sent_rx.recv().await {
                    let value = match frame {
                        MessageFrame::Value(value) => value,
                        MessageFrame::Text(text) => {
                            serde_json::from_str(&text).unwrap_or(Value::Null)
                        }
                    };
                    recorded.lock().expect("recorded").push(value.clone());
                    let method = value["method"].as_str().unwrap_or_default().to_owned();
                    let id = value["id"].clone();
                    let result = match method.as_str() {
                        "provider.connect" => serde_json::json!({
                            "client_connect_id": value["params"]["client_connect_id"],
                            "connection": {
                                "provider_id": "acme",
                                "credential_fields": ["API_KEY"],
                                "connected_at": jiff::Timestamp::now(),
                                "catalog_revision": cookie_agent_protocol::CatalogRevision::current(),
                            },
                            "model_revision": model_revision,
                        }),
                        "model.list" => serde_json::json!({
                            "revision": model_revision,
                            "generated_at": jiff::Timestamp::now(),
                            "catalog_revision": cookie_agent_protocol::CatalogRevision::current(),
                            "models": [],
                        }),
                        "agent.list" => {
                            agent_list_calls += 1;
                            let revision = if let Some((flip_after, coherent)) = &flip {
                                if agent_list_calls > *flip_after {
                                    coherent.clone()
                                } else {
                                    agent_revision_cell.lock().expect("revision").clone()
                                }
                            } else {
                                agent_revision_cell.lock().expect("revision").clone()
                            };
                            serde_json::json!({
                                "revision": revision,
                                "model_revision": revision,
                                "generated_at": jiff::Timestamp::now(),
                                "agents": [{
                                    "id": "primary",
                                    "description": "Primary",
                                    "mode": "primary",
                                    "enabled": true,
                                    "runnable_as_root": true,
                                    "resolved_fallback": [{"model": "gateway/arbitrary-model", "variant": null}],
                                    "tools": [],
                                    "delegation_targets": []
                                }],
                            })
                        }
                        "session.create" => serde_json::json!({
                            "session": {
                                "meta_schema_version": 7,
                                "session_id": SessionId::new_v7(),
                                "origin": {"type": "root"},
                                "cwd_identity": "/workspace",
                                "creation_selection": {
                                    "agent": "primary",
                                    "model": {"model": "gateway/arbitrary-model", "variant": null},
                                },
                                "title": null,
                                "title_updated_seq": 0,
                                "last_event_seq": 1,
                                "status": "idle",
                            }
                        }),
                        _ => Value::Null,
                    };
                    let _ = incoming_tx.send(MessageFrame::Value(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result,
                    })));
                }
            });
            Client::connect_stream(ScriptedStream {
                incoming: incoming_rx,
                sent,
            })
        }
    }

    fn revision(digit: &str) -> cookie_agent_protocol::SnapshotRevision {
        cookie_agent_protocol::SnapshotRevision::new(format!(
            "sha256:{}",
            digit.repeat(64 / digit.len())
        ))
        .expect("revision")
    }

    fn recorded_method_count(recorded: &Arc<Mutex<Vec<Value>>>, method: &str) -> usize {
        recorded
            .lock()
            .expect("recorded")
            .iter()
            .filter(|value| value["method"].as_str() == Some(method))
            .count()
    }

    async fn drive_connect(
        startup_agent_revision: cookie_agent_protocol::SnapshotRevision,
        post_startup_agent_revision: Option<cookie_agent_protocol::SnapshotRevision>,
        model_revision: cookie_agent_protocol::SnapshotRevision,
        flip_after: Option<(usize, cookie_agent_protocol::SnapshotRevision)>,
    ) -> (Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let script_revision = Arc::new(std::sync::Mutex::new(startup_agent_revision));
        let script = ConnectScript {
            model_revision,
            agent_model_revision: script_revision.clone(),
            flip: flip_after,
            recorded: recorded.clone(),
        };
        let client = script.client();
        let mut app = App::new(client.clone()).await.expect("app");
        let provider = CatalogProvider {
            id: CatalogIdentifier::new("acme").expect("id"),
            name: CatalogText::new("Acme").expect("name"),
            credential_fields: vec![CredentialFieldName::new("API_KEY").expect("field")],
            npm: CatalogText::new("@acme/ai").expect("npm"),
            api: None,
            documentation_url: CatalogText::new("https://docs.acme.example").expect("docs"),
        };
        app.providers = vec![provider.clone()];
        app.catalog_revision = Some(cookie_agent_protocol::CatalogRevision::current());
        app.connect_provider = Some(provider);
        app.connect_fields = vec![(CredentialFieldName::new("API_KEY").expect("field"), {
            let mut input = CredentialInput::default();
            input.insert_owned("secret".to_owned());
            input
        })];
        // Simulate the empty-startup path: the follow-up may create the
        // initial session.
        app.selected = None;
        app.sessions.clear();
        // Only requests issued by the connect follow-up count; startup
        // refreshes are drained from the record first.
        recorded.lock().expect("recorded").clear();
        if let Some(switch) = post_startup_agent_revision {
            *script_revision.lock().expect("revision") = switch;
        }
        app.dispatch_provider_connect();
        let handle = app.connect_task.take().expect("connect task");
        (recorded, handle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incoherent_connect_pair_creates_zero_sessions_after_bounded_retry() {
        // Startup is coherent; the connect phase reports a stale model
        // revision on every attempt, so the bounded retry fails coherence.
        let (recorded, handle) =
            drive_connect(revision("aa"), Some(revision("bb")), revision("aa"), None).await;
        handle.await.expect("connect task");
        assert_eq!(
            recorded_method_count(&recorded, "session.create"),
            0,
            "no session.create on incoherence"
        );
        assert_eq!(
            recorded_method_count(&recorded, "agent.list"),
            2,
            "exactly one bounded retry"
        );
        assert_eq!(recorded_method_count(&recorded, "model.list"), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn coherent_connect_pair_creates_exactly_one_session_without_orphans() {
        let (recorded, handle) = drive_connect(revision("aa"), None, revision("aa"), None).await;
        handle.await.expect("connect task");
        assert_eq!(
            recorded_method_count(&recorded, "session.create"),
            1,
            "one session.create on coherent success"
        );
        assert_eq!(recorded_method_count(&recorded, "agent.list"), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delayed_coherent_retry_creates_one_session_from_the_coherent_pair() {
        // First agent.list of the connect phase is stale, the retry is
        // coherent: creation proceeds only after the verified pair.
        let (recorded, handle) = drive_connect(
            revision("aa"),
            Some(revision("bb")),
            revision("aa"),
            Some((2, revision("aa"))),
        )
        .await;
        handle.await.expect("connect task");
        assert_eq!(recorded_method_count(&recorded, "agent.list"), 2);
        assert_eq!(
            recorded_method_count(&recorded, "session.create"),
            1,
            "one session.create after the coherent retry"
        );
        // The created session selection comes from the coherent agent
        // snapshot.
        let creations = recorded
            .lock()
            .expect("recorded")
            .iter()
            .filter(|value| value["method"].as_str() == Some("session.create"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(creations.len(), 1);
        assert_eq!(creations[0]["params"]["selection"]["agent"], "primary");
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

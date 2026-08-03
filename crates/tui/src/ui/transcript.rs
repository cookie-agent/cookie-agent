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
    state::{AssistantPart, SessionState, ToolStatus, TranscriptItem},
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
        let title = if area.width >= 72 {
            format!(
                "Conversation · events ≥ {filter} · click a block to expand · drag the scrollbar"
            )
        } else {
            format!("Conversation · events ≥ {filter}")
        };
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
        BlockId::Tool(_) => "tool block",
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
        TranscriptItem::Assistant { parts, .. } => parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Thinking { id, .. } => Some(BlockId::Thinking(*id)),
                AssistantPart::Text { .. } => None,
            })
            .collect(),
        TranscriptItem::Tool { call_id, .. } => vec![BlockId::Tool(*call_id)],
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
        TranscriptItem::Assistant { id, parts, .. } => {
            assistant_item_layout(state, *id, parts, context)
        }
        TranscriptItem::Tool { call_id, .. } => {
            let block_id = BlockId::Tool(*call_id);
            let selected = context.selected_block == Some(block_id);
            let is_expanded = context
                .expanded
                .is_some_and(|blocks| blocks.contains(&block_id));
            let Some(tool) = state.tools.get(call_id) else {
                let lines = role_block_selected(
                    Role::Error,
                    vec![Line::from(format!("tool {call_id}: unavailable payload"))],
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
            let (state_label, role) = match tool.status {
                ToolStatus::Running => ("RUNNING …", Role::ToolRunning),
                ToolStatus::Completed => ("COMPLETED ✓", Role::ToolSuccess),
                ToolStatus::Failed => ("FAILED !", Role::ToolFailure),
            };
            // Exactly one chevron per tool row: `▸` collapsed, `▾` expanded.
            // Selection is conveyed through the role style (bold/underline),
            // never a second triangle.
            let mut body = if is_expanded {
                vec![
                    Line::from(format!("▾ {} — {state_label}", tool.tool)),
                    Line::from(format!("arguments: {}", tool.arguments)),
                ]
            } else {
                vec![Line::from(format!(
                    "▸ {} — {state_label} (details hidden)",
                    tool.tool
                ))]
            };
            if is_expanded {
                if !tool.detail.is_empty() {
                    body.extend(tool_body_lines(tool, context));
                }
                for (stderr, label) in [(false, "STDOUT"), (true, "STDERR")] {
                    if let Some(output) = state.output.get(&(*call_id, stderr)) {
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
            let lines = role_block_selected(role, body, context.width, context.theme, selected);
            ItemLayout {
                regions: vec![BlockRegion {
                    id: block_id,
                    start_line: 0,
                    end_line: lines.len(),
                }],
                lines,
            }
        }
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
    parts: &[AssistantPart],
    context: &mut TranscriptRenderContext<'_>,
) -> ItemLayout {
    let mut layout = ItemLayout {
        lines: assistant_header(context.width, context.theme),
        regions: Vec::new(),
    };
    for part in parts {
        let block_id = match part {
            AssistantPart::Thinking { id, .. } => Some(BlockId::Thinking(*id)),
            AssistantPart::Text { .. } => None,
        };
        let key = AssistantPartLayoutKey {
            version: part.version(),
            expanded: block_id
                .is_some_and(|id| context.expanded.is_some_and(|blocks| blocks.contains(&id))),
            selected: block_id.is_some_and(|id| context.selected_block == Some(id)),
            streaming: matches!(part, AssistantPart::Thinking { id, .. } if state.is_open_thinking(item_id, *id)),
        };
        let part_layout = if context
            .assistant_part_cache
            .get(&part.id())
            .is_some_and(|cached| cached.key == key)
        {
            context.assistant_part_cache[&part.id()].layout.clone()
        } else {
            let part_layout =
                assistant_part_layout(part, key, context.width, context.theme, context.highlighter);
            context.assistant_part_cache.insert(
                part.id(),
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
    layout
}

fn assistant_part_layout(
    part: &AssistantPart,
    key: AssistantPartLayoutKey,
    width: u16,
    theme: &Theme,
    highlighter: &dyn Highlighter,
) -> ItemLayout {
    match part {
        AssistantPart::Text { markdown, .. } => ItemLayout {
            lines: crate::markdown::render_markdown_width(markdown, theme, highlighter, width)
                .into_iter()
                .flat_map(|line| assistant_body_line(line, width, theme))
                .collect(),
            regions: Vec::new(),
        },
        AssistantPart::Thinking { id, text, .. } => {
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
    if tool.tool == "read" && tool.status == ToolStatus::Completed {
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

fn assistant_header(width: u16, theme: &Theme) -> Vec<Line<'static>> {
    if width < 8 {
        return wrapped_line(Line::styled("[A]", theme.assistant()), width);
    }
    wrapped_line(
        Line::from(vec![
            Span::styled("╭─ ASSISTANT", theme.assistant()),
            Span::raw(" "),
        ]),
        width,
    )
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

#[cfg(test)]
pub(super) fn wrapped_labelled_text(
    affordance: Option<&str>,
    label: &str,
    label_style: Style,
    text: &str,
    width: u16,
) -> Vec<Line<'static>> {
    let mut spans = Vec::new();
    if let Some(affordance) = affordance {
        spans.push(Span::raw(affordance.to_owned()));
    }
    spans.push(Span::styled(label.to_owned(), label_style));
    spans.push(Span::raw(text.to_owned()));
    wrapped_line(Line::from(spans), width)
}

#[cfg(test)]
pub(super) fn wrapped_text(text: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    wrapped_line(Line::styled(text.to_owned(), style), width)
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
fn inner_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use cookie_agent_protocol::{
        AgentDescriptor, ApprovalBoundary, ApprovalCapability, ApprovalConstraints,
        ApprovalDecisionSource, ApprovalEvaluation, ApprovalFinalDecision, ApprovalFinalOutcome,
        ApprovalId, ApprovalInternalDecision, ApprovalInternalDecisionKind, ApprovalListResult,
        ApprovalReasonCode, ApprovalRecord, ApprovalRequest, ApprovalResourceSource,
        ApprovalRespondErrorCode, ApprovalStatus, ApprovalTrigger, ApprovalUserDecision,
        CatalogProvider, Event, EventEnvelope, EventSchemaVersion, EventSubscriptionMessage,
        MatchedPermissionRule, OperationFingerprint, PreparedApprovalResource,
        PreparedBindingLifetime, PreparedCapabilityOperation, PreparedOperationIdentity,
        PreparedResourceDigest, PreparedResourceIdentity, SessionId, SessionMeta, SessionTree,
        Sha256Digest, ToolCallFailureCode, ToolCallId, ToolResult,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Modifier},
        widgets::ListState,
    };

    use super::*;
    use crate::markdown::{PlainHighlighter, SyntectHighlighter};
    use crate::state::{ApprovalState, OrderedOutput, SessionState, StateStore, ToolCallState};
    use crate::ui::app::*;
    use crate::ui::events::{RenderScheduler, TerminalCleanup, TerminalRestore};
    use crate::ui::input::{CredentialInput, InputState, credential_wipe_count};
    use crate::ui::slash::{
        BlockCommand, COMMANDS, InputMode, ScrollCommand, SlashCommand, Submission,
        command_allowed_in_mode, command_help, command_spec, parse_submission,
    };
    use crate::ui::terminal_layout;
    use crate::{Client, ClientDelivery};

    use async_trait::async_trait;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use cookie_agent_protocol::{
        ActionKind, AgentType, DecisionTrace, DelegationSnapshot, DepthLimit, Effect, OutputDelta,
        OutputStream, ProfileSnapshot, SessionOrigin,
    };
    use cookie_agent_server::{MessageFrame, MessageStream, TransportError};
    use jiff::Timestamp;
    use serde_json::{Value, json};
    use zeroize::Zeroize;

    struct NeverStream;

    #[async_trait]
    impl MessageStream for NeverStream {
        async fn send(&mut self, _: MessageFrame) -> Result<(), TransportError> {
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
            std::future::pending().await
        }
    }

    struct RecordingStream {
        sent: Arc<Mutex<Vec<Value>>>,
        request_events: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
        replies: tokio::sync::mpsc::UnboundedReceiver<MessageFrame>,
        reply_sender: tokio::sync::mpsc::UnboundedSender<MessageFrame>,
        created_sessions: HashMap<SessionId, SessionMeta>,
        block_run_start: bool,
        block_first_stdin: bool,
        stdin_seen: bool,
        agents: Vec<AgentDescriptor>,
        /// Methods whose responses are held until released, to exercise
        /// optimistic UI states deterministically without sleeps.
        held_methods: Vec<&'static str>,
        held: Arc<Mutex<Vec<(String, MessageFrame)>>>,
    }

    #[async_trait]
    impl MessageStream for RecordingStream {
        async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
            let request = match frame {
                MessageFrame::Value(request) => request,
                MessageFrame::Text(mut text) => {
                    let request = serde_json::from_str(&text).expect("client request JSON");
                    text.zeroize();
                    request
                }
            };
            if let Some(request_events) = &self.request_events {
                let _ = request_events.send(request.clone());
            }
            let mut recorded = request.clone();
            if recorded["method"] == "provider.connect"
                && let Some(values) = recorded["params"]["credentials"]["values"].as_object_mut()
            {
                for value in values.values_mut() {
                    *value = Value::String("<redacted>".into());
                }
            }
            self.sent.lock().expect("sent requests lock").push(recorded);
            if self.block_run_start && request["method"] == "run.start" {
                return Ok(());
            }
            if self.block_first_stdin && request["method"] == "run.tool_stdin" && !self.stdin_seen {
                self.stdin_seen = true;
                return Ok(());
            }
            let method = request["method"].as_str().unwrap_or_default().to_owned();
            let result = match request["method"].as_str() {
                Some("session.create") => {
                    let profile = request["params"]["profile"]
                        .as_str()
                        .expect("session profile")
                        .to_owned();
                    let cwd = request["params"]["cwd"]
                        .as_str()
                        .expect("session cwd")
                        .to_owned();
                    let agent_type = if profile == "reviewer" {
                        AgentType::All
                    } else {
                        AgentType::Primary
                    };
                    let session =
                        session_meta_with_identity(SessionId::new_v7(), &profile, &cwd, agent_type);
                    self.created_sessions.insert(session.id, session.clone());
                    json!({ "session": session })
                }
                Some("session.list") => json!({ "sessions": [] }),
                Some("session.tree") => {
                    let session_id =
                        serde_json::from_value(request["params"]["session_id"].clone())
                            .expect("session id");
                    let session = self
                        .created_sessions
                        .get(&session_id)
                        .cloned()
                        .unwrap_or_else(|| session_meta(session_id));
                    json!({ "tree": { "session": session, "children": [] } })
                }
                Some("run.start") => {
                    json!({ "run_id": cookie_agent_protocol::RunId::new_v7() })
                }
                Some("run.steer") => json!({ "accepted": true }),
                Some("run.cancel") => json!({ "cancelled": true }),
                Some("events.subscribe") => json!({ "events": [] }),
                Some("run.tool_stdin") => json!({ "accepted": true }),
                Some("approval.respond") => json!({
                    "approval_id": request["params"]["approval_id"],
                    "decision": request["params"]["decision"],
                }),
                Some("provider.connect") => json!({
                    "client_connect_id": request["params"]["client_connect_id"],
                    "connection": {
                        "provider_id": request["params"]["provider_id"],
                        "credential_fields": ["API_KEY"],
                        "connected_at": Timestamp::now(),
                        "catalog_revision": request["params"]["catalog_revision"],
                    },
                    "model_revision": "sha256:test",
                }),
                Some("model.list") => json!({
                    "revision": "sha256:test",
                    "generated_at": Timestamp::now(),
                    "catalog_revision": "catalog-test",
                    "models": [],
                }),
                Some("agent.list") => json!({ "agents": &self.agents }),
                Some("catalog.provider.list") => json!({
                    "snapshot": {
                        "revision": "catalog-test",
                        "source": "test",
                        "fetched_at": Timestamp::now(),
                    },
                    "providers": [],
                }),
                _ => Value::Null,
            };
            let response = MessageFrame::Value(json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": result,
            }));
            if self.held_methods.contains(&method.as_str()) {
                // Held until the test flushes — no timers involved.
                self.held
                    .lock()
                    .expect("held responses lock")
                    .push((method, response));
                return Ok(());
            }
            self.reply_sender
                .send(response)
                .expect("connection receives response");
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
            Ok(self.replies.recv().await)
        }
    }

    fn recording_client() -> (Client, Arc<Mutex<Vec<Value>>>) {
        let (client, sent, _request_events) = recording_client_with_request_events();
        (client, sent)
    }

    fn recording_client_with_request_events() -> (
        Client,
        Arc<Mutex<Vec<Value>>>,
        tokio::sync::mpsc::UnboundedReceiver<Value>,
    ) {
        let (reply_sender, replies) = tokio::sync::mpsc::unbounded_channel();
        let (request_events, request_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            Client::connect_stream(RecordingStream {
                sent: sent.clone(),
                request_events: Some(request_events),
                replies,
                reply_sender,
                created_sessions: HashMap::new(),
                block_run_start: false,
                block_first_stdin: false,
                stdin_seen: false,
                agents: vec![AgentDescriptor {
                    name: "primary".into(),
                    agent_type: AgentType::Primary,
                    enabled: true,
                    models: Vec::new(),
                }],
                held_methods: Vec::new(),
                held: Arc::new(Mutex::new(Vec::new())),
            }),
            sent,
            request_event_rx,
        )
    }

    async fn wait_for_request(
        requests: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
        description: &str,
        matches: impl Fn(&Value) -> bool,
    ) -> Value {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let request = requests
                    .recv()
                    .await
                    .expect("recording stream remains open");
                if matches(&request) {
                    return request;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
    }

    async fn wait_for_tree_update(
        app: &mut App,
        session_id: SessionId,
        request_id: u64,
    ) -> RpcUpdate {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let update = app
                    .rpc_updates_rx
                    .recv()
                    .await
                    .expect("app RPC update channel remains open");
                if matches!(
                    &update,
                    RpcUpdate::Tree {
                        session_id: update_session_id,
                        request_id: update_request_id,
                        ..
                    } if *update_session_id == session_id && *update_request_id == request_id
                ) {
                    return update;
                }
            }
        })
        .await
        .expect("timed out waiting for exact session.tree response")
    }

    fn hanging_start_client() -> (Client, Arc<Mutex<Vec<Value>>>) {
        let (reply_sender, replies) = tokio::sync::mpsc::unbounded_channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            Client::connect_stream(RecordingStream {
                sent: sent.clone(),
                request_events: None,
                replies,
                reply_sender,
                created_sessions: HashMap::new(),
                block_run_start: true,
                block_first_stdin: false,
                stdin_seen: false,
                agents: vec![AgentDescriptor {
                    name: "primary".into(),
                    agent_type: AgentType::Primary,
                    enabled: true,
                    models: Vec::new(),
                }],
                held_methods: Vec::new(),
                held: Arc::new(Mutex::new(Vec::new())),
            }),
            sent,
        )
    }

    fn hanging_stdin_client() -> (Client, Arc<Mutex<Vec<Value>>>) {
        let (reply_sender, replies) = tokio::sync::mpsc::unbounded_channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            Client::connect_stream(RecordingStream {
                sent: sent.clone(),
                request_events: None,
                replies,
                reply_sender,
                created_sessions: HashMap::new(),
                block_run_start: false,
                block_first_stdin: true,
                stdin_seen: false,
                agents: vec![AgentDescriptor {
                    name: "primary".into(),
                    agent_type: AgentType::Primary,
                    enabled: true,
                    models: Vec::new(),
                }],
                held_methods: Vec::new(),
                held: Arc::new(Mutex::new(Vec::new())),
            }),
            sent,
        )
    }

    type HeldResponses = Arc<Mutex<Vec<(String, MessageFrame)>>>;
    type ReplySender = tokio::sync::mpsc::UnboundedSender<MessageFrame>;

    /// A client whose `approval.respond` replies are held until the test
    /// flushes them — deterministic in-flight control without sleeps.
    fn held_approval_client() -> (Client, Arc<Mutex<Vec<Value>>>, HeldResponses, ReplySender) {
        let (reply_sender, replies) = tokio::sync::mpsc::unbounded_channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let held = Arc::new(Mutex::new(Vec::new()));
        (
            Client::connect_stream(RecordingStream {
                sent: sent.clone(),
                request_events: None,
                replies,
                reply_sender: reply_sender.clone(),
                created_sessions: HashMap::new(),
                block_run_start: false,
                block_first_stdin: false,
                stdin_seen: false,
                agents: vec![AgentDescriptor {
                    name: "primary".into(),
                    agent_type: AgentType::Primary,
                    enabled: true,
                    models: Vec::new(),
                }],
                held_methods: vec!["approval.respond"],
                held: held.clone(),
            }),
            sent,
            held,
            reply_sender,
        )
    }

    fn empty_setup_client() -> (Client, Arc<Mutex<Vec<Value>>>) {
        let (reply_sender, replies) = tokio::sync::mpsc::unbounded_channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            Client::connect_stream(RecordingStream {
                sent: sent.clone(),
                request_events: None,
                replies,
                reply_sender,
                created_sessions: HashMap::new(),
                block_run_start: false,
                block_first_stdin: false,
                stdin_seen: false,
                agents: Vec::new(),
                held_methods: Vec::new(),
                held: Arc::new(Mutex::new(Vec::new())),
            }),
            sent,
        )
    }

    fn session_meta(id: SessionId) -> SessionMeta {
        session_meta_for(id, "primary")
    }

    fn session_meta_for(id: SessionId, profile: &str) -> SessionMeta {
        session_meta_with_identity(id, profile, "/workspace", AgentType::All)
    }

    fn session_meta_with_identity(
        id: SessionId,
        profile: &str,
        cwd: &str,
        agent_type: AgentType,
    ) -> SessionMeta {
        SessionMeta {
            id,
            origin: SessionOrigin::Root,
            cwd: cwd.into(),
            profile: ProfileSnapshot {
                name: profile.into(),
                agent_type,
                models: Vec::new(),
                tools: Vec::new(),
                delegation: DelegationSnapshot {
                    enabled: false,
                    allowed_profiles: Vec::new(),
                    depth_limit: DepthLimit::Finite(0),
                    result_limit_bytes: 0,
                },
                permission_rules: Vec::new(),
            },
            title: None,
        }
    }

    fn approval_request() -> ApprovalRequest {
        let resource = PreparedApprovalResource {
            capability: ActionKind::Bash,
            canonical: PreparedResourceIdentity::new("command:git-status")
                .expect("prepared resource identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
            binding_lifetime: PreparedBindingLifetime::RestartStable,
            boundary: ApprovalBoundary::CommandPrefix {
                prefix: "git status".into(),
            },
            source: ApprovalResourceSource::PrimaryOperation,
        };
        let resource_digest = resource.binding_digest.clone();
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"normalized arguments"),
            vec![ApprovalCapability {
                action: ActionKind::Bash,
                operation: PreparedCapabilityOperation::new("execute")
                    .expect("prepared capability operation"),
            }],
            vec![resource.clone()],
            Sha256Digest::of_bytes(b"execution context"),
        )
        .expect("prepared operation");
        ApprovalRequest::new(
            fixed_approval_id(),
            9,
            ApprovalTrigger::PermissionPolicy,
            operation,
            vec![ApprovalEvaluation {
                resource_digest,
                effect: Effect::Ask,
                trace: DecisionTrace {
                    action: ActionKind::Bash,
                    normalized_resource: "git status".into(),
                    candidates: vec![MatchedPermissionRule {
                        rule_id: Some("workspace-bash-review".into()),
                        source_layer: "workspace".into(),
                        effect: Effect::Ask,
                    }],
                    effect: Effect::Ask,
                    precedence_reason: "test".into(),
                },
            }],
            ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: true,
                cancellable: true,
                expires_at: None,
            },
        )
        .expect("approval request")
    }

    fn approval_record(session_id: SessionId, status: ApprovalStatus) -> ApprovalRecord {
        ApprovalRecord {
            session_id,
            request: approval_request(),
            status,
            internal_decision: None,
            user_decision: None,
            final_decision: None,
        }
    }

    fn approval(session_id: SessionId) -> ApprovalState {
        crate::state::approval_state_from_record(approval_record(
            session_id,
            ApprovalStatus::Escalated,
        ))
        .expect("visible approval")
    }

    fn filesystem_approval(session_id: SessionId) -> ApprovalState {
        let primary = PreparedApprovalResource {
            capability: ActionKind::Read,
            canonical: PreparedResourceIdentity::new("filesystem:workspace-config")
                .expect("prepared resource identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                b"workspace config descriptor",
            ),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::PrimaryOperation,
        };
        let external = PreparedApprovalResource {
            capability: ActionKind::ExternalDirectory,
            canonical: PreparedResourceIdentity::new("directory:external-config")
                .expect("prepared resource identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                b"external directory descriptor",
            ),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::ExternalDirectoryGuard,
        };
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"normalized filesystem arguments"),
            vec![
                ApprovalCapability {
                    action: ActionKind::Read,
                    operation: PreparedCapabilityOperation::new("open-read")
                        .expect("prepared capability operation"),
                },
                ApprovalCapability {
                    action: ActionKind::ExternalDirectory,
                    operation: PreparedCapabilityOperation::new("authorize-boundary")
                        .expect("prepared capability operation"),
                },
            ],
            vec![primary.clone(), external.clone()],
            Sha256Digest::of_bytes(b"filesystem execution context"),
        )
        .expect("prepared operation");
        let evaluations = [
            (
                &primary,
                "./.cookie/config.toml",
                "workspace read requires review",
                "workspace-read-review",
                "workspace",
            ),
            (
                &external,
                "../shared-config",
                "model requested access outside the workspace",
                "external-directory-review",
                "global",
            ),
        ]
        .into_iter()
        .map(
            |(resource, normalized_resource, reason, rule_id, source_layer)| ApprovalEvaluation {
                resource_digest: resource.binding_digest.clone(),
                effect: Effect::Ask,
                trace: DecisionTrace {
                    action: resource.capability,
                    normalized_resource: normalized_resource.into(),
                    candidates: vec![MatchedPermissionRule {
                        rule_id: Some(rule_id.into()),
                        source_layer: source_layer.into(),
                        effect: Effect::Ask,
                    }],
                    effect: Effect::Ask,
                    precedence_reason: reason.into(),
                },
            },
        )
        .collect();
        ApprovalState {
            session_id,
            approval_id: fixed_approval_id(),
            request_revision: 12,
            operation_fingerprint: OperationFingerprint::from_prepared_operation(&operation),
            trigger: ApprovalTrigger::ModelToolApproval,
            normalized_arguments_digest: operation.normalized_arguments_digest().clone(),
            execution_context_digest: operation.execution_context_digest().clone(),
            capability_lifetime: operation.capability_lifetime(),
            capabilities: operation.capabilities().to_vec(),
            resources: operation.resources().to_vec(),
            evaluations,
            constraints: ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: false,
                cancellable: true,
                expires_at: None,
            },
            escalated: true,
        }
    }

    fn fixed_approval_id() -> ApprovalId {
        serde_json::from_value(json!("01900000-0000-7000-8000-000000000001"))
            .expect("fixed approval id")
    }

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

    fn approval_terminal_snapshot(
        approval: &ApprovalState,
        width: u16,
        height: u16,
        no_color: bool,
        scroll_to_end: bool,
    ) -> String {
        let mut app = test_app();
        if no_color {
            app.theme = Theme::from_environment("dark", true, "xterm", "truecolor");
        }
        if scroll_to_end {
            app.approval_scroll_request = Some((approval.approval_id, approval.request_revision));
            app.approval_scroll = u16::MAX;
        }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                app.render_approval(frame, approval, frame.area());
            })
            .expect("approval render");
        let buffer = terminal.backend().buffer();
        let rendered = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        stable_approval_snapshot(approval, rendered)
    }

    fn test_app() -> App {
        let (rpc_updates_tx, rpc_updates_rx) = tokio::sync::mpsc::unbounded_channel();
        App {
            client: Client::connect_stream(NeverStream),
            deliveries: None,
            rpc_updates_tx,
            rpc_updates_rx,
            subscription_lanes: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            stdin_lanes: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            store: StateStore::default(),
            sessions: Vec::new(),
            agents: Vec::new(),
            providers: Vec::new(),
            catalog_revision: None,
            draft_agent_profile: Some("primary".into()),
            connect_provider: None,
            connect_fields: Vec::new(),
            connect_field_index: 0,
            connect_task: None,
            tree: None,
            selected: None,
            tree_root: None,
            selection_generation: 0,
            tree_subscription_sessions: HashSet::new(),
            tree_refresh_in_flight: None,
            tree_refresh_pending: false,
            next_tree_refresh_id: 0,
            tree_cursor: None,
            tree_offset: 0,
            tree_viewport_height: 0,
            collapsed_sessions: HashSet::new(),
            expanded_blocks: HashMap::new(),
            selected_block: None,
            reveal_selected_block: false,
            conversation_scroll: ConversationScroll::default(),
            scrollbar_geometry: None,
            scrollbar_drag: None,
            approval_scroll: 0,
            approval_max_scroll: 0,
            approval_scroll_request: None,
            pending_approval: None,
            next_approval_request_id: 0,
            approval_refresh_in_flight: None,
            next_approval_refresh_id: 0,
            layout_cache: LayoutCache::default(),
            tui_config: crate::config::TuiConfig::default(),
            theme: Theme::default(),
            highlighter: Box::<SyntectHighlighter>::default(),
            hit_map: UiHitMap::default(),
            transient_notices: Vec::new(),
            picker_state: ListState::default().with_selected(Some(0)),
            picker_query: String::new(),
            palette_state: ListState::default().with_selected(Some(0)),
            palette_dismissed: false,
            last_escape: None,
            input: InputState::default(),
            modal: Modal::None,
            input_mode: InputMode::Message,
            input_focused: true,
            stdin_target: None,
            status: String::new(),
            should_quit: false,
        }
    }

    fn state_with_blocks() -> (SessionState, cookie_agent_protocol::ToolCallId) {
        let tool_id = cookie_agent_protocol::ToolCallId::new_v7();
        let mut state = SessionState {
            transcript: vec![
                TranscriptItem::assistant_parts(vec![AssistantPart::Thinking {
                    id: 7,
                    version: 0,
                    text: "abcdef".into(),
                }]),
                TranscriptItem::tool(8, tool_id),
            ],
            ..SessionState::default()
        };
        state.tools.insert(
            tool_id,
            ToolCallState {
                id: tool_id,
                tool: "bash".into(),
                arguments: "{\"command\":\"status\"}".into(),
                status: ToolStatus::Completed,
                detail: "done".into(),
            },
        );
        (state, tool_id)
    }

    fn projection_event(session_id: SessionId, seq: u64, event: Event) -> EventEnvelope {
        EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id,
            run_id: None,
            seq,
            timestamp: Timestamp::now(),
            event,
        }
    }

    #[test]
    fn terminal_layout_has_exact_rects_for_wide_square_tall_and_tiny_terminals() {
        for (area, expected) in [
            (
                Rect::new(0, 0, 160, 50),
                crate::ui::UiLayout {
                    agent: Rect::new(0, 0, 160, 5),
                    conversation: Rect::new(0, 5, 160, 39),
                    status: Rect::new(0, 44, 160, 1),
                    input: Rect::new(0, 45, 160, 5),
                },
            ),
            (
                Rect::new(0, 0, 80, 80),
                crate::ui::UiLayout {
                    agent: Rect::new(0, 0, 80, 5),
                    conversation: Rect::new(0, 5, 80, 69),
                    status: Rect::new(0, 74, 80, 1),
                    input: Rect::new(0, 75, 80, 5),
                },
            ),
            (
                Rect::new(0, 0, 60, 100),
                crate::ui::UiLayout {
                    agent: Rect::new(0, 0, 60, 5),
                    conversation: Rect::new(0, 5, 60, 89),
                    status: Rect::new(0, 94, 60, 1),
                    input: Rect::new(0, 95, 60, 5),
                },
            ),
            (
                Rect::new(0, 0, 20, 8),
                crate::ui::UiLayout {
                    agent: Rect::new(0, 0, 20, 1),
                    conversation: Rect::new(0, 1, 20, 1),
                    status: Rect::new(0, 2, 20, 1),
                    input: Rect::new(0, 3, 20, 5),
                },
            ),
        ] {
            assert_eq!(terminal_layout(area), expected);
        }
    }

    #[tokio::test]
    async fn rendered_panels_follow_the_single_full_width_layout_at_all_target_sizes() {
        for (width, height) in [(160, 50), (80, 80), (60, 100), (20, 8)] {
            let mut app = test_app();
            app.status = "ready".into();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| app.draw_for_test(frame))
                .expect("render agents panel");

            let layout = terminal_layout(Rect::new(0, 0, width, height));
            let buffer = terminal.backend().buffer();
            for rect in [layout.agent, layout.input] {
                assert_eq!(buffer[(rect.x, rect.y)].symbol(), "┌");
                assert_eq!(buffer[(rect.x + rect.width - 1, rect.y)].symbol(), "┐");
                if rect.height > 1 {
                    assert_eq!(buffer[(rect.x, rect.y + rect.height - 1)].symbol(), "└");
                    assert_eq!(
                        buffer[(rect.x + rect.width - 1, rect.y + rect.height - 1)].symbol(),
                        "┘"
                    );
                }
            }
            assert_eq!(
                buffer[(layout.conversation.x, layout.conversation.y)].symbol(),
                "┌"
            );
            assert_eq!(
                buffer[(
                    layout.conversation.x + layout.conversation.width - 1,
                    layout.conversation.y
                )]
                    .symbol(),
                "┐"
            );
            assert_eq!(layout.agent, Rect::new(0, 0, width, layout.agent.height));
            assert_eq!(layout.conversation.x, 0);
            assert_eq!(layout.conversation.width, width);
            assert_eq!(layout.conversation.y, layout.agent.height);
            assert_eq!(layout.status.x, 0);
            assert_eq!(layout.status.width, width);
            assert_eq!(layout.status.y, layout.conversation.bottom());
            assert_eq!(layout.input.x, 0);
            assert_eq!(layout.input.width, width);
            assert_eq!(layout.input.bottom(), height);
            assert_eq!(layout.input.y, layout.status.bottom());
            assert_eq!(app.hit_map.input.expect("input hit").text_rect.height, 3);
        }
    }

    #[tokio::test]
    async fn full_app_degrades_safely_on_tiny_terminals() {
        for (width, height) in [(1, 1), (2, 2), (4, 3), (8, 4)] {
            let mut app = test_app();
            app.input.set_buffer("👩‍💻\ntext".into());
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| app.draw_for_test(frame))
                .expect("tiny app render");
            assert_eq!(
                terminal.backend().buffer().area,
                Rect::new(0, 0, width, height)
            );
        }
    }

    #[tokio::test]
    async fn tree_viewport_keeps_the_selected_node_reachable_at_all_target_sizes() {
        let root = SessionId::new_v7();
        let mut app = test_app();
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: (0..30)
                .map(|_| SessionTree {
                    session: session_meta(SessionId::new_v7()),
                    children: Vec::new(),
                })
                .collect(),
        });
        app.tree_root = Some(root);
        app.tree_cursor = app
            .tree
            .as_ref()
            .expect("tree")
            .children
            .last()
            .map(|child| child.session.id);

        for (width, height) in [(40, 12), (80, 24), (160, 50)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| app.draw_for_test(frame))
                .expect("tree render");
            let cursor = app.tree_cursor.expect("cursor");
            assert!(
                app.hit_map
                    .tree_rows
                    .iter()
                    .any(|hit| hit.session_id == cursor)
            );
        }
    }

    #[test]
    fn render_scheduler_coalesces_streams_and_prioritizes_input() {
        let now = Instant::now();
        let mut scheduler = RenderScheduler::default();
        assert!(scheduler.should_draw(now));
        scheduler.drew(now);
        assert!(!scheduler.should_draw(now));
        for _ in 0..20 {
            scheduler.mark_stream();
        }
        assert!(!scheduler.should_draw(now + Duration::from_millis(32)));
        assert!(scheduler.should_draw(now + Duration::from_millis(33)));
        scheduler.drew(now + Duration::from_millis(33));
        scheduler.mark_immediate();
        assert!(scheduler.should_draw(now + Duration::from_millis(34)));
    }

    #[test]
    fn resize_event_autoresizes_marks_immediate_and_reflows_navigation_width() {
        let mut input = InputState::default();
        input.set_buffer("abcdefghij".into());
        let mut terminal = Terminal::new(TestBackend::new(12, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                crate::ui::input::render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    &Theme::default(),
                );
            })
            .expect("initial input render");

        let now = Instant::now();
        let mut scheduler = RenderScheduler::default();
        scheduler.drew(now);
        terminal.backend_mut().resize(8, 5);
        handle_terminal_resize(&mut terminal, &mut scheduler).expect("terminal autoresize");
        assert_eq!(terminal.size().expect("terminal size").width, 8);
        assert!(scheduler.should_draw(now));

        terminal
            .draw(|frame| {
                crate::ui::input::render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    &Theme::default(),
                );
            })
            .expect("resized input render");
        input.move_up();
        assert_eq!(input.cursor_byte(), 4);
    }

    #[tokio::test]
    async fn input_rpc_is_spawned_and_deliveries_continue_processing() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, SessionState::default());

        // NeverStream never answers RPCs; this would hang if submission awaited it.
        app.submit_prompt("hello".into()).await;
        app.handle_delivery(ClientDelivery::RecoveryFailed {
            session_id: Some(session_id),
            error: "test recovery failure".into(),
        })
        .await;
        assert_eq!(
            app.status,
            format!("recovery for {session_id} failed: test recovery failure")
        );
    }

    #[tokio::test]
    async fn a_hung_rpc_does_not_block_a_later_ui_action() {
        let session_id = SessionId::new_v7();
        let run_id = cookie_agent_protocol::RunId::new_v7();
        let (client, sent) = hanging_start_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, SessionState::default());

        app.submit_prompt("hello".into()).await;
        app.store.sessions.insert(
            session_id,
            SessionState {
                active_run: Some(run_id),
                ..SessionState::default()
            },
        );
        app.cancel_active_run();

        for _ in 0..20 {
            tokio::task::yield_now().await;
            if sent.lock().expect("sent requests lock").len() >= 2 {
                break;
            }
        }
        let requests = sent.lock().expect("sent requests lock").clone();
        let methods = requests
            .iter()
            .map(|request| request["method"].as_str().expect("method"))
            .collect::<Vec<_>>();
        assert!(methods.contains(&"run.start"));
        assert!(methods.contains(&"run.cancel"));
    }

    #[tokio::test]
    async fn a_hung_first_stdin_write_releases_its_per_call_lane() {
        let session_id = SessionId::new_v7();
        let run_id = cookie_agent_protocol::RunId::new_v7();
        let call_id = ToolCallId::new_v7();
        let (client, sent) = hanging_stdin_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.stdin_target = Some(call_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                active_run: Some(run_id),
                tools: HashMap::from([(
                    call_id,
                    ToolCallState {
                        id: call_id,
                        tool: "bash".into(),
                        arguments: String::new(),
                        status: ToolStatus::Running,
                        detail: String::new(),
                    },
                )]),
                ..SessionState::default()
            },
        );

        app.send_stdin("first".into(), false).await;
        app.send_stdin("second".into(), false).await;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if sent.lock().expect("sent requests lock").len() >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("second stdin write was not released");

        let requests = sent.lock().expect("sent requests lock").clone();
        let stdin = requests
            .iter()
            .filter(|request| request["method"] == "run.tool_stdin")
            .collect::<Vec<_>>();
        assert_eq!(stdin.len(), 2);
        assert_eq!(stdin[0]["params"]["data"], STANDARD.encode(b"first"));
        assert_eq!(stdin[1]["params"]["data"], STANDARD.encode(b"second"));
    }

    #[test]
    fn layout_cache_invalidates_for_key_changes_and_viewport_slices() {
        let session = SessionId::new_v7();
        let (mut state, _) = state_with_blocks();
        let mut cache = LayoutCache::default();
        let theme = Theme::default();
        let highlighter = PlainHighlighter;
        let hit = ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            None,
            None,
            80,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        );
        assert!(!hit);
        assert_eq!(cache.item_layout_passes, 2);
        let hit = ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            None,
            None,
            80,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        );
        assert!(hit);
        assert_eq!(cache.item_layout_passes, 2);
        let hit = ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            None,
            None,
            81,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        );
        assert!(!hit);
        assert_eq!(cache.item_layout_passes, 4);
        state.version += 1; // A replay projection install advances the version.
        state
            .transcript
            .push(TranscriptItem::assistant("replacement"));
        let hit = ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            None,
            None,
            81,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        );
        assert!(!hit);
        assert_eq!(cache.item_layout_passes, 5);
        assert!(
            cache
                .layout
                .lines
                .iter()
                .any(|line| line.to_string().contains("replacement"))
        );
        let expanded = HashSet::from([BlockId::Thinking(7)]);
        let hit = ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            Some(&expanded),
            None,
            81,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        );
        assert!(!hit);
        assert_eq!(cache.item_layout_passes, 6);
        let hit = ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            Some(&expanded),
            None,
            81,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        );
        assert!(hit);
        assert_eq!(cache.item_layout_passes, 6);
        let hit = ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            Some(&expanded),
            Some(BlockId::Thinking(7)),
            81,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        );
        assert!(!hit);
        assert_eq!(cache.item_layout_passes, 7);
        let selected_line = cache
            .layout
            .lines
            .iter()
            .find(|line| line.to_string().contains("▾ thinking"))
            .expect("selected thinking row");
        // Exactly one chevron: selection is style-only (underline), no glyph.
        assert!(!selected_line.to_string().contains('▶'));
        assert!(lines_contain_underlined(
            std::slice::from_ref(selected_line),
            "thinking"
        ));
        let hit = ensure_cached_transcript_layout(
            &mut cache,
            SessionId::new_v7(),
            &state,
            Some(&expanded),
            None,
            81,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        );
        assert!(!hit);

        let layout = TranscriptLayout {
            lines: (0..6).map(|line| Line::from(line.to_string())).collect(),
            regions: vec![BlockRegion {
                id: BlockId::Thinking(1),
                start_line: 2,
                end_line: 5,
            }],
        };
        let visible = layout
            .lines
            .iter()
            .skip(2)
            .take(2)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(visible, vec!["2", "3"]);
        let hit = block_hit(layout.regions[0], Rect::new(0, 10, 20, 2), 2).expect("block hit");
        assert_eq!(hit.rect, Rect::new(0, 10, 20, 2));
    }

    #[test]
    fn five_hundred_stream_deltas_stay_within_incremental_parse_and_layout_budgets() {
        let session = SessionId::new_v7();
        let mut transcript = (1..=64)
            .map(|id| TranscriptItem::User {
                id,
                version: 0,
                text: format!("history item {id}"),
            })
            .collect::<Vec<_>>();
        let stable = format!("{}\n\nopen", "stable block ".repeat(400));
        transcript.push(TranscriptItem::Assistant {
            id: 65,
            version: 0,
            parts: vec![AssistantPart::Text {
                id: 65,
                version: 0,
                markdown: crate::markdown::MarkdownDocument::new(stable),
            }],
        });
        let mut state = SessionState {
            generation: 7,
            transcript,
            ..SessionState::default()
        };
        let mut cache = LayoutCache::default();
        let theme = Theme::default();
        let highlighter = PlainHighlighter;
        assert!(!ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            None,
            None,
            80,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        ));
        assert_eq!(cache.item_layout_passes, 65);

        for _ in 0..500 {
            let Some(TranscriptItem::Assistant { version, parts, .. }) =
                state.transcript.last_mut()
            else {
                panic!("assistant tail");
            };
            let [
                AssistantPart::Text {
                    version: part_version,
                    markdown,
                    ..
                },
            ] = parts.as_mut_slice()
            else {
                panic!("assistant text child");
            };
            markdown.append("x");
            *part_version = part_version.wrapping_add(1);
            *version = version.wrapping_add(1);
            assert!(!ensure_cached_transcript_layout(
                &mut cache,
                session,
                &state,
                None,
                None,
                80,
                &theme,
                &highlighter,
                crate::state::EventLevel::Debug,
            ));
        }

        let TranscriptItem::Assistant { parts, .. } = state.transcript.last().unwrap() else {
            panic!("assistant tail");
        };
        let [AssistantPart::Text { markdown, .. }] = parts.as_slice() else {
            panic!("assistant text child");
        };
        assert_eq!(markdown.parse_passes(), 501);
        assert_eq!(markdown.reference_reparses(), 0);
        assert!(markdown.stable_prefix_len() > 5_000);
        assert!(markdown.parsed_bytes() < 150_000);
        assert_eq!(cache.item_layout_passes, 565);
        assert_eq!(cache.assistant_part_layout_passes, 501);
    }

    #[tokio::test]
    async fn hit_map_uses_the_single_panel_geometry() {
        let mut app = test_app();
        let mut terminal = Terminal::new(TestBackend::new(140, 80)).expect("wide terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("wide render");
        let wide = terminal_layout(Rect::new(0, 0, 140, 80));
        assert_eq!(app.hit_map.tree, Some(inner_rect(wide.agent)));
        assert_eq!(
            app.hit_map.conversation,
            Some(inner_rect(wide.conversation))
        );

        let mut terminal = Terminal::new(TestBackend::new(60, 100)).expect("tall terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("tall render");
        let tall = terminal_layout(Rect::new(0, 0, 60, 100));
        assert_eq!(app.hit_map.tree, Some(inner_rect(tall.agent)));
        assert_eq!(
            app.hit_map.conversation,
            Some(inner_rect(tall.conversation))
        );
    }

    #[test]
    fn input_wraps_by_display_columns_without_splitting_graphemes() {
        let mut input = InputState::default();
        input.set_buffer("abcdef".into());
        assert_eq!(input.visual_row_count(3), 3);
        assert_eq!(input.cursor_visual_position(3), (2, 0));

        let mut wide = InputState::default();
        wide.set_buffer("界👩\u{200d}💻x".into());
        assert_eq!(UnicodeWidthStr::width("界👩\u{200d}💻"), 4);
        assert_eq!(wide.visual_row_count(4), 2);
        assert_eq!(wide.cursor_visual_position(4), (1, 1));
    }

    #[test]
    fn slash_commands_parse_and_escape_prompts() {
        let commands = [
            ("/quit", SlashCommand::Quit),
            ("/q", SlashCommand::Quit),
            ("/new", SlashCommand::New),
            ("/connect", SlashCommand::Connect),
            ("/sessions", SlashCommand::Sessions),
            ("/cancel", SlashCommand::Cancel),
            ("/stdin", SlashCommand::Stdin { next: false }),
            ("/stdin next", SlashCommand::Stdin { next: true }),
            ("/eof", SlashCommand::Eof),
            ("/message", SlashCommand::Message),
            ("/watch", SlashCommand::Watch),
            ("/tree up", SlashCommand::TreeUp),
            ("/tree down", SlashCommand::TreeDown),
            ("/tree toggle", SlashCommand::TreeToggle),
            (
                "/approve once",
                SlashCommand::Approve(ApprovalUserDecision::ApproveOnce),
            ),
            (
                "/approve tree",
                SlashCommand::Approve(ApprovalUserDecision::ApproveTree),
            ),
            (
                "/approve reject",
                SlashCommand::Approve(ApprovalUserDecision::Reject),
            ),
            (
                "/approve cancel",
                SlashCommand::Approve(ApprovalUserDecision::Cancel),
            ),
            ("/scroll up", SlashCommand::Scroll(ScrollCommand::Up(1))),
            ("/scroll up 3", SlashCommand::Scroll(ScrollCommand::Up(3))),
            (
                "/scroll down 2",
                SlashCommand::Scroll(ScrollCommand::Down(2)),
            ),
            ("/scroll top", SlashCommand::Scroll(ScrollCommand::Top)),
            (
                "/scroll bottom",
                SlashCommand::Scroll(ScrollCommand::Bottom),
            ),
            ("/block next", SlashCommand::Block(BlockCommand::Next)),
            (
                "/block previous",
                SlashCommand::Block(BlockCommand::Previous),
            ),
            ("/block prev", SlashCommand::Block(BlockCommand::Previous)),
            ("/block toggle", SlashCommand::Block(BlockCommand::Toggle)),
            ("/block clear", SlashCommand::Block(BlockCommand::Clear)),
            ("/help", SlashCommand::Help),
        ];
        for (input, expected) in commands {
            assert_eq!(parse_submission(input), Ok(Submission::Command(expected)));
        }
        assert_eq!(
            parse_submission("//foo"),
            Ok(Submission::Prompt("/foo".into()))
        );
        assert_eq!(parse_submission("//"), Ok(Submission::Prompt("/".into())));
        assert_eq!(
            parse_submission("///x"),
            Ok(Submission::Prompt("//x".into()))
        );
        assert_eq!(
            parse_submission("/quit\nstill a prompt"),
            Ok(Submission::Prompt("/quit\nstill a prompt".into()))
        );
        assert_eq!(
            parse_submission("/approve   once"),
            Ok(Submission::Command(SlashCommand::Approve(
                ApprovalUserDecision::ApproveOnce
            )))
        );
        assert!(parse_submission("/").is_err());
        assert!(parse_submission("/approve").is_err());
        assert!(parse_submission("/missing").is_err());
        assert!(parse_submission("/scroll up 0").is_err());
        assert!(parse_submission("/block missing").is_err());
    }

    #[tokio::test]
    async fn opencode_newline_keys_insert_while_bare_enter_submits_exact_multiline_text() {
        let session_id = SessionId::new_v7();
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, SessionState::default());

        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            app.input.set_buffer("first".into());
            app.handle_key(key).await;
            assert_eq!(app.input.as_str(), "first\n");
        }

        app.input.set_buffer("first\nsecond 👩‍💻\n".into());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(app.input.as_str().is_empty());
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if sent
                    .lock()
                    .expect("sent requests lock")
                    .iter()
                    .any(|request| request["method"] == "run.start")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run.start request");
        let requests = sent.lock().expect("sent requests lock");
        let request = requests
            .iter()
            .find(|request| request["method"] == "run.start")
            .expect("run.start");
        assert_eq!(request["params"]["input"], "first\nsecond 👩‍💻\n");
    }

    #[tokio::test]
    async fn multiline_paste_is_one_normalized_edit_and_never_acts_as_submit() {
        let session_id = SessionId::new_v7();
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, SessionState::default());

        app.handle_paste("one\r\ntwo\r👨‍👩‍👧‍👦");
        assert_eq!(app.input.as_str(), "one\ntwo\n👨‍👩‍👧‍👦");
        assert!(sent.lock().expect("sent requests lock").is_empty());

        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .await;
        assert_eq!(app.input.as_str(), "one\ntwo👨‍👩‍👧‍👦");
    }

    #[tokio::test]
    async fn active_run_steering_preserves_multiline_text_and_whitespace_only_input_is_ignored() {
        let session_id = SessionId::new_v7();
        let run_id = cookie_agent_protocol::RunId::new_v7();
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                active_run: Some(run_id),
                ..SessionState::default()
            },
        );

        app.input.set_buffer(" \n\t ".into());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.input.as_str(), " \n\t ");
        assert!(sent.lock().expect("sent requests lock").is_empty());

        app.input.set_buffer("  steer\nexactly  ".into());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if sent
                    .lock()
                    .expect("sent requests lock")
                    .iter()
                    .any(|request| request["method"] == "run.steer")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run.steer request");
        let requests = sent.lock().expect("sent requests lock");
        let request = requests
            .iter()
            .find(|request| request["method"] == "run.steer")
            .expect("run.steer");
        assert_eq!(request["params"]["run_id"], json!(run_id));
        assert_eq!(request["params"]["input"], "  steer\nexactly  ");
    }

    #[tokio::test]
    async fn multiline_slash_text_is_a_prompt_and_newline_dismisses_the_palette() {
        let session_id = SessionId::new_v7();
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, SessionState::default());

        app.input.set_buffer("/qu".into());
        assert!(app.command_palette_visible());
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.input.as_str(), "/qu\n");
        assert!(!app.command_palette_visible());
        app.input.insert_text("not a command");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(!app.should_quit);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if sent
                    .lock()
                    .expect("sent requests lock")
                    .iter()
                    .any(|request| request["method"] == "run.start")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run.start request");
        let requests = sent.lock().expect("sent requests lock");
        let request = requests
            .iter()
            .find(|request| request["method"] == "run.start")
            .expect("run.start");
        assert_eq!(request["params"]["input"], "/qu\nnot a command");
    }

    #[test]
    fn command_registry_drives_help_and_parser() {
        let help = command_help();
        for spec in COMMANDS {
            assert!(help.contains(spec.usage));
            assert!(command_spec(spec.name).is_some());
            for alias in spec.aliases {
                assert!(command_spec(alias).is_some());
            }
        }
        for command in [
            "/quit",
            "/new",
            "/sessions",
            "/cancel",
            "/eof",
            "/message",
            "/watch",
            "/help",
        ] {
            assert!(parse_submission(command).is_ok(), "{command}");
        }
        for command in ["/stdin", "/tree up", "/approve once", "/scroll top"] {
            assert!(parse_submission(command).is_ok(), "{command}");
        }
    }

    #[tokio::test]
    async fn command_palette_filters_navigates_executes_and_preserves_escape_input() {
        let mut app = test_app();
        app.input.set_buffer("/can".into());
        assert!(app.command_palette_visible());
        assert_eq!(app.palette_entries()[0].name, "cancel");
        app.input.set_buffer("plain".into());
        assert!(!app.command_palette_visible());

        app.input.set_buffer("/".into());
        app.palette_dismissed = false;
        app.handle_palette_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(app.palette_state.selected(), Some(1));
        app.handle_palette_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        assert_eq!(app.palette_state.selected(), Some(0));

        app.input.set_buffer("/tree".into());
        app.palette_dismissed = false;
        let tree_index = app
            .palette_entries()
            .iter()
            .position(|spec| spec.name == "tree")
            .expect("tree command");
        app.activate_palette_entry(tree_index).await;
        assert_eq!(app.input.as_str(), "/tree ");
        assert!(app.palette_dismissed);

        app.input.set_buffer("/quit".into());
        app.palette_dismissed = false;
        app.activate_palette_entry(0).await;
        assert!(app.should_quit);

        let mut app = test_app();
        app.input.set_buffer("/help".into());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert_eq!(app.input.as_str(), "/help");
        assert!(app.palette_dismissed);
    }

    #[tokio::test]
    async fn palette_argument_commands_submit_without_reopening() {
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let mut tree_app = test_app();
        tree_app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: session_meta(child),
                children: Vec::new(),
            }],
        });
        tree_app.input.set_buffer("/tree".into());
        tree_app.activate_palette_entry(0).await;
        assert_eq!(tree_app.input.as_str(), "/tree ");
        for character in "down".chars() {
            tree_app
                .handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .await;
            assert!(!tree_app.command_palette_visible());
        }
        tree_app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(tree_app.tree_cursor, Some(child));

        let session_id = SessionId::new_v7();
        let run_id = cookie_agent_protocol::RunId::new_v7();
        let call_id = cookie_agent_protocol::ToolCallId::new_v7();
        let mut stdin_app = test_app();
        stdin_app.selected = Some(session_id);
        stdin_app.store.sessions.insert(
            session_id,
            SessionState {
                active_run: Some(run_id),
                tools: HashMap::from([(
                    call_id,
                    ToolCallState {
                        id: call_id,
                        tool: "bash".into(),
                        arguments: String::new(),
                        status: ToolStatus::Running,
                        detail: String::new(),
                    },
                )]),
                ..SessionState::default()
            },
        );
        stdin_app.input.set_buffer("/stdin".into());
        stdin_app.activate_palette_entry(0).await;
        for character in "next".chars() {
            stdin_app
                .handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .await;
        }
        assert_eq!(stdin_app.input.as_str(), "/stdin next");
        stdin_app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(stdin_app.status, format!("tool stdin for {call_id}"));
        assert_eq!(stdin_app.stdin_target, Some(call_id));

        let mut scroll_app = test_app();
        scroll_app.conversation_scroll.following = false;
        scroll_app.input.set_buffer("/scroll".into());
        scroll_app.activate_palette_entry(0).await;
        for character in "bottom".chars() {
            scroll_app
                .handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .await;
        }
        scroll_app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(scroll_app.conversation_scroll.following);

        let mut unknown_app = test_app();
        unknown_app.input.set_buffer("/unknown".into());
        assert!(unknown_app.command_palette_visible());
        unknown_app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(unknown_app.status, "unknown command: /unknown");
    }

    #[tokio::test]
    async fn stale_background_tree_result_is_discarded_after_reroot() {
        let old_root = SessionId::new_v7();
        let new_root = SessionId::new_v7();
        let mut app = test_app();
        app.tree_root = Some(new_root);
        app.tree_refresh_in_flight = Some((0, 0));
        app.handle_rpc_update(RpcUpdate::Tree {
            session_id: old_root,
            generation: 0,
            request_id: 0,
            tree: Box::new(SessionTree {
                session: session_meta(old_root),
                children: Vec::new(),
            }),
        });
        assert!(app.tree.is_none());

        app.tree_refresh_in_flight = Some((0, 0));
        app.handle_rpc_update(RpcUpdate::Tree {
            session_id: new_root,
            generation: 0,
            request_id: 0,
            tree: Box::new(SessionTree {
                session: session_meta(new_root),
                children: Vec::new(),
            }),
        });
        assert_eq!(
            app.tree.as_ref().expect("current tree").session.id,
            new_root
        );
    }

    #[tokio::test]
    async fn watching_a_descendant_keeps_the_root_tree_and_refreshes_from_the_root() {
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let (client, _sent, mut requests) = recording_client_with_request_events();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(root);
        app.tree_root = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: session_meta(child),
                children: Vec::new(),
            }],
        });

        app.watch_session(child);

        assert_eq!(app.selected, Some(child));
        assert_eq!(app.tree_root, Some(root));
        assert!(
            app.tree.is_some(),
            "watching a descendant never clears the tree"
        );
        assert_eq!(app.tree_cursor, Some(child));
        // Watching refreshes from the original root, never the descendant.
        let tree_request = wait_for_request(&mut requests, "root session.tree", |request| {
            request["method"] == "session.tree"
                && request["params"]["session_id"] == root.to_string()
        })
        .await;
        assert_eq!(tree_request["params"]["session_id"], root.to_string());
        let mut later = Vec::new();
        while let Ok(request) = requests.try_recv() {
            later.push(request);
        }
        assert!(
            later
                .iter()
                .all(|request| !(request["method"] == "session.tree"
                    && request["params"]["session_id"] == child.to_string())),
            "no refresh ever queries the descendant as the root"
        );
    }

    #[tokio::test]
    async fn session_picker_selection_is_the_intentional_reroot_action() {
        let root = SessionId::new_v7();
        let other = SessionId::new_v7();
        let (client, _sent, mut requests) = recording_client_with_request_events();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(root);
        app.tree_root = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: Vec::new(),
        });
        app.sessions = vec![session_meta(root), session_meta(other)];
        app.modal = Modal::Sessions;

        app.choose_picker_entry(1).await;

        assert_eq!(app.selected, Some(other));
        assert_eq!(app.tree_root, Some(other));
        assert!(app.tree.is_none(), "a reroot drops the stale root snapshot");
        let tree_request = wait_for_request(&mut requests, "reroot session.tree", |request| {
            request["method"] == "session.tree"
                && request["params"]["session_id"] == other.to_string()
        })
        .await;
        assert_eq!(tree_request["params"]["session_id"], other.to_string());
    }

    #[tokio::test]
    async fn watching_an_unknown_session_reroots_and_requests_a_fresh_snapshot() {
        let previous = SessionId::new_v7();
        let watched = SessionId::new_v7();
        let (client, _sent, mut requests) = recording_client_with_request_events();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(previous);
        app.tree_root = Some(previous);
        app.tree = Some(SessionTree {
            session: session_meta(previous),
            children: Vec::new(),
        });

        app.watch_session(watched);

        assert_eq!(app.selected, Some(watched));
        assert_eq!(app.tree_root, Some(watched));
        assert!(app.tree.is_none());
        let update = wait_for_tree_update(&mut app, watched, 1).await;
        app.handle_rpc_update(update);
        let subscribe = wait_for_request(&mut requests, "watched events.subscribe", |request| {
            request["method"] == "events.subscribe"
                && request["params"]["session_id"] == watched.to_string()
        })
        .await;
        assert_eq!(subscribe["params"]["session_id"], watched.to_string());
        assert_eq!(app.tree.as_ref().expect("watched tree").session.id, watched);
        assert!(app.tree_refresh_in_flight.is_none());
    }

    #[tokio::test]
    async fn an_unwatched_descendant_link_refreshes_and_subscribes_the_new_grandchild() {
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let grandchild = SessionId::new_v7();
        let (client, _sent, mut requests) = recording_client_with_request_events();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(root);
        app.tree_root = Some(root);
        app.tree_refresh_in_flight = Some((0, 0));
        app.handle_rpc_update(RpcUpdate::Tree {
            session_id: root,
            generation: 0,
            request_id: 0,
            tree: Box::new(SessionTree {
                session: session_meta(root),
                children: vec![SessionTree {
                    session: session_meta(child),
                    children: Vec::new(),
                }],
            }),
        });

        app.handle_delivery(ClientDelivery::Live {
            message: Box::new(EventSubscriptionMessage::Event {
                event: EventEnvelope {
                    schema_version: EventSchemaVersion::current(),
                    session_id: child,
                    run_id: None,
                    seq: 1,
                    timestamp: Timestamp::now(),
                    event: Event::ToolCallLinked {
                        tool_call_id: ToolCallId::new_v7(),
                        child_session_id: grandchild,
                    },
                },
            }),
            generation: 0,
        })
        .await;
        assert_eq!(app.tree_refresh_in_flight, Some((0, 1)));
        let tree_request = wait_for_request(&mut requests, "root session.tree", |request| {
            request["method"] == "session.tree"
                && request["params"]["session_id"] == root.to_string()
        })
        .await;
        assert_eq!(tree_request["params"]["session_id"], root.to_string());

        app.handle_rpc_update(RpcUpdate::Tree {
            session_id: root,
            generation: 0,
            request_id: 1,
            tree: Box::new(SessionTree {
                session: session_meta(root),
                children: vec![SessionTree {
                    session: session_meta(child),
                    children: vec![SessionTree {
                        session: session_meta(grandchild),
                        children: Vec::new(),
                    }],
                }],
            }),
        });

        assert!(app.tree_subscription_sessions.contains(&grandchild));
        assert!(
            app.tree.as_ref().expect("refreshed tree").children[0]
                .children
                .iter()
                .any(|node| node.session.id == grandchild)
        );
        let subscribe = wait_for_request(&mut requests, "grandchild events.subscribe", |request| {
            request["method"] == "events.subscribe"
                && request["params"]["session_id"] == grandchild.to_string()
        })
        .await;
        assert_eq!(subscribe["params"]["session_id"], grandchild.to_string());
    }

    #[tokio::test]
    async fn every_large_tree_descendant_is_subscribed_and_can_refresh_the_root() {
        let root = SessionId::new_v7();
        let descendants = (0..129).map(|_| SessionId::new_v7()).collect::<Vec<_>>();
        let deep_descendant = *descendants.last().expect("deep descendant");
        let (client, _sent, mut requests) = recording_client_with_request_events();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(root);
        app.tree_root = Some(root);
        app.tree_refresh_in_flight = Some((0, 0));
        app.handle_rpc_update(RpcUpdate::Tree {
            session_id: root,
            generation: 0,
            request_id: 0,
            tree: Box::new(SessionTree {
                session: session_meta(root),
                children: descendants
                    .iter()
                    .copied()
                    .map(|session_id| SessionTree {
                        session: session_meta(session_id),
                        children: Vec::new(),
                    })
                    .collect(),
            }),
        });

        assert_eq!(app.selected, Some(root));
        assert_eq!(app.tree_subscription_sessions.len(), 130);
        assert!(app.tree_subscription_sessions.contains(&deep_descendant));

        app.handle_delivery(ClientDelivery::Live {
            message: Box::new(EventSubscriptionMessage::Event {
                event: EventEnvelope {
                    schema_version: EventSchemaVersion::current(),
                    session_id: deep_descendant,
                    run_id: None,
                    seq: 1,
                    timestamp: Timestamp::now(),
                    event: Event::ToolCallLinked {
                        tool_call_id: ToolCallId::new_v7(),
                        child_session_id: SessionId::new_v7(),
                    },
                },
            }),
            generation: 0,
        })
        .await;

        assert_eq!(app.selected, Some(root));
        assert_eq!(app.tree_refresh_in_flight, Some((0, 1)));
        let update = wait_for_tree_update(&mut app, root, 1).await;
        app.handle_rpc_update(update);
        let subscribe = wait_for_request(
            &mut requests,
            "deep descendant events.subscribe",
            |request| {
                request["method"] == "events.subscribe"
                    && request["params"]["session_id"] == deep_descendant.to_string()
            },
        )
        .await;
        assert_eq!(
            subscribe["params"]["session_id"],
            deep_descendant.to_string()
        );
        assert!(app.tree_refresh_in_flight.is_none());
    }

    #[tokio::test]
    async fn command_palette_mouse_row_completes_argument_command() {
        let mut app = test_app();
        app.input.set_buffer("/tree".into());
        let tree_index = app
            .palette_entries()
            .iter()
            .position(|spec| spec.name == "tree")
            .expect("tree command");
        app.hit_map.palette = Some(Rect::new(1, 1, 20, 5));
        app.hit_map.palette_rows.push(PaletteRowHit {
            rect: Rect::new(1, 2, 20, 1),
            index: tree_index,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.input.as_str(), "/tree ");
        assert!(app.palette_dismissed);
    }

    #[tokio::test]
    async fn escape_requires_double_tap_and_panels_take_priority() {
        let now = Instant::now();
        let mut app = test_app();
        assert!(!app.register_escape(now));
        assert!(!app.register_escape(now + Duration::from_millis(501)));
        assert!(app.register_escape(now + Duration::from_millis(700)));

        let session_id = SessionId::new_v7();
        let run_id = cookie_agent_protocol::RunId::new_v7();
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                active_run: Some(run_id),
                ..SessionState::default()
            },
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(sent.lock().expect("sent requests lock").is_empty());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        tokio::task::yield_now().await;
        assert_eq!(
            sent.lock().expect("sent requests lock")[0]["method"],
            "run.cancel"
        );

        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                active_run: Some(run_id),
                ..SessionState::default()
            },
        );
        app.input.set_buffer("/help".into());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(app.palette_dismissed);
        assert!(sent.lock().expect("sent requests lock").is_empty());
    }

    #[test]
    fn transcript_layout_maps_wrapped_block_lines_and_collapse_headers() {
        let (state, tool_id) = state_with_blocks();
        let expanded_blocks = HashSet::from([BlockId::Thinking(7), BlockId::Tool(tool_id)]);
        let expanded = transcript_layout(&state, Some(&expanded_blocks), 5);
        assert_eq!(expanded.regions.len(), 2);
        assert_eq!(expanded.regions[0].id, BlockId::Thinking(7));
        assert_eq!(expanded.regions[0].start_line, 1);
        assert!(expanded.regions[0].end_line > 2);
        assert_eq!(expanded.regions[0].end_line, expanded.regions[1].start_line);
        assert_eq!(expanded.regions[1].id, BlockId::Tool(tool_id));
        assert_eq!(expanded.regions[1].end_line, expanded.lines.len());

        let expanded_lines = transcript_layout(&state, Some(&expanded_blocks), 80)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(expanded_lines.iter().any(|line| line == "│ ┆ abcdef"));
        assert!(
            expanded_lines
                .iter()
                .any(|line| line == "┃ arguments: {\"command\":\"status\"}")
        );

        let layout = transcript_layout(&state, None, 80);
        let lines = layout
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(
            lines
                .iter()
                .any(|line| line == "│ ▸ thinking (1 lines hidden)")
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "┃ ▸ bash — COMPLETED ✓ (details hidden)")
        );
        assert!(!lines.iter().any(|line| line.contains("command")));
        assert!(!lines.iter().any(|line| line.contains("done")));
        assert!(
            layout
                .regions
                .iter()
                .all(|region| region.end_line > region.start_line)
        );
    }

    #[test]
    fn assistant_children_render_in_delta_order_under_one_header_without_duplication() {
        let session_id = SessionId::new_v7();
        let mut store = StateStore::default();
        for (seq, event) in [
            Event::ReasoningDelta {
                text: "think-one".into(),
            },
            Event::TextDelta {
                text: "answer-one".into(),
            },
            Event::ReasoningDelta {
                text: "think-two".into(),
            },
            Event::TextDelta {
                text: "answer-two".into(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            assert!(store.apply_event(projection_event(session_id, seq as u64 + 1, event,)));
        }
        let state = &store.sessions[&session_id];
        let layout = transcript_layout(
            state,
            Some(&HashSet::from([BlockId::Thinking(1), BlockId::Thinking(3)])),
            80,
        );
        let rendered = layout
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.contains("ASSISTANT"))
                .count(),
            1
        );
        assert!(rendered.iter().all(|line| !line.contains("REASONING")));
        let joined = rendered.join("\n");
        let positions = ["think-one", "answer-one", "think-two", "answer-two"]
            .map(|needle| joined.find(needle).expect("ordered assistant child"));
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        for needle in ["think-one", "answer-one", "think-two", "answer-two"] {
            assert_eq!(joined.matches(needle).count(), 1, "{needle}");
        }
    }

    #[test]
    fn thinking_stream_indicator_is_render_only_and_follows_the_latest_open_child() {
        let session_id = SessionId::new_v7();
        let mut store = StateStore::default();
        for (seq, event) in [
            Event::ReasoningDelta {
                text: "first".into(),
            },
            Event::TextDelta {
                text: "answer".into(),
            },
            Event::ReasoningDelta {
                text: "latest".into(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            assert!(store.apply_event(projection_event(session_id, seq as u64 + 1, event,)));
        }
        let collapsed = transcript_layout(&store.sessions[&session_id], None, 80)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let thinking_rows = collapsed
            .iter()
            .filter(|line| line.contains("thinking"))
            .collect::<Vec<_>>();
        assert_eq!(thinking_rows.len(), 2);
        assert!(!thinking_rows[0].contains('…'));
        assert!(thinking_rows[1].contains('…'));

        assert!(store.apply_event(projection_event(session_id, 4, Event::AttemptAbandoned,)));
        let closed = transcript_layout(&store.sessions[&session_id], None, 80)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(
            closed
                .iter()
                .filter(|line| line.contains("thinking"))
                .all(|line| !line.contains('…'))
        );
    }

    #[test]
    fn thinking_expansion_is_plain_wrapped_text_with_cjk_aware_hidden_line_count() {
        let theme = Theme::default();
        let body = thinking_body_lines("界界界界\n**raw**", 8, &theme);
        assert_eq!(body.len(), 4);
        let rendered = body
            .iter()
            .map(ToString::to_string)
            .collect::<String>()
            .replace("│ ┆ ", "")
            .replace("┆ ", "");
        assert!(rendered.contains("**raw**"));

        let state = SessionState {
            transcript: vec![TranscriptItem::assistant_parts(vec![
                AssistantPart::Thinking {
                    id: 7,
                    version: 0,
                    text: "界".repeat(20),
                },
            ])],
            ..SessionState::default()
        };
        let collapsed = transcript_layout(&state, None, 40);
        let region = collapsed.regions[0];
        assert_eq!(
            collapsed.lines[region.start_line].to_string(),
            "│ ▸ thinking (2 lines hidden)"
        );
    }

    #[test]
    fn child_layout_cache_recomputes_only_the_changed_assistant_segment() {
        let session_id = SessionId::new_v7();
        let mut state = SessionState {
            transcript: vec![TranscriptItem::assistant_parts(vec![
                AssistantPart::Thinking {
                    id: 1,
                    version: 0,
                    text: "stable thinking".into(),
                },
                AssistantPart::Text {
                    id: 2,
                    version: 0,
                    markdown: crate::markdown::MarkdownDocument::new("stable text".into()),
                },
                AssistantPart::Thinking {
                    id: 3,
                    version: 0,
                    text: "open thinking".into(),
                },
            ])],
            ..SessionState::default()
        };
        let mut cache = LayoutCache::default();
        let theme = Theme::default();
        let highlighter = PlainHighlighter;
        assert!(!ensure_cached_transcript_layout(
            &mut cache,
            session_id,
            &state,
            None,
            None,
            80,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        ));
        assert_eq!(cache.assistant_part_layout_passes, 3);

        let TranscriptItem::Assistant { version, parts, .. } = &mut state.transcript[0] else {
            panic!("assistant item");
        };
        let AssistantPart::Thinking {
            version: part_version,
            text,
            ..
        } = &mut parts[2]
        else {
            panic!("thinking child");
        };
        text.push_str(" delta");
        *part_version = part_version.wrapping_add(1);
        *version = version.wrapping_add(1);
        assert!(!ensure_cached_transcript_layout(
            &mut cache,
            session_id,
            &state,
            None,
            None,
            80,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        ));
        assert_eq!(cache.assistant_part_layout_passes, 4);

        let expanded = HashSet::from([BlockId::Thinking(1)]);
        assert!(!ensure_cached_transcript_layout(
            &mut cache,
            session_id,
            &state,
            Some(&expanded),
            None,
            80,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        ));
        assert_eq!(cache.assistant_part_layout_passes, 5);
    }

    #[test]
    fn role_block_snapshot_is_textually_distinct_without_relying_on_color() {
        let tool_id = cookie_agent_protocol::ToolCallId::new_v7();
        let output = |stream, text: &str| {
            let mut output = OrderedOutput::default();
            output.replace_snapshot(
                0,
                text.len() as u64,
                vec![OutputDelta {
                    call_id: tool_id,
                    stream,
                    byte_offset: 0,
                    data: STANDARD.encode(text),
                }],
            );
            output
        };
        let mut state = SessionState {
            transcript: vec![
                TranscriptItem::user("user"),
                TranscriptItem::assistant_parts(vec![
                    AssistantPart::Text {
                        id: 1,
                        version: 0,
                        markdown: crate::markdown::MarkdownDocument::new("assistant".into()),
                    },
                    AssistantPart::Thinking {
                        id: 2,
                        version: 0,
                        text: "thought".into(),
                    },
                ]),
                TranscriptItem::tool(4, tool_id),
                TranscriptItem::internal("status"),
            ],
            ..SessionState::default()
        };
        state.tools.insert(
            tool_id,
            ToolCallState {
                id: tool_id,
                tool: "bash".into(),
                arguments: "{\"command\":\"status\"}".into(),
                status: ToolStatus::Completed,
                detail: "detail".into(),
            },
        );
        state
            .output
            .insert((tool_id, false), output(OutputStream::Stdout, "stdout"));
        state
            .output
            .insert((tool_id, true), output(OutputStream::Stderr, "stderr"));

        let layout = transcript_layout(
            &state,
            Some(&HashSet::from([
                BlockId::Thinking(2),
                BlockId::Tool(tool_id),
            ])),
            120,
        );
        let rendered = layout
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec![
                "┌─ USER ",
                "│ user",
                "╭─ ASSISTANT ",
                "│ assistant",
                "│ ▾ thinking",
                "│ ┆ thought",
                "┏✓ TOOL SUCCESS ",
                "┃ ▾ bash — COMPLETED ✓",
                "┃ arguments: {\"command\":\"status\"}",
                "┃ detail",
                "┃ STDOUT:",
                "┃ stdout",
                "┃ STDERR:",
                "┃ stderr",
                "-- EVENT [I] ",
                "· status",
            ]
        );
        assert!(
            layout.lines[0].spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn mono_no_color_and_tiny_assistant_transcripts_keep_one_assistant_tag_and_no_reasoning_tag() {
        let state = SessionState {
            transcript: vec![TranscriptItem::assistant_parts(vec![
                AssistantPart::Thinking {
                    id: 1,
                    version: 0,
                    text: "plain thought".into(),
                },
                AssistantPart::Text {
                    id: 2,
                    version: 0,
                    markdown: crate::markdown::MarkdownDocument::new("answer".into()),
                },
            ])],
            ..SessionState::default()
        };
        let expanded = HashSet::from([BlockId::Thinking(1)]);
        for theme in [
            Theme::new(
                crate::theme::ThemeKind::Mono,
                crate::theme::ColorLevel::None,
            ),
            Theme::from_environment("default", true, "xterm", "truecolor"),
        ] {
            for width in 1..8 {
                let layout = transcript_layout_with(
                    &state,
                    Some(&expanded),
                    width,
                    &theme,
                    &PlainHighlighter,
                );
                let compact = layout
                    .lines
                    .iter()
                    .map(ToString::to_string)
                    .collect::<String>();
                assert_eq!(compact.matches("[A]").count(), 1, "width {width}");
                assert!(!compact.contains("[R]"), "width {width}");
                assert!(!compact.contains("REASONING"), "width {width}");
                assert!(compact.contains('▾'), "width {width}");
                let content = compact
                    .replace(['│', '┆'], "")
                    .chars()
                    .filter(|character| !character.is_whitespace())
                    .collect::<String>();
                assert_eq!(content.matches("plain").count(), 1, "width {width}");
                assert_eq!(content.matches("thought").count(), 1, "width {width}");
                assert_eq!(content.matches("answer").count(), 1, "width {width}");
            }
        }
    }

    #[test]
    fn tool_running_success_and_failure_have_distinct_text_headers() {
        let mut state = SessionState::default();
        for (id, status) in [
            (1, ToolStatus::Running),
            (2, ToolStatus::Completed),
            (3, ToolStatus::Failed),
        ] {
            let call_id = ToolCallId::new_v7();
            state.transcript.push(TranscriptItem::tool(id, call_id));
            state.tools.insert(
                call_id,
                ToolCallState {
                    id: call_id,
                    tool: "bash".into(),
                    arguments: "{}".into(),
                    status,
                    detail: String::new(),
                },
            );
        }
        let rendered = transcript_layout(&state, None, 80)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| line == "┏… TOOL RUNNING "));
        assert!(rendered.iter().any(|line| line == "┏✓ TOOL SUCCESS "));
        assert!(rendered.iter().any(|line| line == "┏! TOOL FAILURE "));
        assert!(rendered.iter().any(|line| line.contains("RUNNING …")));
        assert!(rendered.iter().any(|line| line.contains("COMPLETED ✓")));
        assert!(rendered.iter().any(|line| line.contains("FAILED !")));
    }

    #[test]
    fn transcript_wraps_at_words_without_losing_label_styles() {
        let lines = wrapped_labelled_text(
            None,
            "You: ",
            Style::default().fg(Color::Cyan),
            "one two three",
            10,
        );
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["You: one", "two three"]
        );
        assert_eq!(lines[0].spans[0].style, Style::default().fg(Color::Cyan));
        assert_eq!(lines[0].spans[1].style, Style::default());
    }

    #[test]
    fn long_words_wrap_on_grapheme_boundaries() {
        let family = "👨‍👩‍👧‍👦";
        let lines = wrapped_text(&format!("{family}{family}"), 2, Style::default());
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![family, family]
        );
    }

    #[test]
    fn markdown_unicode_wrap_and_tiny_role_degradation_are_safe() {
        let family = "👨‍👩‍👧‍👦";
        let state = SessionState {
            transcript: vec![TranscriptItem::assistant(format!("**界**{family}{family}"))],
            ..SessionState::default()
        };
        let narrow = transcript_layout(&state, None, 8);
        let rendered = narrow
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<String>();
        assert!(rendered.contains('界'));
        assert_eq!(rendered.matches(family).count(), 2);
        assert!(
            narrow
                .lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.to_string().as_str()) <= 8)
        );

        for width in 1..8 {
            let tiny = transcript_layout(&state, None, width);
            assert!(!tiny.lines.is_empty());
            let compact = tiny
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<String>()
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>();
            assert!(compact.contains("[A]"));
        }
    }

    #[test]
    fn conversation_scroll_clamps_for_content_and_viewport_changes() {
        let mut scroll = ConversationScroll::default();
        scroll.clamp(10, 4);
        assert_eq!((scroll.offset, scroll.following), (6, true));
        scroll.up(2);
        assert_eq!((scroll.offset, scroll.following), (4, false));
        scroll.clamp(3, 4);
        assert_eq!(scroll.offset, 0);
        scroll.down(99);
        scroll.clamp(10, 5);
        assert_eq!(scroll.offset, 5);
        scroll.top();
        assert_eq!((scroll.offset, scroll.following), (0, false));
        scroll.bottom();
        scroll.clamp(12, 5);
        assert_eq!((scroll.offset, scroll.following), (7, true));
    }

    #[tokio::test]
    async fn changing_sessions_resets_conversation_scroll_to_live_following() {
        let mut app = test_app();
        let first = SessionId::new_v7();
        let second = SessionId::new_v7();
        app.selected = Some(first);
        app.conversation_scroll.offset = 17;
        app.conversation_scroll.following = false;
        app.set_selected_session(second);
        assert_eq!(app.selected, Some(second));
        assert_eq!(app.conversation_scroll.offset, 0);
        assert!(app.conversation_scroll.following);

        app.conversation_scroll.offset = 4;
        app.conversation_scroll.following = false;
        app.set_selected_session(second);
        assert_eq!(app.conversation_scroll.offset, 4);
        assert!(!app.conversation_scroll.following);
    }

    #[tokio::test]
    async fn default_collapsed_state_and_mouse_expansion_persist_across_swap() {
        let session_id = SessionId::new_v7();
        let (state, _) = state_with_blocks();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store.sessions.insert(session_id, state.clone());

        let collapsed = transcript_layout(&state, None, 80);
        assert!(
            collapsed
                .lines
                .iter()
                .any(|line| line.to_string() == "│ ▸ thinking (1 lines hidden)")
        );

        app.toggle_block(BlockId::Thinking(7));
        assert!(app.expanded_blocks[&session_id].contains(&BlockId::Thinking(7)));
        let other_session = SessionId::new_v7();
        app.set_selected_session(other_session);
        assert!(app.selected_block.is_none());
        app.set_selected_session(session_id);
        assert!(app.expanded_blocks[&session_id].contains(&BlockId::Thinking(7)));
        app.store.sessions.insert(session_id, state);
        let layout = transcript_layout(
            &app.store.sessions[&session_id],
            app.expanded_blocks.get(&session_id),
            80,
        );
        assert!(
            layout
                .lines
                .iter()
                .any(|line| line.to_string() == "│ ┆ abcdef")
        );

        app.toggle_block(BlockId::Thinking(7));
        assert!(!app.expanded_blocks[&session_id].contains(&BlockId::Thinking(7)));
        app.run_command(SlashCommand::Scroll(ScrollCommand::Up(3)))
            .await;
        assert!(!app.conversation_scroll.following);
        app.run_command(SlashCommand::Scroll(ScrollCommand::Bottom))
            .await;
        assert!(app.conversation_scroll.following);
    }

    #[tokio::test]
    async fn mouse_first_block_interaction_toggles_thinking_and_tools_and_survives_tiny_resize() {
        let session_id = SessionId::new_v7();
        let (state, tool_id) = state_with_blocks();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store.sessions.insert(session_id, state);
        app.input.set_buffer("draft prompt".into());

        // Ctrl-N/P/B are retired: control keys do not navigate blocks.
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.selected_block, None);
        assert_eq!(app.input.as_str(), "draft prompt");

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("transcript render");
        let thinking = app
            .hit_map
            .blocks
            .iter()
            .find(|hit| hit.id == BlockId::Thinking(7))
            .copied()
            .expect("thinking hit");
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: thinking.rect.x + 1,
            row: thinking.rect.y,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.selected_block, Some(BlockId::Thinking(7)));
        assert!(app.expanded_blocks[&session_id].contains(&BlockId::Thinking(7)));
        assert_eq!(app.input.as_str(), "draft prompt");

        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("expanded transcript render");
        let tool = app
            .hit_map
            .blocks
            .iter()
            .find(|hit| hit.id == BlockId::Tool(tool_id))
            .copied()
            .expect("tool hit");
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: tool.rect.x + 1,
            row: tool.rect.y,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.selected_block, Some(BlockId::Tool(tool_id)));
        assert!(app.expanded_blocks[&session_id].contains(&BlockId::Tool(tool_id)));

        app.run_command(SlashCommand::Scroll(ScrollCommand::Up(2)))
            .await;
        for (width, height) in [(100, 20), (8, 6), (3, 4)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| app.draw_for_test(frame))
                .expect("tiny transcript render");
            assert_eq!(app.selected_block, Some(BlockId::Tool(tool_id)));
            if width == 100 {
                let rendered = (0..terminal.backend().buffer().area.height)
                    .map(|y| {
                        (0..terminal.backend().buffer().area.width)
                            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(rendered.contains("click a block to expand"));
                assert!(!rendered.contains("Ctrl-N"));
                assert!(!rendered.contains("Ctrl-B"));
            }
        }

        app.store
            .sessions
            .get_mut(&session_id)
            .expect("session")
            .transcript
            .iter_mut()
            .for_each(|item| {
                if let TranscriptItem::Assistant { parts, .. } = item {
                    parts.retain(|part| !matches!(part, AssistantPart::Thinking { .. }));
                }
            });
        // /block remains the accessible command path.
        app.run_command(SlashCommand::Block(BlockCommand::Previous))
            .await;
        assert_eq!(app.selected_block, Some(BlockId::Tool(tool_id)));
        app.run_command(SlashCommand::Block(BlockCommand::Toggle))
            .await;
        assert!(!app.expanded_blocks[&session_id].contains(&BlockId::Tool(tool_id)));
        app.run_command(SlashCommand::Block(BlockCommand::Clear))
            .await;
        assert!(app.selected_block.is_none());
    }

    #[tokio::test]
    async fn block_navigation_flattens_multiple_thinking_children_and_tool_siblings_in_render_order()
     {
        let session_id = SessionId::new_v7();
        let tool_id = ToolCallId::new_v7();
        let mut state = SessionState {
            transcript: vec![
                TranscriptItem::assistant_parts(vec![
                    AssistantPart::Thinking {
                        id: 1,
                        version: 0,
                        text: "first".into(),
                    },
                    AssistantPart::Text {
                        id: 2,
                        version: 0,
                        markdown: crate::markdown::MarkdownDocument::new("middle".into()),
                    },
                    AssistantPart::Thinking {
                        id: 3,
                        version: 0,
                        text: "second".into(),
                    },
                ]),
                TranscriptItem::tool(4, tool_id),
            ],
            ..SessionState::default()
        };
        state.tools.insert(
            tool_id,
            ToolCallState {
                id: tool_id,
                tool: "bash".into(),
                arguments: "{}".into(),
                status: ToolStatus::Completed,
                detail: "done".into(),
            },
        );
        let layout = transcript_layout(&state, None, 80);
        assert_eq!(
            layout
                .regions
                .iter()
                .map(|region| region.id)
                .collect::<Vec<_>>(),
            vec![
                BlockId::Thinking(1),
                BlockId::Thinking(3),
                BlockId::Tool(tool_id),
            ]
        );

        let mut app = test_app();
        app.selected = Some(session_id);
        app.store.sessions.insert(session_id, state);
        for expected in [
            BlockId::Thinking(1),
            BlockId::Thinking(3),
            BlockId::Tool(tool_id),
        ] {
            app.run_block_command(BlockCommand::Next);
            assert_eq!(app.selected_block, Some(expected));
            app.run_block_command(BlockCommand::Toggle);
            assert!(app.expanded_blocks[&session_id].contains(&expected));
        }
        app.run_block_command(BlockCommand::Previous);
        assert_eq!(app.selected_block, Some(BlockId::Thinking(3)));
    }

    #[test]
    fn slash_commands_are_routed_to_their_input_modes() {
        assert!(command_allowed_in_mode(
            SlashCommand::Stdin { next: false },
            InputMode::Message
        ));
        assert!(!command_allowed_in_mode(
            SlashCommand::Stdin { next: false },
            InputMode::ToolStdin
        ));
        assert!(command_allowed_in_mode(
            SlashCommand::Stdin { next: true },
            InputMode::Message
        ));
        assert!(!command_allowed_in_mode(
            SlashCommand::Stdin { next: true },
            InputMode::ToolStdin
        ));
        for command in [SlashCommand::Eof, SlashCommand::Message] {
            assert!(command_allowed_in_mode(command, InputMode::ToolStdin));
            assert!(!command_allowed_in_mode(command, InputMode::Message));
        }
        assert!(command_allowed_in_mode(
            SlashCommand::Help,
            InputMode::Message
        ));
        assert!(!command_allowed_in_mode(
            SlashCommand::Help,
            InputMode::ToolStdin
        ));
    }

    #[tokio::test]
    async fn unknown_and_wrong_mode_commands_surface_errors() {
        let mut app = test_app();
        for character in "/missing".chars() {
            app.input.insert(character);
        }
        app.submit_input().await;
        assert_eq!(app.status, "unknown command: /missing");
        assert!(app.input.as_str().is_empty());

        for character in "/eof".chars() {
            app.input.insert(character);
        }
        app.submit_input().await;
        assert_eq!(app.status, "/eof is only available in tool stdin mode");
    }

    #[tokio::test]
    async fn help_is_transient_ui_state_not_protocol_projection() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                transcript: vec![TranscriptItem::assistant("persisted")],
                ..SessionState::default()
            },
        );
        for _ in 0..8 {
            app.show_help();
        }
        assert_eq!(app.store.sessions[&session_id].transcript.len(), 1);
        assert_eq!(app.transient_notices.len(), 4);
        app.store
            .sessions
            .insert(session_id, SessionState::default());
        assert_eq!(app.transient_notices.len(), 4);
    }

    #[tokio::test]
    async fn cancel_watch_and_approval_commands_route_to_rpc_methods() {
        let session_id = SessionId::new_v7();
        let run_id = cookie_agent_protocol::RunId::new_v7();
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.tree_root = Some(session_id);
        app.tree = Some(SessionTree {
            session: session_meta(session_id),
            children: Vec::new(),
        });
        app.store.sessions.insert(
            session_id,
            SessionState {
                active_run: Some(run_id),
                approvals: vec![approval(session_id)],
                ..SessionState::default()
            },
        );

        app.run_command(SlashCommand::Cancel).await;
        app.run_command(SlashCommand::Watch).await;
        // The first decision dismisses the modal optimistically; later
        // decisions find no visible approval and are ignored as duplicates.
        for decision in [
            ApprovalUserDecision::ApproveOnce,
            ApprovalUserDecision::ApproveTree,
            ApprovalUserDecision::Reject,
        ] {
            app.run_command(SlashCommand::Approve(decision)).await;
        }
        for _ in 0..20 {
            tokio::task::yield_now().await;
            if sent.lock().expect("sent requests lock").len() >= 4 {
                break;
            }
        }
        let requests = sent.lock().expect("sent requests lock").clone();
        let methods = requests
            .iter()
            .map(|request| request["method"].as_str().expect("method"))
            .collect::<Vec<_>>();
        assert!(methods.contains(&"run.cancel"));
        assert!(methods.contains(&"events.subscribe"));
        assert!(methods.contains(&"session.tree"));
        let decisions = requests
            .iter()
            .filter(|request| request["method"] == "approval.respond")
            .map(|request| request["params"]["decision"].as_str().expect("decision"))
            .collect::<Vec<_>>();
        assert_eq!(decisions, vec!["approve_once"]);

        app.input_mode = InputMode::ToolStdin;
        for (command, expected_status) in [
            (
                SlashCommand::Cancel,
                "/cancel is only available in message mode",
            ),
            (
                SlashCommand::Watch,
                "/watch is only available in message mode",
            ),
            (
                SlashCommand::Approve(ApprovalUserDecision::ApproveOnce),
                "/approve once is only available in message mode",
            ),
        ] {
            app.run_command(command).await;
            assert_eq!(app.status, expected_status);
        }
        assert_eq!(sent.lock().expect("sent requests lock").len(), 4);
    }

    #[test]
    fn bash_prepared_approval_identity_snapshot_is_complete() {
        let approval = approval(SessionId::new_v7());
        insta::assert_snapshot!(
            "bash_prepared_approval_identity",
            stable_approval_snapshot(&approval, approval_content(&approval))
        );
    }

    #[test]
    fn filesystem_prepared_approval_identity_snapshot_is_complete() {
        let approval = filesystem_approval(SessionId::new_v7());
        insta::assert_snapshot!(
            "filesystem_prepared_approval_identity",
            stable_approval_snapshot(&approval, approval_content(&approval))
        );
    }

    #[tokio::test]
    async fn approval_modal_tiny_terminal_snapshot_is_bounded() {
        let approval = filesystem_approval(SessionId::new_v7());
        insta::assert_snapshot!(
            "approval_modal_tiny_terminal",
            approval_terminal_snapshot(&approval, 24, 8, false, false)
        );
    }

    #[tokio::test]
    async fn approval_modal_no_color_snapshot_remains_textually_complete_and_scrollable() {
        let approval = filesystem_approval(SessionId::new_v7());
        insta::assert_snapshot!(
            "approval_modal_no_color",
            approval_terminal_snapshot(&approval, 80, 24, true, true)
        );
    }

    #[tokio::test]
    async fn approval_identity_is_keyboard_and_mouse_scrollable_without_editing_the_draft() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.input.set_buffer("preserved draft".into());
        app.store.sessions.insert(
            session_id,
            SessionState {
                approvals: vec![filesystem_approval(session_id)],
                ..SessionState::default()
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("approval draw");
        assert!(app.approval_max_scroll > 0);

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
            .await;
        assert_eq!(app.approval_scroll, app.approval_max_scroll);
        assert_eq!(app.input.as_str(), "preserved draft");

        let approval_area = app.hit_map.approval.expect("approval hit area");
        app.handle_wheel(approval_area.x, approval_area.y, true);
        assert!(app.approval_scroll < app.approval_max_scroll);
        assert_eq!(app.input.as_str(), "preserved draft");
    }

    #[tokio::test]
    async fn approval_response_is_bound_to_the_exact_displayed_request() {
        let session_id = SessionId::new_v7();
        let expected = filesystem_approval(session_id);
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                approvals: vec![expected.clone()],
                ..SessionState::default()
            },
        );

        app.answer_approval(ApprovalUserDecision::ApproveOnce).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
            if !sent.lock().expect("sent requests lock").is_empty() {
                break;
            }
        }
        let requests = sent.lock().expect("sent requests lock");
        let request = requests.first().expect("approval response request");
        assert_eq!(request["method"], "approval.respond");
        assert_eq!(
            request["params"]["session_id"],
            serde_json::to_value(expected.session_id).expect("session id JSON")
        );
        assert_eq!(
            request["params"]["approval_id"],
            serde_json::to_value(expected.approval_id).expect("approval id JSON")
        );
        assert_eq!(
            request["params"]["request_revision"],
            expected.request_revision
        );
        assert_eq!(
            request["params"]["operation_fingerprint"],
            serde_json::to_value(&expected.operation_fingerprint).expect("fingerprint JSON")
        );
        assert_eq!(request["params"]["decision"], "approve_once");
        assert!(
            request["params"]["client_response_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(request["params"]["feedback"].is_null());
    }

    #[tokio::test]
    async fn unfocused_printable_keys_focus_and_insert_including_removed_hotkeys() {
        let mut app = test_app();
        app.input_focused = false;
        for character in ['n', 's', 'i', 'j', 'k', 'e', 'w', '1', '2', '3', 'q'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .await;
        }
        assert!(app.input_focused);
        assert_eq!(app.input.as_str(), "nsijkew123q");
        assert!(!app.should_quit);
    }

    #[tokio::test]
    async fn modal_picker_typing_filters_and_keeps_local_navigation() {
        let mut app = test_app();
        app.sessions = vec![
            session_meta(SessionId::new_v7()),
            session_meta(SessionId::new_v7()),
        ];
        app.modal = Modal::Sessions;
        app.picker_state.select(Some(1));
        // Typing filters instead of touching the draft input buffer.
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
            .await;
        assert!(app.input.as_str().is_empty());
        assert_eq!(app.picker_query, "p");
        assert_eq!(app.picker_state.selected(), Some(0));
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .await;
        assert!(app.picker_query.is_empty());
        app.picker_state.select(Some(1));
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        assert_eq!(app.picker_state.selected(), Some(0));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::None);
        assert!(app.picker_query.is_empty());
    }

    #[tokio::test]
    async fn tab_switching_is_local_without_rpc_or_session_mutation() {
        let session_id = SessionId::new_v7();
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.agents = vec![
            AgentDescriptor {
                name: "primary".into(),
                agent_type: AgentType::Primary,
                enabled: true,
                models: Vec::new(),
            },
            AgentDescriptor {
                name: "reviewer".into(),
                agent_type: AgentType::All,
                enabled: true,
                models: Vec::new(),
            },
            AgentDescriptor {
                name: "disabled".into(),
                agent_type: AgentType::Primary,
                enabled: false,
                models: Vec::new(),
            },
        ];
        app.sessions.push(session_meta(session_id));
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, SessionState::default());
        let baseline_sessions = app.sessions.clone();
        let baseline_store = app.store.clone();

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.draft_agent_profile.as_deref(), Some("reviewer"));
        assert_eq!(app.selected, Some(session_id));
        assert_eq!(app.sessions, baseline_sessions);
        assert_eq!(app.store.sessions.len(), baseline_store.sessions.len());
        assert!(sent.lock().expect("sent requests").is_empty());

        app.input.set_buffer("review this".into());
        app.submit_input().await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if sent
                    .lock()
                    .expect("sent requests")
                    .iter()
                    .any(|request| request["method"] == "run.start")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run.start");
        let requests = sent.lock().expect("sent requests");
        let run = requests
            .iter()
            .find(|request| request["method"] == "run.start")
            .expect("run request");
        assert_eq!(run["params"]["profile"], "reviewer");
        assert!(
            requests
                .iter()
                .all(|request| request["method"] != "session.create")
        );
    }

    #[tokio::test]
    async fn empty_agent_setup_is_valid_and_never_calls_session_create() {
        let (client, sent) = empty_setup_client();
        let app = App::new_with_new_session(client).await.expect("TUI config");
        assert!(app.sessions.is_empty());
        assert!(app.selected.is_none());
        assert!(app.agents.is_empty());
        assert!(app.status.contains("No user-selectable agent profiles"));
        assert!(app.status.contains("no session was created"));
        assert!(
            sent.lock()
                .expect("sent requests")
                .iter()
                .all(|request| request["method"] != "session.create")
        );
    }

    #[tokio::test]
    async fn explicitly_disabled_profiles_remain_disabled_after_connect_refresh() {
        let mut app = test_app();
        app.apply_connect_outcome(ConnectOutcome::Connected {
            provider_id: "test".into(),
            receipt_model_revision: "receipt".into(),
            follow_up: Box::new(ConnectFollowUp::NoRunnableProfiles {
                agents: vec![AgentDescriptor {
                    name: "disabled".into(),
                    agent_type: AgentType::Primary,
                    enabled: false,
                    models: Vec::new(),
                }],
                model_revision: "models".into(),
                model_count: 1,
            }),
        });
        assert!(!app.agents[0].enabled);
        assert!(app.draft_agent_profile.is_none());
        assert!(app.sessions.is_empty());
        assert!(
            app.status
                .contains("Explicitly disabled profiles remain disabled")
        );
    }

    #[tokio::test]
    async fn credential_inputs_wipe_on_cancel_and_app_drop() {
        let before = credential_wipe_count();
        let mut app = test_app();
        let mut first = CredentialInput::default();
        first.insert_owned("cancel-secret".into());
        app.connect_fields.push(("API_KEY".into(), first));
        app.modal = Modal::ConnectCredentials;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(app.connect_fields.is_empty());
        assert!(credential_wipe_count() > before);

        let before_drop = credential_wipe_count();
        let mut second = CredentialInput::default();
        second.insert_owned("drop-secret".into());
        app.connect_fields.push(("API_KEY".into(), second));
        drop(app);
        assert!(credential_wipe_count() > before_drop);
    }

    #[tokio::test]
    async fn setup_connect_masks_clears_and_refreshes_without_secret_leakage() {
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(SessionId::new_v7());
        app.catalog_revision = Some("catalog-test".into());
        app.providers = vec![CatalogProvider {
            id: "test-provider".into(),
            name: "Test Provider".into(),
            credential_fields: vec!["API_KEY".into()],
            npm: None,
            api: Some("https://api.example.test".into()),
            documentation_url: Some("https://docs.example.test".into()),
        }];

        app.run_command(SlashCommand::Connect).await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        let sentinel = "sentinel-secret";
        for character in sentinel.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .await;
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("render connect form");
        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|y| {
                (0..terminal.backend().buffer().area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains(sentinel));
        assert!(rendered.contains('•'));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(app.connect_fields.is_empty());
        assert!(app.connect_provider.is_none());
        assert!(!app.status.contains(sentinel));
        let update = tokio::time::timeout(Duration::from_secs(1), app.rpc_updates_rx.recv())
            .await
            .expect("connect refresh timeout")
            .expect("connect refresh");
        app.handle_rpc_update(update);
        assert_eq!(app.agents.len(), 1);
        assert!(app.status.contains("sha256:test"));
        assert!(
            app.store
                .sessions
                .values()
                .flat_map(|state| &state.transcript)
                .all(|item| !format!("{item:?}").contains(sentinel))
        );
        assert!(
            sent.lock()
                .expect("sent requests")
                .iter()
                .any(|request| request["method"] == "provider.connect")
        );
    }

    #[tokio::test]
    async fn connect_can_make_an_unresolved_profile_runnable_and_create_initial_session() {
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.draft_agent_profile = None;
        app.agents = vec![AgentDescriptor {
            name: "primary".into(),
            agent_type: AgentType::Primary,
            enabled: false,
            models: Vec::new(),
        }];
        app.catalog_revision = Some("catalog-test".into());
        app.providers = vec![CatalogProvider {
            id: "test-provider".into(),
            name: "Test Provider".into(),
            credential_fields: vec!["API_KEY".into()],
            npm: None,
            api: Some("https://api.example.test".into()),
            documentation_url: None,
        }];

        app.run_command(SlashCommand::Connect).await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        app.handle_paste("sentinel-secret");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        let update = tokio::time::timeout(Duration::from_secs(1), app.rpc_updates_rx.recv())
            .await
            .expect("connect outcome timeout")
            .expect("connect outcome");
        app.handle_rpc_update(update);

        assert!(app.agents[0].enabled);
        assert_eq!(app.draft_agent_profile.as_deref(), Some("primary"));
        assert_eq!(app.sessions.len(), 1);
        assert!(app.status.contains("created the initial session"));
        let requests = sent.lock().expect("sent requests");
        assert!(
            requests
                .iter()
                .any(|request| request["method"] == "agent.list")
        );
        assert!(
            requests
                .iter()
                .any(|request| request["method"] == "session.create")
        );
    }

    #[tokio::test]
    async fn accepted_run_profile_stays_frozen_while_draft_changes() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.agents = vec![
            AgentDescriptor {
                name: "primary".into(),
                agent_type: AgentType::Primary,
                enabled: true,
                models: Vec::new(),
            },
            AgentDescriptor {
                name: "reviewer".into(),
                agent_type: AgentType::All,
                enabled: true,
                models: Vec::new(),
            },
        ];
        app.sessions.push(session_meta(session_id));
        app.selected = Some(session_id);
        let accepted = session_meta(session_id).profile;
        app.store.apply_event(EventEnvelope {
            schema_version: EventSchemaVersion::current(),
            session_id,
            run_id: Some(cookie_agent_protocol::RunId::new_v7()),
            seq: 1,
            timestamp: Timestamp::now(),
            event: Event::RunStarted {
                client_run_id: "accepted".into(),
                input: "hello".into(),
                profile: accepted,
                current_profile: cookie_agent_protocol::ProfileIdentity {
                    name: "primary".into(),
                    agent_type: AgentType::All,
                },
            },
        });

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.draft_agent_profile.as_deref(), Some("reviewer"));
        assert_eq!(
            app.store.sessions[&session_id]
                .run_profile
                .as_ref()
                .map(|profile| profile.name.as_str()),
            Some("primary")
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("render accepted profile");
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("run: primary"));
        assert!(rendered.contains("next: reviewer"));
    }

    #[test]
    fn block_hit_rects_are_clipped_and_shifted_by_scroll_offset() {
        let hit = block_hit(
            BlockRegion {
                id: BlockId::Thinking(7),
                start_line: 5,
                end_line: 10,
            },
            Rect::new(20, 10, 30, 4),
            7,
        )
        .expect("visible block hit");
        assert_eq!(hit.id, BlockId::Thinking(7));
        assert_eq!(hit.rect, Rect::new(20, 10, 30, 3));
        assert!(
            block_hit(
                BlockRegion {
                    id: BlockId::Thinking(7),
                    start_line: 0,
                    end_line: 2,
                },
                Rect::new(20, 10, 30, 4),
                7,
            )
            .is_none()
        );
    }

    #[test]
    fn input_click_columns_respect_wide_characters() {
        let mut input = InputState::default();
        input.set_buffer("a界b".into());
        input.set_cursor_from_display_column(0);
        assert_eq!(input.cursor_byte(), 0);
        input.set_cursor_from_display_column(1);
        assert_eq!(input.cursor_byte(), 1);
        input.set_cursor_from_display_column(2);
        assert_eq!(input.cursor_byte(), 1);
        input.set_cursor_from_display_column(3);
        assert_eq!(input.cursor_byte(), "a界".len());
        input.set_cursor_from_display_column(99);
        assert_eq!(input.cursor_byte(), input.as_str().len());
    }

    #[tokio::test]
    async fn mouse_clicks_focus_blur_and_toggle_blocks() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.input_focused = false;
        app.hit_map.input = Some(InputHit {
            rect: Rect::new(10, 10, 8, 3),
            text_rect: Rect::new(11, 11, 6, 1),
        });
        app.input.set_buffer("a界".into());
        app.input.set_cursor_from_display_column(0);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 11,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert!(app.input_focused);
        assert_eq!(app.input.cursor_byte(), 1);

        app.hit_map.input = None;
        app.hit_map.blocks.push(BlockHit {
            rect: Rect::new(1, 1, 10, 2),
            id: BlockId::Thinking(7),
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 1,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert!(!app.input_focused);
        assert!(app.expanded_blocks[&session_id].contains(&BlockId::Thinking(7)));
    }

    #[tokio::test]
    async fn modal_mouse_priority_blocks_underlying_content() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.modal = Modal::Sessions;
        app.hit_map.modal_open = true;
        app.input_focused = true;
        app.hit_map.input = Some(InputHit {
            rect: Rect::new(1, 1, 5, 3),
            text_rect: Rect::new(2, 2, 3, 1),
        });
        app.hit_map.blocks.push(BlockHit {
            rect: Rect::new(1, 1, 5, 3),
            id: BlockId::Thinking(7),
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert!(app.input_focused);
        assert!(!app.expanded_blocks.contains_key(&session_id));
    }

    #[tokio::test]
    async fn mouse_dispatches_tree_picker_and_approval_targets() {
        let tree_session = SessionId::new_v7();
        let (client, sent) = recording_client();
        let mut tree_app = test_app();
        tree_app.client = client;
        tree_app.hit_map.tree_rows.push(TreeRowHit {
            rect: Rect::new(1, 1, 10, 1),
            session_id: tree_session,
            expand_rect: None,
        });
        tree_app
            .handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
            .await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        // The clicked row becomes the cursor and watched session; without a
        // loaded tree the watch is an intentional reroot to a fresh snapshot.
        assert_eq!(tree_app.tree_cursor, Some(tree_session));
        assert_eq!(tree_app.selected, Some(tree_session));
        let requests = sent.lock().expect("sent requests lock").clone();
        let tree_methods = requests
            .iter()
            .map(|request| request["method"].as_str().expect("method"))
            .collect::<Vec<_>>();
        assert!(tree_methods.contains(&"events.subscribe"));
        assert!(tree_methods.contains(&"session.tree"));

        let glyph_session = SessionId::new_v7();
        let mut glyph_app = test_app();
        glyph_app.hit_map.tree_rows.push(TreeRowHit {
            rect: Rect::new(1, 1, 10, 1),
            session_id: glyph_session,
            expand_rect: Some(Rect::new(1, 1, 1, 1)),
        });
        glyph_app
            .handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
            .await;
        assert!(glyph_app.collapsed_sessions.contains(&glyph_session));
        assert_eq!(glyph_app.tree_cursor, None);

        let picker_session = SessionId::new_v7();
        let (client, sent) = recording_client();
        let mut picker_app = test_app();
        picker_app.client = client;
        picker_app.modal = Modal::Sessions;
        picker_app.hit_map.modal_open = true;
        picker_app.sessions.push(session_meta(picker_session));
        picker_app.hit_map.picker_rows.push(PickerRowHit {
            rect: Rect::new(1, 1, 10, 1),
            index: 0,
        });
        picker_app
            .handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
            .await;
        assert_eq!(picker_app.modal, Modal::None);
        assert_eq!(picker_app.selected, Some(picker_session));
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let requests = sent.lock().expect("sent requests lock").clone();
        let picker_methods = requests
            .iter()
            .map(|request| request["method"].as_str().expect("method"))
            .collect::<Vec<_>>();
        assert!(picker_methods.contains(&"events.subscribe"));
        assert!(picker_methods.contains(&"session.tree"));

        let approval_session = SessionId::new_v7();
        let (client, sent) = recording_client();
        let mut approval_app = test_app();
        approval_app.client = client;
        approval_app.selected = Some(approval_session);
        approval_app.store.sessions.insert(
            approval_session,
            SessionState {
                approvals: vec![approval(approval_session)],
                ..SessionState::default()
            },
        );
        approval_app.hit_map.approval_actions.push(ApprovalHit {
            rect: Rect::new(1, 1, 10, 1),
            decision: ApprovalUserDecision::ApproveTree,
        });
        approval_app
            .handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
            .await;
        tokio::task::yield_now().await;
        let requests = sent.lock().expect("sent requests lock");
        assert_eq!(requests[0]["method"], "approval.respond");
        assert_eq!(requests[0]["params"]["decision"], "approve_tree");
        assert_eq!(requests[0]["params"]["request_revision"], 9);
        // The modal was dismissed optimistically; the in-flight submission
        // retains the exact captured identity.
        let pending = approval_app
            .pending_approval
            .as_ref()
            .expect("pending submission");
        assert_eq!(
            requests[0]["params"]["operation_fingerprint"],
            serde_json::to_value(&pending.approval.operation_fingerprint)
                .expect("fingerprint JSON")
        );
        assert!(
            approval_app.current_approval().is_none(),
            "modal removed immediately while durable identity is retained"
        );
        assert_eq!(
            approval_app.store.sessions[&approval_session]
                .approvals
                .len(),
            1
        );
        assert!(
            requests[0]["params"]["client_response_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
    }

    #[tokio::test]
    async fn mouse_wheel_scrolls_conversation_and_unfollows() {
        let mut app = test_app();
        app.conversation_scroll.offset = 6;
        app.hit_map.conversation = Some(Rect::new(5, 5, 20, 8));
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 6,
            row: 6,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.conversation_scroll.offset, 3);
        assert!(!app.conversation_scroll.following);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 6,
            row: 6,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.conversation_scroll.offset, 6);
        assert!(!app.conversation_scroll.following);
    }

    #[tokio::test]
    async fn page_keys_move_the_focused_input_without_changing_transcript_scroll() {
        let mut app = test_app();
        app.input
            .set_buffer("zero\none\ntwo\nthree\nfour\nfive\nsix".into());
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("draw input layout");
        app.conversation_scroll.offset = 9;
        app.conversation_scroll.following = false;
        assert_eq!(app.input.cursor_visual_position(38), (6, 3));
        assert_eq!(app.input.viewport_row(), 4);

        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            .await;
        assert_eq!(app.input.cursor_visual_position(38), (3, 3));
        assert_eq!(app.input.viewport_row(), 3);
        assert_eq!(app.conversation_scroll.offset, 9);
        assert!(!app.conversation_scroll.following);

        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .await;
        assert_eq!(app.input.cursor_visual_position(38), (6, 3));
        assert_eq!(app.input.viewport_row(), 4);
        assert_eq!(app.conversation_scroll.offset, 9);
        assert!(!app.conversation_scroll.following);

        app.input_focused = false;
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            .await;
        assert_eq!(app.input.cursor_visual_position(38), (6, 3));
        assert_eq!(app.input.viewport_row(), 4);
    }

    #[tokio::test]
    async fn input_and_transcript_wheels_are_independent_when_interleaved() {
        let mut app = test_app();
        app.input
            .set_buffer("zero\none\ntwo\nthree\nfour\nfive\nsix".into());
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("draw hit map");
        let input = app.hit_map.input.expect("input hit");
        let conversation = app.hit_map.conversation.expect("conversation hit");
        app.conversation_scroll.offset = 8;
        app.conversation_scroll.following = false;

        app.handle_wheel(input.rect.x, input.rect.y, true);
        assert_eq!(app.input.cursor_visual_position(38), (3, 3));
        assert_eq!(app.input.viewport_row(), 3);
        assert_eq!(app.conversation_scroll.offset, 8);
        assert!(!app.conversation_scroll.following);

        let input_cursor = app.input.cursor_byte();
        let input_viewport = app.input.viewport_row();
        app.handle_wheel(conversation.x, conversation.y, true);
        assert_eq!(app.conversation_scroll.offset, 5);
        assert!(!app.conversation_scroll.following);
        assert_eq!(app.input.cursor_byte(), input_cursor);
        assert_eq!(app.input.viewport_row(), input_viewport);

        app.handle_wheel(input.text_rect.x, input.text_rect.y, false);
        assert_eq!(app.input.cursor_visual_position(38), (6, 3));
        assert_eq!(app.input.viewport_row(), 4);
        assert_eq!(app.conversation_scroll.offset, 5);
        assert!(!app.conversation_scroll.following);
    }

    #[tokio::test]
    async fn multiline_paste_reanchors_input_without_disturbing_transcript_scroll() {
        let mut app = test_app();
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("draw input layout");
        app.conversation_scroll.offset = 7;
        app.conversation_scroll.following = false;

        app.handle_paste("zero\r\none\rtwo\nthree\nfour\nfive");
        assert_eq!(app.input.as_str(), "zero\none\ntwo\nthree\nfour\nfive");
        assert_eq!(app.input.cursor_visual_position(38), (5, 4));
        assert_eq!(app.input.viewport_row(), 3);
        assert_eq!(app.conversation_scroll.offset, 7);
        assert!(!app.conversation_scroll.following);
    }

    #[tokio::test]
    async fn approval_page_scroll_keeps_dispatch_precedence_over_input_navigation() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                approvals: vec![approval(session_id)],
                ..SessionState::default()
            },
        );
        app.input
            .set_buffer("zero\none\ntwo\nthree\nfour\nfive\nsix".into());
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("draw approval and input");
        app.approval_scroll = 0;
        app.approval_max_scroll = 40;
        let cursor = app.input.cursor_byte();
        let viewport = app.input.viewport_row();

        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .await;
        assert_eq!(app.approval_scroll, 10);
        assert_eq!(app.input.cursor_byte(), cursor);
        assert_eq!(app.input.viewport_row(), viewport);

        let input = app.hit_map.input.expect("input hit");
        app.handle_wheel(input.rect.x, input.rect.y, true);
        assert_eq!(app.input.cursor_byte(), cursor);
        assert_eq!(app.input.viewport_row(), viewport);
    }

    #[test]
    fn terminal_restore_disables_mouse_capture_during_cleanup() {
        let restore = TerminalRestore {
            raw_mode: true,
            alternate_screen: true,
            mouse_capture: true,
            bracketed_paste: true,
            keyboard_enhancement: true,
        };
        assert_eq!(
            restore.cleanup_steps(),
            vec![
                TerminalCleanup::DisableMouseCapture,
                TerminalCleanup::DisableBracketedPaste,
                TerminalCleanup::PopKeyboardEnhancement,
                TerminalCleanup::LeaveAlternateScreen,
                TerminalCleanup::ShowCursor,
            ]
        );
        std::mem::forget(restore);
    }

    #[tokio::test]
    async fn completed_stdin_target_advances_to_another_running_tool() {
        let session_id = SessionId::new_v7();
        let run_id = cookie_agent_protocol::RunId::new_v7();
        let completed = cookie_agent_protocol::ToolCallId::new_v7();
        let running = cookie_agent_protocol::ToolCallId::new_v7();
        let mut state = SessionState {
            active_run: Some(run_id),
            ..SessionState::default()
        };
        state.tools = HashMap::from([
            (
                completed,
                ToolCallState {
                    id: completed,
                    tool: "bash".into(),
                    arguments: "{}".into(),
                    status: ToolStatus::Completed,
                    detail: String::new(),
                },
            ),
            (
                running,
                ToolCallState {
                    id: running,
                    tool: "bash".into(),
                    arguments: "{}".into(),
                    status: ToolStatus::Running,
                    detail: String::new(),
                },
            ),
        ]);
        let mut store = StateStore::default();
        store.sessions.insert(session_id, state);
        let mut app = test_app();
        app.store = store;
        app.selected = Some(session_id);
        app.input_mode = InputMode::ToolStdin;
        app.stdin_target = Some(completed);
        assert_eq!(app.selected_running_tool(), Some((run_id, running)));
        assert_eq!(app.stdin_target, Some(running));
    }

    #[test]
    fn warning_items_render_with_warning_headers_not_error_headers() {
        let state = SessionState {
            transcript: vec![
                TranscriptItem::Event {
                    id: 1,
                    version: 0,
                    level: crate::state::EventLevel::Warning,
                    text: "model warning from primary (provider/model, adapter): slow".into(),
                },
                TranscriptItem::Event {
                    id: 2,
                    version: 0,
                    level: crate::state::EventLevel::Error,
                    text: "run failed".into(),
                },
            ],
            ..SessionState::default()
        };
        let rendered = transcript_layout(&state, None, 80)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let warning_header = rendered
            .iter()
            .position(|line| line.contains("slow"))
            .expect("warning body");
        assert!(
            rendered[..warning_header]
                .iter()
                .any(|line| line.contains("WARNING"))
        );
        assert!(
            !rendered[..warning_header]
                .iter()
                .any(|line| line.contains("ERROR"))
        );
        assert!(rendered.iter().any(|line| line.contains("ERROR")));
        assert!(rendered.iter().any(|line| line.contains("provider/model")));
    }

    #[test]
    fn warning_role_uses_warning_style_and_error_role_uses_error_style() {
        let theme = Theme::default();
        let warning = role_block(Role::Warning, vec![Line::from("w")], 20, &theme);
        let error = role_block(Role::Error, vec![Line::from("e")], 20, &theme);
        assert_eq!(warning[0].spans[0].style, theme.warning());
        assert_eq!(error[0].spans[0].style, theme.error());
        assert_ne!(theme.warning(), theme.error());
    }

    #[test]
    fn scrollbar_geometry_maps_track_ends_to_the_exact_offset_range() {
        let track = Rect::new(79, 1, 1, 10);
        let geometry = ScrollbarGeometry::resolve(track, 110).expect("overflowing content");
        assert_eq!(geometry.max_offset, 100);
        assert_eq!(geometry.thumb_top(0), 0);
        assert_eq!(geometry.thumb_top(100), 9);
        assert_eq!(geometry.with_thumb(0).thumb.y, 1);
        assert_eq!(geometry.with_thumb(100).thumb.y, 10);
        assert_eq!(geometry.with_thumb(50).thumb.height, 1);
        // The thumb maps back to the same ends of the valid range.
        assert_eq!(
            geometry.clamp_offset(geometry.offset_for_thumb_anchor(1, 0)),
            0
        );
        assert_eq!(
            geometry.clamp_offset(geometry.offset_for_thumb_anchor(10, 0)),
            100
        );
        assert_eq!(geometry.clamp_offset(geometry.offset_for_track_row(1)), 0);
        assert_eq!(
            geometry.clamp_offset(geometry.offset_for_track_row(10)),
            100
        );
        // Mid-track rounds near the middle of the valid range.
        let middle = geometry.clamp_offset(geometry.offset_for_track_row(6));
        assert!((45..=60).contains(&middle), "middle {middle}");
        // Rows below the visible area never resolve a scrollbar.
        assert!(ScrollbarGeometry::resolve(track, 10).is_none());
        assert!(ScrollbarGeometry::resolve(Rect::new(0, 0, 1, 0), 100).is_none());
    }

    #[test]
    fn thumb_height_is_constant_across_top_middle_and_bottom() {
        let track = Rect::new(99, 1, 1, 20);
        // 100 rendered lines over a 20-row viewport: 4 rows of thumb.
        let geometry = ScrollbarGeometry::resolve(track, 100).expect("overflowing content");
        assert_eq!(geometry.thumb_size(), 4);
        assert_eq!(geometry.max_offset, 80);
        let top = geometry.with_thumb(0).thumb;
        let middle = geometry.with_thumb(40).thumb;
        let bottom = geometry.with_thumb(geometry.max_offset).thumb;
        assert_eq!(top.height, middle.height);
        assert_eq!(middle.height, bottom.height);
        assert_eq!(bottom.height, 4);
        // Flush top and flush bottom at full height.
        assert_eq!(top.y, track.y);
        assert_eq!(bottom.y + bottom.height, track.y + track.height);
        // A clamped out-of-range offset still yields the same thumb rect.
        assert_eq!(geometry.with_thumb(10_000).thumb, bottom);
    }

    #[test]
    fn thumb_drag_round_trips_offsets_at_constant_height() {
        let track = Rect::new(99, 1, 1, 10);
        let geometry = ScrollbarGeometry::resolve(track, 210).expect("overflowing content");
        assert_eq!(geometry.thumb_size(), 1);
        // Dragging the thumb row-by-row reproduces monotone offsets spanning
        // the exact ends, with an identical thumb height at every stop.
        let mut offsets = Vec::new();
        for row in track.y..track.y + track.height {
            let offset = geometry.clamp_offset(geometry.offset_for_thumb_anchor(row, 0));
            offsets.push(offset);
            assert_eq!(geometry.with_thumb(offset).thumb.height, 1);
        }
        assert_eq!(offsets.first(), Some(&0));
        assert_eq!(offsets.last(), Some(&200));
        assert!(offsets.windows(2).all(|pair| pair[0] <= pair[1]));
        // Every resolved thumb maps back through the drag inverse to a nearby
        // offset, and all thumbs share one height.
        for offset in [0, 50, 100, 150, 200] {
            let thumb = geometry.with_thumb(offset).thumb;
            let round_trip = geometry.clamp_offset(geometry.offset_for_thumb_anchor(thumb.y, 0));
            assert!(
                round_trip.abs_diff(offset) <= geometry.max_offset / 9 + 1,
                "offset {offset} round-trips to {round_trip}"
            );
            assert_eq!(thumb.height, 1);
        }
    }

    #[test]
    fn thumb_height_changes_only_when_content_or_viewport_geometry_changes() {
        let track = Rect::new(99, 1, 1, 20);
        let base = ScrollbarGeometry::resolve(track, 100).expect("base");
        assert_eq!(base.thumb_size(), 4);
        // Content mutations that keep the same ceil fraction keep the height.
        let same = ScrollbarGeometry::resolve(track, 101).expect("same fraction");
        assert_eq!(base.thumb_size(), same.thumb_size());
        // More content shrinks the thumb; less grows it.
        let denser = ScrollbarGeometry::resolve(track, 400).expect("denser");
        let sparser = ScrollbarGeometry::resolve(track, 30).expect("sparser");
        assert!(denser.thumb_size() < base.thumb_size());
        assert!(sparser.thumb_size() > base.thumb_size());
        // A taller viewport (track) with unchanged content grows the thumb.
        let taller = ScrollbarGeometry::resolve(Rect::new(99, 1, 1, 40), 100).expect("taller");
        assert!(taller.thumb_size() > base.thumb_size());
        // Height never exceeds the track and is never zero.
        let huge = ScrollbarGeometry::resolve(Rect::new(99, 1, 1, 10), 11).expect("huge");
        assert_eq!(huge.thumb_size(), 10);
        assert!(huge.with_thumb(huge.max_offset).thumb.y == Rect::new(99, 1, 1, 10).y);
    }

    #[test]
    fn conversation_scroll_reengages_following_at_the_exact_bottom() {
        let mut scroll = ConversationScroll::default();
        scroll.clamp(110, 10);
        assert!(scroll.following);
        scroll.up(3);
        assert!(!scroll.following);
        scroll.clamp(110, 10);
        assert_eq!(scroll.offset, 97);
        // A drag/scroll landing exactly on the last valid top offset re-follows.
        scroll.scroll_to(100);
        scroll.clamp(110, 10);
        assert!(scroll.following);
        assert_eq!(scroll.offset, 100);
        // Content shrink clamp also restores following at the exact bottom.
        scroll.scroll_to(90);
        scroll.following = false;
        scroll.clamp(40, 10);
        assert!(scroll.following);
        assert_eq!(scroll.offset, 30);
    }

    fn tall_transcript_state(lines: usize) -> SessionState {
        SessionState {
            transcript: (1..=lines as u64)
                .map(|id| TranscriptItem::Event {
                    id,
                    version: 0,
                    level: crate::state::EventLevel::Error,
                    text: format!("event line {id}"),
                })
                .collect(),
            ..SessionState::default()
        }
    }

    /// Reduce a durable model-warning event into a session projection, exactly
    /// as the live event stream would.
    fn push_model_warning(store: &mut StateStore, session_id: SessionId, text: &str) {
        assert!(store.apply_event(projection_event(
            session_id,
            1,
            Event::ModelTurnCommitted {
                model: cookie_agent_protocol::ModelRef {
                    name: "alias".into(),
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                    adapter_id: "adapter".into(),
                },
                input_through_seq: 0,
                turn: cookie_agent_protocol::PersistedModelTurn {
                    content: Vec::new(),
                    provider_options: Default::default(),
                    finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
                    usage: cookie_agent_protocol::Usage {
                        input_tokens: None,
                        input_tokens_no_cache: None,
                        input_tokens_cache_read: None,
                        input_tokens_cache_write: None,
                        output_tokens: None,
                        output_tokens_text: None,
                        output_tokens_reasoning: None,
                    },
                    response_metadata: Default::default(),
                    provider_metadata: Default::default(),
                    warnings: vec![text.into()],
                    native_replay: None,
                },
            },
        )));
    }

    fn titled_meta(session_id: SessionId, title: &str) -> SessionMeta {
        let mut meta = session_meta(session_id);
        meta.title = Some(cookie_agent_protocol::SessionTitle::new(title).expect("title"));
        meta
    }

    fn rendered_frame(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("render");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn descendant_warnings_aggregate_to_the_root_with_attribution_without_duplication() {
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let grandchild = SessionId::new_v7();
        let mut app = test_app();
        app.tree_root = Some(root);
        app.tree = Some(SessionTree {
            session: titled_meta(root, "Root task"),
            children: vec![SessionTree {
                session: titled_meta(child, "Child analysis"),
                children: vec![SessionTree {
                    session: session_meta(grandchild),
                    children: Vec::new(),
                }],
            }],
        });
        // Identical warning text from two different agents stays two
        // distinctly attributed rows.
        push_model_warning(&mut app.store, child, "rate budget nearly exhausted");
        push_model_warning(&mut app.store, grandchild, "rate budget nearly exhausted");

        // Root view: both descendants aggregate with source attribution;
        // nothing appears twice.
        app.selected = Some(root);
        let rendered = rendered_frame(&mut app, 160, 40);
        assert_eq!(rendered.matches("rate budget nearly exhausted").count(), 2);
        assert!(rendered.contains("Child analysis"));
        assert!(rendered.contains("alias (provider/model, adapter)"));
        assert!(rendered.contains(&super::super::pickers::short_id(child)));
        assert!(rendered.contains(&super::super::pickers::short_id(grandchild)));
        assert!(
            !rendered.contains("ERROR"),
            "warnings never use error headers"
        );

        // Child view: its own warning is local; the grandchild aggregates.
        app.selected = Some(child);
        let rendered = rendered_frame(&mut app, 160, 40);
        assert_eq!(rendered.matches("rate budget nearly exhausted").count(), 2);
        assert_eq!(rendered.matches("Child analysis").count(), 1);

        // Grandchild view: only its own local warning — no aggregated rows.
        app.selected = Some(grandchild);
        let rendered = rendered_frame(&mut app, 160, 40);
        assert_eq!(rendered.matches("rate budget nearly exhausted").count(), 1);
    }

    #[tokio::test]
    async fn descendant_warnings_survive_replay_swap_with_attribution() {
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let mut app = test_app();
        app.tree_root = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: titled_meta(child, "Replay child"),
                children: Vec::new(),
            }],
        });
        app.selected = Some(root);
        push_model_warning(&mut app.store, child, "context near limit");
        assert_eq!(app.descendant_warnings(root).len(), 1);

        // A validated replay rebuild of the child reproduces the same warning
        // projection and therefore the same attribution at the root.
        let rebuilt = StateStore::default();
        let mut rebuilt = rebuilt;
        push_model_warning(&mut rebuilt, child, "context near limit");
        app.store
            .sessions
            .insert(child, rebuilt.sessions[&child].clone());
        let warnings = app.descendant_warnings(root);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Replay child"));
        assert!(warnings[0].contains("context near limit"));
        assert!(warnings[0].contains("provider/model"));

        // Warnings vanish from the aggregate when the descendant leaves the
        // tree after a refresh.
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: Vec::new(),
        });
        assert!(app.descendant_warnings(root).is_empty());
    }

    #[tokio::test]
    async fn descendant_warnings_render_in_mono_and_tiny_terminals() {
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let mut app = test_app();
        app.theme = Theme::new(
            crate::theme::ThemeKind::Mono,
            crate::theme::ColorLevel::None,
        );
        app.tree_root = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: titled_meta(child, "Kid"),
                children: Vec::new(),
            }],
        });
        app.selected = Some(root);
        push_model_warning(&mut app.store, child, "mono warning");
        let rendered = rendered_frame(&mut app, 60, 30);
        assert!(rendered.contains("WARNING"));
        assert!(!rendered.contains("ERROR"));
        assert!(rendered.contains("mono warning"));
        assert!(rendered.contains("Kid"));
        // Tiny terminals render the aggregated warning without panicking;
        // with a one-row viewport the textual header may scroll off, but the
        // warning body remains reachable at the live tail.
        for (width, height) in [(16, 10), (8, 8), (4, 3)] {
            let rendered = rendered_frame(&mut app, width, height);
            assert!(
                !rendered.contains("ERROR"),
                "no error styling at {width}x{height}"
            );
        }
        let tail = rendered_frame(&mut app, 16, 10);
        assert!(tail.contains("─") || tail.contains("Conversation"));
    }

    #[tokio::test]
    async fn aggregated_warnings_never_touch_parent_model_context_or_tool_results() {
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let tool_id = ToolCallId::new_v7();
        let mut app = test_app();
        app.tree_root = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: session_meta(child),
                children: Vec::new(),
            }],
        });
        app.selected = Some(root);
        let mut root_state = SessionState {
            transcript: vec![TranscriptItem::tool(1, tool_id)],
            ..SessionState::default()
        };
        root_state.tools.insert(
            tool_id,
            ToolCallState {
                id: tool_id,
                tool: "delegate".into(),
                arguments: "{\"task\":\"child\"}".into(),
                status: ToolStatus::Completed,
                detail: "final report only".into(),
            },
        );
        app.store.sessions.insert(root, root_state);
        push_model_warning(&mut app.store, child, "descendant-only warning text");

        // The warning aggregates into the root view...
        assert_eq!(app.descendant_warnings(root).len(), 1);
        // ...but the parent session's durable projection — the exact data the
        // parent model's context and tool results are built from — is
        // untouched: no warning items, no injected text.
        let parent = &app.store.sessions[&root];
        assert!(parent.transcript.iter().all(|item| !matches!(
            item,
            TranscriptItem::Event {
                level: crate::state::EventLevel::Warning,
                ..
            }
        )));
        assert!(
            parent
                .transcript
                .iter()
                .all(|item| !format!("{item:?}").contains("descendant-only warning text"))
        );
        assert!(!parent.tools[&tool_id].detail.contains("warning"));
    }

    #[tokio::test]
    async fn scrollbar_is_reserved_drawn_and_pages_on_track_press() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, tall_transcript_state(120));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("conversation render");

        let track = app.hit_map.scrollbar.expect("scrollbar column reserved");
        assert_eq!(track.width, 1);
        let conversation = app.hit_map.conversation.expect("conversation");
        assert_eq!(track.x, conversation.x + conversation.width);
        assert!(
            app.hit_map
                .blocks
                .iter()
                .all(|hit| hit.rect.x + hit.rect.width <= track.x),
            "block hit regions never cover the scrollbar column"
        );
        let geometry = app.scrollbar_geometry.expect("geometry");
        assert!(geometry.max_offset > 0);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(geometry.thumb.x, geometry.thumb.y)].symbol(), "█");

        // Track press pages the viewport to the pressed position.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: track.x,
            row: track.y + track.height / 2,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert!(app.scrollbar_drag.is_none());
        assert!(!app.conversation_scroll.following);
        assert!(app.conversation_scroll.offset > 0);
    }

    #[tokio::test]
    async fn scrollbar_thumb_press_drag_outside_track_and_release_scrolls_exactly() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, tall_transcript_state(200));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("conversation render");
        let geometry = app.scrollbar_geometry.expect("geometry");
        assert_eq!(app.conversation_scroll.offset, geometry.max_offset);

        // Press the thumb: capture starts without moving the content.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: geometry.thumb.x,
            row: geometry.thumb.y,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.scrollbar_drag.map(|drag| drag.grab_row), Some(0));
        assert_eq!(app.conversation_scroll.offset, geometry.max_offset);

        // Drag above the track (outside it) pins the top exactly.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: geometry.thumb.x,
            row: geometry.track.y.saturating_sub(4),
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.conversation_scroll.offset, 0);
        assert!(!app.conversation_scroll.following);

        // Drag to the last track row pins the exact bottom and releases
        // capture; the following draw re-engages live tail following.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: geometry.thumb.x,
            row: geometry.track.y + geometry.track.height - 1,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.conversation_scroll.offset, geometry.max_offset);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: geometry.thumb.x,
            row: geometry.track.y,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert!(app.scrollbar_drag.is_none());
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("conversation render after release");
        assert!(app.conversation_scroll.following);
        assert_eq!(app.conversation_scroll.offset, geometry.max_offset);

        // A drag without a captured thumb never scrolls.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.conversation_scroll.offset, geometry.max_offset);
    }

    #[tokio::test]
    async fn repeated_drags_keep_identical_thumb_height_top_middle_bottom() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, tall_transcript_state(200));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        let mut heights = Vec::new();
        // Top → middle → bottom → middle → top: identical thumb height at
        // every stop while content and viewport are unchanged.
        for target in [0.0, 0.5, 1.0, 0.5, 0.0] {
            terminal
                .draw(|frame| app.draw_for_test(frame))
                .expect("render");
            let geometry = app.scrollbar_geometry.expect("geometry");
            let row = geometry.track.y
                + u16::try_from((f64::from(geometry.track.height - 1) * target).round() as usize)
                    .unwrap_or(0);
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: geometry.track.x,
                row: geometry.thumb.y,
                modifiers: KeyModifiers::NONE,
            })
            .await;
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: geometry.track.x,
                row,
                modifiers: KeyModifiers::NONE,
            })
            .await;
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: geometry.track.x,
                row,
                modifiers: KeyModifiers::NONE,
            })
            .await;
            terminal
                .draw(|frame| app.draw_for_test(frame))
                .expect("render after drag");
            let after = app.scrollbar_geometry.expect("geometry after drag");
            heights.push(after.thumb.height);
            assert_eq!(after.thumb.height, geometry.thumb.height);
        }
        assert!(
            heights.windows(2).all(|pair| pair[0] == pair[1]),
            "thumb height {heights:?} is constant across repeated drags"
        );
        // Final position is the top: full-height thumb flush at the track top.
        let geometry = app.scrollbar_geometry.expect("final geometry");
        assert_eq!(app.conversation_scroll.offset, 0);
        assert_eq!(geometry.thumb.y, geometry.track.y);
    }

    #[tokio::test]
    async fn bottom_drag_reengages_following_without_hiding_or_shrinking_thumb() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, tall_transcript_state(200));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("render");
        let geometry = app.scrollbar_geometry.expect("geometry");
        let resting = geometry.thumb;

        // Drag to the top, then back to the exact bottom.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: geometry.thumb.x,
            row: geometry.thumb.y,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: geometry.track.x,
            row: geometry.track.y,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: geometry.track.x,
            row: geometry.track.y + geometry.track.height - 1,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: geometry.track.x,
            row: geometry.track.y + geometry.track.height - 1,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("render at bottom");
        let bottom = app.scrollbar_geometry.expect("scrollbar visible at bottom");
        assert!(app.conversation_scroll.following);
        assert_eq!(app.conversation_scroll.offset, bottom.max_offset);
        assert_eq!(bottom.thumb.height, resting.height);
        assert_eq!(
            bottom.thumb.y + bottom.thumb.height,
            bottom.track.y + bottom.track.height,
            "thumb is flush against the track bottom at full height"
        );
    }

    #[tokio::test]
    async fn thumb_height_follows_only_true_geometry_changes() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, tall_transcript_state(120));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("initial render");
        let initial = app.scrollbar_geometry.expect("geometry").thumb.height;

        // Scrolling alone never changes the thumb height.
        app.conversation_scroll.scroll_to(10);
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("scrolled render");
        assert_eq!(
            app.scrollbar_geometry.expect("geometry").thumb.height,
            initial
        );

        // Expanding a collapsible block adds rendered lines and shrinks the
        // thumb — a genuine content-height change.
        app.store
            .sessions
            .get_mut(&session_id)
            .expect("session")
            .transcript
            .push(TranscriptItem::assistant_parts(vec![
                AssistantPart::Thinking {
                    id: 900,
                    version: 0,
                    text: "expanded\nbody\nlines\nhere".into(),
                },
            ]));
        app.expanded_blocks
            .entry(session_id)
            .or_default()
            .insert(BlockId::Thinking(900));
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("expanded render");
        let expanded = app.scrollbar_geometry.expect("geometry").thumb.height;
        assert!(expanded <= initial);

        // Resizing the terminal taller grows the viewport and therefore the
        // thumb; resizing back restores the original height exactly.
        let mut tall = Terminal::new(TestBackend::new(100, 44)).expect("terminal");
        tall.draw(|frame| app.draw_for_test(frame))
            .expect("tall render");
        let taller = app.scrollbar_geometry.expect("geometry").thumb.height;
        assert!(taller > expanded);
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("restored render");
        assert_eq!(
            app.scrollbar_geometry.expect("geometry").thumb.height,
            expanded
        );
    }

    #[tokio::test]
    async fn scrollbar_survives_resize_and_content_mutation() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store
            .sessions
            .insert(session_id, tall_transcript_state(120));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("initial render");
        app.conversation_scroll.scroll_to(5);

        // Shrink the terminal: the offset clamps into the new valid range
        // and the geometry (when the content still overflows) follows.
        let mut small = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
        small
            .draw(|frame| app.draw_for_test(frame))
            .expect("shrunk render");
        let geometry = app.scrollbar_geometry;
        assert!(
            app.conversation_scroll.offset <= geometry.map_or(0, |geometry| geometry.max_offset),
            "offset {} clamps to the shrunk valid range {:?}",
            app.conversation_scroll.offset,
            geometry.map(|geometry| geometry.max_offset),
        );

        // Mutate content below the overflow threshold: the scrollbar hides
        // and the scroll offset stays inside the valid range.
        app.store
            .sessions
            .get_mut(&session_id)
            .expect("session")
            .transcript
            .truncate(1);
        small
            .draw(|frame| app.draw_for_test(frame))
            .expect("mutated render");
        assert!(
            app.scrollbar_geometry.is_none(),
            "short content hides the scrollbar"
        );
        assert_eq!(app.conversation_scroll.offset, 0);
        assert!(app.conversation_scroll.following);
    }

    #[tokio::test]
    async fn sessions_picker_shows_titles_with_filtering_and_no_results_state() {
        let titled = SessionId::new_v7();
        let untitled = SessionId::new_v7();
        let mut titled_meta = session_meta(titled);
        titled_meta.title = Some(
            cookie_agent_protocol::SessionTitle::new("Fix flaky scrollbar test").expect("title"),
        );
        let mut app = test_app();
        app.sessions = vec![titled_meta, session_meta(untitled)];
        app.modal = Modal::Sessions;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("sessions picker render");
        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|y| {
                (0..terminal.backend().buffer().area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Fix flaky scrollbar test"));
        assert!(rendered.contains(&super::super::pickers::short_id(titled)));
        assert!(
            !rendered.contains(&titled.to_string()),
            "full ID is not the label"
        );
        assert!(rendered.contains("primary · untitled"));

        // Responsive filtering while typing.
        for character in "flaky".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .await;
        }
        assert_eq!(app.filtered_sessions().len(), 1);
        assert_eq!(app.filtered_sessions()[0].id, titled);

        // No-results state keeps the picker interactive; Ctrl-U restores it.
        for character in "zzz".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .await;
        }
        assert!(app.filtered_sessions().is_empty());
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("no-results render");
        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|y| {
                (0..terminal.backend().buffer().area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("No matches"));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.filtered_sessions().len(), 2);
    }

    #[tokio::test]
    async fn provider_picker_filters_by_credential_label_and_selects_with_keyboard_and_mouse() {
        let mut app = test_app();
        app.providers = vec![
            CatalogProvider {
                id: "acme-ai".into(),
                name: "Acme AI".into(),
                credential_fields: vec!["ACME_API_KEY".into()],
                npm: None,
                api: Some("https://api.acme.example".into()),
                documentation_url: None,
            },
            CatalogProvider {
                id: "other".into(),
                name: "Other Vendor".into(),
                credential_fields: vec!["OTHER_TOKEN".into()],
                npm: None,
                api: None,
                documentation_url: Some("https://docs.other.example".into()),
            },
        ];
        app.run_command(SlashCommand::Connect).await;
        assert_eq!(app.modal, Modal::ConnectProviders);

        for character in "token".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .await;
        }
        let filtered = app.filtered_providers();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider.id, "other");
        assert!(filtered[0].label.contains("OTHER_TOKEN"));

        // Keyboard Enter advances into the credential flow for the match.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectCredentials);
        assert_eq!(
            app.connect_provider
                .as_ref()
                .map(|provider| provider.id.as_str()),
            Some("other")
        );
        assert!(app.picker_query.is_empty());

        // Mouse selection on a filtered row activates the same entry.
        let mut app = test_app();
        app.providers = vec![CatalogProvider {
            id: "acme-ai".into(),
            name: "Acme AI".into(),
            credential_fields: Vec::new(),
            npm: None,
            api: None,
            documentation_url: None,
        }];
        app.run_command(SlashCommand::Connect).await;
        app.hit_map.modal_open = true;
        app.hit_map.picker_rows.push(PickerRowHit {
            rect: Rect::new(1, 1, 10, 1),
            index: 0,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 1,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.modal, Modal::ConnectConfirm);
        assert_eq!(
            app.connect_provider
                .as_ref()
                .map(|provider| provider.id.as_str()),
            Some("acme-ai")
        );
    }

    #[tokio::test]
    async fn tree_rows_show_titles_and_keep_cursor_by_session_id_across_refresh() {
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let mut root_meta = session_meta(root);
        root_meta.title =
            Some(cookie_agent_protocol::SessionTitle::new("Root investigation").expect("title"));
        let mut app = test_app();
        app.selected = Some(child);
        app.tree_root = Some(root);
        app.tree_cursor = Some(child);
        app.tree = Some(SessionTree {
            session: root_meta,
            children: vec![SessionTree {
                session: session_meta(child),
                children: Vec::new(),
            }],
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("tree render");
        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|y| {
                (0..terminal.backend().buffer().area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Root investigation"));
        assert!(rendered.contains('●'), "watched child is visually distinct");
        assert!(!rendered.contains(&child.to_string()));
        // The title renders in the semantic user color; the shortened ID is
        // subdued secondary metadata.
        let buffer = terminal.backend().buffer();
        let title_row = (0..buffer.area.height)
            .find(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("Root investigation")
            })
            .expect("title row");
        let title_x = (0..buffer.area.width)
            .find(|x| buffer[(*x, title_row)].symbol() == "R")
            .expect("title cell");
        assert_eq!(
            buffer[(title_x, title_row)].style().fg,
            app.theme.user().fg,
            "title uses semantic color"
        );
        let id_x = (0..buffer.area.width)
            .find(|x| buffer[(*x, title_row)].symbol() == "(")
            .expect("id cell");
        assert_eq!(
            buffer[(id_x, title_row)].style().fg,
            app.theme.muted().fg,
            "short id is subdued"
        );
        // A refresh inserting a sibling above the cursor keeps the cursor on
        // the same SessionId, not the same row index.
        let inserted = SessionId::new_v7();
        let mut root_meta = session_meta(root);
        root_meta.title =
            Some(cookie_agent_protocol::SessionTitle::new("Root investigation").expect("title"));
        app.tree = Some(SessionTree {
            session: root_meta,
            children: vec![
                SessionTree {
                    session: session_meta(inserted),
                    children: Vec::new(),
                },
                SessionTree {
                    session: session_meta(child),
                    children: Vec::new(),
                },
            ],
        });
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("tree refresh render");
        assert_eq!(app.tree_cursor, Some(child));
        let cursor_row = app
            .hit_map
            .tree_rows
            .iter()
            .position(|hit| hit.session_id == child)
            .expect("cursor row visible");
        assert_eq!(cursor_row, 2);
    }

    fn read_tool_state(path: &str, status: ToolStatus, detail: &str) -> SessionState {
        let tool_id = ToolCallId::new_v7();
        let mut state = SessionState {
            transcript: vec![TranscriptItem::tool(1, tool_id)],
            ..SessionState::default()
        };
        state.tools.insert(
            tool_id,
            ToolCallState {
                id: tool_id,
                tool: "read".into(),
                arguments: format!("{{\"path\":\"{path}\"}}"),
                status,
                detail: detail.into(),
            },
        );
        state
    }

    fn expanded_read_layout(
        state: &SessionState,
        tool_id: ToolCallId,
        theme: &Theme,
        highlighter: &dyn Highlighter,
    ) -> Vec<Line<'static>> {
        let expanded = HashSet::from([BlockId::Tool(tool_id)]);
        let mut assistant_parts = HashMap::new();
        let mut passes = 0;
        let TranscriptItem::Tool { call_id, .. } = &state.transcript[0] else {
            panic!("tool item");
        };
        assert_eq!(*call_id, tool_id);
        transcript_item_layout(
            state,
            &state.transcript[0],
            &mut TranscriptRenderContext {
                expanded: Some(&expanded),
                selected_block: None,
                width: 80,
                theme,
                highlighter,
                minimum_event_level: crate::state::EventLevel::Debug,
                assistant_part_cache: &mut assistant_parts,
                assistant_part_layout_passes: &mut passes,
            },
        )
        .lines
    }

    fn read_tool_id(state: &SessionState) -> ToolCallId {
        let TranscriptItem::Tool { call_id, .. } = &state.transcript[0] else {
            panic!("tool item");
        };
        *call_id
    }

    /// Content spans of a rendered tool body: spans on gutter-prefixed body
    /// lines, excluding the gutter prefixes and the styled toggle chevron.
    fn content_spans<'a>(lines: &'a [Line<'static>]) -> Vec<&'a ratatui::text::Span<'static>> {
        lines
            .iter()
            .filter(|line| {
                line.spans
                    .first()
                    .is_some_and(|span| matches!(span.content.as_ref(), "┃ " | "│ " | "┆ "))
            })
            .flat_map(|line| line.spans.iter())
            .filter(|span| {
                !matches!(span.content.as_ref(), "┃ " | "│ " | "┆ ")
                    && !span.content.starts_with(['▸', '▾'])
            })
            .collect()
    }

    #[test]
    fn read_rust_output_is_syntax_highlighted_with_tool_gutter_preserved() {
        let state = read_tool_state(
            "src/main.rs",
            ToolStatus::Completed,
            "Read 2 lines\nfn main() {\n    let x = 1;\n}",
        );
        let tool_id = read_tool_id(&state);
        let theme = Theme::default();
        let lines = expanded_read_layout(&state, tool_id, &theme, &SyntectHighlighter::default());
        let rendered = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
        // Tool gutter and header are preserved.
        assert!(
            rendered
                .iter()
                .any(|line| line.starts_with("┏✓ TOOL SUCCESS"))
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("▾ read — COMPLETED ✓"))
        );
        assert!(
            rendered
                .iter()
                .all(|line| line.starts_with('┏') || line.starts_with("┃ "))
        );
        // At least one content span carries a quantized foreground color and
        // none touches the background.
        let colored = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style.fg.is_some())
            .count();
        assert!(colored > 0, "rust keywords are colored");
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .all(|span| span.style.bg.is_none())
        );
        assert!(rendered.iter().any(|line| line.contains("fn main() {")));
    }

    #[test]
    fn read_json_and_shell_outputs_are_highlighted_but_unknown_extensions_stay_plain() {
        let highlighter = SyntectHighlighter::default();
        let theme = Theme::default();
        let json = read_tool_state(
            "data/config.json",
            ToolStatus::Completed,
            "Read 1 line\n{\"key\": 1}",
        );
        let json_lines = expanded_read_layout(&json, read_tool_id(&json), &theme, &highlighter);
        assert!(
            json_lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.style.fg.is_some())
        );

        let shell = read_tool_state(
            "scripts/build.sh",
            ToolStatus::Completed,
            "Read 1 line\necho \"$HOME\"",
        );
        let shell_lines = expanded_read_layout(&shell, read_tool_id(&shell), &theme, &highlighter);
        assert!(
            shell_lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.style.fg.is_some())
        );

        let unknown = read_tool_state(
            "notes/todo.zzz9",
            ToolStatus::Completed,
            "Read 1 line\nplain text output",
        );
        let unknown_lines =
            expanded_read_layout(&unknown, read_tool_id(&unknown), &theme, &highlighter);
        assert!(
            content_spans(&unknown_lines)
                .iter()
                .all(|span| span.style.fg.is_none()),
            "unknown extension renders plain"
        );
        assert!(
            unknown_lines
                .iter()
                .any(|line| line.to_string().contains("plain text output"))
        );
    }

    #[test]
    fn read_truncation_and_attachment_metadata_stay_plain_after_highlighted_content() {
        let detail = "Read huge file\nline one\nline two\nretained output: artifact://sha256/abc (999 bytes, 40 lines)\nattachment: application/pdf · 42 bytes · sha256:ff · artifact://sha256/ff";
        let state = read_tool_state("big/log.rs", ToolStatus::Completed, detail);
        let theme = Theme::default();
        let lines = expanded_read_layout(
            &state,
            read_tool_id(&state),
            &theme,
            &SyntectHighlighter::default(),
        );
        let metadata_lines = lines
            .iter()
            .filter(|line| {
                let text = line.to_string();
                text.contains("retained output:") || text.contains("attachment:")
            })
            .collect::<Vec<_>>();
        assert_eq!(metadata_lines.len(), 2);
        for line in metadata_lines {
            assert!(
                line.spans
                    .iter()
                    .filter(|span| !matches!(span.content.as_ref(), "┃ " | "│ " | "┆ "))
                    .all(|span| span.style.fg.is_none()),
                "metadata stays plain: {line:?}"
            );
        }
        // Content before the metadata is still highlighted.
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.style.fg.is_some())
        );
    }

    #[test]
    fn read_errors_and_non_read_tools_stay_plain() {
        let theme = Theme::default();
        let failed = read_tool_state(
            "src/missing.rs",
            ToolStatus::Failed,
            "read failed: no such file",
        );
        let lines = expanded_read_layout(
            &failed,
            read_tool_id(&failed),
            &theme,
            &SyntectHighlighter::default(),
        );
        assert!(
            content_spans(&lines)
                .iter()
                .all(|span| span.style.fg.is_none())
        );

        let mut bash = read_tool_state("src/main.rs", ToolStatus::Completed, "fn main() {}");
        let tool_id = read_tool_id(&bash);
        bash.tools.get_mut(&tool_id).expect("tool").tool = "bash".into();
        let lines = expanded_read_layout(&bash, tool_id, &theme, &SyntectHighlighter::default());
        assert!(
            content_spans(&lines)
                .iter()
                .all(|span| span.style.fg.is_none())
        );
    }

    #[test]
    fn read_highlighting_is_theme_quantized_and_plain_in_mono() {
        let state = read_tool_state(
            "src/lib.rs",
            ToolStatus::Completed,
            "Read 1 line\npub fn answer() -> u8 { 42 }",
        );
        let tool_id = read_tool_id(&state);
        let highlighter = SyntectHighlighter::default();
        let ansi16 = Theme::new(
            crate::theme::ThemeKind::Default,
            crate::theme::ColorLevel::Ansi16,
        );
        let lines = expanded_read_layout(&state, tool_id, &ansi16, &highlighter);
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .all(|span| !matches!(span.style.fg, Some(Color::Rgb(..)))),
            "ANSI-16 terminals never receive RGB colors"
        );

        let mono = Theme::new(
            crate::theme::ThemeKind::Mono,
            crate::theme::ColorLevel::None,
        );
        let lines = expanded_read_layout(&state, tool_id, &mono, &highlighter);
        assert!(
            content_spans(&lines)
                .iter()
                .all(|span| span.style.fg.is_none()),
            "mono stays uncolored but keeps content"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("pub fn answer()"))
        );
    }

    #[test]
    fn read_highlight_caches_by_tool_item_version() {
        let mut state = read_tool_state(
            "src/lib.rs",
            ToolStatus::Completed,
            "Read 1 line\npub fn answer() -> u8 { 42 }",
        );
        let session = SessionId::new_v7();
        let expanded = HashSet::from([BlockId::Tool(read_tool_id(&state))]);
        let mut cache = LayoutCache::default();
        let theme = Theme::default();
        let highlighter = PlainHighlighter;
        assert!(!ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            Some(&expanded),
            None,
            80,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        ));
        assert_eq!(cache.item_layout_passes, 1);
        // Identical frame: fully cached, no re-layout of the read body.
        assert!(ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            Some(&expanded),
            None,
            80,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        ));
        assert_eq!(cache.item_layout_passes, 1);
        // A new unrelated transcript item only lays out that item.
        state.transcript.push(TranscriptItem::internal("tick"));
        assert!(!ensure_cached_transcript_layout(
            &mut cache,
            session,
            &state,
            Some(&expanded),
            None,
            80,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
        ));
        assert_eq!(cache.item_layout_passes, 2);
    }

    #[test]
    fn read_path_extension_parsing_is_deterministic() {
        assert_eq!(
            read_path_extension("{\"path\":\"/a/b/main.rs\"}"),
            Some("rs")
        );
        assert_eq!(
            read_path_extension("{\"path\":\"C:\\\\src\\\\lib.rs\"}"),
            Some("rs")
        );
        assert_eq!(read_path_extension("{\"path\":\"no-extension\"}"), None);
        assert_eq!(read_path_extension("{\"path\":\".hidden\"}"), None);
        assert_eq!(read_path_extension("not json"), None);
        assert_eq!(read_path_extension("{\"offset\":1}"), None);
    }

    fn diagnostic_state() -> SessionState {
        SessionState {
            transcript: vec![
                TranscriptItem::Event {
                    id: 1,
                    version: 0,
                    level: crate::state::EventLevel::Debug,
                    text: "replay decision detail".into(),
                },
                TranscriptItem::Event {
                    id: 2,
                    version: 0,
                    level: crate::state::EventLevel::Info,
                    text: "run completed".into(),
                },
                TranscriptItem::Event {
                    id: 3,
                    version: 0,
                    level: crate::state::EventLevel::Warning,
                    text: "cache entry discarded".into(),
                },
                TranscriptItem::Event {
                    id: 4,
                    version: 0,
                    level: crate::state::EventLevel::Error,
                    text: "run failed: boom".into(),
                },
                TranscriptItem::user("hello"),
            ],
            ..SessionState::default()
        }
    }

    #[test]
    fn event_badges_render_textually_for_every_level_and_theme() {
        let state = diagnostic_state();
        for theme in [
            Theme::default(),
            Theme::new(
                crate::theme::ThemeKind::Mono,
                crate::theme::ColorLevel::None,
            ),
            Theme::new(
                crate::theme::ThemeKind::HighContrast,
                crate::theme::ColorLevel::Ansi16,
            ),
        ] {
            let layout = transcript_layout_with_level(
                &state,
                None,
                80,
                &theme,
                &PlainHighlighter,
                crate::state::EventLevel::Debug,
            );
            let rendered = layout
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            for (badge, needle) in [
                ("[D]", "replay decision detail"),
                ("[I]", "run completed"),
                ("[W]", "cache entry discarded"),
                ("[E]", "run failed: boom"),
            ] {
                assert!(rendered.contains(badge), "{badge} in {:?}", theme.key());
                assert!(rendered.contains(needle));
            }
            // Error styling differs from warning styling in colored themes;
            // every level is readable without color via the textual badge.
            if theme.key().colors != crate::theme::ColorLevel::None {
                assert_ne!(theme.error(), theme.warning());
            }
        }
    }

    #[test]
    fn event_threshold_hides_lower_levels_without_removing_them_from_state() {
        let state = diagnostic_state();
        let visible_at = |level| {
            transcript_layout_with_level(
                &state,
                None,
                80,
                &Theme::default(),
                &PlainHighlighter,
                level,
            )
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
        };
        let warning_view = visible_at(crate::state::EventLevel::Warning);
        assert!(!warning_view.contains("replay decision detail"));
        assert!(!warning_view.contains("run completed"));
        assert!(warning_view.contains("cache entry discarded"));
        assert!(warning_view.contains("run failed: boom"));
        // Conversation content is never filtered.
        assert!(warning_view.contains("hello"));

        let error_view = visible_at(crate::state::EventLevel::Error);
        assert!(!error_view.contains("cache entry discarded"));
        assert!(error_view.contains("run failed: boom"));

        // Lowering the threshold reveals the same rows — no refetch, no
        // state mutation, identical text.
        let debug_view = visible_at(crate::state::EventLevel::Debug);
        for needle in [
            "replay decision detail",
            "run completed",
            "cache entry discarded",
            "run failed: boom",
        ] {
            assert!(debug_view.contains(needle));
        }
        assert_eq!(state.transcript.len(), 5, "projection untouched");
    }

    #[tokio::test]
    async fn events_command_changes_the_filter_and_reveals_hidden_rows() {
        let session_id = SessionId::new_v7();
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store.sessions.insert(session_id, diagnostic_state());
        // Default WARNING threshold: indicator visible, debug/info hidden.
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("events ≥ warning"));
        assert!(!rendered.contains("replay decision detail"));

        app.run_command(SlashCommand::Events(crate::state::EventLevel::Debug))
            .await;
        assert_eq!(
            app.tui_config.minimum_event_level,
            crate::state::EventLevel::Debug
        );
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("events ≥ debug"));
        assert!(rendered.contains("[D]"));
        assert!(rendered.contains("replay decision detail"));

        app.run_command(SlashCommand::Events(crate::state::EventLevel::Error))
            .await;
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("events ≥ error"));
        assert!(!rendered.contains("cache entry discarded"));
        assert!(rendered.contains("run failed: boom"));
    }

    fn chevron_counts(rendered: &str) -> (usize, usize) {
        (rendered.matches('▸').count(), rendered.matches('▾').count())
    }

    #[test]
    fn assistant_tables_stay_inside_the_gutter_and_wrap_to_width() {
        let state = SessionState {
            transcript: vec![TranscriptItem::assistant(
                "before\n\n| name | value |\n|------|-------|\n| alpha | 1 |\n\nafter",
            )],
            ..SessionState::default()
        };
        let layout = transcript_layout(&state, None, 40);
        let rendered = layout
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        // Every table row carries the assistant gutter and fits the width.
        let table_rows = rendered
            .iter()
            .filter(|line| {
                line.contains('┌')
                    || line.contains('├')
                    || line.contains('└')
                    || line.contains("alpha")
            })
            .collect::<Vec<_>>();
        assert!(table_rows.len() >= 4);
        for line in &table_rows {
            assert!(line.starts_with("│ "), "gutter: {line}");
            assert!(UnicodeWidthStr::width(line.as_str()) <= 40, "fit: {line}");
        }
        assert!(rendered.iter().any(|line| line.contains("alpha")));
        assert!(rendered.iter().any(|line| line.contains("after")));
        // Resize reflow: a narrower width re-lays the table, never overflows.
        let narrow = transcript_layout(&state, None, 24);
        assert!(
            narrow
                .lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.to_string().as_str()) <= 24),
            "narrow reflow"
        );
        // Tiny width: stacked fallback stays inside the gutter.
        let tiny = transcript_layout(&state, None, 10);
        let tiny_text = tiny
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            tiny_text.contains("name:") && tiny_text.contains("alpha"),
            "stacked fallback in tiny gutter: {tiny_text}"
        );
    }

    /// Whether any line containing `needle` carries an underline either at the
    /// line level (style patch from `Line::styled`) or on a span.
    fn lines_contain_underlined(lines: &[Line<'static>], needle: &str) -> bool {
        lines.iter().any(|line| {
            line.to_string().contains(needle)
                && (line.style.add_modifier.contains(Modifier::UNDERLINED)
                    || line
                        .spans
                        .iter()
                        .any(|span| span.style.add_modifier.contains(Modifier::UNDERLINED)))
        })
    }

    #[test]
    fn thinking_toggle_shows_exactly_one_chevron_in_every_state() {
        let state = SessionState {
            transcript: vec![TranscriptItem::assistant_parts(vec![
                AssistantPart::Thinking {
                    id: 7,
                    version: 0,
                    text: "plain thought".into(),
                },
            ])],
            ..SessionState::default()
        };
        for (expanded, selected, expected_glyph) in [
            (false, false, '▸'),
            (true, false, '▾'),
            (false, true, '▸'),
            (true, true, '▾'),
        ] {
            let expanded_blocks = expanded.then(|| HashSet::from([BlockId::Thinking(7)]));
            let selected_block = selected.then_some(BlockId::Thinking(7));
            let layout = transcript_layout_with_selection(
                &state,
                expanded_blocks.as_ref(),
                selected_block,
                80,
                &Theme::default(),
            );
            let rendered = layout
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            let (collapsed, expanded_count) = chevron_counts(&rendered);
            assert_eq!(
                collapsed + expanded_count,
                1,
                "expanded={expanded} selected={selected}: {rendered}"
            );
            assert!(rendered.contains(expected_glyph));
            assert!(!rendered.contains('▶'), "no selection triangle: {rendered}");
            if selected {
                assert!(
                    lines_contain_underlined(&layout.lines, "thinking"),
                    "selection is conveyed through underline style"
                );
            }
        }
    }

    #[test]
    fn thinking_toggle_single_chevron_holds_in_mono_and_tiny_widths() {
        let state = SessionState {
            transcript: vec![TranscriptItem::assistant_parts(vec![
                AssistantPart::Thinking {
                    id: 7,
                    version: 0,
                    text: "界界 thought".into(),
                },
            ])],
            ..SessionState::default()
        };
        let mono = Theme::new(
            crate::theme::ThemeKind::Mono,
            crate::theme::ColorLevel::None,
        );
        for expanded in [false, true] {
            let expanded_blocks = expanded.then(|| HashSet::from([BlockId::Thinking(7)]));
            for width in 1..12 {
                for selected in [false, true] {
                    let layout = transcript_layout_with_selection(
                        &state,
                        expanded_blocks.as_ref(),
                        selected.then_some(BlockId::Thinking(7)),
                        width,
                        &mono,
                    );
                    let rendered = layout
                        .lines
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n");
                    let (collapsed, expanded_count) = chevron_counts(&rendered);
                    assert_eq!(
                        collapsed + expanded_count,
                        1,
                        "mono width={width} expanded={expanded} selected={selected}: {rendered}"
                    );
                    assert!(!rendered.contains('▶'));
                    if selected {
                        assert!(
                            lines_contain_underlined(&layout.lines, "▸")
                                || lines_contain_underlined(&layout.lines, "▾"),
                            "mono selection is underline-only at width {width}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn tool_toggle_shows_exactly_one_chevron_in_every_state() {
        let tool_id = ToolCallId::new_v7();
        let mut state = SessionState {
            transcript: vec![TranscriptItem::tool(1, tool_id)],
            ..SessionState::default()
        };
        state.tools.insert(
            tool_id,
            ToolCallState {
                id: tool_id,
                tool: "bash".into(),
                arguments: "{}".into(),
                status: ToolStatus::Completed,
                detail: "done".into(),
            },
        );
        for (expanded, selected, expected_glyph) in [
            (false, false, '▸'),
            (true, false, '▾'),
            (false, true, '▸'),
            (true, true, '▾'),
        ] {
            let expanded_blocks = expanded.then(|| HashSet::from([BlockId::Tool(tool_id)]));
            let layout = transcript_layout_with_selection(
                &state,
                expanded_blocks.as_ref(),
                selected.then_some(BlockId::Tool(tool_id)),
                80,
                &Theme::default(),
            );
            let rendered = layout
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            let (collapsed, expanded_count) = chevron_counts(&rendered);
            assert_eq!(
                collapsed + expanded_count,
                1,
                "expanded={expanded} selected={selected}: {rendered}"
            );
            assert!(rendered.contains(expected_glyph));
            assert!(!rendered.contains('▶'));
        }
    }

    fn app_with_approval(client: Client) -> (App, SessionId, cookie_agent_protocol::ApprovalId) {
        let session_id = SessionId::new_v7();
        let approval = approval(session_id);
        let approval_id = approval.approval_id;
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                approvals: vec![approval],
                ..SessionState::default()
            },
        );
        (app, session_id, approval_id)
    }

    fn store_has_visible_approval(store: &StateStore, session_id: SessionId) -> bool {
        let mut app = test_app();
        app.store = store.clone();
        app.selected = Some(session_id);
        app.current_approval().is_some()
    }

    #[tokio::test]
    async fn internal_request_is_hidden_until_escalation_and_cannot_respond() {
        let session_id = SessionId::new_v7();
        let request = approval_request();
        let approval_id = request.approval_id();
        let (client, sent) = recording_client();
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);

        assert!(app.store.apply_event(projection_event(
            session_id,
            1,
            Event::ApprovalRequested {
                request: request.clone(),
            },
        )));
        assert_eq!(app.store.sessions[&session_id].approvals.len(), 1);
        assert!(!app.store.sessions[&session_id].approvals[0].escalated);
        assert!(
            app.current_approval().is_none(),
            "request alone stays hidden"
        );

        app.answer_approval(ApprovalUserDecision::ApproveOnce).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(app.pending_approval.is_none());
        assert!(
            sent.lock()
                .expect("sent requests lock")
                .iter()
                .all(|request| request["method"] != "approval.respond"),
            "an internal request cannot produce a user response"
        );

        assert!(app.store.apply_event(projection_event(
            session_id,
            2,
            Event::ApprovalEscalated {
                approval_id,
                reason_code: ApprovalReasonCode::Escalated,
            },
        )));
        let visible = app
            .current_approval()
            .expect("escalated approval is visible");
        assert_eq!(visible.approval_id, approval_id);
        assert_eq!(visible.request_revision, 9);
        assert_eq!(
            visible.operation_fingerprint,
            request.operation_fingerprint().clone()
        );

        let missing_session = SessionId::new_v7();
        app.selected = Some(missing_session);
        assert!(app.store.apply_event(projection_event(
            missing_session,
            1,
            Event::ApprovalEscalated {
                approval_id: ApprovalId::new_v7(),
                reason_code: ApprovalReasonCode::Escalated,
            },
        )));
        assert!(
            app.current_approval().is_none(),
            "an escalation without its request fails closed"
        );
    }

    #[tokio::test]
    async fn internal_allow_and_deny_never_show_a_modal_and_execution_follows_finalization() {
        for (decision, approved) in [
            (ApprovalInternalDecisionKind::Allow, true),
            (ApprovalInternalDecisionKind::Deny, false),
        ] {
            let session_id = SessionId::new_v7();
            let tool_call_id = ToolCallId::new_v7();
            let request = approval_request();
            let approval_id = request.approval_id();
            let evaluations = request.evaluations().to_vec();
            let mut app = test_app();
            app.selected = Some(session_id);

            for (seq, event) in [
                Event::ToolCallStarted {
                    tool_call_id,
                    model_call_id: "approval-test".into(),
                    provider_item_id: None,
                    tool: "bash".into(),
                    arguments: json!({"command":"git status"}),
                },
                Event::ApprovalRequested { request },
                Event::ApprovalEvaluated {
                    approval_id,
                    decision: ApprovalInternalDecision {
                        decision,
                        source: ApprovalDecisionSource::InternalAgent,
                        reason_code: if approved {
                            ApprovalReasonCode::InternalAgentAllowed
                        } else {
                            ApprovalReasonCode::InternalAgentDenied
                        },
                        evaluations,
                    },
                },
            ]
            .into_iter()
            .enumerate()
            {
                assert!(
                    app.store
                        .apply_event(projection_event(session_id, seq as u64 + 1, event,))
                );
                assert!(app.current_approval().is_none());
            }
            assert_eq!(
                app.store.sessions[&session_id].tools[&tool_call_id].status,
                ToolStatus::Running,
                "the tool has not completed during internal evaluation"
            );

            assert!(app.store.apply_event(projection_event(
                session_id,
                4,
                Event::ApprovalFinalized {
                    approval_id,
                    decision: ApprovalFinalDecision {
                        outcome: if approved {
                            ApprovalFinalOutcome::Approved
                        } else {
                            ApprovalFinalOutcome::Rejected
                        },
                        source: ApprovalDecisionSource::InternalAgent,
                        reason_code: if approved {
                            ApprovalReasonCode::InternalAgentAllowed
                        } else {
                            ApprovalReasonCode::InternalAgentDenied
                        },
                        feedback: None,
                        tree_grant_id: None,
                    },
                },
            )));
            assert!(app.current_approval().is_none());
            assert_eq!(
                app.store.sessions[&session_id].tools[&tool_call_id].status,
                ToolStatus::Running,
                "finalization precedes the terminal tool event"
            );

            let terminal = if approved {
                Event::ToolCallCompleted {
                    tool_call_id,
                    result: ToolResult {
                        title: "git status".into(),
                        output: "clean".into(),
                        metadata: json!({}),
                        truncation: None,
                        attachments: Vec::new(),
                    },
                }
            } else {
                Event::ToolCallFailed {
                    tool_call_id,
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: "permission denied by internal approval agent".into(),
                }
            };
            assert!(
                app.store
                    .apply_event(projection_event(session_id, 5, terminal))
            );
            assert_eq!(
                app.store.sessions[&session_id].tools[&tool_call_id].status,
                if approved {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Failed
                }
            );
            assert!(app.current_approval().is_none());
        }
    }

    #[tokio::test]
    async fn approval_live_and_replay_projection_have_identical_escalation_visibility() {
        let session_id = SessionId::new_v7();
        let request = approval_request();
        let approval_id = request.approval_id();
        let requested = projection_event(session_id, 1, Event::ApprovalRequested { request });
        let escalated = projection_event(
            session_id,
            2,
            Event::ApprovalEscalated {
                approval_id,
                reason_code: ApprovalReasonCode::Escalated,
            },
        );

        let mut live = StateStore::default();
        live.apply_delivery(ClientDelivery::Live {
            message: Box::new(EventSubscriptionMessage::Event {
                event: requested.clone(),
            }),
            generation: 0,
        });

        let mut replay = StateStore::default();
        replay.apply_delivery(ClientDelivery::ReplayStart {
            session_id,
            generation: 0,
            final_seq: 1,
            rebuild: true,
        });
        replay.apply_delivery(ClientDelivery::ReplayEvent {
            session_id,
            generation: 0,
            final_seq: 1,
            event: Box::new(requested),
        });
        replay.apply_delivery(ClientDelivery::ReplayEnd {
            session_id,
            generation: 0,
            final_seq: 1,
        });
        assert!(!store_has_visible_approval(&live, session_id));
        assert!(!store_has_visible_approval(&replay, session_id));

        live.apply_delivery(ClientDelivery::Live {
            message: Box::new(EventSubscriptionMessage::Event {
                event: escalated.clone(),
            }),
            generation: 0,
        });
        replay.apply_delivery(ClientDelivery::ReplayStart {
            session_id,
            generation: 0,
            final_seq: 2,
            rebuild: false,
        });
        replay.apply_delivery(ClientDelivery::ReplayEvent {
            session_id,
            generation: 0,
            final_seq: 2,
            event: Box::new(escalated),
        });
        replay.apply_delivery(ClientDelivery::ReplayEnd {
            session_id,
            generation: 0,
            final_seq: 2,
        });
        assert!(store_has_visible_approval(&live, session_id));
        assert!(store_has_visible_approval(&replay, session_id));
    }

    #[tokio::test]
    async fn approval_list_and_root_child_queues_admit_only_escalated_records() {
        let root = SessionId::new_v7();
        let child = SessionId::new_v7();
        let mut app = test_app();
        app.tree_root = Some(root);
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![SessionTree {
                session: session_meta(child),
                children: Vec::new(),
            }],
        });
        app.store.sessions.insert(root, SessionState::default());
        app.store.sessions.insert(child, SessionState::default());
        let internal_request = approval_request();
        let internal_approval_id = internal_request.approval_id();
        assert!(app.store.apply_event(projection_event(
            root,
            1,
            Event::ApprovalRequested {
                request: internal_request,
            },
        )));

        app.apply_approval_list(
            root,
            ApprovalListResult {
                approvals: vec![
                    approval_record(root, ApprovalStatus::Pending),
                    approval_record(child, ApprovalStatus::Escalated),
                ],
                tree_grants: Vec::new(),
            },
        );
        app.selected = Some(root);
        assert!(
            app.current_approval().is_none(),
            "root pending stays hidden"
        );
        assert!(
            app.store.sessions[&root]
                .approvals
                .iter()
                .any(|approval| approval.approval_id == internal_approval_id && !approval.escalated),
            "refresh preserves event-projected internal state"
        );
        app.selected = Some(child);
        assert!(
            app.current_approval().is_some(),
            "child escalation is visible"
        );
        assert!(app.store.apply_event(projection_event(
            root,
            2,
            Event::ApprovalEscalated {
                approval_id: internal_approval_id,
                reason_code: ApprovalReasonCode::Escalated,
            },
        )));
        app.selected = Some(root);
        assert!(
            app.current_approval().is_some(),
            "a later escalation still reveals the preserved exact request"
        );

        app.apply_approval_list(
            root,
            ApprovalListResult {
                approvals: vec![
                    approval_record(root, ApprovalStatus::Escalated),
                    approval_record(child, ApprovalStatus::Pending),
                ],
                tree_grants: Vec::new(),
            },
        );
        app.selected = Some(child);
        assert!(
            app.current_approval().is_none(),
            "child pending stays hidden"
        );
        app.selected = Some(root);
        assert!(
            app.current_approval().is_some(),
            "root escalation is visible"
        );
    }

    #[tokio::test]
    async fn approval_modal_dismisses_before_the_response_lands_and_ignores_duplicates() {
        let (client, sent, held, _replies) = held_approval_client();
        let (mut app, session_id, approval_id) = app_with_approval(client);

        // Approve via the keyboard command path: the modal is removed from
        // the visible queue immediately, before the server responds.
        app.run_command(SlashCommand::Approve(ApprovalUserDecision::ApproveOnce))
            .await;
        assert!(app.current_approval().is_none(), "modal dismissed at once");
        assert!(app.pending_approval.is_some());
        assert!(app.status.contains("Approval submitted"));
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            sent.lock()
                .expect("sent")
                .iter()
                .any(|request| request["method"] == "approval.respond")
        );
        assert_eq!(
            held.lock().expect("held").len(),
            1,
            "response still in flight"
        );

        // A duplicate click while in flight sends nothing new.
        app.answer_approval(ApprovalUserDecision::Reject).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sent.lock()
                .expect("sent")
                .iter()
                .filter(|request| request["method"] == "approval.respond")
                .count(),
            1,
            "duplicate action ignored"
        );

        // The UI stays interactive: input editing works while in flight.
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.input.as_str(), "x");

        // Completing the response resolves the pending state.
        app.handle_rpc_update(RpcUpdate::ApprovalResponse {
            request_id: 1,
            approval_id,
            result: Ok(()),
        });
        assert!(app.pending_approval.is_none());
        assert!(app.status.contains("accepted"));
        assert!(
            app.store.sessions[&session_id].approvals.is_empty(),
            "no resurrection after success"
        );
    }

    #[tokio::test]
    async fn approval_failure_restores_the_modal_only_while_pending() {
        let (client, _sent, _held, _replies) = held_approval_client();
        let (mut app, session_id, approval_id) = app_with_approval(client);
        let captured = app.current_approval().expect("approval").clone();
        let mut second = approval(session_id);
        second.approval_id = ApprovalId::new_v7();
        app.store
            .sessions
            .get_mut(&session_id)
            .expect("session")
            .approvals
            .push(second);

        app.answer_approval(ApprovalUserDecision::ApproveOnce).await;
        assert!(app.current_approval().is_none());
        app.handle_rpc_update(RpcUpdate::ApprovalResponse {
            request_id: 1,
            approval_id,
            result: Err(ApprovalSubmissionError {
                message: "transport closed".into(),
                code: None,
            }),
        });
        // Transport failure: the exact captured request returns to the queue.
        let restored = app.current_approval().expect("modal restored");
        assert_eq!(restored.approval_id, approval_id);
        assert_eq!(restored.request_revision, captured.request_revision);
        assert_eq!(
            restored.operation_fingerprint,
            captured.operation_fingerprint
        );
        assert_eq!(app.store.sessions[&session_id].approvals.len(), 2);
        assert!(app.pending_approval.is_none());
        assert!(app.status.contains("approval response failed"));

        // Expiry during flight never resurrects the modal.
        let session_id = SessionId::new_v7();
        let mut expired = approval(session_id);
        expired.constraints.expires_at = Some(jiff::Timestamp::now());
        let approval_id = expired.approval_id;
        let mut app = test_app();
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                approvals: vec![expired.clone()],
                ..SessionState::default()
            },
        );
        // Force the submission to simulate the race where the request
        // expired after being shown and clicked.
        app.pending_approval = Some(PendingApprovalSubmission {
            request_id: 1,
            approval: expired,
            decision: ApprovalUserDecision::ApproveOnce,
        });
        app.handle_rpc_update(RpcUpdate::ApprovalResponse {
            request_id: 1,
            approval_id,
            result: Err(ApprovalSubmissionError {
                message: "transport closed".into(),
                code: None,
            }),
        });
        assert!(
            app.current_approval().is_none(),
            "expired request stays hidden"
        );
        assert!(app.status.contains("no longer pending"));
    }

    #[tokio::test]
    async fn approval_revision_conflict_refreshes_without_resubmitting() {
        let (client, sent, _held, _replies) = held_approval_client();
        let (mut app, _session_id, approval_id) = app_with_approval(client);

        app.answer_approval(ApprovalUserDecision::ApproveOnce).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let baseline = sent
            .lock()
            .expect("sent")
            .iter()
            .filter(|request| request["method"] == "approval.respond")
            .count();
        app.handle_rpc_update(RpcUpdate::ApprovalResponse {
            request_id: 1,
            approval_id,
            result: Err(ApprovalSubmissionError {
                message: "approval response rejected".into(),
                code: Some(ApprovalRespondErrorCode::ApprovalRevisionConflict),
            }),
        });
        assert!(
            app.current_approval().is_none(),
            "conflict never restores blindly"
        );
        assert!(app.status.contains("refreshing the approval list"));
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sent.lock()
                .expect("sent")
                .iter()
                .filter(|request| request["method"] == "approval.respond")
                .count(),
            baseline,
            "a conflict is never silently resubmitted"
        );
        let requests = sent.lock().expect("sent");
        let list = requests
            .iter()
            .find(|request| request["method"] == "approval.list")
            .expect("approval list refresh");
        assert_eq!(list["params"]["status"], "escalated");
    }

    #[tokio::test]
    async fn next_queued_approval_appears_after_successful_submission() {
        let (client, sent, _held, _replies) = held_approval_client();
        let session_id = SessionId::new_v7();
        let first = approval(session_id);
        let first_id = first.approval_id;
        let mut second = approval(session_id);
        second.approval_id = cookie_agent_protocol::ApprovalId::new_v7();
        let second_id = second.approval_id;
        let mut app = test_app();
        app.client = client;
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                approvals: vec![first, second],
                ..SessionState::default()
            },
        );

        app.answer_approval(ApprovalUserDecision::ApproveOnce).await;
        assert!(
            app.current_approval().is_none(),
            "all approval IDs stay hidden during the global flight"
        );
        app.answer_approval(ApprovalUserDecision::Reject).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sent.lock()
                .expect("sent")
                .iter()
                .filter(|request| request["method"] == "approval.respond")
                .count(),
            1,
            "the queued approval cannot overlap the first request"
        );
        app.handle_rpc_update(RpcUpdate::ApprovalResponse {
            request_id: 1,
            approval_id: first_id,
            result: Ok(()),
        });
        assert!(app.pending_approval.is_none());
        assert_eq!(
            app.current_approval().map(|approval| approval.approval_id),
            Some(second_id)
        );
    }

    #[tokio::test]
    async fn durable_terminal_expiry_revision_and_replay_updates_never_resurrect() {
        enum ProjectionChange {
            Cancelled,
            Finalized,
            Expired,
            RevisionChanged,
            FingerprintChanged,
            InternalPending,
            Replayed,
            Refreshed,
        }

        for change in [
            ProjectionChange::Cancelled,
            ProjectionChange::Finalized,
            ProjectionChange::Expired,
            ProjectionChange::RevisionChanged,
            ProjectionChange::FingerprintChanged,
            ProjectionChange::InternalPending,
            ProjectionChange::Replayed,
            ProjectionChange::Refreshed,
        ] {
            let (client, _sent, _held, _replies) = held_approval_client();
            let (mut app, session_id, approval_id) = app_with_approval(client);
            app.answer_approval(ApprovalUserDecision::ApproveOnce).await;
            let captured_fingerprint = app
                .pending_approval
                .as_ref()
                .expect("submission in flight")
                .approval
                .operation_fingerprint
                .clone();
            match change {
                ProjectionChange::Cancelled => {
                    assert!(app.store.apply_event(EventEnvelope {
                        schema_version: EventSchemaVersion::current(),
                        session_id,
                        run_id: None,
                        seq: 1,
                        timestamp: Timestamp::now(),
                        event: Event::ApprovalCancelled {
                            approval_id,
                            reason_code: ApprovalReasonCode::RequestCancelled,
                        },
                    }));
                }
                ProjectionChange::Finalized => {
                    assert!(app.store.apply_event(EventEnvelope {
                        schema_version: EventSchemaVersion::current(),
                        session_id,
                        run_id: None,
                        seq: 1,
                        timestamp: Timestamp::now(),
                        event: Event::ApprovalFinalized {
                            approval_id,
                            decision: ApprovalFinalDecision {
                                outcome: ApprovalFinalOutcome::Rejected,
                                source: ApprovalDecisionSource::User,
                                reason_code: ApprovalReasonCode::UserRejected,
                                feedback: None,
                                tree_grant_id: None,
                            },
                        },
                    }));
                }
                ProjectionChange::Expired => {
                    app.store
                        .sessions
                        .get_mut(&session_id)
                        .expect("session")
                        .approvals[0]
                        .constraints
                        .expires_at = Some(Timestamp::now());
                }
                ProjectionChange::RevisionChanged => {
                    app.store
                        .sessions
                        .get_mut(&session_id)
                        .expect("session")
                        .approvals[0]
                        .request_revision += 1;
                }
                ProjectionChange::FingerprintChanged => {
                    let mut revised = filesystem_approval(session_id);
                    revised.approval_id = approval_id;
                    revised.request_revision = 9;
                    app.store
                        .sessions
                        .get_mut(&session_id)
                        .expect("session")
                        .approvals = vec![revised];
                }
                ProjectionChange::InternalPending => {
                    app.store
                        .sessions
                        .get_mut(&session_id)
                        .expect("session")
                        .approvals[0]
                        .escalated = false;
                }
                ProjectionChange::Replayed => {
                    assert!(app.store.rebuild_session(session_id, 1, Vec::new()));
                }
                ProjectionChange::Refreshed => app.apply_approval_list(
                    session_id,
                    cookie_agent_protocol::ApprovalListResult {
                        approvals: Vec::new(),
                        tree_grants: Vec::new(),
                    },
                ),
            }
            app.reconcile_pending_approval();
            assert!(app.pending_approval.is_none());
            app.handle_rpc_update(RpcUpdate::ApprovalResponse {
                request_id: 1,
                approval_id,
                result: Err(ApprovalSubmissionError {
                    message: "delayed transport failure".into(),
                    code: None,
                }),
            });
            assert!(
                app.store.sessions[&session_id]
                    .approvals
                    .iter()
                    .all(|approval| {
                        approval.approval_id != approval_id
                            || approval.request_revision != 9
                            || approval.operation_fingerprint != captured_fingerprint
                    }),
                "captured approval must never be resurrected"
            );
        }
    }

    #[tokio::test]
    async fn stale_callback_after_session_switch_and_new_request_is_ignored() {
        let (client, _sent, _held, _replies) = held_approval_client();
        let (mut app, first_session, first_id) = app_with_approval(client);
        app.answer_approval(ApprovalUserDecision::ApproveOnce).await;
        app.store
            .sessions
            .get_mut(&first_session)
            .expect("first session")
            .approvals
            .clear();
        app.reconcile_pending_approval();

        let second_session = SessionId::new_v7();
        let mut second = approval(second_session);
        second.approval_id = ApprovalId::new_v7();
        let second_id = second.approval_id;
        app.store.sessions.insert(
            second_session,
            SessionState {
                approvals: vec![second],
                ..SessionState::default()
            },
        );
        app.set_selected_session(second_session);
        app.answer_approval(ApprovalUserDecision::Reject).await;
        assert_eq!(
            app.pending_approval
                .as_ref()
                .map(|pending| pending.request_id),
            Some(2)
        );
        let status = app.status.clone();

        app.handle_rpc_update(RpcUpdate::ApprovalResponse {
            request_id: 1,
            approval_id: first_id,
            result: Ok(()),
        });
        assert_eq!(
            app.pending_approval
                .as_ref()
                .map(|pending| pending.approval.approval_id),
            Some(second_id)
        );
        assert_eq!(app.status, status, "stale callback cannot overwrite status");
    }

    #[tokio::test]
    async fn optimistic_approval_dismissal_renders_cleanly_tiny_and_no_color() {
        let (client, _sent, _held, _replies) = held_approval_client();
        let (mut app, _session_id, _approval_id) = app_with_approval(client);
        app.answer_approval(ApprovalUserDecision::ApproveOnce).await;
        for theme in [
            Theme::default(),
            Theme::new(
                crate::theme::ThemeKind::Mono,
                crate::theme::ColorLevel::None,
            ),
        ] {
            app.theme = theme;
            for (width, height) in [(80, 24), (16, 10), (4, 3)] {
                let rendered = rendered_frame(&mut app, width, height);
                assert!(
                    !rendered.contains("PERMISSION REQUIRED"),
                    "modal gone at {width}x{height}"
                );
            }
        }
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Approval submitting"));
    }

    #[cfg(test)]
    fn transcript_layout_with_selection(
        state: &SessionState,
        expanded: Option<&HashSet<BlockId>>,
        selected_block: Option<BlockId>,
        width: u16,
        theme: &Theme,
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
                    selected_block,
                    width,
                    theme,
                    highlighter: &PlainHighlighter,
                    minimum_event_level: crate::state::EventLevel::Debug,
                    assistant_part_cache: &mut assistant_parts,
                    assistant_part_layout_passes: &mut assistant_part_layout_passes,
                },
            );
            layout.lines.extend(item_layout.lines);
            layout.regions.extend(item_layout.regions);
        }
        layout
    }
}

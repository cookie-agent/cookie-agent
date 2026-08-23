//! Transcript layout, collapse state, wrapping, scrolling, and hit testing.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

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

use super::app::{App, TextSelection, UserMessageHit};

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
    pub(super) fn resolve(track: Rect, content_height: usize) -> Option<Self> {
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

    pub(super) fn with_thumb(mut self, offset: usize) -> Self {
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
pub struct ConversationScroll {
    pub(super) offset: usize,
    pub(super) following: bool,
}

impl Default for ConversationScroll {
    fn default() -> Self {
        let mut scroll = Self {
            offset: 0,
            following: false,
        };
        scroll.bottom();
        scroll
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
        let target = self.offset.saturating_sub(lines);
        self.reveal(
            BlockRegion {
                id: BlockId::Thinking(0),
                start_line: target,
                end_line: target,
            },
            1,
        );
    }
    pub(super) fn down(&mut self, lines: usize) {
        self.following = false;
        self.offset = self.offset.saturating_add(lines);
    }
    pub fn top(&mut self) {
        self.following = false;
        self.offset = 0;
    }
    pub fn bottom(&mut self) {
        self.following = true;
    }

    /// Absolute top offset from a scrollbar thumb/track gesture.
    pub(super) fn scroll_to(&mut self, offset: usize) {
        if offset == 0 {
            self.top();
            return;
        }
        self.following = false;
        self.offset = offset;
    }

    pub fn reveal(&mut self, region: BlockRegion, viewport_height: u16) {
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
    CommittedTool { turn_seq: u64, content_index: u32 },
}

/// A contiguous logical-line range owned by one collapsible transcript block.
/// Stage 4 mouse handling can translate a y coordinate to a logical line by
/// adding the conversation scroll offset, then find the containing region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockRegion {
    pub(super) id: BlockId,
    pub(super) start_line: usize,
    pub(super) end_line: usize,
}

/// The logical-line range of one user message row, paired with the physical
/// sequence of its `UserInputSubmitted` event. Clicking the range opens the
/// copy/revert/fork menu, which targets the sequence with `through_seq`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct UserRegion {
    pub(super) seq: u64,
    pub(super) start_line: usize,
    pub(super) end_line: usize,
}

/// Width-resolved transcript output and its stage-4 block hit map.
#[derive(Clone, Default)]
pub(super) struct TranscriptLayout {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) regions: Vec<BlockRegion>,
    pub(super) user_regions: Vec<UserRegion>,
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
    interaction: Vec<(BlockId, bool)>,
    /// Animation bucket for items with a streaming thinking part (0 otherwise),
    /// so the "thinking…" ellipsis advances without a transcript mutation.
    clock: u8,
}

#[derive(Clone, Default)]
struct ItemLayout {
    lines: Vec<Line<'static>>,
    regions: Vec<BlockRegion>,
    /// Physical event sequence when this item is a user message row.
    user_seq: Option<u64>,
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
    streaming: bool,
    /// Animated ellipsis bucket while thinking streams (0 when settled).
    dots: u8,
    /// Sealed thinking duration shown as "thought for Ns" when known.
    duration: Option<Duration>,
}

#[derive(Clone)]
struct CachedAssistantPartLayout {
    key: AssistantPartLayoutKey,
    layout: ItemLayout,
}

struct TranscriptRenderContext<'a> {
    expanded: Option<&'a HashSet<BlockId>>,
    width: u16,
    theme: &'a Theme,
    highlighter: &'a dyn Highlighter,
    minimum_event_level: crate::state::EventLevel,
    /// Animation bucket (0–3) driving the streaming "thinking…" ellipsis.
    clock_bucket: u8,
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
    width: u16,
    theme: &Theme,
    highlighter: &dyn Highlighter,
    minimum_event_level: crate::state::EventLevel,
    clock_bucket: u8,
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
            interaction: item_interaction(item, expanded),
            clock: if item_is_live(state, item) {
                clock_bucket
            } else {
                0
            },
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
                    width,
                    theme,
                    highlighter,
                    minimum_event_level,
                    clock_bucket,
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
        append_item_layout(&mut assembled, layout);
    }
    cache.items.truncate(state.transcript.len());
    cache.layout = assembled;
    all_cached
}

impl App {
    /// The transient notice rows rendered after the transcript (notices and
    /// aggregated descendant warnings), exactly as [`Self::render_conversation`]
    /// appends them. Selection extraction consumes the same chain so copied
    /// text matches what is on screen.
    pub(super) fn notice_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut notice_lines = Vec::new();
        for notice in &self.transient_notices {
            // Multiline notices (e.g. /help) keep their structure: the
            // NOTICE badge leads, continuation lines align beneath it.
            let lines = notice
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    if index == 0 {
                        Line::from(format!("NOTICE: {line}"))
                    } else {
                        Line::from(format!("        {line}"))
                    }
                })
                .collect::<Vec<_>>();
            notice_lines.extend(role_block(Role::Internal, lines, width, &self.theme));
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
        notice_lines
    }

    /// The full rendered conversation chain — transcript lines plus the
    /// notice block — for selection extraction. The cached layout is exactly
    /// what the last frame rendered at this width, so logical-line
    /// coordinates from the mouse map one-to-one.
    pub(super) fn conversation_chain(&self, width: u16) -> Vec<Line<'static>> {
        let session_present = self
            .selected
            .is_some_and(|session_id| self.store.sessions.contains_key(&session_id));
        let transcript_empty = self
            .selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .is_none_or(|state| state.transcript.is_empty());
        let mut lines = if session_present && !transcript_empty {
            self.layout_cache.layout.lines.clone()
        } else {
            empty_conversation_lines(session_present, width, &self.theme)
        };
        let mut notices = self.notice_lines(width);
        if !lines.is_empty() && !notices.is_empty() {
            notices.insert(0, Line::default());
        }
        lines.extend(notices);
        lines
    }

    /// The currently selected text, mapped from content coordinates back to
    /// real text: chrome (gutters, bands, box-drawing headers) is stripped,
    /// code copies raw, and the composer leg slices the draft buffer.
    pub(super) fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        Some(match selection {
            TextSelection::Conversation { .. } => {
                let (start, end) = selection.ordered();
                let width = self
                    .hit_map
                    .conversation
                    .map_or(0, |viewport| viewport.width);
                let lines = self.conversation_chain(width);
                extract_selection(&lines, start, end, &self.theme)
            }
            TextSelection::Composer { .. } => {
                let (start, end) = selection.byte_range();
                self.input
                    .as_str()
                    .get(start..end)
                    .unwrap_or_default()
                    .to_owned()
            }
        })
    }

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
        let session_present = self
            .selected
            .is_some_and(|session_id| self.store.sessions.contains_key(&session_id));
        let empty_layout = TranscriptLayout {
            lines: empty_conversation_lines(session_present, width, &self.theme),
            regions: Vec::new(),
            user_regions: Vec::new(),
        };
        let clock_bucket = self.clock_bucket();
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
                width,
                &self.theme,
                self.highlighter.as_ref(),
                self.tui_config.minimum_event_level,
                clock_bucket,
            );
            // A fresh session greets with guidance instead of a blank pane;
            // a filtered-down transcript (lines hidden by the event level)
            // keeps its own rows, empty-looking or not.
            if state.transcript.is_empty() {
                &empty_layout
            } else {
                &self.layout_cache.layout
            }
        } else {
            &empty_layout
        };
        let mut notice_lines = self.notice_lines(width);
        // Notices follow the same rhythm as transcript items: one blank row
        // between real content and the first notice block.
        if !layout.lines.is_empty() && !notice_lines.is_empty() {
            notice_lines.insert(0, Line::default());
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
        self.conversation_scroll
            .clamp(content_height, viewport.height);
        self.hit_map.conversation = Some(viewport);
        self.hit_map.scrollbar = scrollbar_track.filter(|track| track.width > 0);
        self.hit_map.blocks = layout
            .regions
            .iter()
            .filter_map(|region| block_hit(*region, viewport, self.conversation_scroll.offset))
            .collect();
        self.hit_map.user_messages = layout
            .user_regions
            .iter()
            .filter_map(|region| {
                user_message_hit(*region, viewport, self.conversation_scroll.offset)
            })
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
        let title_spans = vec![
            Span::raw("Conversation · "),
            Span::styled(format!("events ≥ {filter}"), self.theme.link()),
        ];
        let filter_span = 1;
        let title_area = Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(2),
            u16::from(area.height > 0),
        );
        self.hit_map.event_level_filter = {
            let mut column = title_area.x;
            title_spans.iter().enumerate().find_map(|(index, span)| {
                let width =
                    UnicodeWidthStr::width(span.content.as_ref()).min(usize::from(u16::MAX)) as u16;
                let hit = (index == filter_span).then(|| {
                    let visible = title_area
                        .x
                        .saturating_add(title_area.width)
                        .saturating_sub(column)
                        .min(width);
                    (visible > 0).then(|| Rect::new(column, title_area.y, visible, 1))
                });
                column = column.saturating_add(width);
                hit.flatten()
            })
        };
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(self.theme.panel_border())
            .title(Line::from(title_spans));
        // A viewport that no longer follows live output says so loudly, in
        // the title row, with the truthful way back — never buried in the
        // muted status line.
        if !self.conversation_scroll.following {
            block = block.title(
                Line::from(Span::styled(
                    "↑ scrolled · PgDn: bottom",
                    self.theme.warning(),
                ))
                .right_aligned(),
            );
        }
        frame.render_widget(Paragraph::new(Text::from(visible_lines)).block(block), area);
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
}

/// Render the reserved scrollbar column: a subdued track with a distinct
/// thumb covering the exact visible fraction of the total rendered height.
/// Shared by the conversation pane and the overflowed message composer.
pub(super) fn render_scrollbar_track(
    frame: &mut ratatui::Frame,
    geometry: ScrollbarGeometry,
    theme: &Theme,
) {
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
                width,
                theme,
                highlighter,
                minimum_event_level,
                clock_bucket: 0,
                assistant_part_cache: &mut assistant_parts,
                assistant_part_layout_passes: &mut assistant_part_layout_passes,
            },
        );
        append_item_layout(&mut layout, item_layout);
    }
    layout
}

/// One blank row of breathing room between top-level transcript items, so
/// messages never butt against each other. Items that render nothing
/// (event rows below the level filter) contribute no lines and no spacer —
/// never a leading, trailing, or doubled blank row.
fn append_item_layout(assembled: &mut TranscriptLayout, item_layout: ItemLayout) {
    if item_layout.lines.is_empty() {
        return;
    }
    if !assembled.lines.is_empty() {
        assembled.lines.push(Line::default());
    }
    let start_line = assembled.lines.len();
    let end_line = start_line + item_layout.lines.len();
    assembled.lines.extend(item_layout.lines);
    for region in item_layout.regions {
        assembled.regions.push(BlockRegion {
            id: region.id,
            start_line: start_line + region.start_line,
            end_line: start_line + region.end_line,
        });
    }
    if let Some(seq) = item_layout.user_seq {
        assembled.user_regions.push(UserRegion {
            seq,
            start_line,
            end_line,
        });
    }
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
                AssistantChild::Text { .. }
                | AssistantChild::Attribution { .. }
                | AssistantChild::CommittedTool { .. } => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn item_interaction(
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
fn item_is_live(state: &SessionState, item: &TranscriptItem) -> bool {
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
                | AssistantChild::CommittedTool { .. } => false,
            })
        }
        TranscriptItem::User { .. } | TranscriptItem::Event { .. } => false,
    }
}

fn transcript_item_layout(
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
                    vec![Line::from(text.clone())],
                    context.width,
                    context.theme,
                ),
                regions: Vec::new(),
                user_seq: None,
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
        user_seq: None,
    };
    for child in children {
        match child {
            AssistantChild::Text { .. } | AssistantChild::Thinking { .. } => {
                let block_id = match child {
                    AssistantChild::Thinking { id, .. } => Some(BlockId::Thinking(*id)),
                    AssistantChild::Text { .. } => None,
                    AssistantChild::Tool { .. }
                    | AssistantChild::Attribution { .. }
                    | AssistantChild::CommittedTool { .. } => {
                        unreachable!()
                    }
                };
                let streaming = matches!(child, AssistantChild::Thinking { id, .. } if state.is_open_thinking(item_id, *id));
                let duration = match child {
                    AssistantChild::Thinking { id, .. } if !streaming => state
                        .thinking_duration(item_id, *id)
                        .filter(|duration| duration.as_secs() >= 1),
                    _ => None,
                };
                let key = AssistantPartLayoutKey {
                    version: child.version(),
                    expanded: block_id.is_some_and(|id| {
                        context.expanded.is_some_and(|blocks| blocks.contains(&id))
                    }),
                    streaming,
                    dots: if streaming { context.clock_bucket } else { 0 },
                    duration,
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
            AssistantChild::Attribution { resolved_model } => {
                layout.lines.extend(attribution_line(
                    resolved_model,
                    context.width,
                    context.theme,
                ));
            }
            AssistantChild::CommittedTool {
                turn_seq,
                content_index,
            } => {
                let child_layout = tool_child_layout(
                    state,
                    None,
                    BlockKey::CommittedTool {
                        turn_seq: *turn_seq,
                        content_index: *content_index,
                    },
                    context,
                );
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
    // The block footer closes the run: one muted, gutter-aligned row with
    // generation speed and context use, from committed-turn usage and
    // durable event timestamps. Passive — no region, no hover — and absent
    // entirely when the data is missing.
    if let Some(footer) = assistant_footer_line(state, item_id, context.width, context.theme) {
        layout.lines.extend(footer);
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
            user_seq: None,
        },
        AssistantChild::Thinking { id, text, .. } => {
            let block_id = BlockId::Thinking(*id);
            let body = thinking_body_lines(text, width, theme);
            let hidden_lines = body.len().max(1);
            // While thinking streams the header animates an ellipsis; once
            // sealed it reads "thought", with the durable elapsed time when
            // the projection recorded one. Exactly one chevron per thinking
            // row: `▸` collapsed, `▾` expanded, after the thinking emoji.
            let status = if key.streaming {
                format!("thinking{}", ".".repeat(usize::from(key.dots)))
            } else if let Some(duration) = key.duration {
                format!("thought for {}", format_thinking_duration(duration))
            } else {
                "thought".to_owned()
            };
            let label = if key.expanded {
                format!("💭 ▾ {status}")
            } else {
                format!("💭 ▸ {status} ({hidden_lines} lines hidden)")
            };
            let mut lines = assistant_body_line(
                Line::from(Span::styled(label, theme.thinking())),
                width,
                theme,
            );
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
                user_seq: None,
            }
        }
        AssistantChild::Tool { .. }
        | AssistantChild::Attribution { .. }
        | AssistantChild::CommittedTool { .. } => {
            unreachable!("tool children use tool_child_layout")
        }
    }
}

/// A compact or expanded tool row inside its owning assistant item. Compact
/// rows render the persisted sanitized title and display argument: running
/// pulses a `…`/dot suffix with the animation clock, success adds no
/// suffix, and failed/cancelled/interrupted use their exact concise
/// markers. `COMPLETED` is never rendered. Exactly one chevron per row,
/// after the tool emoji.
fn tool_child_layout(
    state: &SessionState,
    call_id: Option<cookie_agent_protocol::ToolCallId>,
    block_key: impl Into<BlockKey>,
    context: &mut TranscriptRenderContext<'_>,
) -> ItemLayout {
    let block_id = match block_key.into() {
        BlockKey::Call(call) => BlockId::Tool(call),
        BlockKey::CommittedTool {
            turn_seq,
            content_index,
        } => BlockId::CommittedTool {
            turn_seq,
            content_index,
        },
    };
    let is_expanded = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&block_id));
    let tool = call_id.and_then(|call_id| state.tools.get(&call_id));
    let Some(tool) = tool else {
        let lines = role_block(
            Role::Error,
            vec![Line::from("tool: unavailable payload".to_owned())],
            context.width,
            context.theme,
        );
        return ItemLayout {
            regions: vec![BlockRegion {
                id: block_id,
                start_line: 0,
                end_line: lines.len(),
            }],
            lines,
            user_seq: None,
        };
    };
    let (suffix, role) = match tool.status {
        // The running marker breathes with the animation clock: a resting
        // ellipsis, then growing dots. Subtle liveness, never busy.
        ToolStatus::Running => (
            match context.clock_bucket {
                0 => " …".to_owned(),
                dots => format!(" {}", ".".repeat(usize::from(dots))),
            },
            Role::ToolRunning,
        ),
        ToolStatus::Completed => (String::new(), Role::ToolSuccess),
        ToolStatus::Failed => (" failed".to_owned(), Role::ToolFailure),
        ToolStatus::Cancelled => (" cancelled".to_owned(), Role::ToolFailure),
        ToolStatus::Interrupted => (" interrupted".to_owned(), Role::ToolFailure),
    };
    let title = tool.compact_title();
    let mut body = if is_expanded {
        vec![
            Line::from(format!("🔨 ▾ {title}{suffix}")),
            Line::from(format!("arguments: {}", tool.arguments)),
        ]
    } else {
        vec![Line::from(format!("🔨 ▸ {title}{suffix}"))]
    };
    if is_expanded {
        if !tool.detail.is_empty() {
            body.extend(tool_body_lines(tool, context));
        }
        if !tool.has_output_chunks
            && let Some(call_id) = call_id
        {
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
    let lines = tool_block_lines(role, body, context.width, context.theme);
    ItemLayout {
        regions: vec![BlockRegion {
            id: block_id,
            start_line: 0,
            end_line: lines.len(),
        }],
        lines,
        user_seq: None,
    }
}

/// Identity for a tool row: a started call or a committed placeholder index.
enum BlockKey {
    Call(cookie_agent_protocol::ToolCallId),
    CommittedTool { turn_seq: u64, content_index: u32 },
}

impl From<cookie_agent_protocol::ToolCallId> for BlockKey {
    fn from(call_id: cookie_agent_protocol::ToolCallId) -> Self {
        Self::Call(call_id)
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

/// A settled thinking duration as compact text: seconds under a minute,
/// then minutes and seconds. Sub-second spans never reach the label (they
/// are filtered to plain "thought" by the caller).
fn format_thinking_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// Warm, actionable empty states: what the pane says before there is
/// anything to show. The no-session variant points at session commands; the
/// fresh-session variant invites the first message. Both stay muted so the
/// guidance never competes with real content, and both wrap to the pane.
fn empty_conversation_lines(has_session: bool, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let (headline, hint) = if has_session {
        (
            "🍪 Fresh session, warm out of the oven.",
            "Type a message below to start · `ctrl+p` lists commands · `/help` shows help",
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

fn attribution_line(
    resolved_model: &cookie_agent_protocol::ResolvedModelRef,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let variant = resolved_model
        .selection
        .variant
        .as_ref()
        .map_or_else(|| "base".to_owned(), ToString::to_string);
    let prefix = (width >= 4).then(|| vec![Span::styled("├─ ", theme.muted())]);
    repeated_prefixed_wrapped_line(
        prefix.unwrap_or_default(),
        Line::from(Span::styled(
            format!("now using {}[{variant}]", resolved_model.selection.model),
            theme.muted(),
        )),
        width,
    )
}

/// The assistant block's closing footer:
/// `╰─ ⚡ 42.1 tps · 12.5K ctx · $0.0040` in
/// muted styling — visually subordinate to the body, closing the block's
/// gutter tree. The rate is committed output tokens over generation wall
/// time measured between durable event timestamps, so a replayed log yields
/// the identical row; the ctx is the total context the turn left behind
/// (`input_tokens + output_tokens`). `None` unless every input is present:
/// at least one turn with a positive generation span and a known
/// end-of-turn context total.
fn assistant_footer_line(
    state: &SessionState,
    item_id: u64,
    width: u16,
    theme: &Theme,
) -> Option<Vec<Line<'static>>> {
    let metrics = state.assistant_metrics.get(&item_id)?;
    let context_tokens = metrics.context_tokens?;
    if metrics.timed_output_tokens == 0 || metrics.generation.is_zero() {
        return None;
    }
    let tps = metrics.timed_output_tokens as f64 / metrics.generation.as_secs_f64();
    let cost = metrics
        .estimated_cost_pico_usd
        .map(|cost| super::app::format_cost_usd(cost as f64 / 1_000_000_000_000.0));
    let cost = cost.map_or_else(String::new, |cost| format!(" · {cost}"));
    let prefix = (width >= 4).then(|| vec![Span::styled("╰─ ", theme.muted())]);
    Some(repeated_prefixed_wrapped_line(
        prefix.unwrap_or_default(),
        Line::from(Span::styled(
            format!(
                "⚡ {tps:.1} tps · {} ctx{cost}",
                super::app::format_token_count(context_tokens),
            ),
            theme.muted(),
        )),
        width,
    ))
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
    role_block_lines(role, body, width, theme)
}

/// Tool children render inside the assistant item without a standalone
/// `TOOL` header: the compact/expanded rows keep the assistant gutter and
/// take only their status style.
fn tool_block_lines(
    role: Role,
    body: Vec<Line<'static>>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let style = match role {
        Role::ToolRunning => theme.tool_running(),
        Role::ToolSuccess => theme.tool_success(),
        Role::ToolFailure => theme.tool_failure(),
        _ => theme.tool(),
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
        let spans = line
            .spans
            .into_iter()
            .map(|mut span| {
                span.style = style.patch(span.style);
                span
            })
            .collect::<Vec<_>>();
        lines.extend(repeated_prefixed_wrapped_line(
            vec![Span::styled("│ ", theme.assistant())],
            Line::from(spans),
            width,
        ));
    }
    lines
}

fn role_block_lines(
    role: Role,
    body: Vec<Line<'static>>,
    width: u16,
    theme: &Theme,
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

/// The user-message analogue of [`block_hit`]: clip a message's logical-line
/// range to the visible window so its rows open the copy/revert/fork menu.
pub(super) fn user_message_hit(
    region: UserRegion,
    viewport: Rect,
    scroll_offset: usize,
) -> Option<UserMessageHit> {
    let viewport_end = scroll_offset.saturating_add(usize::from(viewport.height));
    let start = region.start_line.max(scroll_offset);
    let end = region.end_line.min(viewport_end);
    (start < end).then(|| UserMessageHit {
        rect: Rect::new(
            viewport.x,
            viewport.y + u16::try_from(start - scroll_offset).unwrap_or(u16::MAX),
            viewport.width,
            u16::try_from(end - start).unwrap_or(u16::MAX),
        ),
        seq: region.seq,
    })
}

/// Span contents that are pure row chrome (gutters and quote bars) in any
/// leading position. [`FIRST_SPAN_GUTTERS`] additionally holds wrap
/// continuations and narrow-mode tags, which are chrome only in span
/// position 0: a two-space span after a real gutter is code indentation,
/// never a wrap continuation. Text extraction skips exactly these spans,
/// so copied text is the raw content with no band or glyph chrome.
const GUTTER_SPANS: &[&str] = &["│ ", "┆ ", "┃ ", "· ", "! ", "> "];
const FIRST_SPAN_GUTTERS: &[&str] = &[
    "  ",
    "    ",
    "[U] ",
    "[T\u{2026}] ",
    "[T\u{2713}] ",
    "[T!] ",
    "[T] ",
    "[D] ",
    "[W] ",
    "[E] ",
    "[I] ",
];

/// First characters of gutterless header/border/footer rows (role headers,
/// assistant attribution and footer, code fences, table grids). Such rows
/// are chrome-only: they vanish from an extraction rather than leaking
/// border glyphs into copied text.
const CHROME_ROW_PREFIXES: &[&str] = &["┌", "└", "┏", "╭", "╰", "├", "··", "!!", "⚠", "--"];

/// Extract the copyable text of one rendered line inside the display-column
/// window `[col_start, col_end)`: gutter spans are stripped, chrome-only
/// rows yield `None`, and the remaining text is cut on grapheme boundaries.
/// `col_end` beyond the line width selects to the line end; trailing
/// padding is trimmed.
///
/// A row is chrome-only when it is gutterless (or only quote-barred) and
/// starts with a header/border glyph — role headers, attribution, footers —
/// or when every remaining span carries the code/table border signature:
/// fence headers and table grids vanish even inside a role gutter, while
/// code content (syntax-styled, even when it starts with a box glyph)
/// stays. The signature is the border's foreground *and* modifier set,
/// compared exactly: the parchment band only ever patches backgrounds, and
/// in high contrast a quantized plain-code foreground equals the border's
/// white, so only the border's DIM|BOLD set tells a chrome row apart from
/// content there (syntect never emits DIM).
fn extract_line(
    line: &Line<'static>,
    col_start: u16,
    col_end: u16,
    theme: &Theme,
) -> Option<String> {
    if col_start >= col_end {
        return None;
    }
    let border_style = theme.code_border();
    let mut span_index = 0usize;
    let mut spans = line.spans.iter().peekable();
    let mut gutter_width = 0u16;
    // Quoted content rows keep only "> " gutters; a border row inside a
    // quote ("> ┌──┬──") is therefore still recognized as chrome.
    let mut only_quote_gutters = true;
    while let Some(span) = spans.peek() {
        let content = span.content.as_ref();
        let is_gutter = GUTTER_SPANS.contains(&content)
            || (span_index == 0 && FIRST_SPAN_GUTTERS.contains(&content));
        if !is_gutter {
            break;
        }
        gutter_width = gutter_width.saturating_add(UnicodeWidthStr::width(content) as u16);
        if content != "> " {
            only_quote_gutters = false;
        }
        span_index += 1;
        spans.next();
    }
    let remaining: Vec<&ratatui::text::Span<'static>> = spans.collect();
    let rest: String = remaining.iter().map(|span| span.content.as_ref()).collect();
    if (gutter_width == 0 || only_quote_gutters)
        && CHROME_ROW_PREFIXES
            .iter()
            .any(|prefix| rest.starts_with(prefix))
    {
        return None;
    }
    if !rest.is_empty()
        && border_style.fg.is_some()
        && remaining.iter().all(|span| {
            span.style.fg == border_style.fg && span.style.add_modifier == border_style.add_modifier
        })
    {
        return None;
    }
    // The window shifts into content coordinates: cells left of the gutter
    // hold no copyable text.
    let start = col_start.saturating_sub(gutter_width);
    let end = col_end.saturating_sub(gutter_width);
    let mut extracted = String::new();
    let mut column = 0u16;
    for grapheme in rest.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme).max(1) as u16;
        let next = column.saturating_add(width);
        if next > start && column < end {
            extracted.push_str(grapheme);
        }
        column = next;
    }
    Some(extracted.trim_end().to_owned())
}

/// Extract a normalized multi-line selection (start before end, both
/// `(logical line, display column)`) from the rendered conversation lines.
/// Chrome-only rows vanish; blank rows inside the range stay as paragraph
/// breaks; leading/trailing blank rows are dropped.
pub(super) fn extract_selection(
    lines: &[Line<'static>],
    start: (usize, u16),
    end: (usize, u16),
    theme: &Theme,
) -> String {
    if start.0 >= lines.len() || start >= end {
        return String::new();
    }
    let last = end.0.min(lines.len() - 1);
    let mut extracted = Vec::new();
    for (index, line) in lines.iter().enumerate().take(last + 1).skip(start.0) {
        let col_start = if index == start.0 { start.1 } else { 0 };
        let col_end = if index == end.0 { end.1 } else { u16::MAX };
        if let Some(text) = extract_line(line, col_start, col_end, theme) {
            extracted.push(text);
        }
    }
    while extracted.first().is_some_and(String::is_empty) {
        extracted.remove(0);
    }
    while extracted.last().is_some_and(String::is_empty) {
        extracted.pop();
    }
    extracted.join("\n")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use cookie_agent_config::{
        ApprovalConfig, ContextCompactionConfig, LoadedConfiguration, RuntimeConfig, ServerConfig,
        SessionTitleConfig, ToolOutputConfig,
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
        ApprovalTrigger, ApprovalUserDecision, AssistantToolCallRef, AttemptId, DecisionTrace,
        EventPayload, EventSubscriptionMessage, ModelCallId, ModelKey, ModelSelection,
        OperationFingerprint, OutputDelta, OutputStream, PermissionAction, PermissionEffect,
        PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
        PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity, ProviderId,
        RunId, RunSelection, SafeCode, SafeDisplayText, SafeErrorMessage, SessionId, SessionMeta,
        SessionOrigin, SessionStatus, SessionTitle, SessionTree, Sha256Digest, StoredEvent,
        ToolCallId, ToolCallStart, Usage,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use jiff::Timestamp;
    use ratatui::{Terminal, backend::TestBackend, style::Modifier, text::Line};

    use crate::Client;
    use crate::client::ClientDelivery;
    use crate::markdown::{MarkdownDocument, PlainHighlighter};
    use crate::state::{
        ApprovalState, AssistantChild, FrozenAssistantAttribution, SessionState, StateStore,
        ToolCallState,
    };
    use crate::theme::{ColorLevel, ThemeKind};
    use crate::ui::app::*;
    use crate::ui::events::{RenderScheduler, TerminalCleanup, TerminalRestore};
    use crate::ui::input::credential_wipe_count;
    use crate::ui::pickers::SearchPickerFocus;
    use crate::ui::provider::{ProviderAction, ProviderForm, ProviderFormFocus, ProviderOperation};
    use crate::ui::slash::{
        SlashCommand, Submission, command_help, command_spec, parse_submission,
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
        #[cfg(unix)]
        {
            fs::create_dir(&provider_store_path).expect("provider store directory");
            fs::set_permissions(&provider_store_path, fs::Permissions::from_mode(0o700))
                .expect("private provider store");
        }
        #[cfg(windows)]
        cookie_agent_models::secure_store::SecureDirectory::open(&provider_store_path)
            .expect("private provider store");
        let store = ProviderStore::open(&provider_store_path).expect("provider store");
        prepare_store(&store);
        let manager = Arc::new(
            ModelManager::new(BTreeMap::new(), catalog, store).expect("production model manager"),
        );
        let config = LoadedConfiguration {
            runtime: RuntimeConfig {
                server: ServerConfig::default(),
                tool_output: ToolOutputConfig::default(),
                approval: ApprovalConfig::default(),
                context_compaction: ContextCompactionConfig::default(),
                prompt_caching: cookie_agent_config::PromptCachingConfig::default(),
                session_title: SessionTitleConfig::default(),
                delegation: cookie_agent_config::DelegationConfig::default(),
                pricing: cookie_agent_config::PricingConfig::default(),
                providers: BTreeMap::new(),
            },
            agents: BTreeMap::new(),
            agent_presets: BTreeMap::new(),
            mcp_servers: BTreeMap::new(),
            user_mcp_servers: BTreeMap::new(),
            workspace_mcp_servers: BTreeMap::new(),
            plugins: Default::default(),
            config_paths: cookie_agent_config::ConfigLayerPaths::default(),
            skills: cookie_agent_config::SkillRegistry::default(),
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
            cost: None,
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
            engine_version: None,
            session_id,
            run_id: Some(run),
            seq,
            timestamp: Timestamp::now(),
            payload,
        }
    }

    fn runless_event(session_id: SessionId, seq: u64, payload: EventPayload) -> StoredEvent {
        StoredEvent {
            engine_version: None,
            session_id,
            run_id: None,
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
            preset: None,
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
            engine_version: None,
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
                    max_output_tokens: 0,
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

    // Test fixtures stay explicit about each protocol-9 turn field; grouping them
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

    fn usage_recorded(
        session_id: SessionId,
        seq: u64,
        run: RunId,
        model_turn_seq: u64,
        estimated_cost_pico_usd: Option<u64>,
    ) -> StoredEvent {
        event(
            session_id,
            seq,
            run,
            EventPayload::ModelUsageRecorded {
                model_turn_seq,
                agent_id: agent_id(),
                resolved_model: resolved_model(None),
                usage: Usage::default(),
                estimated_cost_pico_usd,
            },
        )
    }

    #[test]
    fn model_turn_committed_updates_latest_context_tokens() {
        let session = SessionId::new_v7();
        let run = run_id();
        let first_attempt = AttemptId::new_v7();
        let second_attempt = AttemptId::new_v7();
        let with_input_tokens = |mut event: StoredEvent, input_tokens| {
            let EventPayload::ModelTurnCommitted { turn, .. } = &mut event.payload else {
                panic!("expected committed turn");
            };
            turn.usage.input_tokens = Some(input_tokens);
            event
        };
        let events = vec![
            session_created(session, 1),
            attempt_started(session, 2, run, first_attempt, None),
            with_input_tokens(
                turn_committed(
                    session,
                    3,
                    run,
                    first_attempt,
                    1,
                    Vec::new(),
                    Vec::new(),
                    None,
                ),
                1_200,
            ),
            attempt_started(session, 4, run, second_attempt, None),
            with_input_tokens(
                turn_committed(
                    session,
                    5,
                    run,
                    second_attempt,
                    2,
                    Vec::new(),
                    Vec::new(),
                    None,
                ),
                48_200,
            ),
        ];
        let mut store = StateStore::default();
        assert!(store.rebuild_session(session, 1, events));
        // End-of-turn total: 48,200 consumed plus the fixture's 4 generated.
        assert_eq!(store.sessions[&session].context_tokens, Some(48_204));
    }

    fn text_part(text: &str) -> cookie_agent_protocol::PersistedAssistantPart {
        cookie_agent_protocol::PersistedAssistantPart::Text {
            text: text.into(),
            metadata: None,
        }
    }

    fn reasoning_part(text: &str) -> cookie_agent_protocol::PersistedAssistantPart {
        cookie_agent_protocol::PersistedAssistantPart::Reasoning {
            text: text.into(),
            metadata: None,
        }
    }

    fn tool_part(call: &str) -> cookie_agent_protocol::PersistedAssistantPart {
        cookie_agent_protocol::PersistedAssistantPart::ToolCall {
            id: ModelCallId::new(call).expect("call"),
            provider_item_id: None,
            name: SafeCode::new("bash").expect("tool"),
            input: serde_json::json!({"command": call}),
            raw_input: None,
            metadata: None,
        }
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
                vec![PreparedApprovalResource {
                    capability: PermissionAction::Bash,
                    canonical: PreparedResourceIdentity::new("command:test")
                        .expect("resource identity"),
                    binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"test"),
                    binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                    boundary: ApprovalBoundary::Exact,
                    source: ApprovalResourceSource::PrimaryOperation,
                }],
                Sha256Digest::of_bytes(b"context"),
            )
            .expect("prepared operation"),
        )
    }

    // Fixture mirrors the exact protocol-9 ownership/presentation fields; grouping
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
            session_id: id,
            origin: SessionOrigin::Root,
            cwd_identity: cookie_agent_protocol::CwdIdentity::new("/workspace").expect("cwd"),
            creation_selection: RunSelection {
                agent: agent_id(),
                model: ModelSelection {
                    model: model_key(),
                    variant: None,
                },
                preset: None,
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
            // Creation plus one user message: a session with content that
            // renders in the Agents panel. Tests for the empty-session ghost
            // filter set this back to 1 explicitly.
            last_event_seq: 2,
            last_activity: "2026-08-06T12:00:00Z".parse().expect("timestamp"),
            status: SessionStatus::Idle,
            skipped_events: Vec::new(),
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
            preset: None,
            description: format!("Test {agent} agent"),
            mode: cookie_agent_protocol::AgentMode::Primary,
            enabled: runnable,
            runnable_as_root: runnable,
            resolved_fallback: vec![ModelSelection {
                model: model_key(),
                variant: None,
            }],
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
            variant_order: vec![
                cookie_agent_protocol::VariantId::new("fast").expect("variant"),
                cookie_agent_protocol::VariantId::new("high").expect("variant"),
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

    fn multi_auth_provider() -> cookie_agent_protocol::ProviderDescriptor {
        let mut value = serde_json::to_value(provider_descriptor(
            "multi-auth",
            "supported",
            "current",
            false,
        ))
        .expect("serialize provider");
        value["auth_methods"] = serde_json::json!([
            {
                "id": "api-key",
                "display_name": "API key",
                "credentials": [{
                    "id": "api_key",
                    "display_name": "API key",
                    "help": "Secret API credential",
                    "required": true,
                    "credential_type": "api_key"
                }]
            },
            {
                "id": "bearer",
                "display_name": "Bearer token",
                "credentials": [{
                    "id": "access_token",
                    "display_name": "Access token",
                    "help": "Secret bearer credential",
                    "required": true,
                    "credential_type": "access_token"
                }]
            }
        ]);
        value["setup_fields"] = serde_json::json!([
            {
                "id": "region",
                "display_name": "Region",
                "help": "Public service region",
                "required": true,
                "default": null,
                "validation": {"value_type": "string", "min_length": 1, "max_length": 32, "minimum": null, "maximum": null},
                "safe_to_project": true
            },
            {
                "id": "service_token",
                "display_name": "Service token",
                "help": "Derived secret setup placeholder",
                "required": true,
                "default": null,
                "validation": {"value_type": "string", "min_length": 1, "max_length": 64, "minimum": null, "maximum": null},
                "safe_to_project": false
            }
        ]);
        serde_json::from_value(value).expect("multi-auth provider")
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
        descriptor
            .variants
            .sort_by(|left, right| left.id.cmp(&right.id));
        descriptor.variant_order = variants
            .iter()
            .map(|id| cookie_agent_protocol::VariantId::new(*id).expect("variant"))
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

    fn assistant_projection(state: &SessionState) -> Vec<(String, Option<u64>, Vec<String>)> {
        state
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Assistant {
                    attribution,
                    committed_turn_seq,
                    children,
                    ..
                } => Some((
                    attribution.header(),
                    *committed_turn_seq,
                    children
                        .iter()
                        .map(|child| match child {
                            AssistantChild::Text { markdown, .. } => {
                                format!("text:{}", markdown.as_str())
                            }
                            AssistantChild::Thinking { text, .. } => {
                                format!("thinking:{text}")
                            }
                            AssistantChild::Tool { call_id } => format!("tool:{call_id}"),
                            AssistantChild::Attribution { resolved_model } => format!(
                                "attribution:{}:{:?}",
                                resolved_model.selection.model, resolved_model.selection.variant
                            ),
                            AssistantChild::CommittedTool {
                                turn_seq,
                                content_index,
                            } => format!("placeholder:{turn_seq}:{content_index}"),
                        })
                        .collect(),
                )),
                _ => None,
            })
            .collect()
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

    fn rendered_row(app: &mut App, width: u16, height: u16, row: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        let buffer = terminal.backend().buffer();
        (0..width)
            .map(|x| buffer[(x, row)].symbol())
            .collect::<String>()
    }

    fn rendered_cursor_visible(app: &mut App, width: u16, height: u16) -> bool {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        terminal.backend().cursor_visible()
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
        for (width, height) in [(160, 50), (80, 24), (40, 12), (20, 8), (8, 2), (4, 1)] {
            let layout = terminal_layout_with_tree_rows(Rect::new(0, 0, width, height), 3, 0, 1);
            assert_eq!(layout.agent.y, 0);
            assert_eq!(layout.conversation.y, layout.agent.height);
            assert_eq!(layout.bar.height, 1.min(height));
            assert_eq!(layout.bar.y + layout.bar.height, height);
            assert_eq!(layout.input.y + layout.input.height, layout.bar.y);
            assert!(layout.status.y + layout.status.height <= layout.input.y);
        }
    }

    #[tokio::test]
    async fn bottom_bar_renders_cwd_context_and_narrow_degradation_order() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.sessions = vec![session_meta(session)];
        app.store.sessions.insert(
            session,
            SessionState {
                context_tokens: Some(48_200),
                estimated_cost_usd: Some(0.18),
                ..SessionState::default()
            },
        );
        let mut descriptor = model_descriptor();
        descriptor.capabilities.context_tokens = 200_000;
        app.models = vec![descriptor];
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
            preset: None,
        });

        let wide = rendered_row(&mut app, 100, 24, 23);
        assert!(wide.contains("/workspace"));
        assert!(wide.contains("auto-approve    $0.18    ctx 48.2K (24%)    `ctrl+p` commands"));

        let without_hint = rendered_row(&mut app, 55, 24, 23);
        assert!(without_hint.contains("auto-approve    $0.18    ctx 48.2K (24%)"));
        assert!(!without_hint.contains("ctrl+p"));

        let without_cost = rendered_row(&mut app, 39, 24, 23);
        assert!(
            without_cost.contains("auto-approve    ctx 48.2K (24%)"),
            "{without_cost}"
        );
        assert!(!without_cost.contains("$0.18"));

        let without_percentage = rendered_row(&mut app, 30, 24, 23);
        assert!(without_percentage.contains("auto-approve    ctx 48.2K"));
        assert!(!without_percentage.contains("24%"));

        let mode_only = rendered_row(&mut app, 18, 24, 23);
        assert!(mode_only.contains("auto-approve"));
        assert!(!mode_only.contains("ctx"));

        app.store
            .sessions
            .get_mut(&session)
            .expect("session")
            .context_tokens = Some(u64::MAX);
        let no_reintroduced_hint = rendered_row(&mut app, 33, 24, 23);
        assert!(no_reintroduced_hint.contains("auto-approve"));
        assert!(!no_reintroduced_hint.contains("ctrl+p"));
        assert!(!no_reintroduced_hint.contains("ctx"));

        app.store
            .sessions
            .get_mut(&session)
            .expect("session")
            .estimated_cost_usd = Some(0.0031);
        let compact = rendered_row(&mut app, 100, 24, 23);
        assert!(compact.contains("$0.0031"), "{compact}");
        app.store
            .sessions
            .get_mut(&session)
            .expect("session")
            .estimated_cost_usd = None;
        let unpriced = rendered_row(&mut app, 100, 24, 23);
        assert!(!unpriced.contains('$'), "{unpriced}");
    }

    #[tokio::test]
    async fn clicking_bottom_bar_cost_opens_usage_panel() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.sessions = vec![session_meta(session)];
        app.store.sessions.insert(
            session,
            SessionState {
                estimated_cost_usd: Some(0.18),
                ..SessionState::default()
            },
        );
        rendered_frame(&mut app, 80, 24);
        let hit = app.hit_map.session_cost.expect("session cost hit");
        app.handle_click(hit.x, hit.y).await;
        assert_eq!(app.modal, Modal::Usage);
        assert!(app.usage_panel.loading);
    }

    async fn open_usage_from_bottom_bar(app: &mut App) {
        rendered_frame(app, 80, 24);
        let hit = app.hit_map.session_cost.expect("session cost hit");
        app.handle_click(hit.x, hit.y).await;
        assert_eq!(app.modal, Modal::Usage);
    }

    fn usage_loaded_update(
        generation: u64,
        session_id: SessionId,
        request_count: u64,
    ) -> RpcUpdate {
        RpcUpdate::UsageLoaded {
            generation,
            session_id: Some(session_id),
            session: Ok(Some(cookie_agent_protocol::SessionUsageResult {
                session_id,
                usage: cookie_agent_protocol::UsageRollup {
                    request_count,
                    ..cookie_agent_protocol::UsageRollup::default()
                },
            })),
            tree: Ok(Some(cookie_agent_protocol::SessionTreeUsageResult {
                session_id,
                usage: cookie_agent_protocol::UsageRollup {
                    request_count,
                    ..cookie_agent_protocol::UsageRollup::default()
                },
                session_count: 1,
            })),
        }
    }

    fn failed_tree_usage_update(
        generation: u64,
        session_id: SessionId,
        code: i32,
        message: &str,
    ) -> RpcUpdate {
        RpcUpdate::UsageLoaded {
            generation,
            session_id: Some(session_id),
            session: Ok(None),
            tree: Err(crate::client::ClientError::Rpc(
                cookie_agent_protocol::JsonRpcError {
                    code,
                    message: message.into(),
                    data: None,
                },
            )),
        }
    }

    #[tokio::test]
    async fn tree_usage_corruption_has_a_distinct_panel_state_from_missing_session() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.modal = Modal::Usage;
        app.usage_load_generation = 1;
        app.usage_panel.begin_load();
        app.handle_rpc_update(failed_tree_usage_update(
            1,
            session,
            cookie_agent_protocol::SESSION_TREE_USAGE_CORRUPT_DELEGATION_CODE,
            "session tree usage corrupted delegation record",
        ));
        let corrupt = rendered_frame(&mut app, 100, 24);
        assert!(
            corrupt.contains("Tree usage unavailable: corrupted delegation record"),
            "{corrupt}"
        );

        app.usage_load_generation = 2;
        app.usage_panel.begin_load();
        app.handle_rpc_update(failed_tree_usage_update(
            2,
            session,
            cookie_agent_protocol::SESSION_TREE_USAGE_MISSING_SESSION_CODE,
            "session tree usage session not found",
        ));
        let missing = rendered_frame(&mut app, 100, 24);
        assert!(
            !missing.contains("corrupted delegation record"),
            "{missing}"
        );
        assert!(missing.contains("No usage available."), "{missing}");
    }

    #[tokio::test]
    async fn stale_usage_load_cannot_clobber_reopen_for_a_different_session() {
        let (client, _requests) = recording_client();
        let mut app = App::new(client).await.expect("test app");
        let first = SessionId::new_v7();
        let second = SessionId::new_v7();
        app.sessions = vec![session_meta(first), session_meta(second)];
        for session_id in [first, second] {
            app.store.sessions.insert(
                session_id,
                SessionState {
                    estimated_cost_usd: Some(0.18),
                    ..SessionState::default()
                },
            );
        }

        app.selected = Some(first);
        open_usage_from_bottom_bar(&mut app).await;
        let stale_generation = app.usage_load_generation;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        app.selected = Some(second);
        open_usage_from_bottom_bar(&mut app).await;
        let current_generation = app.usage_load_generation;
        assert!(current_generation > stale_generation);

        app.handle_rpc_update(usage_loaded_update(stale_generation, first, 99));
        assert!(app.usage_panel.loading);
        assert!(app.usage_panel.session.is_none());
        assert!(app.usage_panel.tree.is_none());
        app.handle_rpc_update(usage_loaded_update(current_generation, second, 2));
        assert!(!app.usage_panel.loading);
        assert_eq!(
            app.usage_panel
                .session
                .as_ref()
                .map(|result| (result.session_id, result.usage.request_count)),
            Some((second, 2))
        );
    }

    #[tokio::test]
    async fn stale_usage_load_cannot_clobber_reopen_for_the_same_session() {
        let (client, _requests) = recording_client();
        let mut app = App::new(client).await.expect("test app");
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.sessions = vec![session_meta(session)];
        app.store.sessions.insert(
            session,
            SessionState {
                estimated_cost_usd: Some(0.18),
                ..SessionState::default()
            },
        );

        open_usage_from_bottom_bar(&mut app).await;
        let stale_generation = app.usage_load_generation;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        open_usage_from_bottom_bar(&mut app).await;
        let current_generation = app.usage_load_generation;

        app.handle_rpc_update(usage_loaded_update(stale_generation, session, 99));
        assert!(app.usage_panel.loading);
        assert!(app.usage_panel.session.is_none());
        app.handle_rpc_update(usage_loaded_update(current_generation, session, 1));
        assert_eq!(
            app.usage_panel
                .session
                .as_ref()
                .map(|result| result.usage.request_count),
            Some(1)
        );
    }

    #[tokio::test]
    async fn usage_panel_loads_session_and_tree_without_refreshing_bottom_bar_cost() {
        let (startup_client, _startup) = recording_client();
        let mut app = App::new(startup_client).await.expect("test app");
        let (client, recorded, incoming) = live_recording_client();
        app.client = client;
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.sessions = vec![session_meta(session)];
        app.store.sessions.insert(
            session,
            SessionState {
                estimated_cost_usd: Some(0.18),
                ..SessionState::default()
            },
        );
        rendered_frame(&mut app, 80, 24);
        let hit = app.hit_map.session_cost.expect("session cost hit");
        app.handle_click(hit.x, hit.y).await;

        let session_request = wait_for_recorded_request(&recorded, "session.usage", 1).await;
        let tree_request = wait_for_recorded_request(&recorded, "session.tree_usage", 1).await;
        assert_eq!(recorded_method_count(&recorded, "usage.global"), 0);
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": session_request,
                "result": cookie_agent_protocol::SessionUsageResult {
                    session_id: session,
                    usage: cookie_agent_protocol::UsageRollup {
                        request_count: 1,
                        estimated_cost_usd: Some(99.0),
                        ..cookie_agent_protocol::UsageRollup::default()
                    }
                }
            })))
            .expect("session usage response");
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": tree_request,
                "result": cookie_agent_protocol::SessionTreeUsageResult {
                    session_id: session,
                    usage: cookie_agent_protocol::UsageRollup {
                        request_count: 3,
                        estimated_cost_usd: Some(0.42),
                        ..cookie_agent_protocol::UsageRollup::default()
                    },
                    session_count: 3,
                }
            })))
            .expect("tree usage response");
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("usage panel update timeout")
            .expect("usage panel update");
        app.handle_rpc_update(update);

        assert_eq!(
            app.usage_panel
                .session
                .as_ref()
                .map(|result| result.usage.request_count),
            Some(1)
        );
        assert_eq!(
            app.usage_panel
                .tree
                .as_ref()
                .map(|result| (result.usage.request_count, result.session_count)),
            Some((3, 3))
        );
        assert_eq!(app.store.sessions[&session].estimated_cost_usd, Some(0.18));
    }

    #[tokio::test]
    async fn usage_modal_owns_keyboard_and_wheel_scrolling() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        let by_model = (0..8)
            .map(|index| {
                (
                    format!("test/model-{index}").parse().unwrap(),
                    cookie_agent_protocol::ModelUsageRollup {
                        request_count: 1,
                        input_tokens: 1_000,
                        estimated_cost_usd: Some(f64::from(index) / 100.0),
                        ..cookie_agent_protocol::ModelUsageRollup::default()
                    },
                )
            })
            .collect();
        let usage = cookie_agent_protocol::UsageRollup {
            request_count: 8,
            by_model,
            ..cookie_agent_protocol::UsageRollup::default()
        };
        app.usage_panel.session = Some(cookie_agent_protocol::SessionUsageResult {
            session_id: session,
            usage: usage.clone(),
        });
        app.usage_panel.tree = Some(cookie_agent_protocol::SessionTreeUsageResult {
            session_id: session,
            usage,
            session_count: 2,
        });
        app.modal = Modal::Usage;
        rendered_frame(&mut app, 80, 16);
        app.conversation_scroll.offset = 17;
        app.conversation_scroll.following = false;

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(app.usage_panel.scroll, 1);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.usage_panel.scroll, 4);
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .await;
        assert!(app.usage_panel.scroll > 4);
        for _ in 0..100 {
            app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
                .await;
        }
        let max_scroll = app.usage_panel.scroll;
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(app.usage_panel.scroll, max_scroll);
        assert_eq!(app.conversation_scroll.offset, 17);
        assert!(!app.conversation_scroll.following);
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            .await;
        assert!(app.usage_panel.scroll < max_scroll);
        assert_eq!(app.conversation_scroll.offset, 17);
    }

    #[tokio::test]
    async fn usage_events_refresh_session_cost_single_flight() {
        let (startup_client, _startup) = recording_client();
        let mut app = App::new(startup_client).await.expect("test app");
        let (client, recorded, incoming) = live_recording_client();
        app.client = client;
        let session = SessionId::new_v7();
        let run = run_id();
        app.selected = Some(session);
        app.store.sessions.insert(session, SessionState::default());

        app.handle_delivery(live_event(usage_recorded(session, 1, run, 1, Some(1))))
            .await;
        // Leading-edge refresh starts without waiting for a debounce update
        // to be driven through the app loop.
        let id = wait_for_recorded_request(&recorded, "session.usage", 1).await;
        assert_eq!(recorded_method_count(&recorded, "session.usage"), 1);
        // This commit arrives after the first request was captured, so its
        // authoritative total requires one trailing refresh.
        app.handle_delivery(live_event(usage_recorded(session, 2, run, 2, Some(1))))
            .await;
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": cookie_agent_protocol::SessionUsageResult {
                    session_id: session,
                    usage: cookie_agent_protocol::UsageRollup {
                        estimated_cost_usd: Some(0.10),
                        ..cookie_agent_protocol::UsageRollup::default()
                    }
                }
            })))
            .expect("script usage response");
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("usage update timeout")
            .expect("usage update");
        app.handle_rpc_update(update);

        let id = drive_until_recorded_request(&mut app, &recorded, "session.usage", 2).await;
        assert_eq!(recorded_method_count(&recorded, "session.usage"), 2);
        let current_request_id = app
            .session_cost_request_id_for_test(session)
            .expect("trailing request id");
        app.handle_rpc_update(RpcUpdate::SessionCostLoaded {
            session_id: session,
            request_id: current_request_id.wrapping_sub(1),
            result: Ok(cookie_agent_protocol::SessionUsageResult {
                session_id: session,
                usage: cookie_agent_protocol::UsageRollup {
                    estimated_cost_usd: Some(99.0),
                    ..cookie_agent_protocol::UsageRollup::default()
                },
            }),
        });
        assert_eq!(app.store.sessions[&session].estimated_cost_usd, Some(0.10));
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": cookie_agent_protocol::SessionUsageResult {
                    session_id: session,
                    usage: cookie_agent_protocol::UsageRollup {
                        estimated_cost_usd: Some(0.18),
                        ..cookie_agent_protocol::UsageRollup::default()
                    }
                }
            })))
            .expect("script trailing usage response");
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("trailing usage update timeout")
            .expect("trailing usage update");
        app.handle_rpc_update(update);

        assert!(app.session_cost_refresh_idle_for_test(session));
        assert_eq!(app.store.sessions[&session].estimated_cost_usd, Some(0.18));
    }

    #[tokio::test]
    async fn bottom_bar_permission_mode_is_per_session_and_click_cycles_with_rpc() {
        let (client, _startup_requests) = recording_client();
        let mut app = App::new(client).await.expect("test app");
        let (client, requests, _incoming) = live_recording_client();
        app.client = client;
        let first = SessionId::new_v7();
        let second = SessionId::new_v7();
        app.sessions = vec![session_meta(first), session_meta(second)];
        app.selected = Some(first);
        app.permission_modes
            .insert(second, cookie_agent_protocol::PermissionMode::Yolo);
        requests.lock().expect("requests lock").clear();

        let first_row = rendered_row(&mut app, 80, 24, 23);
        // Without token data the bar shows no placeholder segment — just
        // the mode and the commands hint.
        assert!(first_row.contains("auto-approve    `ctrl+p` commands"));
        assert!(!first_row.contains("ctx"), "{first_row}");
        let hit = app.hit_map.permission_mode.expect("permission mode hit");
        app.handle_click(hit.x, hit.y).await;
        assert_eq!(
            app.permission_modes[&first],
            cookie_agent_protocol::PermissionMode::AutoApproveN
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if requests
                    .lock()
                    .expect("requests lock")
                    .iter()
                    .any(|request| {
                        request["method"] == "session.set_permission_mode"
                            && request["params"]["session_id"] == serde_json::json!(first)
                            && request["params"]["mode"] == "auto_approve_n"
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("permission mode RPC");
        assert!(
            requests
                .lock()
                .expect("requests lock")
                .iter()
                .any(|request| {
                    request["method"] == "session.set_permission_mode"
                        && request["params"]["session_id"] == serde_json::json!(first)
                        && request["params"]["mode"] == "auto_approve_n"
                })
        );

        app.selected = Some(second);
        let second_row = rendered_row(&mut app, 80, 24, 23);
        assert!(second_row.contains("yolo    `ctrl+p` commands"));
        let hit = app.hit_map.permission_mode.expect("permission mode hit");
        app.handle_click(hit.x, hit.y).await;
        assert_eq!(
            app.permission_modes[&second],
            cookie_agent_protocol::PermissionMode::AutoApprove
        );

        app.selected = Some(first);
        for (expected_mode, expected_label) in [
            (
                cookie_agent_protocol::PermissionMode::AutoApproveY,
                "auto-y",
            ),
            (cookie_agent_protocol::PermissionMode::Ask, "ask"),
            (cookie_agent_protocol::PermissionMode::Yolo, "yolo"),
            (
                cookie_agent_protocol::PermissionMode::AutoApprove,
                "auto-approve",
            ),
        ] {
            rendered_row(&mut app, 80, 24, 23);
            let hit = app.hit_map.permission_mode.expect("permission mode hit");
            app.handle_click(hit.x, hit.y).await;
            assert_eq!(app.permission_modes[&first], expected_mode);
            assert!(
                rendered_row(&mut app, 80, 24, 23).contains(expected_label),
                "missing permission mode label {expected_label}"
            );
        }
    }

    #[tokio::test]
    async fn selecting_a_session_loads_its_permission_mode_for_the_bottom_bar() {
        let (startup_client, _startup) = recording_client();
        let mut app = App::new(startup_client).await.expect("test app");
        let (client, recorded, incoming) = live_recording_client();
        app.client = client;
        let session = SessionId::new_v7();
        app.sessions = vec![session_meta(session)];
        app.store.sessions.insert(session, SessionState::default());

        app.set_selected_session(session);
        let id = wait_for_recorded_request(&recorded, "session.permission.get", 1).await;
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "permissions": [],
                    "current_mode": "auto_approve_y"
                }
            })))
            .expect("script permission mode response");
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("permission mode update timeout")
            .expect("permission mode update");
        app.handle_rpc_update(update);

        assert_eq!(
            app.permission_modes[&session],
            cookie_agent_protocol::PermissionMode::AutoApproveY
        );
        assert!(rendered_row(&mut app, 80, 24, 23).contains("auto-y"));
    }

    #[tokio::test]
    async fn failed_mode_click_reloads_authoritative_state_after_stale_hydration() {
        let (startup_client, _startup) = recording_client();
        let mut app = App::new(startup_client).await.expect("test app");
        let (client, recorded, incoming) = live_recording_client();
        app.client = client;
        let session = SessionId::new_v7();
        app.sessions = vec![session_meta(session)];
        app.store.sessions.insert(session, SessionState::default());

        app.set_selected_session(session);
        let hydration_id = wait_for_recorded_request(&recorded, "session.permission.get", 1).await;
        rendered_row(&mut app, 80, 24, 23);
        let hit = app.hit_map.permission_mode.expect("permission mode hit");
        app.handle_click(hit.x, hit.y).await;
        assert_eq!(
            app.permission_modes[&session],
            cookie_agent_protocol::PermissionMode::AutoApproveN
        );
        let mutation_id =
            wait_for_recorded_request(&recorded, "session.set_permission_mode", 1).await;

        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": hydration_id,
                "result": {
                    "permissions": [],
                    "current_mode": "auto_approve_y"
                }
            })))
            .expect("script stale hydration response");
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("stale hydration update timeout")
            .expect("stale hydration update");
        app.handle_rpc_update(update);
        assert_eq!(
            app.permission_modes[&session],
            cookie_agent_protocol::PermissionMode::AutoApproveN
        );

        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": mutation_id,
                "error": {
                    "code": -32000,
                    "message": "set mode failed",
                    "data": null
                }
            })))
            .expect("script failed mutation response");
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("mutation failure update timeout")
            .expect("mutation failure update");
        app.handle_rpc_update(update);
        assert!(!app.permission_modes.contains_key(&session));

        let retry_id = wait_for_recorded_request(&recorded, "session.permission.get", 2).await;
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": retry_id,
                "result": {
                    "permissions": [],
                    "current_mode": "auto_approve_y"
                }
            })))
            .expect("script authoritative hydration response");
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("authoritative hydration update timeout")
            .expect("authoritative hydration update");
        app.handle_rpc_update(update);

        assert_eq!(
            app.permission_modes[&session],
            cookie_agent_protocol::PermissionMode::AutoApproveY
        );
        assert!(rendered_row(&mut app, 80, 24, 23).contains("auto-y"));
    }

    #[tokio::test]
    async fn agent_panel_text_rows_are_clamped_1_to_8_with_borders_outside() {
        let mut app = test_app().await;
        for (sessions, expected_rows) in [(0usize, 3u16), (1, 3), (2, 4), (3, 5), (8, 10), (9, 10)]
        {
            let layout =
                terminal_layout_with_tree_rows(Rect::new(0, 0, 80, 24), sessions.max(1), 0, 1);
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
            let entries = app.tree_entries();
            terminal
                .draw(|frame| app.render_tree(frame, layout.agent, &entries))
                .expect("render");
            let buffer = terminal.backend().buffer().clone();
            let top = buffer[(0, 0)].symbol() == "┌";
            let bottom = buffer[(0, expected_rows - 1)].symbol() == "└";
            let below = buffer[(0, expected_rows)].symbol() == "└";
            assert!(top, "sessions {sessions}");
            assert!(bottom, "sessions {sessions}");
            assert!(!below, "sessions {sessions}");
        }
        let tiny = terminal_layout_with_tree_rows(Rect::new(0, 0, 20, 8), 20, 0, 1);
        // The single-row composer is three rows tall, so the eight-row
        // terminal leaves four rows above the bar: one for the status line,
        // one guaranteed conversation row, and the rest for the agent panel
        // (borders only at this extreme).
        assert_eq!(tiny.agent.height, 2);
        assert_eq!(tiny.conversation.height, 1);
    }

    #[test]
    fn composer_grows_with_text_rows_and_reclaims_conversation() {
        let area = Rect::new(0, 0, 80, 24);
        let single = terminal_layout_with_tree_rows(area, 3, 0, 1);
        assert_eq!(single.input.height, 3);
        let grown = terminal_layout_with_tree_rows(area, 3, 0, 4);
        assert_eq!(grown.input.height, 6);
        // Every added composer row comes out of the conversation pane; the
        // agent panel, status line, and bar keep their geometry.
        assert_eq!(
            single.conversation.height - grown.conversation.height,
            grown.input.height - single.input.height
        );
        assert_eq!(grown.agent, single.agent);
        assert_eq!(grown.bar, single.bar);
        // The ceiling is five text rows plus borders, and the box stays
        // glued to the bar above it.
        let ceiling = terminal_layout_with_tree_rows(area, 3, 0, 99);
        assert_eq!(ceiling.input.height, 7);
        assert_eq!(ceiling.input.y + ceiling.input.height, ceiling.bar.y);
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
    fn two_turn_assistant_item_renders_one_header() {
        let state = assistant_state(vec![
            AssistantChild::Text {
                id: 10,
                version: 0,
                markdown: MarkdownDocument::new("first turn".into()),
            },
            AssistantChild::Text {
                id: 20,
                version: 0,
                markdown: MarkdownDocument::new("second turn".into()),
            },
        ]);
        let rendered = snapshot_lines(&transcript_layout(&state, None, 60).lines);
        assert_eq!(
            rendered
                .matches("╭─ primary • gateway/arbitrary-model[base]")
                .count(),
            1
        );
        assert!(rendered.contains("first turn"));
        assert!(rendered.contains("second turn"));
    }

    #[test]
    fn attribution_marker_renders_without_region_and_is_skipped_by_navigation() {
        let item = TranscriptItem::Assistant {
            id: 1,
            version: 0,
            attribution: attribution(None),
            committed_turn_seq: Some(2),
            children: vec![
                AssistantChild::Thinking {
                    id: 10,
                    version: 0,
                    text: "thought".into(),
                },
                AssistantChild::Attribution {
                    resolved_model: resolved_model(Some("high")),
                },
            ],
        };
        let state = SessionState {
            transcript: vec![item.clone()],
            ..SessionState::default()
        };
        let layout = transcript_layout(&state, None, 60);
        assert!(
            snapshot_lines(&layout.lines).contains("├─ now using gateway/arbitrary-model[high]")
        );
        assert_eq!(layout.regions.len(), 1);
        assert_eq!(item_block_ids(&item), vec![BlockId::Thinking(10)]);
    }

    // ------------------------------------------------------------------
    // Assistant footer: generation speed + context from committed turns
    // ------------------------------------------------------------------

    /// One committed-turn log with fixed durable timestamps: user input at
    /// T+0 closes the turn's input window, the commit lands at T+2s with
    /// the given usage. Every timestamp is pinned so runs are reproducible.
    fn footer_event_log(
        session: SessionId,
        run: RunId,
        attempt: AttemptId,
        usage: Option<(u64, u64)>,
        commit_after_seconds: i64,
    ) -> Vec<StoredEvent> {
        let base: Timestamp = "2026-08-06T12:00:00Z".parse().expect("timestamp");
        let at = |seconds: i64| {
            base.checked_add(jiff::SignedDuration::from_secs(seconds))
                .expect("timestamp")
        };
        let stamp = |stored: StoredEvent, seconds: i64| StoredEvent {
            timestamp: at(seconds),
            ..stored
        };
        let mut commit = turn_committed(
            session,
            4,
            run,
            attempt,
            1,
            vec![text_part("the answer")],
            Vec::new(),
            None,
        );
        let EventPayload::ModelTurnCommitted {
            input_through_seq,
            turn,
            ..
        } = &mut commit.payload
        else {
            panic!("expected committed turn");
        };
        *input_through_seq = 2;
        match usage {
            Some((input_tokens, output_tokens)) => {
                turn.usage.input_tokens = Some(input_tokens);
                turn.usage.output_tokens = Some(output_tokens);
            }
            None => {
                turn.usage = Usage {
                    input_tokens: None,
                    input_tokens_no_cache: None,
                    input_tokens_cache_read: None,
                    input_tokens_cache_write: None,
                    output_tokens: None,
                    output_tokens_text: None,
                    output_tokens_reasoning: None,
                };
            }
        }
        vec![
            stamp(session_created(session, 1), 0),
            stamp(
                event(
                    session,
                    2,
                    run,
                    EventPayload::UserInputSubmitted {
                        input: "question".into(),
                    },
                ),
                0,
            ),
            stamp(attempt_started(session, 3, run, attempt, None), 0),
            stamp(commit, commit_after_seconds),
        ]
    }

    async fn app_with_footer_log(events: Vec<StoredEvent>, session: SessionId) -> App {
        let mut app = test_app().await;
        app.selected = Some(session);
        for event in events {
            assert!(app.store.apply_event(event));
        }
        app
    }

    #[tokio::test]
    async fn assistant_footer_shows_speed_and_context_from_durable_timestamps() {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        // 84 output tokens over the 2s span between the input-closing event
        // and the commit: 42.0 tps; the ctx is the end-of-turn total,
        // 12,400 input + 84 generated = 12,484 → 12.5K.
        let mut app = app_with_footer_log(
            footer_event_log(session, run, attempt, Some((12_400, 84)), 2),
            session,
        )
        .await;
        let rendered = frame_rows(&mut app, 100, 30).join("\n");
        // ⚡ is a two-cell glyph; assert the gutter and the values around it.
        assert!(rendered.contains("╰─ ⚡"), "gutter: {rendered}");
        assert!(
            rendered.contains("42.0 tps · 12.5K ctx"),
            "footer: {rendered}"
        );
        // The footer is passive: it registers no hover target.
        let footer_row = rendered
            .lines()
            .position(|line| line.contains("tps"))
            .map(|row| row as u16)
            .expect("footer row");
        assert_eq!(app.hover_target_at(2, footer_row), None);
    }

    #[tokio::test]
    async fn assistant_footer_shows_priced_cost_and_omits_unpriced_cost() {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let mut events = footer_event_log(session, run, attempt, Some((12_400, 84)), 2);
        events.push(usage_recorded(session, 5, run, 1, Some(3_100_000_000)));
        let mut app = app_with_footer_log(events, session).await;
        let rendered = frame_rows(&mut app, 100, 30).join("\n");
        assert!(rendered.contains("12.5K ctx · $0.0031"), "{rendered}");

        let attempt = AttemptId::new_v7();
        let mut events = footer_event_log(session, run, attempt, Some((12_400, 84)), 2);
        events.push(usage_recorded(session, 5, run, 1, None));
        let mut app = app_with_footer_log(events, session).await;
        let rendered = frame_rows(&mut app, 100, 30).join("\n");
        assert!(rendered.contains("12.5K ctx"), "{rendered}");
        assert!(!rendered.contains('$'), "{rendered}");
    }

    #[tokio::test]
    async fn replayed_footer_cost_matches_engine_session_usage() {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let base: Timestamp = "2026-08-06T12:00:00Z".parse().unwrap();
        let at = |seconds: i64| {
            base.checked_add(jiff::SignedDuration::from_secs(seconds))
                .unwrap()
        };
        let stamp = |stored: StoredEvent, seconds: i64| StoredEvent {
            timestamp: at(seconds),
            ..stored
        };
        let reported = Usage {
            input_tokens: Some(12_400),
            input_tokens_cache_read: Some(0),
            output_tokens: Some(84),
            output_tokens_reasoning: Some(0),
            ..Usage::default()
        };
        let mut resolved = resolved_model(None);
        resolved.adapter_id = cookie_agent_protocol::AdaptorId::OpenaiResponses;
        let mut binding = frozen_binding(resolved.clone());
        binding.protocol_recipe =
            cookie_agent_protocol::ProtocolRecipeId::new("oven.openai.responses").unwrap();
        binding.descriptor = serde_json::from_value(serde_json::json!({
            "identity": {"provider_id": "gateway", "model_id": "arbitrary-model"},
            "adapter_id": "openai-responses",
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
        .unwrap();
        binding.options = cookie_agent_protocol::ProviderOptions::OpenAiResponses {
            organization: None,
            project: None,
            store: None,
        };
        let selection = RunSelection {
            agent: agent_id(),
            model: resolved.selection.clone(),
            preset: None,
        };
        let created =
            session_created_from_bindings(session, 1, selection.clone(), vec![binding.clone()], 0);
        let EventPayload::SessionCreated {
            creation_agent,
            runtime_revision,
            catalog_revision,
            provider_state_revision,
            model_revision,
            agent_revision,
            recipe_registry_revision,
            manifest_revision,
            ..
        } = &created.payload
        else {
            unreachable!()
        };
        let run_started = event(
            session,
            2,
            run,
            EventPayload::RunStarted {
                client_run_id: cookie_agent_protocol::ClientRunId::new("cost-invariant").unwrap(),
                selection,
                agent: creation_agent.clone(),
                runtime_revision: runtime_revision.clone(),
                catalog_revision: catalog_revision.clone(),
                provider_state_revision: provider_state_revision.clone(),
                model_revision: model_revision.clone(),
                agent_revision: agent_revision.clone(),
                recipe_registry_revision: recipe_registry_revision.clone(),
                manifest_revision: manifest_revision.clone(),
                selected_suffix: vec![binding],
                input_through_seq: 1,
            },
        );
        let mut commit = turn_committed(
            session,
            5,
            run,
            attempt,
            1,
            vec![text_part("the answer")],
            Vec::new(),
            None,
        );
        let EventPayload::ModelTurnCommitted {
            input_through_seq,
            resolved_model: committed_model,
            turn,
            ..
        } = &mut commit.payload
        else {
            unreachable!()
        };
        *input_through_seq = 3;
        *committed_model = resolved.clone();
        turn.usage = reported.clone();
        let mut usage = usage_recorded(session, 6, run, 1, Some(3_100_000_000));
        let EventPayload::ModelUsageRecorded {
            resolved_model,
            usage: event_usage,
            ..
        } = &mut usage.payload
        else {
            unreachable!()
        };
        *resolved_model = resolved.clone();
        *event_usage = reported;
        let attempt_started = event(
            session,
            4,
            run,
            EventPayload::ModelAttemptStarted {
                attempt_id: attempt,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved,
                prompt_fingerprint: creation_agent.prompt_fingerprint.clone(),
            },
        );
        let events = [
            stamp(created, 0),
            stamp(run_started, 0),
            stamp(
                event(
                    session,
                    3,
                    run,
                    EventPayload::UserInputSubmitted {
                        input: "question".into(),
                    },
                ),
                0,
            ),
            stamp(attempt_started, 0),
            stamp(commit, 2),
            stamp(usage, 2),
        ];

        let mut tui_store = StateStore::default();
        for event in events.iter().cloned() {
            assert!(tui_store.apply_event(event));
        }
        let state = &tui_store.sessions[&session];
        let item_id = state
            .transcript
            .iter()
            .find_map(|item| matches!(item, TranscriptItem::Assistant { .. }).then(|| item.id()))
            .expect("assistant item");
        let footer = assistant_footer_line(state, item_id, 100, &Theme::default())
            .expect("assistant footer")
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let footer_cost = footer.rsplit(" · ").next().expect("footer cost");

        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let data_dir = directory.path().join("data");
        let session_dir =
            cookie_agent_engine::session::SessionStore::project_dir(&data_dir, directory.path())
                .join("sessions")
                .join(session.to_string());
        #[cfg(unix)]
        fs::create_dir_all(&session_dir).unwrap();
        #[cfg(windows)]
        cookie_agent_models::secure_store::SecureDirectory::open(&session_dir)
            .expect("private session directory");
        let jsonl = events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        #[cfg(unix)]
        fs::write(session_dir.join("events.jsonl"), jsonl).unwrap();
        #[cfg(windows)]
        {
            use std::io::Write as _;

            let path = session_dir.join("events.jsonl");
            let mut file = cookie_agent_models::secure_store::create_windows_private_file(&path)
                .expect("private event log");
            file.write_all(jsonl.as_bytes()).expect("write event log");
            file.sync_all().expect("sync event log");
        }
        let engine_sessions =
            cookie_agent_engine::session::SessionStore::open(&data_dir, directory.path()).unwrap();
        let session_cost = format_cost_usd(
            engine_sessions
                .session_usage(
                    session,
                    &cookie_agent_config::PricingConfig::default(),
                    &BTreeMap::new(),
                )
                .unwrap()
                .usage
                .estimated_cost_usd
                .expect("session cost"),
        );
        assert_eq!(footer_cost, session_cost);
    }

    #[tokio::test]
    async fn assistant_footer_hides_without_usage_or_a_positive_duration() {
        let session = SessionId::new_v7();
        let run = run_id();
        // Old sessions carry no usage at all: no placeholder, no row.
        let attempt = AttemptId::new_v7();
        let mut app =
            app_with_footer_log(footer_event_log(session, run, attempt, None, 2), session).await;
        let rendered = frame_rows(&mut app, 100, 30).join("\n");
        assert!(!rendered.contains("tps"), "no usage: {rendered}");

        // A zero-duration span (commit timestamp equals the input's) hides
        // the footer rather than dividing by zero.
        let attempt = AttemptId::new_v7();
        let mut app = app_with_footer_log(
            footer_event_log(session, run, attempt, Some((12_400, 84)), 0),
            session,
        )
        .await;
        let rendered = frame_rows(&mut app, 100, 30).join("\n");
        assert!(!rendered.contains("tps"), "zero duration: {rendered}");
    }

    #[tokio::test]
    async fn assistant_footer_is_replay_stable_for_the_same_event_log() {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let footer_of = |app: &mut App| {
            frame_rows(app, 100, 30)
                .into_iter()
                .find(|row| row.contains("tps"))
                .expect("footer row")
        };
        let mut first = app_with_footer_log(
            footer_event_log(session, run, attempt, Some((12_400, 84)), 2),
            session,
        )
        .await;
        let first = footer_of(&mut first);
        let mut second = app_with_footer_log(
            footer_event_log(session, run, attempt, Some((12_400, 84)), 2),
            session,
        )
        .await;
        let second = footer_of(&mut second);
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn assistant_footer_wraps_within_narrow_widths() {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let mut app = app_with_footer_log(
            footer_event_log(session, run, attempt, Some((12_400, 84)), 2),
            session,
        )
        .await;
        // Every width renders without overflow; once the wrapped footer
        // fits whole words (≈16 cells) it stays visible.
        for width in [3, 8, 12, 16, 24] {
            let rows = frame_rows(&mut app, width, 40);
            assert!(
                rows.iter()
                    .all(|row| row.chars().count() <= usize::from(width)),
                "width {width}: {rows:?}"
            );
        }
        for width in [16, 24] {
            let rendered = frame_rows(&mut app, width, 40).join("\n");
            assert!(
                rendered.contains("tps"),
                "width {width} keeps the footer: {rendered}"
            );
        }
    }

    #[test]
    fn merged_thinking_children_have_distinct_regions_and_collapse_state() {
        let state = assistant_state(vec![
            AssistantChild::Thinking {
                id: 10,
                version: 0,
                text: "first thought".into(),
            },
            AssistantChild::Thinking {
                id: 20,
                version: 0,
                text: "second thought".into(),
            },
        ]);
        let expanded = HashSet::from([BlockId::Thinking(10)]);
        let layout = transcript_layout(&state, Some(&expanded), 60);
        assert_eq!(
            layout
                .regions
                .iter()
                .map(|region| region.id)
                .collect::<Vec<_>>(),
            vec![BlockId::Thinking(10), BlockId::Thinking(20)]
        );
        let rendered = snapshot_lines(&layout.lines);
        assert!(rendered.contains("💭 ▾ thought"));
        assert!(rendered.contains("first thought"));
        assert!(rendered.contains("💭 ▸ thought"));
        assert!(!rendered.contains("second thought"));
    }

    #[test]
    fn streaming_thinking_header_animates_its_ellipsis_with_the_clock() {
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let mut store = StateStore::default();
        for event in [
            attempt_started(session, 1, run, attempt, None),
            reasoning_delta(session, 2, run, attempt, "pondering"),
        ] {
            assert!(store.apply_event(event));
        }
        let state = &store.sessions[&session];
        assert!(state.has_open_thinking());
        let mut cache = LayoutCache::default();
        let mut previous = String::new();
        for (bucket, expected) in ["thinking", "thinking.", "thinking..", "thinking..."]
            .iter()
            .enumerate()
        {
            ensure_cached_transcript_layout(
                &mut cache,
                session,
                state,
                None,
                60,
                &Theme::default(),
                &crate::markdown::SyntectHighlighter::default(),
                crate::state::EventLevel::Debug,
                u8::try_from(bucket).expect("bucket"),
            );
            let rendered = snapshot_lines(&cache.layout.lines);
            assert!(
                rendered.contains(&format!("💭 ▸ {expected} ")),
                "bucket {bucket}: {rendered}"
            );
            if bucket > 0 {
                // Each clock bucket invalidates the cached label in place:
                // no transcript mutation is needed to advance the ellipsis.
                assert_ne!(rendered, previous, "bucket {bucket}");
            }
            previous = rendered;
        }
    }

    #[test]
    fn sealed_thinking_header_reports_the_recorded_duration() {
        let mut state = assistant_state(vec![AssistantChild::Thinking {
            id: 7,
            version: 0,
            text: "pondered".into(),
        }]);
        state
            .thinking_durations
            .insert((1, 7), Duration::from_secs(95));
        let collapsed = snapshot_lines(&transcript_layout(&state, None, 60).lines);
        assert!(collapsed.contains("💭 ▸ thought for 1m 35s"), "{collapsed}");
        let expanded = HashSet::from([BlockId::Thinking(7)]);
        let expanded = snapshot_lines(&transcript_layout(&state, Some(&expanded), 60).lines);
        assert!(expanded.contains("💭 ▾ thought for 1m 35s"), "{expanded}");

        // Sub-second streams settle to the plain label: "thought for 0s"
        // would read as noise.
        state
            .thinking_durations
            .insert((1, 7), Duration::from_millis(400));
        let rendered = snapshot_lines(&transcript_layout(&state, None, 60).lines);
        assert!(rendered.contains("💭 ▸ thought "), "{rendered}");
        assert!(!rendered.contains("thought for"), "{rendered}");
    }

    #[tokio::test]
    async fn thinking_clock_cycles_buckets_only_while_thinking_streams() {
        let mut app = test_app().await;
        assert!(!app.animation_active());

        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        for event in [
            session_created(session, 1),
            attempt_started(session, 2, run, attempt, None),
            reasoning_delta(session, 3, run, attempt, "pondering"),
        ] {
            assert!(app.store.apply_event(event));
        }
        app.selected = Some(session);
        assert!(app.animation_active());

        // Twelve 33ms frames per step ≈ 400ms per ellipsis dot, wrapping
        // after "thinking..." back to the bare label.
        assert_eq!(app.clock_bucket(), 0);
        for expected in [1, 2, 3, 0] {
            for _ in 0..12 {
                app.animation_tick();
            }
            assert_eq!(app.clock_bucket(), expected);
        }

        // Sealing the part (any other part opening, or a commit) stops the
        // animation; the UI is event-driven again.
        assert!(
            app.store
                .apply_event(text_delta(session, 4, run, attempt, "answer"))
        );
        assert!(!app.animation_active());
    }

    #[tokio::test]
    async fn running_tool_rows_pulse_with_the_clock() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        let run = run_id();
        let attempt = AttemptId::new_v7();
        let call = ToolCallId::new_v7();
        for event in [
            session_created(session, 1),
            attempt_started(session, 2, run, attempt, None),
            turn_committed(
                session,
                3,
                run,
                attempt,
                1,
                vec![cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                    id: ModelCallId::new("call-1").expect("call"),
                    provider_item_id: None,
                    name: SafeCode::new("bash").expect("tool"),
                    input: serde_json::json!({"command": "sleep 2"}),
                    raw_input: None,
                    metadata: None,
                }],
                Vec::new(),
                None,
            ),
            tool_started_at(
                session,
                4,
                run,
                call,
                1,
                "call-1",
                0,
                "bash",
                Some("sleep 2"),
            ),
        ] {
            assert!(app.store.apply_event(event));
        }
        app.selected = Some(session);

        // A running tool keeps the animation clock alive on its own.
        assert!(app.animation_active());
        let state = &app.store.sessions[&session];
        let mut cache = LayoutCache::default();
        let mut seen = Vec::new();
        for bucket in 0..4u8 {
            ensure_cached_transcript_layout(
                &mut cache,
                session,
                state,
                None,
                60,
                &Theme::default(),
                &crate::markdown::SyntectHighlighter::default(),
                crate::state::EventLevel::Debug,
                bucket,
            );
            seen.push(snapshot_lines(&cache.layout.lines));
        }
        assert!(seen[0].contains("🔨 ▸ bash sleep 2 …"), "{}", seen[0]);
        assert!(seen[1].contains("🔨 ▸ bash sleep 2 ."), "{}", seen[1]);
        assert!(seen[2].contains("🔨 ▸ bash sleep 2 .."), "{}", seen[2]);
        assert!(seen[3].contains("🔨 ▸ bash sleep 2 ..."), "{}", seen[3]);
        // Each bucket re-rendered the cached live item in place.
        assert!(seen.windows(2).all(|pair| pair[0] != pair[1]));

        // Completion settles the row and stops the clock.
        assert!(app.store.apply_event(tool_terminated(
            session,
            5,
            run,
            call,
            1,
            "call-1",
            cookie_agent_protocol::ToolTerminationOutcome::Completed,
        )));
        assert!(!app.animation_active());
        let state = &app.store.sessions[&session];
        let settled = snapshot_lines(&transcript_layout(state, None, 60).lines);
        assert!(settled.contains("🔨 ▸ bash sleep 2"), "{settled}");
        assert!(!settled.contains('…'), "{settled}");
    }

    #[test]
    fn expandable_rows_render_emoji_before_collapsed_and_expanded_chevrons() {
        let call_id = ToolCallId::new_v7();
        let mut state = assistant_state(vec![
            AssistantChild::Thinking {
                id: 10,
                version: 0,
                text: "thought".into(),
            },
            AssistantChild::Tool { call_id },
        ]);
        state.tools.insert(
            call_id,
            ToolCallState {
                id: call_id,
                owner: owner(1, "call-1"),
                presentation: presentation("bash", Some("true")),
                arguments: r#"{"command":"true"}"#.into(),
                status: ToolStatus::Completed,
                detail: String::new(),
                has_output_chunks: false,
            },
        );

        let collapsed = snapshot_lines(&transcript_layout(&state, None, 60).lines);
        assert!(collapsed.contains("💭 ▸ thought"));
        assert!(collapsed.contains("🔨 ▸ bash true"));

        let expanded = HashSet::from([BlockId::Thinking(10), BlockId::Tool(call_id)]);
        let expanded_layout = transcript_layout(&state, Some(&expanded), 60);
        let expanded_rendered = snapshot_lines(&expanded_layout.lines);
        assert!(expanded_rendered.contains("💭 ▾ thought"));
        assert!(expanded_rendered.contains("🔨 ▾ bash true"));
        assert_eq!(expanded_layout.regions.len(), 2);

        let tiny = transcript_layout(&state, Some(&expanded), 4);
        assert_eq!(tiny.regions.len(), 2);
        for width in [6, 7] {
            let layout = transcript_layout(&state, Some(&expanded), width);
            assert_eq!(layout.regions.len(), 2);
            let rendered = snapshot_lines(&layout.lines);
            assert!(rendered.contains('💭'));
            assert!(rendered.contains('🔨'));
            assert_eq!(rendered.matches('▾').count(), 2);
        }
        for width in [8, 12, 18] {
            let layout = transcript_layout(&state, Some(&expanded), width);
            assert!(
                layout.lines.iter().all(|line| {
                    UnicodeWidthStr::width(line.to_string().as_str()) <= usize::from(width)
                }),
                "width {width}: {}",
                snapshot_lines(&layout.lines)
            );
        }
    }

    #[test]
    fn committed_tool_blocks_from_two_turns_have_distinct_ids() {
        let state = assistant_state(vec![
            AssistantChild::CommittedTool {
                turn_seq: 10,
                content_index: 0,
            },
            AssistantChild::CommittedTool {
                turn_seq: 11,
                content_index: 0,
            },
        ]);
        let layout = transcript_layout(&state, None, 60);
        assert_eq!(
            layout
                .regions
                .iter()
                .map(|region| region.id)
                .collect::<Vec<_>>(),
            vec![
                BlockId::CommittedTool {
                    turn_seq: 10,
                    content_index: 0,
                },
                BlockId::CommittedTool {
                    turn_seq: 11,
                    content_index: 0,
                },
            ]
        );
    }

    #[test]
    fn narrow_attribution_marker_preserves_its_gutter() {
        let state = assistant_state(vec![AssistantChild::Attribution {
            resolved_model: resolved_model(Some("high")),
        }]);
        let width = 18;
        let marker_lines = transcript_layout(&state, None, width)
            .lines
            .into_iter()
            .filter(|line| line.to_string().contains("now") || line.to_string().starts_with("├─ "))
            .collect::<Vec<_>>();
        assert!(marker_lines.len() > 1, "marker should wrap at narrow width");
        assert!(marker_lines.iter().all(|line| {
            line.spans
                .first()
                .is_some_and(|span| span.content.as_ref() == "├─ ")
        }));
        assert!(marker_lines.iter().all(|line| {
            unicode_width::UnicodeWidthStr::width(line.to_string().as_str()) <= usize::from(width)
        }));
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
                has_output_chunks: false,
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
                .any(|line| line.trim_end().ends_with("🔨 ▸ bash touch README.md …"))
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
                .any(|line| line.trim_end() == "🔨 ▸ bash touch README.md"
                    || line.trim_end() == "│ 🔨 ▸ bash touch README.md")
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
        assert!(rendered.contains("🔨 ▸ bash touch README.md failed"));
    }

    #[test]
    fn wrapped_tool_arguments_keep_the_assistant_gutter() {
        let call_id = ToolCallId::new_v7();
        let mut state = assistant_state(vec![AssistantChild::Tool { call_id }]);
        state.tools.insert(
            call_id,
            ToolCallState {
                id: call_id,
                owner: owner(1, "call-1"),
                presentation: presentation("bash", None),
                arguments: r#"{"command":"printf a-very-long-single-line-tool-argument"}"#.into(),
                status: ToolStatus::Running,
                detail: String::new(),
                has_output_chunks: false,
            },
        );
        let width = 18;
        let expanded = std::collections::HashSet::from([BlockId::Tool(call_id)]);
        let layout = transcript_layout(&state, Some(&expanded), width);
        let region = layout
            .regions
            .iter()
            .find(|region| region.id == BlockId::Tool(call_id))
            .expect("tool region");
        let body = &layout.lines[region.start_line..region.end_line];

        assert!(body.len() > 2, "long argument should wrap");
        assert!(body.iter().all(|line| {
            line.spans
                .first()
                .is_some_and(|span| span.content.as_ref() == "│ ")
        }));
        assert!(body.iter().all(|line| {
            unicode_width::UnicodeWidthStr::width(line.to_string().as_str()) <= usize::from(width)
        }));
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
    // Streaming reduction against protocol-9 events
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
    fn same_model_retry_after_abandonment_prunes_partials_without_marker() {
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
        let assistants = state
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Assistant { children, .. } => Some(children),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(assistants.len(), 1);
        assert!(matches!(
            assistants[0].as_slice(),
            [AssistantChild::Text { markdown, .. }] if markdown.as_str() == "final"
        ));
    }

    #[test]
    fn model_change_inserts_marker_and_keeps_first_header() {
        let session = SessionId::new_v7();
        let run = run_id();
        let base = AttemptId::new_v7();
        let high = AttemptId::new_v7();
        let mut store = StateStore::default();
        for event in [
            session_created(session, 1),
            attempt_started(session, 2, run, base, None),
            turn_committed(
                session,
                3,
                run,
                base,
                1,
                vec![text_part("base answer")],
                Vec::new(),
                None,
            ),
            attempt_started(session, 4, run, high, Some("high")),
            turn_committed(
                session,
                5,
                run,
                high,
                2,
                vec![text_part("high answer")],
                Vec::new(),
                Some("high"),
            ),
        ] {
            assert!(store.apply_event(event));
        }
        let state = &store.sessions[&session];
        let rendered = transcript_layout(state, None, 60)
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
                .matches("├─ now using gateway/arbitrary-model[high]")
                .count(),
            1
        );
        let assistant = state
            .transcript
            .iter()
            .find_map(|item| match item {
                TranscriptItem::Assistant {
                    attribution,
                    children,
                    ..
                } => Some((attribution, children)),
                _ => None,
            })
            .expect("assistant");
        assert_eq!(assistant.0.variant_label(), "base");
        assert!(matches!(
            assistant.1.as_slice(),
            [
                AssistantChild::Text { markdown: first, .. },
                AssistantChild::Attribution { resolved_model },
                AssistantChild::Text { markdown: second, .. },
            ] if first.as_str() == "base answer"
                && resolved_model.selection.variant.as_ref().is_some_and(|variant| variant.as_str() == "high")
                && second.as_str() == "high answer"
        ));
    }

    #[test]
    fn multi_attempt_run_merges_committed_turns_and_tool_in_order() {
        let session = SessionId::new_v7();
        let run = run_id();
        let first_attempt = AttemptId::new_v7();
        let second_attempt = AttemptId::new_v7();
        let call_id = ToolCallId::new_v7();
        let events = vec![
            attempt_started(session, 1, run, first_attempt, None),
            turn_committed(
                session,
                2,
                run,
                first_attempt,
                10,
                vec![text_part("turn one"), tool_part("call-one")],
                Vec::new(),
                None,
            ),
            tool_started_at(session, 3, run, call_id, 10, "call-one", 1, "bash", None),
            attempt_started(session, 4, run, second_attempt, None),
            turn_committed(
                session,
                5,
                run,
                second_attempt,
                11,
                vec![reasoning_part("turn two thought"), text_part("turn two")],
                Vec::new(),
                None,
            ),
        ];
        let mut store = StateStore::default();
        for event in events {
            assert!(store.apply_event(event));
        }
        let state = &store.sessions[&session];
        assert_eq!(assistant_projection(state).len(), 1);
        let children = match state
            .transcript
            .iter()
            .find(|item| matches!(item, TranscriptItem::Assistant { .. }))
            .expect("assistant")
        {
            TranscriptItem::Assistant { children, .. } => children,
            _ => unreachable!(),
        };
        assert!(matches!(
            children.as_slice(),
            [
                AssistantChild::Text { markdown: first, .. },
                AssistantChild::Tool { call_id: linked },
                AssistantChild::Thinking { text: thought, .. },
                AssistantChild::Text { markdown: second, .. },
            ] if first.as_str() == "turn one"
                && *linked == call_id
                && thought == "turn two thought"
                && second.as_str() == "turn two"
        ));
    }

    #[test]
    fn rebuild_session_matches_live_run_assistant_projection() {
        let session = SessionId::new_v7();
        let run = run_id();
        let first = AttemptId::new_v7();
        let second = AttemptId::new_v7();
        let events = vec![
            attempt_started(session, 1, run, first, None),
            turn_committed(
                session,
                2,
                run,
                first,
                1,
                vec![text_part("first")],
                Vec::new(),
                None,
            ),
            attempt_started(session, 3, run, second, Some("high")),
            text_delta(session, 4, run, second, "partial"),
        ];
        let mut live = StateStore::default();
        for event in events.clone() {
            assert!(live.apply_event(event));
        }
        let mut rebuilt = StateStore::default();
        assert!(rebuilt.rebuild_session(session, 0, events));
        assert_eq!(
            assistant_projection(&live.sessions[&session]),
            assistant_projection(&rebuilt.sessions[&session])
        );
        let live_projection = live.sessions[&session]
            .open_run_assistant
            .as_ref()
            .expect("live run projection");
        let rebuilt_projection = rebuilt.sessions[&session]
            .open_run_assistant
            .as_ref()
            .expect("rebuilt run projection");
        assert_eq!(live_projection.run_id, rebuilt_projection.run_id);
        assert_eq!(
            live_projection.committed_prefix,
            rebuilt_projection.committed_prefix
        );
        assert_eq!(
            live_projection.current_model,
            rebuilt_projection.current_model
        );
    }

    #[test]
    fn a_second_run_starts_a_second_assistant_item() {
        let session = SessionId::new_v7();
        let first_run = run_id();
        let second_run = run_id();
        let first_attempt = AttemptId::new_v7();
        let second_attempt = AttemptId::new_v7();
        let mut store = StateStore::default();
        for event in [
            run_started_with_suffix(session, 1, first_run, vec![resolved_model(None)]),
            attempt_started(session, 2, first_run, first_attempt, None),
            text_delta(session, 3, first_run, first_attempt, "first run"),
            event(
                session,
                4,
                first_run,
                EventPayload::RunCompleted { final_text: None },
            ),
            run_started_with_suffix(session, 5, second_run, vec![resolved_model(None)]),
            attempt_started(session, 6, second_run, second_attempt, None),
            text_delta(session, 7, second_run, second_attempt, "second run"),
        ] {
            assert!(store.apply_event(event));
        }
        assert_eq!(assistant_projection(&store.sessions[&session]).len(), 2);
    }

    #[test]
    fn committed_tools_with_same_content_index_link_to_their_own_turns() {
        let session = SessionId::new_v7();
        let run = run_id();
        let first_attempt = AttemptId::new_v7();
        let second_attempt = AttemptId::new_v7();
        let first_call = ToolCallId::new_v7();
        let second_call = ToolCallId::new_v7();
        let mut store = StateStore::default();
        for event in [
            attempt_started(session, 1, run, first_attempt, None),
            turn_committed(
                session,
                2,
                run,
                first_attempt,
                10,
                vec![tool_part("first-call")],
                Vec::new(),
                None,
            ),
            attempt_started(session, 3, run, second_attempt, None),
            turn_committed(
                session,
                4,
                run,
                second_attempt,
                11,
                vec![tool_part("second-call")],
                Vec::new(),
                None,
            ),
            tool_started(session, 5, run, second_call, 11, "second-call"),
            tool_started(session, 6, run, first_call, 10, "first-call"),
        ] {
            assert!(store.apply_event(event));
        }
        let projection = assistant_projection(&store.sessions[&session]);
        assert_eq!(projection.len(), 1);
        assert_eq!(
            projection[0].2,
            vec![format!("tool:{first_call}"), format!("tool:{second_call}")]
        );
    }

    #[test]
    fn runless_attempts_remain_one_item_per_attempt() {
        let session = SessionId::new_v7();
        let first = AttemptId::new_v7();
        let second = AttemptId::new_v7();
        let mut store = StateStore::default();
        for event in [
            runless_event(
                session,
                1,
                EventPayload::ModelAttemptStarted {
                    attempt_id: first,
                    attempt_ordinal: 1,
                    fallback_index: 0,
                    retry_ordinal: 0,
                    resolved_model: resolved_model(None),
                    prompt_fingerprint: Sha256Digest::of_bytes(b"first"),
                },
            ),
            runless_event(
                session,
                2,
                EventPayload::TextDelta {
                    attempt_id: first,
                    text: "first".into(),
                },
            ),
            runless_event(
                session,
                3,
                EventPayload::ModelAttemptStarted {
                    attempt_id: second,
                    attempt_ordinal: 2,
                    fallback_index: 0,
                    retry_ordinal: 0,
                    resolved_model: resolved_model(None),
                    prompt_fingerprint: Sha256Digest::of_bytes(b"second"),
                },
            ),
            runless_event(
                session,
                4,
                EventPayload::TextDelta {
                    attempt_id: second,
                    text: "second".into(),
                },
            ),
        ] {
            assert!(store.apply_event(event));
        }
        assert_eq!(assistant_projection(&store.sessions[&session]).len(), 2);
    }

    #[test]
    fn tool_call_only_attempt_adds_no_empty_segments() {
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
                9,
                vec![tool_part("only-call")],
                Vec::new(),
                None,
            ),
        ] {
            assert!(store.apply_event(event));
        }
        let projection = assistant_projection(&store.sessions[&session]);
        assert_eq!(projection[0].2, vec!["placeholder:9:0"]);
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
        assert_eq!(label, "  ● ✅ primary:fix the flaky test");
        let label = app.tree_row_label(&entries[0], true);
        assert_eq!(label, "> ● ✅ primary:fix the flaky test");
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
            "    ✅ primary:untitled"
        );
    }
    #[tokio::test]
    async fn watched_tree_row_keeps_its_glyph_but_never_a_color_marker() {
        let mut app = test_app().await;
        // Pin the default true-color theme so the assertions do not depend
        // on the developer's ambient tui.toml or terminal detection.
        app.theme = Theme::default();
        let meta = |id: SessionId, title: &str| {
            let mut meta = titled_meta(id, title, 1);
            // Space-padded statuses keep every glyph single-width: ratatui
            // resets the continuation cell after a wide emoji, which would
            // otherwise break the cell-for-cell comparison below.
            meta.status = SessionStatus::Failed;
            meta
        };
        let watched = SessionId::new_v7();
        let other = SessionId::new_v7();
        let sibling = SessionId::new_v7();
        app.selected = Some(watched);
        app.tree_root = Some(watched);
        app.tree = Some(SessionTree {
            session: meta(watched, "watched root"),
            children: vec![
                SessionTree {
                    session: meta(other, "cursor child"),
                    children: Vec::new(),
                },
                SessionTree {
                    session: meta(sibling, "plain sibling"),
                    children: Vec::new(),
                },
            ],
        });
        // Park the keyboard cursor on the first child so the watched row
        // shows exactly its own styling, not the cursor's.
        app.tree_cursor = Some(other);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        let buffer = terminal.backend().buffer();
        assert_eq!(app.hit_map.tree_rows.len(), 3);
        let watched_row = app.hit_map.tree_rows[0].rect;
        let cursor_row = app.hit_map.tree_rows[1].rect;
        let reference_row = app.hit_map.tree_rows[2].rect;

        // The `●` glyph remains the only "current session" marker…
        let entries = app.tree_entries();
        assert!(app.tree_row_label(&entries[0], false).contains("● "));
        // …while the row renders exactly like every other plain row:
        // cell-for-cell identical styles (no toasted selection band, no
        // user-role color), and none of the bold/reverse modifiers the
        // removed accents carried.
        for x in watched_row.x..watched_row.x.saturating_add(watched_row.width) {
            let cell = buffer[(x, watched_row.y)].style();
            let reference = buffer[(x, reference_row.y)].style();
            assert_eq!(cell, reference, "same as every other row: {cell:?}");
            assert!(
                !cell.add_modifier.contains(Modifier::BOLD),
                "no bold accent: {cell:?}"
            );
            assert!(
                !cell.add_modifier.contains(Modifier::REVERSED),
                "no reverse accent: {cell:?}"
            );
        }
        // The keyboard cursor row keeps its assistant accent — keyboard
        // selection is a separate, intentional highlight.
        let cursor_cell = buffer[(cursor_row.x, cursor_row.y)].style();
        assert_eq!(
            cursor_cell.fg,
            app.theme.assistant().fg,
            "cursor accent: {cursor_cell:?}"
        );
    }

    #[tokio::test]
    async fn session_search_filters_titles_and_untitled_placeholder() {
        let mut app = test_app().await;
        let first = titled_meta(SessionId::new_v7(), "quarterly report", 1);
        let second = session_meta(SessionId::new_v7());
        app.sessions = vec![first, second];
        assert_eq!(
            app.current_session_search_rows()
                .iter()
                .filter(|row| row.session_id().is_some())
                .count(),
            2
        );
        app.session_search
            .input_mut()
            .set_buffer("quarterly".into());
        assert_eq!(
            app.current_session_search_rows()
                .iter()
                .filter(|row| row.session_id().is_some())
                .count(),
            1
        );
        app.session_search.input_mut().set_buffer("untitled".into());
        assert_eq!(
            app.current_session_search_rows()
                .iter()
                .filter(|row| row.session_id().is_some())
                .count(),
            1
        );
        app.session_search.input_mut().set_buffer("primary".into());
        assert!(
            app.current_session_search_rows()
                .iter()
                .all(|row| row.session_id().is_none())
        );
    }

    #[tokio::test]
    async fn session_search_headers_are_not_clickable_and_click_reroots() {
        let mut app = test_app().await;
        let first = SessionId::new_v7();
        let second = SessionId::new_v7();
        app.sessions = vec![
            titled_meta(first, "first session", 1),
            titled_meta(second, "second session", 1),
        ];
        app.modal = Modal::Sessions;
        frame_rows(&mut app, 100, 30);
        assert_eq!(app.hit_map.picker_rows.len(), 2);
        let picker = app.hit_map.picker.expect("picker");
        app.handle_wheel(picker.x + 1, picker.y + 1, false);
        assert_eq!(app.session_search.focus(), SearchPickerFocus::List);
        assert_eq!(app.picker_state.selected(), Some(1));
        let header_y = app.hit_map.picker_rows[0].rect.y.saturating_sub(1);
        let picker_x = picker.x + 1;
        app.handle_click(picker_x, header_y).await;
        assert_eq!(app.modal, Modal::Sessions);
        let hit = app.hit_map.picker_rows[1].rect;
        app.handle_click(hit.x, hit.y).await;
        assert_eq!(app.modal, Modal::None);
        assert_eq!(app.tree_root, Some(second));
    }

    #[tokio::test]
    async fn session_search_enter_selects_and_live_title_patch_updates_open_overlay() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        app.sessions = vec![titled_meta(session, "before rename", 1)];
        app.modal = Modal::Sessions;
        let before = frame_rows(&mut app, 100, 30).join("\n");
        assert!(before.contains("before rename"));
        app.apply_title_patch(
            session,
            Some(SessionTitle::new("after rename").expect("title")),
            2,
        );
        let after = frame_rows(&mut app, 100, 30).join("\n");
        assert!(after.contains("after rename"));
        assert!(!after.contains("before rename"));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.session_search.focus(), SearchPickerFocus::List);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::None);
        assert_eq!(app.tree_root, Some(session));
    }

    #[tokio::test]
    async fn agent_tree_status_icons_keep_row_hit_geometry_intact() {
        let mut app = test_app().await;
        let statuses = [
            (SessionStatus::Running, "⏳ "),
            (SessionStatus::Idle, "✅ "),
            (SessionStatus::Completed, "✅ "),
            (SessionStatus::Failed, "   "),
            (SessionStatus::Cancelled, "   "),
            (SessionStatus::Interrupted, "   "),
        ];
        let root = SessionId::new_v7();
        let mut root_meta = titled_meta(root, "root", 1);
        root_meta.status = statuses[0].0;
        app.tree = Some(SessionTree {
            session: root_meta,
            children: statuses[1..]
                .iter()
                .enumerate()
                .map(|(index, (status, _))| {
                    let mut meta = titled_meta(SessionId::new_v7(), &format!("child {index}"), 1);
                    meta.status = *status;
                    SessionTree {
                        session: meta,
                        children: Vec::new(),
                    }
                })
                .collect(),
        });
        let entries = app.tree_entries();
        for (entry, (_, icon)) in entries.iter().zip(statuses) {
            assert!(app.tree_row_label(entry, false).contains(icon));
        }
        frame_rows(&mut app, 80, 30);
        assert_eq!(app.hit_map.tree_rows.len(), statuses.len());
        assert!(
            app.hit_map
                .tree_rows
                .iter()
                .all(|hit| hit.rect.width == app.hit_map.tree.expect("tree rect").width)
        );
    }

    #[tokio::test]
    async fn run_lifecycle_events_patch_watched_and_background_panel_statuses_without_tree_rpc() {
        let (client, requests) = recording_client();
        let mut app = App::new(client).await.expect("test app");
        let watched = SessionId::new_v7();
        let background = SessionId::new_v7();
        app.selected = Some(watched);
        app.tree_root = Some(watched);
        app.sessions = vec![session_meta(watched), session_meta(background)];
        app.tree = Some(SessionTree {
            session: session_meta(watched),
            children: vec![SessionTree {
                session: session_meta(background),
                children: Vec::new(),
            }],
        });
        assert!(app.store.apply_event(session_created(watched, 1)));
        assert!(app.store.apply_event(session_created(background, 1)));
        requests.lock().expect("requests lock").clear();

        for session_id in [watched, background] {
            let run = run_id();
            app.handle_delivery(ClientDelivery::Live {
                message: Box::new(cookie_agent_protocol::EventSubscriptionMessage::Event {
                    event: Box::new(run_started_with_suffix(
                        session_id,
                        2,
                        run,
                        vec![resolved_model(None)],
                    )),
                }),
                generation: 0,
            })
            .await;
            assert_eq!(
                app.sessions
                    .iter()
                    .find(|meta| meta.session_id == session_id)
                    .expect("session list meta")
                    .status,
                SessionStatus::Running
            );
            let running_entry = app
                .tree_entries()
                .into_iter()
                .find(|entry| entry.0 == session_id)
                .expect("tree meta");
            assert_eq!(running_entry.1.status, SessionStatus::Running);
            assert!(app.tree_row_label(&running_entry, false).contains("⏳ "));

            app.handle_delivery(ClientDelivery::Live {
                message: Box::new(cookie_agent_protocol::EventSubscriptionMessage::Event {
                    event: Box::new(event(
                        session_id,
                        3,
                        run,
                        EventPayload::RunCompleted { final_text: None },
                    )),
                }),
                generation: 0,
            })
            .await;
            assert_eq!(
                app.sessions
                    .iter()
                    .find(|meta| meta.session_id == session_id)
                    .expect("session list meta")
                    .status,
                SessionStatus::Completed
            );
            let completed_entry = app
                .tree_entries()
                .into_iter()
                .find(|entry| entry.0 == session_id)
                .expect("tree meta");
            assert_eq!(completed_entry.1.status, SessionStatus::Completed);
            assert!(app.tree_row_label(&completed_entry, false).contains("✅ "));
        }

        let merged = app.merge_session_meta(session_meta(watched));
        assert_eq!(merged.last_event_seq, 3);
        assert_eq!(merged.status, SessionStatus::Completed);
        let mut stale_tree = SessionTree {
            session: session_meta(watched),
            children: vec![SessionTree {
                session: session_meta(background),
                children: Vec::new(),
            }],
        };
        app.patch_tree_titles(&mut stale_tree);
        assert_eq!(stale_tree.session.status, SessionStatus::Completed);
        assert_eq!(
            stale_tree.children[0].session.status,
            SessionStatus::Completed
        );

        tokio::task::yield_now().await;
        assert_eq!(recorded_method_count(&requests, "session.tree"), 0);
    }

    #[tokio::test]
    async fn tree_panel_hides_empty_sessions_until_their_first_message_lands() {
        let mut app = test_app().await;
        let root = SessionId::new_v7();
        let worker = SessionId::new_v7();
        let ghost = SessionId::new_v7();
        let mut ghost_meta = session_meta(ghost);
        // Only `SessionCreated` in the log: no user message yet.
        ghost_meta.last_event_seq = 1;
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![
                SessionTree {
                    session: session_meta(worker),
                    children: Vec::new(),
                },
                SessionTree {
                    session: ghost_meta,
                    children: Vec::new(),
                },
            ],
        });

        // The ghost renders nowhere: flattened entries and click hit rows
        // skip it while content sessions keep their order.
        assert_eq!(
            app.tree_entries()
                .iter()
                .map(|(id, _, _)| *id)
                .collect::<Vec<_>>(),
            vec![root, worker]
        );
        frame_rows(&mut app, 100, 30);
        assert_eq!(
            app.hit_map
                .tree_rows
                .iter()
                .map(|hit| hit.session_id)
                .collect::<Vec<_>>(),
            vec![root, worker]
        );

        // Its first run event bumps the sequence and the row appears.
        app.apply_status_patch(ghost, SessionStatus::Running, 2);
        assert_eq!(
            app.tree_entries()
                .iter()
                .map(|(id, _, _)| *id)
                .collect::<Vec<_>>(),
            vec![root, worker, ghost]
        );

        // A tree of ghosts only renders the panel's empty state, never rows.
        let mut solo = session_meta(root);
        solo.last_event_seq = 1;
        app.tree = Some(SessionTree {
            session: solo,
            children: Vec::new(),
        });
        assert!(app.tree_entries().is_empty());
        let rendered = frame_rows(&mut app, 100, 30).join("\n");
        assert!(rendered.contains("No sessions yet"));
    }

    #[tokio::test]
    async fn tree_navigation_skips_hidden_sessions_and_heals_a_hidden_cursor() {
        let mut app = test_app().await;
        let root = SessionId::new_v7();
        let first = SessionId::new_v7();
        let ghost = SessionId::new_v7();
        let last = SessionId::new_v7();
        let mut ghost_meta = session_meta(ghost);
        ghost_meta.last_event_seq = 1;
        app.tree = Some(SessionTree {
            session: session_meta(root),
            children: vec![
                SessionTree {
                    session: session_meta(first),
                    children: Vec::new(),
                },
                SessionTree {
                    session: ghost_meta,
                    children: Vec::new(),
                },
                SessionTree {
                    session: session_meta(last),
                    children: Vec::new(),
                },
            ],
        });

        // Navigation walks root → first → last and never lands on the ghost.
        app.tree_cursor = Some(root);
        app.move_tree_selection(false);
        assert_eq!(app.tree_cursor, Some(first));
        app.move_tree_selection(false);
        assert_eq!(app.tree_cursor, Some(last));
        app.move_tree_selection(false);
        assert_eq!(app.tree_cursor, Some(last));
        app.move_tree_selection(true);
        assert_eq!(app.tree_cursor, Some(first));

        // A cursor left pointing at a hidden session — the just-created
        // watched session before its first message — never navigates onto
        // the ghost and heals onto the first visible row at render.
        app.tree_cursor = Some(ghost);
        app.move_tree_selection(true);
        assert_eq!(app.tree_cursor, Some(root));
        app.tree_cursor = Some(ghost);
        frame_rows(&mut app, 100, 30);
        assert_eq!(app.tree_cursor, Some(root));
    }

    #[tokio::test]
    async fn hidden_current_session_stays_fully_usable() {
        let mut app = test_app().await;
        let current = SessionId::new_v7();
        let mut meta = session_meta(current);
        meta.last_event_seq = 1;
        app.selected = Some(current);
        app.tree_root = Some(current);
        app.tree = Some(SessionTree {
            session: meta,
            children: Vec::new(),
        });
        app.tree_cursor = Some(current);
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
            preset: None,
        });

        let rendered = frame_rows(&mut app, 100, 30).join("\n");
        // The panel shows its empty state with no ghost row or hit region…
        assert!(app.tree_entries().is_empty());
        assert!(app.hit_map.tree_rows.is_empty());
        assert!(rendered.contains("No sessions yet"));
        // …while the composer and the Message title bar keep working.
        assert!(app.hit_map.input.is_some());
        assert!(!app.hit_map.title_segments.is_empty());
        assert_eq!(app.selected, Some(current));
    }

    #[test]
    fn run_terminal_status_patches_match_engine_session_projection() {
        let session = SessionId::new_v7();
        let run = run_id();
        let cases = [
            (
                EventPayload::RunCompleted { final_text: None },
                SessionStatus::Completed,
            ),
            (
                EventPayload::RunFailed {
                    error: SafeErrorMessage::new("failed").expect("error"),
                },
                SessionStatus::Failed,
            ),
            (
                EventPayload::RunCancelled { reason: None },
                SessionStatus::Cancelled,
            ),
            (
                EventPayload::RunInterrupted { reason: None },
                SessionStatus::Interrupted,
            ),
        ];
        for (payload, expected) in cases {
            assert_eq!(
                status_change_from_event(&event(session, 2, run, payload)),
                Some((session, expected, 2))
            );
        }
        assert_eq!(
            status_change_from_event(&runless_event(
                session,
                3,
                EventPayload::DelegateChildTerminated {
                    status: SessionStatus::Cancelled,
                    reason: None,
                },
            )),
            Some((session, SessionStatus::Cancelled, 3))
        );
    }

    #[tokio::test]
    async fn runless_delegate_terminal_event_patches_live_session_and_tree_status() {
        let (client, _requests) = recording_client();
        let mut app = App::new(client).await.expect("test app");
        let child = SessionId::new_v7();
        app.sessions = vec![session_meta(child)];
        app.tree_root = Some(child);
        app.tree = Some(SessionTree {
            session: session_meta(child),
            children: Vec::new(),
        });
        assert!(app.store.apply_event(session_created(child, 1)));

        app.handle_delivery(live_event(runless_event(
            child,
            2,
            EventPayload::DelegateChildTerminated {
                status: SessionStatus::Cancelled,
                reason: None,
            },
        )))
        .await;

        assert_eq!(app.sessions[0].status, SessionStatus::Cancelled);
        assert_eq!(
            app.tree.as_ref().expect("tree").session.status,
            SessionStatus::Cancelled
        );
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
                root_selected[0].starts_with("> ● ✅"),
                "width {width}: {root_selected:?}"
            );
            assert!(root_selected[1].starts_with("  -   ✅"));
            assert!(root_selected[2].starts_with("        ✅"));
            assert!(child_selected[0].starts_with("    ✅"));
            assert!(child_selected[1].starts_with("> - ● ✅"));
            assert!(child_selected[2].starts_with("        ✅"));
            assert!(root_selected_again[1].starts_with("  -   ✅"));
            assert!(root_selected_again[2].starts_with("        ✅"));

            // These columns come from the actual rendered buffer. Selection
            // changes cursor/watch cells only; agent text retains depth 0/1/2.
            assert_eq!(text_column(&root_selected[0], "p"), 7);
            assert_eq!(text_column(&root_selected[1], "p"), 9);
            assert_eq!(text_column(&root_selected[2], "p"), 11);
            assert_eq!(text_column(&child_selected[0], "p"), 7);
            assert_eq!(text_column(&child_selected[1], "p"), 9);
            assert_eq!(text_column(&child_selected[2], "p"), 11);
            assert_eq!(text_column(&root_selected_again[0], "p"), 7);
            assert_eq!(text_column(&root_selected_again[1], "p"), 9);
            assert_eq!(text_column(&root_selected_again[2], "p"), 11);
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
            preset: None,
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
    async fn message_title_bolds_only_the_agent_name() {
        let mut app = test_app().await;
        // Pin the default true-color theme so style assertions do not
        // depend on the developer's ambient tui.toml or terminal detection.
        app.theme = Theme::default();
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![model_descriptor()];
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: Some(cookie_agent_protocol::VariantId::new("high").expect("variant")),
            },
            preset: None,
        });
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        let buffer = terminal.backend().buffer();
        let segment_rect = |segment: TitleSegment| {
            app.hit_map
                .title_segments
                .iter()
                .find(|hit| hit.segment == segment)
                .expect("title segment")
                .rect
        };
        let agent = segment_rect(TitleSegment::Agent);
        let model = segment_rect(TitleSegment::Model);
        let variant = segment_rect(TitleSegment::Variant);
        // The bullet is decoration between the agent and model segments.
        let bullet = Rect::new(
            agent.x.saturating_add(agent.width),
            agent.y,
            model.x.saturating_sub(agent.x.saturating_add(agent.width)),
            1,
        );

        // The focused composer's border is bold honey; only the agent
        // name keeps that weight. Everything in the title shares the border
        // accent color — bold is the emphasis, never a color marker.
        for x in agent.x..agent.x.saturating_add(agent.width) {
            let cell = buffer[(x, agent.y)].style();
            assert!(
                cell.add_modifier.contains(Modifier::BOLD),
                "agent name is bold: {cell:?}"
            );
            assert_eq!(
                cell.fg,
                app.theme.input_border(true).fg,
                "border accent: {cell:?}"
            );
        }
        for rect in [bullet, model, variant] {
            for x in rect.x..rect.x.saturating_add(rect.width) {
                let cell = buffer[(x, rect.y)].style();
                assert!(
                    !cell.add_modifier.contains(Modifier::BOLD),
                    "segment stays regular: {cell:?}"
                );
                assert_eq!(
                    cell.fg,
                    app.theme.input_border(true).fg,
                    "shared color: {cell:?}"
                );
            }
        }
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
                preset: None,
            });
            app.input
                .set_buffer("zero\none\ntwo\nthree\nfour\nfive\nsix".into());
            frame_rows(&mut app, width, 24);
            match position {
                0 => app.input.move_buffer_home(),
                1 => {
                    app.input.move_buffer_home();
                    for _ in 0..3 {
                        app.input.move_down();
                    }
                }
                2 => {}
                _ => unreachable!(),
            }
            // The seven-line draft grows the composer to its five-text-row
            // ceiling, so the viewport only scrolls once the cursor passes
            // row four: positions land at rows 0, 3, and 6.
            assert_eq!(app.input.viewport_row(), [0, 0, 2][position]);
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
            // Mirror draw(): the composer's text-row demand comes from its
            // actual content at the frame width, not a fixed height.
            let input_text_rows = u16::try_from(app.input.content_rows(28 - 2))
                .unwrap_or(u16::MAX)
                .clamp(1, crate::ui::input::MAX_TEXT_ROWS);
            let title_y = terminal_layout_with_tree_rows(
                Rect::new(0, 0, 28, 24),
                app.tree_entries().len(),
                0,
                input_text_rows,
            )
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
            preset: None,
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
            preset: None,
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
                // Narrow panels ellipsize rows instead of hard-clipping
                // mid-word: the cut point always ends in an ellipsis.
                assert!(
                    rows.iter()
                        .any(|row| row.contains("other/catalog-model") && row.contains('…'))
                );
                assert!(
                    rows.iter()
                        .any(|row| row.contains("gateway/arbitrary") && row.contains('…'))
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
    async fn composer_variant_hit_cycles_in_declared_order_wraps_and_one_entry_is_noop() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![catalog_model(MODEL, &["high", "default", "fast"], None)];
        app.draft = app.default_draft_selection();

        for expected in [Some("high"), Some("default"), Some("fast"), None] {
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

        app.models = vec![catalog_model(MODEL, &["high", "default", "fast"], None)];
        app.models[0].variant_order =
            vec![cookie_agent_protocol::VariantId::new("high").expect("variant")];
        app.revalidate_draft();
        assert_eq!(
            app.draft_variants()
                .iter()
                .map(|variant| variant.as_ref().map(|id| id.as_str()))
                .collect::<Vec<_>>(),
            vec![None, Some("default"), Some("fast"), Some("high")],
            "descriptor drift falls back to lexical order"
        );
    }

    #[tokio::test]
    async fn composer_variant_hit_cycles_k3_base_low_high_max() {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![catalog_model(
            "kimi-for-coding/k3",
            &["low", "high", "max"],
            None,
        )];
        app.draft = app.default_draft_selection();

        assert_eq!(
            app.draft_variants()
                .iter()
                .map(|variant| variant.as_ref().map(|id| id.as_str()))
                .collect::<Vec<_>>(),
            vec![None, Some("low"), Some("high"), Some("max")]
        );
        for expected in [Some("low"), Some("high"), Some("max"), None] {
            app.cycle_draft_variant();
            assert_eq!(
                app.draft
                    .as_ref()
                    .and_then(|draft| draft.model.variant.as_ref())
                    .map(|variant| variant.as_str()),
                expected
            );
        }
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
            preset: None,
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
        let mut internal = descriptor("approval", true);
        internal.mode = cookie_agent_protocol::AgentMode::Internal;
        app.agents = vec![
            descriptor("primary", true),
            descriptor("worker", false),
            internal,
        ];
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
                preset: None,
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
            preset: None,
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
            preset: None,
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
            preset: None,
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
            preset: None,
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
                    max_output_tokens: 0,
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

    /// A composer draft overflowing the ceiling-height box, rendered once so
    /// the hit map and scrollbar geometry exist.
    async fn app_with_overflowing_composer() -> (App, super::ScrollbarGeometry) {
        let mut app = test_app().await;
        app.handle_paste("a\nb\nc\nd\ne\nf\ng\nh");
        rendered_frame(&mut app, 80, 50);
        let geometry = app
            .hit_map
            .input
            .expect("input hit")
            .scrollbar
            .expect("composer scrollbar at the overflowing ceiling");
        assert_eq!(geometry.track.width, 1);
        (app, geometry)
    }

    #[tokio::test]
    async fn composer_scrollbar_drag_scrolls_without_moving_the_text_cursor() {
        let (mut app, geometry) = app_with_overflowing_composer().await;
        let cursor_before = app.input.cursor_byte();
        // The cursor anchors the bottom of the draft, so the thumb rests at
        // the bottom of the track.
        assert!(app.input.viewport_row() > 0);
        let press = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(press(
            MouseEventKind::Down(MouseButton::Left),
            geometry.thumb.x,
            geometry.thumb.y,
        ))
        .await;
        assert!(
            matches!(app.scrollbar_drag, Some(drag) if drag.target == ScrollbarTarget::Input),
            "thumb press captures an input drag: {:?}",
            app.scrollbar_drag
        );
        // Dragging the thumb to the top of the track scrolls the composer…
        app.handle_mouse(press(
            MouseEventKind::Drag(MouseButton::Left),
            geometry.track.x,
            geometry.track.y,
        ))
        .await;
        rendered_frame(&mut app, 80, 50);
        assert_eq!(app.input.viewport_row(), 0);
        // …while the text cursor never moves.
        assert_eq!(app.input.cursor_byte(), cursor_before);
        // Releasing the press ends the capture.
        app.handle_mouse(press(
            MouseEventKind::Up(MouseButton::Left),
            geometry.track.x,
            geometry.track.y,
        ))
        .await;
        assert!(app.scrollbar_drag.is_none());
    }

    #[tokio::test]
    async fn composer_scrollbar_drag_stays_captured_outside_the_track() {
        let (mut app, geometry) = app_with_overflowing_composer().await;
        let event = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(event(
            MouseEventKind::Down(MouseButton::Left),
            geometry.thumb.x,
            geometry.thumb.y,
        ))
        .await;
        // The pointer wanders far into the conversation pane; the captured
        // drag keeps its anchor and clamps against the original geometry.
        app.handle_mouse(event(MouseEventKind::Drag(MouseButton::Left), 0, 0))
            .await;
        rendered_frame(&mut app, 80, 50);
        assert_eq!(app.input.viewport_row(), 0);
        assert!(
            matches!(app.scrollbar_drag, Some(drag) if drag.target == ScrollbarTarget::Input),
            "capture survives leaving the track"
        );
        app.handle_mouse(event(MouseEventKind::Up(MouseButton::Left), 0, 0))
            .await;
        assert!(app.scrollbar_drag.is_none());
    }

    #[tokio::test]
    async fn composer_scrollbar_track_press_pages_the_viewport() {
        let (mut app, geometry) = app_with_overflowing_composer().await;
        let cursor_before = app.input.cursor_byte();
        assert!(app.input.viewport_row() > 0);
        // Bare track above the thumb pages the viewport toward that offset
        // without capturing a drag or moving the text cursor.
        app.handle_click(geometry.track.x, geometry.track.y).await;
        assert_eq!(app.input.viewport_row(), 0);
        assert!(app.scrollbar_drag.is_none());
        assert_eq!(app.input.cursor_byte(), cursor_before);
    }

    #[tokio::test]
    async fn composer_scrollbar_hold_reanchors_on_the_next_edit() {
        let (mut app, geometry) = app_with_overflowing_composer().await;
        app.handle_click(geometry.track.x, geometry.track.y).await;
        rendered_frame(&mut app, 80, 50);
        assert_eq!(app.input.viewport_row(), 0);
        // The next edit ends the hold: the viewport follows the cursor back
        // to the bottom of the draft.
        app.handle_paste("x");
        rendered_frame(&mut app, 80, 50);
        assert_eq!(app.input.viewport_row(), 3);
    }

    #[tokio::test]
    async fn composer_wheel_still_scrolls_the_overflowing_viewport() {
        let (mut app, geometry) = app_with_overflowing_composer().await;
        let wheel = |kind| MouseEvent {
            kind,
            column: geometry.track.x - 1,
            row: geometry.track.y,
            modifiers: KeyModifiers::NONE,
        };
        // The wheel keeps its existing composer semantics: it walks the text
        // cursor three visual rows per tick, and the viewport follows.
        assert_eq!(app.input.viewport_row(), 3);
        for _ in 0..3 {
            app.handle_mouse(wheel(MouseEventKind::ScrollUp)).await;
        }
        rendered_frame(&mut app, 80, 50);
        assert_eq!(app.input.viewport_row(), 0);
        for _ in 0..3 {
            app.handle_mouse(wheel(MouseEventKind::ScrollDown)).await;
        }
        rendered_frame(&mut app, 80, 50);
        assert_eq!(app.input.viewport_row(), 3);
    }

    // ------------------------------------------------------------------
    // Pending-input lane: the strip between transcript and composer
    // ------------------------------------------------------------------

    /// An app with one selected session whose run is active, so submitting
    /// takes the steer path.
    async fn app_with_active_run() -> (App, SessionId, RunId) {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        let run = run_id();
        app.selected = Some(session);
        app.store.sessions.insert(
            session,
            SessionState {
                active_run: Some(run),
                run_agent: Some(agent_id()),
                initial_input_submitted: HashSet::from([run]),
                ..SessionState::default()
            },
        );
        (app, session, run)
    }

    /// A live delivery carrying one event, the path subscription echoes
    /// take through `handle_delivery`.
    fn live_event(event: StoredEvent) -> ClientDelivery {
        ClientDelivery::Live {
            message: Box::new(EventSubscriptionMessage::Event {
                event: Box::new(event),
            }),
            generation: 0,
        }
    }

    fn admitted(session: SessionId, seq: u64, run: RunId, input: &str) -> StoredEvent {
        event(
            session,
            seq,
            run,
            EventPayload::UserInputAdmitted {
                input: input.into(),
            },
        )
    }

    fn recalled(session: SessionId, seq: u64, run: RunId, input: &str) -> StoredEvent {
        event(
            session,
            seq,
            run,
            EventPayload::UserInputRecalled {
                input: input.into(),
            },
        )
    }

    fn user_input(session: SessionId, seq: u64, run: RunId, input: &str) -> StoredEvent {
        event(
            session,
            seq,
            run,
            EventPayload::UserInputSubmitted {
                input: input.into(),
            },
        )
    }

    fn pending_texts(app: &App, session: SessionId) -> Vec<&str> {
        app.store
            .sessions
            .get(&session)
            .map(|state| {
                state
                    .pending_inputs
                    .iter()
                    .map(|pending| pending.text.as_str())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Poll the recording sink until `count` requests of `method` have been
    /// sent, returning the newest request's JSON-RPC id.
    async fn wait_for_recorded_request(
        recorded: &Arc<Mutex<Vec<Value>>>,
        method: &str,
        count: usize,
    ) -> i64 {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let found = recorded
                    .lock()
                    .expect("recorded")
                    .iter()
                    .filter(|value| value["method"].as_str() == Some(method))
                    .count();
                if found >= count {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("recorded request timeout");
        recorded
            .lock()
            .expect("recorded")
            .iter()
            .rfind(|value| value["method"].as_str() == Some(method))
            .and_then(|value| value["id"].as_i64())
            .expect("request id")
    }

    async fn drive_until_recorded_request(
        app: &mut App,
        recorded: &Arc<Mutex<Vec<Value>>>,
        method: &str,
        count: usize,
    ) -> i64 {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if recorded_method_count(recorded, method) >= count {
                    break;
                }
                tokio::select! {
                    update = app.rpc_updates_rx.recv() => {
                        app.handle_rpc_update(update.expect("RPC update channel"));
                    }
                    () = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        })
        .await
        .expect("recorded request timeout");
        recorded
            .lock()
            .expect("recorded")
            .iter()
            .rfind(|value| value["method"].as_str() == Some(method))
            .and_then(|value| value["id"].as_i64())
            .expect("request id")
    }

    #[tokio::test]
    async fn pending_lane_tracks_admit_promote_and_recall_events() {
        let (mut app, session, run) = app_with_active_run().await;
        for (seq, input) in ["alpha", "beta", "gamma"].iter().enumerate() {
            assert!(
                app.store
                    .apply_event(admitted(session, seq as u64 + 1, run, input))
            );
        }
        assert_eq!(pending_texts(&app, session), ["alpha", "beta", "gamma"]);
        assert_eq!(app.queue_strip_height(), 5);

        // Promotion removes the oldest lane entry positionally and renders
        // the user row exactly once, as it always has.
        app.handle_delivery(live_event(user_input(session, 4, run, "alpha")))
            .await;
        assert_eq!(pending_texts(&app, session), ["beta", "gamma"]);
        let rendered: Vec<&str> = app.store.sessions[&session]
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(rendered, ["alpha"]);

        // Recall removes the newest entry positionally.
        app.handle_delivery(live_event(recalled(session, 5, run, "gamma")))
            .await;
        assert_eq!(pending_texts(&app, session), ["beta"]);

        // The reduction never consults payload text — promotion pops the
        // front, recall pops the back, mirroring the engine's own replay
        // exactly. A payload that names no lane entry still withdraws the
        // newest one, so nothing the engine says is gone can be stranded.
        app.handle_delivery(live_event(recalled(session, 6, run, "ghost")))
            .await;
        assert!(pending_texts(&app, session).is_empty());
        assert_eq!(app.queue_strip_height(), 0);
    }

    #[tokio::test]
    async fn recall_ignores_payload_text_and_pops_the_newest_entry() {
        let (mut app, session, run) = app_with_active_run().await;
        assert!(app.store.apply_event(admitted(session, 1, run, "alpha")));
        assert!(app.store.apply_event(admitted(session, 2, run, "beta")));
        // The recalled payload names the OLDEST entry; positional replay
        // still withdraws the newest, exactly like the engine.
        app.handle_delivery(live_event(recalled(session, 3, run, "alpha")))
            .await;
        assert_eq!(pending_texts(&app, session), ["alpha"]);
        // A promotion payload naming the newest entry still graduates the
        // oldest.
        app.handle_delivery(live_event(user_input(session, 4, run, "anything")))
            .await;
        assert!(pending_texts(&app, session).is_empty());
    }

    #[tokio::test]
    async fn duplicate_texts_resolve_by_position_not_identity() {
        let (mut app, session, run) = app_with_active_run().await;
        assert!(app.store.apply_event(admitted(session, 1, run, "same")));
        assert!(app.store.apply_event(admitted(session, 2, run, "same")));
        // Promotion takes the oldest duplicate; recall takes the newest.
        app.handle_delivery(live_event(user_input(session, 3, run, "same")))
            .await;
        assert_eq!(pending_texts(&app, session), ["same"]);
        app.handle_delivery(live_event(recalled(session, 4, run, "same")))
            .await;
        assert!(pending_texts(&app, session).is_empty());
    }

    #[tokio::test]
    async fn submit_sends_steer_and_the_strip_waits_for_admission() {
        let (mut app, session, _run) = app_with_active_run().await;
        app.submit_prompt("hold on".into()).await;
        // No optimistic entry: the strip derives from engine events only.
        assert!(pending_texts(&app, session).is_empty());
        assert_eq!(app.queue_strip_height(), 0);
        assert!(app.input.as_str().is_empty());
    }

    #[tokio::test]
    async fn pending_lane_rebuilds_from_replay_alone() {
        let (mut app, session, run) = app_with_active_run().await;
        // A rebuild replay derives the lane purely from events: no client
        // state survives or is consulted.
        app.handle_delivery(ClientDelivery::ReplayStart {
            session_id: session,
            generation: 0,
            final_seq: 2,
            rebuild: true,
        })
        .await;
        for seq in 1..=2 {
            app.handle_delivery(ClientDelivery::ReplayEvent {
                session_id: session,
                generation: 0,
                final_seq: 2,
                event: Box::new(admitted(session, seq, run, &format!("m{seq}"))),
            })
            .await;
        }
        app.handle_delivery(ClientDelivery::ReplayEnd {
            session_id: session,
            generation: 0,
            final_seq: 2,
        })
        .await;
        assert_eq!(pending_texts(&app, session), ["m1", "m2"]);
        assert_eq!(app.queue_strip_height(), 4);
    }

    #[tokio::test]
    async fn run_end_voids_pending_and_restores_the_composer() {
        let (mut app, session, run) = app_with_active_run().await;
        assert!(app.store.apply_event(admitted(session, 1, run, "first")));
        assert!(app.store.apply_event(admitted(session, 2, run, "second")));
        app.handle_delivery(live_event(event(
            session,
            3,
            run,
            EventPayload::RunCompleted { final_text: None },
        )))
        .await;
        // The engine voided the lane without per-entry events: the strip
        // clears and the text returns to the composer, FIFO order intact.
        assert!(pending_texts(&app, session).is_empty());
        assert_eq!(app.input.as_str(), "first\nsecond");
        assert!(app.status.contains("restored to the composer"));
    }

    #[tokio::test]
    async fn run_end_in_a_background_session_restores_on_select() {
        let (mut app, session_a, _run_a) = app_with_active_run().await;
        let session_b = SessionId::new_v7();
        let run_b = run_id();
        app.store.sessions.insert(
            session_b,
            SessionState {
                active_run: Some(run_b),
                run_agent: Some(agent_id()),
                ..SessionState::default()
            },
        );
        assert!(
            app.store
                .apply_event(admitted(session_b, 1, run_b, "for b"))
        );
        app.handle_delivery(live_event(event(
            session_b,
            2,
            run_b,
            EventPayload::RunCancelled { reason: None },
        )))
        .await;
        // The composer belongs to session A right now: B's text is parked,
        // not leaked, and its strip is cleared.
        assert!(app.input.as_str().is_empty());
        assert!(pending_texts(&app, session_b).is_empty());
        app.set_selected_session(session_b);
        assert_eq!(app.input.as_str(), "for b");
        let _ = session_a;
    }

    #[tokio::test]
    async fn steer_transport_failure_restores_the_submitted_text() {
        let (mut app, session, _run) = app_with_active_run().await;
        app.input.set_buffer("next draft".into());
        app.handle_rpc_update(RpcUpdate::SteerFailed {
            session_id: session,
            input: "keep me".into(),
            error: "transport closed".into(),
        });
        assert_eq!(app.input.as_str(), "keep me\nnext draft");
        assert!(app.status.contains("restored to the composer"));
    }

    #[tokio::test]
    async fn clicking_a_strip_entry_recalls_and_restores_the_returned_text() {
        // Startup uses the short-lived recording client; the live client
        // swaps in afterwards so the recall RPC can be answered.
        let (startup_client, _startup) = recording_client();
        let mut app = App::new(startup_client).await.expect("test app");
        let (client, recorded, incoming) = live_recording_client();
        app.client = client;
        app.install_initial_runtime(runtime_snapshot(
            "1",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        ));
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
        assert!(app.store.apply_event(admitted(session, 1, run, "first")));
        assert!(app.store.apply_event(admitted(session, 2, run, "second")));
        rendered_frame(&mut app, 80, 24);
        let hit = app.hit_map.queue_entries[0];
        app.handle_click(hit.rect.x, hit.rect.y).await;
        // Any row click recalls the newest pending input.
        let id = wait_for_recorded_request(&recorded, "run.recall_steer", 1).await;
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "recalled": "second" }
            })))
            .expect("script recall response");
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("recall update timeout")
            .expect("recall update");
        app.handle_rpc_update(update);
        assert_eq!(app.input.as_str(), "second");
        assert!(app.status.contains("recalled message restored"));
        // The recalled event removes the entry from the strip itself.
        app.handle_delivery(live_event(recalled(session, 3, run, "second")))
            .await;
        assert_eq!(pending_texts(&app, session), ["first"]);
    }

    #[tokio::test]
    async fn recall_reports_when_the_engine_lane_is_already_empty() {
        let (startup_client, _startup) = recording_client();
        let mut app = App::new(startup_client).await.expect("test app");
        let (client, recorded, incoming) = live_recording_client();
        app.client = client;
        app.install_initial_runtime(runtime_snapshot(
            "1",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        ));
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
        assert!(app.store.apply_event(admitted(session, 1, run, "raced")));
        app.recall_newest_pending();
        let id = wait_for_recorded_request(&recorded, "run.recall_steer", 1).await;
        // A promotion raced the recall: the engine has nothing to withdraw.
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "recalled": null }
            })))
            .expect("script empty recall response");
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("recall update timeout")
            .expect("recall update");
        app.handle_rpc_update(update);
        assert!(app.input.as_str().is_empty());
        assert!(app.status.contains("nothing pending to recall"));
    }

    #[tokio::test]
    async fn press_same_frame_as_overlay_arrival_hits_nothing_underneath() {
        let (mut app, session, run) = app_with_active_run().await;
        assert!(app.store.apply_event(admitted(session, 1, run, "first")));
        assert!(app.store.apply_event(admitted(session, 2, run, "second")));
        rendered_frame(&mut app, 80, 24);
        let hit = app.hit_map.queue_entries[0];
        // An approval arrives AFTER the frame was rendered: state knows the
        // panel, the hit map does not. A press landing where a queue entry
        // was must be swallowed by the panel's ownership, not leak through
        // to the recall action underneath.
        app.store
            .sessions
            .entry(session)
            .or_default()
            .approvals
            .push(approval(session));
        assert!(app.current_approval().is_some());
        let status_before = app.status.clone();
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            hit.rect.x,
            hit.rect.y,
        ))
        .await;
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            hit.rect.x,
            hit.rect.y,
        ))
        .await;
        assert_eq!(app.modal, Modal::None, "nothing underneath opened");
        assert_eq!(
            pending_texts(&app, session),
            ["first", "second"],
            "no recall fired underneath the panel"
        );
        assert_eq!(app.status, status_before, "no content action ran");
        assert!(
            app.current_approval().is_some(),
            "the approval was not answered either"
        );
        // Hover is state-owned the same way: no content target shows
        // through the not-yet-rendered panel.
        assert!(app.hover_target_at(hit.rect.x, hit.rect.y).is_none());
    }

    #[tokio::test]
    async fn up_in_an_empty_composer_recalls_instead_of_moving_the_cursor() {
        let (startup_client, _startup) = recording_client();
        let mut app = App::new(startup_client).await.expect("test app");
        let (client, recorded, _incoming) = live_recording_client();
        app.client = client;
        app.install_initial_runtime(runtime_snapshot(
            "1",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        ));
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
        assert!(app.store.apply_event(admitted(session, 1, run, "pending")));
        app.input_focused = true;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        wait_for_recorded_request(&recorded, "run.recall_steer", 1).await;
        // With text in the composer, Up keeps its plain cursor semantics.
        app.input.set_buffer("draft".into());
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        assert_eq!(recorded_method_count(&recorded, "run.recall_steer"), 1);
    }

    #[tokio::test]
    async fn strip_entries_highlight_on_hover() {
        let (mut app, session, run) = app_with_active_run().await;
        assert!(app.store.apply_event(admitted(session, 1, run, "first")));
        assert!(app.store.apply_event(admitted(session, 2, run, "second")));
        rendered_frame(&mut app, 80, 24);
        let second = app.hit_map.queue_entries[1].rect;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: second.x,
            row: second.y,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.hover, Some(HoverTarget::QueueEntry(1)));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("render");
        let buffer = terminal.backend().buffer();
        let first_row = app.hit_map.queue_entries[0].rect;
        let hovered = buffer[(second.x + 2, second.y)].style();
        let plain = buffer[(first_row.x + 2, first_row.y)].style();
        // The hover patch merges over the row: the glaze background lands
        // while the muted foreground stays.
        assert_eq!(hovered.bg, app.theme.hover().bg);
        assert_ne!(plain.bg, app.theme.hover().bg);
    }

    #[test]
    fn queue_strip_reclaims_conversation_rows_and_keeps_status_pinned() {
        let area = Rect::new(0, 0, 80, 24);
        let plain = terminal_layout_with_tree_rows(area, 3, 0, 1);
        let queued = terminal_layout_with_tree_rows(area, 3, 5, 1);
        assert_eq!(queued.queue.height, 5);
        assert_eq!(plain.conversation.height - queued.conversation.height, 5);
        // Status line, composer, bar, and the agent panel never move.
        assert_eq!(queued.status, plain.status);
        assert_eq!(queued.input, plain.input);
        assert_eq!(queued.bar, plain.bar);
        assert_eq!(queued.agent, plain.agent);
        // The strip sits flush between conversation and status.
        assert_eq!(
            queued.queue.y,
            queued.conversation.y + queued.conversation.height
        );
        assert_eq!(queued.queue.y + queued.queue.height, queued.status.y);
        // On a cramped terminal the strip shrinks away rather than taking
        // the conversation's last row.
        let tiny = terminal_layout_with_tree_rows(Rect::new(0, 0, 20, 8), 20, 5, 1);
        assert!(tiny.conversation.height >= 1);
        assert!(tiny.queue.height < 5);
        // Zero demand reserves nothing.
        assert_eq!(plain.queue.height, 0);
    }

    #[tokio::test]
    async fn queue_strip_is_hidden_when_empty_and_folds_overflow_into_more_row() {
        let (mut app, session, run) = app_with_active_run().await;
        // Empty lane: no rows reserved, no title anywhere in the frame.
        assert_eq!(app.queue_strip_height(), 0);
        let frame = rendered_frame(&mut app, 80, 24);
        assert!(!frame.contains("Pending"));
        // Five entries render the capped three text rows: two entries and
        // the folded overflow count.
        for index in 1..=5 {
            assert!(app.store.apply_event(admitted(
                session,
                index,
                run,
                &format!("message {index}")
            )));
        }
        assert_eq!(app.queue_strip_height(), 5);
        let frame = rendered_frame(&mut app, 80, 24);
        assert!(frame.contains("Pending"));
        assert!(frame.contains("message 1"));
        assert!(frame.contains("message 2"));
        assert!(!frame.contains("message 3"));
        assert!(frame.contains("+3 more"));
        assert!(!frame.contains("message 5"));
    }

    #[tokio::test]
    async fn queue_strip_ellipsizes_long_messages_and_flattens_newlines() {
        let (mut app, session, run) = app_with_active_run().await;
        assert!(
            app.store
                .apply_event(admitted(session, 1, run, &"x".repeat(200)))
        );
        assert!(
            app.store
                .apply_event(admitted(session, 2, run, "first\nsecond line"))
        );
        let frame = rendered_frame(&mut app, 60, 24);
        // The overlong entry truncates with an ellipsis…
        assert!(frame.contains('…'));
        // …and the multiline entry renders flattened onto one row.
        assert!(frame.contains("first second line"));
        // Two entries keep the strip at two text rows plus borders.
        assert_eq!(app.queue_strip_height(), 4);
    }

    #[test]
    fn ellipsize_single_line_flattens_and_truncates_grapheme_safely() {
        assert_eq!(ellipsize_single_line("a\nb  c", 80), "a b c");
        assert_eq!(ellipsize_single_line("short", 80), "short");
        let truncated = ellipsize_single_line(&"東京タワー".repeat(10), 10);
        assert!(truncated.ends_with('…'));
        assert!(UnicodeWidthStr::width(truncated.as_str()) <= 10);
        // Width zero still emits the ellipsis marker only.
        assert_eq!(ellipsize_single_line("abc", 0), "…");
    }

    #[test]
    fn queue_age_label_is_coarse_and_monotonic() {
        assert_eq!(queue_age_label(0), "<1m");
        assert_eq!(queue_age_label(59), "<1m");
        assert_eq!(queue_age_label(60), "1m");
        assert_eq!(queue_age_label(3599), "59m");
        assert_eq!(queue_age_label(3600), "1h");
        assert_eq!(queue_age_label(9000), "2h");
    }

    #[tokio::test]
    async fn queue_strip_renders_the_pending_lane() {
        let (mut app, session, run) = app_with_active_run().await;
        assert!(
            app.store
                .apply_event(admitted(session, 1, run, "first pending message"))
        );
        assert!(
            app.store
                .apply_event(admitted(session, 2, run, "second pending message"))
        );
        assert!(
            app.store
                .apply_event(admitted(session, 3, run, "third pending message"))
        );
        // Admission timestamps are the durable event timestamps: always
        // fresh here, so the coarse "<1m" age keeps the snapshot stable.
        let rendered = rendered_frame(&mut app, 60, 24);
        insta::assert_snapshot!(rendered);
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

    #[tokio::test]
    async fn selected_session_visibility_changes_refresh_skills() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        assert!(app.store.apply_event(session_created(session, 1)));
        app.selected = Some(session);

        let permission_event = StoredEvent {
            engine_version: None,
            session_id: session,
            run_id: None,
            seq: 2,
            timestamp: Timestamp::now(),
            payload: EventPayload::SessionPermissionOverlaySet {
                overlay: cookie_agent_protocol::SessionPermissionOverlay::empty(),
            },
        };
        app.refresh_skills_for_event_for_test(&permission_event);
        assert_eq!(app.skill_refresh_count_for_test(), 1);

        let run_event =
            run_started_with_suffix(session, 3, RunId::new_v7(), vec![resolved_model(None)]);
        app.refresh_skills_for_event_for_test(&run_event);
        assert_eq!(app.skill_refresh_count_for_test(), 2);
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

    #[tokio::test]
    async fn ctrl_p_opens_palette_plain_p_types_and_removed_commands_are_rejected() {
        let mut app = test_app().await;
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.input.as_str(), "/");
        assert!(app.command_palette_visible());

        app.input.set_buffer(String::new());
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.input.as_str(), "p");
        assert!(command_spec("block").is_none());
        assert!(command_spec("scroll").is_none());
        assert!(command_spec("stdin").is_none());
        assert!(command_spec("eof").is_none());
        assert!(command_spec("tree").is_none());
        assert!(command_spec("watch").is_none());
        assert!(parse_submission("/block next").is_err());
        assert!(parse_submission("/scroll top").is_err());
        assert!(parse_submission("/stdin").is_err());
        assert!(parse_submission("/stdin next").is_err());
        assert!(parse_submission("/eof").is_err());
        assert!(parse_submission("/tree up").is_err());
        assert!(parse_submission("/tree down").is_err());
        assert!(parse_submission("/tree toggle").is_err());
        assert!(parse_submission("/watch").is_err());
    }

    #[tokio::test]
    async fn page_keys_scroll_conversation_by_viewport_height() {
        let mut app = test_app().await;
        app.hit_map.conversation = Some(Rect::new(0, 0, 80, 10));
        app.conversation_scroll.offset = 30;
        app.conversation_scroll.following = false;

        app.handle_input_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            .await;
        assert_eq!(app.conversation_scroll.offset, 20);
        app.handle_input_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .await;
        assert_eq!(app.conversation_scroll.offset, 30);
    }

    #[tokio::test]
    async fn chrome_stays_coherent_across_themes_and_tiny_terminals() {
        for theme in [
            Theme::default(),
            Theme::new(ThemeKind::Mono, ColorLevel::None),
            Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16),
        ] {
            let mut app = test_app().await;
            let kind = theme.key();
            app.theme = theme;
            let session = SessionId::new_v7();
            assert!(app.store.apply_event(session_created(session, 1)));
            app.selected = Some(session);
            for (width, height) in [(100, 30), (40, 12), (20, 8)] {
                let rendered = rendered_frame(&mut app, width, height);
                // Every theme kind renders the same textual chrome: the
                // empty-state guidance, the composer placeholder, the
                // command hint. State never depends on color alone.
                if width >= 100 {
                    assert!(rendered.contains("Fresh session"), "{kind:?}: {rendered}");
                    assert!(rendered.contains("ctrl+p"), "{kind:?}: {rendered}");
                }
                if width >= 40 {
                    assert!(rendered.contains("Type a message"), "{kind:?}: {rendered}");
                }
                assert!(rendered.contains("Conversation"), "{kind:?}: {rendered}");
            }
            // A detached viewport announces itself in every theme.
            app.store
                .sessions
                .get_mut(&session)
                .expect("session")
                .transcript = (0..30)
                .map(|index| TranscriptItem::user(format!("message {index}")))
                .collect();
            let _ = rendered_frame(&mut app, 100, 30);
            app.conversation_scroll.top();
            let rendered = rendered_frame(&mut app, 100, 30);
            assert!(rendered.contains("↑ scrolled · PgDn: bottom"), "{kind:?}");
        }
    }

    #[tokio::test]
    async fn help_lists_each_command_on_its_own_transcript_line() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        assert!(app.store.apply_event(session_created(session, 1)));
        app.selected = Some(session);
        submit_direct_command(&mut app, "/help").await;
        let rendered = rendered_frame(&mut app, 110, 40);
        assert!(
            rendered.contains("NOTICE: Available commands:"),
            "{rendered}"
        );
        for expected in [
            "/quit — exit the TUI",
            "/new — choose the next run agent",
            "/approve once|all|reject|cancel — answer an approval",
            "/events debug|info|warning|error — set the diagnostic level filter for this view",
            "/help — show command help",
            "Use // to send a prompt beginning with /.",
        ] {
            assert!(rendered.contains(expected), "{expected}: {rendered}");
        }
        // The wall of semicolon-joined text is gone.
        assert!(!rendered.contains("; /new"), "{rendered}");
    }

    #[tokio::test]
    async fn scroll_follow_state_is_loud_in_the_conversation_title() {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        assert!(app.store.apply_event(session_created(session, 1)));
        // Enough top-level content to overflow the conversation viewport.
        app.store
            .sessions
            .get_mut(&session)
            .expect("session")
            .transcript = (0..30)
            .map(|index| TranscriptItem::user(format!("message {index}")))
            .collect();
        app.selected = Some(session);

        // Following: the title row carries no scroll warning.
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(!rendered.contains("PgDn: bottom"), "{rendered}");

        // Detached: the title row says so, with the truthful way back.
        app.conversation_scroll.top();
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("↑ scrolled · PgDn: bottom"), "{rendered}");

        // Paging down re-engages following at the exact bottom and clears
        // the warning.
        for _ in 0..10 {
            if app.conversation_scroll.following {
                break;
            }
            app.handle_input_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
                .await;
            let _ = rendered_frame(&mut app, 100, 30);
        }
        assert!(app.conversation_scroll.following);
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(!rendered.contains("PgDn: bottom"), "{rendered}");
    }

    #[test]
    fn command_registry_drives_help_and_parser() {
        let help = command_help();
        assert!(help.contains("/new"));
        assert!(help.contains("/connect"));
        assert!(help.contains("/events"));
        assert!(!help.contains("/block"));
        assert!(!help.contains("/scroll"));
        // The stdin era is over: /message left with it.
        assert!(!help.contains("/message"));
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
            preset: None,
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
            ("/quit", None),
            ("/new", Some("Agent")),
            ("/connect", Some("Connect provider")),
            ("/sessions", Some("Sessions")),
            ("/cancel", Some("no active run")),
            ("/approve once", None),
            ("/approve all", None),
            ("/approve reject", None),
            ("/approve cancel", None),
            ("/events debug", Some("diagnostic event filter")),
            ("/events info", Some("diagnostic event filter")),
            ("/events warning", Some("diagnostic event filter")),
            ("/events error", Some("diagnostic event filter")),
            ("/help", Some("Available commands:")),
        ];

        for (command, expected) in cases {
            let mut app = test_app().await;
            let (client, recorded, incoming_guard) = live_recording_client();
            app.client = client;
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

        for command in [
            "/approve once",
            "/approve all",
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

    #[tokio::test]
    async fn conversation_event_filter_hit_cycles_all_levels_wraps_and_applies_immediately() {
        let mut app = test_app().await;
        let session_id = SessionId::new_v7();
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                transcript: [
                    (crate::state::EventLevel::Debug, "debug row"),
                    (crate::state::EventLevel::Info, "info row"),
                    (crate::state::EventLevel::Warning, "warning row"),
                    (crate::state::EventLevel::Error, "error row"),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (level, text))| TranscriptItem::Event {
                    id: index as u64 + 1,
                    version: 0,
                    level,
                    text: text.into(),
                })
                .collect(),
                ..SessionState::default()
            },
        );
        app.tui_config.minimum_event_level = crate::state::EventLevel::Debug;

        for (current, next, visible_after_click, hidden_after_click) in [
            (
                crate::state::EventLevel::Debug,
                crate::state::EventLevel::Info,
                &["info row", "warning row", "error row"][..],
                &["debug row"][..],
            ),
            (
                crate::state::EventLevel::Info,
                crate::state::EventLevel::Warning,
                &["warning row", "error row"][..],
                &["debug row", "info row"][..],
            ),
            (
                crate::state::EventLevel::Warning,
                crate::state::EventLevel::Error,
                &["error row"][..],
                &["debug row", "info row", "warning row"][..],
            ),
            (
                crate::state::EventLevel::Error,
                crate::state::EventLevel::Debug,
                &["debug row", "info row", "warning row", "error row"][..],
                &[][..],
            ),
        ] {
            assert_eq!(app.tui_config.minimum_event_level, current);
            let rows = frame_rows(&mut app, 100, 30);
            let hit = app.hit_map.event_level_filter.expect("event filter hit");
            let label = format!("events ≥ {}", current.name());
            assert_eq!(rect_text(&rows, hit), label);
            assert_eq!(
                usize::from(hit.width),
                UnicodeWidthStr::width(label.as_str())
            );

            app.handle_click(hit.x, hit.y).await;
            assert_eq!(app.tui_config.minimum_event_level, next);
            assert_eq!(app.status, format!("Event level: {}", next.name()));

            let rendered = frame_rows(&mut app, 100, 30).join("\n");
            for text in visible_after_click {
                assert!(rendered.contains(text), "{next:?} should show {text}");
            }
            for text in hidden_after_click {
                assert!(!rendered.contains(text), "{next:?} should hide {text}");
            }
        }
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
    fn inline_code_spans_use_a_foreground_chip_except_in_high_contrast() {
        fn inline_code_style(theme: &Theme) -> ratatui::style::Style {
            let state = assistant_state(vec![AssistantChild::Text {
                id: 1,
                version: 0,
                markdown: MarkdownDocument::new("use `cargo test` here".to_owned()),
            }]);
            let layout = transcript_layout_with(&state, None, 60, theme, &PlainHighlighter);
            layout
                .lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .find(|span| span.content.contains("cargo test"))
                .expect("inline code span")
                .style
        }

        // Default theme: warm terracotta foreground, never a background —
        // the source backticks stay visible, and bold carries the
        // distinction where color is unavailable.
        let default = inline_code_style(&Theme::default());
        assert!(default.bg.is_none());
        assert_eq!(default.fg, Theme::default().inline_code().fg);
        assert!(
            default
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );

        // High contrast keeps its inverse-video chip so code still pops
        // against bright text.
        let contrast = inline_code_style(&Theme::new(
            crate::theme::ThemeKind::HighContrast,
            crate::theme::ColorLevel::Ansi16,
        ));
        assert_eq!(contrast.bg, Some(ratatui::style::Color::LightYellow));
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
                has_output_chunks: false,
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
            preset: None,
        });

        type_input(&mut app, "/connect").await;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;

        assert_eq!(app.modal, Modal::ConnectProviders);
        assert!(app.input.as_str().is_empty());
        assert_eq!(app.provider_search.focus(), SearchPickerFocus::Input);
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Search · Down/Tab/Enter: results"));
        assert!(rendered.contains("Connect provider (0/0) · Enter: details"));
        assert!(rendered.contains("No providers are available in the runtime snapshot."));
        assert!(rendered_cursor_visible(&mut app, 100, 30));
        assert!(app.hit_map.picker.is_some());
        assert!(app.hit_map.picker_input.is_some());
        assert!(app.hit_map.picker_rows.is_empty());
        tokio::task::yield_now().await;
        assert_eq!(recorded_method_count(&recorded, "run.start"), 0);
        assert_eq!(recorded_method_count(&recorded, "run.steer"), 0);
        drop(incoming_guard);

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.provider_search.query(), "x");
        assert_eq!(app.picker_state.selected(), None);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(app.picker_state.selected(), None);
        assert_eq!(app.provider_search.focus(), SearchPickerFocus::Input);
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Connect provider (0/0) · Enter: details"));
        assert!(rendered.contains('x'));
    }

    #[tokio::test]
    async fn provider_search_accepts_non_ascii_typing_and_paste() {
        let mut app = test_app().await;
        let mut provider = catalog_provider();
        provider.display_name = SafeDisplayText::new("阿里云").expect("provider display name");
        app.providers = vec![provider];

        type_input(&mut app, "/connect").await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;

        assert_eq!(app.modal, Modal::ConnectProviders);
        type_input(&mut app, "阿里").await;
        app.handle_paste("云");
        assert_eq!(app.provider_search.query(), "阿里云");
        assert_eq!(app.filtered_providers().len(), 1);
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Connect provider (1/1) · Enter: details"));
        assert!(rendered.contains("阿 里 云"));
        assert!(app.hit_map.picker.is_some());
        assert_eq!(app.hit_map.picker_rows.len(), 1);

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .await;
        type_input(&mut app, "missing").await;
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Connect provider (0/1) · Enter: details"));
        assert!(rendered.contains("No providers match the filter."));
        assert!(app.hit_map.picker.is_some());
        assert!(app.hit_map.picker_rows.is_empty());
    }

    #[tokio::test]
    async fn provider_search_edits_at_the_cursor_and_transitions_focus() {
        let mut app = test_app().await;
        app.providers = vec![catalog_provider()];
        app.run_command(SlashCommand::Connect).await;

        type_input(&mut app, "aXcme").await;
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE))
            .await;
        assert_eq!(app.provider_search.query(), "acme");
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .await;
        assert_eq!(app.provider_search.query(), "ace");

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .await;
        assert!(app.provider_search.query().is_empty());
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(app.provider_search.focus(), SearchPickerFocus::List);
        assert!(!rendered_cursor_visible(&mut app, 100, 30));
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        assert_eq!(app.provider_search.focus(), SearchPickerFocus::Input);
        assert!(rendered_cursor_visible(&mut app, 100, 30));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectProviders);
        assert_eq!(app.provider_search.focus(), SearchPickerFocus::Input);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::None);
        assert!(app.provider_search.query().is_empty());

        app.run_command(SlashCommand::Connect).await;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectSetup);
    }

    #[tokio::test]
    async fn provider_search_arrows_and_enter_select_a_filtered_provider() {
        let mut app = test_app().await;
        app.providers = vec![
            provider_descriptor("match-first", "supported", "current", false),
            provider_descriptor("excluded", "supported", "current", false),
            provider_descriptor("match-second", "supported", "current", false),
        ];
        app.run_command(SlashCommand::Connect).await;
        type_input(&mut app, "match").await;

        assert_eq!(app.filtered_providers().len(), 2);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(app.provider_search.focus(), SearchPickerFocus::List);
        assert_eq!(app.picker_state.selected(), Some(0));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(app.picker_state.selected(), Some(1));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;

        assert_eq!(app.modal, Modal::ConnectSetup);
        assert_eq!(
            app.connect_provider
                .as_ref()
                .map(|provider| provider.id.as_str()),
            Some("match-second")
        );
    }

    #[tokio::test]
    async fn credential_inputs_wipe_on_cancel_and_app_drop() {
        let before = credential_wipe_count();
        {
            let mut app = test_app().await;
            app.begin_provider_form(catalog_provider());
            app.modal = Modal::ConnectSetup;
            app.provider_form.as_mut().expect("provider form").secrets[0]
                .input
                .insert_owned("sentinel-secret".to_owned());
            app.clear_connect_secrets();
            assert!(app.provider_form.is_none());
        }
        assert!(credential_wipe_count() > before);
    }

    #[tokio::test]
    async fn connect_form_fields_focus_on_click_and_submit_dispatches() {
        let mut app = test_app().await;
        app.begin_provider_form(multi_auth_provider());
        frame_rows(&mut app, 120, 40);

        // Every rendered control registered a hit: the auth selector, one
        // credential, two setup fields, and the submit box.
        let fields = app.hit_map.provider_fields.clone();
        assert_eq!(fields.len(), 4);
        let submit = app.hit_map.provider_submit.expect("submit hit");

        // Hovering a control resolves to its own target.
        let credential = fields
            .iter()
            .find(|hit| hit.focus == ProviderFormFocus::Credential(0))
            .copied()
            .expect("credential hit");
        assert_eq!(
            app.hover_target_at(credential.rect.x + 1, credential.rect.y + 1),
            Some(HoverTarget::ProviderField(ProviderFormFocus::Credential(0)))
        );
        assert_eq!(
            app.hover_target_at(submit.x + 1, submit.y + 1),
            Some(HoverTarget::ProviderSubmit)
        );

        // Clicking the auth selector cycles the method, mirroring Enter, and
        // wipes the previous method's stale secrets.
        let auth = fields
            .iter()
            .find(|hit| hit.focus == ProviderFormFocus::AuthMethod)
            .copied()
            .expect("auth hit");
        app.handle_click(auth.rect.x + 2, auth.rect.y + 1).await;
        let form = app.provider_form.as_ref().expect("form");
        assert_eq!(form.auth_method.as_str(), "bearer");
        assert!(form.secrets[0].input.as_str().is_empty());

        // Clicking a credential focuses it and places the cursor at the
        // clicked display column of the real (unmasked) buffer.
        app.provider_form.as_mut().expect("form").secrets[0]
            .input
            .insert_owned("hunter2".to_owned());
        // The cycled auth method replaced the secret editors; a fresh frame
        // gives the new editor its render layout before the click maps
        // display cells to a cursor.
        frame_rows(&mut app, 120, 40);
        app.handle_click(credential.text_rect.x + 3, credential.text_rect.y)
            .await;
        let form = app.provider_form.as_mut().expect("form");
        assert_eq!(form.focus(), ProviderFormFocus::Credential(0));
        assert_eq!(form.secrets[0].input.state_mut().cursor_byte(), 3);

        // Clicking submit routes through the same validation as Enter:
        // required setup fields are empty, so the error stays inline in the
        // form — the modal and focus are retained for correction.
        app.handle_click(submit.x + 1, submit.y + 1).await;
        assert_eq!(app.modal, Modal::ConnectSetup);
        assert_eq!(
            app.provider_form.as_ref().expect("form").focus(),
            ProviderFormFocus::Credential(0)
        );
        assert!(app.provider_form.as_ref().expect("form").error.is_some());
    }

    #[tokio::test]
    async fn enter_submits_from_every_focus_and_validation_keeps_the_form_open() {
        let mut app = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        app.begin_provider_form(multi_auth_provider());

        // Required values are empty: Enter from every focus position routes
        // to the submit path, surfaces the validation error inline, and
        // leaves the modal, the focus, and the auth method untouched.
        let focuses = [
            ProviderFormFocus::AuthMethod,
            ProviderFormFocus::Credential(0),
            ProviderFormFocus::Setup(0),
            ProviderFormFocus::Setup(1),
            ProviderFormFocus::Submit,
        ];
        for focus in focuses {
            let form = app.provider_form.as_mut().expect("form");
            form.set_focus(focus);
            form.error = None;
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .await;
            let form = app.provider_form.as_ref().expect("form retained");
            assert_eq!(app.modal, Modal::ConnectSetup, "modal stays for {focus:?}");
            assert_eq!(form.focus(), focus, "focus unchanged for {focus:?}");
            assert_eq!(
                form.auth_method.as_str(),
                "api-key",
                "Enter does not cycle the auth method for {focus:?}"
            );
            assert!(
                form.error.is_some(),
                "inline validation error for {focus:?}"
            );
            assert!(
                app.provider_operations.is_empty(),
                "nothing dispatched for {focus:?}"
            );
            let rendered = rendered_frame(&mut app, 120, 40);
            assert!(
                rendered.contains("Region is required"),
                "error renders inline for {focus:?}"
            );
        }
        settle_recording().await;
        assert_eq!(recorded_method_count(&recorded, "provider.connect"), 0);

        // With every required value populated, Enter from an input box
        // dispatches exactly like the Submit button.
        let form = app.provider_form.as_mut().expect("form");
        form.secrets[0].input.insert_owned("api-secret".to_owned());
        form.setup[0].input.insert_owned("eu".to_owned());
        form.setup[1].input.insert_owned("derived".to_owned());
        form.set_focus(ProviderFormFocus::Setup(1));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectSetup);
        assert!(matches!(
            app.provider_operations
                .get(&ProviderId::new("multi-auth").expect("provider ID")),
            Some(ProviderOperation::InProgress(ProviderAction::Connect))
        ));
        settle_recording().await;
        assert_eq!(recorded_method_count(&recorded, "provider.connect"), 1);
        let request = recorded
            .lock()
            .expect("recorded")
            .iter()
            .find(|value| value["method"] == "provider.connect")
            .cloned()
            .expect("connect request");
        assert_eq!(request["params"]["auth_method"], "api-key");
        assert_eq!(request["params"]["auth_values"]["api_key"], "api-secret");
        assert_eq!(request["params"]["setup_values"]["region"], "eu");
        app.abort_connect_work();
        drop(incoming_guard);
    }

    #[tokio::test]
    async fn connect_form_cycles_auth_masks_secrets_traverses_and_submits_selected_method() {
        let mut app = test_app().await;
        let (client, recorded, incoming_guard) = live_recording_client();
        app.client = client;
        app.begin_provider_form(multi_auth_provider());

        let form = app.provider_form.as_ref().expect("form");
        assert_eq!(form.focus(), ProviderFormFocus::AuthMethod);
        assert_eq!(form.auth_method.as_str(), "api-key");
        assert_eq!(form.secrets[0].descriptor.id.as_str(), "api_key");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        type_input(&mut app, "stale-secret").await;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .await;
        let form = app.provider_form.as_ref().expect("form");
        assert_eq!(form.focus(), ProviderFormFocus::AuthMethod);
        assert_eq!(form.auth_method.as_str(), "bearer");
        assert_eq!(form.secrets[0].descriptor.id.as_str(), "access_token");
        assert!(form.secrets[0].input.as_str().is_empty());

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        type_input(&mut app, "bearer-secret").await;
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        app.handle_paste("東京");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        type_input(&mut app, "derived-secret").await;

        let rendered = rendered_frame(&mut app, 160, 42);
        assert!(rendered.contains("Authentication method"));
        assert!(rendered.contains("Bearer token (bearer)"));
        assert!(rendered.contains("Credential:"));
        assert!(rendered.contains("Credentials are verified on first use."));
        assert!(rendered.contains("Setup:"));
        assert!(rendered.contains("service_token"));
        assert!(rendered.contains('•'));
        assert!(!rendered.contains("bearer-secret"));
        assert!(!rendered.contains("derived-secret"));
        assert_eq!(
            app.provider_form.as_ref().expect("form").setup[0]
                .input
                .as_str(),
            "東京"
        );

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.provider_form.as_ref().expect("form").focus(),
            ProviderFormFocus::Submit
        );
        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await;
        assert_eq!(
            app.provider_form.as_ref().expect("form").focus(),
            ProviderFormFocus::Setup(1)
        );
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;

        settle_recording().await;
        assert_eq!(recorded_method_count(&recorded, "provider.connect"), 1);
        let request = recorded
            .lock()
            .expect("recorded")
            .iter()
            .find(|value| value["method"] == "provider.connect")
            .cloned()
            .expect("connect request");
        assert_eq!(request["params"]["auth_method"], "bearer");
        assert_eq!(
            request["params"]["auth_values"]["access_token"],
            "bearer-secret"
        );
        assert!(request["params"]["auth_values"].get("api_key").is_none());
        assert_eq!(request["params"]["setup_values"]["region"], "東京");
        assert_eq!(
            request["params"]["setup_values"]["service_token"],
            "derived-secret"
        );
        app.abort_connect_work();
        drop(incoming_guard);
    }

    #[tokio::test]
    async fn connect_form_escape_cancels_and_clears_values() {
        let before = credential_wipe_count();
        let mut app = test_app().await;
        app.begin_provider_form(multi_auth_provider());
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        type_input(&mut app, "cancelled-secret").await;

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;

        assert_eq!(app.modal, Modal::None);
        assert!(app.provider_form.is_none());
        assert!(credential_wipe_count() > before);
    }

    #[tokio::test]
    async fn connect_rpc_error_is_persistent_and_preserves_public_setup_for_retry() {
        let mut app = test_app().await;
        app.begin_provider_form(multi_auth_provider());
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        type_input(&mut app, "temporary-secret").await;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        type_input(&mut app, "東京").await;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        type_input(&mut app, "temporary-setup-secret").await;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;

        let form = app.provider_form.as_ref().expect("form retained in flight");
        assert_eq!(form.setup[0].input.as_str(), "東京");
        assert!(form.setup[1].input.as_str().is_empty());
        assert!(form.secrets[0].input.as_str().is_empty());
        app.abort_connect_work();

        let full_error = "JSON-RPC -32011: catalog_revision_conflict (provider connect error)";
        app.handle_rpc_update(RpcUpdate::ProviderMutationFinished {
            outcome: ProviderMutationOutcome::Failed {
                provider_id: ProviderId::new("multi-auth").expect("provider ID"),
                action: ProviderAction::Connect,
                error: full_error.into(),
            },
        });

        assert_eq!(app.modal, Modal::ConnectError);
        assert_eq!(
            app.provider_form.as_ref().expect("form").setup[0]
                .input
                .as_str(),
            "東京"
        );
        assert!(app.transient_notices.is_empty());
        let first = rendered_frame(&mut app, 160, 36);
        let second = rendered_frame(&mut app, 160, 36);
        assert!(first.contains(full_error));
        assert!(second.contains(full_error));
        assert!(first.contains("catalog_revision_conflict"));
        assert!(first.contains("No credentials were verified"));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectSetup);
        assert_eq!(
            app.provider_form.as_ref().expect("form").setup[0]
                .input
                .as_str(),
            "東京"
        );
        assert!(app.provider_form.as_ref().expect("form").error.is_none());
    }

    #[tokio::test]
    async fn provider_picker_filter_matches_id_and_name_only() {
        let mut app = test_app().await;
        app.providers = vec![catalog_provider()];
        app.provider_search.input_mut().set_buffer("ACME-AI".into());
        assert_eq!(app.filtered_providers().len(), 1);
        app.provider_search
            .input_mut()
            .set_buffer("provider".into());
        assert_eq!(app.filtered_providers().len(), 1);
        app.provider_search.input_mut().set_buffer("api_key".into());
        assert!(app.filtered_providers().is_empty());
        app.provider_search.input_mut().set_buffer("unknown".into());
        assert!(app.filtered_providers().is_empty());
    }

    #[tokio::test]
    async fn new_connect_session_resets_provider_filter() {
        let mut app = test_app().await;
        app.providers = vec![catalog_provider()];
        app.provider_search
            .input_mut()
            .set_buffer("stale filter".into());
        app.provider_search.focus_list();
        app.picker_state.select(None);

        app.run_command(SlashCommand::Connect).await;

        assert_eq!(app.modal, Modal::ConnectProviders);
        assert!(app.provider_search.query().is_empty());
        assert_eq!(app.provider_search.focus(), SearchPickerFocus::Input);
        assert_eq!(app.filtered_providers().len(), 1);
        assert_eq!(app.picker_state.selected(), Some(0));
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
        assert!(public.contains("Setup:"));
        assert!(public.contains("us-east-1"));
        let secret = rendered_frame(&mut app, 100, 30);
        assert!(secret.contains("Credential:"));
        assert!(secret.contains("Setup:"));
        assert!(secret.contains("us-east-1"));
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
        app.provider_search.focus_list();
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
        app.provider_search.focus_list();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectSetup);
        let credentials = rendered_frame(&mut app, 160, 36);
        assert!(credentials.contains("Credential:"));
        assert!(credentials.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
        type_input(&mut app, "removed-secret").await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        let submit = rendered_frame(&mut app, 140, 32);
        assert!(submit.contains("reconnect/update"));
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
        app.provider_search.focus_list();
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
        app.provider_search.focus_list();
        let picker = rendered_frame(&mut app, 140, 32);
        assert!(picker.contains("disconnected"));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectSetup);
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
        app.provider_search.focus_list();
        let picker = rendered_frame(&mut app, 140, 32);
        assert!(
            picker.contains("authored-incomplete provider (authored-incomplete) — disconnected")
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::ConnectSetup);
        let setup = rendered_frame(&mut app, 160, 32);
        assert!(setup.contains("Connect provider"));
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
        app.provider_search.focus_list();
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
        type_input(&mut app, "rotated-authored-secret").await;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        type_input(&mut app, "us-east-1").await;
        let submit = rendered_frame(&mut app, 140, 32);
        assert!(submit.contains("Enter to connect"));
        assert!(!submit.contains("Enter to reconnect/update"));
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
        app.provider_search.focus_list();
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
                has_output_chunks: false,
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
            60,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Debug,
            0,
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
            60,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Debug,
            0,
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
            60,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Debug,
            0,
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
                AssistantChild::Attribution { .. } => "attribution",
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
                    has_output_chunks: false,
                },
            );
        }
        let rendered = snapshot_lines(&transcript_layout(&state, None, 60).lines);
        assert!(rendered.contains("🔨 ▸ bash make cancelled"));
        assert!(rendered.contains("🔨 ▸ bash make interrupted"));
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

    async fn app_with_approval() -> App {
        let mut app = test_app().await;
        let approval = bash_approval_state();
        app.selected = Some(approval.session_id);
        app.store
            .sessions
            .entry(approval.session_id)
            .or_default()
            .approvals
            .push(approval);
        app
    }

    #[tokio::test]
    async fn roomy_approval_renders_glyph_buttons_with_tiled_hit_regions() {
        let mut app = app_with_approval().await;
        let rendered = rendered_frame(&mut app, 120, 40);
        // Glyph-bearing labels: the decision never relies on color alone.
        assert!(rendered.contains("✓ Allow once"), "{rendered}");
        assert!(rendered.contains("✗ Reject"), "{rendered}");
        assert!(rendered.contains("⎋ Cancel"), "{rendered}");
        // Rounded button frames read as distinct buttons.
        assert!(rendered.contains('╭'), "{rendered}");

        let actions = &app.hit_map.approval_actions;
        assert_eq!(
            actions.iter().map(|hit| hit.decision).collect::<Vec<_>>(),
            vec![
                ApprovalUserDecision::ApproveOnce,
                ApprovalUserDecision::Reject,
                ApprovalUserDecision::Cancel,
            ]
        );
        // The roomy panel gets three-row buttons whose hit regions tile the
        // inner width contiguously: no gaps, no overlaps.
        assert!(actions.iter().all(|hit| hit.rect.height == 3));
        let mut column = actions[0].rect.x;
        let row = actions[0].rect.y;
        for hit in actions {
            assert_eq!(hit.rect.x, column);
            assert_eq!(hit.rect.y, row);
            column = column.saturating_add(hit.rect.width);
        }
    }

    #[tokio::test]
    async fn cramped_approval_falls_back_to_a_single_action_row() {
        let mut app = app_with_approval().await;
        let rendered = rendered_frame(&mut app, 80, 24);
        let actions = &app.hit_map.approval_actions;
        assert_eq!(actions.len(), 3);
        assert!(actions.iter().all(|hit| hit.rect.height == 1));
        let mut column = actions[0].rect.x;
        for hit in actions {
            assert_eq!(hit.rect.x, column);
            column = column.saturating_add(hit.rect.width);
        }
        assert!(rendered.contains("✓ Allow once"), "{rendered}");
    }

    #[tokio::test]
    async fn hover_follows_mouse_moves_and_styles_the_target_cells() {
        let mut app = app_with_approval().await;
        // Draw once to populate the hit map.
        let _ = rendered_frame(&mut app, 120, 40);
        let target = app.hit_map.approval_actions[0].rect;
        let moved = |column: u16, row: u16| MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // Moving onto a button resolves it and asks for one redraw.
        assert!(app.handle_mouse(moved(target.x, target.y)).await);
        assert_eq!(
            app.hover,
            Some(HoverTarget::ApprovalAction(
                ApprovalUserDecision::ApproveOnce
            ))
        );
        // Staying put is not redraw-worthy; moving to the next button is.
        assert!(!app.handle_mouse(moved(target.x, target.y)).await);
        let next = app.hit_map.approval_actions[1].rect;
        assert!(app.handle_mouse(moved(next.x, next.y)).await);
        assert_eq!(
            app.hover,
            Some(HoverTarget::ApprovalAction(ApprovalUserDecision::Reject))
        );
        // While the approval is up it owns the pointer: anywhere off a
        // button clears the hover instead of leaking to content beneath.
        assert!(app.handle_mouse(moved(0, 0)).await);
        assert_eq!(app.hover, None);
        assert!(!app.handle_mouse(moved(0, 0)).await);

        // The hovered button is visibly filled with the glaze hover color.
        app.hover = Some(HoverTarget::ApprovalAction(
            ApprovalUserDecision::ApproveOnce,
        ));
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        let buffer = terminal.backend().buffer();
        let cell = buffer[(target.x.saturating_add(1), target.y.saturating_add(1))].style();
        // Environment-independent: whatever the detected color level, the
        // hovered button carries exactly the theme's glaze hover fill, and
        // it visibly differs from the unhovered cream panel.
        assert_eq!(cell.bg, app.theme.hover_fill().bg, "glaze fill: {cell:?}");
        assert_ne!(cell.bg, app.theme.panel().bg, "fill changed: {cell:?}");
    }

    #[tokio::test]
    async fn hover_only_targets_elements_with_a_click_action() {
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
        app.tree = Some(SessionTree {
            session: titled_meta(session, "root", 1),
            children: Vec::new(),
        });
        rendered_frame(&mut app, 80, 24);
        let moved = |column: u16, row: u16| MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // Passive surfaces never resolve a hover target, even though clicks
        // on them still work (focus the composer, drag the scrollbar,
        // toggle a block).
        let input = app.hit_map.input.expect("input hit").rect;
        assert!(
            !app.handle_mouse(moved(input.x.saturating_add(1), input.y.saturating_add(1)))
                .await
        );
        assert_eq!(app.hover, None);
        let block = app.hit_map.blocks.first().copied().expect("block hit").rect;
        assert!(!app.handle_mouse(moved(block.x, block.y)).await);
        assert_eq!(app.hover, None);
        let track = app.hit_map.scrollbar.expect("scrollbar reserved");
        assert!(!app.handle_mouse(moved(track.x, track.y)).await);
        assert_eq!(app.hover, None);

        // Elements whose click performs a real action do hover: cycle the
        // permission mode, cycle the event-level filter, select/watch a
        // tree row.
        let mode = app.hit_map.permission_mode.expect("permission mode hit");
        assert!(app.handle_mouse(moved(mode.x, mode.y)).await);
        assert_eq!(app.hover, Some(HoverTarget::PermissionMode));
        let filter = app.hit_map.event_level_filter.expect("event filter hit");
        assert!(app.handle_mouse(moved(filter.x, filter.y)).await);
        assert_eq!(app.hover, Some(HoverTarget::EventLevelFilter));
        let row = app
            .hit_map
            .tree_rows
            .first()
            .copied()
            .expect("tree row hit");
        assert!(app.handle_mouse(moved(row.rect.x, row.rect.y)).await);
        assert_eq!(app.hover, Some(HoverTarget::TreeRow(session)));
    }

    #[test]
    fn transcript_items_get_exactly_one_breathing_row_between_them() {
        let mut state = assistant_state(vec![AssistantChild::Text {
            id: 2,
            version: 0,
            markdown: MarkdownDocument::new("answer".into()),
        }]);
        state
            .transcript
            .insert(0, TranscriptItem::user("question one"));
        let layout = transcript_layout(&state, None, 60);
        let rendered = layout
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        let blanks = rendered
            .iter()
            .enumerate()
            .filter(|(_, line)| line.trim().is_empty())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(blanks.len(), 1, "{rendered:?}");
        assert!(
            rendered[..blanks[0]]
                .iter()
                .any(|line| line.contains("USER"))
        );
        assert!(
            rendered[blanks[0] + 1..]
                .iter()
                .any(|line| line.contains("answer"))
        );

        // Filtered-out event rows contribute no lines and no spacer, so
        // hiding diagnostics never leaves stray blank rows behind.
        state.transcript.push(TranscriptItem::Event {
            id: 3,
            version: 0,
            level: crate::state::EventLevel::Debug,
            text: "hidden diagnostic".into(),
        });
        state.transcript.push(TranscriptItem::Event {
            id: 4,
            version: 0,
            level: crate::state::EventLevel::Error,
            text: "visible failure".into(),
        });
        let layout = transcript_layout_with_level(
            &state,
            None,
            60,
            &Theme::default(),
            &PlainHighlighter,
            crate::state::EventLevel::Warning,
        );
        let rendered = layout
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        let blanks = rendered
            .iter()
            .filter(|line| line.trim().is_empty())
            .count();
        assert_eq!(blanks, 2, "{rendered:?}");
        assert!(rendered.iter().any(|line| line.contains("visible failure")));
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("hidden diagnostic"))
        );
    }

    #[test]
    fn empty_conversation_guidance_wraps_inside_the_pane() {
        for (has_session, headline, hint) in [
            (false, "No session selected.", "/sessions"),
            (true, "Fresh session", "ctrl+p"),
        ] {
            let lines = empty_conversation_lines(has_session, 60, &Theme::default());
            let rendered = snapshot_lines(&lines);
            assert!(rendered.contains(headline), "{rendered}");
            assert!(rendered.contains(hint), "{rendered}");
            for width in [8, 13, 24] {
                for line in empty_conversation_lines(has_session, width, &Theme::default()) {
                    assert!(
                        unicode_width::UnicodeWidthStr::width(line.to_string().as_str())
                            <= usize::from(width),
                        "width {width}: {line}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn first_launch_and_fresh_session_show_warm_empty_states() {
        let mut app = test_app().await;
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("No session selected."), "{rendered}");
        assert!(rendered.contains("/sessions"), "{rendered}");

        // A fresh, empty session greets instead of showing a blank pane.
        let session = SessionId::new_v7();
        assert!(app.store.apply_event(session_created(session, 1)));
        app.selected = Some(session);
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("Fresh session"), "{rendered}");
        assert!(rendered.contains("ctrl+p"), "{rendered}");

        // Once content exists the guidance is gone.
        let run = run_id();
        let attempt = AttemptId::new_v7();
        for event in [
            attempt_started(session, 2, run, attempt, None),
            text_delta(session, 3, run, attempt, "hello"),
        ] {
            assert!(app.store.apply_event(event));
        }
        let rendered = rendered_frame(&mut app, 100, 30);
        assert!(rendered.contains("hello"), "{rendered}");
        assert!(!rendered.contains("Fresh session"), "{rendered}");
    }

    // ------------------------------------------------------------------
    // User-message action menu (copy / revert / fork)
    // ------------------------------------------------------------------

    /// An app with a capture clipboard and one selected session holding two
    /// user messages (physical sequences 1 and 2), so menu actions can be
    /// checked against exact `through_seq` values.
    async fn app_with_user_messages() -> (App, SessionId, Arc<Mutex<Vec<String>>>) {
        let mut app = test_app().await;
        app.theme = Theme::new(ThemeKind::Default, ColorLevel::TrueColor);
        let session = SessionId::new_v7();
        let run = run_id();
        let copied = Arc::new(Mutex::new(Vec::new()));
        app.clipboard_sink = ClipboardSink::Capture(copied.clone());
        app.selected = Some(session);
        app.tree_root = Some(session);
        app.store.sessions.insert(session, SessionState::default());
        assert!(
            app.store
                .apply_event(user_input(session, 1, run, "first question"))
        );
        assert!(
            app.store
                .apply_event(user_input(session, 2, run, "second question"))
        );
        (app, session, copied)
    }

    /// The visible-row rect of the user message with `seq`, rendered.
    fn user_hit(app: &App, seq: u64) -> UserMessageHit {
        app.hit_map
            .user_messages
            .iter()
            .copied()
            .find(|hit| hit.seq == seq)
            .expect("user message hit")
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[tokio::test]
    async fn user_message_click_opens_the_menu_only_on_user_rows() {
        let (mut app, session, _) = app_with_user_messages().await;
        // A thinking block gives an assistant-owned row with a real click
        // action of its own; it must never open the message menu.
        app.store
            .sessions
            .get_mut(&session)
            .expect("session")
            .transcript
            .push(crate::state::TranscriptItem::Assistant {
                id: 9,
                version: 0,
                attribution: attribution(None),
                committed_turn_seq: Some(1),
                children: vec![AssistantChild::Thinking {
                    id: 1,
                    version: 0,
                    text: "thought".into(),
                }],
            });
        rendered_frame(&mut app, 80, 24);
        let hit = user_hit(&app, 1);
        app.handle_click(hit.rect.x + 2, hit.rect.y).await;
        assert_eq!(app.modal, Modal::UserMessage);
        let menu = app.user_menu.as_ref().expect("menu state");
        assert_eq!(menu.seq, 1);
        assert_eq!(menu.text, "first question");
        // The menu does not open from assistant/tool rows; they keep their
        // expand/collapse toggle. (The menu closes first so its overlay
        // does not swallow the click.)
        app.modal = Modal::None;
        app.user_menu = None;
        let block = app.hit_map.blocks.first().copied().expect("block hit");
        app.handle_click(block.rect.x, block.rect.y).await;
        assert_eq!(app.modal, Modal::None);
        assert!(app.user_menu.is_none());
        assert!(
            app.expanded_blocks
                .get(&session)
                .is_some_and(|set| set.contains(&block.id)),
            "the block kept its toggle"
        );
    }

    #[tokio::test]
    async fn menu_copy_captures_the_message_text() {
        let (mut app, _, copied) = app_with_user_messages().await;
        rendered_frame(&mut app, 80, 24);
        let hit = user_hit(&app, 2);
        app.handle_click(hit.rect.x + 2, hit.rect.y).await;
        assert_eq!(app.modal, Modal::UserMessage);
        // Copy is the first row: Enter activates the keyboard selection.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::None);
        assert_eq!(
            copied.lock().expect("capture").as_slice(),
            ["second question"]
        );
    }

    #[tokio::test]
    async fn menu_revert_is_confirm_guarded_and_targets_seq_minus_one() {
        let (mut app, session, _) = app_with_user_messages().await;
        let (client, recorded, _incoming) = live_recording_client();
        app.client = client;
        rendered_frame(&mut app, 80, 24);
        let hit = user_hit(&app, 2);
        app.handle_click(hit.rect.x + 2, hit.rect.y).await;
        // Choosing revert opens the confirm guard; Esc backs out to the
        // menu without any RPC.
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::RevertConfirm);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::UserMessage);
        assert_eq!(recorded_method_count(&recorded, "session.revert"), 0);
        // Confirming dispatches with through_seq = seq - 1: the message
        // itself leaves the visible branch.
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .await;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::None);
        let id = wait_for_recorded_request(&recorded, "session.revert", 1).await;
        let request = recorded
            .lock()
            .expect("recorded")
            .iter()
            .find(|value| value["id"].as_i64() == Some(id))
            .cloned()
            .expect("revert request");
        assert_eq!(
            request["params"]["session_id"].as_str(),
            Some(session.to_string().as_str())
        );
        assert_eq!(request["params"]["through_seq"].as_u64(), Some(1));
    }

    #[tokio::test]
    async fn menu_fork_targets_the_message_seq_and_switches_sessions() {
        let (mut app, session, _) = app_with_user_messages().await;
        let (client, recorded, _incoming) = live_recording_client();
        app.client = client;
        rendered_frame(&mut app, 80, 24);
        let hit = user_hit(&app, 2);
        app.handle_click(hit.rect.x + 2, hit.rect.y).await;
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.modal, Modal::None);
        let id = wait_for_recorded_request(&recorded, "session.fork", 1).await;
        let request = recorded
            .lock()
            .expect("recorded")
            .iter()
            .find(|value| value["id"].as_i64() == Some(id))
            .cloned()
            .expect("fork request");
        assert_eq!(
            request["params"]["session_id"].as_str(),
            Some(session.to_string().as_str())
        );
        // Fork keeps the message in the copied prefix: through_seq = seq.
        assert_eq!(request["params"]["through_seq"].as_u64(), Some(2));
        // The committed fork switches the viewed session.
        let forked = SessionId::new_v7();
        app.handle_rpc_update(RpcUpdate::Forked { forked });
        assert_eq!(app.selected, Some(forked));
    }

    #[tokio::test]
    async fn reverted_update_restores_the_message_text_into_the_composer() {
        let (mut app, session, _) = app_with_user_messages().await;
        app.handle_rpc_update(RpcUpdate::Reverted {
            session_id: session,
            text: "second question".into(),
        });
        assert_eq!(app.input.as_str(), "second question");
        assert!(app.input_focused);
    }

    // ------------------------------------------------------------------
    // Mouse text selection and clipboard
    // ------------------------------------------------------------------

    #[test]
    fn user_message_hit_rects_clip_and_shift_like_block_hits() {
        let region = UserRegion {
            seq: 7,
            start_line: 10,
            end_line: 20,
        };
        let viewport = Rect::new(0, 0, 40, 5);
        let hit = user_message_hit(region, viewport, 8).expect("hit");
        assert_eq!(hit.rect.y, 2);
        assert_eq!(hit.rect.height, 3);
        assert_eq!(hit.seq, 7);
        assert!(user_message_hit(region, viewport, 25).is_none());
    }

    #[test]
    fn osc52_sequence_frames_base64_clipboard_text() {
        assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(osc52_sequence(""), "\x1b]52;c;\x07");
    }

    #[test]
    fn extraction_strips_chrome_and_copies_raw_content() {
        let theme = Theme::default();
        // One user block, then an assistant block with prose and a fenced
        // code block — exactly what render_conversation chains.
        let mut lines = role_block(
            Role::User,
            vec![Line::from("raw question".to_owned())],
            60,
            &theme,
        );
        lines.push(Line::default());
        lines.extend(assistant_header("primary • test-model", 60, &theme));
        lines.extend(
            crate::markdown::render_markdown_width(
                &MarkdownDocument::new(
                    "some prose\n\n```rust\nfn main() {\n    let x = 1;\n}\n```\n\ntrail".into(),
                ),
                &theme,
                &PlainHighlighter,
                58,
            )
            .into_iter()
            .flat_map(|line| assistant_body_line(line, 60, &theme)),
        );
        let end = (lines.len() - 1, u16::MAX);
        let extracted = extract_selection(&lines, (0, 0), end, &theme);
        assert_eq!(
            extracted, "raw question\n\nsome prose\nfn main() {\n    let x = 1;\n}\ntrail",
            "gutters stripped, fence borders and role headers gone, code raw:\n{extracted:?}"
        );
    }

    #[test]
    fn extraction_column_windows_cut_on_grapheme_boundaries() {
        let theme = Theme::default();
        let lines = role_block(
            Role::User,
            vec![Line::from("abcdef".to_owned())],
            60,
            &theme,
        );
        // The body row is "│ abcdef": display column 2 is 'a'. Selecting
        // columns 4..7 of the rendered line yields "cde" — the gutter is
        // skipped by the coordinate shift, never copied.
        let body_row = 1;
        assert_eq!(
            extract_selection(&lines, (body_row, 4), (body_row, 7), &theme),
            "cde"
        );
        // An empty window extracts nothing.
        assert!(extract_selection(&lines, (body_row, 4), (body_row, 4), &theme).is_empty());
    }

    #[test]
    fn extraction_keeps_code_indentation_behind_real_gutters() {
        let theme = Theme::default();
        // A fenced code line whose content begins with a two-space span
        // (indentation split from the rest by highlighting): continuation
        // indents are chrome only in span position 0, so the code keeps
        // its leading spaces.
        let mut lines = crate::markdown::render_markdown_width(
            &MarkdownDocument::new("```\n  indented\n```".into()),
            &theme,
            &PlainHighlighter,
            58,
        )
        .into_iter()
        .flat_map(|line| assistant_body_line(line, 60, &theme))
        .collect::<Vec<_>>();
        // A wrapped user row's continuation indent still strips.
        lines.extend(role_block(
            Role::User,
            vec![Line::from(
                "a user line long enough to wrap at this width for sure".to_owned(),
            )],
            24,
            &theme,
        ));
        let end = (lines.len() - 1, u16::MAX);
        let extracted = extract_selection(&lines, (0, 0), end, &theme);
        assert!(
            extracted.contains("  indented"),
            "code indentation preserved: {extracted:?}"
        );
        // One copied line per rendered row: the wrapped user row's
        // continuations strip their two-space indent and join with
        // newlines, exactly as displayed.
        assert!(
            extracted.contains("\nenough to wrap at this"),
            "continuation indent stripped: {extracted:?}"
        );
    }

    #[test]
    fn extraction_keeps_high_contrast_content_sharing_the_border_foreground() {
        let theme = Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16);
        let border = theme.code_border();
        // High contrast paints fence grids and syntect's quantized plain
        // code foreground the same white; only the border's DIM|BOLD set
        // still distinguishes a chrome row from content.
        assert_eq!(border.fg, Some(ratatui::style::Color::White));
        let plain_code = Style::default().fg(ratatui::style::Color::White);
        let lines = vec![
            Line::from(vec![
                Span::styled("│ ", border),
                Span::styled("┌─ code: rust", border),
            ]),
            Line::from(vec![
                Span::styled("│ ", border),
                Span::styled("let answer = 42;", plain_code),
            ]),
            Line::from(vec![Span::styled("│ ", border), Span::styled("└─", border)]),
        ];
        let extracted = extract_selection(&lines, (0, 0), (2, u16::MAX), &theme);
        assert_eq!(
            extracted, "let answer = 42;",
            "border rows vanish, same-foreground content stays: {extracted:?}"
        );
    }

    #[tokio::test]
    async fn conversation_drag_selects_and_ctrl_c_copies_raw_text() {
        let (mut app, _, copied) = app_with_user_messages().await;
        rendered_frame(&mut app, 80, 24);
        let viewport = app.hit_map.conversation.expect("viewport");
        let body_row = viewport.y + 1; // first body line of message seq 2
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            viewport.x,
            body_row,
        ))
        .await;
        assert!(app.selection.is_none(), "a press alone selects nothing");
        app.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            viewport.x + 20,
            body_row,
        ))
        .await;
        assert_eq!(
            app.selection,
            Some(TextSelection::Conversation {
                anchor: (1, 0),
                head: (1, 20),
            })
        );
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            viewport.x + 20,
            body_row,
        ))
        .await;
        assert!(
            app.selection.is_some(),
            "a finished drag keeps its selection"
        );
        // ctrl+c copies the raw content and retires the selection.
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;
        assert!(app.selection.is_none());
        assert_eq!(
            copied.lock().expect("capture").as_slice(),
            ["first question"]
        );
    }

    #[tokio::test]
    async fn sub_threshold_drag_stays_a_click_and_opens_the_menu() {
        let (mut app, _, _) = app_with_user_messages().await;
        rendered_frame(&mut app, 80, 24);
        let hit = user_hit(&app, 2);
        let (x, y) = (hit.rect.x + 2, hit.rect.y);
        // A one-cell wobble between press and release is a click, not a
        // selection: the press dispatches on release.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y))
            .await;
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), x + 1, y))
            .await;
        assert!(app.selection.is_none());
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x + 1, y))
            .await;
        assert!(app.selection.is_none());
        assert_eq!(app.modal, Modal::UserMessage, "the click dispatched");
    }

    /// One full conversation drag gesture (press, move, release) across
    /// `rows` starting at the viewport's top-left content cell.
    async fn drag_selection(app: &mut App, viewport: Rect, dx: u16, dy: u16) {
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            viewport.x,
            viewport.y + 1,
        ))
        .await;
        app.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            viewport.x + dx,
            viewport.y + 1 + dy,
        ))
        .await;
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            viewport.x + dx,
            viewport.y + 1 + dy,
        ))
        .await;
    }

    #[tokio::test]
    async fn plain_click_and_esc_each_clear_a_finished_selection() {
        let (mut app, _, _) = app_with_user_messages().await;
        rendered_frame(&mut app, 80, 24);
        let viewport = app.hit_map.conversation.expect("viewport");
        drag_selection(&mut app, viewport, 8, 1).await;
        assert!(app.selection.is_some());
        // A fresh press anywhere clears the selection before doing its
        // work. (The blank spacer row between the two messages carries no
        // click action of its own.)
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            viewport.x,
            viewport.y + 2,
        ))
        .await;
        assert!(app.selection.is_none());
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            viewport.x,
            viewport.y + 2,
        ))
        .await;
        assert_eq!(app.modal, Modal::None, "nothing opened from a blank row");
        drag_selection(&mut app, viewport, 8, 1).await;
        assert!(app.selection.is_some());
        // Esc retires the selection without touching the escape-cancel
        // streak or the run.
        app.last_escape = Some(std::time::Instant::now());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(app.selection.is_none());
        assert_eq!(app.last_escape, None);
    }

    #[tokio::test]
    async fn ctrl_c_without_a_selection_still_cancels_the_active_run() {
        let (mut app, _session, _run) = app_with_active_run().await;
        let (client, recorded, _incoming) = live_recording_client();
        app.client = client;
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;
        wait_for_recorded_request(&recorded, "run.cancel", 1).await;
        assert!(app.selection.is_none());
    }

    #[tokio::test]
    async fn composer_drag_and_ctrl_x_cuts_the_selected_draft_text() {
        let mut app = test_app().await;
        let copied = Arc::new(Mutex::new(Vec::new()));
        app.clipboard_sink = ClipboardSink::Capture(copied.clone());
        app.selected = Some(SessionId::new_v7());
        app.input.set_buffer("hello world".to_owned());
        rendered_frame(&mut app, 80, 24);
        let text_rect = app.hit_map.input.expect("input").text_rect;
        // The draft is a single visual row: drag from the 'w' cell to past
        // the 'r' cell to select "wor".
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            text_rect.x + 6,
            text_rect.y,
        ))
        .await;
        app.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            text_rect.x + 9,
            text_rect.y,
        ))
        .await;
        assert_eq!(
            app.selection,
            Some(TextSelection::Composer { anchor: 6, head: 9 })
        );
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            text_rect.x + 9,
            text_rect.y,
        ))
        .await;
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(copied.lock().expect("capture").as_slice(), ["wor"]);
        assert_eq!(app.input.as_str(), "hello ld");
        assert!(app.selection.is_none());
    }

    #[tokio::test]
    async fn composer_click_without_drag_places_the_cursor_as_before() {
        let mut app = test_app().await;
        app.selected = Some(SessionId::new_v7());
        app.input.set_buffer("hello world".to_owned());
        rendered_frame(&mut app, 80, 24);
        let text_rect = app.hit_map.input.expect("input").text_rect;
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            text_rect.x + 5,
            text_rect.y,
        ))
        .await;
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            text_rect.x + 5,
            text_rect.y,
        ))
        .await;
        assert!(app.selection.is_none());
        assert_eq!(app.input.cursor_byte(), 5, "the click moved the cursor");
        assert!(app.input_focused);
    }

    #[tokio::test]
    async fn conversation_selection_retires_on_session_switch() {
        let (mut app, _session_a, copied) = app_with_user_messages().await;
        let session_b = SessionId::new_v7();
        app.store
            .sessions
            .insert(session_b, SessionState::default());
        rendered_frame(&mut app, 80, 24);
        let viewport = app.hit_map.conversation.expect("viewport");
        drag_selection(&mut app, viewport, 8, 1).await;
        assert!(
            matches!(app.selection, Some(TextSelection::Conversation { .. })),
            "the drag selected conversation rows"
        );
        app.set_selected_session(session_b);
        assert!(
            app.selection.is_none(),
            "watching another session retires the stale conversation leg"
        );
        // ctrl+c after the switch has no selection: nothing is copied from
        // the newly watched session's transcript.
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;
        assert!(
            copied.lock().expect("capture").is_empty(),
            "no stale text copied after the switch"
        );
    }

    #[tokio::test]
    async fn conversation_selection_retires_on_transcript_rebuild() {
        let (mut app, session, copied) = app_with_user_messages().await;
        rendered_frame(&mut app, 80, 24);
        let viewport = app.hit_map.conversation.expect("viewport");
        drag_selection(&mut app, viewport, 8, 1).await;
        assert!(
            matches!(app.selection, Some(TextSelection::Conversation { .. })),
            "the drag selected conversation rows"
        );
        // A revert marker rebuilds the visible transcript onto a new
        // branch; the pre-rebuild selection coordinates are meaningless.
        app.handle_delivery(live_event(runless_event(
            session,
            3,
            EventPayload::SessionReverted { through_seq: 1 },
        )))
        .await;
        assert!(
            app.selection.is_none(),
            "the rebuild retired the stale conversation leg"
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;
        assert!(
            copied.lock().expect("capture").is_empty(),
            "no stale text copied after the rebuild"
        );
    }

    #[tokio::test]
    async fn conversation_selection_retires_on_recovery_replay() {
        let (mut app, session, copied) = app_with_user_messages().await;
        rendered_frame(&mut app, 80, 24);
        let viewport = app.hit_map.conversation.expect("viewport");
        drag_selection(&mut app, viewport, 8, 1).await;
        assert!(
            matches!(app.selection, Some(TextSelection::Conversation { .. })),
            "the drag selected conversation rows"
        );
        // A recovery replay swaps in a whole new projection; the
        // selection's coordinates address the replaced one.
        app.handle_delivery(ClientDelivery::ReplayStart {
            session_id: session,
            generation: 0,
            final_seq: 2,
            rebuild: true,
        })
        .await;
        for seq in 1..=2 {
            app.handle_delivery(ClientDelivery::ReplayEvent {
                session_id: session,
                generation: 0,
                final_seq: 2,
                event: Box::new(user_input(session, seq, run_id(), "replayed")),
            })
            .await;
        }
        app.handle_delivery(ClientDelivery::ReplayEnd {
            session_id: session,
            generation: 0,
            final_seq: 2,
        })
        .await;
        assert!(
            app.selection.is_none(),
            "the recovery replay retired the stale conversation leg"
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;
        assert!(
            copied.lock().expect("capture").is_empty(),
            "no stale text copied after the replay"
        );
    }

    #[tokio::test]
    async fn wheel_during_overlay_transition_is_owned_by_state_not_stale_geometry() {
        let (mut app, session, _) = app_with_user_messages().await;
        rendered_frame(&mut app, 80, 12);
        let viewport = app.hit_map.conversation.expect("viewport");
        assert!(app.hit_map.approval.is_none());
        let (x, y) = (viewport.x + 1, viewport.y + 1);
        // Baseline: the conversation wheel-scrolls with no panel around.
        app.handle_wheel(x, y, false);
        let scrolled = app.conversation_scroll.offset;
        assert!(scrolled > 0, "content overflows the cramped viewport");
        app.conversation_scroll.offset = 0;
        // Arrival side: a panel that opened since the frame owns the wheel
        // even though its geometry does not exist yet.
        app.store
            .sessions
            .entry(session)
            .or_default()
            .approvals
            .push(approval(session));
        assert!(app.current_approval().is_some());
        app.handle_wheel(x, y, false);
        assert_eq!(
            app.conversation_scroll.offset, 0,
            "the state-open panel swallowed the wheel"
        );
        // Steady state: geometry rendered, the rect targets the panel
        // scroll.
        rendered_frame(&mut app, 80, 12);
        let approval_rect = app.hit_map.approval.expect("approval geometry");
        let (px, py) = (approval_rect.x + 1, approval_rect.y + 1);
        app.handle_wheel(px, py, false);
        assert_eq!(
            app.conversation_scroll.offset, 0,
            "the rendered panel still owns the wheel over its rect"
        );
        // Close side: the approval resolved but the hit map still
        // describes the gone panel — the wheel must reach the content.
        app.store
            .sessions
            .get_mut(&session)
            .expect("session")
            .approvals
            .clear();
        assert!(app.current_approval().is_none());
        app.handle_wheel(px, py, false);
        assert!(
            app.conversation_scroll.offset > 0,
            "stale panel geometry did not eat the wheel"
        );
    }

    #[tokio::test]
    async fn wheel_follows_topmost_first_ownership_when_modal_and_approval_coexist() {
        let (mut app, session, _) = app_with_user_messages().await;
        app.store
            .sessions
            .entry(session)
            .or_default()
            .approvals
            .push(approval(session));
        app.modal = Modal::Sessions;
        // Both panels render, stacked modal-over-approval like every other
        // pointer path.
        rendered_frame(&mut app, 80, 24);
        let approval_rect = app.hit_map.approval.expect("approval geometry");
        let picker_rect = app.hit_map.picker.expect("picker geometry");
        let left = approval_rect.x.max(picker_rect.x);
        let top = approval_rect.y.max(picker_rect.y);
        let right = approval_rect.right().min(picker_rect.right());
        let bottom = approval_rect.bottom().min(picker_rect.bottom());
        assert!(
            left < right && top < bottom,
            "centered panels overlap: {approval_rect:?} {picker_rect:?}"
        );
        // A wheel over the overlap reaches the topmost modal — the
        // obscured approval panel never scrolls.
        app.handle_wheel(left, top, false);
        assert_eq!(
            app.approval_scroll, 0,
            "the modal owns the overlap; the approval beneath never scrolled"
        );
        assert_eq!(
            app.conversation_scroll.offset, 0,
            "nothing leaked through to the content"
        );
    }

    #[tokio::test]
    async fn composer_selection_survives_a_watch_and_retires_on_mutation() {
        let mut app = test_app().await;
        app.selected = Some(SessionId::new_v7());
        app.input.set_buffer("hello world".to_owned());
        rendered_frame(&mut app, 80, 24);
        let text_rect = app.hit_map.input.expect("input").text_rect;
        composer_drag_selection(&mut app, text_rect, 1, 5).await;
        assert!(
            matches!(app.selection, Some(TextSelection::Composer { .. })),
            "the drag selected draft bytes"
        );
        // The draft buffer persists across a session watch, so the leg
        // stays valid.
        let session_b = SessionId::new_v7();
        app.store
            .sessions
            .insert(session_b, SessionState::default());
        app.set_selected_session(session_b);
        assert!(
            matches!(app.selection, Some(TextSelection::Composer { .. })),
            "a watch alone keeps the composer leg"
        );
        // Typing mutates the buffer: the stale byte range retires.
        app.handle_input_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
            .await;
        assert!(app.selection.is_none(), "typing retired the composer leg");
        // A paste retires it the same way.
        composer_drag_selection(&mut app, text_rect, 1, 5).await;
        assert!(matches!(
            app.selection,
            Some(TextSelection::Composer { .. })
        ));
        app.handle_paste("pasted");
        assert!(app.selection.is_none(), "pasting retired the composer leg");
    }

    #[tokio::test]
    async fn composer_selection_retires_on_multibyte_type_then_cut_is_sane() {
        let mut app = test_app().await;
        let copied = Arc::new(Mutex::new(Vec::new()));
        app.clipboard_sink = ClipboardSink::Capture(copied.clone());
        app.selected = Some(SessionId::new_v7());
        app.input.set_buffer("hello world".to_owned());
        rendered_frame(&mut app, 80, 24);
        let text_rect = app.hit_map.input.expect("input").text_rect;
        composer_drag_selection(&mut app, text_rect, 6, 9).await;
        assert_eq!(
            app.selection,
            Some(TextSelection::Composer { anchor: 6, head: 9 }),
            "the drag selected \"wor\""
        );
        // A multibyte insertion shifts every later byte offset: the stale
        // range 6..9 must retire, not silently retarget.
        app.handle_input_key(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE))
            .await;
        assert!(
            app.selection.is_none(),
            "multibyte insertion retired the stale byte range"
        );
        assert_eq!(app.input.as_str(), "hello worldé");
        // ctrl+x with no selection cuts and copies nothing.
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.input.as_str(), "hello worldé", "nothing was cut");
        assert!(
            copied.lock().expect("capture").is_empty(),
            "nothing was copied"
        );
        // Cursor navigation retires a selection the same way.
        composer_drag_selection(&mut app, text_rect, 6, 9).await;
        assert!(matches!(
            app.selection,
            Some(TextSelection::Composer { .. })
        ));
        app.handle_input_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
            .await;
        assert!(
            app.selection.is_none(),
            "cursor navigation retired the composer leg"
        );
    }

    /// One full composer drag gesture selecting the byte range covered by
    /// display columns `from`..`to` on the first text row.
    async fn composer_drag_selection(app: &mut App, text_rect: Rect, from: u16, to: u16) {
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            text_rect.x + from,
            text_rect.y,
        ))
        .await;
        app.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            text_rect.x + to,
            text_rect.y,
        ))
        .await;
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            text_rect.x + to,
            text_rect.y,
        ))
        .await;
    }

    #[tokio::test]
    async fn selection_highlight_paints_covered_cells_and_preserves_foregrounds() {
        let (mut app, _, _) = app_with_user_messages().await;
        rendered_frame(&mut app, 80, 24);
        let viewport = app.hit_map.conversation.expect("viewport");
        let body_row = viewport.y + 1;
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            viewport.x,
            body_row,
        ))
        .await;
        app.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            viewport.x + 10,
            body_row,
        ))
        .await;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        let buffer = terminal.backend().buffer();
        let selection = app.theme.text_selection();
        let covered = buffer[(viewport.x + 4, body_row)].style();
        assert_eq!(covered.bg, selection.bg, "covered cells take the wash");
        assert_eq!(
            covered.fg,
            buffer[(viewport.x + 12, body_row)].style().fg,
            "foregrounds are preserved across the boundary"
        );
        let outside = buffer[(viewport.x + 12, body_row)].style();
        assert_ne!(outside.bg, selection.bg, "uncovered cells keep their fill");
    }

    #[tokio::test]
    async fn user_message_menu_snapshot() {
        let (mut app, _, _) = app_with_user_messages().await;
        rendered_frame(&mut app, 80, 24);
        let hit = user_hit(&app, 2);
        app.handle_click(hit.rect.x + 2, hit.rect.y).await;
        insta::assert_snapshot!(rendered_frame(&mut app, 80, 24));
    }

    #[tokio::test]
    async fn selection_overlay_snapshot() {
        let (mut app, _, _) = app_with_user_messages().await;
        rendered_frame(&mut app, 80, 24);
        let viewport = app.hit_map.conversation.expect("viewport");
        // Drag from mid-first-message down into the second: covered rows
        // highlight from the drag column to the row end on the first row,
        // and from the row start to the drag column on the last.
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            viewport.x + 6,
            viewport.y + 1,
        ))
        .await;
        app.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            viewport.x + 10,
            viewport.y + 4,
        ))
        .await;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("app render");
        let buffer = terminal.backend().buffer();
        let selected_bg = app.theme.text_selection().bg;
        // Text with selected cells marked '#': the overlay shows the exact
        // covered region, gutters included, with no layout disturbance.
        let overlay = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| {
                        if selected_bg.is_some() && buffer[(x, y)].style().bg == selected_bg {
                            '#'
                        } else {
                            buffer[(x, y)].symbol().chars().next().unwrap_or(' ')
                        }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(overlay);
    }
}

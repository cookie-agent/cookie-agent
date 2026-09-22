//! Transcript layout, collapse state, wrapping, scrolling, and hit testing.

mod assistant;
mod diff;
mod events;
mod items;
mod output;
mod tool_rows;
mod wrap;

use assistant::*;
use diff::*;
pub(super) use events::producer_summary;
use events::*;
use items::*;
use output::*;
use tool_rows::*;
pub(super) use wrap::extract_selection;
pub(super) use wrap::leading_gutter_columns;
pub(super) use wrap::wrapped_line;
use wrap::*;

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ops::Range,
    time::Duration,
};

use cookie_agent_protocol::{
    AgentId, GoalState, GoalStatus, ProducerDeliveryMode, ProducerMessageId, ProducerOwner,
    SessionId,
};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    markdown::{Highlighter, MarkdownLine, MarkdownLineKind},
    state::{AssistantChild, ProducerMessageStatus, SessionState, ToolStatus, TranscriptItem},
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

/// Columns every scrollable pane keeps free at the right edge of its
/// interior: [`SCROLLBAR_MARGIN`] blank columns, then the
/// [`SCROLLBAR_TRACK_COLUMNS`] track flush against the right border.
///
/// The reservation is **constant** — it never depends on whether content
/// actually overflows. Wrap width is part of [`LayoutCacheKey`], so a
/// conditional reservation re-wrapped the whole transcript the moment the
/// "can this overflow?" heuristic flipped: a phantom reflow with no content
/// change. Only the *thumb* stays conditional on real overflow; reserving is
/// about width, drawing is about need.
pub(super) const SCROLLBAR_RESERVE: u16 = 2;

/// Track columns inside [`SCROLLBAR_RESERVE`].
pub(super) const SCROLLBAR_TRACK_COLUMNS: u16 = 1;

/// Blank columns between a pane's text and its scrollbar track.
pub(super) const SCROLLBAR_MARGIN: u16 = SCROLLBAR_RESERVE - SCROLLBAR_TRACK_COLUMNS;

/// Text width of a bordered pane: both borders, then the constant scrollbar
/// reservation. Shared by the conversation pane and the message composer so
/// both lay out, wrap, highlight and hit-test on the same columns.
pub(super) fn pane_text_width(outer_width: u16) -> u16 {
    outer_width
        .saturating_sub(2)
        .saturating_sub(SCROLLBAR_RESERVE)
}

/// The reserved scrollbar track of a bordered pane, given the pane `area` and
/// the text `viewport` carved out of it: the rightmost reserved column, one
/// blank margin away from the text. Collapses to zero width when the pane is
/// too narrow to hold both the margin and the track.
pub(super) fn pane_scrollbar_track(area: Rect, viewport: Rect) -> Rect {
    Rect::new(
        viewport
            .x
            .saturating_add(viewport.width)
            .saturating_add(SCROLLBAR_MARGIN),
        viewport.y,
        SCROLLBAR_TRACK_COLUMNS.min(
            area.width
                .saturating_sub(2)
                .saturating_sub(viewport.width)
                .saturating_sub(SCROLLBAR_MARGIN),
        ),
        viewport.height,
    )
}

#[derive(Debug)]
pub struct ConversationScroll {
    pub(super) offset: usize,
    pub(super) following: bool,
    max_offset: Option<usize>,
}

impl Default for ConversationScroll {
    fn default() -> Self {
        let mut scroll = Self {
            offset: 0,
            following: false,
            max_offset: None,
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
        self.max_offset = Some(max_offset);
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
                header_lines: None,
                header_gutter: None,
            },
            1,
        );
    }
    pub(super) fn down(&mut self, lines: usize) {
        self.scroll_to(self.offset.saturating_add(lines));
    }
    #[cfg(test)]
    pub fn top(&mut self) {
        self.following = false;
        self.offset = 0;
    }
    pub fn bottom(&mut self) {
        self.following = true;
    }

    /// Absolute top offset from a scrollbar thumb/track gesture.
    pub(super) fn scroll_to(&mut self, offset: usize) {
        // Resolve the gesture against the last rendered content, before an
        // output append can move the bottom on the next frame.
        self.offset = offset;
        self.following = self.max_offset.is_some_and(|max| offset >= max);
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
    SystemPrompt,
    Compaction(u64),
    PluginMessage(u64),
    ProducerMessage(ProducerMessageId),
    MediaFile {
        turn_seq: u64,
        content_index: u32,
    },
    Thinking(u64),
    Tool(cookie_agent_protocol::ToolCallId),
    ToolOutput {
        call_id: cookie_agent_protocol::ToolCallId,
        section: ToolOutputSection,
    },
    CommittedTool {
        turn_seq: u64,
        content_index: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum ToolOutputSection {
    Detail,
    Stdout,
    Stderr,
}

/// A contiguous logical-line range owned by one collapsible transcript block.
/// Stage 4 mouse handling can translate a y coordinate to a logical line by
/// adding the conversation scroll offset, then find the containing region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockRegion {
    pub(super) id: BlockId,
    pub(super) start_line: usize,
    pub(super) end_line: usize,
    /// Item header height before viewport clipping; None retains single-row hover.
    pub(super) header_lines: Option<usize>,
    /// Columns of chrome the builder hung in front of this block's header
    /// rows, counted from the spans it created them with (`chrome` in the tool
    /// layout). Provenance, not inference: the same glyphs arriving later in
    /// the row are command output — a tree listing really does start with
    /// `"│ "` — and must stay inside the highlight. `None` for blocks whose
    /// builder does not count its gutters, where `block_hit` falls back to
    /// reading the row.
    pub(super) header_gutter: Option<u16>,
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
    pub(super) highlighter: usize,
    pub(super) minimum_event_level: crate::state::EventLevel,
}

#[derive(Default)]
pub(super) struct LayoutCache {
    pub(super) key: Option<LayoutCacheKey>,
    pub(super) layout: TranscriptLayout,
    items: Vec<CachedItemLayout>,
    item_offsets: Vec<ItemAssemblyOffset>,
    assistant_parts: HashMap<u64, CachedAssistantPartLayout>,
    system_prompt: Option<CachedSystemPromptLayout>,
    #[cfg(test)]
    pub(super) item_layout_passes: u64,
    #[cfg(test)]
    pub(super) item_assembly_passes: u64,
    pub(super) assistant_part_layout_passes: u64,
}

#[derive(Clone, Copy)]
enum ScrollAnchorPoint {
    Item(u64),
    BlockStart(BlockId),
    BlockEnd(BlockId),
}

impl LayoutCache {
    /// The closest stable boundary before the viewport, plus its row offset.
    /// Block ends also anchor text following a tool within one assistant item.
    fn scroll_anchor(&self, offset: usize) -> Option<(ScrollAnchorPoint, usize)> {
        self.items
            .iter()
            .zip(&self.item_offsets)
            .map(|(item, start)| (ScrollAnchorPoint::Item(item.key.id), start.lines))
            .chain(
                self.layout
                    .regions
                    .iter()
                    // Output notices move after the newly revealed lines;
                    // anchor their containing tool's content instead.
                    .filter(|region| !matches!(region.id, BlockId::ToolOutput { .. }))
                    .flat_map(|region| {
                        [
                            (ScrollAnchorPoint::BlockStart(region.id), region.start_line),
                            (ScrollAnchorPoint::BlockEnd(region.id), region.end_line),
                        ]
                    }),
            )
            .filter(|(_, line)| *line <= offset)
            .max_by_key(|(_, line)| *line)
            .map(|(point, line)| (point, offset - line))
    }

    fn anchor_line(&self, point: ScrollAnchorPoint) -> Option<usize> {
        match point {
            ScrollAnchorPoint::Item(id) => self
                .items
                .iter()
                .zip(&self.item_offsets)
                .find_map(|(item, start)| (item.key.id == id).then_some(start.lines)),
            ScrollAnchorPoint::BlockStart(id) | ScrollAnchorPoint::BlockEnd(id) => self
                .layout
                .regions
                .iter()
                .find(|region| region.id == id)
                .map(|region| match point {
                    ScrollAnchorPoint::BlockStart(_) => region.start_line,
                    _ => region.end_line,
                }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ItemAssemblyOffset {
    /// Boundary before this item's optional separator and rendered lines.
    lines: usize,
    regions: usize,
    user_regions: usize,
}

impl ItemAssemblyOffset {
    fn at_end(layout: &TranscriptLayout) -> Self {
        Self {
            lines: layout.lines.len(),
            regions: layout.regions.len(),
            user_regions: layout.user_regions.len(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SystemPromptLayoutKey {
    fingerprint: cookie_agent_protocol::Sha256Digest,
    snapshot_agent: AgentId,
    draft_agent: Option<AgentId>,
    expanded: bool,
}

struct CachedSystemPromptLayout {
    key: SystemPromptLayoutKey,
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
    assistant_parts: Vec<AssistantPartRange>,
}

#[derive(Clone)]
struct AssistantPartRange {
    id: u64,
    key: AssistantPartLayoutKey,
    lines: Range<usize>,
    regions: Range<usize>,
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
    assistant_part_ranges: &'a mut Vec<AssistantPartRange>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BlockHit {
    pub(super) rect: Rect,
    pub(super) id: BlockId,
    pub(super) hover_rect: Option<Rect>,
    pub(super) toggle_rect: Option<Rect>,
}

// Layout cache validity depends on each independent render input; grouping them
// would obscure invalidation semantics without reducing call-site complexity.
#[allow(clippy::too_many_arguments)]
pub(super) fn ensure_cached_transcript_layout(
    cache: &mut LayoutCache,
    session_id: SessionId,
    state: &SessionState,
    draft_agent: Option<&AgentId>,
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
        highlighter: highlighter as *const dyn Highlighter as *const () as usize,
        minimum_event_level,
    };
    let full_rebuild = cache.key != Some(key);
    if full_rebuild {
        cache.key = Some(key);
        cache.layout = TranscriptLayout::default();
        cache.items.clear();
        cache.item_offsets.clear();
        cache.assistant_parts.clear();
        cache.system_prompt = None;
    }

    let prompt_key = state
        .run_snapshot
        .as_deref()
        .map(|snapshot| SystemPromptLayoutKey {
            fingerprint: snapshot.prompt_fingerprint.clone(),
            snapshot_agent: snapshot.agent.clone(),
            draft_agent: draft_agent.cloned(),
            expanded: expanded.is_some_and(|blocks| blocks.contains(&BlockId::SystemPrompt)),
        });
    let system_prompt_changed =
        cache.system_prompt.as_ref().map(|cached| &cached.key) != prompt_key.as_ref();
    if system_prompt_changed {
        cache.layout = TranscriptLayout::default();
        cache.item_offsets.clear();
        cache.system_prompt = if let (Some(snapshot), Some(prompt_key)) =
            (state.run_snapshot.as_deref(), prompt_key)
        {
            let layout = system_prompt_layout(snapshot, draft_agent, expanded, width, theme);
            append_item_layout(&mut cache.layout, layout);
            Some(CachedSystemPromptLayout { key: prompt_key })
        } else {
            None
        };
    }

    let first_dirty = if full_rebuild || system_prompt_changed {
        Some(0)
    } else {
        state
            .transcript
            .iter()
            .enumerate()
            .find_map(|(index, item)| {
                (!cache.items.get(index).is_some_and(|cached| {
                    item_layout_key_matches(&cached.key, state, item, expanded, clock_bucket)
                }))
                .then_some(index)
            })
            .or_else(|| {
                (cache.items.len() != state.transcript.len()).then_some(state.transcript.len())
            })
    };
    let Some(first_dirty) = first_dirty else {
        return true;
    };

    if !full_rebuild && !system_prompt_changed {
        let offset = cache
            .item_offsets
            .get(first_dirty)
            .copied()
            .unwrap_or_else(|| ItemAssemblyOffset::at_end(&cache.layout));
        cache.layout.lines.truncate(offset.lines);
        cache.layout.regions.truncate(offset.regions);
        cache.layout.user_regions.truncate(offset.user_regions);
        cache.item_offsets.truncate(first_dirty);
    }

    for (index, item) in state.transcript.iter().enumerate().skip(first_dirty) {
        cache
            .item_offsets
            .push(ItemAssemblyOffset::at_end(&cache.layout));
        if cache.items.get(index).is_some_and(|cached| {
            item_layout_key_matches(&cached.key, state, item, expanded, clock_bucket)
        }) {
            append_item_layout(&mut cache.layout, cache.items[index].layout.clone());
        } else {
            let item_key = item_layout_key(state, item, expanded, clock_bucket);
            let mut assistant_part_ranges = Vec::new();
            let mut context = TranscriptRenderContext {
                expanded,
                width,
                theme,
                highlighter,
                minimum_event_level,
                clock_bucket,
                assistant_part_cache: &mut cache.assistant_parts,
                assistant_part_layout_passes: &mut cache.assistant_part_layout_passes,
                assistant_part_ranges: &mut assistant_part_ranges,
            };
            let spliced = cache.items.get_mut(index).is_some_and(|cached| {
                splice_active_assistant_part(cached, state, item, &mut context)
            });
            let layout = if spliced {
                let cached = &mut cache.items[index];
                cached.key = item_key;
                cached.layout.clone()
            } else {
                let layout = transcript_item_layout(state, item, &mut context);
                let cached = CachedItemLayout {
                    key: item_key,
                    layout: layout.clone(),
                    assistant_parts: assistant_part_ranges,
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
            append_item_layout(&mut cache.layout, layout);
        }
        #[cfg(test)]
        {
            cache.item_assembly_passes = cache.item_assembly_passes.wrapping_add(1);
        }
    }
    cache.items.truncate(state.transcript.len());
    cache.item_offsets.truncate(state.transcript.len());
    false
}

impl App {
    /// The transient notice rows rendered after the transcript (transient
    /// notices and goal notices), exactly as [`Self::render_conversation`]
    /// appends them. Descendant warnings are spliced into the transcript
    /// body instead; see [`Self::spliced_conversation_lines`]. Selection
    /// extraction consumes the same chain so copied text matches what is
    /// on screen.
    pub(super) fn notice_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut notice_lines = Vec::new();
        let goal_notices = self
            .selected
            .and_then(|session_id| self.goal_notices.get(&session_id));
        for notice in self
            .transient_notices
            .iter()
            .chain(goal_notices.into_iter().flatten())
        {
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
        notice_lines
    }

    /// The viewed session's rendered transcript lines with aggregated
    /// descendant warnings spliced in at their chronological position.
    ///
    /// Each warning anchors after the last transcript item whose durable
    /// insertion time is at or before the warning's time; the warning's
    /// rendered block takes the next item's separator slot, so pre-warning
    /// content finishes above the break and later content resumes below it.
    /// Warnings older than every timed item land just after the system
    /// prompt (before the first item). Returns the untouched layout when
    /// there is nothing to splice.
    fn spliced_conversation_lines(
        &self,
        width: u16,
        layout_lines: &[Line<'static>],
        warnings: &[(jiff::Timestamp, String)],
    ) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
        let Some(state) = self
            .selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .filter(|_| !warnings.is_empty())
        else {
            return (layout_lines.to_vec(), Vec::new());
        };
        Self::splice_descendant_warnings(
            layout_lines.to_vec(),
            &self.layout_cache.item_offsets,
            state,
            warnings,
            width,
            &self.theme,
        )
    }

    /// Shift a block region's line coordinates by the splice insertions at or
    /// before each coordinate.
    fn shift_region_lines(
        mut region: BlockRegion,
        splice_shifts: &[(usize, usize)],
    ) -> BlockRegion {
        let shift = |line: usize| {
            splice_shifts
                .iter()
                .filter(|(position, _)| *position <= line)
                .map(|(_, inserted)| *inserted)
                .sum::<usize>()
                .saturating_add(line)
        };
        region.start_line = shift(region.start_line);
        region.end_line = shift(region.end_line);
        region
    }

    /// Shift a user region's line coordinates by the splice insertions at or
    /// before each coordinate.
    fn shift_user_region_lines(
        mut region: UserRegion,
        splice_shifts: &[(usize, usize)],
    ) -> UserRegion {
        let shift = |line: usize| {
            splice_shifts
                .iter()
                .filter(|(position, _)| *position <= line)
                .map(|(_, inserted)| *inserted)
                .sum::<usize>()
                .saturating_add(line)
        };
        region.start_line = shift(region.start_line);
        region.end_line = shift(region.end_line);
        region
    }

    /// Splice aggregated descendant warning blocks into a rendered transcript at
    /// their chronological position.
    ///
    /// `item_offsets` holds one [`ItemAssemblyOffset`] per transcript item: item
    /// i's separator and rendered lines span `[offsets[i].lines,
    /// offsets[i+1].lines or lines.len())`, and rows before `offsets[0].lines`
    /// belong to the system prompt. Each warning anchors after the last item
    /// whose durable insertion time is at or before the warning time, taking the
    /// next item's separator slot; warnings older than every timed item land
    /// after the system prompt, before the first item. When no item is timed at
    /// all (or the transcript is empty), warnings keep their historical
    /// bottom-of-transcript position.
    ///
    /// Returns the spliced lines plus, for each splice point, the original-line
    /// position and the number of lines inserted there, so callers can shift
    /// scroll anchors and hit regions that address the unspliced layout.
    fn splice_descendant_warnings(
        lines: Vec<Line<'static>>,
        item_offsets: &[ItemAssemblyOffset],
        state: &crate::state::SessionState,
        warnings: &[(jiff::Timestamp, String)],
        width: u16,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
        if warnings.is_empty() {
            return (lines, Vec::new());
        }
        let timed_anchor = |time: jiff::Timestamp| -> Option<usize> {
            (0..state.transcript.len()).rev().find(|&index| {
                state
                    .item_time(state.transcript[index].id())
                    .is_some_and(|item_time| item_time <= time)
            })
        };
        let mut placements: Vec<(usize, String)> = warnings
            .iter()
            .map(|(time, text)| {
                let position = match timed_anchor(*time) {
                    Some(index) => item_offsets
                        .get(index + 1)
                        .map_or(lines.len(), |offset| offset.lines),
                    None if !item_offsets.is_empty() => item_offsets[0].lines,
                    None => lines.len(),
                };
                (position, text.clone())
            })
            .collect();
        placements.sort_by_key(|(position, _)| *position);
        let mut out = Vec::with_capacity(lines.len() + warnings.len() * 3);
        // Splice points and inserted counts, in original-line coordinates.
        let mut splice_shifts: Vec<(usize, usize)> = Vec::new();
        let mut cursor = 0usize;
        for (position, text) in placements {
            while cursor < position.min(lines.len()) {
                out.push(lines[cursor].clone());
                cursor += 1;
            }
            let needs_separator = out.last().is_some_and(|line| {
                line.spans
                    .iter()
                    .any(|span| !span.content.trim().is_empty())
            });
            let mut block = role_block(Role::Warning, vec![Line::from(text)], width, theme);
            let mut inserted = block.len();
            if needs_separator {
                block.insert(0, Line::default());
                inserted += 1;
            }
            if inserted > 0 {
                splice_shifts.push((position, inserted));
            }
            out.extend(block);
        }
        out.extend(lines[cursor..].iter().cloned());
        (out, splice_shifts)
    }

    /// The full rendered conversation chain — transcript lines plus the
    /// notice block — for selection extraction. The cached layout is exactly
    /// what the last frame rendered at this width, so logical-line
    /// coordinates from the mouse map one-to-one.
    pub(super) fn conversation_chain(&self, width: u16) -> Cow<'_, [Line<'static>]> {
        let session_present = self
            .selected
            .is_some_and(|session_id| self.store.sessions.contains_key(&session_id));
        let transcript_empty = self
            .selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .is_none_or(|state| state.transcript.is_empty() && state.run_snapshot.is_none());
        let descendant_warnings =
            if self.tui_config.minimum_event_level <= crate::state::EventLevel::Warning {
                self.selected
                    .map(|selected| self.descendant_warnings(selected))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
        let mut notices = self.notice_lines(width);
        if session_present
            && !transcript_empty
            && descendant_warnings.is_empty()
            && notices.is_empty()
        {
            return Cow::Borrowed(&self.layout_cache.layout.lines);
        }
        let mut lines = if session_present && !transcript_empty {
            self.spliced_conversation_lines(
                width,
                &self.layout_cache.layout.lines,
                &descendant_warnings,
            )
            .0
        } else {
            let empty = empty_conversation_lines(session_present, width, &self.theme);
            self.spliced_conversation_lines(width, &empty, &descendant_warnings)
                .0
        };
        if !lines.is_empty() && !notices.is_empty() {
            notices.insert(0, Line::default());
        }
        lines.extend(notices);
        Cow::Owned(lines)
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
        // The two rightmost inner columns always belong to the scrollbar: a
        // blank margin and the track. Content layout, block hit regions and
        // highlight rects are built at that reduced width and never extend
        // into it, so the track can be grabbed without hitting blocks — and
        // because the reservation is constant, the wrap width (a
        // `LayoutCacheKey` input) never flips when content starts or stops
        // overflowing.
        let descendant_warnings =
            if self.tui_config.minimum_event_level <= crate::state::EventLevel::Warning {
                self.selected
                    .map(|selected| self.descendant_warnings(selected))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
        let width = pane_text_width(area.width);
        let session_present = self
            .selected
            .is_some_and(|session_id| self.store.sessions.contains_key(&session_id));
        let empty_layout = TranscriptLayout {
            lines: empty_conversation_lines(session_present, width, &self.theme),
            regions: Vec::new(),
            user_regions: Vec::new(),
        };
        let clock_bucket = self.clock_bucket();
        let draft_agent = self.draft.as_ref().map(|draft| draft.agent.clone());
        let anchor_key = self.layout_cache.key;
        let anchor = (!self.conversation_scroll.following)
            .then(|| {
                self.layout_cache
                    .scroll_anchor(self.conversation_scroll.offset)
            })
            .flatten();
        let mut layout_changed = false;
        let layout = if let Some((session_id, state)) = self.selected.and_then(|session_id| {
            self.store
                .sessions
                .get(&session_id)
                .map(|state| (session_id, state))
        }) {
            layout_changed = !ensure_cached_transcript_layout(
                &mut self.layout_cache,
                session_id,
                state,
                draft_agent.as_ref(),
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
            if state.transcript.is_empty() && state.run_snapshot.is_none() {
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
        let (spliced_lines, splice_shifts) =
            self.spliced_conversation_lines(width, &layout.lines, &descendant_warnings);
        let layout_lines: &[Line<'static>] = if splice_shifts.is_empty() {
            &layout.lines
        } else {
            &spliced_lines
        };
        let viewport = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            width,
            area.height.saturating_sub(2),
        );
        let scrollbar_track = pane_scrollbar_track(area, viewport);
        let content_height = layout_lines.len() + notice_lines.len();
        if layout_changed
            && anchor_key == self.layout_cache.key
            && let Some((point, row_offset)) = anchor
            && let Some(line) = self.layout_cache.anchor_line(point)
        {
            // `line` addresses the unspliced layout; rows inserted by the
            // warning splice above it shift the anchor down by their count.
            let splice_shift = splice_shifts
                .iter()
                .filter(|(position, _)| *position <= line)
                .map(|(_, inserted)| *inserted)
                .sum::<usize>();
            self.conversation_scroll.offset =
                line.saturating_add(splice_shift).saturating_add(row_offset);
        }
        self.conversation_scroll
            .clamp(content_height, viewport.height);
        self.hit_map.conversation = Some(viewport);
        self.hit_map.scrollbar = (scrollbar_track.width > 0).then_some(scrollbar_track);
        self.hit_map.blocks.clear();
        let shifted_block_regions: Vec<BlockRegion>;
        let block_regions: &[BlockRegion] = if splice_shifts.is_empty() {
            &layout.regions
        } else {
            shifted_block_regions = layout
                .regions
                .iter()
                .map(|region| Self::shift_region_lines(*region, &splice_shifts))
                .collect();
            &shifted_block_regions
        };
        self.hit_map
            .blocks
            .extend(block_regions.iter().filter_map(|region| {
                block_hit(
                    *region,
                    layout_lines,
                    viewport,
                    self.conversation_scroll.offset,
                )
            }));
        self.hit_map.user_messages.clear();
        let shifted_user_regions: Vec<UserRegion>;
        let user_regions: &[UserRegion] = if splice_shifts.is_empty() {
            &layout.user_regions
        } else {
            shifted_user_regions = layout
                .user_regions
                .iter()
                .map(|region| Self::shift_user_region_lines(*region, &splice_shifts))
                .collect();
            &shifted_user_regions
        };
        self.hit_map
            .user_messages
            .extend(user_regions.iter().filter_map(|region| {
                user_message_hit(*region, viewport, self.conversation_scroll.offset)
            }));
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
            let mut column = title_area.x.saturating_add(crate::ui::PANEL_TITLE_PAD);
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
        let mut block = crate::ui::panel_block()
            .border_style(self.theme.panel_border())
            .title(crate::ui::panel_title(title_spans));
        // A viewport that no longer follows live output says so loudly, in
        // the title row, with the truthful way back — never buried in the
        // muted status line.
        if !self.conversation_scroll.following {
            block = block.title(
                crate::ui::panel_title(Span::styled(
                    "↑ scrolled · PgDn: bottom",
                    self.theme.warning(),
                ))
                .right_aligned(),
            );
        }
        frame.render_widget(block, area);
        // Rendering borrowed Lines avoids viewport clones. Line::style paints
        // the full row, so future background or REVERSED line styles must be
        // reviewed as row-wide rather than span-local styling.
        for (row, line) in layout_lines
            .iter()
            .chain(notice_lines.iter())
            .skip(self.conversation_scroll.offset)
            .take(usize::from(viewport.height))
            .enumerate()
        {
            frame.render_widget(
                line,
                Rect::new(viewport.x, viewport.y + row as u16, viewport.width, 1),
            );
        }
        // The reservation always exists; the thumb only does. A pane too tight
        // for the track, or content that fits the viewport, keeps its reserved
        // columns blank.
        self.scrollbar_geometry = (scrollbar_track.width > 0)
            .then(|| {
                ScrollbarGeometry::resolve(scrollbar_track, content_height)
                    .map(|geometry| geometry.with_thumb(self.conversation_scroll.offset))
            })
            .flatten();
        if let Some(geometry) = self.scrollbar_geometry {
            render_scrollbar_track(frame, geometry, &self.theme);
        }
    }

    pub(super) fn toggle_block(&mut self, block_id: BlockId) {
        let Some(session_id) = self.selected else {
            return;
        };
        // An explicit layout change preserves what the user is reading. Only
        // output received while already following should advance to the tail.
        self.conversation_scroll.following = false;
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
    transcript_layout_at_clock(
        state,
        expanded,
        width,
        theme,
        highlighter,
        minimum_event_level,
        0,
    )
}

/// Layout at an explicit animation bucket, so tests can pin every phase of the
/// running status marker (which changes the header's suffix width).
#[cfg(test)]
fn transcript_layout_at_clock(
    state: &SessionState,
    expanded: Option<&HashSet<BlockId>>,
    width: u16,
    theme: &Theme,
    highlighter: &dyn Highlighter,
    minimum_event_level: crate::state::EventLevel,
    clock_bucket: u8,
) -> TranscriptLayout {
    let mut layout = TranscriptLayout::default();
    let mut assistant_parts = HashMap::new();
    let mut assistant_part_layout_passes = 0;
    if let Some(snapshot) = state.run_snapshot.as_deref() {
        append_item_layout(
            &mut layout,
            system_prompt_layout(snapshot, Some(&snapshot.agent), expanded, width, theme),
        );
    }
    for item in &state.transcript {
        let mut assistant_part_ranges = Vec::new();
        let item_layout = transcript_item_layout(
            state,
            item,
            &mut TranscriptRenderContext {
                expanded,
                width,
                theme,
                highlighter,
                minimum_event_level,
                clock_bucket,
                assistant_part_cache: &mut assistant_parts,
                assistant_part_layout_passes: &mut assistant_part_layout_passes,
                assistant_part_ranges: &mut assistant_part_ranges,
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
            ..region
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

pub(super) fn block_hit(
    region: BlockRegion,
    lines: &[Line<'static>],
    viewport: Rect,
    scroll_offset: usize,
) -> Option<BlockHit> {
    let viewport_end = scroll_offset.saturating_add(usize::from(viewport.height));
    let start = region.start_line.max(scroll_offset);
    let end = region.end_line.min(viewport_end);
    let header_end = region
        .header_lines
        .map_or(start.saturating_add(1), |height| {
            region.start_line.saturating_add(height)
        })
        .min(end);
    // The block's own content columns: everything right of its leading gutter
    // and left of the viewport edge, which already stops before the reserved
    // scrollbar columns. Highlights share this rect with the band and the
    // selection so no painted row ever covers a `│` border or the track.
    // Hit rectangles stay viewport-wide on purpose: the gutter and the blank
    // tail of a row must still click, toggle and drag.
    //
    // The block's header rows stand behind their gutter, and only behind it:
    // a builder that counted its own spans says how wide that is, which is the
    // only reading that cannot mistake `"│ "` arriving as command output — a
    // tree listing, a diff body — for a second gutter and hide real columns.
    // Regions whose builders do not count fall back to reading the row; the
    // walk is bounded to header rows, which carry the block's own gutter, and
    // resolves the one case a bare line cannot settle — leading spaces that
    // could be a continuation indent or indentation inside the content — as
    // content, matching `extract_line` rather than hiding columns a reader
    // could have selected.
    let hovered = start.min(lines.len())..header_end.max(start).min(lines.len());
    let gutter = region.header_gutter.unwrap_or_else(|| {
        lines[hovered]
            .iter()
            .map(leading_gutter_columns)
            .max()
            .unwrap_or(0)
    });
    let content = Rect::new(
        viewport.x.saturating_add(gutter),
        viewport.y,
        viewport.width.saturating_sub(gutter),
        viewport.height,
    );
    (start < end).then(|| BlockHit {
        rect: Rect::new(
            viewport.x,
            viewport.y + u16::try_from(start - scroll_offset).unwrap_or(u16::MAX),
            viewport.width,
            u16::try_from(end - start).unwrap_or(u16::MAX),
        ),
        id: region.id,
        toggle_rect: if matches!(region.id, BlockId::ToolOutput { .. }) {
            // Output regions are built solely from append_output_notice rows.
            (region.start_line >= scroll_offset).then(|| {
                Rect::new(
                    viewport.x,
                    viewport.y + u16::try_from(start - scroll_offset).unwrap_or(u16::MAX),
                    viewport.width,
                    1,
                )
            })
        } else {
            (start < header_end).then(|| {
                Rect::new(
                    viewport.x,
                    viewport.y + u16::try_from(start - scroll_offset).unwrap_or(u16::MAX),
                    viewport.width,
                    u16::try_from(header_end - start).unwrap_or(u16::MAX),
                )
            })
        },
        hover_rect: (start < header_end).then(|| {
            Rect::new(
                content.x,
                viewport.y + u16::try_from(start - scroll_offset).unwrap_or(u16::MAX),
                content.width,
                u16::try_from(header_end - start).unwrap_or(u16::MAX),
            )
        }),
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

#[cfg(test)]
mod tests;

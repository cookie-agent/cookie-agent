//! Transcript layout, collapse state, wrapping, scrolling, and hit testing.

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
    widgets::{Block, Borders},
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

fn item_layout_key(
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

fn item_layout_key_matches(
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

#[derive(Clone, Copy)]
enum Role {
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

fn for_each_item_block_id(item: &TranscriptItem, mut visit: impl FnMut(BlockId) -> bool) -> bool {
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

fn item_interaction_matches(
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

fn item_block_ids(item: &TranscriptItem) -> Vec<BlockId> {
    let mut ids = Vec::new();
    for_each_item_block_id(item, |id| {
        ids.push(id);
        true
    });
    ids
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
            reminder,
            status,
            ..
        } => match status {
            // The initial start must precede streaming, without duplicating its
            // queue preview. Claims leave the queue before the request starts;
            // the row was already anchored at admission by the reducer.
            ProducerMessageStatus::Claimed
                if matches!(producer_owner, ProducerOwner::Goal { .. })
                    && reminder.is_some_and(|reminder| {
                        reminder.kind == cookie_agent_protocol::GoalReminderKind::Started
                    }) =>
            {
                producer_message_layout(
                    *message_id,
                    producer_owner,
                    *mode,
                    body,
                    &producer_summary(producer_owner, *mode, summary.as_deref()),
                    *status,
                    context,
                )
            }
            crate::state::ProducerMessageStatus::Pending
            | crate::state::ProducerMessageStatus::Admitted
            | crate::state::ProducerMessageStatus::Claimed => ItemLayout::default(),
            crate::state::ProducerMessageStatus::Consumed => producer_message_layout(
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

fn goal_activation_layout(goal: &GoalState, width: u16, theme: &Theme) -> Vec<Line<'static>> {
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

fn goal_layout(goal: &GoalState, width: u16, theme: &Theme) -> Vec<Line<'static>> {
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

pub(super) fn producer_summary(
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

fn producer_owner_label(owner: &ProducerOwner) -> String {
    match owner {
        ProducerOwner::Plugin { plugin } => format!("plugin {plugin}"),
        ProducerOwner::Delegation { invocation_id } => format!("delegation {invocation_id}"),
        ProducerOwner::Goal { .. } => "goal controller".to_owned(),
        ProducerOwner::GoalControl { .. } => "goal control".to_owned(),
        ProducerOwner::Agent { session_id } => format!("agent {session_id}"),
    }
}

fn producer_message_layout(
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
        super::app::truncate_with_ellipsis(&header, usize::from(context.width)),
        context.theme.internal(),
    )];
    let status = match status {
        ProducerMessageStatus::Claimed => "claimed",
        ProducerMessageStatus::Consumed => "consumed",
        _ => unreachable!("only claimed starts or consumed messages enter the transcript"),
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

fn discarded_producer_message_layout(
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

fn producer_mode_label(mode: ProducerDeliveryMode) -> &'static str {
    match mode {
        ProducerDeliveryMode::Steer => "steer",
        ProducerDeliveryMode::Queue => "queue",
    }
}

fn goal_status_label(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Completed => "completed",
        GoalStatus::Cancelled => "cancelled",
    }
}

fn system_prompt_layout(
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
            "⚙ {chevron} system prompt · {} (last run){next_agent} ({line_count} lines)",
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

fn compaction_layout(
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
            "🗜 {chevron} context compacted ({kind}, {}→{} tokens)",
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

fn plugin_message_layout(
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

fn collapsible_event_block(
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

fn media_file_layout(
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
        format!("🖼 {chevron} {} · {filename}", file.media_type),
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

fn display_line_count(text: &str) -> usize {
    text.split('\n').count()
}

const MAX_EXPANDED_BODY_LINES: usize = 64;
const MAX_EXPANDED_BODY_BYTES: usize = 8 * 1024;
const MAX_EXPANDED_TOOL_OUTPUT_LINES: usize = 1024;
const MAX_EXPANDED_TOOL_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_SYSTEM_PROMPT_BODY_LINES: usize = 256;
const MAX_SYSTEM_PROMPT_BODY_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy)]
struct RenderLimits {
    lines: usize,
    bytes: usize,
}

const COLLAPSED_TOOL_OUTPUT_LIMITS: RenderLimits = RenderLimits {
    lines: MAX_EXPANDED_BODY_LINES,
    bytes: MAX_EXPANDED_BODY_BYTES,
};
const EXPANDED_TOOL_OUTPUT_LIMITS: RenderLimits = RenderLimits {
    lines: MAX_EXPANDED_TOOL_OUTPUT_LINES,
    bytes: MAX_EXPANDED_TOOL_OUTPUT_BYTES,
};

fn bounded_safe_display_text(
    text: &str,
    style: Style,
    max_lines: usize,
    max_bytes: usize,
) -> Vec<Line<'static>> {
    bounded_safe_display_lines(text.split('\n'), style, max_lines, max_bytes)
}

fn bounded_safe_display_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    style: Style,
    max_lines: usize,
    max_bytes: usize,
) -> Vec<Line<'static>> {
    let mut rendered = Vec::new();
    let mut rendered_bytes = 0;
    let mut fully_rendered_lines = 0usize;
    let mut total_lines = 0usize;

    for line in lines {
        total_lines += 1;
        if rendered.len() >= max_lines || rendered_bytes >= max_bytes {
            continue;
        }
        let available = max_bytes - rendered_bytes;
        let (sanitized, complete) = sanitized_display_prefix(line, available);
        if complete {
            rendered_bytes += sanitized.len();
            rendered.push(Line::styled(sanitized, style));
            fully_rendered_lines += 1;
            continue;
        }

        if !sanitized.is_empty() {
            rendered.push(Line::styled(sanitized, style));
        }
        rendered_bytes = max_bytes;
    }

    let omitted_lines = total_lines.saturating_sub(fully_rendered_lines);
    if omitted_lines > 0 {
        rendered.push(Line::styled(
            format!("… truncated ({omitted_lines} more lines)"),
            style,
        ));
    }
    rendered
}

fn safe_display_text(text: &str) -> String {
    sanitized_display_prefix(text, usize::MAX).0
}

fn sanitized_display_prefix(text: &str, max_bytes: usize) -> (String, bool) {
    let mut sanitized = String::with_capacity(text.len().min(max_bytes));
    for character in text.chars() {
        let character = if character.is_control() && character != '\t' {
            '\u{FFFD}'
        } else {
            character
        };
        if sanitized.len().saturating_add(character.len_utf8()) > max_bytes {
            return (sanitized, false);
        }
        sanitized.push(character);
    }
    (sanitized, true)
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
                let key = assistant_part_layout_key(state, item_id, child, context);
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
                let start_region = layout.regions.len();
                layout.lines.extend(part_layout.lines);
                layout
                    .regions
                    .extend(part_layout.regions.into_iter().map(|region| BlockRegion {
                        id: region.id,
                        start_line: start_line + region.start_line,
                        end_line: start_line + region.end_line,
                        ..region
                    }));
                context.assistant_part_ranges.push(AssistantPartRange {
                    id: child.id(),
                    key,
                    lines: start_line..layout.lines.len(),
                    regions: start_region..layout.regions.len(),
                });
            }
            AssistantChild::Tool { call_id } => {
                let child_layout =
                    tool_child_layout(state, Some(*call_id), *call_id, None, context);
                let start_line = layout.lines.len();
                layout.lines.extend(child_layout.lines);
                layout
                    .regions
                    .extend(child_layout.regions.into_iter().map(|region| BlockRegion {
                        id: region.id,
                        start_line: start_line + region.start_line,
                        end_line: start_line + region.end_line,
                        ..region
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
                name,
            } => {
                let child_layout = tool_child_layout(
                    state,
                    None,
                    BlockKey::CommittedTool {
                        turn_seq: *turn_seq,
                        content_index: *content_index,
                    },
                    Some(name.as_str()),
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
                        ..region
                    }));
            }
            AssistantChild::MediaFile {
                turn_seq,
                content_index,
                file,
            } => {
                let child_layout = media_file_layout(*turn_seq, *content_index, file, context);
                let start_line = layout.lines.len();
                layout.lines.extend(child_layout.lines);
                layout
                    .regions
                    .extend(child_layout.regions.into_iter().map(|region| BlockRegion {
                        id: region.id,
                        start_line: start_line + region.start_line,
                        end_line: start_line + region.end_line,
                        ..region
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

fn assistant_part_layout_key(
    state: &SessionState,
    item_id: u64,
    child: &AssistantChild,
    context: &TranscriptRenderContext<'_>,
) -> AssistantPartLayoutKey {
    let block_id = match child {
        AssistantChild::Thinking { id, .. } => Some(BlockId::Thinking(*id)),
        AssistantChild::Text { .. }
        | AssistantChild::Tool { .. }
        | AssistantChild::Attribution { .. }
        | AssistantChild::CommittedTool { .. }
        | AssistantChild::MediaFile { .. } => None,
    };
    let streaming = matches!(child, AssistantChild::Thinking { id, .. } if state.is_open_thinking(item_id, *id));
    let duration = match child {
        AssistantChild::Thinking { id, .. } if !streaming => state
            .thinking_duration(item_id, *id)
            .filter(|duration| duration.as_secs() >= 1),
        _ => None,
    };
    AssistantPartLayoutKey {
        version: child.version(),
        expanded: block_id
            .is_some_and(|id| context.expanded.is_some_and(|blocks| blocks.contains(&id))),
        streaming,
        dots: if streaming { context.clock_bucket } else { 0 },
        duration,
    }
}

fn splice_active_assistant_part(
    cached: &mut CachedItemLayout,
    state: &SessionState,
    item: &TranscriptItem,
    context: &mut TranscriptRenderContext<'_>,
) -> bool {
    let TranscriptItem::Assistant { id, children, .. } = item else {
        return false;
    };
    let parts = children
        .iter()
        .filter(|child| {
            matches!(
                child,
                AssistantChild::Text { .. } | AssistantChild::Thinking { .. }
            )
        })
        .collect::<Vec<_>>();
    if parts.len() != cached.assistant_parts.len() {
        return false;
    }
    let mut dirty = None;
    for (index, (child, range)) in parts.iter().zip(&cached.assistant_parts).enumerate() {
        if child.id() != range.id {
            return false;
        }
        let key = assistant_part_layout_key(state, *id, child, context);
        if key != range.key && dirty.replace((index, *child, key)).is_some() {
            return false;
        }
    }
    let Some((dirty_index, child, key)) = dirty else {
        return false;
    };
    if !state.is_open_assistant_part(*id, child.id()) {
        return false;
    }

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
    *context.assistant_part_layout_passes = context.assistant_part_layout_passes.wrapping_add(1);

    let old = cached.assistant_parts[dirty_index].clone();
    let old_line_len = old.lines.len();
    let new_line_len = part_layout.lines.len();
    let line_delta = isize::try_from(new_line_len).unwrap_or(isize::MAX)
        - isize::try_from(old_line_len).unwrap_or(isize::MAX);
    cached
        .layout
        .lines
        .splice(old.lines.clone(), part_layout.lines);

    let new_regions = part_layout
        .regions
        .into_iter()
        .map(|region| BlockRegion {
            id: region.id,
            start_line: old.lines.start + region.start_line,
            end_line: old.lines.start + region.end_line,
            ..region
        })
        .collect::<Vec<_>>();
    let old_region_len = old.regions.len();
    let new_region_len = new_regions.len();
    cached
        .layout
        .regions
        .splice(old.regions.clone(), new_regions);
    let region_delta = isize::try_from(new_region_len).unwrap_or(isize::MAX)
        - isize::try_from(old_region_len).unwrap_or(isize::MAX);
    let shifted_region_start = old.regions.start + new_region_len;
    for region in &mut cached.layout.regions[shifted_region_start..] {
        region.start_line = region
            .start_line
            .checked_add_signed(line_delta)
            .expect("assistant region offset remains valid");
        region.end_line = region
            .end_line
            .checked_add_signed(line_delta)
            .expect("assistant region offset remains valid");
    }

    let range = &mut cached.assistant_parts[dirty_index];
    range.key = key;
    range.lines.end = range.lines.start + new_line_len;
    range.regions.end = range.regions.start + new_region_len;
    for range in &mut cached.assistant_parts[dirty_index + 1..] {
        range.lines.start = range
            .lines
            .start
            .checked_add_signed(line_delta)
            .expect("assistant line offset remains valid");
        range.lines.end = range
            .lines
            .end
            .checked_add_signed(line_delta)
            .expect("assistant line offset remains valid");
        range.regions.start = range
            .regions
            .start
            .checked_add_signed(region_delta)
            .expect("assistant region index remains valid");
        range.regions.end = range
            .regions
            .end
            .checked_add_signed(region_delta)
            .expect("assistant region index remains valid");
    }
    true
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
            lines: {
                let markdown_width = width.saturating_sub(u16::from(width >= 3) * 2);
                crate::markdown::render_markdown_lines_width(
                    markdown,
                    theme,
                    highlighter,
                    markdown_width,
                )
                .into_iter()
                .flat_map(|line| assistant_markdown_body_line(line, width, theme))
                .collect()
            },
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
            let header_lines = lines.len();
            if key.expanded {
                lines.extend(body);
            }
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
        AssistantChild::Tool { .. }
        | AssistantChild::Attribution { .. }
        | AssistantChild::CommittedTool { .. }
        | AssistantChild::MediaFile { .. } => {
            unreachable!("tool children use tool_child_layout")
        }
    }
}

fn tool_icon(title: &str) -> &'static str {
    match title {
        "bash" => "💻",
        "read" => "📖",
        "write" | "edit" => "✏️",
        "delegate_subagent" | "get_subagent_result" | "steer_subagent" | "cancel_subagent" => "🤖",
        "skill" => "✨",
        "goal_get" | "goal_update" => "🎯",
        _ => "🔨",
    }
}

/// The compact role name `tool_block_lines` brackets onto a narrow tool row.
fn tool_row_short(role: Role) -> &'static str {
    match role {
        Role::ToolRunning => "T…",
        Role::ToolSuccess => "T✓",
        Role::ToolFailure => "T!",
        _ => "T",
    }
}

/// Narrowest row where `tool_block_lines` can still keep the assistant gutter.
const TOOL_HEADER_GUTTER_MIN_COLUMNS: u16 = 8;
/// The `"│ "` assistant gutter a header row hangs behind at normal widths.
const TOOL_HEADER_GUTTER_COLUMNS: usize = 2;
/// The full `"[T…] "` role label that replaces the gutter below
/// `TOOL_HEADER_GUTTER_MIN_COLUMNS`, leaving the row no room for a wide icon.
const TOOL_HEADER_LABEL_COLUMNS: usize = 5;

/// Columns the compact role label may spend on a narrow row. It shortens with
/// the row and always leaves `reserve` columns for text — at least one, and as
/// much more as the widest grapheme the row has to carry whole needs — so
/// neither the label nor its wrapped continuation can exceed the viewport.
fn tool_row_label_columns(width: u16, reserve: usize) -> usize {
    usize::from(width)
        .saturating_sub(reserve.max(1))
        .clamp(1, TOOL_HEADER_LABEL_COLUMNS)
}

/// The bracketed role label itself, shortened to that budget: `"[T…] "` while it
/// fits, then `"[T…]"`, `"[T"`, and `"[…"` down to a single column.
fn tool_row_prefix(short: &str, width: u16, reserve: usize) -> String {
    let room = tool_row_label_columns(width, reserve);
    let full = format!("[{short}] ");
    if UnicodeWidthStr::width(full.as_str()) <= room {
        return full;
    }
    super::app::truncate_with_ellipsis(&format!("[{short}]"), room)
}

/// Flatten an on-wire primary argument for single-line header display. The wire
/// type is byte-capped only, so control characters reach the client: map each to
/// a space before collapsing whitespace runs.
fn flatten_header_argument(argument: &str) -> String {
    argument
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Columns the header text may use at `width`, after the prefix
/// `tool_block_lines` puts in front of the row. The header never wraps, so the
/// markers, label, argument, and status suffix share exactly this budget.
fn tool_header_content_width(width: u16) -> usize {
    let gutter = if width >= TOOL_HEADER_GUTTER_MIN_COLUMNS {
        TOOL_HEADER_GUTTER_COLUMNS
    } else {
        // The header is cut with `truncate_with_ellipsis`, which already counts
        // display widths, so it only needs a single column to hang its tail on.
        tool_row_label_columns(width, 1)
    };
    usize::from(width).saturating_sub(gutter)
}

/// The `{icon} {chevron} ` markers in front of the header title. A row too
/// narrow for the icon drops it (its role colour and `[T…]` label already say
/// what ran), then the separating space, and keeps the bare chevron last because
/// that is the only expand/collapse cue the row has.
fn tool_header_chrome(icon: &str, chevron: char, width: u16) -> String {
    let room = tool_header_content_width(width);
    [
        format!("{icon} {chevron} "),
        format!("{chevron} "),
        chevron.to_string(),
    ]
    .into_iter()
    .find(|chrome| UnicodeWidthStr::width(chrome.as_str()) <= room)
    .unwrap_or_default()
}

/// The finished header row and its `{label} {argument}` title, which the
/// expanded `path:` row compares against the raw path to see whether anything
/// was lost. Text that does not fit drops out rather than wrapping: the status
/// suffix goes first (the row style already carries the status), then the
/// argument, then the label.
fn tool_header_row(
    tool: &crate::state::ToolCallState,
    chevron: char,
    suffix: &str,
    width: u16,
) -> (String, String) {
    let chrome = tool_header_chrome(tool_icon(tool.presentation.title.as_str()), chevron, width);
    let content =
        tool_header_content_width(width).saturating_sub(UnicodeWidthStr::width(chrome.as_str()));
    let (budget, suffix) = if UnicodeWidthStr::width(suffix) <= content {
        (content - UnicodeWidthStr::width(suffix), suffix)
    } else {
        (content, "")
    };
    let title = tool_header_title(tool, budget);
    // An empty title must not leave the markers' separator space behind.
    let chrome = if title.is_empty() {
        chrome.trim_end().to_owned()
    } else {
        chrome
    };
    (
        format!("{chrome}{title}{suffix}").trim_end().to_owned(),
        title,
    )
}

/// Columns the primary argument may occupy inside the header's `budget`: what
/// the `{label} ` prefix leaves over. Zero means the label alone fills the row,
/// so the argument must be dropped to stay on one line.
fn header_argument_width(label: &str, budget: usize) -> usize {
    budget.saturating_sub(UnicodeWidthStr::width(label) + 1)
}

/// Abbreviate a primary argument to at most `cap` display columns. Path-shaped
/// arguments keep their head and tail; everything else cuts at the right edge.
fn abbreviate_tool_argument(title: &str, argument: &str, cap: usize) -> String {
    let argument = flatten_header_argument(argument);
    if UnicodeWidthStr::width(argument.as_str()) <= cap {
        return argument;
    }
    if matches!(title, "read" | "write" | "edit") {
        if let Some((head, _)) = argument.split_once(['/', '\\']) {
            let tail = argument.rsplit(['/', '\\']).next().unwrap_or_default();
            let abbreviated = format!("{head}/…/{tail}");
            if UnicodeWidthStr::width(abbreviated.as_str()) <= cap {
                return abbreviated;
            }
        }
        let mut used = 1;
        let suffix = argument
            .graphemes(true)
            .rev()
            .take_while(|grapheme| {
                used += UnicodeWidthStr::width(*grapheme);
                used <= cap
            })
            .collect::<Vec<_>>();
        return format!("…{}", suffix.into_iter().rev().collect::<String>());
    }
    super::app::truncate_with_ellipsis(&argument, cap)
}

fn tool_header_title(tool: &crate::state::ToolCallState, budget: usize) -> String {
    let title = tool.presentation.title.as_str();
    let label = if title == "read" { "Read" } else { title };
    let argument_width = header_argument_width(label, budget);
    let Some(argument) = tool.presentation.primary_argument.as_ref() else {
        return super::app::truncate_with_ellipsis(label, budget);
    };
    if argument_width == 0 {
        // The label alone fills the row (long plugin names at narrow widths):
        // keep the label legible and drop the argument rather than wrap.
        return super::app::truncate_with_ellipsis(label, budget);
    }
    format!(
        "{label} {}",
        abbreviate_tool_argument(title, argument.as_str(), argument_width)
    )
}

/// A compact or expanded tool row inside its owning assistant item. Running
/// pulses a suffix; terminal failures retain their exact concise markers.
/// `pending_name` identifies a committed placeholder whose execution has not
/// started yet: it renders a neutral pending row, never an error.
fn tool_child_layout(
    state: &SessionState,
    call_id: Option<cookie_agent_protocol::ToolCallId>,
    block_key: impl Into<BlockKey>,
    pending_name: Option<&str>,
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
        let (role, text) = match pending_name {
            // The turn committed this call but execution has not published
            // its start yet; the placeholder links by content index shortly.
            Some(name) => (
                Role::ToolRunning,
                format!("{} ▸ {} · pending", tool_icon(name), name),
            ),
            None => (Role::Error, "tool: unavailable payload".to_owned()),
        };
        let lines = if pending_name.is_some() {
            tool_block_lines(
                role,
                vec![ToolBodyLine::wrapped(Line::from(text))],
                context.width,
                context.theme,
            )
            .lines
        } else {
            role_block(role, vec![Line::from(text)], context.width, context.theme)
        };
        return ItemLayout {
            regions: vec![BlockRegion {
                id: block_id,
                start_line: 0,
                end_line: lines.len(),
                header_lines: None,
                header_gutter: None,
            }],
            lines,
            user_seq: None,
        };
    };
    let arguments = is_expanded
        .then(|| ParsedToolArguments::parse(&tool.arguments))
        .flatten();
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
    let output_block_id = |section| call_id.map(|call_id| BlockId::ToolOutput { call_id, section });
    let section_expanded = |section| {
        output_block_id(section)
            .is_some_and(|id| context.expanded.is_some_and(|blocks| blocks.contains(&id)))
    };
    let section_count = usize::from(!tool.detail.is_empty());
    let any_output_expanded = [
        ToolOutputSection::Detail,
        ToolOutputSection::Stdout,
        ToolOutputSection::Stderr,
    ]
    .into_iter()
    .any(section_expanded);
    let limits = if any_output_expanded {
        EXPANDED_TOOL_OUTPUT_LIMITS
    } else {
        COLLAPSED_TOOL_OUTPUT_LIMITS
    };
    let mut budget = RenderBudget::new(limits);
    let mut remaining_sections = section_count;
    let tool_name = tool.presentation.title.as_str();
    let chevron = if is_expanded { '▾' } else { '▸' };
    let (header, title) = tool_header_row(tool, chevron, &suffix, context.width);
    let mut body = vec![ToolBodyLine::wrapped(Line::from(header))];
    if is_expanded {
        if tool_name == "read"
            && let Some(path) = tool.presentation.primary_argument.as_ref()
            && title != format!("Read {path}")
        {
            // Tabs survive `safe_display_text` but the renderer drops control
            // characters outright, so flatten them first: the expanded path must
            // show the same text the collapsed header flattened to.
            let path = safe_display_text(&path.as_str().replace('\t', " "));
            body.push(ToolBodyLine::wrapped(Line::from(format!("path: {path}"))));
        }
        if tool_name != "read" {
            let command = arguments.as_ref().and_then(|args| args.command.as_deref());
            let arguments_line = if tool_name == "bash"
                && let Some(command) = command
            {
                let (command, complete) = sanitized_display_prefix(command, 2 * 1024);
                format!("❯ {command}{}", if complete { "" } else { "…" })
            } else {
                format!(
                    "arguments: {}",
                    display_tool_arguments(tool, arguments.as_ref())
                )
            };
            body.push(ToolBodyLine::wrapped(Line::from(arguments_line)));
        }
        if !tool.detail.is_empty() {
            remaining_sections -= 1;
            body.extend(tool_body_lines(
                tool,
                arguments.as_ref(),
                context,
                ToolOutputSection::Detail,
                section_expanded(ToolOutputSection::Detail),
                &mut budget,
                remaining_sections,
            ));
        }
    }
    if tool_name == "bash" && is_expanded {
        for line in &mut body {
            line.banded = line.output_toggle.is_none();
        }
    }
    let rendered = tool_block_lines(role, body, context.width, context.theme);
    let mut regions = vec![BlockRegion {
        id: block_id,
        start_line: 0,
        end_line: rendered.lines.len(),
        header_lines: Some(rendered.header_lines),
        header_gutter: Some(header_gutter_columns(&rendered)),
    }];
    if let Some(call_id) = call_id {
        let chrome = rendered.chrome;
        regions.extend(rendered.output_toggles.into_iter().map(
            |(section, start_line, end_line)| BlockRegion {
                id: BlockId::ToolOutput { call_id, section },
                start_line,
                end_line,
                header_lines: None,
                // A notice row hangs behind the same gutter as the rows it
                // stands in for.
                header_gutter: chrome.get(start_line).copied(),
            },
        ));
    }
    ItemLayout {
        regions,
        lines: rendered.lines,
        user_seq: None,
    }
}

#[derive(serde::Deserialize)]
struct ParsedToolArguments<'a> {
    // Deliberately strict: non-string edit/write fields reject the structured
    // view and fall back to the bounded raw-arguments rendering.
    #[serde(borrow, rename = "filePath")]
    file_path: Option<Cow<'a, str>>,
    #[serde(borrow)]
    path: Option<Cow<'a, str>>,
    #[serde(borrow, rename = "oldString")]
    before: Option<Cow<'a, str>>,
    #[serde(borrow, rename = "newString")]
    after: Option<Cow<'a, str>>,
    #[serde(borrow)]
    content: Option<Cow<'a, str>>,
    #[serde(borrow)]
    command: Option<Cow<'a, str>>,
}

impl<'a> ParsedToolArguments<'a> {
    fn parse(arguments: &'a str) -> Option<Self> {
        serde_json::from_str(arguments).ok()
    }

    fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref().or(self.path.as_deref())
    }
}

fn display_tool_arguments(
    tool: &crate::state::ToolCallState,
    arguments: Option<&ParsedToolArguments<'_>>,
) -> String {
    if matches!(tool.presentation.title.as_str(), "edit" | "write")
        && let Some(path) = arguments.and_then(ParsedToolArguments::file_path)
    {
        return format!("filePath={path} (content shown below)");
    }
    const MAX_ARGUMENT_BYTES: usize = 2 * 1024;
    let (arguments, complete) = sanitized_display_prefix(&tool.arguments, MAX_ARGUMENT_BYTES);
    if complete {
        arguments
    } else {
        format!("{arguments}…")
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

enum ToolBodyLineKind {
    Wrapped,
    Code {
        first_gutter: Vec<Span<'static>>,
        continuation_gutter: Vec<Span<'static>>,
    },
}

struct ToolBodyLine {
    line: Line<'static>,
    kind: ToolBodyLineKind,
    output_toggle: Option<ToolOutputSection>,
    banded: bool,
}

impl ToolBodyLine {
    fn wrapped(line: Line<'static>) -> Self {
        Self {
            line,
            kind: ToolBodyLineKind::Wrapped,
            output_toggle: None,
            banded: false,
        }
    }

    fn code(line: Line<'static>) -> Self {
        Self::guttered_code(line, Vec::new(), Vec::new())
    }

    fn guttered_code(
        line: Line<'static>,
        first_gutter: Vec<Span<'static>>,
        continuation_gutter: Vec<Span<'static>>,
    ) -> Self {
        Self {
            line,
            kind: ToolBodyLineKind::Code {
                first_gutter,
                continuation_gutter,
            },
            output_toggle: None,
            banded: false,
        }
    }

    fn toggle(line: Line<'static>, section: ToolOutputSection) -> Self {
        Self {
            line,
            kind: ToolBodyLineKind::Wrapped,
            output_toggle: Some(section),
            banded: false,
        }
    }
}

const OUTPUT_NOTICE_RESERVE_BYTES: usize = 96;

struct RenderBudget {
    remaining: RenderLimits,
}

impl RenderBudget {
    fn new(limits: RenderLimits) -> Self {
        Self { remaining: limits }
    }

    fn section_capacity(&self, limits: RenderLimits, future_sections: usize) -> RenderLimits {
        let reserved_sections = future_sections.saturating_add(1);
        RenderLimits {
            lines: limits
                .lines
                .saturating_sub(1)
                .min(self.remaining.lines.saturating_sub(reserved_sections)),
            bytes: limits
                .bytes
                .saturating_sub(OUTPUT_NOTICE_RESERVE_BYTES)
                .min(
                    self.remaining
                        .bytes
                        .saturating_sub(reserved_sections * OUTPUT_NOTICE_RESERVE_BYTES),
                ),
        }
    }

    fn consume(&mut self, capacity: &mut RenderLimits, text: &str) -> Option<(String, bool)> {
        if capacity.lines == 0 || capacity.bytes == 0 {
            return None;
        }
        let available = capacity.bytes.min(self.remaining.bytes);
        let (text, complete) = sanitized_display_prefix(text, available);
        if text.is_empty() && !complete {
            return None;
        }
        capacity.lines -= 1;
        capacity.bytes = capacity.bytes.saturating_sub(text.len());
        self.remaining.lines = self.remaining.lines.saturating_sub(1);
        self.remaining.bytes = self.remaining.bytes.saturating_sub(text.len());
        Some((text, complete))
    }

    fn consume_notice(&mut self, text: &str) -> bool {
        if self.remaining.lines == 0 || self.remaining.bytes < text.len() {
            return false;
        }
        self.remaining.lines -= 1;
        self.remaining.bytes -= text.len();
        true
    }
}

struct SectionRenderer<'a> {
    budget: &'a mut RenderBudget,
    capacity: RenderLimits,
    fully_rendered: usize,
}

impl<'a> SectionRenderer<'a> {
    fn new(budget: &'a mut RenderBudget, expanded: bool, future_sections: usize) -> Self {
        let limits = output_section_limits(expanded);
        let capacity = budget.section_capacity(limits, future_sections);
        Self {
            budget,
            capacity,
            fully_rendered: 0,
        }
    }

    fn take(&mut self, text: &str) -> Option<String> {
        let (text, complete) = self.budget.consume(&mut self.capacity, text)?;
        self.fully_rendered += usize::from(complete);
        Some(text)
    }

    fn exhausted(&self) -> bool {
        self.capacity.lines == 0 || self.capacity.bytes == 0
    }
}

fn output_section_limits(expanded: bool) -> RenderLimits {
    if expanded {
        EXPANDED_TOOL_OUTPUT_LIMITS
    } else {
        COLLAPSED_TOOL_OUTPUT_LIMITS
    }
}

/// Expanded tool detail lines. File reads use source line numbers carried by
/// the result. Edit/write arguments and strict unified-diff output use diff
/// gutters. All sections consume one aggregate per-tool render budget.
fn tool_body_lines(
    tool: &crate::state::ToolCallState,
    arguments: Option<&ParsedToolArguments<'_>>,
    context: &TranscriptRenderContext<'_>,
    section: ToolOutputSection,
    expanded: bool,
    budget: &mut RenderBudget,
    future_sections: usize,
) -> Vec<ToolBodyLine> {
    if tool.status == ToolStatus::Completed {
        let path = arguments.and_then(ParsedToolArguments::file_path);
        let language = path.and_then(path_extension);
        if tool.presentation.title.as_str() == "read"
            && let Some(read) = parse_read_output(&tool.detail)
        {
            return render_read_output(
                read,
                language,
                section,
                expanded,
                budget,
                future_sections,
                context,
            );
        }
        if matches!(tool.presentation.title.as_str(), "edit" | "write")
            && let Some(diff) = tool_diff(tool, arguments)
        {
            return render_diff_output(
                &diff,
                language,
                section,
                expanded,
                budget,
                future_sections,
                context,
            );
        }
    }
    generic_output_lines(
        None,
        OutputText::complete(&tool.detail),
        section,
        expanded,
        budget,
        future_sections,
        context.theme,
    )
}

fn path_extension(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next()?;
    name.rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
        .map(|(_, extension)| extension)
}

struct ReadOutput<'a> {
    preamble: &'a str,
    content: &'a str,
    metadata: &'a str,
}

fn parse_read_output(detail: &str) -> Option<ReadOutput<'_>> {
    const OPEN: &str = "<content>\n";
    const CLOSE: &str = "</content>";
    let open = detail.find(OPEN)?;
    let content_start = open + OPEN.len();
    let remaining = &detail[content_start..];
    let close_offset = if remaining.starts_with(CLOSE) {
        0
    } else {
        remaining.find("\n</content>")? + 1
    };
    let close = content_start + close_offset;
    let content = detail[content_start..close].trim_end_matches('\n');
    if !content
        .lines()
        .all(|line| parse_numbered_read_line(line).is_some())
    {
        return None;
    }
    Some(ReadOutput {
        preamble: detail[..open].trim_end_matches('\n'),
        content,
        metadata: detail[close + CLOSE.len()..].trim_start_matches('\n'),
    })
}

fn parse_numbered_read_line(line: &str) -> Option<(usize, &str)> {
    let (number, text) = line.split_once(": ")?;
    Some((number.parse().ok()?, text))
}

fn render_read_output(
    read: ReadOutput<'_>,
    language: Option<&str>,
    section: ToolOutputSection,
    expanded: bool,
    budget: &mut RenderBudget,
    future_sections: usize,
    context: &TranscriptRenderContext<'_>,
) -> Vec<ToolBodyLine> {
    let preamble = || {
        read.preamble
            .lines()
            .filter(|line| !line.starts_with('<') && !line.starts_with("Read file "))
    };
    let metadata = || read.metadata.lines().filter(|line| !line.is_empty());
    let total_lines = preamble().count() + read.content.lines().count() + metadata().count();
    let number_width = read
        .content
        .lines()
        .filter_map(|line| parse_numbered_read_line(line).map(|(number, _)| number))
        .max()
        .unwrap_or(1)
        .max(1)
        .ilog10() as usize
        + 1;
    let mut renderer = SectionRenderer::new(budget, expanded, future_sections);
    let mut output = Vec::new();
    for line in preamble() {
        let Some(line) = renderer.take(line) else {
            break;
        };
        output.push(ToolBodyLine::wrapped(Line::from(Span::styled(
            line,
            context.theme.muted(),
        ))));
    }
    let mut rows = Vec::new();
    if !renderer.exhausted() {
        for line in read.content.lines() {
            let (number, text) = parse_numbered_read_line(line).expect("validated read row");
            let Some(text) = renderer.take(text) else {
                break;
            };
            rows.push((number, text));
        }
    }
    let source = rows
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let highlighted = language.map(|language| {
        context.highlighter.highlight_stable(
            &crate::markdown::normalized_language(language),
            &source,
            context.theme,
        )
    });
    for (index, (number, text)) in rows.into_iter().enumerate() {
        let line = highlighted
            .as_ref()
            .and_then(|lines| lines.get(index))
            .cloned()
            .unwrap_or_else(|| Line::from(text));
        output.push(ToolBodyLine::guttered_code(
            line,
            vec![
                Span::styled(
                    format!("{number:>number_width$}"),
                    context.theme.code_gutter(),
                ),
                Span::styled(" │ ", context.theme.code_gutter()),
            ],
            vec![
                Span::styled(" ".repeat(number_width), context.theme.code_gutter()),
                Span::styled(" │ ", context.theme.code_gutter()),
            ],
        ));
    }
    if !renderer.exhausted() {
        for line in metadata() {
            let Some(line) = renderer.take(line) else {
                break;
            };
            output.push(ToolBodyLine::wrapped(Line::from(Span::styled(
                line,
                context.theme.muted(),
            ))));
        }
    }
    let omitted = total_lines.saturating_sub(renderer.fully_rendered);
    append_output_notice(
        &mut output,
        omitted,
        expanded,
        section,
        renderer.budget,
        context.theme,
    );
    output
}

#[derive(Clone, Copy)]
struct OutputText<'a> {
    text: &'a str,
    original_lines: usize,
}

impl<'a> OutputText<'a> {
    fn complete(text: &'a str) -> Self {
        Self {
            text,
            original_lines: text.lines().count(),
        }
    }
}

fn generic_output_lines(
    heading: Option<&str>,
    source: OutputText<'_>,
    section: ToolOutputSection,
    expanded: bool,
    budget: &mut RenderBudget,
    future_sections: usize,
    theme: &Theme,
) -> Vec<ToolBodyLine> {
    let total_lines = usize::from(heading.is_some()) + source.original_lines;
    let mut renderer = SectionRenderer::new(budget, expanded, future_sections);
    let mut output = Vec::new();
    if let Some(heading) = heading
        && let Some(heading) = renderer.take(heading)
    {
        output.push(ToolBodyLine::wrapped(Line::from(heading)));
    }
    if !renderer.exhausted() {
        for line in source.text.lines() {
            let Some(line) = renderer.take(line) else {
                break;
            };
            output.push(ToolBodyLine::wrapped(Line::from(line)));
        }
    }
    let omitted = total_lines.saturating_sub(renderer.fully_rendered);
    append_output_notice(
        &mut output,
        omitted,
        expanded,
        section,
        renderer.budget,
        theme,
    );
    output
}

fn append_output_notice(
    output: &mut Vec<ToolBodyLine>,
    omitted: usize,
    expanded: bool,
    section: ToolOutputSection,
    budget: &mut RenderBudget,
    theme: &Theme,
) {
    let notice = if omitted > 0 && expanded {
        format!("… {omitted} more lines (maximum shown; click to collapse)")
    } else if omitted > 0 {
        format!("… {omitted} more lines (click to expand)")
    } else if expanded {
        "▴ click to collapse".to_owned()
    } else {
        return;
    };
    if budget.consume_notice(&notice) {
        output.push(ToolBodyLine::toggle(
            Line::from(Span::styled(notice, theme.muted())),
            section,
        ));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiffRowKind {
    Hunk,
    Added(usize),
    Removed(usize),
    Context(usize),
    NoNewline,
    Metadata,
}

struct DiffRow<'a> {
    kind: DiffRowKind,
    text: Cow<'a, str>,
}

enum ToolDiff<'a> {
    Unified(&'a str),
    Edit {
        before: &'a str,
        after: &'a str,
        metadata: &'a str,
    },
    Write {
        content: &'a str,
        metadata: &'a str,
    },
}

fn tool_diff<'a>(
    tool: &'a crate::state::ToolCallState,
    arguments: Option<&'a ParsedToolArguments<'_>>,
) -> Option<ToolDiff<'a>> {
    if is_unified_diff(&tool.detail) {
        return Some(ToolDiff::Unified(&tool.detail));
    }
    match tool.presentation.title.as_str() {
        "edit" => {
            let arguments = arguments?;
            Some(ToolDiff::Edit {
                before: arguments.before.as_deref()?,
                after: arguments.after.as_deref()?,
                metadata: &tool.detail,
            })
        }
        "write" => {
            let arguments = arguments?;
            Some(ToolDiff::Write {
                content: arguments.content.as_deref()?,
                metadata: &tool.detail,
            })
        }
        _ => None,
    }
}

fn is_unified_diff(text: &str) -> bool {
    let mut has_file_header = false;
    for line in text.lines() {
        has_file_header |= line.starts_with("diff --git ");
        if parse_hunk_starts(line).is_some()
            || has_file_header && (line.starts_with("Binary files ") || line == "GIT binary patch")
        {
            return true;
        }
    }
    false
}

fn parse_hunk_starts(line: &str) -> Option<(usize, usize)> {
    let ranges = line.strip_prefix("@@ -")?.split_once(" @@")?.0;
    let (old, new) = ranges.split_once(" +")?;
    let start = |range: &str| range.split(',').next()?.parse::<usize>().ok();
    Some((start(old)?, start(new)?))
}

fn diff_range(count: usize) -> String {
    match count {
        0 => "0,0".to_owned(),
        1 => "1".to_owned(),
        count => format!("1,{count}"),
    }
}

fn for_each_diff_row<'a>(diff: &'a ToolDiff<'a>, mut visit: impl FnMut(DiffRow<'a>) -> bool) {
    match diff {
        ToolDiff::Unified(text) => for_each_unified_diff_row(text, visit),
        ToolDiff::Edit {
            before,
            after,
            metadata,
        } => {
            let before_count = before.lines().count();
            let after_count = after.lines().count();
            if !visit(DiffRow {
                kind: DiffRowKind::Hunk,
                text: Cow::Owned(format!(
                    "@@ -{} +{} @@",
                    diff_range(before_count),
                    diff_range(after_count)
                )),
            }) {
                return;
            }
            for (index, text) in before.lines().enumerate() {
                if !visit(DiffRow {
                    kind: DiffRowKind::Removed(index + 1),
                    text: Cow::Borrowed(text),
                }) {
                    return;
                }
            }
            if !before.is_empty()
                && !before.ends_with('\n')
                && !visit(DiffRow {
                    kind: DiffRowKind::NoNewline,
                    text: Cow::Borrowed("\\ No newline at end of file"),
                })
            {
                return;
            }
            for (index, text) in after.lines().enumerate() {
                if !visit(DiffRow {
                    kind: DiffRowKind::Added(index + 1),
                    text: Cow::Borrowed(text),
                }) {
                    return;
                }
            }
            if !after.is_empty()
                && !after.ends_with('\n')
                && !visit(DiffRow {
                    kind: DiffRowKind::NoNewline,
                    text: Cow::Borrowed("\\ No newline at end of file"),
                })
            {
                return;
            }
            for line in metadata.lines() {
                if !visit(DiffRow {
                    kind: DiffRowKind::Metadata,
                    text: Cow::Borrowed(line),
                }) {
                    return;
                }
            }
        }
        ToolDiff::Write { content, metadata } => {
            let count = content.lines().count();
            if !visit(DiffRow {
                kind: DiffRowKind::Hunk,
                text: Cow::Owned(format!("@@ -0,0 +{} @@", diff_range(count))),
            }) {
                return;
            }
            for (index, text) in content.lines().enumerate() {
                if !visit(DiffRow {
                    kind: DiffRowKind::Added(index + 1),
                    text: Cow::Borrowed(text),
                }) {
                    return;
                }
            }
            if !content.is_empty()
                && !content.ends_with('\n')
                && !visit(DiffRow {
                    kind: DiffRowKind::NoNewline,
                    text: Cow::Borrowed("\\ No newline at end of file"),
                })
            {
                return;
            }
            for line in metadata.lines() {
                if !visit(DiffRow {
                    kind: DiffRowKind::Metadata,
                    text: Cow::Borrowed(line),
                }) {
                    return;
                }
            }
        }
    }
}

fn for_each_unified_diff_row<'a>(text: &'a str, mut visit: impl FnMut(DiffRow<'a>) -> bool) {
    let mut old_line = 0;
    let mut new_line = 0;
    let mut in_hunk = false;
    for line in text.lines() {
        let row = if let Some((old_start, new_start)) = parse_hunk_starts(line) {
            old_line = old_start;
            new_line = new_start;
            in_hunk = true;
            DiffRow {
                kind: DiffRowKind::Hunk,
                text: Cow::Borrowed(line),
            }
        } else if in_hunk && line.starts_with('+') && !line.starts_with("+++") {
            let row = DiffRow {
                kind: DiffRowKind::Added(new_line),
                text: Cow::Borrowed(&line[1..]),
            };
            new_line += 1;
            row
        } else if in_hunk && line.starts_with('-') && !line.starts_with("---") {
            let row = DiffRow {
                kind: DiffRowKind::Removed(old_line),
                text: Cow::Borrowed(&line[1..]),
            };
            old_line += 1;
            row
        } else if in_hunk && let Some(content) = line.strip_prefix(' ') {
            let row = DiffRow {
                kind: DiffRowKind::Context(new_line),
                text: Cow::Borrowed(content),
            };
            old_line += 1;
            new_line += 1;
            row
        } else if line == "\\ No newline at end of file" {
            DiffRow {
                kind: DiffRowKind::NoNewline,
                text: Cow::Borrowed(line),
            }
        } else {
            DiffRow {
                kind: DiffRowKind::Metadata,
                text: Cow::Borrowed(line),
            }
        };
        if !visit(row) {
            break;
        }
    }
}

struct RenderedDiffRow {
    kind: DiffRowKind,
    text: String,
}

fn diff_total_lines(diff: &ToolDiff<'_>) -> usize {
    match diff {
        ToolDiff::Unified(text) => text.lines().count(),
        ToolDiff::Edit {
            before,
            after,
            metadata,
        } => {
            1 + before.lines().count()
                + usize::from(!before.is_empty() && !before.ends_with('\n'))
                + after.lines().count()
                + usize::from(!after.is_empty() && !after.ends_with('\n'))
                + metadata.lines().count()
        }
        ToolDiff::Write { content, metadata } => {
            1 + content.lines().count()
                + usize::from(!content.is_empty() && !content.ends_with('\n'))
                + metadata.lines().count()
        }
    }
}

fn render_diff_output(
    diff: &ToolDiff<'_>,
    language: Option<&str>,
    section: ToolOutputSection,
    expanded: bool,
    budget: &mut RenderBudget,
    future_sections: usize,
    context: &TranscriptRenderContext<'_>,
) -> Vec<ToolBodyLine> {
    let total_lines = diff_total_lines(diff);
    let mut renderer = SectionRenderer::new(budget, expanded, future_sections);
    let mut rows = Vec::new();
    for_each_diff_row(diff, |row| {
        if renderer.exhausted() {
            return false;
        }
        let Some(text) = renderer.take(row.text.as_ref()) else {
            return false;
        };
        rows.push(RenderedDiffRow {
            kind: row.kind,
            text,
        });
        true
    });
    let max_number = rows
        .iter()
        .filter_map(|row| match row.kind {
            DiffRowKind::Added(number)
            | DiffRowKind::Removed(number)
            | DiffRowKind::Context(number) => Some(number),
            DiffRowKind::Hunk | DiffRowKind::NoNewline | DiffRowKind::Metadata => None,
        })
        .max()
        .unwrap_or(1);
    let number_width = max_number.max(1).ilog10() as usize + 1;
    let code_source = rows
        .iter()
        .filter_map(|row| match row.kind {
            DiffRowKind::Added(_) | DiffRowKind::Removed(_) | DiffRowKind::Context(_) => {
                Some(row.text.as_str())
            }
            DiffRowKind::Hunk | DiffRowKind::NoNewline | DiffRowKind::Metadata => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let highlighted = language.map(|language| {
        context.highlighter.highlight_stable(
            &crate::markdown::normalized_language(language),
            &code_source,
            context.theme,
        )
    });
    let mut highlight_index = 0;
    let mut output = Vec::new();
    for row in rows {
        match row.kind {
            DiffRowKind::Hunk => output.push(ToolBodyLine::code(Line::from(Span::styled(
                row.text,
                context.theme.diff_hunk(),
            )))),
            DiffRowKind::NoNewline | DiffRowKind::Metadata => {
                output.push(ToolBodyLine::code(Line::from(Span::styled(
                    row.text,
                    context.theme.code_gutter(),
                ))));
            }
            DiffRowKind::Added(number)
            | DiffRowKind::Removed(number)
            | DiffRowKind::Context(number) => {
                let (marker, marker_style) = match row.kind {
                    DiffRowKind::Added(_) => ("+", context.theme.diff_added()),
                    DiffRowKind::Removed(_) => ("-", context.theme.diff_removed()),
                    DiffRowKind::Context(_) => (" ", context.theme.code_gutter()),
                    DiffRowKind::Hunk | DiffRowKind::NoNewline | DiffRowKind::Metadata => {
                        unreachable!()
                    }
                };
                let line = highlighted
                    .as_ref()
                    .and_then(|lines| lines.get(highlight_index))
                    .cloned()
                    .unwrap_or_else(|| Line::from(row.text));
                highlight_index += 1;
                output.push(ToolBodyLine::guttered_code(
                    line,
                    vec![
                        Span::styled(
                            format!("{number:>number_width$}"),
                            context.theme.code_gutter(),
                        ),
                        Span::styled(format!(" {marker} │ "), marker_style),
                    ],
                    vec![
                        Span::styled(" ".repeat(number_width), context.theme.code_gutter()),
                        Span::styled("   │ ", marker_style),
                    ],
                ));
            }
        }
    }
    let omitted = total_lines.saturating_sub(renderer.fully_rendered);
    append_output_notice(
        &mut output,
        omitted,
        expanded,
        section,
        renderer.budget,
        context.theme,
    );
    output
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
/// gutter tree. A block whose run was interrupted gains a trailing
/// `· interrupted`. The rate is committed output tokens over generation wall
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
    let interrupted = if state.interrupted_assistant_items.contains(&item_id) {
        " · interrupted"
    } else {
        ""
    };
    let prefix = (width >= 4).then(|| vec![Span::styled("╰─ ", theme.muted())]);
    Some(repeated_prefixed_wrapped_line(
        prefix.unwrap_or_default(),
        Line::from(Span::styled(
            format!(
                "⚡ {tps:.1} tps · {} ctx{cost}{interrupted}",
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

fn assistant_markdown_body_line(
    line: MarkdownLine,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let prefix = (width >= 3).then(|| vec![Span::styled("│ ", theme.assistant())]);
    let prefix = prefix.unwrap_or_default();
    match line.kind {
        MarkdownLineKind::Prose => repeated_prefixed_wrapped_line(prefix, line.line, width),
        MarkdownLineKind::ListItem {
            continuation_indent,
        } => repeated_prefixed_hanging_line(prefix, line.line, width, continuation_indent),
        MarkdownLineKind::Code => vec![prefixed_unwrapped_line(prefix, line.line, width)],
        MarkdownLineKind::Table => vec![prefixed_unwrapped_line(prefix, line.line, width)],
    }
}

fn prefixed_unwrapped_line(
    mut prefix: Vec<Span<'static>>,
    line: Line<'static>,
    width: u16,
) -> Line<'static> {
    let width = usize::from(width.max(1));
    let prefix_width = prefix
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    if prefix_width >= width {
        prefix.clear();
    }
    let line_style = line.style;
    prefix.extend(line.spans);
    Line::from(prefix).style(line_style)
}

fn repeated_prefixed_hanging_line(
    mut prefix: Vec<Span<'static>>,
    line: Line<'static>,
    width: u16,
    continuation_indent: usize,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let unbreakable = unbreakable_columns(&line);
    let mut prefix_width = prefix
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    // The gutter is indentation too, so it stands back until the row can host the
    // widest grapheme that has to sit on it whole — and the inner budget is then
    // measured against the columns the row actually has left.
    if prefix_width + unbreakable > width {
        prefix.clear();
        prefix_width = 0;
    }
    let inner_width = width.saturating_sub(prefix_width).max(1);
    let continuation_indent = continuation_indent
        .min(inner_width.saturating_sub(1))
        .min(inner_width.saturating_sub(unbreakable));
    let line_style = line.style;
    let (first_prefix, content) = split_spans_at_width(line.spans, continuation_indent);
    let mut wrapped = wrapped_line(
        Line::from(content).style(line_style),
        u16::try_from(inner_width.saturating_sub(continuation_indent).max(1)).unwrap_or(u16::MAX),
    );
    for (index, line) in wrapped.iter_mut().enumerate() {
        let mut spans = prefix.clone();
        if index == 0 {
            spans.extend(first_prefix.clone());
        } else if continuation_indent > 0 {
            spans.push(Span::raw(" ".repeat(continuation_indent)));
        }
        spans.append(&mut line.spans);
        line.spans = spans;
    }
    wrapped
}

fn split_spans_at_width(
    spans: Vec<Span<'static>>,
    width: usize,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let mut prefix = Vec::new();
    let mut content = Vec::new();
    let mut consumed = 0;
    for span in spans {
        for grapheme in span.content.graphemes(true) {
            let target = if consumed < width {
                consumed += UnicodeWidthStr::width(grapheme);
                &mut prefix
            } else {
                &mut content
            };
            append_span(target, grapheme.to_owned(), span.style);
        }
    }
    (prefix, content)
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
struct ToolBlockLayout {
    lines: Vec<Line<'static>>,
    output_toggles: Vec<(ToolOutputSection, usize, usize)>,
    header_lines: usize,
    /// Chrome columns per row, indexed alongside `lines`: the width of the
    /// spans this builder hung in front of each row, counted from what it
    /// built rather than from what the row says.
    chrome: Vec<u16>,
}

fn tool_block_lines(
    role: Role,
    body: Vec<ToolBodyLine>,
    width: u16,
    theme: &Theme,
) -> ToolBlockLayout {
    let style = match role {
        Role::ToolRunning => theme.tool_running(),
        Role::ToolSuccess => theme.tool_success(),
        Role::ToolFailure => theme.tool_failure(),
        _ => theme.tool(),
    };
    let mut lines = Vec::new();
    let mut output_toggles = Vec::new();
    let mut banded_rows = Vec::new();
    // Row index → how many of its leading spans are chrome this builder put
    // there: the block's `│ ` gutter, a narrow-mode label, a diff's line
    // number and marker. Counted while the row is assembled because it is a
    // fact about where the spans came from, not about what they say — the same
    // characters arriving as command output are content, and command output
    // that reads like a gutter (`│ `) is still content.
    let mut gutters = Vec::new();
    let mut header_lines = 0;
    for (index, body_line) in body.into_iter().enumerate() {
        let banded = body_line.banded;
        let output_toggle = body_line.output_toggle;
        let line_style = body_line.line.style;
        let spans = body_line
            .line
            .spans
            .into_iter()
            .map(|mut span| {
                span.style = style.patch(span.style);
                span
            })
            .collect::<Vec<_>>();
        let line = Line::from(spans).style(line_style);
        let start = lines.len();
        match body_line.kind {
            ToolBodyLineKind::Wrapped if width < 8 => {
                // The label and its aligned indent share one budget, so every
                // wrapped row of the block stays inside the viewport.
                let reserve = unbreakable_columns(&line);
                let label_width = tool_row_label_columns(width, reserve);
                let prefix = if index == 0 {
                    tool_row_prefix(tool_row_short(role), width, reserve)
                } else {
                    " ".repeat(label_width)
                };
                // Every row keeps exactly one label column, even when the
                // label itself would not fit and the span goes empty.
                lines.extend(prefixed_wrapped_line(prefix, style, line, width));
                gutters.resize(lines.len(), 1);
            }
            ToolBodyLineKind::Wrapped => {
                let gutter = vec![Span::styled("│ ", theme.assistant())];
                let chrome = usize::from(gutter_fits(&gutter, &line, width));
                lines.extend(repeated_prefixed_wrapped_line(gutter, line, width));
                gutters.resize(lines.len(), chrome);
            }
            ToolBodyLineKind::Code {
                mut first_gutter,
                mut continuation_gutter,
            } => {
                let prefix = (width >= 3)
                    .then(|| Span::styled("│ ", theme.assistant()))
                    .into_iter()
                    .collect::<Vec<_>>();
                let prefix_width = prefix
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum::<usize>();
                let available = usize::from(width.max(1)).saturating_sub(prefix_width);
                let mut first_gutter_width = first_gutter
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum::<usize>();
                let mut continuation_gutter_width = continuation_gutter
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum::<usize>();
                if first_gutter_width >= available || continuation_gutter_width >= available {
                    first_gutter.clear();
                    continuation_gutter.clear();
                    first_gutter_width = 0;
                    continuation_gutter_width = 0;
                }
                for (wrapped_index, content) in crate::markdown::wrap_code_spans(
                    line.spans,
                    available.saturating_sub(first_gutter_width).max(1),
                    available.saturating_sub(continuation_gutter_width).max(1),
                )
                .into_iter()
                .enumerate()
                {
                    let mut spans = prefix.clone();
                    if wrapped_index == 0 {
                        spans.extend(first_gutter.clone());
                    } else {
                        spans.extend(continuation_gutter.clone());
                    }
                    spans.extend(content);
                    gutters.push(
                        prefix.len()
                            + if wrapped_index == 0 {
                                first_gutter.len()
                            } else {
                                continuation_gutter.len()
                            },
                    );
                    lines.push(Line::from(spans).style(line_style));
                }
            }
        }
        if index == 0 {
            header_lines = lines.len();
        }
        if let Some(section) = output_toggle {
            output_toggles.push((section, start, lines.len()));
        }
        if banded {
            banded_rows.extend((start..lines.len()).map(|row| (row, gutters[row])));
        }
    }
    if let Some(background) = theme.terminal_background() {
        let band_width = banded_rows
            .iter()
            .map(|(index, _)| lines[*index].width())
            .max()
            .unwrap_or(0)
            .min(usize::from(width));
        for (index, chrome) in banded_rows {
            let line = &mut lines[index];
            // The band stops at the block's own gutter: those spans keep their
            // background and the content beside them takes the terminal band.
            // Counted, never guessed — `│ ` arriving as command output is a
            // tree row that must be banded, and a gutter welded to its text by
            // `append_span` is chrome that must not be.
            let content_start = chrome.min(line.spans.len());
            let padding = band_width.saturating_sub(line.width());
            line.spans.push(Span::raw(" ".repeat(padding)));
            for span in &mut line.spans[content_start..] {
                span.style = span.style.bg(background);
            }
        }
    }
    for line in &mut lines {
        line.style = line
            .style
            .remove_modifier(ratatui::style::Modifier::UNDERLINED);
        for span in &mut line.spans {
            span.style = span
                .style
                .remove_modifier(ratatui::style::Modifier::UNDERLINED);
        }
    }
    // Measured last, from the counted spans: a row's chrome is whatever the
    // builder put in front of it, so command output that reads like a gutter
    // (`│ ├── src`, a tree listing) is measured as the content it is.
    let chrome = gutters
        .iter()
        .zip(&lines)
        .map(|(spans, line)| {
            u16::try_from(
                line.spans
                    .iter()
                    .take(*spans)
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum::<usize>(),
            )
            .unwrap_or(u16::MAX)
        })
        .collect();
    ToolBlockLayout {
        lines,
        output_toggles,
        header_lines,
        chrome,
    }
}

/// Chrome columns of a tool block's header rows: the widest chrome row of the
/// header, since one highlight spans them all.
fn header_gutter_columns(rendered: &ToolBlockLayout) -> u16 {
    rendered
        .chrome
        .iter()
        .take(rendered.header_lines)
        .copied()
        .max()
        .unwrap_or(0)
}

fn role_block_lines(
    role: Role,
    body: Vec<Line<'static>>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let diagnostic = matches!(
        role,
        Role::Debug | Role::Internal | Role::Warning | Role::Error
    );
    let (label, marker, gutter, style) = match role {
        Role::User => ("USER", "┌─", "│ ", theme.user()),
        Role::Action => ("ACTION", "--", "│ ", theme.user()),
        Role::Goal => ("GOAL", "◆─", "│ ", theme.assistant()),
        Role::ToolRunning => ("TOOL RUNNING", "┏…", "┃ ", theme.tool_running()),
        Role::ToolSuccess => ("TOOL SUCCESS", "┏✓", "┃ ", theme.tool_success()),
        Role::ToolFailure => ("TOOL FAILURE", "┏!", "┃ ", theme.tool_failure()),
        Role::Debug => ("DEBUG [D]", "··", "· ", theme.muted()),
        // VS16 makes emoji terminals and the width table agree on U+26A0: two cells.
        Role::Warning => ("WARNING [W]", "⚠️─", "│ ", theme.warning()),
        Role::Error => ("ERROR [E]", "!!", "! ", theme.error()),
        Role::Internal => ("EVENT [I]", "--", "· ", theme.internal()),
    };
    if matches!(role, Role::User | Role::Goal | Role::Action) {
        if width == 0 {
            return Vec::new();
        }
        let header = if width < 8 {
            format!(
                "[{}]",
                match role {
                    Role::Goal => "G",
                    Role::Action => "A",
                    Role::User => "U",
                    _ => "P",
                }
            )
        } else {
            format!("{marker} {label}")
        };
        let mut lines = wrapped_line(Line::styled(header, style), width);
        for line in body {
            lines.extend(repeated_prefixed_wrapped_line(
                vec![Span::styled(gutter, style)],
                line,
                width,
            ));
        }
        return lines;
    }
    if width < 8 {
        let short = match role {
            Role::User => "U",
            Role::Action => "A",
            Role::Goal => "G",
            Role::ToolRunning => "T…",
            Role::ToolSuccess => "T✓",
            Role::ToolFailure => "T!",
            Role::Debug => "D",
            Role::Warning => "W",
            Role::Error => "E",
            Role::Internal => "I",
        };
        if diagnostic {
            if width == 0 {
                return Vec::new();
            }
            if body.len() == 1 && body[0].width() + 4 <= usize::from(width) {
                return repeated_prefixed_wrapped_line(
                    vec![Span::styled(format!("[{short}] "), style)],
                    body.into_iter().next().expect("one diagnostic line"),
                    width,
                );
            }
            let mut lines = wrapped_line(Line::styled(format!("[{short}]"), style), width);
            for line in body {
                lines.extend(repeated_prefixed_wrapped_line(
                    vec![Span::styled(gutter, style)],
                    line,
                    width,
                ));
            }
            return lines;
        }
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
    let mut lines = if matches!(role, Role::Internal) {
        Vec::new()
    } else {
        wrapped_line(
            Line::from(vec![
                Span::styled(format!("{marker} {label}"), style),
                Span::raw(" "),
            ]),
            width,
        )
    };
    for line in body {
        if diagnostic {
            lines.extend(repeated_prefixed_wrapped_line(
                vec![Span::styled(gutter, style)],
                line,
                width,
            ));
        } else {
            lines.extend(prefixed_wrapped_line(gutter.into(), style, line, width));
        }
    }
    lines
}

/// Widest grapheme `line` has to carry whole. Wrapping cannot split a grapheme,
/// so any leading indentation has to stand back this far or the row it belongs to
/// overflows the viewport.
fn unbreakable_columns(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| {
            span.content
                .graphemes(true)
                .map(UnicodeWidthStr::width)
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
        .max(1)
}

/// Whether a row can carry `prefix` and still host the widest grapheme it must
/// break whole. Below that the gutter is dropped rather than overflow the
/// viewport — it is indentation, and indentation is allowed to disappear.
fn gutter_fits(prefix: &[Span<'_>], line: &Line<'_>, width: u16) -> bool {
    let prefix_width = prefix
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    prefix_width + unbreakable_columns(line) <= usize::from(width.max(1))
}

fn prefixed_wrapped_line(
    prefix: String,
    prefix_style: Style,
    line: Line<'static>,
    width: u16,
) -> Vec<Line<'static>> {
    let room = usize::from(width);
    // A label that cannot leave the row both a column and the widest grapheme it
    // has to carry whole is dropped: it could not be hung off its own
    // continuation rows without overflowing the viewport.
    let prefix = if UnicodeWidthStr::width(prefix.as_str()) + unbreakable_columns(&line) > room {
        String::new()
    } else {
        prefix
    };
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    let continuation = " ".repeat(prefix_width);
    let mut wrapped = wrapped_line(
        line,
        u16::try_from(room.saturating_sub(prefix_width).max(1)).unwrap_or(u16::MAX),
    );
    for (index, line) in wrapped.iter_mut().enumerate() {
        line.spans.insert(
            0,
            if index == 0 {
                Span::styled(prefix.clone(), prefix_style)
            } else {
                Span::raw(continuation.clone())
            },
        );
    }
    wrapped
}

fn repeated_prefixed_wrapped_line(
    mut prefix: Vec<Span<'static>>,
    line: Line<'static>,
    width: u16,
) -> Vec<Line<'static>> {
    let guttered = gutter_fits(&prefix, &line, width);
    let width = usize::from(width.max(1));
    if !guttered {
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
        // Keep inherited text styling on the content, not on its gutter.
        spans.extend(line.spans.into_iter().map(|mut span| {
            span.style = line.style.patch(span.style);
            span
        }));
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
    if *current_width + word_width <= width {
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
        // A grapheme is the smallest unit a row can hold, so one wider than the
        // row has to be cut to it: spend the remaining columns on the same
        // ellipsis the rest of the transcript truncates with.
        let drawn = if *current_width + grapheme_width > width {
            super::app::truncate_with_ellipsis(grapheme, width - *current_width)
        } else {
            grapheme.to_owned()
        };
        let drawn_width = UnicodeWidthStr::width(drawn.as_str());
        append_span(current, drawn, style);
        *current_width += drawn_width;
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
    "[G] ",
    "[P] ",
    "[T\u{2026}] ",
    "[T\u{2713}] ",
    "[T!] ",
    "[T] ",
    "[D] ",
    "[W] ",
    "[E] ",
    "[I] ",
];

/// The chrome token span `index` of a row hangs in front of its text, if any.
///
/// Row gutters are chrome wherever they stack, and a gutter span welded to its
/// text is still the gutter a row builder hung there: the glyph gutters count
/// from their prefix alone.
///
/// The wrap-continuation indents and narrow-mode tags (the first-span set) are
/// *not* weld-eligible. Leading spaces in span 0 cannot be told apart from
/// indentation that belongs to the content — `"  indented code"` is a code row,
/// not a chrome band — so they only count when a span holds nothing but the
/// token, exactly as [`extract_line`] requires before it strips a span from
/// copied text. Past span 0 *nothing* welds: `"│ nested"` in column 3 is a tree
/// row, while a `"│ "` welded to the front of the row is the gutter it was
/// built as. Longest match wins so a tag is never mistaken for its own indent.
fn leading_gutter_token(content: &str, index: usize) -> Option<&'static str> {
    let first = index == 0;
    GUTTER_SPANS
        .iter()
        .copied()
        .filter(|token| content == *token || (first && content.starts_with(token)))
        .chain(
            first
                .then(|| FIRST_SPAN_GUTTERS.iter().copied())
                .into_iter()
                .flatten()
                .filter(|token| content == *token),
        )
        .max_by_key(|token| token.len())
}

/// Leading chrome columns of one rendered row: the gutter spans hanging off
/// its front, peeled positionally from span 0 onwards.
///
/// This is deliberately *not* a content-equality test over every span: it walks
/// the front of the row only. Row builders prepend gutters as whole spans, but
/// `append_span` merges same-style runs, so the chrome can end up welded to the
/// text it belongs to (`"│ title"`): the prefix counts, the rest of that span
/// does not, and a `"│ "` arriving later is output, not chrome.
pub(super) fn leading_gutter_columns(line: &Line<'_>) -> u16 {
    let mut columns = 0usize;
    for (index, span) in line.spans.iter().enumerate() {
        let content = span.content.as_ref();
        let Some(token) = leading_gutter_token(content, index) else {
            break;
        };
        columns += UnicodeWidthStr::width(token);
        // A welded gutter ends the chrome run at its own width; only a span
        // that is nothing but gutter lets the walk continue behind it.
        if content.len() > token.len() {
            break;
        }
    }
    u16::try_from(columns).unwrap_or(u16::MAX)
}

/// First characters of gutterless header/border/footer rows (role headers,
/// assistant attribution and footer, code fences, table grids). Such rows
/// are chrome-only: they vanish from an extraction rather than leaking
/// border glyphs into copied text.
const CHROME_ROW_PREFIXES: &[&str] = &[
    "┌", "└", "┏", "╭", "╰", "├", "··", "!!", "⚠", "--", "◆", "◇",
];

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
    // Standalone narrow headers carry the diagnostic style on the Line and
    // have no content gutter. Identical message text follows a gutter, so it
    // remains copyable rather than being classified by its text alone.
    if gutter_width == 0
        && match rest.as_str() {
            "[D]" => line.style == theme.muted(),
            "[I]" => line.style == theme.internal(),
            "[W]" => line.style == theme.warning(),
            "[E]" => line.style == theme.error(),
            _ => false,
        }
    {
        return None;
    }
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
mod tests;

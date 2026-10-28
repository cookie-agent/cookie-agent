//! Transcript layout, collapse state, wrapping, scrolling, and hit testing.

mod approval_preview;
mod assistant;
mod blocks;
mod diff;
mod events;
mod items;
mod output;
mod sgr;
mod tool_rows;
mod wrap;

pub(super) use approval_preview::{ApprovalPreviewRow, approval_presentation_rows};
use assistant::*;
use blocks::*;
use diff::*;
pub(super) use events::producer_summary;
use events::*;
use items::*;
use output::*;
use tool_rows::*;
pub(super) use tool_rows::{header_argument, hint_icon};
pub(super) use wrap::extract_selection;
pub(super) use wrap::leading_gutter_columns;
pub(super) use wrap::wrapped_line;
use wrap::*;

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    time::Duration,
};

#[cfg(test)]
use cookie_agent_protocol::GoalStatus;
use cookie_agent_protocol::{
    AgentId, GoalState, ProducerDeliveryMode, ProducerMessageId, ProducerOwner, SessionId,
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
    state::{
        AssistantChild, ModelDisplayNames, ProducerMessageStatus, SessionState, ToolStatus,
        TranscriptItem,
    },
    theme::{Theme, ThemeKey},
};

use super::app::{App, DescendantEvent, TextSelection, UserMessageHit};

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

/// Where the conversation viewport rests, and whether it follows live output.
///
/// Following pins the view to the bottom on every frame. Any scroll the user
/// makes away from the bottom stops it at once, and it stays stopped, however
/// output streams in or content changes under the view, until the user's own
/// scrolling (wheel, keys, the scrollbar) or a block they fold brings the view
/// back to the bottom.
#[derive(Debug)]
pub struct ConversationScroll {
    pub(super) offset: usize,
    pub(super) following: bool,
    max_offset: Option<usize>,
    /// The user moved the view, or folded or opened a block, since the last
    /// clamp: resting on the bottom then means they came back to it, while
    /// content shrinking under a scrolled-up view only pushes it there.
    moved: bool,
}

impl Default for ConversationScroll {
    fn default() -> Self {
        let mut scroll = Self {
            offset: 0,
            following: false,
            max_offset: None,
            moved: false,
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
        // Content that shrank below a scrolled-up view pushes it onto the new
        // bottom without the user coming back there.
        let pushed = !self.moved && self.offset > max_offset;
        self.moved = false;
        self.offset = if self.following {
            max_offset
        } else {
            self.offset.min(max_offset)
        };
        // A non-following view the user brought exactly onto the last valid
        // top offset is the live bottom; following resumes from there.
        if self.offset == max_offset && !pushed {
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
        self.moved = true;
        self.offset = 0;
    }
    pub fn bottom(&mut self) {
        self.following = true;
    }

    /// Keeps the view where it is while the user folds or opens a block:
    /// what they read stays put instead of following the tail.
    pub(super) fn hold(&mut self) {
        self.following = false;
        self.moved = true;
    }

    /// Absolute top offset from a scrollbar thumb/track gesture.
    pub(super) fn scroll_to(&mut self, offset: usize) {
        // Resolve the gesture against the last rendered content, before an
        // output append can move the bottom on the next frame.
        self.offset = offset;
        self.moved = true;
        self.following = self.max_offset.is_some_and(|max| offset >= max);
    }

    pub fn reveal(&mut self, region: BlockRegion, viewport_height: u16) {
        let height = usize::from(viewport_height.max(1));
        self.following = false;
        self.moved = true;
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
    ModelTools,
    /// A multi-line diagnostic row of the viewed session.
    Event(u64),
    /// A descendant's multi-line diagnostic row spliced into this view.
    DescendantEvent {
        session: SessionId,
        item: u64,
    },
    /// A compaction still producing its summary, by its start sequence.
    Compacting(u64),
    Compaction(u64),
    PluginMessage(u64),
    AgentMd(u64),
    SkillLoaded(u64),
    /// A model's memory change, by its event's sequence: a click opens the
    /// Memory panel at the memory.
    MemoryChange(u64),
    ProducerMessage(ProducerMessageId),
    /// A message between the ACP agent's sessions, by its event's sequence.
    AgentSessionMessage(u64),
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
    /// The row that loads, or puts away, the complete content of a call's
    /// cut display block.
    ToolFull {
        call_id: cookie_agent_protocol::ToolCallId,
        block: u32,
    },
    /// A row of a call's display that opens the session it refers to.
    ToolSession {
        call_id: cookie_agent_protocol::ToolCallId,
        session: SessionId,
    },
    /// A row of a call's display, the `link` block at this index, that
    /// opens the memory it names in the Memory panel.
    ToolMemory {
        call_id: cookie_agent_protocol::ToolCallId,
        block: u32,
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

/// Rendered rows shared by reference: a part, tool row, or item renders its
/// rows once and every cache and assembled view above it holds the same
/// allocation.
pub(super) type Chunk = std::sync::Arc<[Line<'static>]>;

/// The one blank row between transcript items.
fn blank_chunk() -> Chunk {
    static BLANK: std::sync::LazyLock<Chunk> =
        std::sync::LazyLock::new(|| std::sync::Arc::from(vec![Line::default()]));
    Chunk::clone(&BLANK)
}

/// A line sequence assembled from shared [`Chunk`]s: appending an unchanged
/// item, part, or tool row costs a pointer, never a copy of its rows.
#[derive(Clone, Default)]
pub(super) struct LineChunks {
    chunks: Vec<Chunk>,
    /// The line index each chunk starts at.
    starts: Vec<usize>,
    len: usize,
}

impl LineChunks {
    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn push_chunk(&mut self, chunk: Chunk) {
        if chunk.is_empty() {
            return;
        }
        self.starts.push(self.len);
        self.len += chunk.len();
        self.chunks.push(chunk);
    }

    fn push_lines(&mut self, lines: Vec<Line<'static>>) {
        if !lines.is_empty() {
            self.push_chunk(lines.into());
        }
    }

    pub(super) fn get(&self, index: usize) -> Option<&Line<'static>> {
        if index >= self.len {
            return None;
        }
        let chunk = self.starts.partition_point(|start| *start <= index) - 1;
        self.chunks[chunk].get(index - self.starts[chunk])
    }

    /// Keep the first `len` lines.
    fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        let kept = self.starts.partition_point(|start| *start < len);
        self.chunks.truncate(kept);
        self.starts.truncate(kept);
        self.len = len;
        if let (Some(last), Some(start)) = (self.chunks.last_mut(), self.starts.last())
            && start + last.len() > len
        {
            *last = last[..len - start].into();
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &Line<'static>> {
        self.chunks.iter().flat_map(|chunk| chunk.iter())
    }

    pub(super) fn to_vec(&self) -> Vec<Line<'static>> {
        self.iter().cloned().collect()
    }

    #[cfg(test)]
    pub(super) fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }
}

impl std::ops::Index<usize> for LineChunks {
    type Output = Line<'static>;

    fn index(&self, index: usize) -> &Line<'static> {
        self.get(index).expect("line index in range")
    }
}

/// Indexed read access to rendered lines, whether one slice or chunks.
pub(super) trait LineSource {
    fn line_count(&self) -> usize;
    fn line_at(&self, index: usize) -> Option<&Line<'static>>;
}

impl LineSource for [Line<'static>] {
    fn line_count(&self) -> usize {
        self.len()
    }

    fn line_at(&self, index: usize) -> Option<&Line<'static>> {
        self.get(index)
    }
}

impl LineSource for Vec<Line<'static>> {
    fn line_count(&self) -> usize {
        self.len()
    }

    fn line_at(&self, index: usize) -> Option<&Line<'static>> {
        self.get(index)
    }
}

impl LineSource for LineChunks {
    fn line_count(&self) -> usize {
        self.len
    }

    fn line_at(&self, index: usize) -> Option<&Line<'static>> {
        self.get(index)
    }
}

/// The cached, assembled transcript: [`TranscriptLayout`] over shared
/// chunks.
#[derive(Clone, Default)]
pub(super) struct ChunkedLayout {
    pub(super) lines: LineChunks,
    pub(super) regions: Vec<BlockRegion>,
    pub(super) user_regions: Vec<UserRegion>,
}

impl From<TranscriptLayout> for ChunkedLayout {
    fn from(layout: TranscriptLayout) -> Self {
        let mut lines = LineChunks::default();
        lines.push_lines(layout.lines);
        Self {
            lines,
            regions: layout.regions,
            user_regions: layout.user_regions,
        }
    }
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
    /// Bumped whenever `layout` changes, so views derived from it (the
    /// descendant splice) know when to rebuild.
    pub(super) revision: u64,
    pub(super) layout: ChunkedLayout,
    items: Vec<CachedItemLayout>,
    item_offsets: Vec<ItemAssemblyOffset>,
    assistant_parts: HashMap<u64, CachedAssistantPartLayout>,
    tool_rows: HashMap<BlockId, CachedToolLayout>,
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
                    .filter(|region| {
                        !matches!(
                            region.id,
                            BlockId::ToolOutput { .. }
                                | BlockId::ToolFull { .. }
                                | BlockId::ToolSession { .. }
                                | BlockId::ToolMemory { .. }
                        )
                    })
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

    /// Ask for the code rendered plain on `lines` (of the assembled layout)
    /// to be highlighted, marking each item holding some for its next
    /// layout. True when any was asked for.
    fn request_highlights(&mut self, lines: std::ops::Range<usize>) -> bool {
        let total = self.layout.lines.len();
        let first = self
            .item_offsets
            .partition_point(|offset| offset.lines <= lines.start)
            .saturating_sub(1);
        let mut requested = false;
        for index in first..self.items.len().min(self.item_offsets.len()) {
            let start = self.item_offsets[index].lines;
            if start >= lines.end {
                break;
            }
            let item = &mut self.items[index];
            if item.deferred.is_empty() {
                continue;
            }
            let end = self
                .item_offsets
                .get(index + 1)
                .map_or(total, |next| next.lines);
            // The item's rows end the span its separator opens.
            let body = end.saturating_sub(item.layout.lines.len());
            for (rows, highlight) in &item.deferred {
                if body + rows.start < lines.end && body + rows.end > lines.start {
                    highlight.request();
                    item.highlight_requested = true;
                    requested = true;
                }
            }
        }
        requested
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
    fn at_end(layout: &ChunkedLayout) -> Self {
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
    /// The pinned tools block renders with the system prompt.
    model_tools: Option<std::sync::Arc<[String]>>,
    model_deferred_tools: std::sync::Arc<[String]>,
    model_tools_expanded: bool,
    /// Whose bridge the tools are, which the block's title names.
    acp_agent: Option<AgentId>,
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
    /// Display-name revision for assistant items (0 otherwise), so headers
    /// relabel when a name arrives after the item was first laid out.
    model_names: u64,
}

#[derive(Clone, Default)]
struct ItemLayout {
    lines: Vec<Line<'static>>,
    regions: Vec<BlockRegion>,
    /// Physical event sequence when this item is a user message row.
    user_seq: Option<u64>,
}

/// [`ItemLayout`] over shared chunks: an assistant item holds one chunk per
/// child, so re-laying it out after one child changed reuses every other
/// child's rows by reference.
#[derive(Clone, Default)]
struct ChunkedItem {
    lines: LineChunks,
    regions: Vec<BlockRegion>,
    user_seq: Option<u64>,
}

impl ChunkedItem {
    /// Append one child's rows and its regions, shifted to where they land.
    fn push(&mut self, lines: &Chunk, regions: &[BlockRegion]) {
        let start = self.lines.len();
        self.regions
            .extend(regions.iter().map(|region| BlockRegion {
                start_line: start + region.start_line,
                end_line: start + region.end_line,
                ..*region
            }));
        self.lines.push_chunk(Chunk::clone(lines));
    }

    /// [`Self::push`] for a multi-chunk child.
    fn push_chunks(&mut self, lines: &LineChunks, regions: &[BlockRegion]) {
        let start = self.lines.len();
        self.regions
            .extend(regions.iter().map(|region| BlockRegion {
                start_line: start + region.start_line,
                end_line: start + region.end_line,
                ..*region
            }));
        for chunk in &lines.chunks {
            self.lines.push_chunk(Chunk::clone(chunk));
        }
    }

    fn push_layout(&mut self, layout: ItemLayout) {
        let start = self.lines.len();
        self.regions
            .extend(layout.regions.into_iter().map(|region| BlockRegion {
                start_line: start + region.start_line,
                end_line: start + region.end_line,
                ..region
            }));
        self.lines.push_lines(layout.lines);
    }

    fn flatten(self) -> ItemLayout {
        ItemLayout {
            lines: self.lines.to_vec(),
            regions: self.regions,
            user_seq: self.user_seq,
        }
    }
}

impl From<ItemLayout> for ChunkedItem {
    fn from(layout: ItemLayout) -> Self {
        let mut lines = LineChunks::default();
        lines.push_lines(layout.lines);
        Self {
            lines,
            regions: layout.regions,
            user_seq: layout.user_seq,
        }
    }
}

#[derive(Clone)]
struct CachedItemLayout {
    key: ItemLayoutKey,
    /// The layout asked for some of `deferred`: render the item again.
    highlight_requested: bool,
    /// Rows of `layout` whose code renders plain, highlighting deferred.
    deferred: Vec<DeferredRows>,
    layout: ChunkedItem,
    /// For an assistant item, one line per child: where that child's rows
    /// (with the gutter row separating it from a different kind) begin in
    /// `layout.lines`. Descendant rows splice between children there.
    child_lines: Vec<usize>,
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
    lines: LineChunks,
    regions: Vec<BlockRegion>,
    /// A reply's rendered markdown blocks and the rows each wrapped to, so
    /// the next render of this part re-wraps only blocks that changed.
    blocks: Vec<WrappedBlock>,
    /// A thinking part's counted rows, carried to its next render.
    thinking_rows: ThinkingRows,
    /// Rows rendering code plain, highlighting deferred.
    deferred: Vec<DeferredRows>,
}

/// One rendered markdown block and the rows it wrapped to.
type WrappedBlock = (std::sync::Arc<[MarkdownLine]>, Chunk);

/// Rows of a part or item rendering a block's code plain, and the handle
/// that asks for it highlighted.
type DeferredRows = (std::ops::Range<usize>, crate::markdown::DeferredHighlight);

/// Everything one tool row renders from beyond the cache-wide inputs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ToolLayoutKey {
    /// The call's [`crate::state::ToolCallState::revision`]; `None` while
    /// the row is a placeholder.
    revision: Option<u64>,
    /// Expansion of the row, then of its detail, stdout and stderr output.
    expanded: [bool; 4],
    /// Animation bucket while the call runs (0 otherwise).
    clock: u8,
}

#[derive(Clone)]
struct CachedToolLayout {
    key: ToolLayoutKey,
    lines: Chunk,
    regions: Vec<BlockRegion>,
}

struct TranscriptRenderContext<'a> {
    expanded: Option<&'a HashSet<BlockId>>,
    width: u16,
    theme: &'a Theme,
    highlighter: &'a dyn Highlighter,
    minimum_event_level: crate::state::EventLevel,
    /// Animation bucket (0–3) driving the streaming "thinking…" ellipsis.
    clock_bucket: u8,
    model_names: &'a ModelDisplayNames,
    assistant_part_cache: &'a mut HashMap<u64, CachedAssistantPartLayout>,
    assistant_part_layout_passes: &'a mut u64,
    tool_cache: &'a mut HashMap<BlockId, CachedToolLayout>,
    assistant_child_lines: &'a mut Vec<usize>,
    /// Render settled code plain unless its highlighting is at hand; the
    /// cached layout highlights what comes into view.
    defer_highlights: bool,
    /// The item's rows rendering code plain.
    deferred_highlights: &'a mut Vec<DeferredRows>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BlockHit {
    pub(super) rect: Rect,
    pub(super) id: BlockId,
    pub(super) hover_rect: Option<Rect>,
    pub(super) toggle_rect: Option<Rect>,
    /// The background button of a running call's title row, when shown.
    pub(super) action_rect: Option<Rect>,
    /// The memory URL a call's title row names, which opens the memory in
    /// the Memory panel.
    pub(super) link_rect: Option<Rect>,
}

impl BlockHit {
    /// The cells a hover paints: the block's header content rect, clipped to
    /// its hit rect and to `viewport` (which already stops before the
    /// reserved scrollbar columns), minus a background button with the blank
    /// column before it and the rest of its row, and minus a memory link
    /// with a blank column on each side. The header highlight leaves out
    /// what hovers on its own, so the highlights never touch.
    pub(super) fn hover_cells(&self, viewport: Option<Rect>) -> Vec<Rect> {
        let Some(rect) = self.hover_rect.map(|rect| rect.intersection(self.rect)) else {
            return Vec::new();
        };
        let rect = viewport.map_or(rect, |viewport| rect.intersection(viewport));
        let on_header = |button: &Rect| button.y >= rect.y && button.y < rect.bottom();
        // Each target on the title row takes the columns `[start, end)`.
        let mut taken = self
            .action_rect
            .filter(on_header)
            .map(|button| (button.y, button.x.saturating_sub(1), rect.right()))
            .into_iter()
            .chain(self.link_rect.filter(on_header).map(|link| {
                (
                    link.y,
                    link.x.saturating_sub(1),
                    link.right().saturating_add(1),
                )
            }))
            .collect::<Vec<_>>();
        let Some(&(row, _, _)) = taken.first() else {
            return vec![rect];
        };
        taken.retain(|(y, _, _)| *y == row);
        taken.sort_by_key(|(_, start, _)| *start);
        let mut cells = vec![Rect::new(rect.x, rect.y, rect.width, row - rect.y)];
        let mut x = rect.x;
        for (_, start, end) in taken {
            let start = start.clamp(x, rect.right());
            cells.push(Rect::new(x, row, start - x, 1));
            x = end.clamp(x, rect.right());
        }
        cells.push(Rect::new(x, row, rect.right() - x, 1));
        cells.push(Rect::new(
            rect.x,
            row + 1,
            rect.width,
            rect.bottom() - row - 1,
        ));
        cells.retain(|rect| !rect.is_empty());
        cells
    }
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
    model_names: &ModelDisplayNames,
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
        cache.layout = ChunkedLayout::default();
        cache.items.clear();
        cache.item_offsets.clear();
        cache.assistant_parts.clear();
        cache.tool_rows.clear();
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
            model_tools: state.model_tools.clone(),
            model_deferred_tools: state.model_deferred_tools.clone(),
            model_tools_expanded: expanded
                .is_some_and(|blocks| blocks.contains(&BlockId::ModelTools)),
            acp_agent: state.acp.agent.clone(),
        });
    let system_prompt_changed =
        cache.system_prompt.as_ref().map(|cached| &cached.key) != prompt_key.as_ref();
    if system_prompt_changed {
        cache.layout = ChunkedLayout::default();
        cache.item_offsets.clear();
        cache.system_prompt = if let (Some(snapshot), Some(prompt_key)) =
            (state.run_snapshot.as_deref(), prompt_key)
        {
            let layout = system_prompt_layout(snapshot, draft_agent, expanded, width, theme);
            append_chunked_item(&mut cache.layout, &layout.into());
            if let Some(tools) = state.model_tools.as_deref() {
                append_chunked_item(
                    &mut cache.layout,
                    &model_tools_layout(state, tools, expanded, width, theme).into(),
                );
            }
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
                    !cached.highlight_requested
                        && item_layout_key_matches(
                            &cached.key,
                            state,
                            item,
                            expanded,
                            clock_bucket,
                            model_names,
                        )
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

    // Items from the first dirty one on are matched by id, not position: a
    // row moved to the end (a folded warning) shifts every later item by
    // one, and each of them is still exactly what it was.
    let mut previous = cache
        .items
        .drain(first_dirty.min(cache.items.len())..)
        .map(|cached| (cached.key.id, cached))
        .collect::<HashMap<_, _>>();
    for item in state.transcript.iter().skip(first_dirty) {
        cache
            .item_offsets
            .push(ItemAssemblyOffset::at_end(&cache.layout));
        let reusable = previous.remove(&item.id()).filter(|cached| {
            !cached.highlight_requested
                && item_layout_key_matches(
                    &cached.key,
                    state,
                    item,
                    expanded,
                    clock_bucket,
                    model_names,
                )
        });
        if let Some(cached) = reusable {
            cache.items.push(cached);
        } else {
            let item_key = item_layout_key(state, item, expanded, clock_bucket, model_names);
            let mut assistant_child_lines = Vec::new();
            let mut deferred = Vec::new();
            let mut context = TranscriptRenderContext {
                expanded,
                width,
                theme,
                highlighter,
                minimum_event_level,
                clock_bucket,
                model_names,
                assistant_part_cache: &mut cache.assistant_parts,
                assistant_part_layout_passes: &mut cache.assistant_part_layout_passes,
                tool_cache: &mut cache.tool_rows,
                assistant_child_lines: &mut assistant_child_lines,
                defer_highlights: true,
                deferred_highlights: &mut deferred,
            };
            // Unchanged parts and tool rows come back from their caches by
            // reference, so this re-renders only what changed.
            let layout = chunked_item_layout(state, item, &mut context);
            let cached = CachedItemLayout {
                key: item_key,
                highlight_requested: false,
                deferred,
                layout,
                child_lines: assistant_child_lines,
            };
            cache.items.push(cached);
            #[cfg(test)]
            {
                cache.item_layout_passes = cache.item_layout_passes.wrapping_add(1);
            }
        }
        let cached = cache.items.last().expect("item pushed");
        append_chunked_item(&mut cache.layout, &cached.layout);
        #[cfg(test)]
        {
            cache.item_assembly_passes = cache.item_assembly_passes.wrapping_add(1);
        }
    }
    cache.items.truncate(state.transcript.len());
    cache.item_offsets.truncate(state.transcript.len());
    cache.revision = next_layout_revision();
    false
}

/// A fresh layout revision, unique across every [`LayoutCache`], so a view
/// derived from one cache never mistakes another (parked) cache's layout
/// for the one it was built from.
fn next_layout_revision() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Layout caches kept for recently viewed sessions besides the current one...
const PARKED_LAYOUT_CACHES: usize = 4;

/// ...holding at most this many rows together: a parked row costs a few
/// hundred bytes, and laying out a session again costs about 5 ms per
/// thousand rows.
const PARKED_LAYOUT_LINES: usize = 40_000;

/// Descendant rows spliced into a rendered transcript, kept apart from the
/// transcript's own lines: the view reads through both, so no frame copies
/// the transcript to show a handful of rows.
#[derive(Clone, Debug, Default)]
pub(super) struct SplicedRows {
    /// Every inserted row, in order.
    rows: Vec<Line<'static>>,
    /// Per splice point, ascending: its original-line position and the
    /// number of rows inserted there.
    pub(super) shifts: Vec<(usize, usize)>,
    /// Per splice point: the spliced-line index of its first row.
    starts: Vec<usize>,
    /// The inserted rows' own collapsible regions, in spliced coordinates.
    pub(super) regions: Vec<BlockRegion>,
}

impl SplicedRows {
    fn is_empty(&self) -> bool {
        self.shifts.is_empty()
    }

    /// Spliced line count over `base_len` original lines.
    fn len(&self, base_len: usize) -> usize {
        base_len + self.rows.len()
    }

    /// Spliced line `index` over the original `base` lines.
    pub(super) fn line<'a, L: LineSource + ?Sized>(
        &'a self,
        base: &'a L,
        index: usize,
    ) -> Option<&'a Line<'static>> {
        let splice = self.starts.partition_point(|start| *start <= index);
        let Some(splice) = splice.checked_sub(1) else {
            return base.line_at(index);
        };
        let start = self.starts[splice];
        let (position, inserted) = self.shifts[splice];
        let inserted_before = start - position;
        if index < start + inserted {
            self.rows.get(inserted_before + index - start)
        } else {
            base.line_at(index - inserted_before - inserted)
        }
    }

    /// Shift one original line coordinate by the rows spliced in before it;
    /// see [`splice_shifted`].
    fn shift(&self, line: usize, inclusive: bool) -> usize {
        let splices = self
            .shifts
            .partition_point(|(position, _)| *position < line || (inclusive && *position == line));
        let Some(last) = splices.checked_sub(1) else {
            return line;
        };
        let (position, inserted) = self.shifts[last];
        line + (self.starts[last] - position) + inserted
    }

    fn shift_region(&self, mut region: BlockRegion) -> BlockRegion {
        let start = self.shift(region.start_line, true);
        region.end_line = self.shift(region.end_line, false).max(start);
        region.start_line = start;
        region
    }

    fn shift_user_region(&self, mut region: UserRegion) -> UserRegion {
        let start = self.shift(region.start_line, true);
        region.end_line = self.shift(region.end_line, false).max(start);
        region.start_line = start;
        region
    }

    /// Every spliced line, copied out.
    pub(super) fn materialize<L: LineSource + ?Sized>(&self, base: &L) -> Vec<Line<'static>> {
        (0..self.len(base.line_count()))
            .filter_map(|index| self.line(base, index).cloned())
            .collect()
    }
}

/// The descendant splice over the cached layout, rebuilt only when the
/// layout, the descendant rows, or their presentation changed.
#[derive(Default)]
pub(in crate::ui) struct SpliceCache {
    key: Option<SpliceKey>,
    /// The aggregated descendant rows and the state they were read from.
    events_key: Option<u64>,
    events: Vec<DescendantEvent>,
    /// Each descendant row rendered apart from where it lands.
    row_layouts: EventRowLayouts,
    rows: SplicedRows,
    /// The layout's regions shifted into spliced coordinates, then the
    /// spliced rows' own.
    block_regions: Vec<BlockRegion>,
    user_regions: Vec<UserRegion>,
}

/// Descendant rows rendered apart from where they land, by owning session
/// and item, so a splice over a changed layout places rows again without
/// rendering them again.
pub(super) type EventRowLayouts = HashMap<(SessionId, u64), CachedEventRow>;

/// One descendant row's layout and everything it renders from.
pub(super) struct CachedEventRow {
    key: EventRowKey,
    text: String,
    layout: ItemLayout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventRowKey {
    level: crate::state::EventLevel,
    repeat: u32,
    expanded: bool,
    width: u16,
    theme: ThemeKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SpliceKey {
    layout_revision: u64,
    events_key: u64,
    /// Expansion of each descendant row, in row order.
    expanded: Vec<bool>,
    width: u16,
    theme: ThemeKey,
    model_names: u64,
}

/// One descendant row's splice point in the unspliced layout, and the
/// assistant block it breaks, as (item index, child index), when the row
/// lands between two of that block's children.
type Placement<'a> = (usize, &'a DescendantEvent, Option<(usize, usize)>);

/// Shift one unspliced line coordinate by the rows spliced in before it. A
/// region start (`inclusive`) moves with rows inserted exactly at it; an
/// exclusive region end does not, so a region ending where rows are
/// inserted never grows over them.
fn splice_shifted(line: usize, splice_shifts: &[(usize, usize)], inclusive: bool) -> usize {
    splice_shifts
        .iter()
        .filter(|(position, _)| *position < line || (inclusive && *position == line))
        .map(|(_, inserted)| *inserted)
        .sum::<usize>()
        .saturating_add(line)
}

/// Map a spliced line back to the unspliced layout. A line inside spliced-in
/// rows maps to their splice point, with its depth into them.
fn unsplice_line(line: usize, splice_shifts: &[(usize, usize)]) -> (usize, Option<usize>) {
    let mut inserted_before = 0;
    for (position, inserted) in splice_shifts {
        let start = position + inserted_before;
        if line < start {
            break;
        }
        if line < start + inserted {
            return (*position, Some(line - start));
        }
        inserted_before += inserted;
    }
    (line - inserted_before, None)
}

/// The header that resumes an assistant block below descendant rows spliced
/// in before child `child`: the block's producer with the model in effect at
/// that point, exactly as a block split by an in-session row would read.
fn continuation_header(
    item: &TranscriptItem,
    child: usize,
    width: u16,
    theme: &Theme,
    model_names: &ModelDisplayNames,
) -> Vec<Line<'static>> {
    let TranscriptItem::Assistant {
        attribution,
        children,
        ..
    } = item
    else {
        return Vec::new();
    };
    let resolved_model = children[..child.min(children.len())]
        .iter()
        .rev()
        .find_map(|child| match child {
            AssistantChild::Attribution { resolved_model } => Some(resolved_model),
            _ => None,
        })
        .unwrap_or(&attribution.resolved_model);
    let attribution = crate::state::FrozenAssistantAttribution {
        agent: attribution.agent.clone(),
        resolved_model: resolved_model.clone(),
        // A model change within a reply; an ACP agent's model is one model.
        acp_model: None,
    };
    assistant_header(attribution.header(model_names).as_str(), width, theme)
}

impl App {
    /// The transient notice rows rendered after the transcript (transient
    /// notices and goal notices), exactly as [`Self::render_conversation`]
    /// appends them. Descendant warnings and errors are spliced into the
    /// transcript body instead; see [`Self::spliced_conversation_rows`].
    /// Selection extraction consumes the same chain so copied text matches
    /// what is on screen.
    pub(super) fn notice_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut notice_lines = Vec::new();
        let goal_notices = self
            .viewed_session()
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

    /// Descendant warning and error rows for the viewed session, at or
    /// above the event-level filter.
    fn viewed_descendant_events(&self) -> Vec<DescendantEvent> {
        self.viewed_session()
            .map(|selected| self.descendant_events(selected, self.tui_config.minimum_event_level))
            .unwrap_or_default()
    }

    /// The viewed session's descendant rows spliced into `layout_lines`; see
    /// [`Self::splice_descendant_events`]. `None` when there is nothing to
    /// splice.
    fn spliced_conversation_rows(
        &self,
        width: u16,
        layout_lines: &(impl LineSource + ?Sized),
        events: &[DescendantEvent],
        row_layouts: &mut EventRowLayouts,
    ) -> Option<SplicedRows> {
        let state = self
            .selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .filter(|_| !events.is_empty())?;
        Some(Self::splice_descendant_events(
            layout_lines,
            &self.layout_cache.items,
            &self.layout_cache.item_offsets,
            state,
            events,
            self.selected
                .and_then(|session_id| self.expanded_blocks.get(&session_id)),
            width,
            &self.theme,
            self.runtime.model_names(),
            row_layouts,
        ))
    }

    /// Refresh the cached descendant rows when any descendant projection,
    /// its label, or the filter changed since they were aggregated.
    fn refresh_descendant_events(&self, cache: &mut SpliceCache) {
        let key = self
            .viewed_session()
            .map(|viewed| self.descendant_events_key(viewed, self.tui_config.minimum_event_level));
        if cache.events_key == key && key.is_some() {
            return;
        }
        cache.events = self.viewed_descendant_events();
        cache.events_key = key;
    }

    /// Bring `cache` up to date for this frame's layout. `base` is the
    /// layout being drawn; only the cached transcript layout (`cacheable`)
    /// is worth keeping a splice for.
    fn refresh_splice(
        &self,
        cache: &mut SpliceCache,
        base: &ChunkedLayout,
        cacheable: bool,
        width: u16,
    ) {
        self.refresh_descendant_events(cache);
        let expanded = self
            .selected
            .and_then(|session_id| self.expanded_blocks.get(&session_id));
        let key = cache.events_key.map(|events_key| SpliceKey {
            layout_revision: self.layout_cache.revision,
            events_key,
            expanded: cache
                .events
                .iter()
                .map(|row| {
                    expanded.is_some_and(|blocks| {
                        blocks.contains(&BlockId::DescendantEvent {
                            session: row.session,
                            item: row.item,
                        })
                    })
                })
                .collect(),
            width,
            theme: self.theme.key(),
            model_names: self.runtime.model_names().revision(),
        });
        if cacheable && key.is_some() && cache.key == key {
            return;
        }
        cache.key = key.filter(|_| cacheable);
        cache.rows = self
            .spliced_conversation_rows(width, &base.lines, &cache.events, &mut cache.row_layouts)
            .unwrap_or_default();
        if cache.row_layouts.len() > cache.events.len() {
            let shown = cache
                .events
                .iter()
                .map(|row| (row.session, row.item))
                .collect::<HashSet<_>>();
            cache.row_layouts.retain(|row, _| shown.contains(row));
        }
        cache.block_regions.clear();
        cache.user_regions.clear();
        if !cache.rows.is_empty() {
            cache.block_regions.extend(
                base.regions
                    .iter()
                    .map(|region| cache.rows.shift_region(*region))
                    .chain(cache.rows.regions.iter().copied()),
            );
            cache.user_regions.extend(
                base.user_regions
                    .iter()
                    .map(|region| cache.rows.shift_user_region(*region)),
            );
        }
    }

    /// Splice aggregated descendant warning and error blocks into a rendered
    /// transcript at their chronological position.
    ///
    /// The viewed transcript is a sequence of timed segments in render
    /// order: each item at its durable insertion time, except that an
    /// assistant block contributes one segment per child at the child's own
    /// time (one run's turns and tool calls share a block, so the block's
    /// start time alone would push a row that happened mid-run below
    /// everything the run did afterwards). Each row lands just before the
    /// first segment that happened after it, so nothing rendered above a row
    /// happened later than the row; rows newer than every segment (or when
    /// nothing is timed) land at the bottom.
    ///
    /// Before an item, a row takes that item's separator slot. Between two
    /// children of an assistant block, it breaks the block: the rows render
    /// below the earlier children and a continuation header resumes the
    /// block, as an in-session row splitting a block would.
    ///
    /// `item_offsets` holds one [`ItemAssemblyOffset`] per transcript item
    /// and `items` the cached layouts they were assembled from. One pass
    /// walks the segments against the time-ordered rows.
    ///
    /// Returns the spliced lines plus, for each splice point, the
    /// original-line position and the number of lines inserted there, so
    /// callers can shift scroll anchors and hit regions that address the
    /// unspliced layout.
    // Each input is independent render state (layout, projection, rows,
    // width, theme, display names); bundling them would only add a type.
    #[allow(clippy::too_many_arguments)]
    fn splice_descendant_events(
        lines: &(impl LineSource + ?Sized),
        items: &[CachedItemLayout],
        item_offsets: &[ItemAssemblyOffset],
        state: &SessionState,
        events: &[DescendantEvent],
        expanded: Option<&HashSet<BlockId>>,
        width: u16,
        theme: &Theme,
        model_names: &ModelDisplayNames,
        row_layouts: &mut EventRowLayouts,
    ) -> SplicedRows {
        let mut placements: Vec<Placement<'_>> = Vec::with_capacity(events.len());
        let mut next = 0;
        for (index, item) in state.transcript.iter().enumerate() {
            if next == events.len() {
                break;
            }
            let Some(offset) = item_offsets.get(index) else {
                break;
            };
            let item_end = item_offsets
                .get(index + 1)
                .map_or(lines.line_count(), |next| next.lines);
            let child_segments = match (item, items.get(index)) {
                (
                    TranscriptItem::Assistant {
                        children,
                        child_times,
                        ..
                    },
                    Some(cached),
                ) if !children.is_empty()
                    && child_times.len() == children.len()
                    && cached.child_lines.len() == children.len()
                    && cached.key.id == item.id()
                    && cached.key.version == item.version() =>
                {
                    Some((
                        child_times,
                        &cached.child_lines,
                        item_end.saturating_sub(cached.layout.lines.line_count()),
                    ))
                }
                _ => None,
            };
            if let Some((child_times, child_lines, body_start)) = child_segments {
                for (child, time) in child_times.iter().enumerate() {
                    while next < events.len() && events[next].time < *time {
                        let placement = if child == 0 {
                            (offset.lines, &events[next], None)
                        } else {
                            (
                                body_start + child_lines[child],
                                &events[next],
                                Some((index, child)),
                            )
                        };
                        placements.push(placement);
                        next += 1;
                    }
                }
            } else if let Some(time) = state.item_time(item.id()) {
                while next < events.len() && events[next].time < time {
                    placements.push((offset.lines, &events[next], None));
                    next += 1;
                }
            }
        }
        placements.extend(
            events[next..]
                .iter()
                .map(|event| (lines.line_count(), event, None)),
        );
        // Positions already ascend with segment order; the stable sort only
        // guards stale cached offsets.
        placements.sort_by_key(|(position, _, _)| *position);

        let mut spliced = SplicedRows::default();
        let mut placements = placements.into_iter().peekable();
        while let Some((position, event, resume)) = placements.next() {
            let position = position.min(lines.line_count());
            let before = spliced.rows.len();
            let mut resume = resume;
            let mut event = Some(event);
            while let Some(row) = event.take() {
                // The line above: this splice's previous row, else the
                // transcript line before the splice point.
                let above = if spliced.rows.len() > before {
                    spliced.rows.last()
                } else {
                    position.checked_sub(1).and_then(|line| lines.line_at(line))
                };
                let needs_separator = above.is_some_and(|line| {
                    line.spans
                        .iter()
                        .any(|span| !span.content.trim().is_empty())
                });
                if needs_separator {
                    spliced.rows.push(Line::default());
                }
                let block_id = BlockId::DescendantEvent {
                    session: row.session,
                    item: row.item,
                };
                let key = EventRowKey {
                    level: row.level,
                    repeat: row.repeat,
                    expanded: expanded.is_some_and(|blocks| blocks.contains(&block_id)),
                    width,
                    theme: theme.key(),
                };
                let cached = match row_layouts.entry((row.session, row.item)) {
                    std::collections::hash_map::Entry::Occupied(entry)
                        if entry.get().key == key && entry.get().text == row.text =>
                    {
                        entry.into_mut()
                    }
                    entry => entry
                        .insert_entry(CachedEventRow {
                            key,
                            text: row.text.clone(),
                            layout: event_row_layout(
                                block_id, row.level, &row.text, row.repeat, expanded, width, theme,
                            ),
                        })
                        .into_mut(),
                };
                let row_layout = &cached.layout;
                let row_start = position + spliced.rows.len();
                spliced
                    .regions
                    .extend(row_layout.regions.iter().map(|region| BlockRegion {
                        start_line: row_start + region.start_line,
                        end_line: row_start + region.end_line,
                        ..*region
                    }));
                spliced.rows.extend(row_layout.lines.iter().cloned());
                if let Some((_, row, row_resume)) =
                    placements.next_if(|(next, _, _)| (*next).min(lines.line_count()) == position)
                {
                    resume = resume.or(row_resume);
                    event = Some(row);
                }
            }
            if let Some((item, child)) = resume
                && let Some(item) = state.transcript.get(item)
            {
                spliced.rows.push(Line::default());
                spliced
                    .rows
                    .extend(continuation_header(item, child, width, theme, model_names));
            }
            let inserted = spliced.rows.len() - before;
            if inserted > 0 {
                spliced.shifts.push((position, inserted));
                spliced.starts.push(position + before);
            }
        }
        spliced
    }

    /// The full rendered conversation chain — transcript lines plus the
    /// notice block — for selection extraction. The cached layout is exactly
    /// what the last frame rendered at this width, so logical-line
    /// coordinates from the mouse map one-to-one.
    pub(super) fn conversation_chain(&self, width: u16) -> Vec<Line<'static>> {
        // A new-session draft shows the fresh session it will create, not the
        // conversation still selected behind it.
        let viewed = self.viewed_session();
        let session_present = self.new_session_draft.is_some()
            || viewed.is_some_and(|session_id| self.store.sessions.contains_key(&session_id));
        let transcript_empty = viewed
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .is_none_or(|state| state.transcript.is_empty() && state.run_snapshot.is_none());
        let descendant_events = self.viewed_descendant_events();
        let mut notices = self.notice_lines(width);
        let base = if session_present && !transcript_empty {
            self.layout_cache.layout.lines.to_vec()
        } else {
            empty_conversation_lines(session_present, width, &self.theme)
        };
        let mut lines = match self.spliced_conversation_rows(
            width,
            &base,
            &descendant_events,
            &mut EventRowLayouts::new(),
        ) {
            Some(spliced) => spliced.materialize(&base),
            None => base,
        };
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
        // The two rightmost inner columns always belong to the scrollbar: a
        // blank margin and the track. Content layout, block hit regions and
        // highlight rects are built at that reduced width and never extend
        // into it, so the track can be grabbed without hitting blocks — and
        // because the reservation is constant, the wrap width (a
        // `LayoutCacheKey` input) never flips when content starts or stops
        // overflowing.
        let width = pane_text_width(area.width);
        // A new-session draft shows the fresh session it will create, not the
        // conversation still selected behind it.
        let viewed = self.viewed_session();
        let session_present = self.new_session_draft.is_some()
            || viewed.is_some_and(|session_id| self.store.sessions.contains_key(&session_id));
        let empty_layout = ChunkedLayout::from(TranscriptLayout {
            lines: empty_conversation_lines(session_present, width, &self.theme),
            regions: Vec::new(),
            user_regions: Vec::new(),
        });
        let clock_bucket = self.clock_bucket();
        let draft_agent = self.draft.as_ref().map(|draft| draft.agent.clone());
        let anchor_key = self.layout_cache.key;
        // The offset addresses the last drawn (spliced) lines; the anchor
        // addresses the cached layout, so map the offset back first.
        let (unspliced_offset, spliced_depth) = unsplice_line(
            self.conversation_scroll.offset,
            &self.conversation_splice_shifts,
        );
        let anchor = (!self.conversation_scroll.following)
            .then(|| self.layout_cache.scroll_anchor(unspliced_offset))
            .flatten();
        let mut layout_changed = false;
        let mut cached_layout = false;
        if let Some(session_id) = viewed {
            self.select_layout_cache(session_id);
        }
        if let Some((changed, cached)) =
            self.ensure_viewed_layout(viewed, width, clock_bucket, draft_agent.as_ref())
        {
            layout_changed = changed;
            cached_layout = cached;
        }
        let layout = if cached_layout {
            &self.layout_cache.layout
        } else {
            &empty_layout
        };
        // Descendant rows read through to the layout instead of copying it;
        // the splice itself is rebuilt only when its inputs change.
        let mut splice = std::mem::take(&mut self.splice_cache);
        self.refresh_splice(&mut splice, layout, cached_layout, width);
        let mut notice_lines = self.notice_lines(width);
        // Notices follow the same rhythm as transcript items: one blank row
        // between real content and the first notice block.
        if !layout.lines.is_empty() && !notice_lines.is_empty() {
            notice_lines.insert(0, Line::default());
        }
        let spliced = &splice.rows;
        let splice_shifts = spliced.shifts.as_slice();
        let layout_len = spliced.len(layout.lines.len());
        let viewport = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            width,
            area.height.saturating_sub(2),
        );
        let scrollbar_track = pane_scrollbar_track(area, viewport);
        let content_height = layout_len + notice_lines.len();
        if layout_changed
            && anchor_key == self.layout_cache.key
            && let Some((point, row_offset)) = anchor
            && let Some(line) = self.layout_cache.anchor_line(point)
        {
            // `line` addresses the unspliced layout; rows spliced in above
            // the anchored row shift it down by their count. A view resting
            // inside spliced rows keeps its depth into them.
            let line = line.saturating_add(row_offset);
            self.conversation_scroll.offset = match spliced_depth {
                Some(depth) => splice_shifted(line, splice_shifts, false).saturating_add(depth),
                None => splice_shifted(line, splice_shifts, true),
            };
        }
        self.conversation_splice_shifts.clear();
        self.conversation_splice_shifts
            .extend_from_slice(splice_shifts);
        self.conversation_scroll
            .clamp(content_height, viewport.height);
        // Code rendered plain while out of view is highlighted as it comes
        // within a viewport of the visible rows, before this frame draws
        // it. Highlighting moves no row, so the scroll position holds.
        let (visible, _) = unsplice_line(
            self.conversation_scroll.offset,
            &self.conversation_splice_shifts,
        );
        let height = usize::from(viewport.height.max(1));
        if cached_layout
            && self
                .layout_cache
                .request_highlights(visible.saturating_sub(height)..visible + 2 * height)
        {
            self.ensure_viewed_layout(viewed, width, clock_bucket, draft_agent.as_ref());
            self.refresh_splice(&mut splice, &self.layout_cache.layout, true, width);
        }
        let layout = if cached_layout {
            &self.layout_cache.layout
        } else {
            &empty_layout
        };
        let spliced = &splice.rows;
        let line_at = |index: usize| spliced.line(&layout.lines, index);
        let layout_len = spliced.len(layout.lines.len());
        let content_height = layout_len + notice_lines.len();
        self.hit_map.conversation = Some(viewport);
        self.hit_map.scrollbar = (scrollbar_track.width > 0).then_some(scrollbar_track);
        self.hit_map.blocks.clear();
        let (block_regions, user_regions) = if spliced.is_empty() {
            (&layout.regions, &layout.user_regions)
        } else {
            (&splice.block_regions, &splice.user_regions)
        };
        self.hit_map
            .blocks
            .extend(block_regions.iter().filter_map(|region| {
                block_hit_at(*region, line_at, viewport, self.conversation_scroll.offset)
            }));
        self.hit_map.user_messages.clear();
        self.hit_map
            .user_messages
            .extend(user_regions.iter().filter_map(|region| {
                user_message_hit(*region, line_at, viewport, self.conversation_scroll.offset)
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
        for row in 0..viewport.height {
            let index = self.conversation_scroll.offset + usize::from(row);
            let line = if index < layout_len {
                line_at(index)
            } else {
                notice_lines.get(index - layout_len)
            };
            let Some(line) = line else {
                break;
            };
            frame.render_widget(
                line,
                Rect::new(viewport.x, viewport.y + row, viewport.width, 1),
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
        self.splice_cache = splice;
    }

    /// Lay out the viewed session in its layout cache: whether the layout
    /// changed, and whether it shows the cached layout rather than the
    /// empty-session guidance. `None` without a viewed session.
    fn ensure_viewed_layout(
        &mut self,
        viewed: Option<SessionId>,
        width: u16,
        clock_bucket: u8,
        draft_agent: Option<&AgentId>,
    ) -> Option<(bool, bool)> {
        let (session_id, state) = viewed.and_then(|session_id| {
            self.store
                .sessions
                .get(&session_id)
                .map(|state| (session_id, state))
        })?;
        let changed = !ensure_cached_transcript_layout(
            &mut self.layout_cache,
            session_id,
            state,
            draft_agent,
            self.expanded_blocks.get(&session_id),
            width,
            &self.theme,
            self.highlighter.as_ref(),
            self.tui_config.minimum_event_level,
            clock_bucket,
            self.runtime.model_names(),
        );
        // A fresh session greets with guidance instead of a blank pane; a
        // filtered-down transcript (lines hidden by the event level) keeps
        // its own rows, empty-looking or not.
        Some((
            changed,
            !(state.transcript.is_empty() && state.run_snapshot.is_none()),
        ))
    }

    /// Make `session_id`'s layout cache current, parking the current one,
    /// so switching between a few sessions does not lay each out afresh.
    fn select_layout_cache(&mut self, session_id: SessionId) {
        let current = self.layout_cache.key.map(|key| key.session_id);
        if current.is_none_or(|current| current == session_id) {
            return;
        }
        let parked = self
            .parked_layout_caches
            .iter()
            .position(|cache| cache.key.is_some_and(|key| key.session_id == session_id))
            .map(|index| self.parked_layout_caches.remove(index))
            .unwrap_or_default();
        let previous = std::mem::replace(&mut self.layout_cache, parked);
        self.parked_layout_caches.push(previous);
        for evicted in evict_parked_layouts(
            &mut self.parked_layout_caches,
            PARKED_LAYOUT_CACHES,
            PARKED_LAYOUT_LINES,
        ) {
            // Its markdown keeps rendered blocks only for this layout to
            // reuse; they would outlive it in the session for good.
            if let Some(state) = evicted
                .key
                .and_then(|key| self.store.sessions.get(&key.session_id))
            {
                release_rendered_markdown(state);
            }
        }
    }

    pub(super) fn toggle_block(&mut self, block_id: BlockId) {
        let Some(session_id) = self.selected else {
            return;
        };
        match block_id {
            BlockId::ToolFull { call_id, block } => {
                self.toggle_full_content(session_id, call_id, block as usize);
                return;
            }
            BlockId::ToolSession { session, .. } => {
                self.watch_session(session);
                return;
            }
            BlockId::ToolMemory { call_id, block } => {
                let url = self
                    .store
                    .sessions
                    .get(&session_id)
                    .and_then(|state| state.tools.get(&call_id))
                    .and_then(|tool| match tool.shown_display().get(block as usize) {
                        Some(cookie_agent_protocol::Open::Known(
                            cookie_agent_protocol::DisplayBlock::Link { url, .. },
                        )) => Some(url.clone()),
                        _ => None,
                    });
                if let Some(url) = url {
                    self.open_memory_panel_at(url);
                }
                return;
            }
            BlockId::MemoryChange(seq) => {
                let url = self.store.sessions.get(&session_id).and_then(|state| {
                    state.transcript.iter().rev().find_map(|item| match item {
                        TranscriptItem::MemoryChange { seq: at, url, .. } if *at == seq => {
                            Some(url.clone())
                        }
                        _ => None,
                    })
                });
                if let Some(url) = url {
                    self.open_memory_panel_at(url);
                }
                return;
            }
            _ => {}
        }
        // An explicit layout change preserves what the user is reading. Only
        // output received while already following should advance to the tail.
        self.conversation_scroll.hold();
        let expanded = self.expanded_blocks.entry(session_id).or_default();
        if !expanded.insert(block_id) {
            expanded.remove(&block_id);
        }
    }
}

impl App {
    /// Load the complete content of a call's cut display block with
    /// `artifact.read`, or put loaded content away again. Loading expands
    /// the call's output, whose larger budget then shows it.
    pub(super) fn toggle_full_content(
        &mut self,
        session_id: SessionId,
        call_id: cookie_agent_protocol::ToolCallId,
        block: usize,
    ) {
        let Some(state) = self.store.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(read) = state.toggle_full_content(call_id, block) else {
            return;
        };
        self.expanded_blocks
            .entry(session_id)
            .or_default()
            .insert(BlockId::ToolOutput {
                call_id,
                section: ToolOutputSection::Detail,
            });
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = crate::state::FullContentState::of_read(
                client
                    .read_artifact(cookie_agent_protocol::ArtifactReadParams {
                        session_id,
                        sha256: read.artifact.sha256.clone(),
                        offset: 0,
                        length: read.length,
                    })
                    .await
                    .map_err(|error| error.to_string()),
            );
            let _ = updates.send(super::app::RpcUpdate::FullContentLoaded {
                session_id,
                call_id,
                block,
                state: result,
            });
        });
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
    let mut tool_rows = HashMap::new();
    let mut assistant_part_layout_passes = 0;
    let model_names = ModelDisplayNames::default();
    if let Some(snapshot) = state.run_snapshot.as_deref() {
        append_item_layout(
            &mut layout,
            system_prompt_layout(snapshot, Some(&snapshot.agent), expanded, width, theme),
        );
        if let Some(tools) = state.model_tools.as_deref() {
            append_item_layout(
                &mut layout,
                model_tools_layout(state, tools, expanded, width, theme),
            );
        }
    }
    for item in &state.transcript {
        let mut assistant_child_lines = Vec::new();
        let mut deferred_highlights = Vec::new();
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
                model_names: &model_names,
                assistant_part_cache: &mut assistant_parts,
                assistant_part_layout_passes: &mut assistant_part_layout_passes,
                tool_cache: &mut tool_rows,
                assistant_child_lines: &mut assistant_child_lines,
                defer_highlights: false,
                deferred_highlights: &mut deferred_highlights,
            },
        );
        append_item_layout(&mut layout, item_layout);
    }
    layout
}

/// Remove the oldest of the `parked` layout caches (oldest first) until at
/// most `max_caches` remain holding at most `max_lines` rows together, and
/// return them.
fn evict_parked_layouts(
    parked: &mut Vec<LayoutCache>,
    max_caches: usize,
    max_lines: usize,
) -> Vec<LayoutCache> {
    let mut lines = parked
        .iter()
        .map(|cache| cache.layout.lines.len())
        .sum::<usize>();
    let mut evicted = 0;
    while evicted < parked.len() && (parked.len() - evicted > max_caches || lines > max_lines) {
        lines -= parked[evicted].layout.lines.len();
        evicted += 1;
    }
    parked.drain(..evicted).collect()
}

/// Drop the rendered blocks `state`'s markdown keeps for a layout, keeping
/// their highlighted code: once no layout cache of the session is left,
/// the next layout renders every block afresh anyway.
fn release_rendered_markdown(state: &SessionState) {
    for item in &state.transcript {
        match item {
            TranscriptItem::User { markdown, .. } => crate::markdown::release_rendered(markdown),
            TranscriptItem::Assistant { children, .. } => {
                for child in children {
                    if let AssistantChild::Text { markdown, .. } = child {
                        crate::markdown::release_rendered(markdown);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Append one item to the cached layout after the one-blank-row separator
/// (see `append_item_layout`); the item's chunks are shared, not copied.
fn append_chunked_item(assembled: &mut ChunkedLayout, item: &ChunkedItem) {
    if item.lines.is_empty() {
        return;
    }
    if !assembled.lines.is_empty() {
        assembled.lines.push_chunk(blank_chunk());
    }
    let start_line = assembled.lines.len();
    let end_line = start_line + item.lines.len();
    for chunk in &item.lines.chunks {
        assembled.lines.push_chunk(Chunk::clone(chunk));
    }
    assembled
        .regions
        .extend(item.regions.iter().map(|region| BlockRegion {
            start_line: start_line + region.start_line,
            end_line: start_line + region.end_line,
            ..*region
        }));
    if let Some(seq) = item.user_seq {
        assembled.user_regions.push(UserRegion {
            seq,
            start_line,
            end_line,
        });
    }
}

/// One blank row of breathing room between top-level transcript items, so
/// messages never butt against each other. Items that render nothing
/// (event rows below the level filter) contribute no lines and no spacer —
/// never a leading, trailing, or doubled blank row.
#[cfg(test)]
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

#[cfg(test)]
pub(super) fn block_hit(
    region: BlockRegion,
    lines: &[Line<'static>],
    viewport: Rect,
    scroll_offset: usize,
) -> Option<BlockHit> {
    block_hit_at(region, |index| lines.get(index), viewport, scroll_offset)
}

/// [`block_hit`] over lines read by index, so a spliced view need not be
/// copied into one slice.
pub(super) fn block_hit_at<'a>(
    region: BlockRegion,
    line_at: impl Fn(usize) -> Option<&'a Line<'static>>,
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
    let hovered = start..header_end.max(start);
    let gutter = region.header_gutter.unwrap_or_else(|| {
        hovered
            .filter_map(&line_at)
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
    let action_rect = match region.id {
        BlockId::Tool(_) if region.start_line >= scroll_offset && region.start_line < end => {
            line_at(region.start_line).and_then(|line| {
                let text = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                let at = text
                    .trim_end()
                    .strip_suffix(tool_rows::BACKGROUND_BUTTON)?
                    .len();
                let column = UnicodeWidthStr::width(&text[..at]);
                Some(Rect::new(
                    viewport
                        .x
                        .saturating_add(u16::try_from(column).unwrap_or(u16::MAX)),
                    viewport.y
                        + u16::try_from(region.start_line - scroll_offset).unwrap_or(u16::MAX),
                    u16::try_from(UnicodeWidthStr::width(tool_rows::BACKGROUND_BUTTON))
                        .unwrap_or(u16::MAX),
                    1,
                ))
            })
        }
        _ => None,
    };
    // A memory URL the title row names links to the Memory panel.
    let link_rect = match region.id {
        BlockId::Tool(_) if region.start_line >= scroll_offset && region.start_line < end => {
            line_at(region.start_line).and_then(|line| {
                let text = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                let span = cookie_agent_client_state::tool_header::memory_url_span(&text)?;
                let column = UnicodeWidthStr::width(&text[..span.start]);
                Some(Rect::new(
                    viewport
                        .x
                        .saturating_add(u16::try_from(column).unwrap_or(u16::MAX)),
                    viewport.y
                        + u16::try_from(region.start_line - scroll_offset).unwrap_or(u16::MAX),
                    u16::try_from(UnicodeWidthStr::width(&text[span])).unwrap_or(u16::MAX),
                    1,
                ))
            })
        }
        _ => None,
    };
    (start < end).then(|| BlockHit {
        action_rect,
        link_rect,
        rect: Rect::new(
            viewport.x,
            viewport.y + u16::try_from(start - scroll_offset).unwrap_or(u16::MAX),
            viewport.width,
            u16::try_from(end - start).unwrap_or(u16::MAX),
        ),
        id: region.id,
        toggle_rect: if matches!(
            region.id,
            BlockId::ToolOutput { .. }
                | BlockId::ToolFull { .. }
                | BlockId::ToolSession { .. }
                | BlockId::ToolMemory { .. }
        ) {
            // Action regions are single clickable rows of a tool body.
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
/// The hover paints the message's content columns only, right of the widest
/// leading gutter among its visible rows, like a block header's.
pub(super) fn user_message_hit<'a>(
    region: UserRegion,
    line_at: impl Fn(usize) -> Option<&'a Line<'static>>,
    viewport: Rect,
    scroll_offset: usize,
) -> Option<UserMessageHit> {
    let viewport_end = scroll_offset.saturating_add(usize::from(viewport.height));
    let start = region.start_line.max(scroll_offset);
    let end = region.end_line.min(viewport_end);
    let rect = Rect::new(
        viewport.x,
        viewport.y + u16::try_from(start.saturating_sub(scroll_offset)).unwrap_or(u16::MAX),
        viewport.width,
        u16::try_from(end.saturating_sub(start)).unwrap_or(u16::MAX),
    );
    let gutter = (start..end)
        .filter_map(line_at)
        .map(leading_gutter_columns)
        .max()
        .unwrap_or(0)
        .min(viewport.width);
    (start < end).then(|| UserMessageHit {
        rect,
        hover_rect: Rect {
            x: rect.x.saturating_add(gutter),
            width: rect.width - gutter,
            ..rect
        },
        seq: region.seq,
    })
}

#[cfg(test)]
mod tests;

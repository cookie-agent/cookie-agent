//! Tool call header rows, argument display, and tool block layout.

use super::*;
use ratatui::style::Color;

pub(in crate::ui) use cookie_agent_client_state::tool_header::{
    flatten_header_argument, hint_icon,
};

/// The icon of a call, from its presentation's hint.
pub(in crate::ui) fn tool_icon(tool: &crate::state::ToolCallState) -> &'static str {
    hint_icon(tool.presentation.hint.as_ref().map(|hint| hint.rendered()))
}

/// The compact role name `tool_block_lines` brackets onto a narrow tool row.
pub(super) fn tool_row_short(role: Role) -> &'static str {
    match role {
        Role::ToolRunning => "T…",
        Role::ToolSuccess => "T✓",
        Role::ToolFailure => "T!",
        _ => "T",
    }
}

/// Narrowest row where `tool_block_lines` can still keep the assistant gutter.
pub(super) const TOOL_HEADER_GUTTER_MIN_COLUMNS: u16 = 8;

/// The `"│ "` assistant gutter a header row hangs behind at normal widths.
pub(super) const TOOL_HEADER_GUTTER_COLUMNS: usize = 2;

/// The full `"[T…] "` role label that replaces the gutter below
/// `TOOL_HEADER_GUTTER_MIN_COLUMNS`, leaving the row no room for a wide icon.
pub(super) const TOOL_HEADER_LABEL_COLUMNS: usize = 5;

/// Columns the compact role label may spend on a narrow row. It shortens with
/// the row and always leaves `reserve` columns for text — at least one, and as
/// much more as the widest grapheme the row has to carry whole needs — so
/// neither the label nor its wrapped continuation can exceed the viewport.
pub(super) fn tool_row_label_columns(width: u16, reserve: usize) -> usize {
    usize::from(width)
        .saturating_sub(reserve.max(1))
        .clamp(1, TOOL_HEADER_LABEL_COLUMNS)
}

/// The bracketed role label itself, shortened to that budget: `"[T…] "` while it
/// fits, then `"[T…]"`, `"[T"`, and `"[…"` down to a single column.
pub(super) fn tool_row_prefix(short: &str, width: u16, reserve: usize) -> String {
    let room = tool_row_label_columns(width, reserve);
    let full = format!("[{short}] ");
    if UnicodeWidthStr::width(full.as_str()) <= room {
        return full;
    }
    crate::ui::app::truncate_with_ellipsis(&format!("[{short}]"), room)
}

/// Columns the header text may use at `width`, after the prefix
/// `tool_block_lines` puts in front of the row. The header never wraps, so the
/// markers, label, argument, and status suffix share exactly this budget.
pub(super) fn tool_header_content_width(width: u16) -> usize {
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
pub(super) fn tool_header_chrome(icon: &str, chevron: char, width: u16) -> String {
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
pub(super) fn tool_header_row(
    tool: &crate::state::ToolCallState,
    chevron: char,
    suffix: &str,
    width: u16,
) -> (String, String) {
    let chrome = tool_header_chrome(tool_icon(tool), chevron, width);
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
pub(super) fn header_argument_width(label: &str, budget: usize) -> usize {
    budget.saturating_sub(UnicodeWidthStr::width(label) + 1)
}

/// The primary argument as the header shows it after the title, flattened
/// to one line; `None` when there is none or the title already names it.
pub(in crate::ui) fn header_argument(tool: &crate::state::ToolCallState) -> Option<String> {
    tool.header_argument()
}

pub(super) fn tool_header_title(tool: &crate::state::ToolCallState, budget: usize) -> String {
    let title = tool.presentation.title.as_str();
    let argument_width = header_argument_width(title, budget);
    let Some(argument) = header_argument(tool) else {
        return crate::ui::app::truncate_with_ellipsis(title, budget);
    };
    if argument_width == 0 {
        // The label alone fills the row (long plugin names at narrow widths):
        // keep the label legible and drop the argument rather than wrap.
        return crate::ui::app::truncate_with_ellipsis(title, budget);
    }
    format!(
        "{title} {}",
        crate::ui::app::truncate_with_ellipsis(&argument, argument_width)
    )
}

/// The button at the right end of a running background-capable call's title
/// row; clicking it moves the call to the background.
pub(in crate::ui) const BACKGROUND_BUTTON: &str = "[background]";
/// Title columns a row must keep beside the button for the button to show.
const MIN_TITLE_BESIDE_BUTTON: usize = 24;

/// A compact or expanded tool row inside its owning assistant item. Running
/// pulses a suffix; terminal failures retain their exact concise markers.
/// `pending_name` identifies a committed placeholder whose execution has not
/// started yet: it renders a neutral pending row, never an error.
pub(super) fn tool_child_layout(
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
                format!("{} ▸ {} · pending", hint_icon(None), name),
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
    let has_body = tool.failure.is_some()
        || (tool.status == ToolStatus::Running && tool.progress.is_some())
        || !tool.shown_display().is_empty();
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
    let chevron = if is_expanded { '▾' } else { '▸' };
    // A title keeps a margin column on each side where the theme has them.
    let header_width = if panel_margin(context.width) {
        context.width - 2
    } else {
        context.width
    };
    // A backgrounded call says so; a running call of a background-capable
    // tool offers the move at the right end of its title row.
    let suffix = if tool.backgrounded {
        format!("{suffix} · background")
    } else {
        suffix
    };
    // A running call's latest progress message reads as its status; an
    // expanded call shows it above its output instead.
    let suffix = match tool.progress.as_deref() {
        Some(progress) if tool.status == ToolStatus::Running && !is_expanded => {
            format!(" · {}{suffix}", flatten_header_argument(progress))
        }
        _ => suffix,
    };
    let button_width = UnicodeWidthStr::width(BACKGROUND_BUTTON);
    let content_width = tool_header_content_width(header_width);
    let offer_background = tool.status == ToolStatus::Running
        && tool.backgroundable
        && content_width >= button_width + 1 + MIN_TITLE_BESIDE_BUTTON;
    let row_width = if offer_background {
        header_width.saturating_sub(u16::try_from(button_width + 1).unwrap_or(u16::MAX))
    } else {
        header_width
    };
    let (mut header, title) = tool_header_row(tool, chevron, &suffix, row_width);
    if offer_background {
        let used = UnicodeWidthStr::width(header.as_str());
        header.push_str(&" ".repeat(content_width.saturating_sub(used + button_width)));
        header.push_str(BACKGROUND_BUTTON);
    }
    let mut body = Vec::new();
    if is_expanded {
        body.extend(expanded_title_lines(
            tool,
            ExpandedHeader {
                row: header,
                title: &title,
                chevron,
                suffix: &suffix,
            },
            context.width,
        ));
        // The output band keeps a blank row above and below its text so
        // nothing touches the band's edge.
        let padded = panel_margin(context.width);
        // Everything under the title: the call's arguments, then what it
        // shows.
        let style = BlockStyle {
            theme: context.theme,
            highlighter: context.highlighter,
            width: panel_text_width(context.width),
        };
        let mut panel =
            argument_section_lines(tool, &style, context.minimum_event_level, !padded, has_body);
        if has_body {
            panel.extend(tool_body_lines(
                tool,
                &style,
                section_expanded(ToolOutputSection::Detail),
                &mut budget,
            ));
        }
        // Every expanded tool is one panel: a grey title block over a lighter
        // grey output band.
        for line in &mut body {
            line.band = Some(ToolBand::Title);
        }
        if padded && !panel.is_empty() {
            panel.insert(0, ToolBodyLine::wrapped(Line::default()));
            panel.push(ToolBodyLine::wrapped(Line::default()));
        }
        for line in &mut panel {
            line.band = Some(ToolBand::Terminal);
        }
        body.extend(panel);
        // One clear row between the panel and whatever follows it.
        body.push(ToolBodyLine::wrapped(Line::default()));
    } else {
        // Collapsed rows are muted body text; only a failure's status suffix
        // keeps the error colour, so failed calls still stand out.
        let line = match header.strip_suffix(suffix.as_str()) {
            Some(head) if matches!(role, Role::ToolFailure) && !suffix.is_empty() => {
                Line::from(vec![
                    Span::raw(head.to_owned()),
                    Span::styled(suffix.clone(), context.theme.tool_failure()),
                ])
            }
            _ => Line::from(header),
        };
        body.push(ToolBodyLine::wrapped(line));
    }
    // Collapsed rows are muted body text; expanded blocks are ordinary text
    // whose header suffix still names a failure.
    let text_style = if is_expanded {
        context.theme.body()
    } else {
        context.theme.muted_text()
    };
    let rendered = tool_block_lines_with(text_style, role, body, context.width, context.theme);
    let mut regions = vec![BlockRegion {
        id: block_id,
        start_line: 0,
        end_line: rendered.lines.len(),
        header_lines: Some(rendered.header_lines),
        header_gutter: Some(header_gutter_columns(&rendered)),
    }];
    if let Some(call_id) = call_id {
        let chrome = rendered.chrome;
        regions.extend(
            rendered
                .actions
                .into_iter()
                .map(|(action, start_line, end_line)| BlockRegion {
                    id: action.block_id(call_id),
                    start_line,
                    end_line,
                    header_lines: None,
                    // A notice row hangs behind the same gutter as the rows it
                    // stands in for.
                    header_gutter: chrome.get(start_line).copied(),
                }),
        );
    }
    ItemLayout {
        regions,
        lines: rendered.lines,
        user_seq: None,
    }
}

/// Bytes of argument rows the expanded title panel shows before eliding.
const MAX_ARGUMENT_BYTES: usize = 4 * 1024;

/// Rows of arguments the expanded title panel shows before eliding.
const MAX_ARGUMENT_LINES: usize = 48;

/// Columns a panel row's text gets inside the block's `│ ` gutter and, where
/// the panel has them, its margins.
pub(super) fn panel_text_width(width: u16) -> u16 {
    let margins = if panel_margin(width) { 2 } else { 0 };
    width.saturating_sub(2 + margins).max(1)
}

/// The call's arguments, set apart from what it shows: a blank row, a muted
/// `arguments` label, the presentation's argument blocks, and a blank row
/// before any output. A call whose presentation has no argument blocks shows
/// its raw arguments instead, as diagnostic detail at the `info` filter and
/// below.
fn argument_section_lines(
    tool: &crate::state::ToolCallState,
    style: &BlockStyle<'_>,
    minimum_event_level: crate::state::EventLevel,
    leading_blank: bool,
    output_follows: bool,
) -> Vec<ToolBodyLine> {
    let theme = style.theme;
    let muted = theme.muted();
    let limits = RenderLimits {
        lines: MAX_ARGUMENT_LINES,
        bytes: MAX_ARGUMENT_BYTES,
    };
    let arguments = tool.shown_arguments();
    let rows = if arguments.is_empty() {
        // A presentation with argument blocks or a preview is complete as
        // given; only one with neither falls back to the raw arguments.
        if minimum_event_level > crate::state::EventLevel::Info
            || !tool.presentation.arguments.is_empty()
            || !tool.presentation.preview.is_empty()
        {
            return Vec::new();
        }
        let raw = serde_json::from_str::<serde_json::Value>(&tool.arguments)
            .ok()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| tool.arguments.clone());
        bounded_safe_display_text(&raw, muted, limits.lines, limits.bytes)
            .into_iter()
            .map(ToolBodyLine::wrapped)
            .collect::<Vec<_>>()
    } else {
        let mut budget = RenderBudget::new(limits);
        let mut sink = LineSink::new(&mut budget, limits);
        let mut rows = display_rows(&arguments, &[], style, &mut sink);
        let omitted = sink.omitted();
        if omitted > 0 {
            rows.push(ToolBodyLine::wrapped(Line::styled(
                format!("… {omitted} more lines not shown"),
                muted,
            )));
        }
        rows
    };
    if rows.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    // A padded panel already opens on a blank row.
    if leading_blank {
        lines.push(ToolBodyLine::wrapped(Line::default()));
    }
    lines.push(ToolBodyLine::wrapped(Line::styled("arguments", muted)));
    lines.extend(rows);
    if output_follows {
        lines.push(ToolBodyLine::wrapped(Line::default()));
    }
    lines
}

/// The collapsed header row and the pieces an expanded title block reuses.
pub(super) struct ExpandedHeader<'a> {
    pub(super) row: String,
    pub(super) title: &'a str,
    pub(super) chevron: char,
    pub(super) suffix: &'a str,
}

/// An expanded tool's title block: the header row, or, when the collapsed
/// header had to shorten its title or argument, the whole of them on rows
/// hanging under where the title starts. Every row is part of the clickable
/// header.
fn expanded_title_lines(
    tool: &crate::state::ToolCallState,
    header: ExpandedHeader<'_>,
    width: u16,
) -> Vec<ToolBodyLine> {
    let name = flatten_header_argument(tool.presentation.title.as_str());
    let chrome = tool_header_chrome(tool_icon(tool), header.chevron, width);
    let full = match header_argument(tool) {
        Some(argument) => format!("{name} {argument}"),
        None => name.clone(),
    };
    if header.title == full {
        let indent =
            UnicodeWidthStr::width(chrome.as_str()) + UnicodeWidthStr::width(name.as_str()) + 1;
        return vec![ToolBodyLine::title(Line::from(header.row), indent)];
    }
    let indent = UnicodeWidthStr::width(chrome.as_str());
    vec![ToolBodyLine::title(
        Line::from(format!("{chrome}{full}{}", header.suffix)),
        indent,
    )]
}

/// Identity for a tool row: a started call or a committed placeholder index.
pub(super) enum BlockKey {
    Call(cookie_agent_protocol::ToolCallId),
    CommittedTool { turn_seq: u64, content_index: u32 },
}

impl From<cookie_agent_protocol::ToolCallId> for BlockKey {
    fn from(call_id: cookie_agent_protocol::ToolCallId) -> Self {
        Self::Call(call_id)
    }
}

pub(super) enum ToolBodyLineKind {
    Wrapped,
    /// Wrapped with continuation rows indented by `indent` columns.
    Hanging {
        indent: usize,
    },
    Code {
        first_gutter: Vec<Span<'static>>,
        continuation_gutter: Vec<Span<'static>>,
    },
}

/// Paint one banded row out to the block's full `width`, not its widest row,
/// so short output still reads as one solid panel. Spans behind the row's
/// first `gutter_spans` take `tint` or else the band; spans that already carry
/// a background (the left margin) keep it, and a `padded` row ends on one
/// plain band column, so a tinted row sits inset in the band. A theme without
/// band colours gets the same layout, uncoloured.
pub(super) fn paint_band(
    line: &mut Line<'static>,
    gutter_spans: usize,
    width: u16,
    background: Option<Color>,
    tint: Option<Color>,
    padded: bool,
) {
    let fill = usize::from(width).saturating_sub(line.width());
    let right = usize::from(padded).min(fill);
    let start = gutter_spans.min(line.spans.len());
    if fill > right {
        line.spans
            .push(Span::raw(crate::markdown::spaces(fill - right)));
    }
    if let Some(row) = tint.or(background) {
        for span in &mut line.spans[start..] {
            if span.style.bg.is_none() {
                span.style = span.style.bg(row);
            }
        }
    }
    if right > 0 {
        let mut margin = Span::raw(crate::markdown::spaces(right));
        if let Some(background) = background {
            margin.style = margin.style.bg(background);
        }
        line.spans.push(margin);
    }
}

/// The one-column left margin behind a panel row's `│ ` gutter. It wears the
/// gutter's own style, which is how copying recognises it as chrome in every
/// theme, plus the band's background when the row sits on one.
pub(super) fn margin_span(theme: &Theme, background: Option<Color>) -> Span<'static> {
    let mut style = theme.assistant();
    if let Some(background) = background {
        style = style.bg(background);
    }
    Span::styled(" ", style)
}

/// Whether panel titles and bands keep a one-column margin on each side:
/// only on blocks wide enough to spare it.
pub(super) fn panel_margin(width: u16) -> bool {
    width >= MIN_PADDED_BAND_WIDTH
}

/// Narrowest block that pads its bands; below it every column carries text.
pub(super) const MIN_PADDED_BAND_WIDTH: u16 = 20;

/// A full-width background behind a tool body row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolBand {
    /// The expanded title block.
    Title,
    /// Tool output.
    Terminal,
}

/// What clicking a tool body row does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolRowAction {
    /// Expands or collapses an output section.
    Toggle(ToolOutputSection),
    /// Loads, or puts away, the complete content of the cut display block
    /// at this index.
    LoadFull(usize),
    /// Opens a session the display refers to.
    OpenSession(SessionId),
    /// Opens the memory the `link` block at this index names.
    OpenMemory(usize),
}

impl ToolRowAction {
    /// The clickable region the row becomes in `call_id`'s block.
    pub(super) fn block_id(self, call_id: cookie_agent_protocol::ToolCallId) -> BlockId {
        match self {
            Self::Toggle(section) => BlockId::ToolOutput { call_id, section },
            Self::LoadFull(block) => BlockId::ToolFull {
                call_id,
                block: u32::try_from(block).unwrap_or(u32::MAX),
            },
            Self::OpenSession(session) => BlockId::ToolSession { call_id, session },
            Self::OpenMemory(block) => BlockId::ToolMemory {
                call_id,
                block: u32::try_from(block).unwrap_or(u32::MAX),
            },
        }
    }
}

pub(super) struct ToolBodyLine {
    pub(super) line: Line<'static>,
    pub(super) kind: ToolBodyLineKind,
    pub(super) action: Option<ToolRowAction>,
    pub(super) band: Option<ToolBand>,
    /// Part of the block's clickable header (an expanded title block row).
    pub(super) header: bool,
    /// Row tint over the band inside its padding (a diff's added or removed
    /// line); `None` keeps the plain band.
    pub(super) tint: Option<Color>,
}

impl ToolBodyLine {
    pub(super) fn wrapped(line: Line<'static>) -> Self {
        Self {
            line,
            kind: ToolBodyLineKind::Wrapped,
            action: None,
            band: None,
            header: false,
            tint: None,
        }
    }

    /// A row of an expanded title block, wrapping under `indent`.
    pub(super) fn title(line: Line<'static>, indent: usize) -> Self {
        Self {
            line,
            kind: ToolBodyLineKind::Hanging { indent },
            action: None,
            band: None,
            header: true,
            tint: None,
        }
    }

    pub(super) fn code(line: Line<'static>) -> Self {
        Self::guttered_code(line, Vec::new(), Vec::new())
    }

    pub(super) fn guttered_code(
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
            action: None,
            band: None,
            header: false,
            tint: None,
        }
    }

    /// A row whose wrapped rows hang under `indent`.
    pub(super) fn hanging(line: Line<'static>, indent: usize) -> Self {
        Self {
            kind: ToolBodyLineKind::Hanging { indent },
            ..Self::wrapped(line)
        }
    }

    pub(super) fn with_tint(mut self, tint: Option<Color>) -> Self {
        self.tint = tint;
        self
    }

    pub(super) fn with_action(mut self, action: ToolRowAction) -> Self {
        self.action = Some(action);
        self
    }

    pub(super) fn toggle(line: Line<'static>, section: ToolOutputSection) -> Self {
        Self::wrapped(line).with_action(ToolRowAction::Toggle(section))
    }
}

pub(super) const OUTPUT_NOTICE_RESERVE_BYTES: usize = 96;

pub(super) struct RenderBudget {
    pub(super) remaining: RenderLimits,
}

impl RenderBudget {
    pub(super) fn new(limits: RenderLimits) -> Self {
        Self { remaining: limits }
    }

    pub(super) fn section_capacity(
        &self,
        limits: RenderLimits,
        future_sections: usize,
    ) -> RenderLimits {
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

    pub(super) fn consume(
        &mut self,
        capacity: &mut RenderLimits,
        text: &str,
    ) -> Option<(String, bool)> {
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

    pub(super) fn consume_notice(&mut self, text: &str) -> bool {
        if self.remaining.lines == 0 || self.remaining.bytes < text.len() {
            return false;
        }
        self.remaining.lines -= 1;
        self.remaining.bytes -= text.len();
        true
    }
}

pub(super) fn output_section_limits(expanded: bool) -> RenderLimits {
    if expanded {
        EXPANDED_TOOL_OUTPUT_LIMITS
    } else {
        COLLAPSED_TOOL_OUTPUT_LIMITS
    }
}

/// What an expanded call shows under its arguments: why it failed, the
/// running call's progress message, and its display blocks, all from one
/// render budget, then the row that expands or collapses them.
pub(super) fn tool_body_lines(
    tool: &crate::state::ToolCallState,
    style: &BlockStyle<'_>,
    expanded: bool,
    budget: &mut RenderBudget,
) -> Vec<ToolBodyLine> {
    let theme = style.theme;
    let mut sink = LineSink::new(budget, output_section_limits(expanded));
    let mut output = Vec::new();
    if let Some(failure) = &tool.failure {
        for line in failure.split('\n') {
            // Expanded text is ordinary text; the header suffix already
            // names the failure.
            if let Some(text) = sink.line(line) {
                output.push(ToolBodyLine::wrapped(Line::from(text)));
            }
        }
    }
    if tool.status == ToolStatus::Running
        && let Some(progress) = &tool.progress
        && let Some(text) = sink.line(progress)
    {
        output.push(ToolBodyLine::wrapped(Line::styled(
            format!("⋯ {text}"),
            theme.muted(),
        )));
    }
    let blocks = display_rows(tool.shown_display(), &tool.full, style, &mut sink);
    if !output.is_empty() && !blocks.is_empty() {
        output.push(ToolBodyLine::wrapped(Line::default()));
    }
    output.extend(blocks);
    let omitted = sink.omitted();
    append_output_notice(
        &mut output,
        omitted,
        expanded,
        ToolOutputSection::Detail,
        sink.budget(),
        theme,
    );
    output
}

/// Tool children render inside the assistant item without a standalone
/// `TOOL` header: the compact/expanded rows keep the assistant gutter and
/// take only their status style.
pub(super) struct ToolBlockLayout {
    pub(super) lines: Vec<Line<'static>>,
    /// Rows that act when clicked, by their start and end line.
    pub(super) actions: Vec<(ToolRowAction, usize, usize)>,
    pub(super) header_lines: usize,
    /// Chrome columns per row, indexed alongside `lines`: the width of the
    /// spans this builder hung in front of each row, counted from what it
    /// built rather than from what the row says.
    pub(super) chrome: Vec<u16>,
}

/// A collapsed-style tool block: muted body text. `role` still picks the
/// narrow-row status label.
pub(super) fn tool_block_lines(
    role: Role,
    body: Vec<ToolBodyLine>,
    width: u16,
    theme: &Theme,
) -> ToolBlockLayout {
    tool_block_lines_with(theme.muted_text(), role, body, width, theme)
}

/// [`tool_block_lines`] with an explicit base text style: an expanded block
/// uses ordinary text rather than its status colour.
pub(super) fn tool_block_lines_with(
    style: Style,
    role: Role,
    body: Vec<ToolBodyLine>,
    width: u16,
    theme: &Theme,
) -> ToolBlockLayout {
    let mut lines = Vec::new();
    let mut actions = Vec::new();
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
        let band = body_line.band;
        let tint = body_line.tint;
        let header = body_line.header;
        let action = body_line.action;
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
        let background = match band {
            Some(ToolBand::Title) => theme.tool_title_background(),
            Some(ToolBand::Terminal) => theme.terminal_background(),
            None => None,
        };
        // Banded rows and title rows keep one margin column on each side of
        // their text, so a title sits in the same column collapsed or
        // expanded. The left one rides in the gutter (chrome, so hover and
        // copy skip it); the right one is reserved from the wrap width.
        let margin = panel_margin(width) && (band.is_some() || index == 0 || header);
        let wrap_width = if margin { width - 1 } else { width };
        let gutter = || {
            let mut gutter = vec![Span::styled("│ ", theme.assistant())];
            if margin {
                gutter.push(margin_span(theme, background));
            }
            gutter
        };
        match body_line.kind {
            ToolBodyLineKind::Wrapped | ToolBodyLineKind::Hanging { .. } if width < 8 => {
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
            // Only the `│ ` span is chrome here: the margin behind it takes the
            // hover fill with the row's text, so a highlighted title or notice
            // covers its padding. Copying still skips it by its style.
            ToolBodyLineKind::Wrapped => {
                let gutter = gutter();
                let chrome = usize::from(gutter_fits(&gutter, &line, wrap_width));
                lines.extend(repeated_prefixed_wrapped_line(gutter, line, wrap_width));
                gutters.resize(lines.len(), chrome);
            }
            ToolBodyLineKind::Hanging { indent } => {
                let gutter = gutter();
                let chrome = usize::from(gutter_fits(&gutter, &line, wrap_width));
                lines.extend(repeated_prefixed_hanging_line(
                    gutter, line, wrap_width, indent,
                ));
                gutters.resize(lines.len(), chrome);
            }
            ToolBodyLineKind::Code {
                mut first_gutter,
                mut continuation_gutter,
            } => {
                let prefix = if width >= 3 { gutter() } else { Vec::new() };
                let prefix_width = prefix
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum::<usize>();
                let available = usize::from(wrap_width.max(1)).saturating_sub(prefix_width);
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
        // An expanded title block's rows are all header: one click target,
        // one hover highlight.
        if index == 0 || header {
            header_lines = lines.len();
        }
        if let Some(action) = action {
            actions.push((action, start, lines.len()));
        }
        if band.is_some() {
            banded_rows.extend(
                (start..lines.len()).map(|row| (row, gutters[row], background, tint, margin)),
            );
        }
    }
    for (index, chrome, background, tint, padded) in banded_rows {
        // The band stops at the block's own `│ ` gutter, its first chrome
        // span; line numbers and diff markers behind it are band too.
        // Counted, never guessed — `│ ` arriving as command output is a tree
        // row that must be banded, and a gutter welded to its text by
        // `append_span` is chrome that must not be.
        paint_band(
            &mut lines[index],
            chrome.min(1),
            width,
            background,
            tint,
            padded,
        );
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
        actions,
        header_lines,
        chrome,
    }
}

/// Chrome columns of a tool block's header rows: the widest chrome row of the
/// header, since one highlight spans them all.
pub(super) fn header_gutter_columns(rendered: &ToolBlockLayout) -> u16 {
    rendered
        .chrome
        .iter()
        .take(rendered.header_lines)
        .copied()
        .max()
        .unwrap_or(0)
}

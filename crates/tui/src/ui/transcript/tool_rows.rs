//! Tool call header rows, argument display, and tool block layout.

use super::*;

pub(super) fn tool_icon(title: &str) -> &'static str {
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

/// Flatten an on-wire primary argument for single-line header display. The wire
/// type is byte-capped only, so control characters reach the client: map each to
/// a space before collapsing whitespace runs.
pub(super) fn flatten_header_argument(argument: &str) -> String {
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
pub(super) fn header_argument_width(label: &str, budget: usize) -> usize {
    budget.saturating_sub(UnicodeWidthStr::width(label) + 1)
}

/// Abbreviate a primary argument to at most `cap` display columns. Path-shaped
/// arguments keep their head and tail; everything else cuts at the right edge.
pub(super) fn abbreviate_tool_argument(title: &str, argument: &str, cap: usize) -> String {
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
    crate::ui::app::truncate_with_ellipsis(&argument, cap)
}

pub(super) fn tool_header_title(tool: &crate::state::ToolCallState, budget: usize) -> String {
    let title = tool.presentation.title.as_str();
    let label = if title == "read" { "Read" } else { title };
    let argument_width = header_argument_width(label, budget);
    let Some(argument) = tool.presentation.primary_argument.as_ref() else {
        return crate::ui::app::truncate_with_ellipsis(label, budget);
    };
    if argument_width == 0 {
        // The label alone fills the row (long plugin names at narrow widths):
        // keep the label legible and drop the argument rather than wrap.
        return crate::ui::app::truncate_with_ellipsis(label, budget);
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
            if tool_name == "bash"
                && let Some(command) = command
            {
                body.extend(
                    bash_command_lines(command)
                        .into_iter()
                        .map(|line| ToolBodyLine::wrapped(Line::from(line))),
                );
            } else {
                body.push(ToolBodyLine::wrapped(Line::from(format!(
                    "arguments: {}",
                    display_tool_arguments(tool, arguments.as_ref())
                ))));
            }
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
pub(super) struct ParsedToolArguments<'a> {
    // Deliberately strict: non-string edit/write fields reject the structured
    // view and fall back to the bounded raw-arguments rendering.
    #[serde(borrow, rename = "filePath")]
    pub(super) file_path: Option<Cow<'a, str>>,
    #[serde(borrow)]
    pub(super) path: Option<Cow<'a, str>>,
    #[serde(borrow, rename = "oldString")]
    pub(super) before: Option<Cow<'a, str>>,
    #[serde(borrow, rename = "newString")]
    pub(super) after: Option<Cow<'a, str>>,
    #[serde(borrow)]
    pub(super) content: Option<Cow<'a, str>>,
    #[serde(borrow)]
    pub(super) command: Option<Cow<'a, str>>,
}

impl<'a> ParsedToolArguments<'a> {
    pub(super) fn parse(arguments: &'a str) -> Option<Self> {
        serde_json::from_str(arguments).ok()
    }

    pub(super) fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref().or(self.path.as_deref())
    }
}

pub(super) fn display_tool_arguments(
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

/// Bytes of a bash command the expanded row shows before eliding the rest.
const MAX_COMMAND_BYTES: usize = 2 * 1024;

/// The `❯ command` body of an expanded bash row: one line per command line,
/// so a heredoc or multi-line script keeps its line breaks instead of showing
/// each newline as a replacement character. The byte budget spans all lines,
/// and an elided tail is marked on the last line shown.
fn bash_command_lines(command: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut remaining = MAX_COMMAND_BYTES;
    let mut source_lines = command.split('\n').peekable();
    while let Some(line) = source_lines.next() {
        let prefix = if lines.is_empty() { "❯ " } else { "  " };
        let (text, complete) = sanitized_display_prefix(line, remaining);
        remaining = remaining.saturating_sub(text.len());
        let elided = !complete || (remaining == 0 && source_lines.peek().is_some());
        lines.push(format!("{prefix}{text}{}", if elided { "…" } else { "" }));
        if elided {
            break;
        }
        // Count the newline itself so the budget matches the source length.
        remaining = remaining.saturating_sub(1);
    }
    lines
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
    Code {
        first_gutter: Vec<Span<'static>>,
        continuation_gutter: Vec<Span<'static>>,
    },
}

pub(super) struct ToolBodyLine {
    pub(super) line: Line<'static>,
    pub(super) kind: ToolBodyLineKind,
    pub(super) output_toggle: Option<ToolOutputSection>,
    pub(super) banded: bool,
}

impl ToolBodyLine {
    pub(super) fn wrapped(line: Line<'static>) -> Self {
        Self {
            line,
            kind: ToolBodyLineKind::Wrapped,
            output_toggle: None,
            banded: false,
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
            output_toggle: None,
            banded: false,
        }
    }

    pub(super) fn toggle(line: Line<'static>, section: ToolOutputSection) -> Self {
        Self {
            line,
            kind: ToolBodyLineKind::Wrapped,
            output_toggle: Some(section),
            banded: false,
        }
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

pub(super) struct SectionRenderer<'a> {
    pub(super) budget: &'a mut RenderBudget,
    pub(super) capacity: RenderLimits,
    pub(super) fully_rendered: usize,
}

impl<'a> SectionRenderer<'a> {
    pub(super) fn new(
        budget: &'a mut RenderBudget,
        expanded: bool,
        future_sections: usize,
    ) -> Self {
        let limits = output_section_limits(expanded);
        let capacity = budget.section_capacity(limits, future_sections);
        Self {
            budget,
            capacity,
            fully_rendered: 0,
        }
    }

    pub(super) fn take(&mut self, text: &str) -> Option<String> {
        let (text, complete) = self.budget.consume(&mut self.capacity, text)?;
        self.fully_rendered += usize::from(complete);
        Some(text)
    }

    pub(super) fn exhausted(&self) -> bool {
        self.capacity.lines == 0 || self.capacity.bytes == 0
    }
}

pub(super) fn output_section_limits(expanded: bool) -> RenderLimits {
    if expanded {
        EXPANDED_TOOL_OUTPUT_LIMITS
    } else {
        COLLAPSED_TOOL_OUTPUT_LIMITS
    }
}

/// Expanded tool detail lines. File reads use source line numbers carried by
/// the result. Edit/write arguments and strict unified-diff output use diff
/// gutters. All sections consume one aggregate per-tool render budget.
pub(super) fn tool_body_lines(
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

/// Tool children render inside the assistant item without a standalone
/// `TOOL` header: the compact/expanded rows keep the assistant gutter and
/// take only their status style.
pub(super) struct ToolBlockLayout {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) output_toggles: Vec<(ToolOutputSection, usize, usize)>,
    pub(super) header_lines: usize,
    /// Chrome columns per row, indexed alongside `lines`: the width of the
    /// spans this builder hung in front of each row, counted from what it
    /// built rather than from what the row says.
    pub(super) chrome: Vec<u16>,
}

pub(super) fn tool_block_lines(
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
pub(super) fn header_gutter_columns(rendered: &ToolBlockLayout) -> u16 {
    rendered
        .chrome
        .iter()
        .take(rendered.header_lines)
        .copied()
        .max()
        .unwrap_or(0)
}

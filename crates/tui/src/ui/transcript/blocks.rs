//! Display blocks as tool body rows (spec 12.6). Rows follow from each
//! block's kind alone: nothing here knows which tool produced a block.

use super::sgr::{Rendition, styled_runs};
use super::*;

use cookie_agent_protocol::{
    ArtifactRef, DiffAsOf, DiffBlock, DiffChange, DiffLineOp, DisplayBlock, KeyValueEntry, Open,
    TerminalSegment, TerminalStream,
};

use crate::state::{FullContent, FullContentState};

/// What the rows of a block list are drawn with.
pub(super) struct BlockStyle<'a> {
    pub(super) theme: &'a Theme,
    pub(super) highlighter: &'a dyn crate::markdown::Highlighter,
    /// Columns a row's text gets once its gutters and margins are taken.
    pub(super) width: u16,
}

/// The source lines one block list takes from a render budget, and how many
/// it had in all, so the rows can say how many the budget left out.
pub(super) struct LineSink<'a> {
    budget: &'a mut RenderBudget,
    capacity: RenderLimits,
    rendered: usize,
    total: usize,
}

impl<'a> LineSink<'a> {
    pub(super) fn new(budget: &'a mut RenderBudget, limits: RenderLimits) -> Self {
        let capacity = budget.section_capacity(limits, 0);
        Self {
            budget,
            capacity,
            rendered: 0,
            total: 0,
        }
    }

    /// One source line, sanitized and cut to the budget, and whether all of
    /// it fit; `None` once the budget is spent. Every call counts the line.
    fn take(&mut self, text: &str) -> Option<(String, bool)> {
        self.total += 1;
        let (text, complete) = self.budget.consume(&mut self.capacity, text)?;
        self.rendered += usize::from(complete);
        Some((text, complete))
    }

    pub(super) fn line(&mut self, text: &str) -> Option<String> {
        self.take(text).map(|(text, _)| text)
    }

    /// One source line of styled runs, cut to the budget like [`Self::take`].
    fn runs(&mut self, runs: Vec<(Style, String)>) -> Option<Vec<Span<'static>>> {
        let runs = runs
            .into_iter()
            .map(|(style, text)| (style, safe_display_text(&text)))
            .collect::<Vec<_>>();
        let joined = runs
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<String>();
        let (kept, _) = self.take(&joined)?;
        let mut left = kept.len();
        let mut spans = Vec::new();
        for (style, text) in runs {
            if left == 0 {
                break;
            }
            let end = text.len().min(left);
            left -= end;
            spans.push(Span::styled(text[..end].to_owned(), style));
        }
        Some(spans)
    }

    /// A row that is not a source line (a block's notice or exit code):
    /// shown while the budget has room for it.
    fn notice(&mut self, text: &str) -> bool {
        self.budget.consume_notice(text)
    }

    /// The source lines the budget left out, whole or in part.
    pub(super) fn omitted(&self) -> usize {
        self.total.saturating_sub(self.rendered)
    }

    pub(super) fn budget(&mut self) -> &mut RenderBudget {
        self.budget
    }
}

/// `text`'s lines: none for empty text, and no empty last line for a final
/// newline.
fn source_lines(text: &str) -> impl Iterator<Item = &str> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    (!text.is_empty())
        .then_some(text)
        .into_iter()
        .flat_map(|text| text.split('\n'))
}

/// The rows of `blocks`, one block after another with a blank row between
/// them. `full` is the complete content loaded for cut blocks; a block that
/// has it shows that instead of its cut rows. Blocks of a kind this build
/// does not know show nothing (the daemon downgrades them before they get
/// here).
pub(super) fn display_rows(
    blocks: &[Open<DisplayBlock>],
    full: &[FullContent],
    style: &BlockStyle<'_>,
    sink: &mut LineSink<'_>,
) -> Vec<ToolBodyLine> {
    let mut out = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let Open::Known(block) = block else {
            continue;
        };
        let mut rows = Vec::new();
        let loaded = full
            .iter()
            .find(|content| content.block == index)
            .map(|content| &content.state);
        match (block_full(block), loaded) {
            (Some(_), Some(FullContentState::Loaded { text, truncated })) => {
                full_content_rows(block, index, text, *truncated, style, sink, &mut rows);
            }
            _ => block_rows(block, index, loaded, style, sink, &mut rows),
        }
        if rows.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(ToolBodyLine::wrapped(Line::default()));
        }
        out.extend(rows);
    }
    out
}

/// The `full` artifact a cut block names.
use cookie_agent_client_state::session_actions::block_full;

fn block_rows(
    block: &DisplayBlock,
    index: usize,
    loaded: Option<&FullContentState>,
    style: &BlockStyle<'_>,
    sink: &mut LineSink<'_>,
    out: &mut Vec<ToolBodyLine>,
) {
    let theme = style.theme;
    let cut = |omitted: Option<u64>, unit: Unit| Cut {
        omitted: omitted.unwrap_or(0),
        unit,
        index,
        full: block_full(block),
        loaded,
        detail: None,
    };
    match block {
        DisplayBlock::Text {
            text,
            omitted_lines,
        } => {
            for line in source_lines(text) {
                if let Some(text) = sink.line(line) {
                    out.push(ToolBodyLine::wrapped(Line::from(text)));
                }
            }
            cut(*omitted_lines, Unit::LINE).rows(sink, theme, out);
        }
        DisplayBlock::Markdown {
            text,
            omitted_lines,
        } => {
            markdown_rows(text, style, sink, out);
            cut(*omitted_lines, Unit::LINE).rows(sink, theme, out);
        }
        DisplayBlock::Code {
            text,
            language,
            start_line,
            omitted_lines,
        } => {
            let mut number = *start_line;
            let mut rows = Vec::new();
            for line in source_lines(text) {
                if let Some(text) = sink.line(line) {
                    rows.push((number, text));
                }
                number = number.map(|number| number + 1);
            }
            out.extend(numbered_rows(rows, language.as_deref(), style));
            cut(*omitted_lines, Unit::LINE).rows(sink, theme, out);
        }
        DisplayBlock::FileExcerpt {
            path,
            language,
            start_line,
            lines,
            total_lines,
            omitted_lines,
            ..
        } => {
            excerpt_header(
                path,
                *start_line,
                lines.len(),
                *total_lines,
                sink,
                theme,
                out,
            );
            let mut rows = Vec::new();
            for (offset, line) in lines.iter().enumerate() {
                if let Some(text) = sink.line(line) {
                    rows.push((Some(start_line + offset as u64), text));
                }
            }
            let language = language.as_deref().or_else(|| path_extension(path));
            out.extend(numbered_rows(rows, language, style));
            cut(*omitted_lines, Unit::LINE).rows(sink, theme, out);
        }
        DisplayBlock::Diff(diff) => {
            diff_header(diff, sink, theme, out);
            let mut rows = Vec::new();
            for hunk in &diff.hunks {
                let header = format!(
                    "@@ -{} +{} @@",
                    hunk_range(hunk.old_start, hunk.old_lines),
                    hunk_range(hunk.new_start, hunk.new_lines)
                );
                if let Some(text) = sink.line(&header) {
                    rows.push(RenderedDiffRow {
                        kind: DiffRowKind::Hunk,
                        text,
                    });
                }
                let (mut old, mut new) = (hunk.old_start, hunk.new_start);
                for line in &hunk.lines {
                    let kind = match line.op {
                        DiffLineOp::Context => {
                            old += 1;
                            new += 1;
                            DiffRowKind::Context(new - 1)
                        }
                        DiffLineOp::Add => {
                            new += 1;
                            DiffRowKind::Added(new - 1)
                        }
                        DiffLineOp::Remove => {
                            old += 1;
                            DiffRowKind::Removed(old - 1)
                        }
                    };
                    if let Some(text) = sink.line(&line.text) {
                        rows.push(RenderedDiffRow { kind, text });
                    }
                }
            }
            let language = diff
                .language
                .as_deref()
                .or_else(|| path_extension(&diff.path));
            out.extend(diff_body_lines(rows, language, style));
            let omitted = diff.omitted.filter(|omitted| omitted.hunks > 0);
            Cut {
                omitted: omitted.map_or(0, |omitted| omitted.hunks),
                unit: Unit::HUNK,
                index,
                full: diff.full.as_ref(),
                loaded,
                detail: None,
            }
            .with_detail(omitted.map(|omitted| {
                format!(
                    "{} {}",
                    omitted.lines,
                    if omitted.lines == 1 { "line" } else { "lines" }
                )
            }))
            .rows(sink, theme, out);
        }
        DisplayBlock::Terminal {
            segments,
            exit_code,
            omitted_lines,
            ..
        } => {
            for runs in terminal_lines(segments, theme) {
                if let Some(spans) = sink.runs(runs) {
                    out.push(ToolBodyLine::code(Line::from(spans)));
                }
            }
            cut(*omitted_lines, Unit::LINE).rows(sink, theme, out);
            if let Some(code) = exit_code {
                let text = format!("exit code {code}");
                if sink.notice(&text) {
                    let tone = if *code == 0 {
                        theme.muted()
                    } else {
                        theme.tool_failure()
                    };
                    out.push(ToolBodyLine::wrapped(Line::styled(text, tone)));
                }
            }
        }
        DisplayBlock::KeyValue {
            entries,
            omitted_entries,
        } => {
            key_value_rows(entries, sink, theme, out);
            cut(*omitted_entries, Unit::ENTRY).rows(sink, theme, out);
        }
        DisplayBlock::Table {
            columns,
            rows,
            omitted_rows,
        } => {
            table_rows(columns, rows, sink, theme, out);
            cut(*omitted_rows, Unit::ROW).rows(sink, theme, out);
        }
        DisplayBlock::Plan { entries } => {
            for line in super::events::plan_lines(entries, theme) {
                let runs = line
                    .spans
                    .into_iter()
                    .map(|span| (span.style, span.content.into_owned()))
                    .collect();
                if let Some(spans) = sink.runs(runs) {
                    out.push(ToolBodyLine::wrapped(Line::from(spans)));
                }
            }
        }
        DisplayBlock::Json { value } => {
            let text = serde_json::to_string_pretty(value).unwrap_or_default();
            let rows = source_lines(&text)
                .filter_map(|line| sink.line(line).map(|text| (None, text)))
                .collect();
            out.extend(numbered_rows(rows, Some("json"), style));
        }
        DisplayBlock::Media { artifact, alt } => {
            let chip = crate::state::attachment_chip(artifact);
            if let Some(chip) = sink.line(&chip) {
                let mut spans = vec![Span::raw(chip)];
                if let Some(alt) = alt.as_deref().filter(|alt| !alt.is_empty()) {
                    spans.push(Span::styled(
                        format!(" — {}", flatten_header_argument(alt)),
                        theme.muted(),
                    ));
                }
                out.push(ToolBodyLine::wrapped(Line::from(spans)));
            }
        }
        DisplayBlock::Link { url, label } => {
            let shown = label.as_deref().unwrap_or(url);
            if let Some(shown) = sink.line(shown) {
                let mut spans = vec![Span::styled(shown, theme.link())];
                if label.is_some() {
                    spans.push(Span::styled(
                        format!(" <{}>", safe_display_text(url)),
                        theme.muted(),
                    ));
                }
                out.push(ToolBodyLine::wrapped(Line::from(spans)));
            }
        }
        DisplayBlock::SessionRef {
            session_id,
            short_id,
            label,
        } => {
            let mut text = format!(
                "↗ session {}",
                short_id
                    .as_deref()
                    .map_or_else(|| session_id.to_string(), str::to_owned)
            );
            if let Some(label) = label.as_deref().filter(|label| !label.is_empty()) {
                text.push_str(" · ");
                text.push_str(&flatten_header_argument(label));
            }
            if let Some(text) = sink.line(&text) {
                out.push(
                    ToolBodyLine::wrapped(Line::styled(text, theme.link()))
                        .with_action(ToolRowAction::OpenSession(*session_id)),
                );
            }
        }
    }
}

/// What a cut left out of a block, in units such as lines.
#[derive(Clone, Copy)]
struct Unit {
    one: &'static str,
    many: &'static str,
}

impl Unit {
    const LINE: Self = Self {
        one: "line",
        many: "lines",
    };
    const HUNK: Self = Self {
        one: "hunk",
        many: "hunks",
    };
    const ENTRY: Self = Self {
        one: "entry",
        many: "entries",
    };
    const ROW: Self = Self {
        one: "row",
        many: "rows",
    };
}

/// A block's `omitted` count and, when the block names its complete
/// content, the row that loads it.
struct Cut<'a> {
    omitted: u64,
    unit: Unit,
    index: usize,
    full: Option<&'a ArtifactRef>,
    loaded: Option<&'a FullContentState>,
    detail: Option<String>,
}

impl<'a> Cut<'a> {
    fn with_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }

    fn rows(self, sink: &mut LineSink<'_>, theme: &Theme, out: &mut Vec<ToolBodyLine>) {
        if self.omitted == 0 && self.full.is_none() {
            return;
        }
        let unit = if self.omitted == 1 {
            self.unit.one
        } else {
            self.unit.many
        };
        let mut text = format!("… {} more {unit}", self.omitted);
        if let Some(detail) = &self.detail {
            text.push_str(&format!(" ({detail})"));
        }
        text.push_str(" not shown");
        let action = self.full.map(|full| {
            let size = cookie_agent_protocol::human_bytes(full.byte_length);
            match self.loaded {
                Some(FullContentState::Loading) => format!(" · loading the full output ({size})…"),
                Some(FullContentState::Failed(error)) => {
                    format!(" · could not load the full output ({error}); click to retry")
                }
                _ => format!(" · click to load the full output ({size})"),
            }
        });
        if let Some(action) = &action {
            text.push_str(action);
        }
        if !sink.notice(&text) {
            return;
        }
        let row = ToolBodyLine::wrapped(Line::styled(text, theme.muted()));
        out.push(match action {
            Some(_) => row.with_action(ToolRowAction::LoadFull(self.index)),
            None => row,
        });
    }
}

/// A block's rows from its loaded complete content, then the row that puts
/// the cut rows back.
fn full_content_rows(
    block: &DisplayBlock,
    index: usize,
    text: &str,
    truncated: bool,
    style: &BlockStyle<'_>,
    sink: &mut LineSink<'_>,
    out: &mut Vec<ToolBodyLine>,
) {
    let theme = style.theme;
    match block {
        DisplayBlock::Diff(diff) => {
            diff_header(diff, sink, theme, out);
            let mut rows = Vec::new();
            for_each_unified_diff_row(text, |row| {
                // The file headers repeat what the block's header says.
                if row.kind == DiffRowKind::Metadata
                    && (row.text.starts_with("--- ") || row.text.starts_with("+++ "))
                {
                    return true;
                }
                if let Some(text) = sink.line(&row.text) {
                    rows.push(RenderedDiffRow {
                        kind: row.kind,
                        text,
                    });
                }
                true
            });
            let language = diff
                .language
                .as_deref()
                .or_else(|| path_extension(&diff.path));
            out.extend(diff_body_lines(rows, language, style));
        }
        DisplayBlock::Terminal { .. } => {
            let segment = TerminalSegment {
                stream: TerminalStream::Stdout,
                text: cookie_agent_protocol::sanitize_terminal_text(text).into_owned(),
            };
            for runs in terminal_lines(std::slice::from_ref(&segment), theme) {
                if let Some(spans) = sink.runs(runs) {
                    out.push(ToolBodyLine::code(Line::from(spans)));
                }
            }
        }
        DisplayBlock::FileExcerpt {
            path,
            language,
            start_line,
            ..
        } => {
            let mut rows = Vec::new();
            for (offset, line) in source_lines(text).enumerate() {
                if let Some(text) = sink.line(line) {
                    rows.push((Some(start_line + offset as u64), text));
                }
            }
            let language = language.as_deref().or_else(|| path_extension(path));
            out.extend(numbered_rows(rows, language, style));
        }
        _ => {
            for line in source_lines(text) {
                if let Some(text) = sink.line(line) {
                    out.push(ToolBodyLine::wrapped(Line::from(text)));
                }
            }
        }
    }
    let mut notice = String::from("▴ full output");
    if truncated {
        notice.push_str(&format!(
            " (its first {})",
            cookie_agent_protocol::human_bytes(crate::state::MAX_FULL_CONTENT_BYTES)
        ));
    }
    notice.push_str(" · click to show the cut display");
    if sink.notice(&notice) {
        out.push(
            ToolBodyLine::wrapped(Line::styled(notice, theme.muted()))
                .with_action(ToolRowAction::LoadFull(index)),
        );
    }
}

fn hunk_range(start: u64, lines: u64) -> String {
    if lines == 1 {
        start.to_string()
    } else {
        format!("{start},{lines}")
    }
}

/// A diff's header row: how the file changed, its path, and the line
/// counts of the whole change.
fn diff_header(
    diff: &DiffBlock,
    sink: &mut LineSink<'_>,
    theme: &Theme,
    out: &mut Vec<ToolBodyLine>,
) {
    let change = match diff.change {
        DiffChange::Created => "created",
        DiffChange::Deleted => "deleted",
        DiffChange::Renamed => "renamed",
        DiffChange::Modified | DiffChange::Unknown => "modified",
    };
    let path = match (&diff.old_path, diff.change) {
        (Some(old), DiffChange::Renamed) => format!("{old} → {}", diff.path),
        _ => diff.path.clone(),
    };
    let Some(path) = sink.line(&path) else {
        return;
    };
    let mut spans = vec![
        Span::styled(format!("{change} "), theme.muted()),
        Span::styled(path, theme.heading()),
        Span::styled(" · ", theme.muted()),
        Span::styled(format!("+{}", diff.added), theme.diff_added()),
        Span::raw(" "),
        Span::styled(format!("-{}", diff.removed), theme.diff_removed()),
    ];
    if diff.as_of == DiffAsOf::Prepare {
        spans.push(Span::styled(" · preview", theme.muted()));
    }
    out.push(ToolBodyLine::wrapped(Line::from(spans)));
}

fn excerpt_header(
    path: &str,
    start_line: u64,
    lines: usize,
    total_lines: Option<u64>,
    sink: &mut LineSink<'_>,
    theme: &Theme,
    out: &mut Vec<ToolBodyLine>,
) {
    let Some(path) = sink.line(path) else {
        return;
    };
    let last = start_line + (lines as u64).saturating_sub(1);
    let mut range = if lines == 0 {
        String::new()
    } else if lines == 1 {
        format!(" · line {start_line}")
    } else {
        format!(" · lines {start_line}–{last}")
    };
    if let Some(total) = total_lines {
        range.push_str(&format!(" of {total}"));
    }
    out.push(ToolBodyLine::wrapped(Line::from(vec![
        Span::styled(path, theme.heading()),
        Span::styled(range, theme.muted()),
    ])));
}

/// Code rows, highlighted in `language` when it is known, behind a gutter
/// of file line numbers when the rows have them.
fn numbered_rows(
    rows: Vec<(Option<u64>, String)>,
    language: Option<&str>,
    style: &BlockStyle<'_>,
) -> Vec<ToolBodyLine> {
    let theme = style.theme;
    let highlighted = language.map(|language| {
        let source = rows
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        style.highlighter.highlight_stable(
            &crate::markdown::normalized_language(language),
            &source,
            theme,
        )
    });
    let number_width = rows
        .iter()
        .filter_map(|(number, _)| *number)
        .max()
        .map(|number| number.max(1).ilog10() as usize + 1);
    rows.into_iter()
        .enumerate()
        .map(|(index, (number, text))| {
            let line = highlighted
                .as_ref()
                .and_then(|lines| lines.get(index))
                .cloned()
                .unwrap_or_else(|| Line::from(text));
            match (number, number_width) {
                (Some(number), Some(width)) => ToolBodyLine::guttered_code(
                    line,
                    vec![
                        Span::styled(format!("{number:>width$}"), theme.code_gutter()),
                        Span::styled(" │ ", theme.code_gutter()),
                    ],
                    vec![
                        Span::styled(" ".repeat(width), theme.code_gutter()),
                        Span::styled(" │ ", theme.code_gutter()),
                    ],
                ),
                _ => ToolBodyLine::code(line),
            }
        })
        .collect()
}

fn markdown_rows(
    text: &str,
    style: &BlockStyle<'_>,
    sink: &mut LineSink<'_>,
    out: &mut Vec<ToolBodyLine>,
) {
    let document = crate::markdown::MarkdownDocument::new(text.to_owned()).finished();
    for line in crate::markdown::render_markdown_lines_width(
        &document,
        style.theme,
        style.highlighter,
        style.width,
    ) {
        let plain = line
            .line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let Some((taken, complete)) = sink.take(&plain) else {
            continue;
        };
        let row = if complete {
            line.line
        } else {
            Line::from(taken)
        };
        out.push(match line.kind {
            MarkdownLineKind::Prose => ToolBodyLine::wrapped(row),
            MarkdownLineKind::ListItem {
                continuation_indent,
            } => ToolBodyLine::hanging(row, continuation_indent),
            MarkdownLineKind::Code | MarkdownLineKind::Table => ToolBodyLine::code(row),
        });
    }
}

/// Columns a `key_value` key takes at most before the values start.
const MAX_KEY_COLUMNS: usize = 24;

fn key_value_rows(
    entries: &[KeyValueEntry],
    sink: &mut LineSink<'_>,
    theme: &Theme,
    out: &mut Vec<ToolBodyLine>,
) {
    let keys = entries
        .iter()
        .map(|entry| {
            crate::ui::app::truncate_with_ellipsis(
                &flatten_header_argument(&entry.key),
                MAX_KEY_COLUMNS,
            )
        })
        .collect::<Vec<_>>();
    let width = keys
        .iter()
        .map(|key| UnicodeWidthStr::width(key.as_str()))
        .max()
        .unwrap_or(0);
    let indent = width + 2;
    for (key, entry) in keys.iter().zip(entries) {
        let mut lines = source_lines(&entry.value).peekable();
        if lines.peek().is_none() {
            if sink.line("").is_some() {
                out.push(ToolBodyLine::wrapped(Line::styled(
                    key.clone(),
                    theme.muted(),
                )));
            }
            continue;
        }
        for (index, line) in lines.enumerate() {
            let Some(text) = sink.line(line) else {
                continue;
            };
            let lead = if index == 0 {
                let pad = width.saturating_sub(UnicodeWidthStr::width(key.as_str()));
                Span::styled(format!("{key}{}  ", " ".repeat(pad)), theme.muted())
            } else {
                Span::raw(" ".repeat(indent))
            };
            out.push(ToolBodyLine::hanging(
                Line::from(vec![lead, Span::raw(text)]),
                indent,
            ));
        }
    }
}

/// Columns a table cell takes at most.
const MAX_CELL_COLUMNS: usize = 40;

fn table_rows(
    columns: &[String],
    rows: &[Vec<String>],
    sink: &mut LineSink<'_>,
    theme: &Theme,
    out: &mut Vec<ToolBodyLine>,
) {
    let cell = |text: &str| {
        crate::ui::app::truncate_with_ellipsis(&flatten_header_argument(text), MAX_CELL_COLUMNS)
    };
    let header = columns.iter().map(|text| cell(text)).collect::<Vec<_>>();
    let body = rows
        .iter()
        .map(|row| row.iter().map(|text| cell(text)).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let widths = (0..header.len())
        .map(|column| {
            std::iter::once(&header)
                .chain(&body)
                .filter_map(|row| row.get(column))
                .map(|text| UnicodeWidthStr::width(text.as_str()))
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let render = |row: &[String]| {
        row.iter()
            .zip(&widths)
            .enumerate()
            .map(|(index, (text, width))| {
                let last = index + 1 == row.len();
                let pad = if last {
                    0
                } else {
                    width.saturating_sub(UnicodeWidthStr::width(text.as_str())) + 2
                };
                format!("{text}{}", " ".repeat(pad))
            })
            .collect::<String>()
    };
    if let Some(text) = sink.line(&render(&header)) {
        out.push(ToolBodyLine::code(Line::styled(
            text,
            theme.muted().add_modifier(ratatui::style::Modifier::BOLD),
        )));
    }
    for row in &body {
        if let Some(text) = sink.line(&render(row)) {
            out.push(ToolBodyLine::code(Line::from(text)));
        }
    }
}

/// Terminal segments as lines of styled runs. Each stream keeps its own SGR
/// state across lines; stderr is drawn in the warning tone unless its own
/// colours say otherwise. A segment that does not start a line continues
/// the line the previous one left open, whichever stream it came from.
fn terminal_lines(segments: &[TerminalSegment], theme: &Theme) -> Vec<Vec<(Style, String)>> {
    let mut renditions = [Rendition::default(); 2];
    let mut lines = Vec::new();
    let mut current: Vec<(Style, String)> = Vec::new();
    for segment in segments {
        let (slot, base) = match segment.stream {
            TerminalStream::Stderr => (1, theme.warning()),
            TerminalStream::Stdout | TerminalStream::Unknown => (0, Style::default()),
        };
        for (index, piece) in segment.text.split('\n').enumerate() {
            if index > 0 {
                lines.push(std::mem::take(&mut current));
            }
            for (rendition, text) in styled_runs(piece, &mut renditions[slot], theme) {
                current.push((rendition.style(base), text));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

//! Tool output rendering: read output, generic output, and display limits.

use super::*;

pub(super) fn display_line_count(text: &str) -> usize {
    text.split('\n').count()
}

pub(super) const MAX_EXPANDED_BODY_LINES: usize = 64;

pub(super) const MAX_EXPANDED_BODY_BYTES: usize = 8 * 1024;

pub(super) const MAX_EXPANDED_TOOL_OUTPUT_LINES: usize = 1024;

pub(super) const MAX_EXPANDED_TOOL_OUTPUT_BYTES: usize = 128 * 1024;

pub(super) const MAX_SYSTEM_PROMPT_BODY_LINES: usize = 256;

pub(super) const MAX_SYSTEM_PROMPT_BODY_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy)]
pub(super) struct RenderLimits {
    pub(super) lines: usize,
    pub(super) bytes: usize,
}

pub(super) const COLLAPSED_TOOL_OUTPUT_LIMITS: RenderLimits = RenderLimits {
    lines: MAX_EXPANDED_BODY_LINES,
    bytes: MAX_EXPANDED_BODY_BYTES,
};

pub(super) const EXPANDED_TOOL_OUTPUT_LIMITS: RenderLimits = RenderLimits {
    lines: MAX_EXPANDED_TOOL_OUTPUT_LINES,
    bytes: MAX_EXPANDED_TOOL_OUTPUT_BYTES,
};

pub(super) fn bounded_safe_display_text(
    text: &str,
    style: Style,
    max_lines: usize,
    max_bytes: usize,
) -> Vec<Line<'static>> {
    bounded_safe_display_lines(text.split('\n'), style, max_lines, max_bytes)
}

pub(super) fn bounded_safe_display_lines<'a>(
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

pub(super) fn safe_display_text(text: &str) -> String {
    sanitized_display_prefix(text, usize::MAX).0
}

pub(super) fn sanitized_display_prefix(text: &str, max_bytes: usize) -> (String, bool) {
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

pub(super) fn path_extension(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next()?;
    name.rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
        .map(|(_, extension)| extension)
}

pub(super) struct ReadOutput<'a> {
    pub(super) preamble: &'a str,
    pub(super) content: &'a str,
    pub(super) metadata: &'a str,
}

pub(super) fn parse_read_output(detail: &str) -> Option<ReadOutput<'_>> {
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

pub(super) fn parse_numbered_read_line(line: &str) -> Option<(usize, &str)> {
    let (number, text) = line.split_once(": ")?;
    Some((number.parse().ok()?, text))
}

pub(super) fn render_read_output(
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
pub(super) struct OutputText<'a> {
    pub(super) text: &'a str,
    pub(super) original_lines: usize,
}

impl<'a> OutputText<'a> {
    pub(super) fn complete(text: &'a str) -> Self {
        Self {
            text,
            original_lines: text.lines().count(),
        }
    }
}

pub(super) fn generic_output_lines(
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

pub(super) fn append_output_notice(
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

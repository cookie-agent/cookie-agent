//! Assistant turn layout: headers, attribution, body, and thinking blocks.

use super::*;

pub(super) fn assistant_item_layout(
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
    let mut previous_is_prose = None;
    for child in children {
        // Prose and the compact tool/thinking rows are separated by one blank
        // gutter row whenever the reply switches between them; runs of rows
        // stay tight. Separators sit between parts, never inside a part's
        // line range, so streaming splices are unaffected.
        let is_prose = matches!(child, AssistantChild::Text { .. });
        if previous_is_prose.is_some_and(|previous| previous != is_prose) {
            layout.lines.extend(assistant_body_line(
                Line::default(),
                context.width,
                context.theme,
            ));
        }
        previous_is_prose = Some(is_prose);
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
            AssistantChild::Notice { text } => {
                layout.lines.extend(assistant_body_line(
                    Line::from(Span::styled(text.clone(), context.theme.muted())),
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

pub(super) fn assistant_part_layout_key(
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
        | AssistantChild::MediaFile { .. }
        | AssistantChild::Notice { .. } => None,
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

pub(super) fn splice_active_assistant_part(
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

pub(super) fn assistant_child_layout(
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
            let (body, text_rows) = thinking_body_lines(text, width, theme);
            let hidden_lines = text_rows.max(1);
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
                let noun = if hidden_lines == 1 { "line" } else { "lines" };
                format!("💭 ▸ {status} ({hidden_lines} {noun} hidden)")
            };
            // Thinking is secondary to the reply: its row reads as muted body
            // text whether collapsed or expanded. Expanded, it heads a panel
            // like an expanded tool's, on the title band.
            // Collapsed, the title keeps the same margin, so it does not
            // shift a column when it expands.
            let label = Line::from(Span::styled(label, theme.muted_text()));
            let (mut lines, header_gutter) = if panel_margin(width) {
                let title_band = theme.tool_title_background().filter(|_| key.expanded);
                let (mut rows, gutter) = margin_rows(label, width, theme, title_band);
                if key.expanded {
                    for row in &mut rows {
                        paint_band(row, gutter.min(1), width, title_band, None, true);
                    }
                }
                // Hover starts behind the margin, as on a tool title.
                (rows, (gutter > 0).then_some(3))
            } else {
                (assistant_body_line(label, width, theme), None)
            };
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
                    header_gutter,
                }],
                lines,
                user_seq: None,
            }
        }
        AssistantChild::Tool { .. }
        | AssistantChild::Attribution { .. }
        | AssistantChild::CommittedTool { .. }
        | AssistantChild::MediaFile { .. }
        | AssistantChild::Notice { .. } => {
            unreachable!("tool children use tool_child_layout")
        }
    }
}

/// A settled thinking duration as compact text: seconds under a minute,
/// then minutes and seconds. Sub-second spans never reach the label (they
/// are filtered to plain "thought" by the caller).
pub(super) fn format_thinking_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

pub(super) fn assistant_header(attribution: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    // The frozen `Agent • Model` attribution wraps at tiny widths and is
    // never reduced to a tag: it is the sole producer identity. The agent
    // leads in the bold role style; the model trails muted so the header
    // reads as one name, not two competing labels.
    let (agent, model) = attribution
        .split_once(" • ")
        .map_or((attribution, None), |(agent, model)| (agent, Some(model)));
    let text = if width >= 8 {
        format!("╭─ {agent}")
    } else {
        agent.to_owned()
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
    let mut spans = vec![Span::styled(text, theme.assistant())];
    if let Some(model) = model {
        spans.push(Span::styled(format!(" • {model}"), theme.muted()));
    }
    spans.push(Span::raw(" "));
    wrapped_line(Line::from(spans), wrap_width)
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

pub(super) fn attribution_line(
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
pub(super) fn assistant_footer_line(
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
        .map(|cost| crate::ui::app::format_cost_usd(cost as f64 / 1_000_000_000_000.0));
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
                crate::ui::app::format_token_count(context_tokens),
            ),
            theme.muted(),
        )),
        width,
    ))
}

pub(super) fn assistant_body_line(
    line: Line<'static>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let prefix = (width >= 3).then(|| vec![Span::styled("│ ", theme.assistant())]);
    repeated_prefixed_wrapped_line(prefix.unwrap_or_default(), line, width)
}

pub(super) fn assistant_markdown_body_line(
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

/// Expanded thinking, with the number of rows its text wraps to: muted
/// italic text on the output band, padded like a tool panel's and followed
/// by a clear row. Blocks too narrow to pad mark each row with a muted `│`
/// instead; a dashed `┆` renders as slanted strokes in some fonts (macOS
/// Terminal).
pub(super) fn thinking_body_lines(
    text: &str,
    width: u16,
    theme: &Theme,
) -> (Vec<Line<'static>>, usize) {
    let style = theme
        .muted_text()
        .add_modifier(ratatui::style::Modifier::ITALIC);
    let text_line = |text: &str| Line::styled(text.to_owned(), style);
    if panel_margin(width) {
        let background = theme.terminal_background();
        let blank = || padded_band_rows(Line::default(), width, theme, background);
        let text_rows = text
            .split('\n')
            .flat_map(|text| padded_band_rows(text_line(text), width, theme, background))
            .collect::<Vec<_>>();
        let rows = text_rows.len();
        let mut lines = blank();
        lines.extend(text_rows);
        lines.extend(blank());
        lines.extend(assistant_body_line(Line::default(), width, theme));
        return (lines, rows);
    }
    let lines = text
        .split('\n')
        .flat_map(|text| {
            let prefix = if width >= 5 {
                vec![
                    Span::styled("│ ", theme.assistant()),
                    Span::styled("│ ", style),
                ]
            } else if width >= 3 {
                vec![Span::styled("│ ", style)]
            } else {
                Vec::new()
            };
            repeated_prefixed_wrapped_line(prefix, text_line(text), width)
        })
        .collect::<Vec<_>>();
    let rows = lines.len();
    (lines, rows)
}

/// One line behind the assistant `│ ` gutter on a padded band: a band column
/// on each side of the text, which wraps between them.
fn padded_band_rows(
    line: Line<'static>,
    width: u16,
    theme: &Theme,
    background: Option<ratatui::style::Color>,
) -> Vec<Line<'static>> {
    let (mut rows, gutter) = margin_rows(line, width, theme, background);
    for row in &mut rows {
        paint_band(row, gutter.min(1), width, background, None, true);
    }
    rows
}

/// One line behind the assistant `│ ` gutter and its one-column margin
/// (on `background` when the row sits on a band), wrapped one column short
/// of `width` so the right side keeps its margin too, with the number of
/// chrome spans each row starts with (none when the gutter did not fit).
fn margin_rows(
    line: Line<'static>,
    width: u16,
    theme: &Theme,
    background: Option<ratatui::style::Color>,
) -> (Vec<Line<'static>>, usize) {
    let prefix = vec![
        Span::styled("│ ", theme.assistant()),
        margin_span(theme, background),
    ];
    let wrap_width = width - 1;
    let gutter = if gutter_fits(&prefix, &line, wrap_width) {
        prefix.len()
    } else {
        0
    };
    (
        repeated_prefixed_wrapped_line(prefix, line, wrap_width),
        gutter,
    )
}

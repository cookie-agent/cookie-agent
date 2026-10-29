//! Assistant turn layout: headers, attribution, body, and thinking blocks.

use cookie_agent_client_state::transcript_mode;

use super::*;

pub(super) fn assistant_item_layout(
    state: &SessionState,
    item_id: u64,
    attribution: &crate::state::FrozenAssistantAttribution,
    children: &[AssistantChild],
    context: &mut TranscriptRenderContext<'_>,
) -> ChunkedItem {
    let mut layout = ChunkedItem::default();
    layout.lines.push_lines(assistant_header(
        attribution.header(context.model_names).as_str(),
        context.width,
        context.theme,
    ));
    let mut previous_is_prose = None;
    let fold = (context.transcript == TranscriptMode::Reduced)
        .then(|| transcript_mode::reply_start(children))
        .flatten();
    let Some(reply_start) = fold else {
        for child in children {
            push_assistant_child(
                state,
                item_id,
                child,
                &mut layout,
                &mut previous_is_prose,
                context,
            );
        }
        return finish_assistant_layout(state, item_id, layout, context);
    };
    // The reduced mode folds the block's work into one row above its reply;
    // a call waiting on the user stays out of the fold until answered.
    let (work, reply) = children.split_at(reply_start);
    let open = context
        .expanded
        .is_some_and(|blocks| blocks.contains(&BlockId::Work(item_id)));
    let summary = transcript_mode::work_summary(state, item_id, work, context.now);
    let subtext = summary
        .live
        .then(|| transcript_mode::working_subtext(state, work));
    layout.push_layout(work_row_layout(
        item_id,
        &summary,
        subtext.as_deref(),
        open,
        context.width,
        context.theme,
    ));
    previous_is_prose = Some(false);
    for child in work {
        let shown = open
            || matches!(child, AssistantChild::Tool { call_id } if transcript_mode::needs_user(state, call_id));
        if shown {
            push_assistant_child(
                state,
                item_id,
                child,
                &mut layout,
                &mut previous_is_prose,
                context,
            );
        } else {
            // Rows spliced in before a folded child land below the row.
            context.assistant_child_lines.push(layout.lines.len());
        }
    }
    for child in reply {
        push_assistant_child(
            state,
            item_id,
            child,
            &mut layout,
            &mut previous_is_prose,
            context,
        );
    }
    finish_assistant_layout(state, item_id, layout, context)
}

/// Closes an assistant block's layout with its footer.
fn finish_assistant_layout(
    state: &SessionState,
    item_id: u64,
    mut layout: ChunkedItem,
    context: &mut TranscriptRenderContext<'_>,
) -> ChunkedItem {
    // The block footer closes the run: one muted, gutter-aligned row with
    // generation speed and context use, from committed-turn usage and
    // durable event timestamps. Passive — no region, no hover — and absent
    // entirely when the data is missing.
    if let Some(footer) = assistant_footer_line(state, item_id, context.width, context.theme) {
        layout.lines.push_lines(footer);
    }
    layout
}

/// Appends one child of an assistant block, with the blank row that
/// separates prose from the compact tool and thinking rows.
fn push_assistant_child(
    state: &SessionState,
    item_id: u64,
    child: &AssistantChild,
    layout: &mut ChunkedItem,
    previous_is_prose: &mut Option<bool>,
    context: &mut TranscriptRenderContext<'_>,
) {
    context.assistant_child_lines.push(layout.lines.len());
    // Prose and the compact tool/thinking rows are separated by one blank
    // gutter row whenever the reply switches between them; runs of rows
    // stay tight. Separators sit between parts, never inside a part's
    // rows, so each part stays one shared chunk.
    let is_prose = matches!(child, AssistantChild::Text { .. });
    if previous_is_prose.is_some_and(|previous| previous != is_prose) {
        layout.lines.push_lines(assistant_body_line(
            Line::default(),
            context.width,
            context.theme,
        ));
    }
    *previous_is_prose = Some(is_prose);
    match child {
        AssistantChild::Text { .. } | AssistantChild::Thinking { .. } => {
            let key = assistant_part_layout_key(state, item_id, child, context);
            // A part with code left plain renders again once the code
            // is asked for, or wherever highlighting is not deferred.
            let defer = context.defer_highlights;
            let cached = context
                .assistant_part_cache
                .get(&child.id())
                .filter(|cached| {
                    cached.key == key
                        && cached
                            .deferred
                            .iter()
                            .all(|(_, deferred)| defer && !deferred.is_requested())
                });
            let start = layout.lines.len();
            if let Some(cached) = cached {
                layout.push_chunks(&cached.lines, &cached.regions);
                context
                    .deferred_highlights
                    .extend(shifted_deferred(&cached.deferred, start));
            } else {
                let previous = context.assistant_part_cache.remove(&child.id());
                let cached = match child {
                    AssistantChild::Text { markdown, .. } => {
                        let (lines, blocks, deferred) = text_part_lines(
                            markdown,
                            previous.as_ref().map_or(&[], |previous| &previous.blocks),
                            context.width,
                            context.theme,
                            context.highlighter,
                            defer,
                        );
                        CachedAssistantPartLayout {
                            key,
                            lines,
                            regions: Vec::new(),
                            blocks,
                            thinking_rows: ThinkingRows::default(),
                            deferred,
                        }
                    }
                    _ => {
                        let mut thinking_rows = previous
                            .map(|previous| previous.thinking_rows)
                            .unwrap_or_default();
                        let part_layout = assistant_child_layout(
                            child,
                            key,
                            context.width,
                            context.theme,
                            context.highlighter,
                            &mut thinking_rows,
                        );
                        let mut lines = LineChunks::default();
                        lines.push_lines(part_layout.lines);
                        CachedAssistantPartLayout {
                            key,
                            lines,
                            regions: part_layout.regions,
                            blocks: Vec::new(),
                            thinking_rows,
                            deferred: Vec::new(),
                        }
                    }
                };
                layout.push_chunks(&cached.lines, &cached.regions);
                context
                    .deferred_highlights
                    .extend(shifted_deferred(&cached.deferred, start));
                context.assistant_part_cache.insert(child.id(), cached);
                *context.assistant_part_layout_passes =
                    context.assistant_part_layout_passes.wrapping_add(1);
            }
        }
        AssistantChild::Tool { call_id } => {
            let row = cached_tool_row(state, Some(*call_id), *call_id, None, context);
            layout.push(&row.lines, &row.regions);
        }
        AssistantChild::Attribution { resolved_model } => {
            layout.lines.push_lines(attribution_line(
                resolved_model,
                context.model_names,
                context.width,
                context.theme,
            ));
        }
        AssistantChild::Notice { text } => {
            layout.lines.push_lines(assistant_body_line(
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
            let row = cached_tool_row(
                state,
                None,
                BlockKey::CommittedTool {
                    turn_seq: *turn_seq,
                    content_index: *content_index,
                },
                Some(name.as_str()),
                context,
            );
            layout.push(&row.lines, &row.regions);
        }
        AssistantChild::MediaFile {
            turn_seq,
            content_index,
            file,
        } => {
            layout.push_layout(media_file_layout(*turn_seq, *content_index, file, context));
        }
    }
}

/// The reduced mode's row for an assistant block's work: `▸ Worked for
/// 1m 23s · 7 tool calls`, with a failed count in the warning colour, and
/// while the block is live the line saying what it does now. The whole row
/// opens and folds the work.
fn work_row_layout(
    item_id: u64,
    summary: &transcript_mode::WorkSummary,
    subtext: Option<&str>,
    open: bool,
    width: u16,
    theme: &Theme,
) -> ItemLayout {
    let chevron = if open { "▾" } else { "▸" };
    let mut spans = vec![Span::styled(
        format!("{chevron} {}", summary.label()),
        theme.muted_text(),
    )];
    if let Some(failed) = summary.failed_label() {
        spans.push(Span::styled(format!(" {failed}"), theme.warning()));
    }
    let label = Line::from(spans);
    let (mut lines, header_gutter) = if panel_margin(width) {
        let (rows, gutter) = margin_rows(label, width, theme, None);
        (rows, (gutter > 0).then_some(2))
    } else {
        (assistant_body_line(label, width, theme), None)
    };
    if let Some(subtext) = subtext {
        // Two columns under the label's text, one line, cut to fit.
        let gutter = if panel_margin(width) {
            3
        } else {
            usize::from(width >= 3) * 2
        };
        let room = usize::from(width).saturating_sub(gutter + 3);
        let text = crate::ui::app::truncate_with_ellipsis(subtext, room);
        let line = Line::from(Span::styled(format!("  {text}"), theme.muted()));
        let rows = if panel_margin(width) {
            margin_rows(line, width, theme, None).0
        } else {
            assistant_body_line(line, width, theme)
        };
        lines.extend(rows);
    }
    ItemLayout {
        regions: vec![BlockRegion {
            id: BlockId::Work(item_id),
            start_line: 0,
            end_line: lines.len(),
            header_lines: Some(lines.len()),
            header_gutter,
        }],
        lines,
        user_seq: None,
    }
}

/// `deferred` rows moved down by `start`.
fn shifted_deferred(
    deferred: &[DeferredRows],
    start: usize,
) -> impl Iterator<Item = DeferredRows> + '_ {
    deferred
        .iter()
        .map(move |(rows, highlight)| (rows.start + start..rows.end + start, highlight.clone()))
}

/// One tool row, from the tool-row cache unless the call, its expansion, or
/// (while it runs) the animation clock changed since it was rendered.
fn cached_tool_row(
    state: &SessionState,
    call_id: Option<cookie_agent_protocol::ToolCallId>,
    block_key: impl Into<BlockKey>,
    pending_name: Option<&str>,
    context: &mut TranscriptRenderContext<'_>,
) -> CachedToolLayout {
    let block_key = block_key.into();
    let block_id = match block_key {
        BlockKey::Call(call) => BlockId::Tool(call),
        BlockKey::CommittedTool {
            turn_seq,
            content_index,
        } => BlockId::CommittedTool {
            turn_seq,
            content_index,
        },
    };
    let tool = call_id.and_then(|call_id| state.tools.get(&call_id));
    let is_expanded = |id: BlockId| context.expanded.is_some_and(|blocks| blocks.contains(&id));
    let output_expanded = |section| {
        call_id.is_some_and(|call_id| is_expanded(BlockId::ToolOutput { call_id, section }))
    };
    let key = ToolLayoutKey {
        revision: tool.map(|tool| tool.revision),
        expanded: [
            is_expanded(block_id),
            output_expanded(ToolOutputSection::Detail),
            output_expanded(ToolOutputSection::Stdout),
            output_expanded(ToolOutputSection::Stderr),
        ],
        clock: if tool.is_some_and(|tool| tool.status == ToolStatus::Running) {
            context.clock_bucket
        } else {
            0
        },
    };
    if let Some(cached) = context
        .tool_cache
        .get(&block_id)
        .filter(|cached| cached.key == key)
    {
        return cached.clone();
    }
    let layout = tool_child_layout(state, call_id, block_key, pending_name, context);
    let cached = CachedToolLayout {
        key,
        lines: layout.lines.into(),
        regions: layout.regions,
    };
    context.tool_cache.insert(block_id, cached.clone());
    cached
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

/// A reply's rows, one chunk per markdown block. A block rendered from the
/// same settled allocation as last time (`previous`, by position) keeps its
/// wrapped rows, so a streaming reply re-wraps only its open tail. With
/// `defer`, code not highlighted yet renders plain, and the rows of each such
/// block come back with its handle.
pub(super) fn text_part_lines(
    markdown: &crate::markdown::MarkdownDocument,
    previous: &[WrappedBlock],
    width: u16,
    theme: &Theme,
    highlighter: &dyn Highlighter,
    defer: bool,
) -> (LineChunks, Vec<WrappedBlock>, Vec<DeferredRows>) {
    let blocks = crate::markdown::render_markdown_blocks_width(
        markdown,
        theme,
        highlighter,
        markdown_body_width(width),
        defer,
    );
    let body_line = |line: MarkdownLine| markdown_body_line(line, width, theme.assistant());
    let mut lines = LineChunks::default();
    let mut wrapped_blocks = Vec::with_capacity(blocks.len());
    let mut separator: Option<Chunk> = None;
    let mut deferred_rows = Vec::new();
    for (index, (block, deferred)) in blocks.into_iter().enumerate() {
        if !lines.is_empty() {
            let separator =
                separator.get_or_insert_with(|| body_line(MarkdownLine::default()).into());
            lines.push_chunk(Chunk::clone(separator));
        }
        let wrapped = previous
            .get(index)
            .filter(|(source, _)| std::sync::Arc::ptr_eq(source, &block))
            .map(|(_, wrapped)| Chunk::clone(wrapped))
            .unwrap_or_else(|| {
                block
                    .iter()
                    .cloned()
                    .flat_map(body_line)
                    .collect::<Vec<_>>()
                    .into()
            });
        if let Some(deferred) = deferred {
            deferred_rows.push((lines.len()..lines.len() + wrapped.len(), deferred));
        }
        lines.push_chunk(Chunk::clone(&wrapped));
        wrapped_blocks.push((block, wrapped));
    }
    if lines.is_empty() {
        lines.push_lines(body_line(MarkdownLine::default()));
    }
    (lines, wrapped_blocks, deferred_rows)
}

pub(super) fn assistant_child_layout(
    child: &AssistantChild,
    key: AssistantPartLayoutKey,
    width: u16,
    theme: &Theme,
    highlighter: &dyn Highlighter,
    thinking_rows: &mut ThinkingRows,
) -> ItemLayout {
    match child {
        AssistantChild::Text { markdown, .. } => ItemLayout {
            lines: text_part_lines(markdown, &[], width, theme, highlighter, false)
                .0
                .to_vec(),
            regions: Vec::new(),
            user_seq: None,
        },
        AssistantChild::Thinking { id, text, .. } => {
            let block_id = BlockId::Thinking(*id);
            // Collapsed, only the row count shows: count incrementally
            // instead of wrapping the whole trace on every streamed delta.
            let body = key
                .expanded
                .then(|| thinking_body_lines(text, width, theme).0);
            let hidden_lines = thinking_rows.count(text, width, theme).max(1);
            // While thinking streams the header animates an ellipsis; once
            // sealed it reads "thought", with the durable elapsed time when
            // the projection recorded one. Exactly one chevron per thinking
            // row: `▸` collapsed, `▾` expanded, after the thinking emoji.
            let status = if key.streaming {
                format!("thinking{}", ".".repeat(usize::from(key.dots)))
            } else {
                cookie_agent_client_state::reply::thought_label(key.duration)
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
                // Hover starts on the margin, as on a tool title, so the
                // highlight covers the padding too.
                (rows, (gutter > 0).then_some(2))
            } else {
                (assistant_body_line(label, width, theme), None)
            };
            let header_lines = lines.len();
            if let Some(body) = body {
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

/// The mid-block `├─ now using <Model Name> • <variant>` row, labelled
/// exactly like the assistant header's model part.
pub(super) fn attribution_line(
    resolved_model: &cookie_agent_protocol::ResolvedModelRef,
    model_names: &ModelDisplayNames,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let prefix = (width >= 4).then(|| vec![Span::styled("├─ ", theme.muted())]);
    repeated_prefixed_wrapped_line(
        prefix.unwrap_or_default(),
        Line::from(Span::styled(
            format!(
                "now using {}",
                model_names.selection_label(&resolved_model.selection)
            ),
            theme.muted(),
        )),
        width,
    )
}

/// The assistant block's closing footer:
/// `╰─ ⚡ 42.1 tps · 12.5K ctx · $0.0040` in
/// muted styling — visually subordinate to the body, closing the block's
/// gutter tree. Its text, and whether it shows at all, is the shared
/// [`cookie_agent_client_state::reply::assistant_footer`].
pub(super) fn assistant_footer_line(
    state: &SessionState,
    item_id: u64,
    width: u16,
    theme: &Theme,
) -> Option<Vec<Line<'static>>> {
    let footer = cookie_agent_client_state::reply::assistant_footer(state, item_id)?;
    let prefix = (width >= 4).then(|| vec![Span::styled("╰─ ", theme.muted())]);
    Some(repeated_prefixed_wrapped_line(
        prefix.unwrap_or_default(),
        Line::from(Span::styled(footer, theme.muted())),
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

/// The text width a markdown body gets once [`markdown_body_line`] spends two
/// columns on its gutter.
pub(super) fn markdown_body_width(width: u16) -> u16 {
    width.saturating_sub(u16::from(width >= 3) * 2)
}

/// One rendered markdown line behind a `│ ` gutter in `gutter_style`,
/// wrapped by its kind: prose wraps, list items hang, code and tables clip.
pub(super) fn markdown_body_line(
    line: MarkdownLine,
    width: u16,
    gutter_style: Style,
) -> Vec<Line<'static>> {
    let prefix = (width >= 3).then(|| vec![Span::styled("│ ", gutter_style)]);
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
    let text_rows = text
        .split('\n')
        .flat_map(|text| thinking_text_rows(text, width, theme))
        .collect::<Vec<_>>();
    let rows = text_rows.len();
    if panel_margin(width) {
        let background = theme.terminal_background();
        let blank = || padded_band_rows(Line::default(), width, theme, background);
        let mut lines = blank();
        lines.extend(text_rows);
        lines.extend(blank());
        lines.extend(assistant_body_line(Line::default(), width, theme));
        return (lines, rows);
    }
    (text_rows, rows)
}

/// The rows one source line of thinking wraps to.
fn thinking_text_rows(text: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let style = theme
        .muted_text()
        .add_modifier(ratatui::style::Modifier::ITALIC);
    let line = Line::styled(text.to_owned(), style);
    if panel_margin(width) {
        return padded_band_rows(line, width, theme, theme.terminal_background());
    }
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
    repeated_prefixed_wrapped_line(prefix, line, width)
}

/// The wrapped row count of a thinking trace's complete lines, carried
/// between renders of one part: thinking only grows, so a re-render counts
/// just the lines that arrived since.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ThinkingRows {
    /// Bytes of the trace through its last counted newline.
    counted: usize,
    /// The counted bytes' tail, to tell the same trace from another.
    tail: [u8; 16],
    rows: usize,
}

impl ThinkingRows {
    /// The rows all of `text` wraps to, remembering its complete lines.
    fn count(&mut self, text: &str, width: u16, theme: &Theme) -> usize {
        let extends = text.len() >= self.counted && Self::tail(text, self.counted) == self.tail;
        if !extends {
            *self = Self::default();
        }
        let rest = &text[self.counted..];
        let (complete, open) = match rest.rfind('\n') {
            Some(newline) => (&rest[..=newline], &rest[newline + 1..]),
            None => ("", rest),
        };
        for line in complete.split_terminator('\n') {
            self.rows += thinking_text_rows(line, width, theme).len();
        }
        self.counted += complete.len();
        self.tail = Self::tail(text, self.counted);
        self.rows + thinking_text_rows(open, width, theme).len()
    }

    fn tail(text: &str, end: usize) -> [u8; 16] {
        let bytes = &text.as_bytes()[end.saturating_sub(16)..end];
        let mut tail = [0; 16];
        tail[16 - bytes.len()..].copy_from_slice(bytes);
        tail
    }
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

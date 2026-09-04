//! Incremental Markdown parsing and direct ratatui span rendering.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    mem::size_of,
    sync::{Arc, Mutex},
};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use pulldown_cmark::{
    BrokenLink, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd,
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style as SyntectStyle, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

use crate::theme::{Theme, ThemeKey, ThemeKind};

const HIGHLIGHT_CACHE_BUDGET_BYTES: usize = 8 * 1024 * 1024;
const HIGHLIGHT_CACHE_ENTRY_OVERHEAD_BYTES: usize = 256;
const HIGHLIGHT_CACHE_ALLOCATION_OVERHEAD_BYTES: usize = 16;

#[derive(Clone, Debug, Default)]
pub struct MarkdownDocument {
    source: String,
    stable_prefix_len: usize,
    stable_blocks: Vec<MarkdownBlock>,
    tail_blocks: Vec<MarkdownBlock>,
    stable_reference_definitions: BTreeMap<String, ReferenceDefinition>,
    reference_definitions: BTreeMap<String, ReferenceDefinition>,
    stable_reference_dependencies: BTreeSet<String>,
    parse_passes: u64,
    parsed_bytes: u64,
    reference_reparses: u64,
}

#[derive(Debug)]
struct HighlightCache {
    entries: HashMap<HighlightCacheKey, HighlightCacheEntry>,
    budget_bytes: usize,
    used_bytes: usize,
    generation: u64,
    theme: Option<ThemeKey>,
}

impl Default for HighlightCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            budget_bytes: HIGHLIGHT_CACHE_BUDGET_BYTES,
            used_bytes: 0,
            generation: 0,
            theme: None,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct HighlightCacheKey {
    content_hash: u64,
    code_len: usize,
    language: String,
    theme: ThemeKey,
    syntect_theme: &'static str,
}

#[derive(Debug)]
struct HighlightCacheEntry {
    source: Arc<str>,
    lines: Vec<Line<'static>>,
    bytes: usize,
    last_used: u64,
}

impl HighlightCache {
    fn set_theme(&mut self, theme: ThemeKey) {
        if self.theme.is_some_and(|current| current != theme) {
            self.entries = HashMap::new();
            self.used_bytes = 0;
        }
        self.theme = Some(theme);
    }

    fn get(&mut self, key: &HighlightCacheKey, source: &str) -> Option<Vec<Line<'static>>> {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let entry = self.entries.get_mut(key)?;
        // The hash narrows the lookup, while this exact comparison prevents a
        // same-length collision from reusing spans for different source text.
        if entry.source.as_ref() != source {
            return None;
        }
        entry.last_used = generation;
        Some(entry.lines.clone())
    }

    fn insert(&mut self, key: HighlightCacheKey, source: &str, lines: Vec<Line<'static>>) {
        debug_assert!(self.used_bytes <= self.budget_bytes);
        self.generation = self.generation.wrapping_add(1);
        let bytes = highlight_entry_bytes(&key, source, &lines, lines.capacity());
        // A single entry cannot displace enough data to fit, so return it to
        // the renderer without caching it.
        if bytes > self.budget_bytes {
            return;
        }
        if let Some(replaced) = self.entries.remove(&key) {
            self.used_bytes = self.used_bytes.saturating_sub(replaced.bytes);
        }
        let available = self.budget_bytes - bytes;
        while self.used_bytes > available {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.used_bytes = self.used_bytes.saturating_sub(evicted.bytes);
            }
            debug_assert!(self.used_bytes <= self.budget_bytes);
        }
        debug_assert!(self.used_bytes <= available);
        let entry = HighlightCacheEntry {
            source: Arc::from(source),
            lines,
            bytes,
            last_used: self.generation,
        };
        self.used_bytes += bytes;
        self.entries.insert(key, entry);
        debug_assert!(self.used_bytes <= self.budget_bytes);
    }
}

fn highlight_entry_bytes(
    key: &HighlightCacheKey,
    source: &str,
    lines: &[Line<'static>],
    lines_capacity: usize,
) -> usize {
    size_of::<HighlightCacheKey>()
        + size_of::<HighlightCacheEntry>()
        + HIGHLIGHT_CACHE_ENTRY_OVERHEAD_BYTES
        + 3 * HIGHLIGHT_CACHE_ALLOCATION_OVERHEAD_BYTES
        + key.language.capacity()
        + source.len()
        + 2 * size_of::<usize>()
        + size_of::<Line<'static>>() * lines_capacity
        + lines
            .iter()
            .map(|line| {
                size_of::<Span<'static>>() * line.spans.capacity()
                    + usize::from(line.spans.capacity() > 0)
                        * HIGHLIGHT_CACHE_ALLOCATION_OVERHEAD_BYTES
                    + line
                        .spans
                        .iter()
                        .map(|span| match &span.content {
                            std::borrow::Cow::Borrowed(content) => content.len(),
                            std::borrow::Cow::Owned(content) => {
                                content.capacity() + HIGHLIGHT_CACHE_ALLOCATION_OVERHEAD_BYTES
                            }
                        })
                        .sum::<usize>()
            })
            .sum::<usize>()
}

#[derive(Clone, Debug)]
pub struct MarkdownBlock {
    kind: MarkdownBlockKind,
    events: Vec<Event<'static>>,
}

impl MarkdownBlock {
    pub const fn kind(&self) -> MarkdownBlockKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkdownBlockKind {
    Paragraph,
    Heading,
    Quote,
    Code,
    List,
    Table,
    Html,
    ThematicBreak,
    Other,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum MarkdownLineKind {
    #[default]
    Prose,
    ListItem {
        continuation_indent: usize,
    },
    Code,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MarkdownLine {
    pub(crate) line: Line<'static>,
    pub(crate) kind: MarkdownLineKind,
}

impl MarkdownDocument {
    pub fn new(source: String) -> Self {
        let mut document = Self {
            source,
            ..Self::default()
        };
        document.reparse_open_tail();
        document
    }

    pub fn append(&mut self, delta: &str) {
        self.source.push_str(delta);
        self.reparse_open_tail();
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    pub fn stable_prefix_len(&self) -> usize {
        self.stable_prefix_len
    }

    pub fn parse_passes(&self) -> u64 {
        self.parse_passes
    }

    pub fn parsed_bytes(&self) -> u64 {
        self.parsed_bytes
    }

    pub fn reference_reparses(&self) -> u64 {
        self.reference_reparses
    }

    pub fn blocks(&self) -> impl Iterator<Item = &MarkdownBlock> {
        self.stable_blocks.iter().chain(&self.tail_blocks)
    }

    fn reparse_open_tail(&mut self) {
        let tail = self.source[self.stable_prefix_len..].to_owned();
        let parsed = self.parse(&tail);
        let effective_definitions = self.effective_definitions(&parsed.definitions);
        let references_changed = self
            .stable_reference_dependencies
            .iter()
            .any(|label| self.reference_definitions.get(label) != effective_definitions.get(label));
        if references_changed {
            self.reference_reparses = self.reference_reparses.wrapping_add(1);
            self.stable_prefix_len = 0;
            self.stable_blocks.clear();
            self.tail_blocks.clear();
            self.stable_reference_definitions.clear();
            self.reference_definitions.clear();
            self.stable_reference_dependencies.clear();
            let source = self.source.clone();
            let parsed = self.parse(&source);
            self.reference_definitions = self.effective_definitions(&parsed.definitions);
            self.apply_parsed(parsed);
            return;
        }
        self.reference_definitions = effective_definitions;
        self.apply_parsed(parsed);
    }

    fn parse(&mut self, source: &str) -> ParsedDocument {
        self.parse_passes = self.parse_passes.wrapping_add(1);
        self.parsed_bytes = self.parsed_bytes.wrapping_add(source.len() as u64);
        parse_document(source)
    }

    fn effective_definitions(
        &self,
        tail_definitions: &[ParsedReferenceDefinition],
    ) -> BTreeMap<String, ReferenceDefinition> {
        let mut definitions = self.stable_reference_definitions.clone();
        for definition in tail_definitions {
            definitions
                .entry(definition.label.clone())
                .or_insert_with(|| definition.definition.clone());
        }
        definitions
    }

    fn apply_parsed(&mut self, parsed: ParsedDocument) {
        let split = parsed.blocks.last().map_or(0, |block| block.range.start);
        self.stable_reference_dependencies.extend(
            parsed
                .blocks
                .iter()
                .filter(|block| block.range.end <= split)
                .flat_map(|block| block.reference_dependencies.iter().cloned()),
        );
        self.stable_blocks.extend(
            parsed
                .blocks
                .iter()
                .filter(|block| block.range.end <= split)
                .map(|block| block.block.clone()),
        );
        self.tail_blocks = parsed
            .blocks
            .into_iter()
            .filter(|block| block.range.start >= split)
            .map(|block| block.block)
            .collect();
        for definition in parsed.definitions {
            if definition.span.end <= split {
                self.stable_reference_definitions
                    .entry(definition.label)
                    .or_insert(definition.definition);
            }
        }
        self.stable_prefix_len += split;
    }
}

fn markdown_options() -> Options {
    Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
}

struct ParsedBlock {
    block: MarkdownBlock,
    range: std::ops::Range<usize>,
    reference_dependencies: BTreeSet<String>,
}

struct ParsedDocument {
    blocks: Vec<ParsedBlock>,
    definitions: Vec<ParsedReferenceDefinition>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReferenceDefinition {
    destination: String,
    title: Option<String>,
}

struct ParsedReferenceDefinition {
    label: String,
    definition: ReferenceDefinition,
    span: std::ops::Range<usize>,
}

fn parse_document(source: &str) -> ParsedDocument {
    let (definitions, events, broken_references) = {
        let mut broken_references = Vec::new();
        let mut broken_link_callback = |broken: BrokenLink<'_>| {
            broken_references.push((
                broken.span,
                normalize_reference_label(broken.reference.as_ref()),
            ));
            None
        };
        let parser = Parser::new_with_broken_link_callback(
            source,
            markdown_options(),
            Some(&mut broken_link_callback),
        );
        let definitions = parser
            .reference_definitions()
            .iter()
            .map(|(label, definition)| ParsedReferenceDefinition {
                label: normalize_reference_label(label),
                definition: ReferenceDefinition {
                    destination: definition.dest.to_string(),
                    title: definition.title.as_ref().map(ToString::to_string),
                },
                span: definition.span.clone(),
            })
            .collect::<Vec<_>>();
        let events = parser.into_offset_iter().collect::<Vec<_>>();
        (definitions, events, broken_references)
    };
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    let mut current_kind = MarkdownBlockKind::Other;
    let mut current_start = 0;
    let mut depth = 0usize;

    for (event, range) in events {
        if depth == 0 && current.is_empty() {
            current_start = range.start;
            current_kind = block_kind(&event);
        }
        let current_end = range.end;
        match &event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        current.push(event.into_static());
        if depth == 0 {
            blocks.push(ParsedBlock {
                block: MarkdownBlock {
                    kind: current_kind,
                    events: std::mem::take(&mut current),
                },
                range: current_start..current_end,
                reference_dependencies: BTreeSet::new(),
            });
        }
    }
    for block in &mut blocks {
        for event in &block.block.events {
            let Event::Start(Tag::Link { link_type, id, .. } | Tag::Image { link_type, id, .. }) =
                event
            else {
                continue;
            };
            if matches!(
                link_type,
                LinkType::Reference
                    | LinkType::ReferenceUnknown
                    | LinkType::Collapsed
                    | LinkType::CollapsedUnknown
                    | LinkType::Shortcut
                    | LinkType::ShortcutUnknown
            ) {
                block
                    .reference_dependencies
                    .insert(normalize_reference_label(id.as_ref()));
            }
        }
        block.reference_dependencies.extend(
            broken_references
                .iter()
                .filter(|(span, _)| span.start >= block.range.start && span.end <= block.range.end)
                .map(|(_, label)| label.clone()),
        );
    }
    ParsedDocument {
        blocks,
        definitions,
    }
}

fn normalize_reference_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn block_kind(event: &Event<'_>) -> MarkdownBlockKind {
    match event {
        Event::Start(Tag::Paragraph) => MarkdownBlockKind::Paragraph,
        Event::Start(Tag::Heading { .. }) => MarkdownBlockKind::Heading,
        Event::Start(Tag::BlockQuote(_)) => MarkdownBlockKind::Quote,
        Event::Start(Tag::CodeBlock(_)) => MarkdownBlockKind::Code,
        Event::Start(Tag::List(_)) => MarkdownBlockKind::List,
        Event::Start(Tag::Table(_)) => MarkdownBlockKind::Table,
        Event::Start(Tag::HtmlBlock) | Event::Html(_) => MarkdownBlockKind::Html,
        Event::Rule => MarkdownBlockKind::ThematicBreak,
        _ => MarkdownBlockKind::Other,
    }
}

pub trait Highlighter: Send + Sync + 'static {
    fn highlight(&self, language: &str, code: &str, theme: &Theme) -> Vec<Line<'static>>;

    fn highlight_stable(&self, language: &str, code: &str, theme: &Theme) -> Vec<Line<'static>> {
        self.highlight(language, code, theme)
    }
}

#[derive(Default)]
pub struct PlainHighlighter;

impl Highlighter for PlainHighlighter {
    fn highlight(&self, _language: &str, code: &str, _theme: &Theme) -> Vec<Line<'static>> {
        plain_code_lines(code)
    }
}

pub struct SyntectHighlighter {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
    plain: PlainHighlighter,
    cache: Mutex<HighlightCache>,
    #[cfg(test)]
    highlight_calls: AtomicUsize,
}

impl Default for SyntectHighlighter {
    fn default() -> Self {
        Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            themes: ThemeSet::load_defaults(),
            plain: PlainHighlighter,
            cache: Mutex::new(HighlightCache::default()),
            #[cfg(test)]
            highlight_calls: AtomicUsize::new(0),
        }
    }
}

impl Highlighter for SyntectHighlighter {
    fn highlight(&self, language: &str, code: &str, theme: &Theme) -> Vec<Line<'static>> {
        #[cfg(test)]
        self.highlight_calls.fetch_add(1, Ordering::Relaxed);
        let language = normalized_language(language);
        let Some(syntax) = self
            .syntaxes
            .find_syntax_by_token(&language)
            .or_else(|| self.syntaxes.find_syntax_by_extension(&language))
        else {
            return self.plain.highlight(&language, code, theme);
        };
        // Match syntax highlighting to the curated surface; colors quantize
        // away in mono.
        let theme_name = syntect_theme_name(theme);
        let Some(syntax_theme) = self.themes.themes.get(theme_name) else {
            return self.plain.highlight(&language, code, theme);
        };
        let mut highlighter = HighlightLines::new(syntax, syntax_theme);
        let mut lines = Vec::new();
        for source_line in LinesWithEndings::from(code) {
            let Ok(ranges) = highlighter.highlight_line(source_line, &self.syntaxes) else {
                return self.plain.highlight(&language, code, theme);
            };
            let mut spans = Vec::new();
            for (syntect_style, content) in ranges {
                let content = content.strip_suffix('\n').unwrap_or(content);
                let content = content.strip_suffix('\r').unwrap_or(content);
                if content.is_empty() {
                    continue;
                }
                spans.push(Span::styled(
                    content.to_owned(),
                    terminal_style_from_syntect(syntect_style, theme),
                ));
            }
            lines.push(Line::from(spans));
        }
        if lines.is_empty() {
            lines.push(Line::default());
        }
        lines
    }

    fn highlight_stable(&self, language: &str, code: &str, theme: &Theme) -> Vec<Line<'static>> {
        let mut hasher = DefaultHasher::new();
        language.hash(&mut hasher);
        code.hash(&mut hasher);
        let key = HighlightCacheKey {
            content_hash: hasher.finish(),
            code_len: code.len(),
            language: language.to_owned(),
            theme: theme.key(),
            syntect_theme: syntect_theme_name(theme),
        };
        {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.set_theme(theme.key());
            if let Some(lines) = cache.get(&key, code) {
                return lines;
            }
        }

        let lines = self.highlight(language, code, theme);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.set_theme(theme.key());
        cache.insert(key, code, lines.clone());
        lines
    }
}

#[cfg(test)]
impl SyntectHighlighter {
    fn highlight_calls(&self) -> usize {
        self.highlight_calls.load(Ordering::Relaxed)
    }

    fn set_cache_budget(&self, budget_bytes: usize) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.entries = HashMap::new();
        cache.used_bytes = 0;
        cache.budget_bytes = budget_bytes;
    }
}

fn syntect_theme_name(theme: &Theme) -> &'static str {
    match theme.key().kind {
        ThemeKind::Dark | ThemeKind::HighContrast => "base16-eighties.dark",
        ThemeKind::Default | ThemeKind::Mono => "InspiredGitHub",
    }
}

fn terminal_style_from_syntect(syntect_style: SyntectStyle, theme: &Theme) -> Style {
    let mut style = Style::default();
    if let Some(color) = theme.quantize_rgb(
        syntect_style.foreground.r,
        syntect_style.foreground.g,
        syntect_style.foreground.b,
    ) {
        style = style.fg(color);
    }
    let mut modifiers = Modifier::empty();
    if syntect_style.font_style.contains(FontStyle::BOLD) {
        modifiers |= Modifier::BOLD;
    }
    if syntect_style.font_style.contains(FontStyle::ITALIC) {
        modifiers |= Modifier::ITALIC;
    }
    if syntect_style.font_style.contains(FontStyle::UNDERLINE) {
        modifiers |= Modifier::UNDERLINED;
    }
    style.add_modifier(modifiers)
}

fn plain_code_lines(code: &str) -> Vec<Line<'static>> {
    let mut lines = code
        .lines()
        .map(|line| Line::from(line.strip_suffix('\r').unwrap_or(line).to_owned()))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

// --- Table layout -----------------------------------------------------------
//
// Terminal-native tables: accessible box borders, a distinct header style
// (bold in every theme, colored where available), per-column alignment, and
// deterministic width allocation. Narrow terminals fall back to a stacked
// `Header: value` representation.

use pulldown_cmark::Alignment;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr;

/// Display width of one rendered cell (widest span sequence).
fn cell_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Deterministic column widths for the available text width. Natural widths
// shrink proportionally (columns at least 3 wide); the total never exceeds
/// `available` when the table fits, otherwise a stacked fallback is used.
fn table_columns(natural: &[usize], available: usize) -> Option<Vec<usize>> {
    let columns = natural.len();
    if columns == 0 {
        return None;
    }
    let borders = columns * 3 + 1; // "│ " per column + closing "│"
    let budget = available.saturating_sub(borders);
    let minimum = columns * 3;
    if budget < minimum.max(8) {
        return None;
    }
    let budget = budget.max(minimum);
    let total: usize = natural.iter().sum();
    if total <= budget {
        return Some(natural.to_vec());
    }
    // Shrink the widest columns first, never below 3.
    let mut widths: Vec<usize> = natural.to_vec();
    let mut overflow = total - budget;
    while overflow > 0 {
        let Some((index, widest)) = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, width)| *width)
            .map(|(index, width)| (index, *width))
        else {
            break;
        };
        if widest <= 3 {
            break;
        }
        widths[index] -= 1;
        overflow -= 1;
    }
    Some(widths)
}

/// Wrap cell spans to a column width on grapheme boundaries, preserving
/// styles and alignment padding per wrapped row.
fn wrap_cell(
    spans: &[Span<'static>],
    width: usize,
    alignment: Alignment,
) -> Vec<Vec<Span<'static>>> {
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut column = 0usize;
    for span in spans {
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
            if column > 0 && column + grapheme_width > width {
                rows.push(Vec::new());
                column = 0;
            }
            if let Some(last) = rows
                .last_mut()
                .and_then(|row: &mut Vec<Span<'static>>| row.last_mut())
                && last.style == span.style
            {
                last.content.to_mut().push_str(grapheme);
            } else {
                rows.last_mut()
                    .expect("one row")
                    .push(Span::styled(grapheme.to_owned(), span.style));
            }
            column += grapheme_width;
        }
    }
    // Alignment is applied at line assembly; here rows are raw cell content.
    let _ = alignment;
    rows
}

fn pad_cell(
    mut spans: Vec<Span<'static>>,
    width: usize,
    alignment: Alignment,
) -> Vec<Span<'static>> {
    let used = cell_width(&spans);
    if used >= width {
        return spans;
    }
    let padding = width - used;
    let (left, right) = match alignment {
        Alignment::Left => (0, padding),
        Alignment::Right => (padding, 0),
        Alignment::Center => (padding / 2, padding - padding / 2),
        Alignment::None => (0, padding),
    };
    let mut padded = Vec::new();
    if left > 0 {
        padded.push(Span::raw(" ".repeat(left)));
    }
    padded.append(&mut spans);
    if right > 0 {
        padded.push(Span::raw(" ".repeat(right)));
    }
    padded
}

/// Sanitize cell text: control characters (except those already handled by
/// the parser) render as the replacement character so terminal control
/// sequences can never be injected through table content.
fn sanitize_cell(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|span| {
            let content = span
                .content
                .chars()
                .map(|character| {
                    if character.is_control() && character != '\t' {
                        '\u{FFFD}'
                    } else {
                        character
                    }
                })
                .collect::<String>();
            Span::styled(content, span.style)
        })
        .collect()
}

fn border_row(
    left: &str,
    junction: &str,
    right: &str,
    widths: &[usize],
    quote_prefix: &str,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(quote_prefix.to_owned(), theme.quote()),
        Span::styled(left.to_owned(), theme.code_border()),
    ];
    for (index, width) in widths.iter().enumerate() {
        spans.push(Span::styled("─".repeat(width + 2), theme.code_border()));
        spans.push(Span::styled(
            if index + 1 == widths.len() {
                right.to_owned()
            } else {
                junction.to_owned()
            },
            theme.code_border(),
        ));
    }
    Line::from(spans)
}

fn cell_row(
    cells: &[Vec<Span<'static>>],
    widths: &[usize],
    alignments: &[Alignment],
    quote_prefix: &str,
    theme: &Theme,
    style: Style,
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            wrap_cell(
                cell,
                widths.get(index).copied().unwrap_or(3),
                alignments.get(index).copied().unwrap_or(Alignment::None),
            )
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let mut lines = Vec::new();
    for row in 0..height {
        let mut spans = vec![
            Span::styled(quote_prefix.to_owned(), theme.quote()),
            Span::styled("│ ".to_owned(), theme.code_border()),
        ];
        for (index, width) in widths.iter().enumerate() {
            let cell = wrapped
                .get(index)
                .and_then(|rows| rows.get(row))
                .cloned()
                .unwrap_or_default();
            let mut padded = pad_cell(
                sanitize_cell(cell),
                *width,
                alignments.get(index).copied().unwrap_or(Alignment::None),
            );
            for span in &mut padded {
                span.style = span.style.patch(style);
            }
            spans.append(&mut padded);
            spans.push(Span::styled(
                if index + 1 == widths.len() {
                    " │".to_owned()
                } else {
                    " │ ".to_owned()
                },
                theme.code_border(),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn render_table(
    alignments: &[Alignment],
    header: &[Vec<Span<'static>>],
    rows: &[Vec<Vec<Span<'static>>>],
    quote_prefix: &str,
    available: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let columns = alignments.len().max(header.len());
    let mut natural = vec![3usize; columns];
    for (index, width) in natural.iter_mut().enumerate() {
        if let Some(cell) = header.get(index) {
            *width = (*width).max(cell_width(cell));
        }
        for row in rows {
            if let Some(cell) = row.get(index) {
                *width = (*width).max(cell_width(cell));
            }
        }
        *width = (*width).min(48);
    }
    let Some(widths) = table_columns(&natural, available) else {
        // Stacked fallback: one "Header: value" row per body cell.
        let mut lines = Vec::new();
        for row in rows {
            for (index, cell) in row.iter().enumerate() {
                let header_text = header
                    .get(index)
                    .map(|spans| {
                        spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>()
                    })
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| format!("column {}", index + 1));
                let mut spans = vec![
                    Span::styled(quote_prefix.to_owned(), theme.quote()),
                    Span::styled("· ".to_owned(), theme.code_border()),
                    Span::styled(format!("{header_text}: "), theme.heading()),
                ];
                spans.extend(sanitize_cell(cell.clone()));
                lines.push(Line::from(spans));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(quote_prefix.to_owned(), theme.quote()),
                Span::styled("· (empty table)".to_owned(), theme.muted()),
            ]));
        }
        return lines;
    };
    let mut lines = vec![border_row("┌", "┬", "┐", &widths, quote_prefix, theme)];
    lines.extend(cell_row(
        header,
        &widths,
        alignments,
        quote_prefix,
        theme,
        theme.heading(),
    ));
    lines.push(border_row("├", "┼", "┤", &widths, quote_prefix, theme));
    for row in rows {
        lines.extend(cell_row(
            row,
            &widths,
            alignments,
            quote_prefix,
            theme,
            theme.body(),
        ));
    }
    lines.push(border_row("└", "┴", "┘", &widths, quote_prefix, theme));
    lines
}

pub fn render_markdown(
    document: &MarkdownDocument,
    theme: &Theme,
    highlighter: &dyn Highlighter,
) -> Vec<Line<'static>> {
    render_markdown_width(document, theme, highlighter, u16::MAX)
}

/// Render with a known text width so tables can allocate columns and fall
/// back to a stacked form on narrow terminals. `u16::MAX` means unbounded.
pub fn render_markdown_width(
    document: &MarkdownDocument,
    theme: &Theme,
    highlighter: &dyn Highlighter,
    width: u16,
) -> Vec<Line<'static>> {
    render_markdown_lines_width(document, theme, highlighter, width)
        .into_iter()
        .map(|line| line.line)
        .collect()
}

pub(crate) fn render_markdown_lines_width(
    document: &MarkdownDocument,
    theme: &Theme,
    highlighter: &dyn Highlighter,
    width: u16,
) -> Vec<MarkdownLine> {
    let mut renderer = MarkdownRenderer::new(theme, highlighter);
    renderer.width = usize::from(width);
    for block in &document.stable_blocks {
        renderer.begin_block(true);
        for event in &block.events {
            renderer.event(event);
        }
    }
    for block in &document.tail_blocks {
        renderer.begin_block(false);
        for event in &block.events {
            renderer.event(event);
        }
    }
    renderer.finish()
}

pub(crate) fn normalized_language(language: &str) -> String {
    let language = language
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(['{', '}'])
        .trim_start_matches('.')
        .to_ascii_lowercase();
    match language.as_str() {
        "rs" => "rust".into(),
        "sh" | "shell" | "zsh" => "bash".into(),
        "js" | "jsx" => "javascript".into(),
        "ts" | "tsx" => "typescript".into(),
        "py" => "python".into(),
        "rb" => "ruby".into(),
        "yml" => "yaml".into(),
        "md" => "markdown".into(),
        "c++" => "cpp".into(),
        "c#" => "cs".into(),
        _ => language,
    }
}

struct MarkdownRenderer<'a> {
    theme: &'a Theme,
    highlighter: &'a dyn Highlighter,
    width: usize,
    lines: Vec<MarkdownLine>,
    current: Vec<Span<'static>>,
    current_kind: MarkdownLineKind,
    styles: Vec<Style>,
    lists: Vec<ListState>,
    quote_depth: usize,
    code: Option<CodeCapture>,
    links: Vec<String>,
    image_depth: usize,
    table: Option<TableCapture<'a>>,
    stable_block: bool,
}

/// In-progress table capture. Cell inline markup uses a nested renderer so
/// emphasis/links/inline code keep their semantic styles inside cells.
struct TableCapture<'a> {
    alignments: Vec<pulldown_cmark::Alignment>,
    header: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    current_row: Vec<Vec<Span<'static>>>,
    cell: Option<Box<MarkdownRenderer<'a>>>,
}

struct ListState {
    next: Option<u64>,
    item_content_indent: Option<usize>,
}

struct CodeCapture {
    language: String,
    code: String,
    stable: bool,
}

impl<'a> MarkdownRenderer<'a> {
    fn new(theme: &'a Theme, highlighter: &'a dyn Highlighter) -> Self {
        Self {
            theme,
            highlighter,
            width: usize::MAX,
            lines: Vec::new(),
            current: Vec::new(),
            current_kind: MarkdownLineKind::Prose,
            styles: vec![theme.body()],
            lists: Vec::new(),
            quote_depth: 0,
            code: None,
            links: Vec::new(),
            image_depth: 0,
            table: None,
            stable_block: false,
        }
    }

    fn begin_block(&mut self, stable_block: bool) {
        self.stable_block = stable_block;
    }

    fn event(&mut self, event: &Event<'static>) {
        if let Some(code) = &mut self.code {
            match event {
                Event::Text(text) => code.code.push_str(text),
                Event::End(TagEnd::CodeBlock) => self.finish_code(),
                _ => {}
            }
            return;
        }
        // Inside a table, structure events drive the capture; everything else
        // is forwarded into the active cell renderer.
        if self.table.is_some()
            && !matches!(
                event,
                Event::Start(Tag::Table(_) | Tag::TableHead | Tag::TableRow | Tag::TableCell)
                    | Event::End(
                        TagEnd::Table | TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell
                    )
            )
        {
            if let Some(cell) = self
                .table
                .as_mut()
                .and_then(|table| table.cell.as_deref_mut())
            {
                cell.event(event);
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(*tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => self.text(text),
            Event::Code(code) => self.span(
                format!("`{code}`"),
                self.styles
                    .last()
                    .copied()
                    .unwrap_or_default()
                    .patch(self.theme.inline_code()),
            ),
            Event::InlineMath(math) => self.span(format!("${math}$"), self.theme.inline_code()),
            Event::DisplayMath(math) => {
                self.flush();
                self.span(format!("$$ {math} $$"), self.theme.inline_code());
                self.flush();
            }
            Event::FootnoteReference(reference) => {
                self.span(format!("[^{reference}]"), self.theme.link())
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => {
                self.flush();
                self.start_line_prefix();
            }
            Event::Rule => {
                self.flush();
                self.start_line_prefix();
                let prefix_width = cell_width(&self.current);
                let rule_width = if self.width == usize::from(u16::MAX) {
                    16
                } else {
                    self.width.saturating_sub(prefix_width)
                };
                self.span("─".repeat(rule_width), self.theme.muted());
                self.flush();
            }
            Event::TaskListMarker(done) => self.span(
                if *done { "[x] " } else { "[ ] " }.into(),
                self.theme.tool(),
            ),
        }
    }

    fn start(&mut self, tag: &Tag<'static>) {
        match tag {
            Tag::Paragraph => self.start_line_prefix(),
            Tag::Heading { level, .. } => {
                self.flush();
                self.start_line_prefix();
                let style = match level {
                    HeadingLevel::H1 => self
                        .theme
                        .heading()
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    HeadingLevel::H2 => self.theme.heading().remove_modifier(Modifier::UNDERLINED),
                    HeadingLevel::H3 | HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => {
                        self.theme
                            .heading()
                            .remove_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                            .add_modifier(Modifier::ITALIC)
                    }
                };
                self.styles.push(style);
            }
            Tag::BlockQuote(kind) => {
                self.flush();
                self.quote_depth += 1;
                self.start_line_prefix();
                if let Some(kind) = kind {
                    self.span(format!("{kind:?}: ").to_uppercase(), self.theme.quote());
                }
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.code = Some(CodeCapture {
                    language: match kind {
                        CodeBlockKind::Indented => String::new(),
                        CodeBlockKind::Fenced(language) => language.to_string(),
                    },
                    code: String::new(),
                    stable: self.stable_block,
                });
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(ListState {
                    next: *start,
                    item_content_indent: None,
                });
            }
            Tag::Item => {
                self.flush();
                self.start_quote_prefix();
                let depth = self.lists.len().saturating_sub(1);
                self.span("  ".repeat(depth), self.theme.body());
                let marker = self.lists.last_mut().map_or_else(
                    || "• ".to_owned(),
                    |list| match &mut list.next {
                        Some(next) => {
                            let marker = format!("{next}. ");
                            *next += 1;
                            marker
                        }
                        None => "• ".to_owned(),
                    },
                );
                self.span(marker, self.theme.tool());
                let continuation_indent = cell_width(&self.current);
                self.current_kind = MarkdownLineKind::ListItem {
                    continuation_indent,
                };
                if let Some(list) = self.lists.last_mut() {
                    list.item_content_indent = Some(continuation_indent);
                }
            }
            Tag::Table(alignments) => {
                self.flush();
                self.table = Some(TableCapture {
                    alignments: alignments.clone(),
                    header: Vec::new(),
                    rows: Vec::new(),
                    current_row: Vec::new(),
                    cell: None,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.current_row = Vec::new();
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cell = Some(Box::new(MarkdownRenderer::new(
                        self.theme,
                        self.highlighter,
                    )));
                }
            }
            Tag::Emphasis => self.push_modifier(Modifier::ITALIC),
            Tag::Strong => self.push_modifier(Modifier::BOLD),
            Tag::Strikethrough => self.push_modifier(Modifier::CROSSED_OUT),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                self.styles.push(self.theme.link());
            }
            Tag::Image { dest_url, .. } => {
                self.image_depth += 1;
                self.links.push(dest_url.to_string());
                self.span("[image: ".into(), self.theme.link());
            }
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Item => {
                self.flush();
                if let Some(list) = self.lists.last_mut() {
                    list.item_content_indent = None;
                }
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Table => self.finish_table(),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.header = std::mem::take(&mut table.current_row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(std::mem::take(&mut table.current_row));
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table
                    && let Some(cell) = table.cell.take()
                {
                    table.current_row.push(
                        cell.finish()
                            .into_iter()
                            .next()
                            .map(|line| line.line.spans)
                            .unwrap_or_default(),
                    );
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some(destination) = self.links.pop() {
                    self.span(format!(" <{destination}>"), self.theme.link());
                }
            }
            TagEnd::Image => {
                if let Some(destination) = self.links.pop() {
                    self.span(format!(" -> {destination}]"), self.theme.link());
                }
                self.image_depth = self.image_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    fn push_modifier(&mut self, modifier: Modifier) {
        let style = self
            .styles
            .last()
            .copied()
            .unwrap_or_default()
            .add_modifier(modifier);
        self.styles.push(style);
    }

    fn text(&mut self, text: &str) {
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.flush();
                self.start_line_prefix();
            }
            if !part.is_empty() {
                self.span(
                    part.to_owned(),
                    self.styles.last().copied().unwrap_or_default(),
                );
            }
        }
    }

    fn start_line_prefix(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        self.start_quote_prefix();
        if let Some(continuation_indent) = self
            .lists
            .iter()
            .rev()
            .find_map(|list| list.item_content_indent)
        {
            let prefix_width = cell_width(&self.current);
            self.span(
                " ".repeat(continuation_indent.saturating_sub(prefix_width)),
                self.theme.body(),
            );
            self.current_kind = MarkdownLineKind::ListItem {
                continuation_indent,
            };
        }
    }

    fn start_quote_prefix(&mut self) {
        if self.current.is_empty() && self.quote_depth > 0 {
            self.span("> ".repeat(self.quote_depth), self.theme.quote());
        }
    }

    fn span(&mut self, content: String, style: Style) {
        if let Some(last) = self.current.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(&content);
        } else {
            self.current.push(Span::styled(content, style));
        }
    }

    fn flush(&mut self) {
        if !self.current.is_empty() {
            self.lines.push(MarkdownLine {
                line: Line::from(std::mem::take(&mut self.current)),
                kind: self.current_kind,
            });
            self.current_kind = MarkdownLineKind::Prose;
        }
    }

    fn finish_code(&mut self) {
        let Some(code) = self.code.take() else {
            return;
        };
        let label = code.language.split_whitespace().next().unwrap_or_default();
        let quote_prefix = "> ".repeat(self.quote_depth);
        // The whole block (borders included) sits on one subtle parchment
        // band; content lines pad to a common width so the band is solid.
        let band = self
            .theme
            .code_background()
            .map(|background| Style::default().bg(background));
        let patch = |style: Style| band.map_or(style, |band| style.patch(band));
        let border = patch(self.theme.code_border());
        self.push_code_line(Line::from(vec![
            Span::styled(quote_prefix.clone(), self.theme.quote()),
            Span::styled(
                if label.is_empty() {
                    "┌─ code".to_owned()
                } else {
                    format!("┌─ code: {label}")
                },
                border,
            ),
        ]));
        // Highlighting is cached before code-band styling and width-dependent
        // wrapping, so stable entries survive terminal resizes.
        let mut highlighted = if code.stable {
            self.highlighter
                .highlight_stable(label, &code.code, self.theme)
        } else {
            self.highlighter.highlight(label, &code.code, self.theme)
        };
        if let Some(band) = band {
            for line in &mut highlighted {
                for span in &mut line.spans {
                    span.style = span.style.patch(band);
                }
            }
        }
        let quote_width = UnicodeWidthStr::width(quote_prefix.as_str());
        let first_width = self.width.saturating_sub(quote_width + 2).max(1);
        let continuation_width = self.width.saturating_sub(quote_width + 1).max(1);
        let mut body = Vec::new();
        for line in highlighted {
            let line_style = line.style;
            for (index, content) in wrap_code_spans(line.spans, first_width, continuation_width)
                .into_iter()
                .enumerate()
            {
                let mut spans = vec![
                    Span::styled(quote_prefix.clone(), self.theme.quote()),
                    Span::styled(if index == 0 { "│ " } else { "│" }, border),
                ];
                spans.extend(content);
                body.push(Line::from(spans).style(line_style));
            }
        }
        if let Some(band) = band {
            let band_width = body.iter().map(Line::width).max().unwrap_or(0);
            for line in &mut body {
                let padding = band_width.saturating_sub(line.width());
                if padding > 0 {
                    line.spans.push(Span::styled(" ".repeat(padding), band));
                }
            }
        }
        for line in body {
            self.push_code_line(line);
        }
        self.push_code_line(Line::from(vec![
            Span::styled(quote_prefix, self.theme.quote()),
            Span::styled("└─".to_owned(), border),
        ]));
    }

    fn push_code_line(&mut self, line: Line<'static>) {
        self.lines.push(MarkdownLine {
            line,
            kind: MarkdownLineKind::Code,
        });
    }

    /// Render the captured table as a terminal-native grid. Column widths
    /// come from display widths of grapheme-safe cell text; alignment markers
    /// are honored per column. Narrow tables fall back to a stacked
    /// `Header: value` representation rather than unusable slivers.
    fn finish_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let quote_prefix = "> ".repeat(self.quote_depth);
        let prefix_width = UnicodeWidthStr::width(quote_prefix.as_str());
        let available = self.width.saturating_sub(prefix_width).max(1);
        for line in render_table(
            &table.alignments,
            &table.header,
            &table.rows,
            &quote_prefix,
            available,
            self.theme,
        ) {
            self.lines.push(MarkdownLine {
                line,
                kind: MarkdownLineKind::Prose,
            });
        }
    }

    fn finish(mut self) -> Vec<MarkdownLine> {
        self.flush();
        if self.lines.is_empty() {
            self.lines.push(MarkdownLine::default());
        }
        self.lines
    }
}

fn wrap_code_spans(
    spans: Vec<Span<'static>>,
    first_width: usize,
    continuation_width: usize,
) -> Vec<Vec<Span<'static>>> {
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0;
    let mut available = first_width;

    for span in spans {
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if current_width > 0 && current_width + grapheme_width > available {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                available = continuation_width;
            }
            if let Some(last) = current.last_mut()
                && last.style == span.style
            {
                last.content.to_mut().push_str(grapheme);
            } else {
                current.push(Span::styled(grapheme.to_owned(), span.style));
            }
            current_width += grapheme_width;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ratatui::style::Modifier;
    use syntect::{
        dumps::{
            dump_binary, dump_to_file, dump_to_uncompressed_file, from_binary, from_dump_file,
            from_uncompressed_data, from_uncompressed_dump_file,
        },
        highlighting::{
            Color as SyntectColor, FontStyle, Highlighter as SyntectThemeHighlighter,
            ScopeSelectors, StyleModifier, Theme as SyntectTheme, ThemeItem, ThemeSet,
            ThemeSettings,
        },
        parsing::{Scope, SyntaxSet},
    };

    use super::{
        Highlighter, MarkdownBlockKind, MarkdownDocument, PlainHighlighter, SyntectHighlighter,
        render_markdown, terminal_style_from_syntect,
    };
    use crate::theme::{ColorLevel, Theme, ThemeKind};
    use unicode_width::UnicodeWidthStr;

    fn strings(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn streamed_markdown_keeps_a_stable_prefix_and_only_reparses_the_open_tail() {
        let mut document = MarkdownDocument::new("first paragraph\n\nsecond".into());
        assert!(document.stable_prefix_len() >= "first paragraph\n\n".len());
        let parsed_before = document.parsed_bytes();
        document.append(" paragraph");
        assert_eq!(document.parse_passes(), 2);
        assert!(document.parsed_bytes() - parsed_before < document.as_str().len() as u64);
    }

    #[test]
    fn streamed_reference_links_and_images_match_full_commonmark_after_every_delta() {
        let fixtures: &[&[&str]] = &[
            &[
                "[forward][target]\n\nopen",
                "\n\n[target]: https://exa",
                "mple.test/path",
            ],
            &[
                "![diagram][asset]\n\nopen",
                "\n\n[asset]: https://example.test/diagram.png \"Diagram\"",
            ],
            &[
                "[docs][] and [guide]\n\nopen",
                "\n\n[docs]: https://example.test/docs\n[guide]: https://example.test/guide",
            ],
        ];
        for chunks in fixtures {
            let mut source = String::new();
            let mut streamed = MarkdownDocument::new(String::new());
            for chunk in *chunks {
                source.push_str(chunk);
                streamed.append(chunk);
                let full = MarkdownDocument::new(source.clone());
                assert_eq!(
                    render_markdown(&streamed, &Theme::default(), &PlainHighlighter),
                    render_markdown(&full, &Theme::default(), &PlainHighlighter),
                    "incremental parse diverged for {source:?}"
                );
            }
            assert!(streamed.reference_reparses() > 0);
        }
    }

    #[test]
    fn parser_exposes_terminal_block_kinds_without_losing_html() {
        let document = MarkdownDocument::new(
            "# h\n\np\n\n> q\n\n- x\n\n|a|\n|-|\n|b|\n\n---\n\n<div>html</div>\n\n```json\n{}\n```"
                .into(),
        );
        assert_eq!(
            document
                .blocks()
                .map(|block| block.kind())
                .collect::<Vec<_>>(),
            vec![
                MarkdownBlockKind::Heading,
                MarkdownBlockKind::Paragraph,
                MarkdownBlockKind::Quote,
                MarkdownBlockKind::List,
                MarkdownBlockKind::Table,
                MarkdownBlockKind::ThematicBreak,
                MarkdownBlockKind::Html,
                MarkdownBlockKind::Code,
            ]
        );
        assert!(
            strings(&render_markdown(
                &document,
                &Theme::default(),
                &PlainHighlighter
            ))
            .join("\n")
            .contains("<div>html</div>")
        );
    }

    #[test]
    fn markdown_renders_inline_blocks_links_lists_quotes_and_tables() {
        let document = MarkdownDocument::new(
            "# Heading\n\n**bold** and *italic* with [link](https://example.test) and `code`.\n\n> quote\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |"
                .into(),
        );
        let lines = render_markdown(&document, &Theme::default(), &PlainHighlighter);
        let rendered = strings(&lines).join("\n");
        for expected in [
            "Heading",
            "https://example.test",
            "> quote",
            "• one",
            "│ a   │ b   │",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
        assert!(strings(&lines).iter().any(|line| line == "│ a   │ b   │"));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.contains("bold") && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn headings_drop_markers_preserve_hierarchy_and_rules_fill_the_render_width() {
        let heading_lines = super::render_markdown_width(
            &MarkdownDocument::new(
                "# One `code`\n\n## Two\n\n### Three\n\n#### Four\n\n##### Five\n\n###### Six"
                    .into(),
            ),
            &Theme::default(),
            &PlainHighlighter,
            20,
        );
        assert_eq!(
            strings(&heading_lines),
            ["One `code`", "Two", "Three", "Four", "Five", "Six"]
        );
        let h1 = heading_lines[0].spans[0].style;
        let inline_code = heading_lines[0]
            .spans
            .iter()
            .find(|span| span.content.contains("code"))
            .expect("inline code")
            .style;
        let h2 = heading_lines[1].spans[0].style;
        assert!(h1.add_modifier.contains(Modifier::BOLD));
        assert!(h1.add_modifier.contains(Modifier::UNDERLINED));
        assert!(inline_code.add_modifier.contains(Modifier::UNDERLINED));
        assert!(h2.add_modifier.contains(Modifier::BOLD));
        assert!(!h2.add_modifier.contains(Modifier::UNDERLINED));
        for line in &heading_lines[2..] {
            let style = line.spans[0].style;
            assert!(style.add_modifier.contains(Modifier::ITALIC));
            assert!(!style.add_modifier.contains(Modifier::BOLD));
            assert!(!style.add_modifier.contains(Modifier::UNDERLINED));
        }

        for width in [0, 5, 12, 31] {
            let rules = strings(&super::render_markdown_width(
                &MarkdownDocument::new("---".into()),
                &Theme::default(),
                &PlainHighlighter,
                width,
            ));
            assert_eq!(rules, ["─".repeat(usize::from(width))]);
        }
    }

    #[test]
    fn code_wraps_by_grapheme_with_preserved_styles_and_continuation_bar() {
        struct SplitHighlighter;

        impl Highlighter for SplitHighlighter {
            fn highlight(
                &self,
                _language: &str,
                _code: &str,
                _theme: &Theme,
            ) -> Vec<ratatui::text::Line<'static>> {
                vec![ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(
                        "abc",
                        ratatui::style::Style::default().add_modifier(Modifier::BOLD),
                    ),
                    ratatui::text::Span::styled(
                        "defghijkl",
                        ratatui::style::Style::default().add_modifier(Modifier::ITALIC),
                    ),
                ])]
            }
        }

        let lines = super::render_markdown_lines_width(
            &MarkdownDocument::new("```text\nignored\n```".into()),
            &Theme::default(),
            &SplitHighlighter,
            8,
        );
        let body = lines
            .iter()
            .filter(|line| {
                line.kind == super::MarkdownLineKind::Code && line.line.to_string().starts_with('│')
            })
            .collect::<Vec<_>>();
        assert_eq!(
            body.iter()
                .map(|line| line.line.to_string())
                .collect::<Vec<_>>(),
            ["│ abcdef", "│ghijkl "]
        );
        assert!(
            body.iter()
                .flat_map(|line| line.line.spans.iter())
                .any(|span| {
                    span.content.contains("abc") && span.style.add_modifier.contains(Modifier::BOLD)
                })
        );
        assert!(
            body.iter()
                .flat_map(|line| line.line.spans.iter())
                .any(|span| {
                    span.content.contains("def")
                        && span.style.add_modifier.contains(Modifier::ITALIC)
                })
        );
        let content = body
            .iter()
            .flat_map(|line| line.line.spans.iter())
            .filter(|span| {
                span.style.add_modifier.contains(Modifier::BOLD)
                    || span.style.add_modifier.contains(Modifier::ITALIC)
            })
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(content, "abcdefghijkl");
    }

    #[test]
    fn fences_highlight_known_languages_and_fall_back_for_unknown_or_unclosed_fences() {
        let theme = Theme::default();
        let highlighter = SyntectHighlighter::default();
        let known = render_markdown(
            &MarkdownDocument::new("```rust\nfn main() {}\n```".into()),
            &theme,
            &highlighter,
        );
        assert!(strings(&known).join("\n").contains("fn main() {}"));
        assert!(
            known
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.style.fg.is_some())
        );

        for markdown in ["```made-up\nplain text\n```", "```unknown\nstill open"] {
            let lines = render_markdown(
                &MarkdownDocument::new(markdown.into()),
                &theme,
                &highlighter,
            );
            assert!(
                strings(&lines)
                    .join("\n")
                    .contains(if markdown.contains("plain") {
                        "plain text"
                    } else {
                        "still open"
                    })
            );
            if markdown.contains("plain") {
                assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
                    span.content.contains("plain text") && span.style.fg.is_none()
                }));
            }
        }
    }

    #[test]
    fn stable_code_highlights_once_across_renders_and_width_changes() {
        let document =
            MarkdownDocument::new("```rust\nfn cached() {}\n```\n\nopen paragraph".to_owned());
        let highlighter = SyntectHighlighter::default();

        for width in [80, 24, 120] {
            super::render_markdown_width(&document, &Theme::default(), &highlighter, width);
        }

        assert_eq!(highlighter.highlight_calls(), 1);
    }

    #[test]
    fn highlight_cache_evicts_least_recent_entries_under_budget_pressure() {
        let documents = [
            MarkdownDocument::new("```rust\nfn one() {}\n```\n\ntail".to_owned()),
            MarkdownDocument::new("```json\n{\"two\": 2}\n```\n\ntail".to_owned()),
            MarkdownDocument::new("```bash\necho three\n```\n\ntail".to_owned()),
        ];
        let highlighter = SyntectHighlighter::default();
        for document in &documents {
            render_markdown(document, &Theme::default(), &highlighter);
        }
        let sizes = highlighter
            .cache
            .lock()
            .unwrap()
            .entries
            .values()
            .map(|entry| entry.bytes)
            .collect::<Vec<_>>();
        assert_eq!(sizes.len(), 3);
        let budget = (sizes[0] + sizes[1])
            .max(sizes[0] + sizes[2])
            .max(sizes[1] + sizes[2]);
        highlighter.set_cache_budget(budget);

        for index in [0, 1, 0, 2] {
            render_markdown(&documents[index], &Theme::default(), &highlighter);
            let cache = highlighter.cache.lock().unwrap();
            assert!(cache.used_bytes <= cache.budget_bytes);
        }
        {
            let cache = highlighter.cache.lock().unwrap();
            assert!(cache.used_bytes <= cache.budget_bytes);
            assert_eq!(
                cache.used_bytes,
                cache
                    .entries
                    .values()
                    .map(|entry| entry.bytes)
                    .sum::<usize>()
            );
            assert!(cache.entries.keys().any(|key| key.language == "rust"));
            assert!(cache.entries.keys().any(|key| key.language == "bash"));
            assert!(!cache.entries.keys().any(|key| key.language == "json"));
        }

        for index in 0..256 {
            let document = MarkdownDocument::new(format!(
                "```rust\nfn unique_{index}() -> usize {{ {index} }}\n```\n\ntail"
            ));
            render_markdown(&document, &Theme::default(), &highlighter);
            let cache = highlighter.cache.lock().unwrap();
            assert!(cache.used_bytes <= cache.budget_bytes);
        }
    }

    #[test]
    fn highlight_cache_exactly_compares_source_after_hash_lookup() {
        let theme = Theme::default();
        let key = super::HighlightCacheKey {
            content_hash: 7,
            code_len: 4,
            language: "rust".to_owned(),
            theme: theme.key(),
            syntect_theme: super::syntect_theme_name(&theme),
        };
        let mut cache = super::HighlightCache::default();
        cache.set_theme(theme.key());
        cache.insert(key.clone(), "left", vec![ratatui::text::Line::from("left")]);

        assert!(cache.get(&key, "rift").is_none());
        assert_eq!(cache.get(&key, "left").unwrap()[0].to_string(), "left");
    }

    #[test]
    fn highlight_cache_enforces_exact_oversized_and_zero_budget_boundaries() {
        let theme = Theme::default();
        let source = "boundary";
        let key = super::HighlightCacheKey {
            content_hash: 11,
            code_len: source.len(),
            language: "rust".to_owned(),
            theme: theme.key(),
            syntect_theme: super::syntect_theme_name(&theme),
        };
        let make_lines = || vec![ratatui::text::Line::from(source)];
        let lines = make_lines();
        let entry_bytes = super::highlight_entry_bytes(&key, source, &lines, lines.capacity());

        let mut exact = super::HighlightCache {
            budget_bytes: entry_bytes,
            ..super::HighlightCache::default()
        };
        exact.insert(key.clone(), source, make_lines());
        assert_eq!(exact.used_bytes, exact.budget_bytes);
        assert_eq!(exact.entries.len(), 1);

        let mut oversized = super::HighlightCache {
            budget_bytes: entry_bytes - 1,
            ..super::HighlightCache::default()
        };
        oversized.insert(key.clone(), source, make_lines());
        assert_eq!(oversized.used_bytes, 0);
        assert!(oversized.entries.is_empty());

        let mut zero = super::HighlightCache {
            budget_bytes: 0,
            ..super::HighlightCache::default()
        };
        zero.insert(key, source, make_lines());
        assert_eq!(zero.used_bytes, 0);
        assert!(zero.entries.is_empty());
    }

    #[test]
    fn theme_changes_invalidate_stable_highlights() {
        let document = MarkdownDocument::new("```rust\nfn themed() {}\n```\n\ntail".to_owned());
        let highlighter = SyntectHighlighter::default();
        let light = Theme::default();
        let dark = Theme::new(ThemeKind::Dark, ColorLevel::TrueColor);

        render_markdown(&document, &light, &highlighter);
        render_markdown(&document, &light, &highlighter);
        render_markdown(&document, &dark, &highlighter);
        render_markdown(&document, &light, &highlighter);

        assert_eq!(highlighter.highlight_calls(), 3);
    }

    #[test]
    fn streaming_tail_rehighlights_without_rehighlighting_stable_code() {
        let mut document = MarkdownDocument::new(
            "```rust\nfn stable() {}\n```\n\n```rust\nfn streaming".to_owned(),
        );
        let highlighter = SyntectHighlighter::default();

        render_markdown(&document, &Theme::default(), &highlighter);
        render_markdown(&document, &Theme::default(), &highlighter);
        assert_eq!(highlighter.highlight_calls(), 3);

        document.append("() {}\n");
        render_markdown(&document, &Theme::default(), &highlighter);
        assert_eq!(highlighter.highlight_calls(), 4);

        document.append("```\n\ntail");
        render_markdown(&document, &Theme::default(), &highlighter);
        render_markdown(&document, &Theme::default(), &highlighter);
        assert_eq!(highlighter.highlight_calls(), 5);
    }

    #[test]
    fn cached_highlights_are_identical_to_fresh_highlights() {
        let huge_line = "x".repeat(16_384);
        let fixtures = vec![
            "```rust\nfn main() { println!(\"hi\"); }\n```\n\ntail".to_owned(),
            "```javascript\nconst value = { ok: true };\n```\n\ntail".to_owned(),
            "```json\n{\"nested\": [true, null, 3]}\n```\n\ntail".to_owned(),
            "```bash\nprintf '%s\\n' \"$HOME\"\n```\n\ntail".to_owned(),
            "```python\ndef f(value):\n    return value + 1\n```\n\ntail".to_owned(),
            "```made-up\nplain fallback\n```\n\ntail".to_owned(),
            "```rust\n```\n\ntail".to_owned(),
            "```rust\nfn 挨拶() { println!(\"你好 👋🏽\"); }\n```\n\ntail".to_owned(),
            format!("```text\n{huge_line}\n```\n\ntail"),
        ];
        let highlighter = SyntectHighlighter::default();

        for source in fixtures {
            let document = MarkdownDocument::new(source.clone());
            let fresh =
                super::render_markdown_width(&document, &Theme::default(), &highlighter, 73);
            let cached =
                super::render_markdown_width(&document, &Theme::default(), &highlighter, 73);
            assert_eq!(cached, fresh, "cached render diverged for {source:?}");
        }

        let mut unclosed = MarkdownDocument::new("```rust\nlet still_open = true;".to_owned());
        let calls_before = highlighter.highlight_calls();
        super::render_markdown_width(&unclosed, &Theme::default(), &highlighter, 73);
        assert_eq!(highlighter.highlight_calls(), calls_before + 1);
        unclosed.append("\n```\n\ntail");
        let fresh_after_stabilizing =
            super::render_markdown_width(&unclosed, &Theme::default(), &highlighter, 73);
        assert_eq!(highlighter.highlight_calls(), calls_before + 2);
        let cached = super::render_markdown_width(&unclosed, &Theme::default(), &highlighter, 73);
        assert_eq!(cached, fresh_after_stabilizing);
        assert_eq!(highlighter.highlight_calls(), calls_before + 2);
    }

    #[test]
    fn vendored_syntect_decodes_published_dumps_and_preserves_legacy_helpers() {
        let newlines: SyntaxSet = from_uncompressed_data(include_bytes!(
            "../../../vendor/syntect/assets/default_newlines.packdump"
        ))
        .unwrap();
        let nonewlines: SyntaxSet = from_uncompressed_data(include_bytes!(
            "../../../vendor/syntect/assets/default_nonewlines.packdump"
        ))
        .unwrap();
        for syntaxes in [&newlines, &nonewlines] {
            for token in ["rs", "json", "sh"] {
                assert!(
                    syntaxes
                        .find_syntax_by_token(token)
                        .or_else(|| syntaxes.find_syntax_by_extension(token))
                        .is_some(),
                    "missing built-in syntax token: {token}"
                );
            }
        }

        let themes: ThemeSet = from_binary(include_bytes!(
            "../../../vendor/syntect/assets/default.themedump"
        ));
        for theme in ["base16-ocean.dark", "base16-eighties.dark"] {
            assert!(
                themes.themes.contains_key(theme),
                "missing built-in theme: {theme}"
            );
        }

        let fixture = vec!["rust".to_owned(), "json".to_owned(), "shell".to_owned()];
        let compressed = dump_binary(&fixture);
        assert_eq!(from_binary::<Vec<String>>(&compressed), fixture);

        let directory = tempfile::tempdir().unwrap();
        let compressed_path = directory.path().join("fixture.dump");
        dump_to_file(&fixture, &compressed_path).unwrap();
        assert_eq!(
            from_dump_file::<Vec<String>, _>(&compressed_path).unwrap(),
            fixture
        );

        let uncompressed_path = directory.path().join("fixture.packdump");
        dump_to_uncompressed_file(&fixture, &uncompressed_path).unwrap();
        let mut legacy_bytes = Vec::new();
        legacy_bytes.extend_from_slice(&3_u64.to_le_bytes());
        for value in ["rust", "json", "shell"] {
            legacy_bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
            legacy_bytes.extend_from_slice(value.as_bytes());
        }
        assert_eq!(std::fs::read(&uncompressed_path).unwrap(), legacy_bytes);
        assert_eq!(
            from_uncompressed_data::<Vec<String>>(&legacy_bytes).unwrap(),
            fixture
        );
        assert_eq!(
            from_uncompressed_dump_file::<Vec<String>, _>(&uncompressed_path).unwrap(),
            fixture
        );
    }

    #[test]
    fn syntax_highlight_snapshot_covers_rust_json_bash_aliases_and_plain_fallback() {
        let highlighter = SyntectHighlighter::default();
        let mut snapshot = Vec::new();
        for (theme_name, theme) in [
            ("default-truecolor", Theme::default()),
            (
                "high-contrast-ansi16",
                Theme::new(
                    crate::theme::ThemeKind::HighContrast,
                    crate::theme::ColorLevel::Ansi16,
                ),
            ),
            (
                "mono",
                Theme::new(
                    crate::theme::ThemeKind::Mono,
                    crate::theme::ColorLevel::None,
                ),
            ),
        ] {
            snapshot.push(format!("{theme_name}:"));
            for (language, code) in [
                ("rs", "fn main() {}"),
                ("json", r#"{"ok": true}"#),
                ("sh", "echo \"$HOME\""),
                ("not-a-language", "plain"),
            ] {
                snapshot.push(format!("  {language}:"));
                for span in highlighter
                    .highlight(language, code, &theme)
                    .into_iter()
                    .flat_map(|line| line.spans)
                {
                    snapshot.push(format!(
                        "    text={:?} fg={:?} bg={:?} modifiers={:?}",
                        span.content, span.style.fg, span.style.bg, span.style.add_modifier
                    ));
                }
            }
        }
        assert_snapshot!(snapshot.join("\n"), @r#"
default-truecolor:
  rs:
    text="fn" fg=Some(Rgb(167, 29, 93)) bg=None modifiers=BOLD
    text=" " fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
    text="main" fg=Some(Rgb(121, 93, 163)) bg=None modifiers=BOLD
    text="(" fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
    text=")" fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
    text=" " fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
    text="{" fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
    text="}" fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
  json:
    text="{" fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
    text="\"" fg=Some(Rgb(24, 54, 145)) bg=None modifiers=BOLD
    text="ok" fg=Some(Rgb(24, 54, 145)) bg=None modifiers=BOLD
    text="\"" fg=Some(Rgb(24, 54, 145)) bg=None modifiers=BOLD
    text=":" fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
    text=" " fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
    text="true" fg=Some(Rgb(0, 134, 179)) bg=None modifiers=NONE
    text="}" fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
  sh:
    text="echo" fg=Some(Rgb(98, 163, 92)) bg=None modifiers=NONE
    text=" " fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
    text="\"" fg=Some(Rgb(24, 54, 145)) bg=None modifiers=NONE
    text="$" fg=Some(Rgb(24, 54, 145)) bg=None modifiers=NONE
    text="HOME" fg=Some(Rgb(50, 50, 50)) bg=None modifiers=NONE
    text="\"" fg=Some(Rgb(24, 54, 145)) bg=None modifiers=NONE
  not-a-language:
    text="plain" fg=None bg=None modifiers=NONE
high-contrast-ansi16:
  rs:
    text="fn" fg=Some(White) bg=None modifiers=NONE
    text=" " fg=Some(White) bg=None modifiers=NONE
    text="main" fg=Some(LightCyan) bg=None modifiers=NONE
    text="(" fg=Some(White) bg=None modifiers=NONE
    text=")" fg=Some(White) bg=None modifiers=NONE
    text=" " fg=Some(White) bg=None modifiers=NONE
    text="{" fg=Some(White) bg=None modifiers=NONE
    text="}" fg=Some(White) bg=None modifiers=NONE
  json:
    text="{" fg=Some(White) bg=None modifiers=NONE
    text="\"" fg=Some(White) bg=None modifiers=NONE
    text="ok" fg=Some(White) bg=None modifiers=NONE
    text="\"" fg=Some(White) bg=None modifiers=NONE
    text=":" fg=Some(White) bg=None modifiers=NONE
    text=" " fg=Some(White) bg=None modifiers=NONE
    text="true" fg=Some(LightYellow) bg=None modifiers=NONE
    text="}" fg=Some(White) bg=None modifiers=NONE
  sh:
    text="echo" fg=Some(LightCyan) bg=None modifiers=NONE
    text=" " fg=Some(White) bg=None modifiers=NONE
    text="\"" fg=Some(White) bg=None modifiers=NONE
    text="$" fg=Some(White) bg=None modifiers=NONE
    text="HOME" fg=Some(LightRed) bg=None modifiers=NONE
    text="\"" fg=Some(White) bg=None modifiers=NONE
  not-a-language:
    text="plain" fg=None bg=None modifiers=NONE
mono:
  rs:
    text="fn" fg=None bg=None modifiers=BOLD
    text=" " fg=None bg=None modifiers=NONE
    text="main" fg=None bg=None modifiers=BOLD
    text="(" fg=None bg=None modifiers=NONE
    text=")" fg=None bg=None modifiers=NONE
    text=" " fg=None bg=None modifiers=NONE
    text="{" fg=None bg=None modifiers=NONE
    text="}" fg=None bg=None modifiers=NONE
  json:
    text="{" fg=None bg=None modifiers=NONE
    text="\"" fg=None bg=None modifiers=BOLD
    text="ok" fg=None bg=None modifiers=BOLD
    text="\"" fg=None bg=None modifiers=BOLD
    text=":" fg=None bg=None modifiers=NONE
    text=" " fg=None bg=None modifiers=NONE
    text="true" fg=None bg=None modifiers=NONE
    text="}" fg=None bg=None modifiers=NONE
  sh:
    text="echo" fg=None bg=None modifiers=NONE
    text=" " fg=None bg=None modifiers=NONE
    text="\"" fg=None bg=None modifiers=NONE
    text="$" fg=None bg=None modifiers=NONE
    text="HOME" fg=None bg=None modifiers=NONE
    text="\"" fg=None bg=None modifiers=NONE
  not-a-language:
    text="plain" fg=None bg=None modifiers=NONE
"#);
    }

    #[test]
    fn synthetic_syntect_theme_propagates_real_font_modifiers_across_color_modes() {
        let foreground = SyntectColor {
            r: 12,
            g: 34,
            b: 56,
            a: 255,
        };
        let background = SyntectColor {
            r: 78,
            g: 90,
            b: 123,
            a: 255,
        };
        let styles = [
            ("synthetic.bold", FontStyle::BOLD),
            ("synthetic.italic", FontStyle::ITALIC),
            ("synthetic.underline", FontStyle::UNDERLINE),
            ("synthetic.bold-italic", FontStyle::BOLD | FontStyle::ITALIC),
            (
                "synthetic.all",
                FontStyle::BOLD | FontStyle::ITALIC | FontStyle::UNDERLINE,
            ),
        ];
        let synthetic_theme = SyntectTheme {
            name: Some("cookie modifier propagation".to_owned()),
            settings: ThemeSettings {
                foreground: Some(foreground),
                background: Some(background),
                ..ThemeSettings::default()
            },
            scopes: styles
                .iter()
                .map(|(scope, font_style)| ThemeItem {
                    scope: scope.parse::<ScopeSelectors>().unwrap(),
                    style: StyleModifier {
                        foreground: Some(foreground),
                        background: Some(background),
                        font_style: Some(*font_style),
                    },
                })
                .collect(),
            ..SyntectTheme::default()
        };
        let syntect_highlighter = SyntectThemeHighlighter::new(&synthetic_theme);
        let mut snapshot = Vec::new();
        for (theme_name, theme) in [
            ("default-truecolor", Theme::default()),
            (
                "high-contrast-ansi16",
                Theme::new(
                    crate::theme::ThemeKind::HighContrast,
                    crate::theme::ColorLevel::Ansi16,
                ),
            ),
            (
                "mono",
                Theme::new(
                    crate::theme::ThemeKind::Mono,
                    crate::theme::ColorLevel::None,
                ),
            ),
        ] {
            snapshot.push(format!("{theme_name}:"));
            for &(scope, expected_font_style) in &styles {
                let syntect_style =
                    syntect_highlighter.style_for_stack(&[Scope::new(scope).unwrap()]);
                assert_eq!(syntect_style.font_style, expected_font_style);
                let terminal = terminal_style_from_syntect(syntect_style, &theme);
                snapshot.push(format!(
                    "  {scope}: syntect={:?} fg={:?} bg={:?} modifiers={:?}",
                    syntect_style.font_style, terminal.fg, terminal.bg, terminal.add_modifier
                ));
            }
        }
        assert_snapshot!(snapshot.join("\n"), @r#"
default-truecolor:
  synthetic.bold: syntect=BOLD fg=Some(Rgb(12, 34, 56)) bg=None modifiers=BOLD
  synthetic.italic: syntect=ITALIC fg=Some(Rgb(12, 34, 56)) bg=None modifiers=ITALIC
  synthetic.underline: syntect=UNDERLINE fg=Some(Rgb(12, 34, 56)) bg=None modifiers=UNDERLINED
  synthetic.bold-italic: syntect=BOLD | ITALIC fg=Some(Rgb(12, 34, 56)) bg=None modifiers=BOLD | ITALIC
  synthetic.all: syntect=BOLD | UNDERLINE | ITALIC fg=Some(Rgb(12, 34, 56)) bg=None modifiers=BOLD | ITALIC | UNDERLINED
high-contrast-ansi16:
  synthetic.bold: syntect=BOLD fg=Some(LightBlue) bg=None modifiers=BOLD
  synthetic.italic: syntect=ITALIC fg=Some(LightBlue) bg=None modifiers=ITALIC
  synthetic.underline: syntect=UNDERLINE fg=Some(LightBlue) bg=None modifiers=UNDERLINED
  synthetic.bold-italic: syntect=BOLD | ITALIC fg=Some(LightBlue) bg=None modifiers=BOLD | ITALIC
  synthetic.all: syntect=BOLD | UNDERLINE | ITALIC fg=Some(LightBlue) bg=None modifiers=BOLD | ITALIC | UNDERLINED
mono:
  synthetic.bold: syntect=BOLD fg=None bg=None modifiers=BOLD
  synthetic.italic: syntect=ITALIC fg=None bg=None modifiers=ITALIC
  synthetic.underline: syntect=UNDERLINE fg=None bg=None modifiers=UNDERLINED
  synthetic.bold-italic: syntect=BOLD | ITALIC fg=None bg=None modifiers=BOLD | ITALIC
  synthetic.all: syntect=BOLD | UNDERLINE | ITALIC fg=None bg=None modifiers=BOLD | ITALIC | UNDERLINED
"#);
    }

    #[test]
    fn markdown_text_snapshot_is_stable_across_color_themes() {
        let document = MarkdownDocument::new(
            "## Result\n\n1. **first**\n2. [second](https://example.test)\n\n> done".into(),
        );
        let expected = vec![
            "Result",
            "1. first",
            "2. second <https://example.test>",
            "> done",
        ];
        for theme in [
            Theme::default(),
            Theme::new(
                crate::theme::ThemeKind::Mono,
                crate::theme::ColorLevel::None,
            ),
            Theme::new(
                crate::theme::ThemeKind::HighContrast,
                crate::theme::ColorLevel::Ansi16,
            ),
        ] {
            assert_eq!(
                strings(&render_markdown(&document, &theme, &PlainHighlighter)),
                expected
            );
        }
    }

    #[test]
    fn markdown_terminal_snapshot_covers_required_block_aesthetics() {
        let document = MarkdownDocument::new(
            "## Result\n\n**bold** and *italic*, `code`, [link](https://example.test).\n\n- [x] done\n- [ ] next\n\n> quoted\n\n| key | value |\n| --- | --- |\n| a | b |\n\n---\n\n<kbd>html</kbd>\n\n```rust\nfn main() {}\n```"
                .into(),
        );
        let rendered = strings(&render_markdown(
            &document,
            &Theme::default(),
            &PlainHighlighter,
        ))
        .join("\n");
        assert_snapshot!(rendered, @r#"
Result
bold and italic, `code`, link <https://example.test>.
• [x] done
• [ ] next
> quoted
┌─────┬───────┐
│ key │ value │
├─────┼───────┤
│ a   │ b     │
└─────┴───────┘
────────────────
<kbd>html</kbd>
┌─ code: rust
│ fn main() {}
└─
"#);
    }

    fn table_render(source: &str, width: u16, theme: &Theme) -> Vec<String> {
        let document = MarkdownDocument::new(source.into());
        strings(&super::render_markdown_width(
            &document,
            theme,
            &PlainHighlighter,
            width,
        ))
    }

    #[test]
    fn table_alignment_markers_are_honored_per_column() {
        let lines = table_render(
            "| left | center | right |\n|:-----|:------:|------:|\n| a | b | c |\n| longer | x | y |",
            80,
            &Theme::default(),
        );
        // Left column hugs the left border; right column hugs the right
        // border; center column is padded on both sides.
        assert!(
            lines
                .iter()
                .any(|line| line.as_str() == "│ left   │ center │ right │")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.as_str() == "│ a      │   b    │     c │")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.as_str() == "│ longer │   x    │     y │")
        );
        assert!(lines.iter().any(|line| line.as_str().starts_with('└')));
    }

    #[test]
    fn table_empty_cells_unicode_and_escaped_pipes_render_safely() {
        let lines = table_render(
            "| name | note |\n|------|------|\n| 界面 | a \\| b |\n| | 👨‍👩‍👧‍👦 |",
            80,
            &Theme::default(),
        );
        let rendered = lines.join("\n");
        assert!(rendered.contains("界面"));
        assert!(rendered.contains("a | b"), "escaped pipe stays in the cell");
        assert!(rendered.contains("👨‍👩‍👧‍👦"));
        // The empty cell row renders with padding, not a collapsed column.
        assert!(
            lines
                .iter()
                .any(|line| line.contains('│') && line.trim_end().ends_with('│'))
        );
        // No raw markdown pipes leak outside the border glyphs.
        assert!(!rendered.contains("|------|"));
    }

    #[test]
    fn table_cells_keep_inline_markup_styles_with_distinct_inline_code_foreground() {
        let document = MarkdownDocument::new(
            "| fn | note |\n|----|------|\n| `render()` | **fast** [docs](https://x.test) |".into(),
        );
        let lines =
            super::render_markdown_width(&document, &Theme::default(), &PlainHighlighter, 80);
        let code_span = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("render()"))
            .expect("inline code span");
        // Inline code is foreground-only in the default theme: a warm
        // terracotta plus bold, never a background or reverse video.
        assert!(code_span.style.bg.is_none());
        assert!(code_span.style.fg.is_some());
        assert!(!code_span.style.add_modifier.contains(Modifier::REVERSED));
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.content.contains("fast")
                    && span.style.add_modifier.contains(Modifier::BOLD))
        );
        let rendered = strings(&lines).join("\n");
        assert!(rendered.contains("https://x.test"));
    }

    #[test]
    fn streamed_incomplete_table_completes_incrementally() {
        let mut document = MarkdownDocument::new("| a | b |\n|---|---|\n| 1".into());
        let partial = strings(&super::render_markdown_width(
            &document,
            &Theme::default(),
            &PlainHighlighter,
            80,
        ))
        .join("\n");
        document.append(" | 2 |\n| 3 | 4 |");
        let complete = strings(&super::render_markdown_width(
            &document,
            &Theme::default(),
            &PlainHighlighter,
            80,
        ))
        .join("\n");
        assert!(complete.contains('┌'));
        assert!(complete.contains("│ 1   │ 2   │"));
        assert!(complete.contains("│ 3   │ 4   │"));
        assert!(complete.contains('└'));
        // The completed render contains the partial row content; structure
        // grows without duplicating rows.
        let row_count = complete.matches("│ 1").count();
        assert_eq!(row_count, 1);
        let _ = partial;
    }

    #[test]
    fn table_width_allocation_wraps_cells_and_falls_back_to_stacked_on_narrow() {
        let source = "| description | status |\n|---|---|\n| a very long description that must wrap | done |";
        let wrapped = table_render(source, 40, &Theme::default());
        let joined = wrapped.join("\n");
        assert!(joined.contains('┌'), "table still renders at 40 columns");
        // No rendered line exceeds the available width.
        assert!(
            wrapped
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 40),
            "lines fit: {joined}"
        );

        // Below the minimum useful column width the layout switches to a
        // readable stacked representation instead of sliver columns.
        let stacked = table_render(source, 9, &Theme::default());
        let stacked_joined = stacked.join("\n");
        assert!(stacked_joined.contains("description: "), "stacked fallback");
        assert!(stacked_joined.contains("status: done"));
        assert!(
            stacked.iter().all(|line| !line.contains('┌')),
            "no unusable sliver columns at 9 columns"
        );

        // Tiny widths never panic and stay readable.
        for width in 1..10 {
            let tiny = table_render(source, width, &Theme::default());
            assert!(!tiny.is_empty(), "width {width}");
        }
    }

    #[test]
    fn table_header_is_styled_distinctly_without_color_dependency() {
        let document = MarkdownDocument::new("| h |\n|---|\n| b |".into());
        for theme in [
            Theme::default(),
            Theme::new(
                crate::theme::ThemeKind::Mono,
                crate::theme::ColorLevel::None,
            ),
            Theme::new(
                crate::theme::ThemeKind::HighContrast,
                crate::theme::ColorLevel::Ansi16,
            ),
        ] {
            let lines = super::render_markdown_width(&document, &theme, &PlainHighlighter, 40);
            let header = lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .find(|span| span.content.contains('h'))
                .expect("header span");
            // Bold marks the header in every theme — never color-only.
            assert!(
                header.style.add_modifier.contains(Modifier::BOLD),
                "{:?}",
                theme.key()
            );
            let rendered = strings(&lines).join("\n");
            assert!(rendered.contains('├'));
        }
    }

    #[test]
    fn table_cells_never_inject_control_sequences() {
        let document = MarkdownDocument::new("| a |\n|---|\n| pre\u{7}\u{1b}[31mpost |".into());
        let lines =
            super::render_markdown_width(&document, &Theme::default(), &PlainHighlighter, 40);
        for line in &lines {
            for span in &line.spans {
                assert!(
                    !span.content.contains('\u{7}') && !span.content.contains('\u{1b}'),
                    "control characters are replaced: {span:?}"
                );
            }
        }
    }

    #[test]
    fn inline_code_is_foreground_only_except_in_high_contrast() {
        let document = MarkdownDocument::new("before `let x = 1;` after".into());
        for (theme, has_background) in [
            (Theme::default(), false),
            (
                Theme::new(
                    crate::theme::ThemeKind::HighContrast,
                    crate::theme::ColorLevel::Ansi16,
                ),
                true,
            ),
            (
                Theme::new(
                    crate::theme::ThemeKind::Mono,
                    crate::theme::ColorLevel::None,
                ),
                false,
            ),
        ] {
            let lines = render_markdown(&document, &theme, &PlainHighlighter);
            let code_span = lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .find(|span| span.content.contains("let x = 1;"))
                .expect("inline code span");
            assert_eq!(
                code_span.style.bg.is_some(),
                has_background,
                "{:?}",
                theme.key()
            );
            assert!(!code_span.style.add_modifier.contains(Modifier::REVERSED));
            // The backticks stay visible and bold carries the distinction in
            // mono terminals, so inline code never depends on color alone.
            let rendered = strings(&lines).join("");
            assert!(rendered.contains("`let x = 1;`"));
            assert!(
                code_span.style.add_modifier.contains(Modifier::BOLD)
                    || theme.key().colors != crate::theme::ColorLevel::None
            );
        }
    }
}

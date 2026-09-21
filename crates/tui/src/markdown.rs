//! Incremental Markdown parsing and direct ratatui span rendering.

use std::{
    any::TypeId,
    collections::{BTreeMap, BTreeSet, HashMap},
    hash::BuildHasher,
    mem::{size_of, size_of_val},
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
    highlighting::{FontStyle, HighlightState, Style as SyntectStyle, ThemeSet},
    parsing::{ParseState, SyntaxSet},
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
    lines: Arc<[Line<'static>]>,
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

    fn get(&mut self, key: &HighlightCacheKey, source: &str) -> Option<Arc<[Line<'static>]>> {
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

    fn insert(&mut self, key: HighlightCacheKey, source: &str, lines: Arc<[Line<'static>]>) {
        debug_assert!(self.used_bytes <= self.budget_bytes);
        self.generation = self.generation.wrapping_add(1);
        let bytes = highlight_entry_bytes(&key, source, &lines);
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

fn highlight_entry_bytes(key: &HighlightCacheKey, source: &str, lines: &[Line<'static>]) -> usize {
    size_of::<HighlightCacheKey>()
        + size_of::<HighlightCacheEntry>()
        + HIGHLIGHT_CACHE_ENTRY_OVERHEAD_BYTES
        + 3 * HIGHLIGHT_CACHE_ALLOCATION_OVERHEAD_BYTES
        + key.language.capacity()
        + source.len()
        + 2 * size_of::<usize>()
        + size_of_val(lines)
        + lines
            .iter()
            .map(|line| {
                size_of::<Span<'static>>() * line.spans.capacity()
                    + usize::from(!line.spans.is_empty())
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
    source: Arc<str>,
    content_hash: u64,
    content_len: usize,
    rendered: Arc<Mutex<Option<RenderedBlockCacheEntry>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderedBlockCacheKey {
    content_hash: u64,
    code_len: usize,
    width: u16,
    theme: ThemeKey,
    quote_depth: usize,
    highlighter: u64,
}

#[derive(Clone, Debug)]
struct RenderedBlockCacheEntry {
    key: RenderedBlockCacheKey,
    source: Arc<str>,
    lines: Arc<[MarkdownLine]>,
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
    Table,
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
            let range = current_start..current_end;
            blocks.push(ParsedBlock {
                block: MarkdownBlock {
                    kind: current_kind,
                    events: std::mem::take(&mut current),
                    source: Arc::from(&source[range.clone()]),
                    content_hash: foldhash::fast::FixedState::default()
                        .hash_one(&source[range.clone()]),
                    content_len: range.len(),
                    rendered: Arc::new(Mutex::new(None)),
                },
                range,
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
    fn highlight(&self, language: &str, code: &str, theme: &Theme) -> Arc<[Line<'static>]>;

    fn highlight_stable(&self, language: &str, code: &str, theme: &Theme) -> Arc<[Line<'static>]> {
        self.highlight(language, code, theme)
    }

    fn cache_key(&self) -> u64 {
        foldhash::fast::FixedState::default().hash_one((
            TypeId::of::<Self>(),
            std::ptr::from_ref(self).cast::<()>() as usize,
        ))
    }
}

#[derive(Default)]
pub struct PlainHighlighter;

impl Highlighter for PlainHighlighter {
    fn highlight(&self, _language: &str, code: &str, _theme: &Theme) -> Arc<[Line<'static>]> {
        plain_code_lines(code).into()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct IncrementalHighlightKey {
    language: String,
    theme: ThemeKey,
    syntect_theme: &'static str,
}

#[derive(Debug)]
struct IncrementalHighlightEntry {
    complete_source: String,
    lines: Vec<Line<'static>>,
    highlight_state: HighlightState,
    parse_state: ParseState,
}

pub struct SyntectHighlighter {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
    plain: PlainHighlighter,
    cache: Mutex<HighlightCache>,
    incremental: Mutex<HashMap<IncrementalHighlightKey, IncrementalHighlightEntry>>,
    #[cfg(test)]
    highlight_calls: AtomicUsize,
    #[cfg(test)]
    incremental_bytes: AtomicUsize,
}

impl Default for SyntectHighlighter {
    fn default() -> Self {
        Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            themes: ThemeSet::load_defaults(),
            plain: PlainHighlighter,
            cache: Mutex::new(HighlightCache::default()),
            incremental: Mutex::new(HashMap::new()),
            #[cfg(test)]
            highlight_calls: AtomicUsize::new(0),
            #[cfg(test)]
            incremental_bytes: AtomicUsize::new(0),
        }
    }
}

impl Highlighter for SyntectHighlighter {
    fn highlight(&self, language: &str, code: &str, theme: &Theme) -> Arc<[Line<'static>]> {
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
        let complete_len = code.rfind('\n').map_or(0, |index| index + 1);
        let complete_source = &code[..complete_len];
        let partial_source = &code[complete_len..];
        let key = IncrementalHighlightKey {
            language: language.clone(),
            theme: theme.key(),
            syntect_theme: theme_name,
        };
        let mut cache = self
            .incremental
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = cache.entry(key).or_insert_with(|| {
            let highlighter = HighlightLines::new(syntax, syntax_theme);
            let (highlight_state, parse_state) = highlighter.state();
            IncrementalHighlightEntry {
                complete_source: String::new(),
                lines: Vec::new(),
                highlight_state,
                parse_state,
            }
        });
        let extends_cached = complete_source.starts_with(&entry.complete_source);
        let mut highlighter = if extends_cached {
            HighlightLines::from_state(
                syntax_theme,
                entry.highlight_state.clone(),
                entry.parse_state.clone(),
            )
        } else {
            entry.complete_source.clear();
            entry.lines.clear();
            HighlightLines::new(syntax, syntax_theme)
        };
        let appended = if extends_cached {
            &complete_source[entry.complete_source.len()..]
        } else {
            complete_source
        };
        #[cfg(test)]
        self.incremental_bytes
            .fetch_add(appended.len() + partial_source.len(), Ordering::Relaxed);
        if append_highlighted_lines(
            &mut highlighter,
            appended,
            &self.syntaxes,
            theme,
            &mut entry.lines,
        )
        .is_err()
        {
            let reset = HighlightLines::new(syntax, syntax_theme);
            let (highlight_state, parse_state) = reset.state();
            entry.complete_source.clear();
            entry.lines.clear();
            entry.highlight_state = highlight_state;
            entry.parse_state = parse_state;
            return self.plain.highlight(language.as_str(), code, theme);
        }
        entry.complete_source.clear();
        entry.complete_source.push_str(complete_source);
        let (highlight_state, parse_state) = highlighter.state();
        entry.highlight_state = highlight_state;
        entry.parse_state = parse_state;

        let mut lines = entry.lines.clone();
        if !partial_source.is_empty() {
            let mut partial = HighlightLines::from_state(
                syntax_theme,
                entry.highlight_state.clone(),
                entry.parse_state.clone(),
            );
            if append_highlighted_lines(
                &mut partial,
                partial_source,
                &self.syntaxes,
                theme,
                &mut lines,
            )
            .is_err()
            {
                return self.plain.highlight(language.as_str(), code, theme);
            }
        }
        if lines.is_empty() {
            lines.push(Line::default());
        }
        lines.into()
    }

    fn highlight_stable(&self, language: &str, code: &str, theme: &Theme) -> Arc<[Line<'static>]> {
        let incremental_key = IncrementalHighlightKey {
            language: normalized_language(language),
            theme: theme.key(),
            syntect_theme: syntect_theme_name(theme),
        };
        self.incremental
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&incremental_key);
        let key = HighlightCacheKey {
            content_hash: foldhash::fast::FixedState::default().hash_one((language, code)),
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

        #[cfg(test)]
        self.highlight_calls.fetch_add(1, Ordering::Relaxed);
        let lines = self.highlight_fresh(language, code, theme);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.set_theme(theme.key());
        cache.insert(key, code, Arc::clone(&lines));
        lines
    }
}

impl SyntectHighlighter {
    fn highlight_fresh(&self, language: &str, code: &str, theme: &Theme) -> Arc<[Line<'static>]> {
        let language = normalized_language(language);
        let Some(syntax) = self
            .syntaxes
            .find_syntax_by_token(&language)
            .or_else(|| self.syntaxes.find_syntax_by_extension(&language))
        else {
            return self.plain.highlight(&language, code, theme);
        };
        let Some(syntax_theme) = self.themes.themes.get(syntect_theme_name(theme)) else {
            return self.plain.highlight(&language, code, theme);
        };
        let mut highlighter = HighlightLines::new(syntax, syntax_theme);
        let mut lines = Vec::new();
        if append_highlighted_lines(&mut highlighter, code, &self.syntaxes, theme, &mut lines)
            .is_err()
        {
            return self.plain.highlight(&language, code, theme);
        }
        if lines.is_empty() {
            lines.push(Line::default());
        }
        lines.into()
    }
}

fn append_highlighted_lines(
    highlighter: &mut HighlightLines<'_>,
    source: &str,
    syntaxes: &SyntaxSet,
    theme: &Theme,
    lines: &mut Vec<Line<'static>>,
) -> Result<(), syntect::Error> {
    for source_line in LinesWithEndings::from(source) {
        let ranges = highlighter.highlight_line(source_line, syntaxes)?;
        let spans = ranges
            .into_iter()
            .filter_map(|(syntect_style, content)| {
                let content = content.strip_suffix('\n').unwrap_or(content);
                let content = content.strip_suffix('\r').unwrap_or(content);
                (!content.is_empty()).then(|| {
                    Span::styled(
                        content.to_owned(),
                        terminal_style_from_syntect(syntect_style, theme),
                    )
                })
            })
            .collect::<Vec<_>>();
        lines.push(Line::from(spans));
    }
    Ok(())
}

#[cfg(test)]
impl SyntectHighlighter {
    fn highlight_calls(&self) -> usize {
        self.highlight_calls.load(Ordering::Relaxed)
    }

    fn incremental_bytes(&self) -> usize {
        self.incremental_bytes.load(Ordering::Relaxed)
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
    let width = width.max(1);
    let mut words: Vec<Vec<Span<'static>>> = Vec::new();
    let mut word = Vec::new();
    for span in spans {
        for grapheme in span.content.graphemes(true) {
            let is_space = grapheme.chars().all(char::is_whitespace);
            if is_space && !word.is_empty() {
                word.push(Span::styled(grapheme.to_owned(), span.style));
                words.push(std::mem::take(&mut word));
            } else if let Some(last) = word.last_mut()
                && last.style == span.style
            {
                last.content.to_mut().push_str(grapheme);
            } else {
                word.push(Span::styled(grapheme.to_owned(), span.style));
            }
        }
    }
    if !word.is_empty() {
        words.push(word);
    }

    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut column = 0usize;
    for word in words {
        let word_width = cell_width(&word);
        if column > 0 && column + word_width > width {
            rows.push(Vec::new());
            column = 0;
        }
        if word_width <= width {
            column += word_width;
            rows.last_mut().expect("one row").extend(word);
            continue;
        }
        for span in word {
            for grapheme in span.content.graphemes(true) {
                let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
                if column > 0 && column + grapheme_width > width {
                    rows.push(Vec::new());
                    column = 0;
                }
                if let Some(last) = rows.last_mut().and_then(|row| row.last_mut())
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
    if used > width {
        let mut truncated = Vec::new();
        let mut remaining = width;
        for span in spans {
            let mut content = String::new();
            for grapheme in span.content.graphemes(true) {
                let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
                if grapheme_width > remaining {
                    break;
                }
                content.push_str(grapheme);
                remaining -= grapheme_width;
            }
            if !content.is_empty() {
                truncated.push(Span::styled(content, span.style));
            }
            if remaining == 0 {
                break;
            }
        }
        return truncated;
    }
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
                let marker = if available >= 2 { "· " } else { "" };
                let mut spans = vec![
                    Span::styled(quote_prefix.to_owned(), theme.quote()),
                    Span::styled(marker.to_owned(), theme.code_border()),
                    Span::styled(format!("{header_text}: "), theme.heading()),
                ];
                spans.extend(sanitize_cell(cell.clone()));
                let wrapped = wrap_cell(
                    &spans[1..],
                    available.saturating_sub(UnicodeWidthStr::width(marker)),
                    Alignment::Left,
                );
                for row in wrapped {
                    let mut line = vec![spans[0].clone()];
                    line.extend(row);
                    lines.push(Line::from(line));
                }
            }
        }
        if lines.is_empty() {
            let marker = if available >= 2 { "· " } else { "" };
            let wrapped = wrap_cell(
                &[Span::styled(
                    format!("{marker}(empty table)"),
                    theme.muted(),
                )],
                available.saturating_sub(UnicodeWidthStr::width(marker)),
                Alignment::Left,
            );
            for row in wrapped {
                let mut line = vec![Span::styled(quote_prefix.to_owned(), theme.quote())];
                line.extend(row);
                lines.push(Line::from(line));
            }
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
    let mut lines = Vec::new();
    let highlighter_key = highlighter.cache_key();
    for block in &document.stable_blocks {
        let key = RenderedBlockCacheKey {
            content_hash: block.content_hash,
            code_len: block.content_len,
            width,
            theme: theme.key(),
            quote_depth: 0,
            highlighter: highlighter_key,
        };
        let cached = block
            .rendered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|entry| entry.key == key && entry.source == block.source)
            .map(|entry| Arc::clone(&entry.lines));
        let rendered = cached.unwrap_or_else(|| {
            let rendered: Arc<[MarkdownLine]> =
                render_block(block, true, theme, highlighter, width).into();
            *block
                .rendered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(RenderedBlockCacheEntry {
                    key,
                    source: Arc::clone(&block.source),
                    lines: Arc::clone(&rendered),
                });
            rendered
        });
        lines.extend(rendered.iter().cloned());
    }
    let mut renderer = MarkdownRenderer::new(theme, highlighter);
    renderer.width = usize::from(width);
    for block in &document.tail_blocks {
        renderer.begin_block(false);
        for event in &block.events {
            renderer.event(event);
        }
    }
    renderer.flush();
    lines.extend(renderer.lines);
    if lines.is_empty() {
        lines.push(MarkdownLine::default());
    }
    lines
}

fn render_block(
    block: &MarkdownBlock,
    stable: bool,
    theme: &Theme,
    highlighter: &dyn Highlighter,
    width: u16,
) -> Vec<MarkdownLine> {
    let mut renderer = MarkdownRenderer::new(theme, highlighter);
    renderer.width = usize::from(width);
    renderer.begin_block(stable);
    for event in &block.events {
        renderer.event(event);
    }
    // Stable blocks render independently for cacheability. This flush is
    // intentionally a block boundary, so trailing HTML/footnote text cannot
    // merge into the following paragraph as it did before block caching.
    renderer.flush();
    renderer.lines
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
        let highlighted = if code.stable {
            self.highlighter
                .highlight_stable(label, &code.code, self.theme)
        } else {
            self.highlighter.highlight(label, &code.code, self.theme)
        };
        let quote_width = UnicodeWidthStr::width(quote_prefix.as_str());
        let first_width = self.width.saturating_sub(quote_width + 2).max(1);
        let continuation_width = self.width.saturating_sub(quote_width + 1).max(1);
        let mut body = Vec::new();
        for line in highlighted.iter() {
            let line_style = line.style;
            for (index, mut content) in
                wrap_code_spans(line.spans.clone(), first_width, continuation_width)
                    .into_iter()
                    .enumerate()
            {
                if let Some(band) = band {
                    for span in &mut content {
                        span.style = span.style.patch(band);
                    }
                }
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
                kind: MarkdownLineKind::Table,
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

pub(crate) fn wrap_code_spans(
    spans: Vec<Span<'static>>,
    first_width: usize,
    continuation_width: usize,
) -> Vec<Vec<Span<'static>>> {
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0;
    let mut available = first_width.max(1);

    for span in spans {
        for grapheme in span.content.graphemes(true) {
            let mut grapheme_width = UnicodeWidthStr::width(grapheme);
            if current_width > 0 && current_width + grapheme_width > available {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                available = continuation_width.max(1);
            }
            let grapheme = if grapheme_width > available {
                grapheme_width = 1;
                "�"
            } else {
                grapheme
            };
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
mod tests;

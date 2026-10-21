//! Markdown kept in a renderer-neutral form: the source text and its
//! pulldown-cmark events, split into top-level blocks and parsed
//! incrementally while text streams in. Each client renders the blocks its
//! own way and may keep what it derives from a settled block in that block's
//! [`RendererCache`].

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    hash::BuildHasher,
    sync::{Arc, OnceLock},
};

use pulldown_cmark::{BrokenLink, Event, LinkType, Options, Parser, Tag};

#[derive(Clone, Debug, Default)]
pub struct MarkdownDocument {
    source: String,
    stable_prefix_len: usize,
    stable_blocks: Vec<MarkdownBlock>,
    tail_blocks: Vec<MarkdownBlock>,
    stable_reference_definitions: BTreeMap<String, ReferenceDefinition>,
    reference_definitions: BTreeMap<String, ReferenceDefinition>,
    stable_reference_dependencies: BTreeSet<String>,
    /// Render every source newline as a line break instead of joining soft
    /// breaks with a space: typed text means its newlines.
    hard_breaks: bool,
    /// No more text follows: the open tail renders and caches like the
    /// stable prefix. Cleared by [`Self::append`].
    finished: bool,
    parse_passes: u64,
    parsed_bytes: u64,
    reference_reparses: u64,
}

/// One top-level block of a [`MarkdownDocument`]: its kind, its source, and
/// its pulldown-cmark events.
#[derive(Clone, Debug)]
pub struct MarkdownBlock {
    kind: MarkdownBlockKind,
    events: Vec<Event<'static>>,
    source: Arc<str>,
    content_hash: u64,
    content_len: usize,
    renderer_cache: RendererCache,
}

impl MarkdownBlock {
    pub const fn kind(&self) -> MarkdownBlockKind {
        self.kind
    }

    /// The block's events, soft breaks already turned into hard breaks in a
    /// document that wants them.
    pub fn events(&self) -> &[Event<'static>] {
        &self.events
    }

    /// The block's source text.
    pub fn source(&self) -> &Arc<str> {
        &self.source
    }

    /// A hash of the block's source, for cache keys; compare the source too.
    pub const fn content_hash(&self) -> u64 {
        self.content_hash
    }

    /// The block's source length in bytes.
    pub const fn content_len(&self) -> usize {
        self.content_len
    }

    /// The renderer's cache of this block, shared by every clone of it.
    pub fn renderer_cache(&self) -> &RendererCache {
        &self.renderer_cache
    }
}

/// A slot where a client keeps what it derives from one block, such as its
/// rendered lines. It holds one value of the type the client first asks
/// for; clones of the block share it.
#[derive(Clone, Default)]
pub struct RendererCache(Arc<OnceLock<Arc<dyn Any + Send + Sync>>>);

impl RendererCache {
    /// The cache, created empty on first use. A slot already holding
    /// another type answers a fresh, unshared value.
    pub fn get_or_default<T: Any + Default + Send + Sync>(&self) -> Arc<T> {
        Arc::clone(
            self.0
                .get_or_init(|| Arc::new(T::default()) as Arc<dyn Any + Send + Sync>),
        )
        .downcast::<T>()
        .unwrap_or_default()
    }
}

impl std::fmt::Debug for RendererCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RendererCache")
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

impl MarkdownDocument {
    pub fn new(source: String) -> Self {
        let mut document = Self {
            source,
            ..Self::default()
        };
        document.reparse_open_tail();
        document
    }

    /// A document whose soft line breaks render as hard breaks, for text a
    /// person typed (user messages) rather than prose a model reflowed.
    pub fn with_hard_breaks(source: String) -> Self {
        let mut document = Self {
            source,
            hard_breaks: true,
            ..Self::default()
        };
        document.reparse_open_tail();
        document
    }

    pub fn append(&mut self, delta: &str) {
        self.source.push_str(delta);
        self.finished = false;
        self.reparse_open_tail();
    }

    /// Mark the document complete, so its last block — never promoted to
    /// the stable prefix, since more text could still extend it — renders
    /// through the stable block and highlight caches instead of being
    /// re-rendered and re-highlighted on every layout.
    #[must_use]
    pub fn finished(mut self) -> Self {
        self.finished = true;
        self
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
            self.apply_parsed(&source, parsed);
            return;
        }
        self.reference_definitions = effective_definitions;
        self.apply_parsed(&tail, parsed);
    }

    fn parse(&mut self, source: &str) -> ParsedDocument {
        self.parse_passes = self.parse_passes.wrapping_add(1);
        self.parsed_bytes = self.parsed_bytes.wrapping_add(source.len() as u64);
        parse_document(source, self.hard_breaks)
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

    fn apply_parsed(&mut self, source: &str, parsed: ParsedDocument) {
        let split = settled_split(source, &parsed.blocks);
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

    /// The settled blocks: no text that follows can change them.
    pub fn stable_blocks(&self) -> &[MarkdownBlock] {
        &self.stable_blocks
    }

    /// The blocks after the stable prefix, which text that follows may still
    /// change unless the document is finished.
    pub fn tail_blocks(&self) -> &[MarkdownBlock] {
        &self.tail_blocks
    }

    /// Whether no more text follows.
    pub const fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Where the settled blocks end in `source`: before the last block, and
/// before every block that runs straight into the one after it. A block
/// settles only once a blank line separates it from what follows, because
/// until then the next line may still belong to it: a table's first body
/// row, half streamed as `|`, parses as a paragraph of its own and would
/// otherwise settle the table without its rows.
fn settled_split(source: &str, blocks: &[ParsedBlock]) -> usize {
    let Some(last) = blocks.last() else {
        return 0;
    };
    let mut split = last.range.start;
    for block in blocks.iter().rev().skip(1) {
        let between = &source[block.range.end..split];
        let separated = between.contains('\n') || source[block.range.clone()].ends_with("\n\n");
        if separated {
            break;
        }
        split = block.range.start;
    }
    split
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

fn parse_document(source: &str, hard_breaks: bool) -> ParsedDocument {
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
        current.push(match event {
            Event::SoftBreak if hard_breaks => Event::HardBreak,
            event => event.into_static(),
        });
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
                    renderer_cache: RendererCache::default(),
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{MarkdownBlockKind, MarkdownDocument};

    #[test]
    fn streamed_text_settles_blocks_and_keeps_the_open_tail_apart() {
        let mut document = MarkdownDocument::new("# Title\n\nfirst".to_owned());
        assert_eq!(document.stable_blocks().len(), 1);
        assert_eq!(
            document.stable_blocks()[0].kind(),
            MarkdownBlockKind::Heading
        );
        assert_eq!(document.tail_blocks().len(), 1);
        document.append(" paragraph\n\n```rust\nfn a() {}\n```\n");
        assert_eq!(
            document
                .blocks()
                .map(super::MarkdownBlock::kind)
                .collect::<Vec<_>>(),
            [
                MarkdownBlockKind::Heading,
                MarkdownBlockKind::Paragraph,
                MarkdownBlockKind::Code,
            ]
        );
        assert!(!document.is_finished());
        assert!(document.clone().finished().is_finished());
    }

    /// Streaming a table one character or one small chunk at a time ends
    /// with the blocks a whole parse gives: no row settles as a paragraph
    /// of its own while the table is still open.
    #[test]
    fn streamed_tables_keep_every_row() {
        let samples = [
            "Here is a table:\n\n| Name | Value |\n|------|-------|\n| a | 1 |\n| b | 2 |\n\nDone.\n",
            "Here is a table:\n| Name | Value |\n|------|-------|\n| a | 1 |\n| b | 2 |\n\nDone.\n",
            "Intro\n\nName | Value\n--- | ---\na | 1\nb | 2\n\nDone.\n",
            "| Name | Value |\n| :--- | ---: |\n| `x` | **1** |\n| - item | 2 |\n",
            "1. Step\n\n| A | B |\n|---|---|\n| 1 | 2 |\n",
            "# Heading\n| A | B |\n| - | - |\n| 1 | 2 |\n- after\n",
        ];
        let blocks = |document: &MarkdownDocument| {
            document
                .blocks()
                .map(|block| (block.kind(), block.source().to_string()))
                .collect::<Vec<_>>()
        };
        for sample in samples {
            let whole = blocks(&MarkdownDocument::new(sample.to_owned()));
            for chunk in [1, 2, 3, 7] {
                let mut streamed = MarkdownDocument::default();
                let characters = sample.chars().collect::<Vec<_>>();
                for piece in characters.chunks(chunk) {
                    streamed.append(&piece.iter().collect::<String>());
                }
                assert_eq!(blocks(&streamed), whole, "{sample:?} in chunks of {chunk}");
            }
        }
    }

    #[test]
    fn clones_of_a_block_share_the_renderer_cache() {
        let document = MarkdownDocument::new("first\n\nsecond".to_owned());
        let block = document.stable_blocks()[0].clone();
        block
            .renderer_cache()
            .get_or_default::<Mutex<Vec<String>>>()
            .lock()
            .unwrap()
            .push("rendered".to_owned());
        let shared = document.stable_blocks()[0]
            .renderer_cache()
            .get_or_default::<Mutex<Vec<String>>>();
        assert_eq!(*shared.lock().unwrap(), ["rendered"]);
        // A renderer asking for another type gets a fresh, unshared value.
        let other = block.renderer_cache().get_or_default::<Mutex<u8>>();
        *other.lock().unwrap() = 7;
        assert_eq!(
            *block
                .renderer_cache()
                .get_or_default::<Mutex<u8>>()
                .lock()
                .unwrap(),
            0
        );
        assert!(Arc::ptr_eq(
            &shared,
            &block
                .renderer_cache()
                .get_or_default::<Mutex<Vec<String>>>()
        ));
    }
}

use std::sync::Arc;

use insta::assert_snapshot;
use ratatui::style::Modifier;
use syntect::{
    highlighting::{
        Color as SyntectColor, FontStyle, Highlighter as SyntectThemeHighlighter, ScopeSelectors,
        StyleModifier, Theme as SyntectTheme, ThemeItem, ThemeSet, ThemeSettings,
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
            "# One `code`\n\n## Two\n\n### Three\n\n#### Four\n\n##### Five\n\n###### Six".into(),
        ),
        &Theme::default(),
        &PlainHighlighter,
        20,
    );
    // One blank row separates each top-level block.
    assert_eq!(
        strings(&heading_lines),
        [
            "One code", "", "Two", "", "Three", "", "Four", "", "Five", "", "Six"
        ]
    );
    let heading_lines = heading_lines
        .into_iter()
        .filter(|line| line.width() > 0)
        .collect::<Vec<_>>();
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
fn code_wraps_by_grapheme_with_preserved_styles_and_blank_continuation_marker() {
    struct SplitHighlighter;

    impl Highlighter for SplitHighlighter {
        fn highlight(
            &self,
            _language: &str,
            _code: &str,
            _theme: &Theme,
        ) -> Arc<[ratatui::text::Line<'static>]> {
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
            .into()
        }
    }

    let lines = super::render_markdown_lines_width(
        &MarkdownDocument::new("```text\nignored\n```".into()),
        &Theme::default(),
        &SplitHighlighter,
        8,
    );
    let code = lines
        .iter()
        .filter(|line| line.kind == super::MarkdownLineKind::Code)
        .collect::<Vec<_>>();
    // No language label: every row pads to the full width, and a wrapped
    // continuation keeps the blank left marker of a one-line block.
    assert_eq!(
        code.iter()
            .map(|line| line.line.to_string())
            .collect::<Vec<_>>(),
        [" abcdefg", " hijkl  "]
    );
    let body = &code;
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
                span.content.contains("def") && span.style.add_modifier.contains(Modifier::ITALIC)
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
            assert!(
                lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .any(|span| { span.content.contains("plain text") && span.style.fg.is_none() })
            );
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
fn stable_markdown_blocks_keep_their_render_cache_while_the_tail_grows() {
    let mut document = MarkdownDocument::new("first\n\nsecond\n\ntail".to_owned());
    let theme = Theme::default();
    let highlighter = PlainHighlighter;
    render_markdown(&document, &theme, &highlighter);
    let cached = document.stable_blocks[0]
        .rendered
        .lock()
        .unwrap()
        .as_ref()
        .expect("stable block render")
        .lines
        .clone();

    document.append(" grows");
    render_markdown(&document, &theme, &highlighter);
    let reused = document.stable_blocks[0]
        .rendered
        .lock()
        .unwrap()
        .as_ref()
        .expect("stable block render")
        .lines
        .clone();

    assert!(Arc::ptr_eq(&cached, &reused));
}

#[test]
fn stable_block_cache_invalidates_for_width_theme_and_source() {
    let document = MarkdownDocument::new("---\n\ntail".to_owned());
    let light = Theme::default();
    let dark = Theme::new(ThemeKind::Dark, ColorLevel::TrueColor);
    let highlighter = PlainHighlighter;

    let light_narrow = super::render_markdown_width(&document, &light, &highlighter, 8);
    let light_wide = super::render_markdown_width(&document, &light, &highlighter, 16);
    let dark_narrow = super::render_markdown_width(&document, &dark, &highlighter, 8);
    let dark_wide = super::render_markdown_width(&document, &dark, &highlighter, 16);

    assert_eq!(light_narrow[0].to_string(), "─".repeat(8));
    assert_eq!(light_wide[0].to_string(), "─".repeat(16));
    assert_eq!(dark_narrow[0].to_string(), "─".repeat(8));
    assert_eq!(dark_wide[0].to_string(), "─".repeat(16));
    assert_ne!(light_narrow[0], dark_narrow[0]);
    assert_ne!(light_wide[0], dark_wide[0]);

    let mut cached = document.stable_blocks[0].rendered.lock().unwrap();
    let entry = cached.as_mut().expect("stable block render");
    entry.source = Arc::from("same-length collision");
    entry.lines = vec![super::MarkdownLine {
        line: ratatui::text::Line::from("wrong cached output"),
        kind: super::MarkdownLineKind::Prose,
    }]
    .into();
    drop(cached);

    let rerendered = super::render_markdown_width(&document, &dark, &highlighter, 16);
    assert_eq!(rerendered[0].to_string(), "─".repeat(16));
}

#[test]
fn unstable_highlighting_resumes_after_the_complete_line_prefix() {
    let theme = Theme::default();
    let highlighter = SyntectHighlighter::default();
    let initial = "fn first() {}\nlet value";
    highlighter.highlight("rust", initial, &theme);
    let before = highlighter.incremental_bytes();
    let appended = "fn first() {}\nlet value = 1;\nnext";
    let incremental = highlighter.highlight("rust", appended, &theme);
    let processed = highlighter.incremental_bytes() - before;
    let fresh = highlighter.highlight_fresh("rust", appended, &theme);

    assert_eq!(incremental, fresh);
    assert_eq!(processed, "let value = 1;\nnext".len());
    assert!(processed < appended.len());
}

#[test]
fn highlight_cache_evicts_least_recent_entries_under_budget_pressure() {
    let sources = [
        ("rust", "fn one() {}"),
        ("json", "{\"two\": 2}"),
        ("bash", "echo three"),
    ];
    let highlighter = SyntectHighlighter::default();
    for (language, source) in sources {
        highlighter.highlight_stable(language, source, &Theme::default());
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
        let (language, source) = sources[index];
        highlighter.highlight_stable(language, source, &Theme::default());
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
        let source = format!("fn unique_{index}() -> usize {{ {index} }}");
        highlighter.highlight_stable("rust", &source, &Theme::default());
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
    cache.insert(
        key.clone(),
        "left",
        vec![ratatui::text::Line::from("left")].into(),
    );

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
    let make_lines = || Arc::from(vec![ratatui::text::Line::from(source)]);
    let lines = make_lines();
    let entry_bytes = super::highlight_entry_bytes(&key, source, &lines);

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
    let mut document =
        MarkdownDocument::new("```rust\nfn stable() {}\n```\n\n```rust\nfn streaming".to_owned());
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
    assert!(highlighter.incremental.lock().unwrap().is_empty());
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
        let fresh = super::render_markdown_width(&document, &Theme::default(), &highlighter, 73);
        let cached = super::render_markdown_width(&document, &Theme::default(), &highlighter, 73);
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
fn bundled_syntect_defaults_cover_the_languages_and_themes_the_tui_highlights() {
    let newlines = SyntaxSet::load_defaults_newlines();
    let nonewlines = SyntaxSet::load_defaults_nonewlines();
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

    let themes = ThemeSet::load_defaults();
    for theme in ["base16-ocean.dark", "base16-eighties.dark"] {
        assert!(
            themes.themes.contains_key(theme),
            "missing built-in theme: {theme}"
        );
    }
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
                .iter()
                .flat_map(|line| &line.spans)
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
            let syntect_style = syntect_highlighter.style_for_stack(&[Scope::new(scope).unwrap()]);
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
        "",
        "1. first",
        "2. second <https://example.test>",
        "",
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

bold and italic, code, link <https://example.test>.

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

 fn main() {} 
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
    let lines = super::render_markdown_width(&document, &Theme::default(), &PlainHighlighter, 80);
    let code_span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains("render()"))
        .expect("inline code span");
    // Inline code keeps its own near-body colour inside a cell, with no
    // tint and never reverse video.
    assert_eq!(code_span.style.bg, None);
    assert_eq!(code_span.style.fg, Theme::default().inline_code().fg);
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
    let source =
        "| description | status |\n|---|---|\n| a very long description that must wrap | done |";
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
    assert!(stacked_joined.contains("descr"), "stacked fallback");
    assert!(stacked_joined.contains("status"));
    assert!(
        stacked.iter().all(|line| !line.contains('┌')),
        "no unusable sliver columns at 9 columns"
    );
    assert!(
        stacked
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 9)
    );

    let wide = "| name | details | state |\n|---|---|---|\n| alpha | a long `piledDynamicModel` value → should wrap — cleanly | ready |\n| beta | another long value that has multiple words | waiting |";
    let wrapped = table_render(wide, 42, &Theme::default());
    assert!(
        wrapped
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 42)
    );
    let border_columns = wrapped
        .iter()
        .filter_map(|line| line.find('│'))
        .collect::<Vec<_>>();
    assert!(
        border_columns
            .windows(2)
            .all(|columns| columns[0] == columns[1])
    );
    let joined = wrapped.join("\n");
    assert!(joined.contains("long") && joined.contains("value"));
    assert!(!joined.contains("amily"));

    // Tiny widths never panic and stay readable.
    for width in 1..10 {
        let tiny = table_render(source, width, &Theme::default());
        assert!(!tiny.is_empty(), "width {width}");
    }

    let empty = "| name | details | state |\n|---|---|---|\n";
    for width in 1..=14 {
        let lines = table_render(empty, width, &Theme::default());
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= usize::from(width)),
            "empty table exceeds width {width}: {lines:?}"
        );
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
    let lines = super::render_markdown_width(&document, &Theme::default(), &PlainHighlighter, 40);
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
fn inline_code_is_marked_by_colour_a_tint_or_backticks() {
    let document = MarkdownDocument::new("before `let x = 1;` after".into());
    for (theme, expected) in [
        // Bakery palettes: the code's own near-body colour, nothing else.
        (Theme::default(), "before let x = 1; after"),
        // A tinted chip widened by half-block caps.
        (
            Theme::new(
                crate::theme::ThemeKind::HighContrast,
                crate::theme::ColorLevel::Ansi16,
            ),
            "before ▐let x = 1;▌ after",
        ),
        // No colour at all: backticks and bold carry it.
        (
            Theme::new(
                crate::theme::ThemeKind::Mono,
                crate::theme::ColorLevel::None,
            ),
            "before `let x = 1;` after",
        ),
    ] {
        let lines = render_markdown(&document, &theme, &PlainHighlighter);
        assert_eq!(strings(&lines).join(""), expected, "{:?}", theme.key());
        let code_span = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("let x = 1;"))
            .expect("inline code span");
        assert!(!code_span.style.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(
            code_span.style.fg,
            theme.inline_code().fg,
            "{:?}",
            theme.key()
        );
        match theme.inline_code_cap() {
            Some(cap) => {
                assert_eq!(cap.fg, code_span.style.bg, "{:?}", theme.key());
                for glyph in ["▐", "▌"] {
                    assert!(
                        lines
                            .iter()
                            .flat_map(|line| line.spans.iter())
                            .any(|span| span.content == glyph && span.style == cap),
                        "{glyph} cap: {:?}",
                        theme.key()
                    );
                }
            }
            None => assert_eq!(code_span.style.bg, None, "{:?}", theme.key()),
        }
        // Never colour alone where there is no colour to carry it.
        assert!(
            code_span.style.add_modifier.contains(Modifier::BOLD)
                || theme.key().colors != crate::theme::ColorLevel::None
        );
    }
}

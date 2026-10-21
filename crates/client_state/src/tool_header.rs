//! How a tool call's header reads in every client: the icon of its hint
//! and the primary argument shown after its title; and which argument
//! blocks an expanded call shows under it, leaving out what the header
//! already says.

use cookie_agent_protocol::{DisplayBlock, Open, ToolHint, ToolPresentation};

use crate::ToolCallState;

/// The icon for a tool hint (spec 12.2). Hints are open labels: one this
/// build does not document, or none, gets the generic icon. The tool's name
/// is never consulted.
pub fn hint_icon(hint: Option<&str>) -> &'static str {
    match hint.unwrap_or(ToolHint::GENERIC) {
        ToolHint::SHELL => "💻",
        ToolHint::FILE_READ => "📖",
        ToolHint::FILE_WRITE | ToolHint::FILE_EDIT => "📝",
        ToolHint::FETCH => "🌐",
        ToolHint::SEARCH => "🔍",
        ToolHint::DELEGATE => "🤖",
        ToolHint::MESSAGE => "📨",
        ToolHint::SKILL => "✨",
        ToolHint::GOAL => "🎯",
        _ => "🔨",
    }
}

/// Flatten an on-wire primary argument for single-line header display. The
/// wire type is byte-capped only, so control characters reach the client:
/// map each to a space before collapsing whitespace runs.
pub fn flatten_header_argument(argument: &str) -> String {
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

/// Characters of an argument a title must show before its `…` for the
/// title to count as naming the argument it cut short.
const MIN_CUT_ARGUMENT_CHARS: usize = 8;

/// Whether `title` already shows `argument`, whole or cut short before a
/// `…`, so the header does not repeat it.
fn title_names_argument(title: &str, argument: &str) -> bool {
    if argument.is_empty() || title.contains(argument) {
        return true;
    }
    title.match_indices('…').any(|(end, _)| {
        let shown = &title[..end];
        shown.char_indices().any(|(start, _)| {
            let prefix = &shown[start..];
            prefix.chars().count() >= MIN_CUT_ARGUMENT_CHARS && argument.starts_with(prefix)
        })
    })
}

/// The texts a header already shows of a call: its primary argument, and
/// what its title quotes between backticks, each trimmed.
fn header_texts(presentation: &ToolPresentation) -> Vec<String> {
    let mut texts = Vec::new();
    if let Some(argument) = &presentation.primary_argument {
        texts.push(argument.as_str().trim().to_owned());
    }
    let title = presentation.title.as_str();
    let mut quoted = title.split('`').skip(1).step_by(2);
    // Only a closed quote counts: `split` yields an unclosed one too.
    if title.matches('`').count() >= 2 {
        texts.extend(quoted.by_ref().map(|text| text.trim().to_owned()));
    }
    texts.retain(|text| !text.is_empty());
    texts
}

/// The argument blocks an expanded call shows: a block whose text is what
/// the header already shows (the primary argument, or the text its title
/// quotes) is left out, and so is a key/value entry whose value is the
/// primary argument, and a key/value block left with no entries. A block
/// that differs, such as a long command the header shortened, stays. The
/// decision reads the presentation only, never the tool's name.
pub fn shown_arguments(presentation: &ToolPresentation) -> Vec<Open<DisplayBlock>> {
    let primary = presentation
        .primary_argument
        .as_ref()
        .map(|argument| argument.as_str());
    let shown = header_texts(presentation);
    let repeats = |text: &str| {
        let text = text.trim();
        shown.iter().any(|shown| shown == text)
    };
    presentation
        .arguments
        .iter()
        .filter_map(|block| match block {
            Open::Known(
                DisplayBlock::Text { text, .. }
                | DisplayBlock::Code { text, .. }
                | DisplayBlock::Markdown { text, .. },
            ) if repeats(text) => None,
            Open::Known(DisplayBlock::KeyValue { entries, .. }) if primary.is_some() => {
                let kept = entries
                    .iter()
                    .filter(|entry| Some(entry.value.as_str()) != primary)
                    .cloned()
                    .collect::<Vec<_>>();
                if kept.is_empty() {
                    return None;
                }
                let mut block = block.clone();
                if let Open::Known(DisplayBlock::KeyValue { entries, .. }) = &mut block {
                    *entries = kept;
                }
                Some(block)
            }
            _ => Some(block.clone()),
        })
        .collect()
}

impl ToolCallState {
    /// The argument blocks its expanded body shows ([`shown_arguments`]).
    pub fn shown_arguments(&self) -> Vec<Open<DisplayBlock>> {
        shown_arguments(&self.presentation)
    }

    /// The primary argument as the header shows it after the title,
    /// flattened to one line; `None` when there is none or the title
    /// already names it.
    pub fn header_argument(&self) -> Option<String> {
        let argument =
            flatten_header_argument(self.presentation.primary_argument.as_ref()?.as_str());
        let title = flatten_header_argument(self.presentation.title.as_str());
        (!title_names_argument(&title, &argument)).then_some(argument)
    }

    /// The header's text: the title, then the argument it does not name.
    pub fn header_text(&self) -> String {
        let title = flatten_header_argument(self.presentation.title.as_str());
        match self.header_argument() {
            Some(argument) => format!("{title} {argument}"),
            None => title,
        }
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        BoundedDisplayText, DisplayBlock, KeyValueEntry, Open, PreparedPresentation,
        SafeDisplayText, ToolIdentity, ToolPresentation,
    };

    use super::{flatten_header_argument, hint_icon, shown_arguments, title_names_argument};

    fn presentation(
        title: &str,
        primary: Option<&str>,
        arguments: Vec<DisplayBlock>,
    ) -> ToolPresentation {
        let prepared = PreparedPresentation::new(
            SafeDisplayText::new(title).expect("title"),
            primary.map(|primary| BoundedDisplayText::new(primary).expect("argument")),
        )
        .with_hint("shell")
        .with_arguments(arguments);
        ToolPresentation::new(
            prepared,
            ToolIdentity {
                name: "x".into(),
                published_name: "x".into(),
                source: None,
            },
        )
    }

    fn code(text: &str) -> DisplayBlock {
        DisplayBlock::Code {
            text: text.into(),
            language: Some("bash".into()),
            start_line: None,
            omitted_lines: None,
        }
    }

    #[test]
    fn a_block_the_header_already_shows_is_left_out() {
        // The title quotes it.
        let quoted = presentation(
            "Run `python3 test_greet.py`",
            None,
            vec![code(" python3 test_greet.py\n")],
        );
        assert!(shown_arguments(&quoted).is_empty());
        // The primary argument is it.
        let primary = presentation(
            "Run a command",
            Some("cargo test"),
            vec![code("cargo test")],
        );
        assert!(shown_arguments(&primary).is_empty());
        // A key/value entry of the primary argument, beside one kept.
        let entries = presentation(
            "Fetch a page",
            Some("https://example.com"),
            vec![DisplayBlock::KeyValue {
                entries: vec![
                    KeyValueEntry {
                        key: "url".into(),
                        value: "https://example.com".into(),
                    },
                    KeyValueEntry {
                        key: "format".into(),
                        value: "markdown".into(),
                    },
                ],
                omitted_entries: None,
            }],
        );
        let shown = shown_arguments(&entries);
        let [Open::Known(DisplayBlock::KeyValue { entries, .. })] = shown.as_slice() else {
            panic!("one key/value block: {shown:?}")
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "format");
    }

    #[test]
    fn a_block_that_says_more_than_the_header_stays() {
        // The header shortened a long command: the block has all of it.
        let long = "for f in *.rs; do\n  rustfmt --check \"$f\"\ndone";
        let shortened = presentation("Run `for f in *.rs; do…`", None, vec![code(long)]);
        assert_eq!(shown_arguments(&shortened).len(), 1);
        let other = presentation("Run `ls`", Some("ls"), vec![code("ls -la")]);
        assert_eq!(shown_arguments(&other).len(), 1);
        // An unclosed backtick quotes nothing.
        let unclosed = presentation("Run `ls", None, vec![code("ls")]);
        assert_eq!(shown_arguments(&unclosed).len(), 1);
    }

    #[test]
    fn a_title_that_names_its_argument_is_not_repeated() {
        assert!(title_names_argument("Run `sleep 8`", "sleep 8"));
        assert!(title_names_argument(
            "Run `printf 'a long…`",
            "printf 'a long command'"
        ));
        assert!(!title_names_argument("Run a command", "sleep 8"));
        assert!(!title_names_argument("Run `pr…`", "printf 'x'"));
        assert_eq!(flatten_header_argument("a\n\tb  c"), "a b c");
    }

    #[test]
    fn icons_follow_the_hint_not_the_tool() {
        assert_eq!(hint_icon(Some("shell")), "💻");
        assert_eq!(hint_icon(Some("bash")), "🔨");
        assert_eq!(hint_icon(None), "🔨");
    }
}

//! How a tool call's header reads in every client: the icon of its hint
//! and the primary argument shown after its title.

use cookie_agent_protocol::ToolHint;

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

impl ToolCallState {
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
    use super::{flatten_header_argument, hint_icon, title_names_argument};

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

//! One label line for every attachment, whatever its source: a user's
//! file, a reply's `<attach/>` card, a call's media, or a file the model
//! produced (`docs/site/specs/attachment-handling.md` section 5.2):
//! `📎 Screenshot.jpg  JPEG image · 671 KB  ↗ open`. Its `↗ open`, shown
//! when the TUI knows the web page, opens the file there.

use cookie_agent_client_state::tool_header::flatten_header_argument;
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// The end of a file's line that opens it in the web page.
pub(super) const OPEN_LINK: &str = "↗ open";

/// What a file's line says.
pub(super) struct FileLabel<'a> {
    pub(super) name: &'a str,
    pub(super) media_type: &'a str,
    /// Its size, when known.
    pub(super) byte_length: Option<u64>,
    /// The model saw it by its name and size alone.
    pub(super) name_only: bool,
    /// What the file shows, as its producer described it.
    pub(super) alt: Option<&'a str>,
    /// The line ends in [`OPEN_LINK`].
    pub(super) link: bool,
}

/// The line of `label`, after `lead` (an indent, or a chevron).
pub(super) fn file_line(lead: &str, label: &FileLabel<'_>, theme: &Theme) -> Line<'static> {
    let mut about = cookie_agent_client_state::media_type_word(label.media_type);
    if let Some(bytes) = label.byte_length {
        about.push_str(" · ");
        about.push_str(&cookie_agent_protocol::human_bytes(bytes));
    }
    if label.name_only {
        about.push_str(" · name only");
    }
    if let Some(alt) = label.alt.filter(|alt| !alt.is_empty()) {
        about.push_str(" — ");
        about.push_str(&flatten_header_argument(alt));
    }
    let mut spans = vec![
        Span::raw(format!("{lead}📎 {}", flatten_header_argument(label.name))),
        Span::styled(format!("  {about}"), theme.muted()),
    ];
    if label.link {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(OPEN_LINK, theme.link()));
    }
    Line::from(spans)
}

/// Where [`OPEN_LINK`] ends `text`, a file's line: its byte offset.
pub(super) fn open_link_at(text: &str) -> Option<usize> {
    Some(text.trim_end().strip_suffix(OPEN_LINK)?.len())
}

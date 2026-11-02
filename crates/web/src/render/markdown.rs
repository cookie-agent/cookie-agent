//! Markdown to HTML from the shared [`MarkdownDocument`]'s events. Raw HTML
//! in the text is escaped and shown as written, never rendered; links go
//! only where [`super::safe_link`] allows and open in a new tab; images are
//! shown as links, so a page never loads a remote image on its own; code
//! blocks carry their language for the highlighter.

use std::sync::{Arc, OnceLock};

use cookie_agent_client_state::markdown::{MarkdownBlock, MarkdownDocument};
use pulldown_cmark::{CowStr, Event, Tag, TagEnd};

use super::{escape, safe_link};

/// Where an `<attach …/>` tag of a reply stands in its HTML, which the
/// transcript replaces with the attachment's card, in the tags' order.
pub const ATTACH_MARKER: &str = "<span data-attach-marker></span>";

/// The document as HTML, raw HTML in it shown as text, tags included.
pub fn document_html(document: &MarkdownDocument) -> String {
    document
        .blocks()
        .map(plain_block_html)
        .collect::<Vec<_>>()
        .concat()
}

/// A block's HTML as a document shows it, kept with the block.
#[derive(Default)]
struct PlainHtml(OnceLock<Arc<str>>);

/// A block's HTML as a reply shows it, each attach tag an
/// [`ATTACH_MARKER`], kept with the block.
#[derive(Default)]
struct ReplyHtml(OnceLock<Arc<str>>);

fn plain_block_html(block: &MarkdownBlock) -> Arc<str> {
    let slot = block.renderer_cache().get_or_default::<PlainHtml>();
    Arc::clone(slot.0.get_or_init(|| {
        let mut html = String::new();
        push_events(&mut html, block.events().iter().cloned(), false);
        Arc::from(html)
    }))
}

/// One block of a reply as HTML, drawn once per block: a block never
/// changes, and a settled one lasts while the text after it streams on.
/// Each `<attach …/>` tag is an [`ATTACH_MARKER`].
pub fn block_html(block: &MarkdownBlock) -> Arc<str> {
    let slot = block.renderer_cache().get_or_default::<ReplyHtml>();
    Arc::clone(slot.0.get_or_init(|| {
        let mut html = String::new();
        push_events(&mut html, block.events().iter().cloned(), true);
        Arc::from(html)
    }))
}

/// `text`, a whole markdown document, as HTML.
pub fn markdown_html(text: &str) -> String {
    document_html(&MarkdownDocument::new(text.to_owned()).finished())
}

fn push_events<'a>(html: &mut String, events: impl Iterator<Item = Event<'a>>, attach: bool) {
    let mut link_closers = Vec::new();
    // A reply's attach tags become markers; the rest of its raw HTML, and
    // all of a document's, shows as text.
    let events = events.flat_map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) if attach && raw.contains("<attach") => {
            cookie_agent_protocol::split_attach_tags(&raw)
                .into_iter()
                .map(|piece| match piece {
                    cookie_agent_protocol::HtmlPiece::Text(text) => {
                        Event::Text(CowStr::from(text.to_owned()))
                    }
                    cookie_agent_protocol::HtmlPiece::Tag(_) => {
                        Event::Html(CowStr::from(ATTACH_MARKER))
                    }
                })
                .collect::<Vec<_>>()
        }
        event => vec![event],
    });
    let events = events.map(|event| match event {
        // Raw HTML from the text shows as text.
        Event::Html(raw) | Event::InlineHtml(raw) if raw.as_ref() != ATTACH_MARKER => {
            Event::Text(raw)
        }
        Event::Start(Tag::Link {
            dest_url, title, ..
        }) => {
            let (open, close) = link_tags(&dest_url, &title);
            link_closers.push(close);
            Event::Html(CowStr::from(open))
        }
        Event::Start(Tag::Image {
            dest_url, title, ..
        }) => {
            let (open, close) = link_tags(&dest_url, &title);
            link_closers.push(close);
            Event::Html(CowStr::from(open))
        }
        Event::End(TagEnd::Link | TagEnd::Image) => {
            Event::Html(CowStr::from(link_closers.pop().unwrap_or_default()))
        }
        event => event,
    });
    pulldown_cmark::html::push_html(html, events);
}

/// The opening and closing tags of a link to `url`: an anchor that opens a
/// new tab without handing it this page, or a plain span for a URL the
/// page does not follow.
fn link_tags(url: &str, title: &str) -> (String, String) {
    if !safe_link(url) || url.is_empty() {
        return (
            "<span class=\"text-muted\">".to_owned(),
            "</span>".to_owned(),
        );
    }
    let title = if title.is_empty() {
        String::new()
    } else {
        format!(" title=\"{}\"", escape(title))
    };
    (
        format!(
            "<a href=\"{}\"{title} target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-accent underline hover:text-fg\">",
            escape(url)
        ),
        "</a>".to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::markdown_html;

    #[test]
    fn markdown_renders_its_structure() {
        let html = markdown_html("# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n");
        assert!(html.contains("<h1>Title</h1>"), "{html}");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(html.contains("<code>code</code>"), "{html}");
        assert!(html.contains("<li>one</li>"), "{html}");
    }

    #[test]
    fn raw_html_is_shown_as_text() {
        let html =
            markdown_html("<script>alert(1)</script>\n\nInline <img src=x onerror=alert(1)> too.");
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
        assert!(
            html.contains("&lt;img src=x onerror=alert(1)&gt;"),
            "{html}"
        );
    }

    #[test]
    fn links_are_neutralized_unless_safe_and_images_become_links() {
        let html =
            markdown_html("[ok](https://example.org/?a=1&b=\"2\") [bad](javascript:alert(1))");
        assert!(
            html.contains("href=\"https://example.org/?a=1&amp;b=&quot;2&quot;\""),
            "{html}"
        );
        assert!(html.contains("rel=\"noopener noreferrer\""), "{html}");
        assert!(!html.contains("javascript:"), "{html}");
        let html = markdown_html("![a cat](https://example.org/cat.png)");
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains(">a cat</a>"), "{html}");
    }

    #[test]
    fn code_blocks_name_their_language_for_the_highlighter() {
        let html = markdown_html("```rust\nfn main() {}\n```\n");
        assert!(
            html.contains("<pre><code class=\"language-rust\">fn main() {}\n</code></pre>"),
            "{html}"
        );
    }
}

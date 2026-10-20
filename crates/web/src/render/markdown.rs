//! Markdown to HTML from the shared [`MarkdownDocument`]'s events. Raw HTML
//! in the text is escaped and shown as written, never rendered; links go
//! only where [`super::safe_link`] allows and open in a new tab; images are
//! shown as links, so a page never loads a remote image on its own; code
//! blocks carry their language for the highlighter.

use cookie_agent_client_state::markdown::MarkdownDocument;
use pulldown_cmark::{CowStr, Event, Tag, TagEnd};

use super::{escape, safe_link};

/// The document as HTML.
pub fn document_html(document: &MarkdownDocument) -> String {
    let mut html = String::new();
    for block in document.blocks() {
        push_events(&mut html, block.events().iter().cloned());
    }
    html
}

/// `text`, a whole markdown document, as HTML.
pub fn markdown_html(text: &str) -> String {
    document_html(&MarkdownDocument::new(text.to_owned()).finished())
}

fn push_events<'a>(html: &mut String, events: impl Iterator<Item = Event<'a>>) {
    let mut link_closers = Vec::new();
    let events = events.map(|event| match event {
        // Raw HTML from the text shows as text.
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
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
            "<span class=\"text-ink dark:text-sand\">".to_owned(),
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
            "<a href=\"{}\"{title} target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-caramel underline hover:text-espresso dark:text-maple dark:hover:text-latte\">",
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

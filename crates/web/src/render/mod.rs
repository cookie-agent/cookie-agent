//! The transcript as HTML: pure functions from the shared client state to
//! markup the page inserts as is. Everything a model, a tool, or a user
//! wrote is escaped here, so no input can add markup of its own; the only
//! markup is what these functions write. Interactive parts carry
//! `data-action` attributes, which one click handler on the transcript
//! dispatches.

pub mod blocks;
pub mod markdown;
pub mod sgr;
pub mod transcript;

/// `text` with the HTML-special characters escaped, for element content and
/// quoted attribute values.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    push_escaped(&mut escaped, text);
    escaped
}

/// Appends `text` to `out` with the HTML-special characters escaped.
pub fn push_escaped(out: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            character => out.push(character),
        }
    }
}

/// Whether a link to `url` may be followed from the page: `http`, `https`,
/// and `mailto` URLs, and relative ones. Anything else, such as
/// `javascript:` or `data:`, renders as plain text.
pub fn safe_link(url: &str) -> bool {
    // Browsers ignore whitespace and control characters inside a scheme.
    let compact = url
        .chars()
        .filter(|character| !character.is_whitespace() && !character.is_control())
        .collect::<String>()
        .to_ascii_lowercase();
    let Some(colon) = compact.find(':') else {
        return true;
    };
    let before = &compact[..colon];
    if before.contains(['/', '?', '#']) {
        return true;
    }
    matches!(before, "http" | "https" | "mailto")
}

#[cfg(test)]
mod tests {
    use super::{escape, safe_link};

    #[test]
    fn markup_characters_are_escaped() {
        assert_eq!(
            escape(r#"<script>alert("x") & 'y'</script>"#),
            "&lt;script&gt;alert(&quot;x&quot;) &amp; &#39;y&#39;&lt;/script&gt;"
        );
    }

    #[test]
    fn only_web_and_mail_links_are_followed() {
        for url in [
            "https://example.org/a",
            "http://example.org",
            "mailto:me@example.org",
            "/relative/path",
            "#anchor",
            "page?x=a:b",
        ] {
            assert!(safe_link(url), "{url}");
        }
        for url in [
            "javascript:alert(1)",
            " JavaScript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<b>x</b>",
            "vbscript:msgbox",
            "file:///etc/passwd",
        ] {
            assert!(!safe_link(url), "{url}");
        }
    }
}

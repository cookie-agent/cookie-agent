//! What a push carries (push notifications spec section 5.2): a small JSON
//! object the service worker turns into a notification.

use aws_lc_rs::digest::{SHA256, digest};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cookie_agent_config::NotificationPreview;
use cookie_agent_engine::{NeedsYouItem, NeedsYouSignal, SettledOutcome, SettledSignal};
use cookie_agent_protocol::SessionId;
use jiff::Timestamp;
use serde::Serialize;

/// The most characters a body keeps of a reply, an error, or a question.
const BODY_CHARS: usize = 160;
/// The most characters a title keeps.
const TITLE_CHARS: usize = 100;
/// The most bytes a payload has, well under one push record.
const MAX_PAYLOAD_BYTES: usize = 3 * 1024;

#[derive(Debug, Serialize)]
pub(super) struct Payload {
    v: u8,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionId>,
    title: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    at: Timestamp,
}

/// `title`, else the session's short id, as a notification names it.
fn session_title(title: Option<&str>, short_id: Option<&str>) -> String {
    match (
        title.map(str::trim).filter(|title| !title.is_empty()),
        short_id,
    ) {
        (Some(title), _) => cut(title, TITLE_CHARS),
        (None, Some(short_id)) => format!("Session {short_id}"),
        (None, None) => "Session".to_owned(),
    }
}

impl Payload {
    pub(super) fn settled(signal: &SettledSignal, preview: NotificationPreview) -> Self {
        let (outcome, body) = match (&signal.outcome, preview) {
            (SettledOutcome::Completed { .. }, NotificationPreview::Status) => {
                ("completed", "Finished.".to_owned())
            }
            (SettledOutcome::Failed { .. }, NotificationPreview::Status) => {
                ("failed", "Failed.".to_owned())
            }
            (SettledOutcome::Completed { final_text }, NotificationPreview::Reply) => (
                "completed",
                final_text
                    .as_deref()
                    .map(|text| flatten(text, BODY_CHARS))
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| "Finished.".to_owned()),
            ),
            (SettledOutcome::Failed { error }, NotificationPreview::Reply) => {
                ("failed", format!("Failed: {}", flatten(error, BODY_CHARS)))
            }
        };
        Self {
            v: 1,
            kind: "settled",
            root: Some(signal.root),
            session: Some(signal.root),
            title: session_title(signal.title.as_deref(), signal.short_id.as_deref()),
            body,
            outcome: Some(outcome),
            at: signal.at,
        }
    }

    /// One push for the `count` items gathered for a tree, `first` named.
    pub(super) fn needs_you(
        first: &NeedsYouSignal,
        count: usize,
        preview: NotificationPreview,
    ) -> Self {
        let (kind, prefix) = match first.item {
            NeedsYouItem::Approval { .. } => ("approval", "Approval"),
            NeedsYouItem::Question { .. } => ("question", "Question"),
        };
        let mut body = match (&first.item, preview) {
            (NeedsYouItem::Approval { .. }, NotificationPreview::Status) => {
                "Approval waiting".to_owned()
            }
            (NeedsYouItem::Question { .. }, NotificationPreview::Status) => {
                "Question waiting".to_owned()
            }
            (NeedsYouItem::Approval { tool, summary, .. }, NotificationPreview::Reply) => {
                match summary
                    .as_deref()
                    .map(|summary| flatten(summary, BODY_CHARS))
                {
                    Some(summary) if !summary.is_empty() => format!("{tool}: {summary}"),
                    _ => tool.clone(),
                }
            }
            (
                NeedsYouItem::Question {
                    agent,
                    message,
                    url,
                },
                NotificationPreview::Reply,
            ) => {
                let message = flatten(message, BODY_CHARS);
                if message.is_empty() {
                    if *url {
                        format!("{agent}: sign-in requested")
                    } else {
                        format!("{agent} asks a question")
                    }
                } else {
                    format!("{agent}: {message}")
                }
            }
        };
        if count > 1 {
            body = format!("{count} waiting · {body}");
        }
        if let Some(handle) = &first.handle {
            body.push_str(&format!(" · in {handle}"));
        }
        if let Some(expires) = first.expires_at() {
            let local = expires.to_zoned(jiff::tz::TimeZone::system());
            body.push_str(&format!(" · expires {}", local.strftime("%H:%M")));
        }
        Self {
            v: 1,
            kind,
            root: Some(first.root),
            session: Some(first.session),
            title: format!(
                "{prefix} · {}",
                session_title(first.root_title.as_deref(), first.root_short_id.as_deref())
            ),
            body,
            outcome: None,
            at: first.at,
        }
    }

    pub(super) fn test() -> Self {
        Self {
            v: 1,
            kind: "test",
            root: None,
            session: None,
            title: "Cookie Agent".to_owned(),
            body: "Notifications from this daemon work.".to_owned(),
            outcome: None,
            at: Timestamp::now(),
        }
    }

    /// The JSON, its body cut until it fits [`MAX_PAYLOAD_BYTES`].
    pub(super) fn into_bytes(mut self) -> Vec<u8> {
        loop {
            let bytes = serde_json::to_vec(&self).expect("the payload serializes");
            if bytes.len() <= MAX_PAYLOAD_BYTES || self.body.is_empty() {
                return bytes;
            }
            let keep = self.body.chars().count() / 2;
            self.body = cut(&self.body, keep);
        }
    }
}

/// The `Topic` of every push for `root`'s tree: 22 base64url characters,
/// so a push service replaces an undelivered message for the same tree.
pub(super) fn topic(root: SessionId) -> String {
    let hash = digest(&SHA256, root.to_string().as_bytes());
    URL_SAFE_NO_PAD.encode(&hash.as_ref()[..16])
}

/// `text` cut to `max` characters, with `…` when cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut kept = text
        .chars()
        .take(max.saturating_sub(1))
        .collect::<String>()
        .trim_end()
        .to_owned();
    kept.push('…');
    kept
}

/// `markdown` as one line of plain text, cut to `max` characters: code
/// fences, heading and quote markers, emphasis, and inline code marks go,
/// links keep their text, and line breaks become spaces.
pub(super) fn flatten(markdown: &str, max: usize) -> String {
    let mut lines = Vec::new();
    for line in markdown.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            continue;
        }
        let line = line.trim_start_matches(['#', '>']).trim_start();
        let line = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
            .unwrap_or(line);
        lines.push(line);
    }
    let joined = lines.join(" ");
    let mut text = String::with_capacity(joined.len());
    let mut rest = joined.as_str();
    while let Some(start) = rest.find('[') {
        text.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find("](").and_then(|close| {
            after[close + 2..]
                .find(')')
                .map(|end| (close, close + 2 + end))
        }) {
            Some((close, end)) => {
                text.push_str(&after[..close]);
                rest = &after[end + 1..];
            }
            None => {
                text.push('[');
                rest = after;
            }
        }
    }
    text.push_str(rest);
    let text = text.replace("**", "").replace("__", "").replace('`', "");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&text, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_flattens_to_one_cut_line() {
        assert_eq!(
            flatten(
                "## Done\n\nThe **test** now waits on `fire.history`; see [the spec](https://x/y).\n```rust\nlet a = 1;\n```\n- one\n- two",
                200
            ),
            "Done The test now waits on fire.history; see the spec. let a = 1; one two"
        );
        assert_eq!(flatten("snake_case stays", 200), "snake_case stays");
        assert_eq!(flatten("a [bracket] alone", 200), "a [bracket] alone");
        assert_eq!(flatten("abcdefghij", 5), "abcd…");
        assert_eq!(flatten("abcde", 5), "abcde");
    }

    #[test]
    fn a_topic_is_22_base64url_characters_per_tree() {
        let root = SessionId::new_v7();
        let topic = topic(root);
        assert_eq!(topic.len(), 22);
        assert!(
            topic
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
        assert_eq!(topic, super::topic(root));
        assert_ne!(topic, super::topic(SessionId::new_v7()));
    }

    #[test]
    fn a_long_payload_is_cut_to_fit() {
        let mut payload = Payload::test();
        payload.body = "x".repeat(10_000);
        assert!(payload.into_bytes().len() <= MAX_PAYLOAD_BYTES);
    }
}

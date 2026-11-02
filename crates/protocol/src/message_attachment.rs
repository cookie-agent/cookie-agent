//! Attachments in assistant messages (`docs/site/specs/message-attachments.md`,
//! client protocol 1.73.0): an agent writes `<attach src="…"/>`,
//! `<attach artifact="…"/>`, or `<attach asset="…"/>` in its reply, and the
//! engine resolves each tag once, recording an `assistant_attachment` event.
//!
//! [`attach_tags`] is the one reading of which tags a text holds, in order,
//! that the engine and every client share: only an HTML event of the reply's
//! Markdown counts, so a tag in code is text.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AssetBlock, AssetScope, PersistedFilePart, SessionId};

pub const ATTACHMENT_DECIDE_METHOD: &str = "attachment.decide";
pub const ATTACHMENT_SAVE_ASSET_METHOD: &str = "attachment.save_asset";

/// The most attachments one message holds; later tags fail.
pub const ATTACH_MAX_PER_MESSAGE: usize = 12;
/// The most characters a tag's title has.
pub const ATTACH_TITLE_MAX_CHARS: usize = 200;

/// The options every reader of a reply's Markdown parses it with, so that
/// all of them see the same HTML events and so the same tags.
#[must_use]
pub fn markdown_options() -> pulldown_cmark::Options {
    pulldown_cmark::Options::ENABLE_STRIKETHROUGH
        | pulldown_cmark::Options::ENABLE_TABLES
        | pulldown_cmark::Options::ENABLE_TASKLISTS
        | pulldown_cmark::Options::ENABLE_GFM
}

/// What a tag attaches.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachSource {
    /// A file on a machine: `src`, with the tag's `machine`.
    Path {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine: Option<String>,
    },
    /// An artifact of the session tree, by its hex SHA-256.
    Artifact { sha256: String },
    /// An asset, by its handle.
    Asset { handle: String },
}

/// One `<attach …/>` tag as written.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct AttachTag {
    pub source: AttachSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl AttachTag {
    /// What the tag names, for a card or a line before it resolves: its
    /// title, else its file's name, its artifact's digest's start, or its
    /// asset's handle.
    #[must_use]
    pub fn label(&self) -> String {
        if let Some(title) = &self.title {
            return title.clone();
        }
        match &self.source {
            AttachSource::Path { path, .. } => path
                .rsplit(['/', '\\'])
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(path)
                .to_owned(),
            AttachSource::Artifact { sha256 } => {
                format!("artifact {}", sha256.get(..12).unwrap_or(sha256))
            }
            AttachSource::Asset { handle } => handle.clone(),
        }
    }
}

/// How a tag resolved.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentOutcome {
    /// The `read` (or `asset`) rule asks: the user's answer decides, with
    /// `attachment.decide`.
    PendingApproval,
    /// A file or an artifact, stored in the session tree's artifacts.
    Attached {
        file: PersistedFilePart,
    },
    /// An asset, as it was when shown; clients show it as it is now.
    Shown {
        asset: AssetBlock,
    },
    Failed {
        reason: String,
    },
}

/// One piece of a stretch of raw HTML: HTML that stays text, or a tag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HtmlPiece<'a> {
    Text(&'a str),
    Tag(AttachTag),
}

/// `raw`, the text of one HTML event, cut into its attach tags and the
/// rest, in order.
#[must_use]
pub fn split_attach_tags(raw: &str) -> Vec<HtmlPiece<'_>> {
    let mut pieces = Vec::new();
    let mut rest = raw;
    let mut text_start = 0usize;
    let mut offset = 0usize;
    while let Some(at) = rest.find("<attach") {
        let start = offset + at;
        match parse_tag(&raw[start..]) {
            Some((tag, length)) => {
                if text_start < start {
                    pieces.push(HtmlPiece::Text(&raw[text_start..start]));
                }
                pieces.push(HtmlPiece::Tag(tag));
                offset = start + length;
                text_start = offset;
            }
            None => offset = start + "<attach".len(),
        }
        rest = &raw[offset..];
    }
    if text_start < raw.len() {
        pieces.push(HtmlPiece::Text(&raw[text_start..]));
    }
    pieces
}

/// Every attach tag of `markdown`, in order.
#[must_use]
pub fn attach_tags(markdown: &str) -> Vec<AttachTag> {
    use pulldown_cmark::{Event, Parser};
    if !markdown.contains("<attach") {
        return Vec::new();
    }
    Parser::new_ext(markdown, markdown_options())
        .filter_map(|event| match event {
            Event::Html(raw) | Event::InlineHtml(raw) => Some(raw),
            _ => None,
        })
        .flat_map(|raw| {
            split_attach_tags(&raw)
                .into_iter()
                .filter_map(|piece| match piece {
                    HtmlPiece::Tag(tag) => Some(tag),
                    HtmlPiece::Text(_) => None,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The tag `text` starts with, and its length: `<attach`, attributes, and
/// `/>`, naming exactly one source.
fn parse_tag(text: &str) -> Option<(AttachTag, usize)> {
    let mut rest = text.strip_prefix("<attach")?;
    if !rest.starts_with(|char: char| char.is_whitespace()) {
        return None;
    }
    let (mut src, mut machine, mut artifact, mut asset, mut title) = (None, None, None, None, None);
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("/>") {
            let length = text.len() - after.len();
            let source = match (src, artifact, asset) {
                (Some(path), None, None) => AttachSource::Path { path, machine },
                (None, Some(sha256), None) if machine.is_none() => {
                    AttachSource::Artifact { sha256 }
                }
                (None, None, Some(handle)) if machine.is_none() => AttachSource::Asset { handle },
                _ => return None,
            };
            let title = title
                .filter(|title: &String| !title.trim().is_empty())
                .map(|title: String| title.chars().take(ATTACH_TITLE_MAX_CHARS).collect());
            return Some((AttachTag { source, title }, length));
        }
        let name_end = rest.find(|char: char| !(char.is_ascii_alphanumeric() || char == '-'))?;
        if name_end == 0 {
            return None;
        }
        let name = &rest[..name_end];
        rest = rest[name_end..]
            .trim_start()
            .strip_prefix('=')?
            .trim_start();
        let quote = rest
            .chars()
            .next()
            .filter(|quote| matches!(quote, '"' | '\''))?;
        let close = rest[1..].find(quote)? + 1;
        let value = unescape(&rest[1..close]);
        rest = &rest[close + 1..];
        if value.trim().is_empty() || value.contains(['\n', '\r']) {
            return None;
        }
        let slot = match name {
            "src" => &mut src,
            "machine" => &mut machine,
            "artifact" => &mut artifact,
            "asset" => &mut asset,
            "title" => &mut title,
            // An attribute the tag does not take is passed over.
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return None;
        }
    }
}

/// `value` with the entities an author may write in an attribute decoded.
fn unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Answers an attachment waiting for the user: `allow` attaches it, else it
/// fails as denied.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentDecideParams {
    pub session_id: SessionId,
    pub model_turn_seq: u64,
    pub content_index: u32,
    pub ordinal: u32,
    pub allow: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AttachmentDecideResult {}

/// Makes an asset of an attached file or artifact.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentSaveAssetParams {
    pub session_id: SessionId,
    pub model_turn_seq: u64,
    pub content_index: u32,
    pub ordinal: u32,
    /// The session tree's own (the default), or the project's.
    #[serde(default)]
    pub scope: AssetScope,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AttachmentSaveAssetResult {
    pub asset: crate::Asset,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(path: &str, machine: Option<&str>) -> AttachSource {
        AttachSource::Path {
            path: path.into(),
            machine: machine.map(Into::into),
        }
    }

    #[test]
    fn a_tag_names_one_source_with_its_machine_and_title() {
        let tags = attach_tags(
            "See:\n\n<attach src=\"runs/loss.png\" machine='labpc' title=\"Loss &amp; accuracy\"/>\n\nand <attach artifact=\"ab12\" /> or <attach asset=\"asset_3f9a1c2e\" extra=\"x\"/>.",
        );
        assert_eq!(
            tags,
            [
                AttachTag {
                    source: path("runs/loss.png", Some("labpc")),
                    title: Some("Loss & accuracy".into()),
                },
                AttachTag {
                    source: AttachSource::Artifact {
                        sha256: "ab12".into()
                    },
                    title: None,
                },
                AttachTag {
                    source: AttachSource::Asset {
                        handle: "asset_3f9a1c2e".into()
                    },
                    title: None,
                },
            ]
        );
    }

    #[test]
    fn code_and_malformed_tags_are_text() {
        for text in [
            "`<attach src=\"a.png\"/>`",
            "```\n<attach src=\"a.png\"/>\n```",
            "<attach src=\"a.png\">",
            "<attach/>",
            "<attachment src=\"a.png\"/>",
            "<attach src=\"a.png\" asset=\"asset_00000000\"/>",
            "<attach artifact=\"ab\" machine=\"labpc\"/>",
            "<attach src=\"\"/>",
            "<attach src=\"a.png\" src=\"b.png\"/>",
        ] {
            assert!(attach_tags(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn raw_html_splits_into_its_tags_and_the_rest() {
        let pieces =
            split_attach_tags("<b>x</b><attach src=\"a.png\"/>  <attach src=\"b.png\"/><i>");
        assert_eq!(pieces.len(), 5);
        assert_eq!(pieces[0], HtmlPiece::Text("<b>x</b>"));
        assert!(matches!(&pieces[1], HtmlPiece::Tag(tag) if tag.source == path("a.png", None)));
        assert_eq!(pieces[2], HtmlPiece::Text("  "));
        assert_eq!(pieces[4], HtmlPiece::Text("<i>"));
    }

    #[test]
    fn a_label_is_the_title_else_what_the_tag_names() {
        let tag = |source, title: Option<&str>| AttachTag {
            source,
            title: title.map(Into::into),
        };
        assert_eq!(tag(path("runs/loss.png", None), None).label(), "loss.png");
        assert_eq!(tag(path("a.png", None), Some("Loss")).label(), "Loss");
        assert_eq!(
            tag(
                AttachSource::Artifact {
                    sha256: "0123456789abcdef".into()
                },
                None
            )
            .label(),
            "artifact 0123456789ab"
        );
    }
}

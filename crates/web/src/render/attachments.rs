//! A reply's attachments as HTML (`docs/site/specs/message-attachments.md`
//! section 6): each `<attach …/>` tag of its text, which the Markdown
//! renderer left as a [`super::markdown::ATTACH_MARKER`], becomes a card
//! where it is, and tags next to each other one row of cards whose widths
//! let them share rows. Pictures and video of a row are one deck, a page
//! and a document cards that open in the side panel, audio a player, any
//! other file a chip with a download. Each card is a placeholder
//! (`data-asset`) the page fills in after drawing it (`crate::assets`), in
//! the way its `data-asset-view` names: an asset's live, an attached file's
//! from the session's artifacts. Handles of shown assets the text names
//! become chips that open their card.

use std::collections::{BTreeMap, HashMap};

use cookie_agent_client_state::{MessageAttachment, assets::AssetKind};
use cookie_agent_protocol::{AssetBlock, AttachmentOutcome, PersistedFileSource};

use super::{escape, markdown::ATTACH_MARKER};
use crate::icons::svg;

const NOTE: &str = "text-xs text-muted";
/// A card's surface.
const SURFACE: &str = "asset-card group relative flex min-w-0 overflow-hidden rounded-xl border border-line bg-surface shadow-sm";
/// A card's corner button, shown while the pointer is over the card (and
/// always on a touch screen).
const CORNER: &str = "absolute top-1.5 right-1.5 z-20 inline-flex min-h-8 min-w-8 items-center justify-center gap-1 rounded-md bg-surface/90 px-1.5 text-xs opacity-0 shadow-sm group-hover:opacity-100 group-focus-within:opacity-100 hover:bg-hover focus-visible:opacity-100 [@media(hover:none)]:opacity-100";
/// A small label of a card's file type.
const PILL: &str =
    "shrink-0 rounded-full border border-line px-1.5 font-mono text-[0.6875rem] text-muted";
const BUTTON: &str = "inline-flex min-h-9 items-center gap-1 rounded-md border border-line-2 px-2.5 text-xs hover:bg-hover active:bg-press";

/// How a card shows its file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Card {
    /// In the deck: an image, an SVG, or a video.
    Deck,
    Page,
    Document,
    Audio,
    File,
}

impl Card {
    #[must_use]
    pub fn of(media_type: &str) -> Self {
        if media_type.starts_with("image/") || media_type.starts_with("video/") {
            Self::Deck
        } else if media_type.starts_with("audio/") {
            Self::Audio
        } else {
            match AssetKind::of(media_type) {
                AssetKind::Page => Self::Page,
                AssetKind::Document => Self::Document,
                _ => Self::File,
            }
        }
    }
}

/// Where a tag stands in its turn: `turn:content:ordinal`, which the
/// page's `attachment.*` requests name it by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttachKey {
    pub model_turn_seq: u64,
    pub content_index: u32,
    pub ordinal: u32,
}

impl AttachKey {
    fn attribute(self) -> String {
        format!(
            "{}:{}:{}",
            self.model_turn_seq, self.content_index, self.ordinal
        )
    }
}

/// A file a card shows: an asset, live, or a file an attachment stored.
#[derive(Clone, Debug)]
pub struct CardFile {
    pub title: String,
    pub media_type: String,
    pub byte_length: u64,
    pub filename: Option<String>,
    source: CardSource,
}

#[derive(Clone, Debug)]
enum CardSource {
    Asset(AssetBlock),
    Artifact { sha256: String, key: AttachKey },
}

impl CardFile {
    fn asset(block: &AssetBlock) -> Self {
        Self {
            title: block.title.clone(),
            media_type: block.media_type.clone(),
            byte_length: block.byte_length,
            filename: block.filename.clone(),
            source: CardSource::Asset(block.clone()),
        }
    }

    /// The attributes every placeholder carries: what it shows, what it
    /// was when shown, and how to fill it.
    fn placeholder(&self, view: &str) -> String {
        let common = format!(
            "data-asset-title=\"{}\" data-asset-type=\"{}\" data-asset-view=\"{view}\"",
            escape(&self.title),
            escape(&self.media_type),
        );
        match &self.source {
            CardSource::Asset(block) => format!(
                "data-asset=\"{}\" data-asset-store=\"{}\" data-asset-sha=\"{}\" data-asset-handle=\"{}\" {common}",
                escape(&block.asset.to_string()),
                escape(&serde_json::to_string(&block.store).unwrap_or_default()),
                escape(&block.sha256),
                escape(block.handle.as_str()),
            ),
            CardSource::Artifact { sha256, key } => format!(
                "data-asset=\"artifact\" data-asset-sha=\"{sha}\" data-artifact-sha=\"{sha}\" data-attach-key=\"{}\" data-asset-name=\"{}\" {common}",
                key.attribute(),
                escape(self.filename.as_deref().unwrap_or(&self.title)),
                sha = escape(sha256),
            ),
        }
    }

    /// The card's corner: *Copy handle* for an asset, *Save as asset* for
    /// a stored file.
    fn corner(&self) -> String {
        match &self.source {
            CardSource::Asset(block) => {
                let handle = escape(block.handle.as_str());
                format!(
                    "<button type=\"button\" data-action=\"asset-copy\" data-asset-handle=\"{handle}\" class=\"{CORNER}\" aria-label=\"Copy the handle {handle}\" title=\"Copy {handle}\">{}</button>",
                    svg(icondata::LuCopy, ""),
                )
            }
            CardSource::Artifact { key, .. } => format!(
                "<button type=\"button\" data-action=\"attachment-save\" data-attach-key=\"{}\" class=\"{CORNER}\" aria-label=\"Save as asset\" title=\"Save as asset\">{}</button>",
                key.attribute(),
                svg(icondata::LuBookmark, ""),
            ),
        }
    }

    /// What the type pill says: the file's extension, else its media
    /// subtype, at most five characters.
    fn type_label(&self) -> String {
        type_label_of(self.filename.as_deref(), &self.media_type)
    }
}

/// What a type pill says of a file named `filename` of `media_type`.
#[must_use]
pub fn type_label_of(filename: Option<&str>, media_type: &str) -> String {
    let extension = filename
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension)
        .filter(|extension| !extension.is_empty() && extension.len() <= 5);
    let label = extension.unwrap_or_else(|| {
        let subtype = media_type.split('/').nth(1).unwrap_or("file");
        subtype
            .split(['+', ';', '.', '-'])
            .next()
            .unwrap_or(subtype)
    });
    label.chars().take(5).collect::<String>().to_uppercase()
}

/// What a tag's card shows now.
#[derive(Clone, Debug)]
enum CardState {
    /// Not resolved yet: the message streams, or the file is read.
    Attaching {
        label: String,
    },
    Asking {
        label: String,
        key: AttachKey,
    },
    Failed {
        label: String,
        reason: String,
    },
    Ready(CardFile),
}

impl CardState {
    fn of(
        attachment: Option<&MessageAttachment>,
        tag: Option<&cookie_agent_protocol::AttachTag>,
        key: Option<AttachKey>,
    ) -> Self {
        let Some(attachment) = attachment else {
            return Self::Attaching {
                label: tag.map_or_else(|| "attachment".to_owned(), |tag| tag.label()),
            };
        };
        let label = attachment.tag.label();
        match (&attachment.outcome, key) {
            (AttachmentOutcome::PendingApproval, Some(key)) => Self::Asking { label, key },
            (AttachmentOutcome::PendingApproval, None) => Self::Attaching { label },
            (AttachmentOutcome::Failed { reason }, _) => Self::Failed {
                label,
                reason: reason.clone(),
            },
            (AttachmentOutcome::Shown { asset }, _) => Self::Ready(CardFile::asset(asset)),
            (AttachmentOutcome::Attached { file }, Some(key)) => match &file.source {
                PersistedFileSource::Artifact {
                    byte_length,
                    sha256,
                    ..
                } => Self::Ready(CardFile {
                    title: label,
                    media_type: file.media_type.as_str().to_owned(),
                    byte_length: *byte_length,
                    filename: file.filename.clone(),
                    source: CardSource::Artifact {
                        sha256: sha256.as_str().to_owned(),
                        key,
                    },
                }),
                _ => Self::Failed {
                    label,
                    reason: "it holds no stored file".to_owned(),
                },
            },
            (AttachmentOutcome::Attached { .. }, None) => Self::Attaching { label },
        }
    }
}

/// The attachments of a text part: by ordinal, and where the part stands.
pub struct PartAttachments<'a> {
    pub attachments: Option<&'a BTreeMap<u32, MessageAttachment>>,
    /// The part's `model_turn_seq` and `content_index`, once committed.
    pub place: Option<(u64, u32)>,
    /// Its tags as written, which name a card before it resolves.
    pub tags: Vec<cookie_agent_protocol::AttachTag>,
}

/// `html`, one block of a reply, with each attach marker a card, markers
/// next to each other one row; `next` is the first marker's ordinal, moved
/// past this block's. `None` when the block has none.
#[must_use]
pub fn with_attachments(html: &str, part: &PartAttachments<'_>, next: &mut u32) -> Option<String> {
    if !html.contains(ATTACH_MARKER) {
        return None;
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find(ATTACH_MARKER) {
        out.push_str(&rest[..at]);
        rest = &rest[at + ATTACH_MARKER.len()..];
        // The markers of a row: only white space or line breaks between.
        let mut states = vec![state(part, next)];
        loop {
            let gap = rest.len() - rest.trim_start().len();
            let after_space = &rest[gap..];
            let after_break = after_space
                .strip_prefix("<br />")
                .map(str::trim_start)
                .unwrap_or(after_space);
            match after_break.strip_prefix(ATTACH_MARKER) {
                Some(after) => {
                    states.push(state(part, next));
                    rest = after;
                }
                None => break,
            }
        }
        out.push_str(&row_html(&states));
    }
    out.push_str(rest);
    Some(out)
}

fn state(part: &PartAttachments<'_>, next: &mut u32) -> CardState {
    let ordinal = *next;
    *next += 1;
    let key = part.place.map(|(model_turn_seq, content_index)| AttachKey {
        model_turn_seq,
        content_index,
        ordinal,
    });
    CardState::of(
        part.attachments
            .and_then(|attachments| attachments.get(&ordinal)),
        part.tags.get(ordinal as usize),
        key,
    )
}

/// A row of cards: one deck where its first picture is, holding every
/// picture, then each other card where it is.
fn row_html(states: &[CardState]) -> String {
    let mut groups: Vec<(Card, Vec<&CardState>)> = Vec::new();
    let mut deck: Option<usize> = None;
    for state in states {
        let card = match state {
            CardState::Ready(file) => Card::of(&file.media_type),
            _ => Card::File,
        };
        match (card, deck) {
            (Card::Deck, Some(at)) => groups[at].1.push(state),
            (Card::Deck, None) => {
                deck = Some(groups.len());
                groups.push((card, vec![state]));
            }
            _ => groups.push((card, vec![state])),
        }
    }
    let mut html = String::from(
        "<span class=\"attach-row my-1 flex flex-wrap items-start gap-3.5\" data-attach-row>",
    );
    for (card, states) in groups {
        let files = states
            .iter()
            .filter_map(|state| match state {
                CardState::Ready(file) => Some(file),
                _ => None,
            })
            .collect::<Vec<_>>();
        let (width, body, kind) = match (card, states.first()) {
            (Card::Deck, _) => (
                if files.len() == 1 {
                    "w-[300px]"
                } else {
                    "w-[220px]"
                },
                deck_html(&files),
                "deck",
            ),
            (Card::Page, _) => ("w-[240px]", page_html(files[0]), "page"),
            (Card::Document, _) => ("w-[240px]", document_html(files[0]), "document"),
            (Card::Audio, _) => ("w-[240px]", audio_html(files[0]), "audio"),
            (Card::File, Some(CardState::Ready(file))) => ("w-[240px]", file_html(file), "file"),
            (Card::File, Some(state)) => ("w-[240px]", waiting_html(state), "waiting"),
            (Card::File, None) => continue,
        };
        html.push_str(&format!(
            "<span class=\"flex max-w-full {width} flex-col gap-1.5\" data-attach-item=\"{kind}\">{body}</span>"
        ));
    }
    html.push_str("</span>");
    html
}

/// A card not ready: attaching, asking the user, or failed.
fn waiting_html(state: &CardState) -> String {
    let (icon, label, line, actions, status) = match state {
        CardState::Attaching { label } => (
            icondata::LuPaperclip,
            label,
            "Attaching…".to_owned(),
            String::new(),
            "attaching",
        ),
        CardState::Asking { label, key } => (
            icondata::LuShieldHalf,
            label,
            "The agent wants to attach this file.".to_owned(),
            format!(
                "<span class=\"flex gap-1.5 pt-1\"><button type=\"button\" class=\"{BUTTON}\" data-action=\"attachment-allow\" data-attach-key=\"{key}\">Allow</button><button type=\"button\" class=\"{BUTTON}\" data-action=\"attachment-deny\" data-attach-key=\"{key}\">Deny</button></span>",
                key = key.attribute(),
            ),
            "asking",
        ),
        CardState::Failed { label, reason } => (
            icondata::LuCircleAlert,
            label,
            reason.clone(),
            String::new(),
            "failed",
        ),
        CardState::Ready(_) => return String::new(),
    };
    format!(
        "<span class=\"{SURFACE} flex-col gap-0.5 px-2.5 py-2\" data-attach-state=\"{status}\"><span class=\"flex min-w-0 items-center gap-2\">{}<span class=\"min-w-0 flex-1 truncate text-sm font-bold\">{}</span></span><span class=\"line-clamp-3 {NOTE}\">{}</span>{actions}</span>",
        svg(icon, "shrink-0 text-accent"),
        escape(label),
        escape(&line),
    )
}

/// The deck: its first three as stacked cards, the first on top, and every
/// one of them filled in, for the media viewer to step through.
fn deck_html(files: &[&CardFile]) -> String {
    let count = files.len();
    let mut cards = String::new();
    // Drawn back to front: the first shown is the last drawn, on top.
    for (index, file) in files.iter().enumerate().rev() {
        let layer = if index < 3 {
            format!(" data-layer=\"{index}\"")
        } else {
            " hidden".to_owned()
        };
        cards.push_str(&format!(
            "<span class=\"asset-deck-card absolute inset-0 flex items-center justify-center overflow-hidden rounded-xl border border-line bg-surface shadow-md\" {} data-index=\"{index}\"{layer}><span class=\"px-3 text-center {NOTE}\" data-asset-body>{}</span></span>",
            file.placeholder("thumb"),
            escape(&file.title),
        ));
    }
    let what = if files
        .iter()
        .all(|file| file.media_type.starts_with("video/"))
    {
        "videos"
    } else {
        "images"
    };
    let count_badge = if count > 1 {
        format!(
            "<span class=\"pointer-events-none absolute right-2 bottom-2 z-10 rounded-full bg-black/60 px-2 text-[0.6875rem] text-white\" data-deck-count>{count} {what}</span>"
        )
    } else {
        String::new()
    };
    let label = match files {
        [file] => format!("Open {}", file.title),
        _ => format!("Open the {count} {what}"),
    };
    let height = if count == 1 { "h-[200px]" } else { "h-[162px]" };
    format!(
        "<span role=\"button\" tabindex=\"0\" data-action=\"asset-deck\" aria-label=\"{}\" title=\"{}\" class=\"asset-deck relative block {height} w-full cursor-zoom-in rounded-xl\" data-deck=\"{count}\">{cards}{count_badge}</span>",
        escape(&label),
        escape(&label),
    )
}

/// A page's card: a live thumbnail over its title, the whole card a button
/// that opens it beside the chat.
fn page_html(file: &CardFile) -> String {
    format!(
        "<span class=\"{SURFACE} flex-col transition-[translate,box-shadow] hover:-translate-y-0.5 hover:shadow-md\" {}><span class=\"relative block h-[132px] overflow-hidden border-b border-line bg-white\" data-asset-body></span>{}<span class=\"px-2.5 pb-2 text-xs text-accent\" data-asset-note>Open beside the chat →</span><button type=\"button\" data-action=\"asset-panel\" class=\"absolute inset-0 z-10 rounded-xl\" aria-label=\"Open {} beside the chat\"></button>{}</span>",
        file.placeholder("page"),
        title_row(file),
        escape(&file.title),
        file.corner(),
    )
}

/// A document's card: its title and first lines.
fn document_html(file: &CardFile) -> String {
    format!(
        "<span class=\"{SURFACE} flex-col transition-[translate,box-shadow] hover:-translate-y-0.5 hover:shadow-md\" {}>{}<span class=\"asset-excerpt relative mx-2.5 mb-2 block h-[84px] overflow-hidden text-xs text-muted\" data-asset-body></span><span class=\"px-2.5 pb-2 text-xs text-accent\" data-asset-note>Read beside the chat →</span><button type=\"button\" data-action=\"asset-panel\" class=\"absolute inset-0 z-10 rounded-xl\" aria-label=\"Read {} beside the chat\"></button>{}</span>",
        file.placeholder("doc"),
        title_row(file),
        escape(&file.title),
        file.corner(),
    )
}

/// An audio file's card: its title over a player.
fn audio_html(file: &CardFile) -> String {
    format!(
        "<span class=\"{SURFACE} flex-col\" {}>{}<span class=\"block px-2.5 pb-2.5\" data-asset-body></span><span class=\"px-2.5 pb-2 empty:hidden {NOTE}\" data-asset-note></span>{}</span>",
        file.placeholder("audio"),
        title_row(file),
        file.corner(),
    )
}

/// Any other file's chip: its type, name, and size, and a download.
fn file_html(file: &CardFile) -> String {
    let label = file.type_label();
    let hue = crate::badges::hue_class(Some(cookie_agent_client_state::badges::badge_hue(&label)));
    let name = file.filename.as_deref().unwrap_or(&file.title);
    format!(
        "<span class=\"{SURFACE} items-center gap-2.5 px-2.5 py-2\" {}><span class=\"grid h-9 w-9 shrink-0 place-items-center rounded-lg border font-mono text-[0.625rem] font-bold {hue}\">{}</span><span class=\"flex min-w-0 flex-1 flex-col\"><span class=\"truncate text-sm\" data-asset-title>{}</span><span class=\"truncate text-[0.6875rem] text-muted\" data-asset-note>{}</span></span><button type=\"button\" data-action=\"asset-download\" class=\"inline-flex size-9 shrink-0 items-center justify-center rounded-md text-accent hover:bg-hover\" aria-label=\"Download {}\" title=\"Download\">{}</button>{}</span>",
        file.placeholder("file"),
        escape(&label),
        escape(name),
        escape(&cookie_agent_protocol::human_bytes(file.byte_length)),
        escape(name),
        svg(icondata::LuDownload, ""),
        file.corner(),
    )
}

/// A card's title line: the kind's icon, the title, and the type pill.
fn title_row(file: &CardFile) -> String {
    format!(
        "<span class=\"flex min-w-0 items-center gap-2 px-2.5 pt-2 pb-1\">{}<span class=\"min-w-0 flex-1 truncate text-sm font-bold\" data-asset-title>{}</span><span class=\"{PILL}\">{}</span></span>",
        svg(
            crate::icons::asset_icon(AssetKind::of(&file.media_type)),
            "shrink-0 text-accent"
        ),
        escape(&file.title),
        escape(&file.type_label()),
    )
}

/// An asset a reply shows, as a chip in the reply's text: its icon and
/// title. Copied, it is its handle, as the text was.
fn chip_html(block: &AssetBlock) -> String {
    let handle = escape(block.handle.as_str());
    format!(
        "<span role=\"button\" tabindex=\"0\" data-action=\"asset-chip\" data-asset-handle=\"{handle}\" title=\"{handle}\" class=\"asset-chip inline-flex max-w-full cursor-pointer items-center gap-1 rounded-full border border-line-2 bg-surface-2 px-2 align-baseline text-[0.8125rem] leading-snug hover:bg-hover\">{}<span class=\"truncate select-none\">{}</span><span class=\"sr-only\">{handle}</span></span>",
        svg(
            crate::icons::asset_icon(AssetKind::of(&block.media_type)),
            "shrink-0 text-accent"
        ),
        escape(&block.title),
    )
}

const HANDLE_PREFIX: &str = "asset_";
const HANDLE_LEN: usize = HANDLE_PREFIX.len() + 8;

/// The handle `text` starts with: `asset_` and eight lowercase hex digits,
/// not followed by more of a word.
fn handle_at(text: &str) -> Option<&str> {
    let handle = text.get(..HANDLE_LEN)?;
    let digits = handle.strip_prefix(HANDLE_PREFIX)?;
    let hex = digits
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    let ends = text[HANDLE_LEN..]
        .chars()
        .next()
        .is_none_or(|next| !next.is_alphanumeric() && next != '_');
    (hex && ends).then_some(handle)
}

/// `html`, a reply's text, with each handle of `known` it names as a chip:
/// in its text, or alone in inline code, but not inside a link or a code
/// block. `None` when it names none.
#[must_use]
pub fn with_chips(html: &str, known: &HashMap<String, AssetBlock>) -> Option<String> {
    if known.is_empty() || !html.contains(HANDLE_PREFIX) {
        return None;
    }
    let mut out = String::with_capacity(html.len());
    let mut changed = false;
    // How deep in `<a>`, `<pre>`, and `<code>` the text is.
    let mut closed = 0usize;
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with('<') {
            // Text is escaped, so `<` always starts a tag.
            if let Some(code) = rest.strip_prefix("<code>")
                && closed == 0
                && let Some(handle) = handle_at(code)
                && code[HANDLE_LEN..].starts_with("</code>")
                && let Some(block) = known.get(handle)
            {
                out.push_str(&chip_html(block));
                rest = &code[HANDLE_LEN + "</code>".len()..];
                changed = true;
                continue;
            }
            let end = rest.find('>').map_or(rest.len(), |end| end + 1);
            let tag = &rest[..end];
            let name = tag
                .trim_start_matches(['<', '/'])
                .split(|char: char| char.is_whitespace() || char == '>')
                .next()
                .unwrap_or("");
            if matches!(name, "a" | "pre" | "code") {
                if tag.starts_with("</") {
                    closed = closed.saturating_sub(1);
                } else {
                    closed += 1;
                }
            }
            out.push_str(tag);
            rest = &rest[end..];
            continue;
        }
        let end = rest.find('<').unwrap_or(rest.len());
        let mut text = &rest[..end];
        rest = &rest[end..];
        if closed > 0 {
            out.push_str(text);
            continue;
        }
        while let Some(at) = text.find(HANDLE_PREFIX) {
            let word_before = text[..at]
                .chars()
                .next_back()
                .is_some_and(|before| before.is_alphanumeric() || before == '_');
            let block = (!word_before)
                .then(|| handle_at(&text[at..]))
                .flatten()
                .and_then(|handle| known.get(handle));
            match block {
                Some(block) => {
                    out.push_str(&text[..at]);
                    out.push_str(&chip_html(block));
                    text = &text[at + HANDLE_LEN..];
                    changed = true;
                }
                None => {
                    out.push_str(&text[..at + HANDLE_PREFIX.len()]);
                    text = &text[at + HANDLE_PREFIX.len()..];
                }
            }
        }
        out.push_str(text);
    }
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AssetHandle, AssetId, AssetStore, AttachSource, AttachTag, MimeType, PersistedFilePart,
        Sha256Digest,
    };

    use super::*;

    fn block(handle: &str, media_type: &str) -> AssetBlock {
        AssetBlock {
            store: AssetStore::Shared,
            asset: AssetId::new_v7(),
            handle: AssetHandle::new(handle).expect("handle"),
            title: format!("{handle} title"),
            media_type: media_type.into(),
            byte_length: 1,
            sha256: "a".repeat(64),
            filename: None,
        }
    }

    fn tag(path: &str) -> AttachTag {
        AttachTag {
            source: AttachSource::Path {
                path: path.into(),
                machine: None,
            },
            title: None,
        }
    }

    fn attached(path: &str, media_type: &str) -> MessageAttachment {
        MessageAttachment {
            tag: tag(path),
            outcome: AttachmentOutcome::Attached {
                file: PersistedFilePart {
                    media_type: MimeType::new(media_type).unwrap(),
                    filename: Some(path.into()),
                    source: PersistedFileSource::Artifact {
                        byte_length: 3,
                        sha256: Sha256Digest::new("b".repeat(64)).unwrap(),
                        reference: cookie_agent_protocol::ArtifactReference {
                            uri: format!("artifact://sha256/{}", "b".repeat(64)),
                        },
                    },
                    metadata: None,
                },
            },
        }
    }

    fn cards(html: &str, attachments: &BTreeMap<u32, MessageAttachment>) -> (String, u32) {
        let part = PartAttachments {
            attachments: Some(attachments),
            place: Some((7, 1)),
            tags: Vec::new(),
        };
        let mut next = 0;
        let out = with_attachments(html, &part, &mut next).expect("markers");
        (out, next)
    }

    #[test]
    fn markers_next_to_each_other_are_one_row_and_pictures_one_deck() {
        let mut attachments = BTreeMap::new();
        attachments.insert(0, attached("a.png", "image/png"));
        attachments.insert(
            1,
            MessageAttachment {
                tag: AttachTag {
                    source: AttachSource::Asset {
                        handle: "asset_3f9a1c2e".into(),
                    },
                    title: None,
                },
                outcome: AttachmentOutcome::Shown {
                    asset: block("asset_3f9a1c2e", "image/png"),
                },
            },
        );
        attachments.insert(2, attached("data.csv", "text/csv"));
        let html = format!(
            "<p>See {ATTACH_MARKER} {ATTACH_MARKER}<br />\n{ATTACH_MARKER} and</p><p>{ATTACH_MARKER}</p>"
        );
        let (out, next) = cards(&html, &attachments);
        assert_eq!(next, 4);
        assert_eq!(out.matches("data-attach-row").count(), 2, "{out}");
        assert_eq!(out.matches("data-deck=\"2\"").count(), 1, "{out}");
        assert_eq!(out.matches("data-asset-view=\"file\"").count(), 1, "{out}");
        // The fourth tag resolved to nothing yet: it attaches.
        assert!(out.contains("data-attach-state=\"attaching\""), "{out}");
        // A stored file offers saving; the deck's asset keeps its handle,
        // which the viewer copies.
        assert!(
            out.contains("data-action=\"attachment-save\" data-attach-key=\"7:1:2\""),
            "{out}"
        );
        assert!(
            out.contains("data-asset-handle=\"asset_3f9a1c2e\""),
            "{out}"
        );
    }

    #[test]
    fn a_tag_that_asks_offers_allow_and_deny_and_a_failed_one_says_why() {
        let mut attachments = BTreeMap::new();
        attachments.insert(
            0,
            MessageAttachment {
                tag: tag("secret.txt"),
                outcome: AttachmentOutcome::PendingApproval,
            },
        );
        attachments.insert(
            1,
            MessageAttachment {
                tag: tag("gone.png"),
                outcome: AttachmentOutcome::Failed {
                    reason: "it does not exist".into(),
                },
            },
        );
        let (out, _) = cards(
            &format!("<p>{ATTACH_MARKER}</p><p>{ATTACH_MARKER}</p>"),
            &attachments,
        );
        assert!(
            out.contains("data-action=\"attachment-allow\" data-attach-key=\"7:1:0\""),
            "{out}"
        );
        assert!(out.contains("data-action=\"attachment-deny\""), "{out}");
        assert!(out.contains("it does not exist"), "{out}");
    }

    #[test]
    fn a_type_label_is_the_extension_else_the_subtype() {
        assert_eq!(type_label_of(Some("runs.csv"), "text/csv"), "CSV");
        assert_eq!(type_label_of(Some("README"), "application/zip"), "ZIP");
        assert_eq!(type_label_of(None, "image/svg+xml"), "SVG");
    }

    #[test]
    fn handles_a_reply_shows_become_chips_outside_links_and_code_blocks() {
        let mut known = HashMap::new();
        known.insert(
            "asset_3f9a1c2e".to_owned(),
            block("asset_3f9a1c2e", "text/html"),
        );
        let chips = |html: &str| with_chips(html, &known);
        assert_eq!(chips("<p>plain</p>"), None);
        assert_eq!(chips("<p>see asset_00000000</p>"), None);
        for html in [
            "<p>see asset_3f9a1c2e.</p>",
            "<p>see <code>asset_3f9a1c2e</code>.</p>",
        ] {
            let shown = chips(html).expect("a chip");
            assert_eq!(shown.matches("data-action=\"asset-chip\"").count(), 1);
            assert!(!shown.contains("<code>"));
        }
        for html in [
            "<p><a href=\"x\">asset_3f9a1c2e</a></p>",
            "<pre><code class=\"language-sh\">asset_3f9a1c2e</code></pre>",
            "<p><code>read asset_3f9a1c2e</code></p>",
            "<p>my_asset_3f9a1c2e asset_3f9a1c2ef</p>",
        ] {
            assert_eq!(chips(html), None, "{html}");
        }
    }
}

//! A run's shelf as HTML (`docs/site/specs/assets.md` section 8.1): its
//! assets as cards in one wrapping row, whose widths let them share rows.
//! Images and video are one deck, a page and a document cards that open in
//! the side panel, audio a player, and any other file a chip with a
//! download. Each card is an asset placeholder (`data-asset`) the page fills
//! in after drawing it (`crate::assets`), in the way its `data-asset-view`
//! names. Handles the reply names become chips that open their card.

use std::collections::HashMap;

use cookie_agent_client_state::assets::{AssetKind, Shelf, ShelfEntry};
use cookie_agent_protocol::AssetBlock;

use super::escape;
use crate::icons::svg;

const NOTE: &str = "text-xs text-ink dark:text-sand";
/// A card's surface.
const SURFACE: &str = "asset-card group relative flex min-w-0 overflow-hidden rounded-xl border border-toasted bg-cream shadow-sm dark:border-ember dark:bg-char";
/// A card's corner button, shown while the pointer is over the card (and
/// always on a touch screen).
const CORNER: &str = "absolute top-1.5 right-1.5 z-20 inline-flex size-8 items-center justify-center rounded-md bg-cream/90 text-xs opacity-0 shadow-sm group-hover:opacity-100 group-focus-within:opacity-100 hover:bg-toasted focus-visible:opacity-100 [@media(hover:none)]:opacity-100 dark:bg-char/90 dark:hover:bg-ember";
/// A small label of a card's file type.
const PILL: &str = "shrink-0 rounded-full border border-toasted px-1.5 font-mono text-[0.6875rem] text-ink dark:border-ember dark:text-sand";

/// How a shelf entry shows.
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

/// What a shelf shows, in order: one deck where its first image or video
/// is, holding every one of them, then each other asset where it is.
#[must_use]
pub fn arrange(shelf: &Shelf) -> Vec<(Card, Vec<&ShelfEntry>)> {
    let mut groups: Vec<(Card, Vec<&ShelfEntry>)> = Vec::new();
    let mut deck: Option<usize> = None;
    for entry in &shelf.entries {
        let card = Card::of(&entry.block.media_type);
        match (card, deck) {
            (Card::Deck, Some(at)) => groups[at].1.push(entry),
            (Card::Deck, None) => {
                deck = Some(groups.len());
                groups.push((card, vec![entry]));
            }
            _ => groups.push((card, vec![entry])),
        }
    }
    groups
}

/// The shelf as HTML.
#[must_use]
pub fn shelf_html(shelf: &Shelf) -> String {
    let mut html = String::from(
        "<div class=\"flex flex-col gap-1.5 pt-1\" data-shelf><div class=\"text-[0.6875rem] font-semibold tracking-wider text-ink uppercase dark:text-sand\">From this reply</div><div class=\"flex flex-wrap items-start gap-3.5\" data-shelf-row>",
    );
    for (card, entries) in arrange(shelf) {
        let caption = entries
            .iter()
            .find_map(|entry| entry.caption.as_deref())
            .map(|caption| {
                format!(
                    "<p class=\"line-clamp-2 {NOTE}\" data-shelf-caption>{}</p>",
                    escape(caption)
                )
            })
            .unwrap_or_default();
        let (width, body) = match card {
            Card::Deck => (
                if entries.len() == 1 {
                    "w-[300px]"
                } else {
                    "w-[220px]"
                },
                deck_html(&entries),
            ),
            Card::Page => ("w-[240px]", page_html(&entries[0].block)),
            Card::Document => ("w-[240px]", document_html(&entries[0].block)),
            Card::Audio => ("w-[240px]", audio_html(&entries[0].block)),
            Card::File => ("w-[240px]", file_html(&entries[0].block)),
        };
        html.push_str(&format!(
            "<div class=\"flex max-w-full {width} flex-col gap-1.5\" data-shelf-item=\"{}\">{body}{caption}</div>",
            match card {
                Card::Deck => "deck",
                Card::Page => "page",
                Card::Document => "document",
                Card::Audio => "audio",
                Card::File => "file",
            }
        ));
    }
    html.push_str("</div></div>");
    html
}

/// The attributes every placeholder carries: which asset, what it was
/// when shown, and how to fill it.
fn placeholder(block: &AssetBlock, view: &str) -> String {
    format!(
        "data-asset=\"{}\" data-asset-store=\"{}\" data-asset-sha=\"{}\" data-asset-handle=\"{}\" data-asset-title=\"{}\" data-asset-type=\"{}\" data-asset-view=\"{view}\"",
        escape(&block.asset.to_string()),
        escape(&serde_json::to_string(&block.store).unwrap_or_default()),
        escape(&block.sha256),
        escape(block.handle.as_str()),
        escape(&block.title),
        escape(&block.media_type),
    )
}

/// The button that copies a card's handle.
fn copy_button(handle: &str) -> String {
    format!(
        "<button type=\"button\" data-action=\"asset-copy\" data-asset-handle=\"{handle}\" class=\"{CORNER}\" aria-label=\"Copy the handle {handle}\" title=\"Copy {handle}\">{}</button>",
        svg(icondata::FaCopyRegular, ""),
        handle = escape(handle),
    )
}

/// What the type pill of `block` says: its file's extension, else its
/// media subtype, at most five characters.
#[must_use]
pub fn type_label(block: &AssetBlock) -> String {
    let extension = block
        .filename
        .as_deref()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension)
        .filter(|extension| !extension.is_empty() && extension.len() <= 5);
    let label = extension.unwrap_or_else(|| {
        let subtype = block.media_type.split('/').nth(1).unwrap_or("file");
        subtype
            .split(['+', ';', '.', '-'])
            .next()
            .unwrap_or(subtype)
    });
    label.chars().take(5).collect::<String>().to_uppercase()
}

/// The deck: its first three as stacked cards, the first on top, and every
/// one of them filled in, for the media viewer to step through.
fn deck_html(entries: &[&ShelfEntry]) -> String {
    let count = entries.len();
    let mut cards = String::new();
    // Drawn back to front: the first shown is the last drawn, on top.
    for (index, entry) in entries.iter().enumerate().rev() {
        let layer = if index < 3 {
            format!(" data-layer=\"{index}\"")
        } else {
            " hidden".to_owned()
        };
        cards.push_str(&format!(
            "<div class=\"asset-deck-card absolute inset-0 flex items-center justify-center overflow-hidden rounded-xl border border-toasted bg-cream shadow-md dark:border-ember dark:bg-char\" {} data-index=\"{index}\"{layer}><span class=\"px-3 text-center {NOTE}\" data-asset-body>{}</span></div>",
            placeholder(&entry.block, "thumb"),
            escape(&entry.block.title),
        ));
    }
    let what = if entries
        .iter()
        .all(|entry| entry.block.media_type.starts_with("video/"))
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
    let label = if count > 1 {
        format!("Open the {count} {what}")
    } else {
        format!("Open {}", entries[0].block.title)
    };
    let height = if count == 1 { "h-[200px]" } else { "h-[162px]" };
    format!(
        "<div role=\"button\" tabindex=\"0\" data-action=\"asset-deck\" aria-label=\"{}\" title=\"{}\" class=\"asset-deck relative {height} w-full cursor-zoom-in rounded-xl\" data-deck=\"{count}\">{cards}{count_badge}</div>",
        escape(&label),
        escape(&label),
    )
}

/// A page's card: a live thumbnail over its title, the whole card a button
/// that opens it beside the chat.
fn page_html(block: &AssetBlock) -> String {
    format!(
        "<div class=\"{SURFACE} flex-col transition-[translate,box-shadow] hover:-translate-y-0.5 hover:shadow-md\" {}><div class=\"relative h-[132px] overflow-hidden border-b border-toasted bg-white dark:border-ember\" data-asset-body></div>{}<span class=\"px-2.5 pb-2 text-xs text-caramel dark:text-maple\" data-asset-note>Open beside the chat →</span><button type=\"button\" data-action=\"asset-panel\" class=\"absolute inset-0 z-10 rounded-xl\" aria-label=\"Open {} beside the chat\"></button>{}</div>",
        placeholder(block, "page"),
        title_row(block),
        escape(&block.title),
        copy_button(block.handle.as_str()),
    )
}

/// A document's card: its title and first lines.
fn document_html(block: &AssetBlock) -> String {
    format!(
        "<div class=\"{SURFACE} flex-col transition-[translate,box-shadow] hover:-translate-y-0.5 hover:shadow-md\" {}>{}<div class=\"asset-excerpt relative mx-2.5 mb-2 h-[84px] overflow-hidden text-xs text-ink dark:text-sand\" data-asset-body></div><span class=\"px-2.5 pb-2 text-xs text-caramel dark:text-maple\" data-asset-note>Read beside the chat →</span><button type=\"button\" data-action=\"asset-panel\" class=\"absolute inset-0 z-10 rounded-xl\" aria-label=\"Read {} beside the chat\"></button>{}</div>",
        placeholder(block, "doc"),
        title_row(block),
        escape(&block.title),
        copy_button(block.handle.as_str()),
    )
}

/// An audio file's card: its title over a player.
fn audio_html(block: &AssetBlock) -> String {
    format!(
        "<div class=\"{SURFACE} flex-col\" {}>{}<div class=\"px-2.5 pb-2.5\" data-asset-body></div><span class=\"px-2.5 pb-2 empty:hidden {NOTE}\" data-asset-note></span>{}</div>",
        placeholder(block, "audio"),
        title_row(block),
        copy_button(block.handle.as_str()),
    )
}

/// Any other file's chip: its type, name, and size, and a download.
fn file_html(block: &AssetBlock) -> String {
    let label = type_label(block);
    let hue = crate::badges::hue_class(Some(cookie_agent_client_state::badges::badge_hue(&label)));
    let name = block.filename.as_deref().unwrap_or(&block.title);
    format!(
        "<div class=\"{SURFACE} items-center gap-2.5 px-2.5 py-2\" {}><span class=\"grid h-9 w-9 shrink-0 place-items-center rounded-lg border font-mono text-[0.625rem] font-bold {hue}\">{}</span><span class=\"flex min-w-0 flex-1 flex-col\"><span class=\"truncate text-sm\" data-asset-title>{}</span><span class=\"truncate text-[0.6875rem] text-ink dark:text-sand\" data-asset-note>{}</span></span><button type=\"button\" data-action=\"asset-download\" class=\"inline-flex size-9 shrink-0 items-center justify-center rounded-md text-caramel hover:bg-toasted dark:text-maple dark:hover:bg-ember\" aria-label=\"Download {}\" title=\"Download\">{}</button>{}</div>",
        placeholder(block, "file"),
        escape(&label),
        escape(name),
        escape(&cookie_agent_protocol::human_bytes(block.byte_length)),
        escape(name),
        svg(icondata::FaDownloadSolid, ""),
        copy_button(block.handle.as_str()),
    )
}

/// A card's title line: the kind's icon, the title, and the type pill.
fn title_row(block: &AssetBlock) -> String {
    format!(
        "<span class=\"flex min-w-0 items-center gap-2 px-2.5 pt-2 pb-1\">{}<span class=\"min-w-0 flex-1 truncate text-sm font-bold\" data-asset-title>{}</span><span class=\"{PILL}\">{}</span></span>",
        svg(
            crate::icons::asset_icon(AssetKind::of(&block.media_type)),
            "shrink-0 text-caramel dark:text-maple"
        ),
        escape(&block.title),
        escape(&type_label(block)),
    )
}

/// An asset a shelf shows, as a chip in the reply's text: its icon and
/// title. Copied, it is its handle, as the text was.
fn chip_html(block: &AssetBlock) -> String {
    let handle = escape(block.handle.as_str());
    format!(
        "<span role=\"button\" tabindex=\"0\" data-action=\"asset-chip\" data-asset-handle=\"{handle}\" title=\"{handle}\" class=\"asset-chip inline-flex max-w-full cursor-pointer items-center gap-1 rounded-full border border-crust bg-parchment px-2 align-baseline text-[0.8125rem] leading-snug hover:bg-toasted dark:border-smoke dark:bg-ember dark:hover:bg-smoke\">{}<span class=\"truncate select-none\">{}</span><span class=\"sr-only\">{handle}</span></span>",
        svg(
            crate::icons::asset_icon(AssetKind::of(&block.media_type)),
            "shrink-0 text-caramel dark:text-maple"
        ),
        escape(&block.title),
    )
}

/// Every asset on a shelf of `shelves`, by handle, as the newest shelf
/// shows it.
#[must_use]
pub fn shelved_handles<'a>(
    shelves: impl IntoIterator<Item = &'a Shelf>,
) -> HashMap<String, AssetBlock> {
    let mut handles = HashMap::new();
    for shelf in shelves {
        for entry in &shelf.entries {
            handles.insert(entry.block.handle.to_string(), entry.block.clone());
        }
    }
    handles
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
    use cookie_agent_client_state::assets::{Shelf, ShelfEntry};
    use cookie_agent_protocol::{AssetBlock, AssetHandle, AssetId, AssetStore, ToolCallId};

    use super::*;

    fn block(handle: &str, media_type: &str, filename: Option<&str>) -> AssetBlock {
        AssetBlock {
            store: AssetStore::Shared,
            asset: AssetId::new_v7(),
            handle: AssetHandle::new(handle).expect("handle"),
            title: format!("{handle} title"),
            media_type: media_type.into(),
            byte_length: 1,
            sha256: "a".repeat(64),
            filename: filename.map(str::to_owned),
        }
    }

    fn entry(block: AssetBlock) -> ShelfEntry {
        ShelfEntry {
            call_id: ToolCallId::new_v7(),
            block,
            caption: None,
        }
    }

    #[test]
    fn a_shelf_puts_every_image_and_video_in_one_deck_where_the_first_was() {
        let shelf = Shelf {
            after: None,
            entries: vec![
                entry(block("asset_00000001", "text/plain", None)),
                entry(block("asset_00000002", "image/png", None)),
                entry(block("asset_00000003", "text/html", None)),
                entry(block("asset_00000004", "video/mp4", None)),
                entry(block("asset_00000005", "text/markdown", None)),
                entry(block("asset_00000006", "audio/mpeg", None)),
            ],
        };
        let arranged = arrange(&shelf)
            .into_iter()
            .map(|(card, entries)| {
                (
                    card,
                    entries
                        .iter()
                        .map(|entry| entry.block.handle.to_string())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            arranged,
            [
                (Card::File, vec!["asset_00000001".to_owned()]),
                (
                    Card::Deck,
                    vec!["asset_00000002".to_owned(), "asset_00000004".to_owned()]
                ),
                (Card::Page, vec!["asset_00000003".to_owned()]),
                (Card::Document, vec!["asset_00000005".to_owned()]),
                (Card::Audio, vec!["asset_00000006".to_owned()]),
            ]
        );
    }

    #[test]
    fn a_type_label_is_the_extension_else_the_subtype() {
        assert_eq!(
            type_label(&block("asset_00000001", "text/csv", Some("runs.csv"))),
            "CSV"
        );
        assert_eq!(
            type_label(&block("asset_00000001", "application/zip", Some("README"))),
            "ZIP"
        );
        assert_eq!(
            type_label(&block("asset_00000001", "image/svg+xml", None)),
            "SVG"
        );
        assert_eq!(
            type_label(&block(
                "asset_00000001",
                "application/octet-stream",
                Some("weights.safetensors")
            )),
            "OCTET"
        );
    }

    #[test]
    fn handles_a_shelf_shows_become_chips_outside_links_and_code_blocks() {
        let known = shelved_handles([&Shelf {
            after: None,
            entries: vec![entry(block("asset_3f9a1c2e", "text/html", None))],
        }]);
        let chips = |html: &str| with_chips(html, &known);
        // Nothing to do: no handle, or one no shelf shows.
        assert_eq!(chips("<p>plain</p>"), None);
        assert_eq!(chips("<p>see asset_00000000</p>"), None);
        // In text and alone in inline code.
        for html in [
            "<p>see asset_3f9a1c2e.</p>",
            "<p>see <code>asset_3f9a1c2e</code>.</p>",
        ] {
            let shown = chips(html).expect("a chip");
            assert_eq!(shown.matches("data-action=\"asset-chip\"").count(), 1);
            assert!(shown.starts_with("<p>see <span"));
            assert!(shown.ends_with("</span>.</p>"));
            assert!(!shown.contains("<code>"));
        }
        // Not in a link, a code block, longer code, or a longer word.
        for html in [
            "<p><a href=\"x\">asset_3f9a1c2e</a></p>",
            "<pre><code class=\"language-sh\">asset_3f9a1c2e</code></pre>",
            "<p><code>read asset_3f9a1c2e</code></p>",
            "<p>my_asset_3f9a1c2e asset_3f9a1c2ef</p>",
        ] {
            assert_eq!(chips(html), None, "{html}");
        }
        // Text after a link or a code block is text again.
        let after = chips("<pre>x</pre><p><a href=\"x\">y</a> asset_3f9a1c2e</p>").expect("a chip");
        assert!(after.contains("</a> <span"));
    }
}

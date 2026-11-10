# Asset thumbnails

Status: **implemented** (2026-11-10; client protocol 1.93.0, and 1.94.0 for
the browser's cache of section 5). User decisions of 2026-11-10. This revises [Opening assets](asset-opening.md) section 3 (the
Assets tab's cards) and section 6.2 (a PDF's thumbnail).

## 1. Problem

The Assets tab drew each card from the asset itself: once a card came within
a screen of the view, the page read the whole file over the connection, a
megabyte of base64 at a time, and drew it small. A video card read the whole
video, a PDF card the whole PDF for pdf.js, and a page card the whole page,
which it then ran in a frame. With a few dozen assets every card is within
that screen, so opening the tab read every asset: on 2026-11-10 that was
38 MB, 17 MB of it one page with its videos inlined. Opening an asset then
waited behind those reads, and read again what was still in flight.

## 2. Decisions

- **The daemon makes thumbnails.** A new method, `asset.thumbnail`, returns
  a small picture of an asset's current content. The tab reads that and
  never a whole file.
- **Pictures** (PNG, JPEG, GIF, WebP) are scaled in the daemon with the
  `image` crate, nothing to install.
- **Videos and PDFs** use `ffmpeg` (the first frame) and `pdftoppm` (the
  first page) when they are on the daemon's `PATH`. Without them, the card
  shows its icon.
- **HTML pages and every other kind** show their icon card: the kind's icon
  and the file's type. A Markdown document's card keeps its first lines,
  read from the first 8 KiB of the file.

## 3. The method

`asset.thumbnail { store, asset }` returns `{ sha256, thumbnail? }`:
`sha256` is the content the answer is about, and `thumbnail` an
`AssetThumbnail` (`media_type`, base64 `data`, `width`, `height`), absent
when there is none. Without one, `retry_later` (client protocol 1.94.0) says
whether asking again later may give one: a missing or slow tool, or content
gone while it was read. Otherwise the content never has one. A thumbnail is a JPEG (quality 80), or a PNG when a pixel
of the picture is not opaque, whose longer side is at most 768 pixels
(`ASSET_THUMBNAIL_MAX_EDGE`), enough for a card on a 2× screen. A smaller
picture keeps its size. A guest of a conversation share may ask for the
assets it may read, as it may `asset.read` them.

## 4. The daemon

- **Kept with the content.** A thumbnail is drawn the first time it is
  asked for and kept as `thumbnails/<sha256>.jpg` (or `.png`) in the
  asset's store. It goes when its blob goes: replaced or deleted content,
  and the store's first open in a process removes thumbnails no content has,
  with half-written ones. A thumbnail is kept only while an asset of the
  store still has its content.
- **Drawn off the async workers, two at a time.** A tab's worth of cards
  asking at once start at most two tools. A request that waited checks
  again for a thumbnail another drew meanwhile.
- **Tools.** `ffmpeg -v error -nostdin -i <blob> -frames:v 1 -vf scale=…
  -c:v png -f image2pipe -` and `pdftoppm -f 1 -l 1 -singlefile -scale-to
  768 -png <blob>`, each given 30 s and killed after. Their PNG goes
  through the same scaling and encoding as a picture.
- **Failures.** Content that does not decode, or that its tool refuses, has
  no thumbnail, and the daemon remembers it (up to 4,096 contents) so it
  does not try again in this process. A missing tool, a timeout, or a blob
  replaced while it was read gives none this time and is tried again on the
  next request.
- Decoding is limited to 16,384 pixels a side and 512 MiB of memory.

## 5. The web client

- A tab card (`data-asset-view="tile"`) starts as its icon card. Once it
  comes within a screen of the view it shows its thumbnail, a PDF's top
  first and a video with a play mark, from wherever it is known first:
  1. **this page**, by content, up to 16 MiB of thumbnails and every
     content without one;
  2. **the browser** (user decision 2026-11-10): the Cache Storage cache
     `cookie-asset-thumbnails-1`, one entry per content `sha256`, the
     thumbnail or an empty entry for content that never has one. A
     thumbnail is named by its content, so an entry never goes stale. It
     holds at most 1,000 entries; past that the oldest tenth goes. Cache
     Storage exists only on HTTPS or `localhost`; elsewhere only the page
     keeps them. The service worker still caches nothing;
  3. **the daemon**, `asset.thumbnail`, whose answer the page and the
     browser then keep, except a `retry_later` miss, which only the page
     keeps until it loads again.

  So a load asks the daemon only about content the browser has not seen.
- A Markdown card reads its first 8 KiB with one `asset.read`; so does a
  Markdown card in the chat.
- The PDF viewer's thumbnail mode, and the page count it showed on a card,
  are gone: the asset page's reader is its only mode.
- Opening an asset reads it whole, as before; nothing else is reading.

## 6. Not done

- Thumbnails for the chat's cards, which still read the asset: a reply
  shows a few, not a list.
- HTML thumbnails. Rendering an agent's page in headless Chrome on the
  daemon's machine runs its scripts there, which the user declined.
- SVG. The `image` crate does not draw it, so an SVG card is an icon.
- A video's length and a PDF's page count on the card.

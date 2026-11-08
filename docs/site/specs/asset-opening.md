# Opening assets: a gallery, the media viewer, and an asset page

Status: **approved** (2026-11-08), being implemented. The user approved the
mockup [`asset-opening/preview.html`](asset-opening/preview.html) and asked
for PDF and office files to be Documents. This revises
[assets](assets.md) section 8.3 (the Assets tab). The shelf, the side panel,
and the sandbox of section 8.2 are unchanged except where section 6 says so.

## 1. Problem

The Assets tab lists rows of text with five buttons each. *Open* shows the
asset under its row, inside the list: a picture squeezed into the column, a
page in a frame as tall as its content up to 80% of the window, under the
rows above it. Nothing steps from one asset to the next, the media viewer
the chat uses is not reachable from the tab, and a PDF or a Word file is
"Other files" with only a download.

## 2. Goals and non-goals

**Goals**

- The tab is a **gallery** of cards with real thumbnails, or a compact
  list, and a click opens the asset (section 3).
- Pictures and videos open in the **media viewer**, which steps through
  every picture and video the tab lists and can rename, share, download,
  and delete (section 4).
- Every other asset opens on an **asset page** that takes the whole page,
  with the page or document filling it (section 5).
- **Documents** are Markdown, PDF, and office files. A PDF is shown, pages
  and all, in the tab and on its page (section 6).

**Non-goals**

- Previews of office files (Word, PowerPoint, Excel, OpenDocument). They
  are Documents and open on an asset page with a download (section 6.3).
- An address for an open asset. Handles are unique only within a store, and
  Back already closes the page (section 5.4).
- Changes to the chat: its cards, side panel, and links stay as they are.
  A PDF on a shelf stays a file chip this round.
- The TUI.

## 3. The Assets tab

```text
[filter] [ Search by title or file name ............ ] [grid|list]
(All) (Images) (Pages) (Documents) (Video and audio) (Other files)
All projects · All levels · 13 assets · 54 MB
┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐
│ thumb  │ │ thumb  │ │ thumb  │ │ thumb  │   ← hover: Copy handle, ⋯
│ title  │ │ title  │ │ title  │ │ title  │
│ PNG · 312 KB · 2h ago            │
│ UCMA · Sweep lr warmup           │
```

- **Kinds** are as before, and Documents grows (section 6.1).
- **A layout switch**, grid or list, beside the search. The browser
  remembers the choice. The grid is the default.
- **A card** (grid): a 4:3 thumbnail; under it the kind's icon, the title,
  and a share mark while a link is live; the type, size, and age; and where
  it lives (the project, and the session for a session's own asset).
  - The thumbnail is what the shelf's card of that kind shows: a picture
    (SVG on white), a video's first frame with its length, a page's live
    thumbnail taking no input, a Markdown document's first lines, a PDF's
    first page with its page count (section 6.2), and for audio, office,
    and other files the kind's icon and the file's extension.
  - **Hover or focus** shows *Copy handle* and **⋯** in the corner. On a
    touch screen **⋯** is always there and *Copy handle* is in the menu.
- **A row** (list): a small thumbnail, the title, where it lives; on a wide
  window the type, size, who saved it, and when; and **⋯**.
- **The menu** (**⋯** or a right-click): *Open*, *Copy handle*, *Rename*,
  *Download*, *Share link…* (when sharing is on), *Open the session* (a
  session's own asset, and the session that saved it otherwise), and
  *Delete…*. Rename and delete open a small dialog: the title to edit, or
  "Delete <title>? Every place it was shown will say it was deleted." The
  row buttons, inline forms, and *Open* under the row are gone.
- **Thumbnails load when they come into view**, not all at once: a card
  reads its asset only once it is within a screen of the visible part of
  the list. A video over 16 MiB shows its icon instead of its first frame,
  so the list never reads large videos to draw it. Pictures and pages over
  the content cache's budget do the same.
- **A click or Enter opens** the asset: a picture or a video in the media
  viewer (section 4), anything else on its asset page (section 5).
- *Show more* (100 at a time), the total, the filter, and the search are
  unchanged.

## 4. The media viewer

The viewer the chat uses, opened from the tab:

- **Its deck** is every picture and video the tab lists now, in the tab's
  order, starting at the one clicked. It steps as it does today: the
  arrows, Left and Right, the filmstrip, and a swipe.
- **It reads an asset when it shows it.** A step to an asset not read yet
  shows "Loading…" until it is; the filmstrip shows the thumbnails already
  read and the kind's icon for the rest.
- **Its bar** gains, for an asset: *Copy handle* (as today), *Share link*
  (when sharing is on), *Download*, and **ⓘ Details**.
- **Details** (`i`) is a pane beside the file, or a sheet from the bottom
  on a narrow window: the title, type and size, pixels (and length), the
  handle, where it lives, who saved it and when (a link to that session),
  and *Rename*, *Open the session*, and *Delete…*. Deleting closes the
  file and shows the next one, or closes the viewer when it was the last.
- Opened from the chat, the viewer shows the same bar and Details for an
  asset; for a stored file (an attachment) there is no handle to copy and
  no Details.

## 5. The asset page

An HTML page, a document, audio, or any other file opens on its own page,
in place of the list:

```text
[←] [icon] PI update 2026-11-03          [Preview|Source] [↑ 1/6 ↓] [⟳][⧉][share][↓][⤢][⋯]
    asset_91fd8f6f · UCMA project · 5d ago
┌──────────────────────────────────────────────────────────────────────────┐
│ the sandboxed page, filling the rest of the window, scrolling inside     │
└──────────────────────────────────────────────────────────────────────────┘
```

### 5.1 Head

- **Back** (←, Escape, or browser Back) returns to the list, scrolled
  where it was.
- **The title** with the kind's icon; under it the handle (selectable),
  where it lives (the session a link to it), and when it was last saved.
- **Preview and Source** for anything with text (HTML, Markdown, and any
  `text/*`, JSON, XML, YAML, or CSV file up to 1 MiB).
- **Up and down** (`k`, `j`) step through the assets on the list that open
  on a page, with "2/6" between them. Hidden on a phone.
- *Reload* (HTML), *Copy handle*, *Share link* (when sharing is on),
  *Download*, *Full screen* (HTML, Markdown, and PDF: the body over the
  whole window, Escape back), and **⋯** with *Rename*, *Open the session*,
  and *Delete…*. Deleting returns to the list.

### 5.2 Body, by kind

| Kind | Body |
|---|---|
| HTML | The sandboxed frame of section 8.2 filling the body; the page scrolls inside it. It does not follow the page's height. |
| Markdown | The whole document, rendered as the side panel renders it, in a column at most 48rem wide. |
| PDF | Its pages (section 6.2). |
| Audio | A player, the title, type, size, and length. |
| Text files | Their source, with *Source* the only tab. |
| Office files and anything else | The kind's icon, the title, type, and size, "No preview for this kind of file.", and *Download*. |

### 5.3 Live

The page follows `assets.changed` as a card does: replaced, it shows the
new content and says it was updated; deleted, it says so and offers only
Back.

### 5.4 Back and history

The asset page is a layer of the Assets view, as an editor is of the
Memory view: it adds a history entry, so browser Back and a phone's Back
gesture close it before the view. It has no address of its own.

## 6. Documents and PDF

### 6.1 Which files are Documents

`AssetKind::Document` holds:

- Markdown (`text/markdown`);
- PDF (`application/pdf`);
- Word, PowerPoint, and Excel files, new and old
  (`application/vnd.openxmlformats-officedocument.*`, `application/msword`,
  `application/vnd.ms-powerpoint`, `application/vnd.ms-excel`);
- OpenDocument text, presentations, and spreadsheets
  (`application/vnd.oasis.opendocument.{text,presentation,spreadsheet}`);
- RTF (`application/rtf`, `text/rtf`) and EPUB (`application/epub+zip`).

CSV, JSON, and plain text stay Other files. The daemon already sniffs these
types or takes them from the file's extension (assets section 7); a type it
names `application/octet-stream` stays Other.

### 6.2 PDF

- **pdf.js** 6.5 (Mozilla, Apache-2.0) draws a PDF. Its two files,
  `pdf.min.mjs` and `pdf.worker.min.mjs` (about 1.7 MB together, 0.5 MB
  gzipped), are vendored under `crates/web/assets/vendor/` with their
  licence, as `highlight.js` is, served beside the page, listed in
  `THIRD-PARTY-NOTICES.txt`, and read only when a PDF is first shown.
- **No character maps or standard fonts** are bundled: a PDF that relies
  on a non-embedded CJK character map or font draws with the browser's
  fonts, or not at all for that text.
- **In a sandbox.** A PDF comes from an agent and may come from the web, so
  it is parsed where the token cannot be reached: in a frame of a new
  `pdf-viewer.html`, sandboxed like `viewer.html` (opaque origin, the same
  policy from the daemon and a `<meta>` tag, but with no network at all:
  `default-src 'none'`, scripts only from `blob:`). The page reads the
  asset and pdf.js's two files over its own origin and posts all three to
  the frame as `Blob`s with a token. The frame imports pdf.js from a
  `blob:` URL, so it needs no network and no CORS. A sandboxed frame
  without its origin may not start workers (Chrome refuses even a `blob:`
  worker), so pdf.js runs its worker's code on the frame's own thread,
  which browsers that isolate sandboxed frames keep off the page's.
- **The page's viewer** in that frame: the pages one under another on a
  grey ground, each drawn when it nears the view, sharp at the screen's
  pixel ratio. A bar on top: the page number of the page in view and the
  count, previous and next page, zoom out, the zoom, zoom in, and *Fit
  width* (the default). Text is selectable (pdf.js's text layer). Links
  open in a new tab. Forms and the PDF's own scripts do not run.
- **The thumbnail:** the same frame, made small, draws the first page only
  and reports the page count back with the token, which the card shows.
- **Messages**, as section 8.2's: the frame takes the PDF only from its
  parent, and the parent takes from the frame only `ready`, the page count,
  and a failure, each with the frame's token. A PDF that fails to open
  shows the error and *Download*.
- **The share page** (share links) is unchanged: a shared PDF is still a
  download there.

### 6.3 Office files

A browser cannot draw Word, PowerPoint, or Excel files. They open on the
asset page with *Download* (section 5.2), and their card shows the kind's
icon in the application's colour with the extension.

## 7. Protocol and compatibility

- No protocol change: the tab already reads with `assets.search`,
  `asset.get`, and `asset.read`. `AssetKind` lives in the client state
  crate.
- The daemon serves `pdf-viewer.html` with the policy it gives
  `viewer.html` (`web_assets`), and the share listener does not serve it.

## 8. Phases

1. **The gallery and opening:** Documents (section 6.1), the grid and
   list, cards with lazy thumbnails, the menu, the rename and delete
   dialogs, the media viewer from the tab with lazy reads, its new bar
   buttons and Details, and the asset page (section 5).
2. **PDF:** pdf.js, `pdf-viewer.html`, the reader and the thumbnail.

Each phase is committed and pushed on its own.

## 9. Decisions

By the user, 2026-11-08:

- The mockup's design: the grid (with a list), the media viewer for
  pictures and videos, and an asset page for the rest.
- PDF, Word, PowerPoint, and similar files are Documents, not Other files.

Made while writing this, for the user to overrule:

- **No `#asset=` address** for the page (the mockup had one): handles
  collide across stores, and Back already works.
- **pdf.js in a sandboxed frame**, rather than the browser's own PDF
  viewer: the browser's viewer shows only the first page on iOS, nothing
  inline on Android, and cannot draw a card's thumbnail.
- **No office previews.** Converting them would need LibreOffice on the
  daemon's machine (`soffice --headless --convert-to pdf`, cached by
  digest), a large dependency for a rare file; it can come later and reuse
  the PDF viewer.
- **The chat is unchanged.** "Open as page" from the side panel, and
  opening a shelf's PDF chip on an asset page, can follow.

## 10. Alternatives considered

- **pdf.js on the page's own origin:** simpler, but a flaw in pdf.js
  would then reach the token. The HTML sandbox exists for the same reason.
- **The browser's PDF viewer in a `blob:` frame:** no vendored code, but
  see section 9; a sandboxed frame cannot use it either.
- **Keeping *Open* under the row** and making it taller: still inside the
  list, still no stepping.

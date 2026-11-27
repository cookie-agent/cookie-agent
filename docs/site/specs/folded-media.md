# Media the agent saw, in folded work

Status: **implemented** (2026-11-27; no protocol change), with the user's
decisions in [section 9](#9-decisions-2026-11-26) and what the
implementation does differently in the
[implementation notes](#implementation-notes). Uses
[attachment handling](attachment-handling.md)'s cards, `artifact.thumbnail`,
and TUI label lines. Builds on
[reduced transcript display](reduced-transcript.md); there is a
[preview](folded-media/preview.html).

## 1. Problem

In the reduced transcript, which is the default in both clients, a reply's
tool calls fold into one `Working…` / `Worked for 1m 23s · 7 tool calls`
row. The pictures and videos the agent looked at while working fold away
with them:

- screenshots it took and then `read`;
- plots and renders it made and read back;
- frames it extracted from a video;
- images an MCP tool returned, such as a browser screenshot;
- images Claude Code's own Read tool showed it.

They are often the most telling part of the work. To see what the agent
saw, the user has to open the fold and scan every tool row for them.

## 2. Goals and non-goals

**Goals:**

- A folded work row shows the pictures, videos, and PDFs its tool calls
  returned to the agent, as a strip of thumbnails under the row. The strip fills in
  live while the agent works.
- One click opens a picture in the media viewer, which steps through the
  strip. A second way leads to the call that returned it.
- The TUI lists them on one line under the row, with `↗ open` links.

**Non-goals:**

- Full transcript mode: tool rows already show their media inline.
- Media the model sends (`MediaFile`) and `<attach/>` cards: these never
  fold (reduced transcript, section 2).
- The user's own attachments: they are in the user message.
- Audio.
- Media a subagent saw: it shows in the subagent's own transcript.

## 3. What counts

A picture or video is **seen** when it is a `DisplayBlock::Media` of a
`Tool` or `CommittedTool` child in the fold's work:

- `read`, `read_artifact`, MCP and plugin tools, and the bridge;
- an ACP agent's own tool calls whose content held an image (stored as a
  `Media` block by the runner);
- delivered in the tool result, or in a user turn after it
  (`DeliverViaUserTurn`); both show a `Media` block on the call's row.

The rules:

- **Kinds.** Media types starting with `image/` or `video/`, and
  `application/pdf`.
- **Calls that fail** add nothing. A call that is still running adds its
  media once the media shows on its row.
- **Repeats.** The same picture (same `sha256`) read twice shows once, at
  its first place. The count still counts it once.
- **Order** is the order of the calls.
- **The model's view.** A picture the model never saw still counts, since
  the user should see what the agent fetched. This happens when
  `fit_media` turned it into a placeholder, or it was over the model's
  limit. Its thumbnail carries a small "not shown to the model" mark with
  the placeholder's reason. ACP agents never hit this case.

`client_state::transcript_mode` works the list out for both clients:
`WorkSummary` gains `seen: Vec<SeenMedia>`, each with the artifact, the
call's tool row ID, its header (`client_state::tool_header`), and whether
the model saw it. It is computed from the same children as the counts, so
a fold of several blocks (one run, one row) lists the whole run's media.

## 4. Web

### 4.1 The strip

```
▸ Worked for 1m 23s · 7 tool calls · 4 images
  ┌────┐ ┌────┐ ┌────┐ ┌────┐
  │ 🖼 │ │ 🖼 │ │ 🖼 │ │ ▶  │
  └────┘ └────┘ └────┘ └────┘
```

- **The label** adds the count after the tool calls: `· 4 images`,
  `· 1 video`, `· 1 PDF`, `· 3 images, 1 video, 1 PDF`. It is left out at
  none.
- **Thumbnails.** The strip sits under the row's label lines, indented
  like them. Thumbnails are 64 px tall, at the picture's own aspect
  ratio, cropped to at most 2:1, with rounded corners and an 8 px gap.
  A video shows its first frame, a play glyph, and its duration once
  known. A PDF shows its first page with a small `PDF` badge, or the PDF
  icon and its name when the daemon has no `pdftoppm`. They come from
  `artifact.thumbnail`, and load only when the row
  is near the viewport (the attachment spec's lazy loading).
- **More than fit.** The strip is one line. When more thumbnails than
  fit, the last slot is a `+5` tile that opens the viewer at the first
  hidden one. On a phone the strip scrolls sideways instead.
- **Click** opens the media viewer at that picture. The viewer steps
  through every picture and video of the strip, not just the visible ones.
  A PDF tile opens the PDF beside the chat instead, as an `<attach/>` PDF
  card does; the viewer skips PDFs.
- **Caption.** The viewer's caption is the call's header
  (`Read runs/loss.png`, `browser_screenshot`), with a **Show call**
  button. Show call closes the viewer, opens the fold, and scrolls to that
  tool row, which flashes once.
- **Hover** (desktop): a thumbnail's tooltip is the call's header.
- **"Not shown to the model"**: a small eye-off badge in the thumbnail's
  corner. Its tooltip gives the placeholder reason, such as "claude-x does
  not accept images in tool results".

### 4.2 While the agent works

- **The working row** shows the strip as well, under its two live lines:
  the newest text and the call running now. A thumbnail joins the strip
  when its call's media arrives. It rises in with the same 280 ms soft
  blur as a new line, and with none under `prefers-reduced-motion`.
- **The newest thumbnail** stays in view. When the strip is full, the
  oldest slide into the `+N` tile, so the row keeps one line and does not
  grow while it works.

### 4.3 When the fold opens

The strip hides, since each tool row then shows its own media. Folding
again brings it back. The count in the label stays.

## 5. TUI

Text only, as decided for attachments (Q5). One line under the row:

```
▸ Worked for 1m 23s · 7 tool calls · 4 images
  🖼 screenshot.png · loss.png · frame_0012.png · demo.mp4  ↗ open
```

- **Names** are the media's filenames. Without a filename, the line uses
  the call's primary argument (`Read runs/loss.png` → `loss.png`), else
  `image 2`. A name is cut with `…` at 24 columns. The line ends with
  `+N` when the rest does not fit.
- **Each name** is a click target (a new `PointerTarget::SeenMedia`, which
  shows a hover). It opens the attachment link of that picture in the web
  page, as `↗ open` does for attachments. Without a page URL, it opens the
  fold at that call instead.
- **`↗ open`** opens the web page at the first picture. It shows only with
  a page URL.
- **A picture the model never saw** gets a trailing `°`, explained in the
  row's tooltip line when hovered.
- **While working**, the line appears under the two live lines once the
  first picture arrives, and updates in place. A new name draws dim for
  one animation step, like a new live line.
- The line hides when the fold opens, as on the web.

## 6. Settings

None. The strip is part of reduced mode. If it turns out to be noisy for
agents that read many images, such as a browser-testing loop, Q2 offers
a cap.

## 7. Phases

1. **`client_state`**: `SeenMedia` and the list in `WorkSummary`, with the
   rules of section 3, and the label count.
2. **Web**: the strip, the viewer stepping through it, Show call, live
   arrival, and hiding while open. Screenshots in light and dark, desktop
   and phone.
3. **TUI**: the line, the click targets, and the live update.

No protocol change. Thumbnails use `artifact.thumbnail`, from the
attachment spec's phase 4.

## 8. Questions

- **Q1. Where the strip sits.** Under the row's label lines (this draft),
  or inline at the end of the label line as small 20 px chips, which keeps
  the row one line taller at most?
- **Q2. A cap.** Show every seen picture (with `+N`), or only the last
  eight, for agents that read dozens of screenshots in a loop?
- **Q3. Pictures the agent made but never read.** A plot the agent wrote to
  disk without reading it is not seen. Should a file the agent wrote that
  is a picture (`write`, or a `bash` command's output path) also count?
  This draft says no: "seen" means it reached the agent.
- **Q4. PDFs.** Leave them out (this draft), or show a first-page
  thumbnail like a picture?

## 9. Decisions (2026-11-26)

- **Q1.** The strip sits under the row's label lines.
- **Q2.** No cap. Every seen picture is listed, with the `+N` tile for those
  that do not fit.
- **Q3.** No. Only media that reached the agent counts.
- **Q4.** Yes, PDFs count. `artifact.thumbnail` draws a PDF's first page
  with `pdftoppm` (the PDF icon without it). A PDF tile opens the document
  beside the chat, and the media viewer skips it.

## Implementation notes

- **Where it lives.** `client_state::transcript_mode` lists the files
  (`SeenMedia`, `WorkSummary::seen`, `seen_label`) as it counts the calls.
  The web draws the strip in `render/transcript.rs` (`seen_strip_html`), a
  reply part of its own beside the work row, so a live row's ticking time
  does not redraw it; `assets.rs` fills each thumbnail as it nears the view
  (`data-asset-view="seen"`), fits the strip to its line (`fit_seen`), opens
  the viewer over it, and runs Show call. The TUI draws the line in
  `ui/transcript/assistant.rs` (`seen_line_layout`), each name and the
  `↗ open` a `BlockId::SeenMedia` region resolved as
  `PointerTarget::SeenMedia`.
- **The model's view cannot be told yet.** `fit_media` and
  `elide_excess_media` fit each request as it is sent and record nothing a
  client reads, and a call whose media the model cannot take fails at the
  call. So `SeenMedia::not_shown` is always `None`: the badge (web) and the
  `°` (TUI) are drawn from it but never show. Showing them needs the engine
  to record what it dropped, a protocol addition. The TUI has no tooltip
  line to explain the `°`.
- **Unnamed files** are named by their kind and place, `image 2`, `video
  1`, `PDF 1`, not `image N` for every kind.
- **The TUI's live line** keeps the newest names, as the web strip does
  (section 4.2): `+N` leads the line while the work goes on, and ends it
  once the work ended.
- **Videos.** The strip shows the daemon's thumbnail with a play glyph, and
  no duration: the thumbnail does not carry it, and reading the video for
  it would undo the lazy thumbnail. The viewer's video bar shows it. A
  video the daemon draws no thumbnail of (no `ffmpeg`) shows a film icon
  rather than being read whole; a PDF without one, its icon and name.
- **The viewer's caption** is a line under the file: the call's header and
  **Show call**; the bar on top keeps the file's name. The viewer reads a
  file the strip showed as a thumbnail, or hid in `+N`, when it comes to it.
- **Show call** flashes the call's row with an accent ring. In the TUI a
  name opens the fold and scrolls its call into view, without a flash.
- **Live updates.** The page's DOM patcher leaves a keyed element marked
  `data-keep` as the page filled it, so a thumbnail does not fill again as
  others join the strip. The plan's settled rows are keyed by each call's
  status and media count too, so media that arrives after a fold settled
  (a call still running when text broke the work) draws.


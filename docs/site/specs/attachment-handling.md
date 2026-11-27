# Attachments: recognition, delivery, and display

Status: **implemented** (2026-11-26; client protocol 1.105.0), with the
user's decisions in [section 10](#10-decisions-2026-11-26) and what the
implementation does differently in the
[implementation notes](#implementation-notes). Builds on
[compatibility v1](compatibility-v1.md) section 11.3 (uploads, media
input), [machines on demand](machines-on-demand.md) section 6.4 (file
labels, `kind: file` uploads), [ACP agents](acp-agents.md) section 15
(input), [message attachments](message-attachments.md) (the `<attach/>`
cards), and [asset thumbnails](asset-thumbnails.md).

## 1. Problem

A user attached a phone screenshot, `Screenshot_20261125_164036_Chrome.jpg`,
to a message for Claude Code (session `01a21459…`, 2026-11-25). The
transcript chip read `application/octet-stream`. The agent received
`[attached file Screenshot_20261125_164036_Chrome.jpg
(application/octet-stream), which this agent cannot read]` and ended up
digging the blob out of the session store with `bash`.

That is three separate faults:

1. **Recognition rejects valid images.** The file is a valid baseline JPEG
   (1968×2184). Samsung adds a 299-byte `SEFT` trailer after the
   end-of-image marker. `jpeg_dimensions`
   (`crates/protocol/src/media_sniff.rs`) walks the segments to EOI and then
   requires `offset == bytes.len()`, so the upload counts as `Malformed`.
   PNG (after `IEND`) and GIF (after the trailer) are just as strict. Many
   phone files carry data after the end marker: Samsung's SEF trailer, Motion
   Photos (an MP4 appended to the JPEG), and Ultra HDR / MPF gain maps (a
   second JPEG after the first EOI). The same check guards tool attachments,
   so `read` of such a file on disk fails too.
2. **The fallback is silent.** The web composer's `assess`
   (`crates/web/src/attach.rs`) turns `Malformed` into `AsFile`. The upload
   goes up as `kind: file`, which is stored as `application/octet-stream`.
   The only sign is a transient status line. The TUI composer does the same.
3. **An artifact sent as a file cannot be recovered.** The model sees only
   its label, and the tools that could fetch it cannot show an image.
   - Native models get `[file NAME, SIZE, artifact 0c697e90]`, which names
     the artifact for `transfer`.
   - `read_artifact` pages any blob as numbered text lines, so an image
     comes back as garbage.
   - Over the ACP bridge the turn context's `input_media` is empty
     (`bridge_turn_context`), so cookie's `read` refuses images. Bridge
     replies are text only (`runner.rs`, the `BridgeCall` answer), so a tool
     result's attachments are dropped.
   - ACP agents get an image block with no label at all
     (`acp/prompt.rs::file_block`). They never learn the artifact's digest,
     so they cannot `<attach artifact=…/>` it back or `transfer` it.

The clients also show attachments three different ways, depending on where
they came from:

| Source | Web | TUI |
|---|---|---|
| User message | The bubble's Markdown carries a `📎 name · type · size` line, **and** a bare `<img>`/player/`<a download>` is drawn below it, so every file shows twice | `📎 name · type · size` |
| `<attach/>` in a reply | A card: gallery row, type pill, size, download, media viewer | ` 📎 label — name (type, size)` plus a `↗` link to the web page |
| Tool result media | A bare `<img>` or player | `📎 name · type · size — alt` |
| Model-produced file | `media_html` | `📎 ▸ type · filename`, expandable |

The web also reads every rendered artifact in full (`Workspace::read_media`
has no cap), including a 20 MiB download nobody clicks. Composer chips show
no thumbnail, and for a `kind: file` upload they show the raw
`application/octet-stream`.

## 2. Goals and non-goals

**Goals:**

- A valid image is recognized as one, whatever follows its end marker.
- A file is never sent as a file without the user seeing it, in the
  composer and in the transcript.
- One label for a user's file, the same for native models and ACP agents.
  It names the file, its type, its size, and its artifact.
- `read_artifact` on an image, PDF, audio, or video artifact returns the
  file itself, as `read` does for a file on disk, when the model takes it.
  This also works over the ACP bridge.
- One attachment card on the web, and one label line in the TUI, for user
  files, `<attach/>` tags, and tool media alike.
- The web never reads a file the user has not asked to see.

**Non-goals:**

- Rewriting stored history. Files already stored as
  `application/octet-stream` stay so. `read_artifact` is how a model gets at
  them.
- Trimming trailers. The bytes are stored and sent as uploaded (question
  Q1).
- Inline images in the terminal (kitty graphics, iTerm2, sixel; question
  Q5).
- New upload limits or new media kinds.

## 3. Recognition

### 3.1 Data after the end marker

`jpeg_dimensions`, `png_dimensions`, and `gif_frame_count` stop at their end
marker (JPEG EOI, PNG `IEND`, the GIF trailer `0x3B`) and accept whatever
follows. The structure before the marker is still checked as now: segments,
the frame header, `IDAT` before `IEND`, a single frame header, and so on.
WebP is unchanged, since RIFF declares its own length.

- **Byte caps** count the whole file. A Motion Photo over the image cap gets
  the existing downscale offer, and re-encoding drops the trailer.
- **Providers.** Decoders stop at the end marker, so Anthropic, OpenAI,
  and Gemini should accept phone photos with trailers. Phase 0 checks this with one
  live request per provider on a cheap model.
- **Where it applies.** The one validator in `cookie_agent_protocol::media_sniff`
  is shared by uploads, the web and TUI composers, tool attachments
  (`read`, MCP results), and `<attach/>`. Fixing it there fixes all of them.

### 3.2 No silent fallback

`Malformed` stops being a reason to send a file as a file. The composer
refuses it instead, with the reason, and offers **Attach as file**.

> Screenshot.jpg starts like a JPEG image but could not be read.
> [Attach as file] [Dismiss]

The other `AsFile` cases stay automatic, because the user cannot do better:
the model takes no files of that kind, or the file is unrecognized. But they
stop being quiet. The draft chip carries a **file** badge whose tooltip (web)
or second line (TUI) gives the reason, such as "deepseek-v4 takes no images:
the model sees its name only".

## 4. Delivery into the conversation

### 4.1 One label

A user's file reaches every model as one text part ahead of its content:

```text
[file Screenshot.jpg · image/jpeg · 671 KB · artifact 0c697e90]
<the image>
```

A file the model does not take inline gets the label alone, with what the
model can do about it:

```text
[file data.zip · application/zip · 3.2 MB · artifact 5f2c81d0 · not shown inline; transfer it to a machine to use it]
[file plot.png · image/png · 210 KB · artifact 7a1e0b44 · not shown inline; read_artifact shows it]
```

The second form is for an artifact `read_artifact` can show (section 4.2),
for example a file stored as `application/octet-stream` before this change
whose content is in fact an image the model takes.

- **Shared renderer.** The label moves from `model_history::file_label` into
  `cookie_agent_protocol::model_text`. Clients then show the exact text the
  model got (section 5.3), since everything cookie adds to model input
  must be visible to the user.
- **ACP agents** get the same label as a text block before each image,
  audio, or resource block. It replaces the "which this agent cannot read"
  note, and it gives them the digest for `<attach artifact=…/>` and
  `transfer`.
- **Cache cost (native).** The label is rendered when each request is built,
  so the new format re-renders past turns too. On the first request after an
  upgrade, a session with files in its history misses the prompt cache once
  from its first file on. On Anthropic models, reasoning after that point is
  also cut by the [frozen-context](frozen-context.md) prefix check (question
  Q3).

### 4.2 `read_artifact` reads media

Before paging, `read_artifact` sniffs the blob's head, as `artifact.read`'s
`content_media_type` does.

| The artifact is | `read_artifact` returns |
|---|---|
| Text (UTF-8) | Numbered lines, as now |
| Image, PDF, audio, or video, and the turn's model takes it (`gate_attachment` with `turn_context.input_media`) | The file as the result's attachment, as `read` returns a media file: a `Media` display block plus the attachment, or a user-turn delivery where the model takes media only there. `offset` and `limit` are refused for it |
| Media the model does not take | An error: "artifact 0c697e90 is an image/jpeg (1968×2184, 671 KB) that claude-x takes no images of; transfer it to a machine to work on it" |
| Other binary | An error naming it as binary, `transfer` suggested, in place of lossy text lines |

No new artifact is retained, since the blob is already in the tree's store.
The description gains one sentence: "An image, PDF, audio, or video artifact
the model takes comes back as the file itself."

### 4.3 Media over the ACP bridge

- **Accepted media.** `bridge_turn_context` stops passing an empty
  `input_media`. It passes the agent's known input media (`KnownAgent.input_media`,
  from its `initialize` prompt capabilities) with delivery `tool_result`.
- **Replies.** Bridge replies carry each attachment of a tool result as an
  MCP content block after the text: `{"type": "image", "data", "mimeType"}`
  or `{"type": "audio", …}`. A PDF or video stays a reference in the text,
  as ACP prompts already treat them.
- **What this enables.** Cookie's `read` of an image file, `read_artifact`
  of an image artifact, and MCP tools whose results hold images all reach
  Claude Code and Codex as images.
- **Phase 0** probes, on a cheap model, that each official agent's MCP
  client hands an image content block to its model: Claude Code, Codex, and
  `agy`. An agent that does not is marked so in its declaration
  (`acp.bridge_media = false`), and it keeps text-only replies.

## 5. Display

### 5.1 Web: one card for every source

User files, tool-result media, and model-produced files render with the
`<attach/>` card code (`web/src/render/attachments.rs`).

- **Pictures and videos** form a gallery row under the message text,
  thumbnails at most 12 rem tall. Clicking one opens the media viewer, which
  steps through every picture and video of that message.
- **Audio** is the existing player card.
- **Other files** are the file chip: type pill, name, size, and a Download
  button. Nothing is read until Download is pressed.
- **Pages and documents** (HTML, Markdown, PDF) open beside the chat, as
  `<attach/>` cards do.
- **The user bubble** drops the `📎` line from its Markdown, so each file
  shows once. `user_content_text` keeps the chip line for copying, search,
  and notifications.
- **A file the model saw only by name** (section 4.1) gets the **file**
  badge on its card. Its tooltip gives the label the model received, word
  for word.

Loading:

- Pictures and videos load when their card comes within one screen of the
  viewport (`IntersectionObserver`), never on render.
- The transcript shows a thumbnail for a picture over 1 MiB. The full bytes
  load only in the viewer. The thumbnail comes from a new `artifact.thumbnail`
  method that reuses the asset thumbnail drawer and its cache (client
  protocol minor, question Q4). Without it, phase 3 ships with lazy loading
  only.

Composer chips:

- A picture's chip shows a 2 rem thumbnail drawn from the local bytes,
  before any upload.
- A `kind: file` chip shows the **file** badge with its reason (section 3.2).
- The type shows as a short word ("JPEG image", "ZIP archive", "file")
  rather than a raw media type. The full media type moves to the tooltip.

### 5.2 TUI: one label line

Every attachment, whatever its source, is one line:

```text
📎 Screenshot.jpg  JPEG image · 671 KB  ↗ open
📎 data.zip  file · 3.2 MB · name only  ↗ open
```

- **`↗ open`** is the existing `attachment_link`
  (`#session=…&attachment=<sha256>`), shown when the daemon has a
  `page_url`. User files and tool media gain it; today only `<attach/>`
  cards have it.
- **`name only`** marks a file the model saw by its label alone. Expanding
  the line shows the label text and the full digest, as the model-produced
  `MediaFile` row does now.
- The draft chips use the same short type words.

### 5.3 What the model got

- **The label.** Both clients show the label of section 4.1 for each user
  file, rendered by `model_text`: the web on hover and in the card's details,
  the TUI on expanding the line.
- **Content.** For a file sent inline, the label is followed by "sent
  inline". For one sent by name only, by the reason. This covers the
  transparency rule for text cookie adds around the user's own files.

## 6. Protocol and compatibility

| Change | Version |
|---|---|
| `media_sniff` accepts data after end markers | None (behaviour) |
| Label format, ACP label blocks, `read_artifact` media, bridge media | None (model input and tool behaviour); bridge media can be turned off per agent with `acp.bridge_media` (config) |
| `artifact.thumbnail` (Q4) and `attachment.delivery` (decision of 2026-11-26) | Client protocol minor (1.105.0) |

History stays readable both ways. No event changes shape (Q2b needs no
field).

## 7. Phases

0. **Probes** (no code). Provider acceptance of JPEG, PNG, and GIF with
   trailers, one request each on a cheap model. ACP agents' MCP clients and
   image content blocks: Claude Code, Codex, `agy`.
1. **Recognition.** End-marker leniency in `media_sniff`. The
   composer's `Malformed` refusal with **Attach as file**, and the **file**
   badge and reason, on web and TUI. This fixes the reported bug.
2. **Delivery.** The shared label, ACP label blocks, `read_artifact` media,
   and bridge `input_media` and media replies.
3. **Display.** Web cards for every source, lazy loading, no eager reads of
   other files, composer thumbnails and type words. TUI label lines with
   links and `name only`. The model-got details.
4. **Thumbnails and real types.** `artifact.thumbnail` (Q4), and stored
   types and per-request delivery (Q2b).

## 8. Alternatives considered

- **Strip trailers on upload.** This keeps provider requests small, but the
  stored file is then not the user's file (the digest differs from the
  file's, and *Save as asset* or a download returns other bytes). It also
  needs a JPEG rewriter. Re-encoding through the downscale offer already
  strips trailers when size matters.
- **Re-sniff old `application/octet-stream` files when building history**
  and send them inline. This repairs old sessions automatically, but changes
  past turns' content (cache and reasoning cuts, as in 4.1) and makes the
  stored record say something other than what was sent. `read_artifact`
  recovers them on demand instead.
- **Let the daemon decide `kind`.** The client would always upload as media
  and let the daemon fall back. The client still has to ask the user
  (section 3.2), and it already holds the model's limits, so this moves code
  without changing what the user sees.

## 9. Questions

- **Q1. Trailers.** Store and send the bytes as uploaded (recommended), or
  strip what follows the end marker?
- **Q2. A file's real type.** `ArtifactRef.media_type` does two jobs today.
  It says what the file is, and `application/octet-stream` also means "sent
  by name only": `model_history::user_content` skips the bytes of an
  octet-stream part, and admission lets it through only because it is no
  media type. So a JPEG sent to a model without images is recorded as
  octet-stream. Its chip says so, the web cannot draw it without sniffing
  it, and it stays name-only even after the session switches to a model that
  takes images. A media part that a later model cannot take works
  differently: `fit_media` turns it into an `[image omitted: …]`
  placeholder for that request only.
  - **Q2a. A flag.** Keep the recognized type and add
    `UserContentPart::File.as_file: true`. The engine sends only the label
    for an `as_file` part. A client or history protocol change, and a part
    stays name-only forever, as now.
  - **Q2b. No flag (recommended).** Recognized media is always stored with
    its own type. Whether a model gets the bytes is decided for each request,
    as for a model switch today: `fit_media` for native models, and the
    agent's prompt capabilities (kind, type, and size) when an ACP prompt is
    sent. Admission stops refusing media the model does not take, since the
    composer's **name only** badge has already told the user.
    `application/octet-stream` then means only "not recognized", which is
    true. No protocol change, and the file comes back inline once the session
    uses a model that takes it. `kind: file` stays for unrecognized content,
    unreadable media the user chose to attach as a file, and text over the
    inline limit. Its stored type is the recognized one, or octet-stream.
- **Q3. Label format change on old turns.** Accept the one-time cache miss
  and reasoning cut in sessions that have files (recommended: it happens once
  per session), or keep the old format for inputs recorded before the change,
  at the cost of two formats in the renderer?
- **Q4. Thumbnails for artifacts.** Add `artifact.thumbnail` so the
  transcript never loads a full-size photo (recommended), or only lazy-load
  full files?
- **Q5. Terminal images.** Leave the TUI text-only with `↗ open`
  (recommended for now), or draw inline previews where the terminal supports
  kitty graphics, iTerm2 images, or sixel?

## 10. Decisions (2026-11-26)

- **Q1.** Bytes are stored and sent as uploaded, trailers included.
- **Q3.** The new label applies to every turn. The one-time cache miss and
  reasoning cut in sessions with files are accepted.
- **Q4.** Add `artifact.thumbnail`.
- **Q5.** The TUI stays text-only, with `↗ open`.
- **Q2.** Q2b, the recommended option: no flag. Recognized media keeps its
  own type, and each request decides whether its model gets the bytes.
- Files already stored as `application/octet-stream` are left as they are.
  The web does not sniff them to draw them as images.
- **A draft file nothing can read** (later on 2026-11-26): a draft file
  whose ending would be `name_only` (not included, and no tool of the
  request helps) is still attachable and sendable, but its chip warns with
  the reason. A new method, `attachment.delivery`, answers the chip: for
  each draft file, the ending the request would give it, the label, and
  why.

## Implementation notes

What the implementation of phase 2 (delivery) does differently from the
sections above, or adds:

- **The label says what the request carries** (section 4.1, revised by the
  user 2026-11-26): `[file NAME attached to this user message · TYPE · SIZE
  · artifact DIGEST8 · ENDING]`, without `NAME ` for an unnamed file. The
  ending is `included below` when the file follows the label in this
  request, else `not included; read_artifact returns it` (UTF-8 text, or
  media whose type and size the model takes from a tool per
  `gate_attachment`, a `file` upload's content sniffed as `artifact.read`
  does), `not included; use transfer to copy it to a machine you work on`
  (the request offers `transfer`, at once or deferred, and a copy exists),
  or `not included; only its name is available`. `model_text::
  user_file_label(artifact, FileLabelEnding)` renders it. It replaces the
  two `not shown inline; …` forms.
- **Where the ending is decided** (native): history assembly renders each
  label as though the request carried what follows it and records the
  file's facts by the label's head; `media::settle_file_labels` ends every
  label after `fit_media` and `elide_excess_media`, at each place a request
  is fitted (the attempt, after plugins, and a native compaction request).
  A file `fit_media` drops no longer joins its reason into the label: the
  `[image omitted: …]` placeholder with the reason follows the label.
- **ACP labels**: the ending follows the agent's prompt capabilities for
  what the prompt carries, the bridge's tool media (images and audio its
  prompts take) for `read_artifact`, and the session's latest recorded
  bridge tool list for `read_artifact` and `transfer`. A stored copy that is
  gone adds the `[name (type) is unavailable: …]` note native models get.
- **`read_artifact` and `read` share one result builder**
  (`runtime_api::media::media_file_result`), the same gate for all four
  kinds (including delivery in the user message after the result), and
  the same caps (`max_tool_attachment_bytes`: 25 MiB for video, 20 MiB
  otherwise). Its refusals name the kind's command-line tools (`ffmpeg` and
  `ffprobe`, `pdftotext` or `pdftoppm`, ImageMagick `convert`; `file` or
  `xxd` for other binary) and name `transfer` only when the session's
  latest request offered it; an image's dimensions come from reading the
  blob when it is within the attachment cap.
- **Accepted bridge media** (section 4.3) come from the live agent process's
  prompt capabilities, the source `KnownAgent.input_media` is recorded
  from, limited to images and audio. A PDF or video attachment a bridged
  result still carries goes as an embedded `resource` block with `blob`
  and `mimeType`.
- **No `acp.bridge_media`** (section 4.3): bridge replies carry media
  blocks to every agent; no declaration key turns them off.

Phase 4 (thumbnails and real types, client protocol 1.105.0):

- **Q2b in the clients.** The web's `assess` and the TUI's upload upload
  recognized media (and UTF-8 text within the 1 MiB text cap) the model
  does not take as `kind: media`, with its own type, and the status line
  says the model sees its name only. The downscale offer stays for an image
  too large for the model; **Dismiss** now attaches it as it is (as a file
  when the daemon's own limits refuse it as media). `kind: file` stays for
  unrecognized content, text over the cap, and an unreadable file the user
  attaches as a file.
- **Admission** (`admit_input_media`) checks only that each artifact is in
  the store and, for a recognized media type or `text/plain`, that its
  content is that media within the daemon-wide upload limits. It no longer
  takes the model: `media_unsupported`, `media_limit_exceeded`, and the
  model's `media_too_large` are no longer sent for an input (the registry
  keeps the codes, marked so).
- **Fitting.** `fit_media` also drops a user file of no media kind (text)
  that the model lists no `text` entry for, which is every native model:
  `[text omitted: … does not accept text files (…)]` follows its label, which
  ends `not included; read_artifact returns it`. `elide_excess_media` no
  longer fails a request whose latest message alone is over a kind's
  `max_count`: every older part of the kind goes, and the message keeps its
  first parts up to the count. Per-model image dimensions need no check of
  their own: every model's limit is the daemon-wide one uploads enforce.
- **ACP prompts** check a file's size as well as its kind and type against
  the agent's prompt capabilities (`prompt_misfit`, the same fit as a
  model's). The bridge tools a label's ending counts are the session's
  latest recorded list; before one is recorded, what the run's policy
  bridges.
- **`attachment.delivery`** (`runtime/artifacts/delivery.rs`) freezes the
  draft's selection as a run start does (root or delegated, the fallback
  the session reached, the ACP agent placed on its runtime), and decides
  each file with the shared code: `user_files_fit` (the misfit reason
  `fit_media` gives, and the latest message's count rule) and
  `missing_file_ending` with the tools `compose_for_policy` offers, at once
  or deferred; an ACP agent's caps come from what it last advertised on
  its runtime (`KnownAgent.input_media`). A `name_only` reason adds `no
  machine is attached to copy it to` when no attached machine takes files,
  else `transfer is off in this session`. The web asks 150 ms after the
  draft's files, model or agent, or the session's runtimes change; the TUI
  asks when that key changes as it draws. A new session's draft, and a
  daemon before 1.105.0, keep the client-side check (`name_only_reason`).
  The ending is an open enum (`unknown` for a newer one).
- **Chips.** `included` has no badge; `read_artifact` and `transfer` the
  neutral **name only**; `name_only` a warning **model can't read it**, its
  tooltip the reason and the label. TUI chips end ` · name only` or ` ·
  can't read`, and the status line gives the reason of a file just attached
  once the answer comes.
- **`artifact.thumbnail`** draws with the asset drawer, in its turn, and
  shares its memory of content that does not draw (both are keyed by
  content digest). A tree keeps its thumbnails in `thumbnails/` beside its
  `blobs/`, so deleting the tree deletes them; `ArtifactStore::remove`,
  which both the lease sweep and a revert's sweep use, deletes a blob's
  thumbnail with it. A fork does not copy thumbnails; it draws its own. A
  [folder artifact](folder-artifacts.md) shows its main file's thumbnail,
  and `attachment.delivery` reports a folder as not included ("it is a
  folder of N files"), since a model only ever gets a folder's label.
- **Web cards.** A stored picture over 1 MiB and a video show the thumbnail
  (kept in Cache Storage by digest, as asset thumbnails are) and are read
  whole only when the media viewer opens them; a stored PDF's file chip
  shows its first page in place of its type square. Without a thumbnail
  (none drawn, or an older daemon), a picture card reads the file as
  before.

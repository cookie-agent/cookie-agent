# Attaching Media

A message can carry files alongside its text. The daemon stores each in the
session's artifact store, an image, PDF, audio, video, or text file with
its own type. Each request sends the files its model accepts; any other
file reaches the model as its label, which names it so the model can read
it with `read_artifact` or copy it to a machine with
[`transfer`](../reference/tools.md#transfer), and a later model that
accepts it gets the file.

## What a model accepts

Every model in the catalog view states its `input_media`: the kinds it
accepts in a message, the media types of each, and the limits that apply
(see [The catalog view](../reference/protocol.md#the-catalog-view)). The
limits are the lowest of the model's own, its provider's for a message, and
the daemon's caps: 20 MiB per file, 25 MiB for video, and at most 16384
pixels a side and 16 Mi pixels for an image. A client checks a file against
them before sending it and says what the model will get; media the model
does not take is still uploaded with its own type, and only content the
daemon does not recognize (or text over 1 MiB) goes up as a file.

For example, an Anthropic model that accepts PNG and JPEG images and PDFs
takes images of up to 7.5 MiB and PDFs of up to 20 MiB, and no video; a
Gemini model takes each kind it accepts up to 14 MiB, which keeps the whole
request within the Gemini API's limit.

## Attaching a file

A client uploads the file into the session first, then names it in the
message:

1. `artifact.upload.begin` with the session, the file's size, its name,
   and its `kind`: `media` (the default) for an image, PDF, audio, video,
   or text file, or `file` for any other, up to 25 MiB.
2. `artifact.upload.chunk` for each piece of at most 1 MiB, in order.
3. `artifact.upload.finish` with the file's SHA-256. The daemon recognizes
   media by its content (the name never decides its type), checks it, and
   returns the reference to send; a `file` upload is stored as
   `application/octet-stream` without checks.
4. `run.start`, `run.steer`, or `session.submit_input` with `content` holding
   the text and a `file` part with that reference.

The [protocol reference](../reference/protocol.md#media) lists every rule and
error. An upload the connection leaves unfinished is dropped when it closes.

The daemon checks every attachment again when the message arrives: it must
be in the store and be the media it says, within the daemon's limits;
otherwise the message is refused and nothing of it is recorded. It is not
checked against the model: each request decides what its model gets, so an
attachment the model does not take, of a kind, type, or size it does not
accept or past its count of a kind, reaches it as its label.

Before the message is sent, a client asks `attachment.delivery` what the
model or agent the draft names would get of each file, and its chip says
so: no mark for a file it gets, **name only** for one it sees by its label
but can fetch with `read_artifact` or `transfer`, and a warning, **model
can't read it**, for one nothing it has can read. The file can still be
sent; the tooltip says why.

## File labels

Each attached file reaches the model, and an ACP agent, with a label just
before it:

```text
[file Screenshot.jpg attached to this user message · image/jpeg · 671 KB · artifact 0c697e90 · included below]
```

The label names the artifact by the first 8 hex digits of its digest, which
`read_artifact` and `transfer` take, so "save this to labpc" from a phone is
a single call. Its last part says whether this request carries the file, and
if not, what works for it:

| Ending | When |
|---|---|
| `included below` | The file follows the label in this request. |
| `not included; read_artifact returns it` | The request leaves it out, but `read_artifact` returns it: UTF-8 text (native models take none in a message), or media this model takes from a tool (such as an image an earlier build stored as a file, or one over the request's count of images). |
| `not included; use transfer to copy it to a machine you work on` | Neither, and the request offers `transfer`, at once or deferred. |
| `not included; only its name is available` | Neither, and no `transfer`; or the stored copy is gone. |

A `file` upload, such as an archive, is its label alone:

```text
[file report.zip attached to this user message · application/octet-stream · 3.2 MB · artifact 5f2c81d0 · not included; use transfer to copy it to a machine you work on]
```

The ending is decided for each request, after the model's media limits
apply, so it holds for the model that reads it.

`read_artifact` of an image, PDF, audio, or video artifact the model takes
returns the file itself, as `read` returns one; of one it does not take, or
of other binary content, an error that says so and suggests what to do on a
machine instead (for example `ffmpeg` for a video, `pdftoppm` for a PDF),
naming `transfer` when the session offers it.

## In the terminal UI

`/attach` asks for a file's path and attaches the file to the next message,
and pasting the path of one existing file (what a terminal pastes when a file
is dropped on it) offers to attach it, once a model is selected. The TUI
checks the file's kind, size, and an image's pixels against the model's
limits before it uploads, and says why when the model does not take it;
media still goes up with its own type, unrecognized content as a file, and
only a file over the daemon's 25 MiB limit is refused. It does not
downscale.
Attachments show on the composer's border as `📎 name · type · size`, with
` · name only` or ` · can't read` as `attachment.delivery` answers, and
Backspace at the start of the draft removes the newest. See
[Terminal UI](../clients/tui.md#attachments).

## When the model changes

A session can move to another model, or a run can fall back to one. A model
that does not accept a file in the history gets the file's label, ended by
what works for it, and a note with the reason in place of the file, such as:

```text
[file screenshot.png attached to this user message · image/png · 471 KB · artifact 5f2c81d0 · not included; use transfer to copy it to a machine you work on]
[image omitted: acme/text-model does not accept images (screenshot.png, 471 KB)]
```

A file a tool returned has no label; a short note stands in for it, such as
`[image omitted: acme/text-model does not accept images (screenshot.png, 471
KB)]`.

The saved history keeps the file, so a later model that accepts it sees it
again. A file uploaded for a model that did not take it is kept with its own
type too, so it reaches the next model that does. Compaction summaries never see media: the summarizer reads notes in
place of every file.

## Thumbnails

The web page shows a picture over 1 MiB, a video, or a PDF in the
transcript by a thumbnail the daemon draws (`artifact.thumbnail`, as
[asset thumbnails](assets.md) are drawn), and reads the file itself only
when it is opened or downloaded. The daemon keeps each thumbnail beside the
session tree's files and deletes it with the file or the session.

## How long a file is kept

An uploaded file no message references is kept for an hour, then removed.
Once a message names it, it lives as long as the session history references
it: a revert that removes every message naming it removes the file once its
hour is up, and a fork takes it into the forked session. A daemon restart
forgets the hour; a file no message referenced then stays in the store.

Any artifact of a session, an uploaded file, a tool's attachment, or the full
output behind a truncated tool display, can be read back with
`artifact.read`.

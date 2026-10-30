# Attaching Media

A message can carry files alongside its text. The daemon stores each in the
session's artifact store. Images, PDFs, audio, and video the session's model
accepts are sent to it as files; any other file reaches the model as its
label, which names it so the model can copy it to a machine with
[`transfer`](../reference/tools.md#transfer).

## What a model accepts

Every model in the catalog view states its `input_media`: the kinds it
accepts in a message, the media types of each, and the limits that apply
(see [The catalog view](../reference/protocol.md#the-catalog-view)). The
limits are the lowest of the model's own, its provider's for a message, and
the daemon's caps: 20 MiB per file, 25 MiB for video, and at most 16384
pixels a side and 16 Mi pixels for an image. A client checks a file against
them before sending it, and uploads one that does not fit as a file.

For example, an Anthropic model that accepts PNG and JPEG images and PDFs
takes images of up to 7.5 MiB and PDFs of up to 20 MiB, and no video; a
Gemini model takes each kind it accepts up to 14 MiB, which keeps the whole
request within the Gemini API's limit.

## Attaching a file

A client uploads the file into the session first, then names it in the
message:

1. `artifact.upload.begin` with the session, the file's size, its name,
   and its `kind`: `media` (the default) for a file the model may take, or
   `file` for any other, up to 25 MiB.
2. `artifact.upload.chunk` for each piece of at most 1 MiB, in order.
3. `artifact.upload.finish` with the file's SHA-256. The daemon recognizes
   media by its content (the name never decides its type), checks it, and
   returns the reference to send; a `file` upload is stored as
   `application/octet-stream` without checks.
4. `run.start`, `run.steer`, or `session.submit_input` with `content` holding
   the text and a `file` part with that reference.

The [protocol reference](../reference/protocol.md#media) lists every rule and
error. An upload the connection leaves unfinished is dropped when it closes.

The daemon checks every attachment again when the message arrives, against
the model the message goes to. One the model takes must be the media it says
and within the model's image limits, and no more of a kind than the model
takes; otherwise the message is refused and nothing of it is recorded, and
the error says which limit, so a client can offer to downscale an image that
is too large. An attachment the model does not take reaches it as its label.

## File labels

Each attached file reaches the model with a label just before it:

```text
[file report.zip, 3.2 MB, artifact 5f2c81d0]
```

The label names the artifact by the first 8 hex digits of its digest, which
`transfer` takes, so "save this to labpc" from a phone is a single call. A
`file` upload is its label alone.

## In the terminal UI

`/attach` asks for a file's path and attaches the file to the next message,
and pasting the path of one existing file (what a terminal pastes when a file
is dropped on it) offers to attach it, once a model is selected. The TUI
checks the file's kind, size, and an image's pixels against the model's
limits before it uploads, and uploads one that does not fit as a file,
saying why; only a file over the daemon's 25 MiB limit is refused. It does
not downscale.
Attachments show on the composer's border as `📎 name · type · size`, and
Backspace at the start of the draft removes the newest. See
[Terminal UI](../clients/tui.md#attachments).

## When the model changes

A session can move to another model, or a run can fall back to one. A model
that does not accept a file in the history gets the reason in the file's
label instead of the file, such as:

```text
[file screenshot.png, 471 KB, artifact 5f2c81d0; not shown: acme/text-model does not accept images]
```

A file a tool returned has no label; a short note stands in for it, such as
`[image omitted: acme/text-model does not accept images (screenshot.png, 471
KB)]`.

The saved history keeps the file, so a later model that accepts it sees it
again. Compaction summaries never see media: the summarizer reads notes in
place of every file.

## How long a file is kept

An uploaded file no message references is kept for an hour, then removed.
Once a message names it, it lives as long as the session history references
it: a revert that removes every message naming it removes the file once its
hour is up, and a fork takes it into the forked session. A daemon restart
forgets the hour; a file no message referenced then stays in the store.

Any artifact of a session, an uploaded file, a tool's attachment, or the full
output behind a truncated tool display, can be read back with
`artifact.read`.

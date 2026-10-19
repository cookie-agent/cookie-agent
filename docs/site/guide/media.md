# Attaching Media

A message can carry images, PDFs, audio, and video alongside its text, when
the session's model accepts them. The daemon stores each attachment in the
session's artifact store and sends it to the model as a file.

## What a model accepts

Every model in the catalog view states its `input_media`: the kinds it
accepts in a message, the media types of each, and the limits that apply
(see [The catalog view](../reference/protocol.md#the-catalog-view)). The
limits are the lowest of the model's own, its provider's for a message, and
the daemon's caps: 20 MiB per file, 25 MiB for video, and at most 16384
pixels a side and 16 Mi pixels for an image. A client offers attachments only
for the kinds the selected model accepts and checks a file before sending it.

For example, an Anthropic model that accepts PNG and JPEG images and PDFs
takes images of up to 7.5 MiB and PDFs of up to 20 MiB, and no video; a
Gemini model takes each kind it accepts up to 14 MiB, which keeps the whole
request within the Gemini API's limit.

## Attaching a file

A client uploads the file into the session first, then names it in the
message:

1. `artifact.upload.begin` with the session, the file's size, and its name.
2. `artifact.upload.chunk` for each piece of at most 1 MiB, in order.
3. `artifact.upload.finish` with the file's SHA-256. The daemon recognizes
   the file by its content (the name never decides its type), checks it, and
   returns the reference to send.
4. `run.start`, `run.steer`, or `session.submit_input` with `content` holding
   the text and a `file` part with that reference.

The [protocol reference](../reference/protocol.md#media) lists every rule and
error. An upload the connection leaves unfinished is dropped when it closes.

The daemon checks every attachment again when the message arrives, against
the model the message goes to. An attachment the model does not accept, one
over its limits, or more of a kind than it takes refuses the message, and
nothing of it is recorded; the error says which limit, so a client can offer
to downscale an image that is too large.

## In the terminal UI

`/attach` asks for a file's path and attaches the file to the next message,
and pasting the path of one existing file (what a terminal pastes when a file
is dropped on it) offers to attach it. Both appear only while the selected
model accepts some kind of file. The TUI checks the file's kind, size, and an
image's pixels against the model's limits before it uploads, and shows the
limit when a file is refused, by the TUI or the daemon; it does not downscale.
Attachments show on the composer's border as `📎 name · type · size`, and
Backspace at the start of the draft removes the newest. See
[Run](run.md#attachments).

## When the model changes

A session can move to another model, or a run can fall back to one. A model
that does not accept a file in the history is sent a short note in its place,
such as:

```text
[image omitted: acme/text-model does not accept images (screenshot.png, 471 KB)]
```

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

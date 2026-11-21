# Voice input

Status: **implemented** (2026-11-21; client protocol 1.98.0), approved with
the [preview](voice-input/preview.html); the user's decisions are in
[section 12](#12-decisions-2026-11-21), and where the code differs is in
[section 13](#13-implementation-notes).
Related: [message attachments](message-attachments.md) and
[compatibility v1](compatibility-v1.md) section 11.3 (uploads, media
input), [engine configuration](engine-configuration.md),
[providers](../extend/providers.md).

## 1. Problem

Typing a long instruction on a phone is slow, and the web client is often
used from one. The browser can record the microphone, and speech
recognition models (Whisper and its successors) are cheap and accurate,
both hosted and on a local GPU. Cookie has no way to turn speech into a
message.

What exists already: a user can attach an audio file to a message, and a
model that accepts audio (`media.audio` in its capabilities, or an ACP
agent whose prompt capabilities include `audio`) receives it. Most coding
models do not accept audio, and a recording in the history costs far more
context than its words do.

## 2. Goals and non-goals

**Goals:**

- An optional speech-to-text backend configured on the daemon in a
  `[speech]` table. Without it nothing changes and no microphone shows.
- A microphone button in the web composer: record, stop, and the words
  appear in the composer, where they can be edited before sending
  (dictation). The recording is then thrown away.
- Backends speak the OpenAI-compatible `POST /audio/transcriptions` API.
  That one adaptor covers hosted services (OpenAI `gpt-4o-transcribe`,
  `gpt-4o-mini-transcribe`, `whisper-1`; Groq `whisper-large-v3-turbo`;
  Mistral Voxtral) and local servers (speaches / faster-whisper-server,
  whisper.cpp's server, vLLM, LocalAI).
- A backend can name any configured provider (`provider = "openai"`),
  reusing its API URL and credentials instead of repeating them. Cookie
  does not check which providers offer transcription: a backend that
  fails hands the recording to the next (section 6.3).
- A fallback list of backends, tried in order, as the model fields have.
- Works on a phone (Safari and Chrome) through the daemon's https page.

**Non-goals:**

- Running a model inside the daemon (whisper.cpp or candle linked in). It
  brings a C++/CUDA build, model downloads, and GPU scheduling into every
  build of cookie. A local model runs as its own server and is configured
  by URL (section 5.3).
- Live, word-by-word transcription while speaking (OpenAI's realtime
  transcription, streaming partials). v1 transcribes after Stop. The
  protocol leaves room for it (section 7.3).
- The browser's own Web Speech API. Chrome sends the audio to Google,
  Firefox has none, and its results differ per browser.
- Voice in the terminal UI. A terminal has no microphone API; a TUI
  recorder (`cpal`) can come later on the same daemon methods.
- Speech output (text to speech, reading replies aloud).
- Sending the recording itself to an audio-capable model. Without
  `[speech]` there is no microphone, whatever the session's model accepts.

## 3. What it looks like

A [preview](voice-input/preview.html) shows every state on the real page,
desktop and phone, light and dark, and lets you click through the flow.

The composer gains a microphone button beside Send, where a thumb reaches
it on a phone, shown only when the daemon reports speech as available
(section 7.1). `Ctrl+Shift+Space` in the composer starts it too.

1. **Tap the microphone.** The first time, the browser asks for the
   microphone. The composer's bottom row (attach, chips, Send) becomes a
   recording bar: Cancel (×), a red dot, the elapsed time against the
   limit (`0:12 / 5:00`), a scrolling level meter so the user sees it
   hears them, and Stop (✓) where Send was. The draft above stays as it
   is; an empty one reads "Listening…".
2. **Tap Stop** (or press Enter, or reach the limit). The bar reads
   "Transcribing…" with a spinner; Cancel still works and abandons the
   request.
3. **The text lands in the composer** at the end of the draft, joined to
   what was there with a space, and the composer takes focus with the
   cursor after it. The user edits and
   sends as usual. Recording again adds to the same draft.

Escape cancels a recording or a transcription. Leaving the session or
closing the page cancels too, and the microphone is released (the
browser's recording indicator goes off) as soon as recording stops.

An optional **send when done** setting (web Settings, per browser, off by
default) sends the transcript as the message on Stop, for hands-free use.
It still shows the words in the transcript like any message, so a wrong
transcription is visible.

**Failure is visible and never loses the recording silently.** When
transcription fails, the bar says why (`transcription failed: openai
returned 401 (invalid API key)`) and offers **Retry** and **Discard**. The
recording is kept in the page's memory until one of them, the next
recording, or leaving the page.

**Where the button cannot work it says so** rather than disappearing:
on a page that is not a secure context (plain `http` to an address other
than `localhost`), browsers do not offer the microphone, and the button is
disabled with the tooltip "the microphone needs https or localhost". When
the user refused the microphone, the bar says how to allow it again.

## 4. Recording in the browser

- `navigator.mediaDevices.getUserMedia({ audio: true })` and
  `MediaRecorder`, through `web-sys` (features `MediaDevices`,
  `MediaRecorder`, `MediaStream`, `MediaStreamConstraints`, `BlobEvent`,
  `AudioContext`, `AnalyserNode` for the meter).
- The container is the first of `audio/webm;codecs=opus`,
  `audio/ogg;codecs=opus`, `audio/mp4` that `MediaRecorder.isTypeSupported`
  accepts: Chrome and Firefox give WebM/Opus, Safari MP4/AAC. All three are
  formats the OpenAI API accepts, so the daemon sends them as recorded and
  never transcodes.
- Opus at the browser's default bitrate is about 32 kb/s, so five minutes
  is about 1.2 MB and the default limit stays far below the 25 MB OpenAI
  accepts.
- `audioBitsPerSecond` is left to the browser; mono is requested
  (`channelCount: 1`) where the browser honours it.
- The recorder stops itself at `max_seconds` (section 5).

## 5. Configuration

### 5.1 The `[speech]` table

A new top-level table. Absent, voice input is off.

```toml
[speech]
# One backend, or a fallback list tried in order.
backends = [
  { provider = "openai", model = "gpt-4o-mini-transcribe" },
  { endpoint = "http://127.0.0.1:8000/v1", model = "Systran/faster-whisper-large-v3" },
]
language = "en"     # optional hint (ISO 639-1); omitted, the backend detects it
prompt = "cookie, UniEnv, UCMA, Leptos, ACP"  # optional spelling and vocabulary hint
max_seconds = 300   # the longest recording the web client makes
```

| Key | Type | Meaning |
|---|---|---|
| `backends` | array of 1 to 8 tables | The backends, tried in order (section 6.3). |
| `language` | string | Sent as `language`. Omitted: not sent, and the backend detects the language. |
| `prompt` | string, at most 1,000 characters | Sent as `prompt`: names and terms the model should spell as written. |
| `max_seconds` | integer | The longest recording; the web client stops there. 0 or less means no limit (the user's rule for limits), bounded only by `max_bytes`. |
| `max_bytes` | integer | The largest recording the daemon accepts. 0 or less means no limit. |

Defaults for `max_seconds` and `max_bytes` are chosen in implementation
(around 300 s and 24 MiB) and are not pinned by tests.

### 5.2 A backend

| Key | Type | Meaning |
|---|---|---|
| `provider` | string | Any provider of `[providers]` or the provider store (`openai`, `groq`, `mistral`, a custom one, …). The backend uses its API URL and its credentials (`api_key`, `auth_override`, `auth`, or a stored connection), sent as that provider's model requests send them. Exactly one of `provider` and `endpoint`. |
| `endpoint` | string | An OpenAI-compatible base URL; the daemon posts to `<endpoint>/audio/transcriptions`. `https`, or `http` to a loopback address, as for custom providers. Interpolates `${env:NAME}`. |
| `auth` | table | With `endpoint`: the typed auth of custom providers (`bearer-api-key-v1`, `no-auth-v1`, …). Omitted: `no-auth-v1`. Not allowed with `provider`. |
| `model` | string, required | Sent as `model`. Not looked up in the model catalog: transcription models are not chat models, and the catalog lists only models with tool calling. |
| `headers` | map of strings | Extra request headers, as for providers. |
| `timeout_secs` | integer | How long one request may take before the next backend is tried. 0 or less means no limit. |

Whether a provider offers `/audio/transcriptions` is not checked, when
the configuration loads or ever: the models.dev catalog lists transcription
models for some providers but not others (its `openai` entry has no
`whisper-1`), and lists audio chat models where no transcription route
exists, so it cannot tell. A provider without the route fails its request
and the next backend is tried (section 6.3). A provider whose endpoint is
computed from setup (Vertex, Bedrock) has no OpenAI-compatible route and
always fails; it is allowed all the same, as is a provider that is not
configured at all, which fails at request time (section 13).

### 5.3 A local Whisper

Cookie does not start or manage a local model. Any server with the
OpenAI-compatible endpoint works, for example on a GPU machine:

```sh
docker run --gpus all -p 127.0.0.1:8000:8000 ghcr.io/speaches-ai/speaches:latest-cuda
```

with `endpoint = "http://127.0.0.1:8000/v1"`. A server on another machine
is reached over `https`, or through an SSH tunnel to a loopback port,
because plain `http` is allowed only to loopback (the same rule as
providers; section 12).

### 5.4 Changes while running

`[speech]` reloads with the rest of the configuration (`config.reload`,
`config.apply`, editing from a client). A reload that turns voice input on
or off, or changes `max_seconds` or `max_bytes`, reaches open clients in
`daemon.status` (section 7.1). A transcription in flight finishes on the
backends it started with.

## 6. The daemon

### 6.1 Transcribing

The daemon receives the recording from the client (section 7.2), then
posts it to the first backend as `multipart/form-data`: `file` (the bytes,
with a filename whose extension matches the media type: `voice.webm`,
`voice.ogg`, `voice.m4a`), `model`, `response_format=json`, and `language`
and `prompt` when configured. It reads `text` from the JSON answer and
returns it trimmed.

`reqwest` gains its `multipart` feature. The request goes out with the
configured and global request headers, as provider requests do.

The implementation checks the request against OpenAI's API reference and
each named service's documentation (accepted fields, formats, size limit),
not against memory.

### 6.2 What is kept

Nothing. The recording lives in the daemon's memory for the request and
is dropped when it returns; it is not written to a session's artifact
store, the history, or logs. The daemon log records one line per request:
the backend, the model, the byte length, the media type, the duration,
and the outcome, never the audio or the text.

### 6.3 Falling back

Any failure of a backend hands the recording to the next one: it cannot be
reached, times out, answers with any non-2xx status (a route it does not
have, a bad key, a model it does not know, a format it refuses, a rate
limit, a server error), or answers with JSON that has no `text`. Each
failure is logged with the backend and the status. When every backend
fails, the error the client gets lists each one's failure; when a later
backend answers, the client gets the text and the daemon log keeps the
earlier failures.

There are no retries of the same backend; the user's Retry is the retry.

### 6.4 Cost

Transcription is billed per minute of audio (about $0.003/min for
`gpt-4o-mini-transcribe` and $0.006/min for `gpt-4o-transcribe` and
`whisper-1` on OpenAI at the time of writing; Groq's turbo Whisper is
cheaper still). v1 records no cost: the requests are not part of any
session's usage (section 12).

## 7. Protocol

Client protocol: the next minor version (after 1.97.0). The plugin and
runtime protocols are unchanged.

### 7.1 Telling clients it is there

`DaemonStatus` gains `speech`, present when `[speech]` has at least one
backend:

```json
{ "speech": { "max_seconds": 300, "max_bytes": 25165824 } }
```

The web client shows the microphone while it is present, and learns of
changes from the `daemon.status` notification it already handles.

### 7.2 Sending the recording

The recording is not a session's artifact: a new session's draft has no
session yet, and nothing should be kept. It travels in its own chunked
request, the shape of `artifact.upload.*` and sharing its chunk size and
base64 encoding:

| Method | Params | Result |
|---|---|---|
| `speech.transcribe.begin` | `byte_length`, `media_type` (`audio/webm`, `audio/ogg`, `audio/mp4`, …), optional `session_id` (for the log line only) | `transcription_id`, `chunk_bytes` |
| `speech.transcribe.chunk` | `transcription_id`, `offset`, `data` (base64) | `{}` |
| `speech.transcribe.finish` | `transcription_id`, `sha256` | `text`, `backend` (the one that answered), `duration_ms` (the backend's request time) |
| `speech.transcribe.cancel` | `transcription_id` | `{}` |

`begin` refuses a `byte_length` above `max_bytes` and a media type that is
not `audio/*` (`speech_unavailable` when `[speech]` is absent,
`speech_too_large`, `speech_unsupported_media`). The daemon sniffs the
bytes (`media_sniff`) at `finish` and refuses ones that are not audio. A
transcription is valid only on the connection that began it and is
dropped when the connection closes or after a short idle time. `finish`
may take as long as the backends do; `cancel` from the same connection, or
the connection closing, aborts the backend request.

`finish` fails with `speech_failed`, whose message lists each backend's
failure (section 6.3).

### 7.3 Room for later

- Live transcription can add a `speech.transcribe.partial` notification
  while chunks arrive, for backends that stream; v1 sends none.
- The terminal UI can record with `cpal` and use the same methods.

## 8. Permissions and guests

Transcription is a client action, like uploading, not a tool: no
permission rule applies. Connections with the daemon token, dial-in
clients with client tokens, and share guests whose share lets them send
(`can_send`) may call it; a watch-only guest has no composer and may not.
A guest's requests are billed to the owner's backends like the owner's
own.

## 9. Errors the user sees

| Situation | Where | Text |
|---|---|---|
| No secure context | microphone button, disabled | the microphone needs https or localhost |
| Microphone refused | recording bar | microphone access was refused; allow it in the browser's site settings |
| No microphone | recording bar | no microphone found |
| Too long | recorder stops itself at the limit, then transcribes | (none; the timer shows the limit) |
| Backend failed | recording bar, with Retry and Discard | transcription failed: `<backend>`: `<reason>`, one per backend |
| Speech turned off while recording | recording bar, with Discard | voice input was turned off on the daemon |
| Empty result | recording bar | heard no speech |

## 10. Testing

- Configuration: parsing and refusals with explicit values (both `provider`
  and `endpoint`, neither, `auth` with `provider`, a provider that is not
  configured, `http` to a non-loopback address, a list of 9). No tests of
  defaults.
- The daemon: a local fake `/audio/transcriptions` server checks the
  multipart fields and filename, the JSON answer, falling back on 5xx,
  timeouts, 401, and 404, the error listing every backend, `max_bytes`, sniffing,
  `cancel` aborting the request, and that nothing is written to the store.
- Protocol: schema and corpus entries for the four methods and
  `DaemonStatus.speech`.
- Web: a headless Chrome run with `--use-fake-ui-for-media-stream
  --use-fake-device-for-media-stream` (a fake microphone playing a tone, or
  a WAV via `--use-file-for-fake-audio-capture`) against the fake backend:
  record, stop, the text lands in the composer; cancel; retry after a
  failure; a guest with `can_send` dictating in a share. Disposable like
  the others.
- One live probe against OpenAI with `gpt-4o-mini-transcribe` (the cheap
  model) on a short WAV, and one against a local speaches server on
  yunhao-labserver, before calling it done.

## 11. Phases

1. **Daemon and protocol:** `[speech]` configuration and validation, the
   OpenAI-compatible backend with provider credential reuse and
   fallback, the four methods, `DaemonStatus.speech`, the configuration
   reference page (`docs/site/engine/speech.md`), the configuration table
   row, and the protocol changelog.
2. **Web client:** the microphone button, the recorder and its bar, the
   level meter, insertion into the composer, errors with Retry and
   Discard, the "send when done" setting, and the phone layout.
Each phase is committed and pushed on its own.

## 12. Decisions (2026-11-21)

The user agreed with the draft's recommendations except where noted.

1. **No microphone without `[speech]`** (user's change). The recording is
   never sent to the model as audio, even when the session's model
   accepts audio.
2. **No `chat` backend** (transcribing by asking an audio-capable chat
   model) in v1.
3. **The recording is not kept**: it is thrown away once transcribed, and
   is not attached to the message.
4. **Share guests may dictate** (user's change): guests whose share lets
   them send get the microphone (section 8).
5. **Plain `http` only to loopback**, as for providers; a Whisper server on
   another machine is reached over https or an SSH tunnel.
6. **No cost accounting** of transcriptions in v1.
7. **Cookie does not start a local Whisper server**; it is configured by
   URL.
8. **Every configured provider may be a backend** (user, 2026-11-21).
   Cookie does not detect which providers offer `/audio/transcriptions`;
   a failed request falls back to the next backend (sections 5.2 and
   6.3). There is no probe or test command and no catalog suggestions.
9. **The microphone sits beside Send** (approved with the preview), where
   a thumb reaches it on a phone; Stop (✓) takes Send's place while
   recording.
10. **`Ctrl+Shift+Space`** in the composer starts dictation (approved with
    the preview).

## 13. Implementation notes

Where the code differs from the sections above:

- **No handshake feature** (7.1): the handshake already carries the daemon
  status, so `DaemonStatus.speech` alone says voice input is on; the
  `features` list stays empty.
- **`backends` is always an array** (5.1), `[[speech.backends]]` or
  `backends = [...]`; a single table is refused.
- **A backend may have a `name`** (5.2), what errors and the log call it;
  without one, the provider, else the endpoint's host and port.
- **An unknown provider fails at request time** (5.2), not when the
  configuration loads: the provider store and the catalog are the models
  layer's, not the configuration's. Like every other failure, it falls
  through to the next backend.
- **Provider credentials** come from the models layer's own precedence
  (`CompiledModelRuntime::transcription_target`): authored `api_key` or
  `auth_override`, else the stored connection, else no auth for a local
  server. Bearer, API-key-header, Anthropic, Google, Azure, and OAuth
  access-token methods become one header; ChatGPT sign-in and AWS SigV4
  cannot, and fail over. Headers are the global ones, then the provider's,
  then the backend's.
- **Recordings are recognized by `recording_media_type`**: the generic
  sniffer takes WebM and MP4 for video, so a recording's container is read
  as audio (WebM, Ogg, MP4, WAV, FLAC, MP3) and its extension names the
  file the service sees.
- **Errors** (7.2): chunk-protocol refusals reuse the upload codes
  (`upload_not_found`, `upload_offset_mismatch`, `upload_size_mismatch`,
  `digest_mismatch`, `too_many_uploads`, at two recordings per
  connection); a `finish` abandoned by `cancel` or a closed connection is
  `speech_cancelled`. An idle recording is dropped after two minutes.
- **The web client** (phase 2): `crates/web/src/dictation.rs` records with
  `MediaRecorder` (WebM/Opus where offered, else Ogg, else MP4), meters
  the level with an `AnalyserNode`, and keeps a failed recording in the
  page for Retry; leaving the session or the composer abandons a
  dictation. The "send when done" switch is **Settings › Display ›
  Dictation**, shown while the daemon has `[speech]`. The mock model of the
  browser scenarios also transcribes, so the `dictation` scenario goes
  through a provider backend, with Chrome's fake microphone.

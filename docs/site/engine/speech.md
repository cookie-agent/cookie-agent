# Voice input [speech]

Complete `config.toml` turning on voice input, with OpenAI first and a local
Whisper server when OpenAI fails:

```toml
[speech]
language = "en"
prompt = "cookie, UniEnv, UCMA, Leptos"

[[speech.backends]]
provider = "openai"
model = "gpt-4o-mini-transcribe"

[[speech.backends]]
name = "local-whisper"
endpoint = "http://127.0.0.1:8000/v1"
model = "Systran/faster-whisper-large-v3"
```

Without a `[speech]` table voice input is off, and clients show no
microphone. An authored table replaces the lower table and uses defaults for
omitted fields. Unknown fields fail. See [config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `backends` | array of 1 to 8 tables | *(required)* | The transcription backends, tried in order (below). |
| `language` | string | *(none)* | A language hint such as `en`; omitted, the backend detects the language. |
| `prompt` | string | *(none)* | At most 1,000 characters: names and terms the backend should spell as written. |
| `max_seconds` | integer | `300` | The longest recording a client makes; it stops recording there and transcribes what it has. `0` or less means no limit. |
| `max_bytes` | integer | `25165824` | The largest recording the daemon accepts. `0` or less means no limit. |
| `recording` | string | `"compressed"` | How clients record. `"compressed"`: what the browser records, WebM or Ogg with Opus, else MP4 audio, about 0.25 MB a minute; some services cannot read WebM (a gateway that measures the audio's length may refuse it). `"wav"`: uncompressed 16 kHz mono WAV, about 1.9 MB a minute, which every backend reads. |

## Backends

Each backend speaks the OpenAI-compatible `POST /audio/transcriptions` API:
OpenAI (`gpt-4o-transcribe`, `gpt-4o-mini-transcribe`, `whisper-1`), Groq
(`whisper-large-v3-turbo`), Mistral (Voxtral), and local servers such as
speaches, faster-whisper-server, whisper.cpp's server, vLLM, and LocalAI.

| Key | Type | Default | Description |
|---|---|---|---|
| `provider` | string | *(none)* | Any provider of `[providers]` or one connected with `/connect`. The backend uses its API URL and the credentials its models use. |
| `endpoint` | string | *(none)* | An OpenAI-compatible base URL; requests go to `<endpoint>/audio/transcriptions`. `https`, or `http` to a loopback address. Interpolates `${env:NAME}`. |
| `auth` | table | `no-auth-v1` | With `endpoint` only: `{ method, values }` as for [custom providers](../extend/providers.md#authentication-and-adaptors), one of `no-auth-v1`, `bearer-api-key-v1` (`api_key`), or `api-key-header-v1` (`api_key`, and `parameters.header_name`). Values interpolate `${env:NAME}`. |
| `model` | string | *(required)* | The model the request names. It is not looked up in the model catalog. |
| `name` | string | the provider, else the endpoint's host | What errors and the daemon log call the backend. |
| `headers` | table | empty | Request headers after the global and the provider's own, as [request headers](headers.md) are written. |
| `timeout_secs` | integer | `0` | How long one request may take before the next backend is tried. `0` or less means no limit. |

A backend names exactly one of `provider` and `endpoint`. Cookie does not
check which providers offer transcription: a provider without the route, a
bad key, an unknown model, a timeout, or any other failure hands the
recording to the next backend. When every backend fails, the client is told
each one's reason. A provider whose credentials cannot be sent as a header
(ChatGPT sign-in, Bedrock's AWS signatures) always fails over.

A local model runs as its own server; cookie does not start one. For
example, on a machine with a GPU:

```sh
docker run --gpus all -p 127.0.0.1:8000:8000 ghcr.io/speaches-ai/speaches:latest-cuda
```

A server on another machine is reached over `https`, or through an SSH
tunnel to a loopback port.

## What is kept

Nothing. The recording stays in the daemon's memory while it is
transcribed and is dropped when the answer comes, when the client cancels,
or when its connection closes. It is never written to a session, its
history, or the log. The daemon's log gets one line per backend tried: its
name, model, the recording's size and type, the time taken, and the
outcome, never the audio or the words.

Guests of a [shared conversation](../sessions/sharing.md) who may send
messages may dictate too; their transcriptions use these backends.

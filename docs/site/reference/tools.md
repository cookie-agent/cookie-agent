<!-- The regions between the generated markers are rendered by
crates/cookie_agent/tests/tool_reference.rs from the built-in tool specs. Do not
edit them by hand; regenerate them with
COOKIE_UPDATE_TOOL_DOCS=1 cargo test -p cookie_agent --test tool_reference -->

# Tools

This page shows each built-in tool exactly as Cookie Agent sends it to the model:
its name, its description, and the JSON Schema of its parameters. Provider adapters
place these three fields in their own request format but do not rewrite them. It
also states each tool's contract: what a call does, its output, retention,
limits, and how clients display it.

Cookie Agent publishes tools according to the active agent's permissions and the
capabilities of the selected model. Tool argument objects are strict: unknown
fields and wrong types are rejected.

A tool is sent only when the active agent has an `allow` or `ask` rule for the
tool's permission. A tool whose permission is *none* needs no rule: it is always
sent and every call is allowed. Some tools have further conditions, noted under
each tool.
MCP and plugin tools are described by their servers and plugins; this page
covers only how clients display [MCP tools](#mcp-tools).

To see which tools a real session sent with its latest request, expand the tools
block under the system prompt in the TUI transcript.

<!-- generated: tool-table -->

| Tool | Permission |
|---|---|
| [`read`](#read) | `read` |
| [`write`](#write) | `write` |
| [`edit`](#edit) | `write` |
| [`bash`](#bash) | `bash` |
| [`webfetch`](#webfetch) | `webfetch` |
| [`skill`](#skill) | `skill` |
| [`delegate_subagent`](#delegate_subagent) | `delegate` |
| [`get_subagent_result`](#get_subagent_result) | `delegate` |
| [`send_message`](#send_message) | `message` |
| [`goal_get`](#goal_get) | *none* |
| [`goal_update`](#goal_update) | *none* |
| [`read_artifact`](#read_artifact) | *none* |
| [`stop_background`](#stop_background) | *none* |
| [`memory_search`](#memory_search) | *none* |
| [`memory_read`](#memory_read) | `memory` |
| [`memory_write`](#memory_write) | `memory_write` |
| [`memory_edit`](#memory_edit) | `memory_write` |
| [`memory_delete`](#memory_delete) | `memory_write` |
| [`list_runtimes`](#list_runtimes) | `list_runtimes` |

<!-- /generated -->

## Execution concurrency

Tool providers declare whether each tool is safe to overlap with sibling calls
from the same model turn through `ToolSpec::concurrency`. `ToolConcurrency` is
`Exclusive` by default; parallel execution requires an explicit `Parallel`
declaration.

| Tools | Eligibility | Coordination |
|---|---|---|
| `read`, `read_artifact`, `bash`, `webfetch`, `stop_background` | Parallel | Each call owns its execution and streaming state. |
| `write`, `edit` | Parallel | Matching prepared serialization keys serialize mutations to the same target. |
| `delegate_subagent` | Parallel | Delegate admission serializes durable child reservation and session creation internally. |
| `send_message` | Parallel | Message acceptance serializes guard checks and the durable append inside the recipient actor. |
| MCP tools | Parallel | Each MCP server's service mutex serializes calls to that server; different servers can overlap. |
| `skill`, `get_subagent_result` | Exclusive | These calls interact with session-scoped state. |
| Plugin and otherwise undeclared tools | Exclusive | External declarations cannot currently opt in. |

All parallel-eligible calls in the turn are dispatched together without a
fan-out limit. Exclusive calls run one at a time after the parallel calls have
finished. Results remain associated with their tool call IDs, while terminal
events are persisted in completion order.

## Tool call failures

A failed tool call terminates with a failure code and a message for the model:

| Code | Meaning |
|---|---|
| `execution_failed` | The tool ran, or tried to, and failed. |
| `operation_changed` | What the call was prepared and approved against changed before it ran, such as an MCP tool list or a restarted runtime. |
| `prepared_capability_lost` | The prepared call was lost, for example in a daemon restart between preparation and execution. |
| `permission_denied` | A permission rule denied the call. |
| `runtime_unavailable` | The runtime or MCP server that owns the tool is reconnecting or dropped during the call. The call is not retried for the model; the tool stays published while its owner reconnects. |
| `unsupported_platform` | The tool cannot run on this platform. |
| `invalid_url`, `redirect_error`, `timeout`, `transport_error`, `unsupported_content_type` | `webfetch` failures (see [Webfetch](#webfetch)). |

## Cancelling a run

Interrupting a run (`run.cancel`: the TUI's Ctrl-C or double Esc, the web
client's Stop or Escape, a stop signal to `cookie run`) cancels every call it
has in the foreground at once, and each call ends as `cancelled`:

- **`bash`** kills the command's whole process group (a job object on
  Windows) with no grace period. Output still in flight drains for at most
  250 ms, so a process that left the group and holds the pipes open does not
  hold the call.
- **A runtime's or plugin's tool** gets `tool/cancel`. The coding runtime
  and the plugin SDK drop a call that has not stopped 2 s later.
- **An MCP tool** stops waiting and sends the server
  `notifications/cancelled` for the request, as a call that reaches
  `timeout_ms` does.
- **`webfetch`** drops its request.
- **A foreground `delegate_subagent`** stops waiting; its subagent keeps
  running and reports its turn (see [Background calls](#background-calls)).

The engine waits at most 2 s for a call to stop on its own. A call still
running then ends as cancelled with `cleanup deadline elapsed`, its late
result is dropped, and the run records `RunCancelled` without waiting
further. Calls in the background belong to the session, not the run, and
keep running; `stop_background` stops them.

## Tool display

Every built-in tool describes its calls to clients as typed display blocks
(the [structured tool display spec](../specs/compatibility-v1.md#12-structured-tool-display)).
Display never enters model history: the output, title, and metadata the model
reads are the same with or without it.

- **Presentation.** `tool_call_started.presentation` carries a human title
  (such as `Edit src/main.rs` or ``Run `cargo test` ``), a `hint` for icons
  and grouping, the `primary_argument` shown beside the title, `arguments`
  (the call's arguments as blocks), and, for `edit` and `write`, a
  `preview` of what the call will do.
- **Results.** A result's `display` replaces the live progress display when
  the call ends.
- **Diffs.** `edit` and `write` show a `diff` with 1-based file line numbers
  and five lines of context on each side of each change. Changes whose
  context overlaps or touches share a hunk. A preview is `as_of: prepare`
  and a result `as_of: execute`; the two differ when the file changed
  between preparation and execution. A side with no lines in a hunk starts
  at the line before it (`0` at the top of the file), as a unified diff
  numbers it.
- **Budget.** A result's blocks share 64 KiB serialized. A block that had to
  be cut counts what it left out (`omitted`, `omitted_lines`, or
  `omitted_rows`) and, where its complete content exists, names it in
  `full`: an executed `edit` or `write` retains its complete diff (as
  unified-diff text, `text/plain`, named `<file>.diff`) and `bash` names its
  retained output. Previews and `read` excerpts have no `full`: nothing is
  retained when a call is prepared, and a `read` page is the file itself.

The per-tool **Display** sections below list each tool's hint, arguments,
preview, and result blocks.

## Filesystem tools

`read`'s `limit` must be positive. The path is resolved when the call is prepared; the content is read when it executes.
A text line longer than 4 KiB is cut and ends with a
`[line truncated: N more bytes]` marker, and a page that would exceed the result
size limit stops early with a `continue with offset=N` note (also reported as
`next_offset` in the metadata). Media files are checked against the size limits
before they are read in full.

`write` takes the file's complete `content`. `edit` requires `oldString` to
identify one unambiguous replacement. Preparation checks the replacement against the current
content so a mismatch fails before approval, but the edit itself is applied to
the file's content at execution: changes made while an approval was pending are
kept, and an `oldString` that no longer matches fails the call. Both tools
publish through a staged file renamed over the target. Writes and edits that
resolve to the same file share a serialization key and run one at a time, in
call order, so same-file edits in one turn apply on top of each other.

Tool results may include image, PDF, audio, or video attachments. Attachments
are validated, content-addressed, and supplied to models as file parts rather
than embedded in text output.

### `read`

<!-- generated: tool read -->

```text
read(filePath: string, limit?: integer, offset?: integer)
```

Description sent to the model:

```text
Read a text/image/video file or a directory snapshot using a zero-based offset.
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `filePath` | `string` | yes | Path to the file or directory. Relative paths resolve against the session working directory. |
| `limit` | `integer` | no | Maximum number of entries or lines to return. Defaults to 2000. |
| `offset` | `integer` | no | Zero-based entry or line offset. Defaults to 0. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "filePath": {
          "description": "Path to the file or directory. Relative paths resolve against the session working directory.",
          "type": "string"
        },
        "limit": {
          "description": "Maximum number of entries or lines to return. Defaults to 2000.",
          "format": "uint",
          "minimum": 0,
          "type": [
            "integer",
            "null"
          ]
        },
        "offset": {
          "description": "Zero-based entry or line offset. Defaults to 0.",
          "format": "uint",
          "minimum": 0,
          "type": [
            "integer",
            "null"
          ]
        }
      },
      "required": [
        "filePath"
      ],
      "title": "ReadArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### `write`

<!-- generated: tool write -->

```text
write(content: string, filePath: string)
```

Description sent to the model:

```text
Write content to a file atomically, creating missing parent directories.
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `content` | `string` | yes |  |
| `filePath` | `string` | yes | Path to the file. Relative paths resolve against the session working directory. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "properties": {
        "content": {
          "type": "string"
        },
        "filePath": {
          "description": "Path to the file. Relative paths resolve against the session working directory.",
          "type": "string"
        }
      },
      "required": [
        "filePath",
        "content"
      ],
      "title": "WriteArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### `edit`

<!-- generated: tool edit -->

```text
edit(filePath: string, newString: string, oldString: string, replaceAll?: boolean)
```

Description sent to the model:

```text
Replace oldString with newString in a file, atomically.
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `filePath` | `string` | yes | Path to the file. Relative paths resolve against the session working directory. |
| `newString` | `string` | yes |  |
| `oldString` | `string` | yes |  |
| `replaceAll` | `boolean` | no | Default: `false`. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "properties": {
        "filePath": {
          "description": "Path to the file. Relative paths resolve against the session working directory.",
          "type": "string"
        },
        "newString": {
          "type": "string"
        },
        "oldString": {
          "type": "string"
        },
        "replaceAll": {
          "default": false,
          "type": "boolean"
        }
      },
      "required": [
        "filePath",
        "oldString",
        "newString"
      ],
      "title": "EditArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### Display

Paths are shown as the call's display argument shows them: relative to the
workspace, or under `~`. A path relative to the workspace (a display
argument, an AGENTS.md file's name) separates its components with `/` on
every platform. An absolute path (the workspace root a runtime reports, the
path in a `read` page, a write's or an edit's result) is spelled the
platform's way; on Windows it is the canonical long form without the `\\?\`
verbatim prefix, which is kept only where the path needs it (longer than
`MAX_PATH`, or naming a device such as `CON`). The title and display argument already name the
file, so no argument block repeats the path, and an edit's or a write's new
text shows in its preview `diff` rather than again as an argument. A diff's and an excerpt's `language` comes from the
path's extension (`rs` is `rust`, `py` is `python`, and so on; an extension
the table does not know is passed on lowercased).

| | `read` | `write` | `edit` |
|---|---|---|---|
| Hint | `file_read` | `file_write` | `file_edit` |
| Title | `Read <path>` | `Write <path>` | `Edit <path>` |
| Arguments | `key_value` of `offset` and `limit` when the call gives them | – | `key_value` `replace all: yes` when set; `newString` as `code` in the path's language only when there is no preview (a call that failed to prepare) |
| Preview | – | `diff` against the file preparation finds (`created` when there is none) | `diff` of the edit applied to the content preparation read |
| Result | `file_excerpt` of the page (`start_line` is `offset + 1`, `total_lines` the file's), `table` (`name`, `type`) of a directory page, or `media` of the retained attachment | `diff` against the text the write replaced | `diff` built from the ranges the edit replaced |

- **`edit`'s diff** comes from the replaced byte ranges, not a diff
  algorithm. Each replacement covers the whole lines it touches;
  replacements whose lines overlap (several on one line, for example) are
  one change, and lines `oldString` and `newString` share at either end are
  context. With `replaceAll`, each replacement is its own change, and the
  changes share hunks where their context overlaps.
- **`write`'s diff** is a line diff of the old and new text when both are
  UTF-8 of at most 1 MiB. Otherwise the `diff` has no hunks and counts the
  lines each side has (a side that is not text counts its newline-separated
  lines; a file that cannot be read counts none).
- **A `read` excerpt** holds the page's lines as the output shows them,
  without line numbers (a cut long line keeps its
  `[line truncated: N more bytes]` marker), up to 64 KiB; the budget cuts the
  rest.

### Media reads

When `read` targets an image (PNG, JPEG, GIF, WebP), PDF, audio file (MP3,
WAV, Ogg, FLAC), or video container (MP4/MOV/WebM/MKV/AVI/FLV/MPEG/WMV/3GPP),
the file is sniffed by content (never by extension alone), strictly validated,
and retained as an attachment. Whether the attachment reaches the model
depends on two checks, in order:

1. **Model capability.** The selected model must declare the matching input
   modality in the catalog (for example `image` for `image/png`). If it does
   not, the call fails with a tool error naming the model and the missing
   capability.
2. **Family deliverability.** The provider's wire API must accept the media
   kind either inside a tool result or in a following user turn (see the
   [delivery matrix](#media-delivery)). If it does
   not, the call fails with a tool error naming the family.

Both rejections are ordinary tool errors: the model sees the reason and can
recover (for example by asking the user, or by sampling the file through
`bash`). Size is clamped to the smaller of the model's advertised limit and
the provider's inline limit (Anthropic: 7.5 MiB raw images, 10 MiB once
base64-encoded; Bedrock: 3.75 MiB images, 4.5 MiB documents, ≈18.7 MiB raw
video; Vertex: 7 MiB images; Gemini API: 14 MiB for any media, keeping the
base64 request under its 20 MiB cap; other families: 20 MiB images, PDFs, and
audio, or 25 MiB video). Media goes in the tool result when the family's wire
API accepts it there; otherwise, when the API accepts that kind in user
messages, it is sent as a file in one emitted user turn immediately after the
tool result. Each model also advertises per-kind count limits, enforced per
request (catalog models: 20 images, 5 PDFs, 5 audio files, and 2 videos, 1 on
Bedrock). When the history holds more parts of a kind than the limit, the
oldest are replaced in the outgoing request by a placeholder such as
`[image omitted: over the model's per-request image limit]`; the saved history
keeps them. The elided count grows in steps of half the limit, so later media
appends to an unchanged prompt prefix (and prompt cache) until the limit is
reached again, at the cost of sending as few as half the limit's parts right
after a step. The latest user message is never elided: a request fails only
when that message alone attaches more parts than the limit. Media parts do not
contribute to context fit estimates, except video, which carries a flat
conservative cost.

Before the count applies, every file part in the outgoing request that the
attempt's model cannot take is replaced by a placeholder: user attachments,
tool results' attachments, and emitted user messages alike. That covers a
session that moved to another model, and a fallback to a model without the
kind. A part fits when the model accepts its kind and media type within its
byte limit, in a user turn for user and emitted messages and in a tool result
for a tool result's attachment (the [delivery matrix](#media-delivery)). The
placeholder says why, for example
`[image omitted: acme/text-model does not accept images (screenshot.png, 471 KB)]`,
`[image omitted: acme/model does not accept image/webp (…)]`,
`[image omitted: larger than the 3.8 MB acme/model accepts (…)]`, or
`[image omitted: acme/model does not accept images in tool results (…)]`. It
depends only on the history and the model, so the prompt prefix stays
cacheable; the saved history keeps the files.

MCP media blocks (images, audio, embedded resource blobs) follow the same gate
and delivery selection. Blob resources without a declared MIME type are
retained under the sniffed type. Results that would exceed the combined
attachment budget keep what fits and degrade the rest to inline notes. The MCP
wire format cannot author arbitrary additional messages.

Media does not survive context pressure. Tool-output elision removes the parent
result and all of its emitted messages as one unit, and compaction checkpoints
drop attachments. The model can re-read the source file to recover it.

### Media delivery

This matrix describes tool-origin media, not every input type the upstream API
accepts. The effective model must also declare the modality and limits.

| Effective adaptor family | Images | PDF | Video | Audio |
|---|---|---|---|---|
| Anthropic | Tool result | Tool result | No | No |
| Anthropic-compatible | Tool result | Tool result | User turn on capable MiniMax models | No |
| Bedrock Converse | Tool result | Tool result | Tool result on capable Nova models | No |
| OpenAI/compatible Responses, Azure Responses | Tool result | Tool result | No | No |
| OpenAI Chat | User turn | User turn | No | No |
| Azure Chat | User turn | No | No | No |
| OpenAI-compatible Chat | User turn | User turn | User turn on capable Kimi/Qwen models | No |
| Gemini, Vertex Gemini | User turn | User turn | User turn | User turn |
| Cohere | User turn | No | No | No |

## `bash`

<!-- generated: tool bash -->

```text
bash(command: string, timeout?: integer)
```

Description sent to the model on Linux and macOS:

```text
Execute one prepared shell command. The timeout applies in the background too: pass timeout 0 for commands that keep running, such as dev servers.
```

Description sent to the model on Windows, where commands run in Git Bash:

```text
Execute one prepared Git Bash command. Single-quote native Windows paths (for example, 'C:\Users\name\file') or use C:/ paths. The timeout applies in the background too: pass timeout 0 for commands that keep running, such as dev servers.
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `command` | `string` | yes |  |
| `timeout` | `integer` | no | Milliseconds before the command is killed; 0 never times out. Default: `120000`. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "properties": {
        "command": {
          "type": "string"
        },
        "timeout": {
          "default": 120000,
          "description": "Milliseconds before the command is killed; 0 never times out.",
          "format": "uint64",
          "minimum": 0,
          "type": "integer"
        }
      },
      "required": [
        "command"
      ],
      "title": "BashArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

`bash` takes one complete `command`. It is
[background-capable](#background-calls): the host adds `background`
(default `false`), and the timeout applies in the background too, so a
command that keeps running, such as a dev server, needs `timeout: 0`.
The command's standard input is empty: tools take no input while they run.
Standard output and standard error are streamed separately during execution
and retained as separate artifacts for the terminal result.

The command runs in its own session and process group (a job object on
Windows). When the shell exits, whatever it left in that group is killed, so
`cmd &` and `nohup cmd &` do not outlive the call; only a process that leaves
the group, such as one started with `setsid`, does.

A bash call reaches its detach point as soon as its command runs; its detach
note is `Process <pid>`. In the background it keeps its timeout and
process-group cleanup.

### Display

| | `bash` |
|---|---|
| Hint | `shell` |
| Title | ``Run `<command>` ``, the whole command on one line; clients cut it to the width they have |
| Arguments | the command as `code` in `sh` |
| Preview | – |
| Result | one `terminal` block: stdout and stderr segments in the order they were read, with `exit_code` |

Terminal text keeps SGR colour sequences (`ESC [ … m`); every other escape
sequence and control character but newline and tab is removed. An escape
sequence split across two reads of a pipe is held until it is whole. Live
progress appends each chunk to block `0` with its `stream`, so the live
display is the same terminal block as it grows. The block holds at most
64 KiB of output: the rest, and whatever the display budget cuts, is counted
in `omitted_lines`, and a cut block's `full` names the call's retained output:
the first declared stream (`stdout`, then `stderr`) that outgrew its preview
and was kept as an artifact. When neither stream was kept (both fit their
previews), the model's output holds everything and `full` is absent.

## `webfetch`

<!-- generated: tool webfetch -->

```text
webfetch(raw?: boolean, url: string)
```

Description sent to the model:

```text
Fetch an HTTP or HTTPS URL. Output has final_url, status_code, content_type, and truncated header lines, a blank line, then the text body. Returns HTML as plaintext unless raw is true; other text passes through. Use read_artifact with the artifact a truncation hint names to page retained output, including the body.
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `raw` | `boolean` | no | Default: `false`. |
| `url` | `string` | yes |  |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "raw": {
          "default": false,
          "type": "boolean"
        },
        "url": {
          "type": "string"
        }
      },
      "required": [
        "url"
      ],
      "title": "WebfetchArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

A call looks like `{"url":"https://docs.quantumcookie.xyz/", "raw":false}`.
The URL must start with a lowercase
`http://` or `https://` prefix; any other scheme is rejected with `invalid_url`
before a request is sent. One permission check uses the initial URL including
its query string. Without a matching rule the call is denied. Explicit `ask`
rules still use the normal approval flow.

Output starts with four header lines (`final_url`, `status_code`, `content_type`,
and `truncated`), followed by a blank line and the text body verbatim. Body
newlines are physical newlines, not JSON escapes. Structured `url`, `final_url`,
`status_code`, `content_type`, and `truncated` fields remain in result metadata.
HTML (`text/html` or `application/xhtml+xml`) is rendered by html2text at width
80 unless `raw` is true. Text types, JSON, XML, JavaScript, form data, and
`+json`/`+xml` types pass through as text; raw and non-HTML text use UTF-8 lossy
decoding. Binary types are rejected, including in raw mode. Missing or invalid
Content-Type is treated as `application/octet-stream` and rejected.

The request has a 30-second overall timeout, inherits environment proxies, and
uses reqwest's default redirect policy (10 hops). Redirect hops are not
permission-checked. HTTP error statuses are returned with their text bodies.
There are no additional SSRF, host/IP, DNS, or userinfo checks.

Downloads are streamed to a fixed 16 MiB cap, or 4 MiB for HTML that is
converted to text. Over-cap responses return the fetched prefix with
`truncated: true`, not an error. The caps have no input or configuration
parameter. Full results use the standard event log and result
store. The standard Bounded policy retains oversized output and sends a preview
to the model. `read_artifact` pages the retained header and body by physical
line; its zero-based offset 5 starts at the body. Follow `next_offset` for
successive pages. Metadata is separate and is not paged.

Errors distinguish `invalid_url`, `redirect_error`, `timeout`,
`transport_error`, and `unsupported_content_type` (which names the content type).
`permission_denied` uses the standard policy-denial path and its structured
`tool_denied` reason.

### Display

| | `webfetch` |
|---|---|
| Hint | `fetch` |
| Title | `Fetch <url>` |
| Arguments | `link` of the URL, plus `key_value` `raw: yes` when `raw` is set |
| Preview | – |
| Result | `key_value` of `status`, `content type`, and `final URL` (and `truncated: yes` for an over-cap body), then the body's first lines within 24 KiB as `markdown` for `text/markdown` and `text` otherwise, counting the rest in `omitted_lines` |

## Background calls

A tool that declares `background` support (`bash`, `delegate_subagent`, and
a runtime's own tools that declare it) can run in the background, as the [background tool calls
spec](../specs/background-tools.md) describes:

- **The host owns the argument.** It adds `background` (a boolean, defaulting
  to the tool's `background_default`) to the tool's schema and removes it
  before the tool prepares the call, so the tool never sees it and an
  approval covers the call either way.
- **Detaching.** A call the model starts with `background: true`, or one the
  user moves to the background (`run.tool_background`; the TUI's background
  button), detaches once the tool reaches its detach point. A call that
  finishes first returns as an ordinary result. Detaching commits an
  immediate result for the call, `Started in the background as job
  job_1a2b3c4d. …` or `User manually moved this call to the background; it
  is still running as job job_1a2b3c4d. …`, ending with `Stop it with
  stop_background(job_1a2b3c4d).` and followed by the tool's note, and
  the run goes on. Cancelling the run no longer reaches the call.
- **Reporting.** When the call ends, the engine accepts one producer message
  owned by `tool { tool_call_id }` that steers into the active run or wakes
  the idle session: `<background_result job="…" tool="…" status="…">`
  wrapping exactly what the foreground result would have said, its output
  previewed with a `read_artifact` hint into the retained output for
  anything omitted. `status` is `completed`, `failed`, or `stopped`. The
  call reports only after its immediate result is committed; a revert past
  the call, deleting its session, or the daemon stopping kills it without a
  report.
- **Stopping.** `stop_background` (`job`) stops a background call of the
  session tree and returns once it has ended; it needs no permission and is
  published only while a background-capable tool is. A user stops one with
  `session.background.stop`. Either way the call still reports, with
  `status="stopped"` and a first line saying who stopped it.
- **Subagents report themselves.** A `delegate_subagent` call in the
  background, or moved there, gets no job: its immediate result names the
  subagent's handle, and every turn the subagent finishes reports to the
  parent as a `<subagent_report handle="…" status="…">` producer message
  owned by `subagent { session_id }`, keyed by the turn's run ID so it
  reports once. A foreground call's first turn is its tool result instead.
  The subagent's running turn lists among the parent's background calls
  under its handle, and `stop_background` with the handle cancels that turn
  (`status="cancelled"`; an idle subagent is an error). Cancelling the
  parent's run does not reach its subagents: a foreground call it was
  waiting on ends cancelled with the detach note, and the subagent reports
  its turn as if the call had been moved to the background.

### `stop_background`

<!-- generated: tool stop_background -->

```text
stop_background(job: string)
```

Description sent to the model:

```text
Stop a call running in the background. It returns once the call has ended; its <background_result> still arrives, with status "stopped" and the output it produced. Given a subagent's handle, it cancels the turn the subagent is running; its <subagent_report> follows with status "cancelled", and the subagent can still be messaged or resumed.
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `job` | `string` | yes | The job ID from the result that moved the call to the background, such as `job_1a2b3c4d`, or a subagent's handle. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "job": {
          "description": "The job ID from the result that moved the call to the background, such as `job_1a2b3c4d`, or a subagent's handle.",
          "type": "string"
        }
      },
      "required": [
        "job"
      ],
      "title": "StopBackgroundArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### Display

| | `stop_background` |
|---|---|
| Hint | `generic` |
| Title | `Stop <job>` |
| Arguments | `key_value` of `job` |
| Preview | – |
| Result | the output as `text` |

## Retained tool output

Tools declare `ToolOutputDeclaration::Single` or an ordered list of 1 to 8 named
streams. Names are unique, case-sensitive ASCII path segments, 1 to 64 bytes,
using letters, digits, `_`, `-`, and `.`; `.` and `..` alone are forbidden. The
same validator governs declarations and the `stream` argument of `read_artifact`.

The runtime keeps bounded previews and counts of accepted text chunks in
memory. A stream that fits its preview is stored nowhere else: the result output
holds all of it and it has no artifact. Once a stream outgrows its preview, its
bytes are buffered and written in batches to a temporary file, hashed as they
are written, and published as a full artifact at completion. An artifact keeps
at most the first 128 MiB of a stream; past that only the last mebibyte is
kept, after a marker line counting the dropped bytes, and the stream's
`dropped_bytes` and the result's read hint report the loss. A single output has
at most one artifact; named output gets a generic manifest when any stream has
an artifact, and the manifest lists every declared stream. Declaration
order determines model rendering. Bash uses the same named-output interface as
other tools; it does not build its own retained-output manifest.

Each stream receives its own configured `[tool_output]` line/byte preview limits.
Only truncated streams receive a `read_artifact(artifact="<digest>",
stream="<stream>", offset=...)` hint, where the digest identifies the manifest.
Single-output hints omit `stream` and the heading. A partial-line byte cut points back to that
line, so readback does not skip unseen text. Aggregate preview construction also
reserves 16 KiB for headings/hints inside the existing 2 MiB output bound; when
necessary, previews are reduced with truthful read hints.

`ToolProgress.output` carries at most 8 typed chunks and 64 KiB of authoritative
text per delta. Its optional `display` is a delta of the live display, independent
of the output: `append { index, text, stream? }` appends to a `text` (or, with a
`stream`, a `terminal`) block, creating it when `index` is just past the list,
and `replace { blocks }` replaces the list. The `message` and an append's text
are each limited to 1 KiB; the runtime caps the cumulative live-display budget at
64 KiB per call, and the messages' at another 64 KiB, without stopping
authoritative output. `bash` appends to block `0` with each chunk's stream.

A result's `display` is a list of display blocks; empty means nothing to show,
and a result a tool completes with `ToolCompletion::single` and no display shows
its output as one `text` block. Each built-in tool's blocks are listed in its
**Display** section, and [Tool display](#tool-display) describes the shared
rules. Final `display` replaces live display; the daemon sanitizes its blocks,
drops kinds it does not know (with a `diagnostic` naming the plugin or
runtime that sent them), and fits the list to its own 64 KiB serialized
budget, cutting a block to whole lines and counting the rest in its
`omitted_lines`. A prepared call's presentation (its title, hint, primary
argument, and display blocks of its arguments and preview) is fitted the same
way. Control characters are sanitized while newlines and tabs remain
usable in display text. Display is never included in model history.

Executors return `ToolCompletion`: `Single`/`Named` supplies terminal output once,
whereas `Streamed` finalizes accepted deltas without resending them. Resupplying
terminal output after streaming is an error. Failures and cancellation preserve
accepted output as incomplete, retain the correct terminal status, and expose the
error alongside output previews to the model. Terminal events carry
`retained_output` references and final `display`; replay does not recapture chunks.

The self-paginating `read`, `read_artifact`, and `get_subagent_result` tools
declare an absolute truncation opt-out. Their requested page is returned in full
without artifact retention or truncation metadata, regardless of
`[tool_output]`. Callers bound these results with each tool's offset/limit
arguments. A requested page above the event schema's 2 MiB output limit fails
with a resource-limit tool error; it is never silently truncated. The
`delegate_subagent` terminal result also opts out, because its teaser preview is
bounded by `[subagent_output]` instead; the owner of the delegation flow is the
only place it is bounded. MCP and plugin tools remain subject to normal truncation. External
opt-out would require a future extension-protocol capability and is not currently
authorable.

Artifacts are content-addressed by SHA-256, deduplicated, and newly created with
mode `0600` on Unix. Their digest is verified when content is first read. A
missing or corrupt artifact produces a normal tool error and does not prevent
the engine or session from opening. Artifacts are written without fsync and
without a store-wide lock (each publish is one atomic rename), so a crash or
power loss can lose an artifact a durable event already references. Reads then
report the output as unavailable, and model history replaces a lost attachment
with a short note instead of failing.

Artifacts are collected only when a session is reverted. The revert deletes
the digests that the cut-away events (and any delegated children the revert
deletes) referenced, directly or through a generic manifest, unless an event
still in the tree's logs references them too. The sweep is skipped while any
session of the tree runs or a capture is still publishing its terminal
references. Artifacts orphaned any other way, such as by a crash or a cancelled
capture, stay in the tree's `artifacts/` directory until the tree is deleted.

### `read_artifact`

<!-- generated: tool read_artifact -->

```text
read_artifact(artifact: string, limit?: integer, offset?: integer, stream?: string)
```

Description sent to the model:

```text
Read retained tool output by line, using a zero-based offset. Truncated results and elided history name the artifact (and stream) to read in a read_artifact hint; continue from the offset it gives.
```

Always, whatever the agent's permission rules: it reads only output retained by the session's own tree.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `artifact` | `string` | yes | The artifact's 64-character lowercase SHA-256 digest, as a truncation hint names it. |
| `limit` | `integer` | no | Maximum number of lines to return, at most 2000. Defaults to 2000. |
| `offset` | `integer` | no | Zero-based line offset. Defaults to 0. |
| `stream` | `string` | no | Named stream of a multi-stream output, such as `stdout`, as a truncation hint names it. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "artifact": {
          "description": "The artifact's 64-character lowercase SHA-256 digest, as a truncation hint names it.",
          "type": "string"
        },
        "limit": {
          "description": "Maximum number of lines to return, at most 2000. Defaults to 2000.",
          "format": "uint",
          "minimum": 0,
          "type": [
            "integer",
            "null"
          ]
        },
        "offset": {
          "description": "Zero-based line offset. Defaults to 0.",
          "format": "uint",
          "minimum": 0,
          "type": [
            "integer",
            "null"
          ]
        },
        "stream": {
          "description": "Named stream of a multi-stream output, such as `stdout`, as a truncation hint names it.",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "artifact"
      ],
      "title": "ReadArtifactArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

`read_artifact` is a base-layer tool. A digest that names content another tree stored does not resolve. Digests are
content-addressed SHA-256 references, not random secrets. Malformed digests
(bare `artifact://` URIs, the internal `sha256/` form, uppercase or short
digests) and invalid stream names are rejected. `read` reads only the
filesystem.

Artifact reads return the stored content without unwrapping JSON. Normal
single-output artifacts contain the original full text. Named-stream artifacts
contain their complete individual text. Compaction retry artifacts may contain serialized
tool-content JSON, including output text and metadata. Their markers identify
this format and expose usable IDs without requiring durable elision events.

Without `stream`, `read_artifact` reads stored text as-is, including manifest
JSON; it does not merge streams. `stream` selects a declared name from the generic
manifest. This is content-based and works for `results`, `diagnostics`, or any
other valid declared name, not just Bash streams. Unknown names and selection on
non-manifests are errors. Stored `ArtifactReference` values keep the internal
`artifact://sha256/<digest>` form, which is not a `read_artifact` argument.

Returned pages include `next_offset` metadata when more lines remain. A zero
limit fails; larger limits are capped at 2000 lines. Pages over the 2 MiB
terminal-result limit fail and must be requested with a smaller limit. Missing
and corrupt artifacts produce tool errors.

```json
{"artifact":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stream":"results","offset":0,"limit":200}
```

### Display

| | `read_artifact` |
|---|---|
| Hint | `file_read` |
| Title | `Read artifact <12 hex>[/<stream>]` |
| Arguments | `key_value` of `artifact`, and `stream`, `offset`, and `limit` when given |
| Preview | – |
| Result | the page as `text` |

### MCP tools

MCP tools are shown with the hint `generic`, the published tool name as the
title, and their arguments as one `json` block (none for an empty object).
A result shows its text content as `text` (consecutive text parts as one
block, joined as the output joins them), each retained image, audio, or
embedded resource blob as `media` in place of its placeholder, and
`structured_content` that the text does not already hold as `json`.
`structured_content` also stays in the result's metadata, which the model
reads.

## Goal checklist tools

`goal_get` returns the current root goal's objective, status, revision, and entire
checklist. `goal_update { items }` replaces the entire ordered checklist;
each item contains only `description` and `finished`, with no item ID. The session
actor serializes replacements, and the last accepted update wins. The tool has no
`goal_id` or `expected_revision` parameter or lost-update protection. Its target is
the current/latest session goal when the engine actor accepts the update, so an
older run's update can intentionally affect a newly activated active or paused
goal. Lifecycle changes do not
change an already-admitted run's tool set. Updates reject if the current
goal is absent or terminal, even when the tool remains in an admitted run.
Engine-owned goal IDs and revisions remain; `SessionGoalLifecycleParams.goal_id`
and `expected_revision` apply to user lifecycle RPC controls, not model checklist
updates.

The model cannot set an objective, pause, resume, or cancel. Empty checklists
preserve the active or paused lifecycle; nonempty all-finished checklists complete
it even while paused. The root must verify evidence before marking items finished. Both tools
return the full state without display truncation. Neither declares a
permission: setting the goal is what publishes them, and they act on that goal
alone, so permission rules do not govern them and every call is allowed.

### `goal_get`

<!-- generated: tool goal_get -->

```text
goal_get()
```

Description sent to the model:

```text
Read the root session's objective, lifecycle status, revision, and full checklist. The root checklist is authoritative; verify work directly or through subagents before treating an item as finished.
```

Only in a root session whose goal is active or paused when the run starts.

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "title": "GoalGetParams",
      "type": "object"
    }
    ```

<!-- /generated -->

### `goal_update`

<!-- generated: tool goal_update -->

```text
goal_update(items: object[])
```

Description sent to the model:

```text
Replace the current root goal's full checklist. The current active or paused goal is selected when this update executes, even if the goal changed since this run started. The last accepted replacement wins. This cannot change the objective or issue lifecycle commands; a nonempty all-finished checklist completes the goal. Verify evidence directly or through subagents before marking items finished; the root checklist is authoritative over subagent self-reports.
```

Only in a root session whose goal is active or paused when the run starts.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `items` | `object[]` | yes |  |

??? note "Parameters JSON Schema"

    ```json
    {
      "$defs": {
        "GoalItem": {
          "additionalProperties": false,
          "properties": {
            "description": {
              "type": "string"
            },
            "finished": {
              "type": "boolean"
            }
          },
          "required": [
            "description",
            "finished"
          ],
          "type": "object"
        }
      },
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "description": "`goal_update` replaces the current root goal's checklist at actor execution,\neven if the goal changed since the calling run started. It is never a lifecycle\ncommand. The engine rejects absent/terminal goals and blank descriptions.\nReplacements are serialized; the last accepted update wins. Duplicate descriptions\nare allowed. Empty items preserve active/paused status. A nonempty\nall-finished update completes even while paused, without scheduling a wake.",
      "properties": {
        "items": {
          "items": {
            "$ref": "#/$defs/GoalItem"
          },
          "type": "array"
        }
      },
      "required": [
        "items"
      ],
      "title": "GoalUpdateParams",
      "type": "object"
    }
    ```

<!-- /generated -->

### Display

| | `goal_get` | `goal_update` |
|---|---|---|
| Hint | `goal` | `goal` |
| Title | `Read goal` | `Update goal checklist` |
| Arguments | – | `key_value` with one entry per item: `done` or `to do`, and its description |
| Preview | – | – |
| Result | `markdown`: the objective in bold, the status and revision, and the checklist as `- [x]` and `- [ ]` items (`No goal is set.` without a goal) | the same, for the goal as updated |

## Delegation and skills

Delegation tools start, inspect, and cancel owned subagent sessions. Skill
tools load configured skill instructions for the current turn. Their
availability and targets are derived from the frozen agent policy.

When delegation is available, its provider also freezes the currently eligible
target IDs and agent descriptions into the run's system prompt under
`<tool_instructions provider="builtin.delegate">`. The list uses the same depth
ceiling and enabled-target filtering as `delegate_subagent`; no section is added
when no target is available.

`delegate_subagent`, `get_subagent_result`, `stop_background`, and
`send_message` address subagents by a short tree-unique **handle** (`<agent_type>_<8 hex>`, e.g.
`explore_1a2b3c4d`) or a full session UUID. A handle is generated when the
session is created and resolves only within the caller's tree; a partial handle,
another tree's handle, or a fabricated value is rejected with a self-repairing
error that lists the live candidates as
`handle — description — status` plus their UUIDs. Detach notes and reports
pre-wrap the handle, for example
`[subagent session explore_1a2b3c4d] Each time its turn ends you get a <subagent_report handle="explore_1a2b3c4d">. …` and
`[subagent session explore_1a2b3c4d; completed; 79 lines; use get_subagent_result with session_id "explore_1a2b3c4d" for the full output]`.
The report previews the turn's final message per
[`[subagent_output]`](../engine/subagent_output.md), with its line breaks and
tabs kept and other control characters folded to spaces. A report over the limit
shows its first and last lines around a marker such as
`[… 200 lines omitted. Read more: get_subagent_result(session_id="explore_1a2b3c4d", offset=20)]`;
a complete one ends in `full output shown`.
Terminal and lifecycle results keep the UUID under the unified `session_id`
metadata key and add `handle`. `resume_session_id` accepts the same two forms,
and a terminal child woken by `send_message` reports as running until its new
turn ends.

In a root session with the `global` runtime attached, `delegate_subagent`
also takes `capabilities`: the runtime capabilities the child works through,
each written `<runtime_id>/<capability_id>` with a `workspace` when the
capability takes one (see [Delegating to runtimes](../sessions/delegation.md)).
It is required when the root has nothing but `global` attached, optional when
it has other attachments too, and absent everywhere else, where a child
inherits its parent's attachments. Each runtime it names is checked as
`delegate@<runtime>`, and every runtime starts before the child is created:
one that cannot start fails the call and creates nothing. A root with only
`global` also gets a paragraph in its delegation instructions saying it has no
workspace.

### `skill`

<!-- generated: tool skill -->

```text
skill(args?: string, name: string)
```

Description sent to the model:

```text
Load an available skill by name with optional arguments.
```

Only when at least one skill is visible to the session.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `args` | `string` | no | Default: `""`. |
| `name` | `string` | yes |  |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "args": {
          "default": "",
          "type": "string"
        },
        "name": {
          "type": "string"
        }
      },
      "required": [
        "name"
      ],
      "title": "SkillArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### `delegate_subagent`

<!-- generated: tool delegate_subagent -->

```text
delegate_subagent(agent_type: "<subagent id>", capabilities?: object[], description: string, inherit_context?: boolean, prompt: string, resume_session_id?: string)
```

Description sent to the model:

```text
Delegate a self-contained task to a specialist agent. In the foreground (default) the call waits for the subagent's first turn and returns its result. In the background it returns at once with the subagent's handle. Every later turn of the subagent, and the first one when it runs in the background, arrives as a <subagent_report> that wakes this session, so to wait for one, tell the user and end your turn. To continue an existing subagent, pass its resume_session_id or send it a message.
```

Only when the agent can delegate to at least one subagent at this depth. The `agent_type` enum lists those subagents; this page shows a placeholder. `capabilities` exists only in a root session with the `global` runtime attached: optional, as shown, when the root has other attachments too, and required when it has only `global`. Each runtime it names is checked as `delegate@<runtime>`.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `agent_type` | `"<subagent id>"` | yes |  |
| `capabilities` | `object[]` | no | Optional. Leave it out and the subagent inherits this session's attachments. Each item gives the subagent one runtime capability to work through, at `workspace` when the capability takes one. These are runtime capabilities, where the subagent works, not its tools or permissions. Write each as the full `<runtime_id>/<capability_id>`, from a `<runtime id=…>` and one of its `<capability name=…>` in list_runtimes, e.g. `labpc/coding`. The same runtime may be listed twice with different workspaces. The subagent also has `global` whenever this session does. Cannot be combined with resume_session_id. |
| `description` | `string` | yes | Short (3-5 words) summary of the task |
| `inherit_context` | `boolean` | no | Default: `false`. |
| `prompt` | `string` | yes | Full task brief with objective, context, and deliverable |
| `resume_session_id` | `string` | no | Optional. Handle or UUID of an existing subagent of yours to resume, e.g. "explore_1a2b3c4d". Only subagents you delegated are valid. |

??? note "Parameters JSON Schema"

    ```json
    {
      "additionalProperties": false,
      "properties": {
        "agent_type": {
          "enum": [
            "<subagent id>"
          ],
          "type": "string"
        },
        "capabilities": {
          "description": "Optional. Leave it out and the subagent inherits this session's attachments. Each item gives the subagent one runtime capability to work through, at `workspace` when the capability takes one. These are runtime capabilities, where the subagent works, not its tools or permissions. Write each as the full `<runtime_id>/<capability_id>`, from a `<runtime id=…>` and one of its `<capability name=…>` in list_runtimes, e.g. `labpc/coding`. The same runtime may be listed twice with different workspaces. The subagent also has `global` whenever this session does. Cannot be combined with resume_session_id.",
          "items": {
            "additionalProperties": false,
            "properties": {
              "capability": {
                "description": "`<runtime_id>/<capability_id>`, from list_runtimes, e.g. `labpc/coding`",
                "type": "string"
              },
              "workspace": {
                "description": "Directory on that runtime's machine; required when the capability takes a workspace",
                "type": "string"
              }
            },
            "required": [
              "capability"
            ],
            "type": "object"
          },
          "minItems": 1,
          "type": "array"
        },
        "description": {
          "description": "Short (3-5 words) summary of the task",
          "type": "string"
        },
        "inherit_context": {
          "default": false,
          "type": "boolean"
        },
        "prompt": {
          "description": "Full task brief with objective, context, and deliverable",
          "type": "string"
        },
        "resume_session_id": {
          "description": "Optional. Handle or UUID of an existing subagent of yours to resume, e.g. \"explore_1a2b3c4d\". Only subagents you delegated are valid.",
          "type": "string"
        }
      },
      "required": [
        "description",
        "prompt",
        "agent_type"
      ],
      "type": "object"
    }
    ```

<!-- /generated -->

### `get_subagent_result`

<!-- generated: tool get_subagent_result -->

```text
get_subagent_result(limit?: integer, offset?: integer, session_id: string)
```

Description sent to the model:

```text
Read the current status and latest result of a subagent you delegated, such as the full output a <subagent_report> cut short. This call returns immediately; it never waits. Use the subagent's handle, e.g. "explore_1a2b3c4d". Only your own subagents are visible.
```

Only when the agent can delegate to at least one subagent at this depth.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `limit` | `integer` | no | Default: `2000`. |
| `offset` | `integer` | no | Default: `0`. |
| `session_id` | `string` | yes | Handle (agent_type + 8 hex, e.g. "coder_9f8e7d6b") or full UUID of one of your subagents. |

??? note "Parameters JSON Schema"

    ```json
    {
      "additionalProperties": false,
      "properties": {
        "limit": {
          "default": 2000,
          "maximum": 4294967295,
          "minimum": 1,
          "type": "integer"
        },
        "offset": {
          "default": 0,
          "minimum": 0,
          "type": "integer"
        },
        "session_id": {
          "description": "Handle (agent_type + 8 hex, e.g. \"coder_9f8e7d6b\") or full UUID of one of your subagents.",
          "type": "string"
        }
      },
      "required": [
        "session_id"
      ],
      "type": "object"
    }
    ```

<!-- /generated -->

### Display

| | `delegate_subagent` | `get_subagent_result` | `skill` |
|---|---|---|---|
| Hint | `delegate` | `delegate` | `skill` |
| Title | `Delegate to <agent>` | `Read subagent <session>` | `Load skill <name>` |
| Arguments | `key_value` of `agent`, `description`, `resume` and `inherit context` when set, and `prompt` | `key_value` of `session`, and `offset` (when not 0) and `limit` when given | `key_value` of `name`, and `arguments` when not empty |
| Preview | – | – | – |
| Result | `session_ref` of the subagent (handle as `short_id`, `<agent> · <status>` as `label`), then its whole final message as `markdown` | `session_ref` (its status as `label`), then the page's lines as `markdown` | the loaded skill as `markdown` |

The delegate result's `markdown` is the subagent's report itself, not the
model's bounded preview: up to 48 KiB, then cut by the display budget with
`omitted_lines`. A cancelled or failed delegation shows the `session_ref`,
when it has a session, and its output as `text`. A delegation that moves to
the background commits the immediate result the background section
describes, which shows its output.

## Agent messaging

`send_message` delivers a durable message to another session in the same
delegation tree. It replaces the removed `steer_subagent` tool; see the
[migration notes](../specs/agent-messaging.md#migration-notes). Its argument
object is strict, and `body` is the markdown message text. The earlier
`recipient_session_id` spelling of `to` is not an alias and fails with `send_message:invalid_arguments`. A
non-UUID recipient is resolved as a handle in the sender's tree; if it matches
no live peer the send fails with the self-repairing `unknown subagent reference`
error rather than a `send_message:` code.

### `send_message`

<!-- generated: tool send_message -->

```text
send_message(body: string, mode?: "steer" | "queue", to: string)
```

Description sent to the model:

```text
Send a message to another agent in your session tree. Recipient accepts a subagent handle or a full UUID.
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `body` | `string` | yes |  |
| `mode` | `"steer" \| "queue"` | no | Default: `"steer"`. |
| `to` | `string` | yes | Handle or full UUID of the recipient agent. |

??? note "Parameters JSON Schema"

    ```json
    {
      "additionalProperties": false,
      "properties": {
        "body": {
          "minLength": 1,
          "type": "string"
        },
        "mode": {
          "default": "steer",
          "enum": [
            "steer",
            "queue"
          ],
          "type": "string"
        },
        "to": {
          "description": "Handle or full UUID of the recipient agent.",
          "type": "string"
        }
      },
      "required": [
        "to",
        "body"
      ],
      "type": "object"
    }
    ```

<!-- /generated -->

Calls use the `message` permission action. The resource label is the
recipient's relationship to the sender — `parent`, `child`, `sibling`, or `*`
for any other same-tree peer — derived from stored session-origin metadata.
Messaging is deny-by-default and the tool is hidden until an agent document or
session overlay declares a `message` `allow` or `ask` rule. Denied calls use
the standard policy-denial path and `ask` rules use the normal approval flow;
there is no messaging-specific permission error.

A successful result means **durably accepted**: the message is persisted in
the recipient's event log and survives a daemon restart. It does not mean the
recipient has seen or consumed the message. The result carries `message_id`,
the effective `mode`, and `recipient_state` (`running`, `queued`, or
`waking_finished`). A `steer` send joins a running recipient's next safe
model-request boundary; a `queue` send is claimed into the recipient's prompt
at the next safe boundary. Idle and finished recipients are woken through the
producer reconcile path, resuming the session with the message as input.
There is no inbox tool.

Failures are explicit tool errors with stable codes:

| Code | Meaning |
|---|---|
| `send_message:disabled` | `[messaging] enabled = false` |
| `send_message:invalid_arguments` | Missing, unknown, or wrongly typed arguments, including the removed `recipient_session_id` spelling |
| `send_message:invalid_body` | Empty body or body over `max_body_bytes` |
| `send_message:unknown_session` | Well-formed recipient UUID that names no session |
| `send_message:not_tree_peer` | Well-formed recipient UUID in a different delegation tree |
| `send_message:self_send` | Sender targeted its own session |
| `send_message:inbox_full` | Recipient has `max_pending_per_session` pending agent messages; nothing was admitted |
| `send_message:max_hops_exceeded` | The message chain exceeded `max_hops` |
| `send_message:inflight_full` | The directed pair already has `max_inflight_per_pair` unacknowledged messages |
| `send_message:engine_shutdown` | The send raced daemon shutdown |

Retries are safe: the idempotency key derives from the sender session, run,
and tool call, so a retried call returns the original `message_id` and
delivers once. Two loop-safety guards reject sends, and each can be disabled:
`max_hops <= 0` turns off hop counting (hop metadata is internal and never
appears in the delivered envelope; mail between a parent and its direct child
adds no hop, and subagent reports are not messages), and `max_inflight_per_pair = 0` turns off
the per-pair window. A pair message counts as unacknowledged while accepted,
admitted, claimed, or released, and is acknowledged once consumed or
discarded. See [Agent Messaging](../engine/messaging.md) for the settings and
the [agent messaging spec](../specs/agent-messaging.md) for the full contract.

### Display

| | `send_message` |
|---|---|
| Hint | `message` |
| Title | `Message <to>` |
| Arguments | `key_value` of `to`, `mode`, and `body` |
| Preview | – |
| Result | the output as `text` |

## Memory tools

The memory tools search, read, and change the memories the `global` runtime
keeps; see [Memory](../sessions/memory.md).

### `memory_search`

<!-- generated: tool memory_search -->

```text
memory_search(layer?: "user" | "agent" | "machine" | "project", limit?: integer, query: string)
```

Description sent to the model:

```text
Search memory: notes saved in earlier sessions about the user, agents, machines, and projects. Terms are matched case-insensitively against each memory's keywords, name, description, and body; results are ranked by keyword hits first. Results show each memory's URL, description, keywords, and up to three matching lines; a result you need approval to read shows only its URL and description. Search for the project a task belongs to by repository name, remote URL, or directory before starting work. `layer` narrows the search to `user`, `agent`, `machine`, or `project` memories.
```

Only when the session has the `global` runtime attached and the agent or the session overlay has an `allow` or `ask` rule for `memory`. `memory_search` needs no rule of its own: each result is checked against the `memory` rules for its URL instead.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `layer` | `"user" \| "agent" \| "machine" \| "project"` | no | Only search this layer. |
| `limit` | `integer` | no | The most results to return: 1 to 25. Defaults to 10. |
| `query` | `string` | yes | Search terms, separated by whitespace: 1 to 200 characters. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$defs": {
        "LayerArg": {
          "description": "The layer a search narrows to.",
          "enum": [
            "user",
            "agent",
            "machine",
            "project"
          ],
          "type": "string"
        }
      },
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "layer": {
          "anyOf": [
            {
              "$ref": "#/$defs/LayerArg"
            },
            {
              "type": "null"
            }
          ],
          "description": "Only search this layer."
        },
        "limit": {
          "description": "The most results to return: 1 to 25. Defaults to 10.",
          "format": "uint32",
          "maximum": 25,
          "minimum": 1,
          "type": [
            "integer",
            "null"
          ]
        },
        "query": {
          "description": "Search terms, separated by whitespace: 1 to 200 characters.",
          "maxLength": 200,
          "minLength": 1,
          "type": "string"
        }
      },
      "required": [
        "query"
      ],
      "title": "SearchArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### `memory_read`

<!-- generated: tool memory_read -->

```text
memory_read(url: string)
```

Description sent to the model:

```text
Read one memory whole by its URL (user://<category>, agent://<agent>, machine://<machine>/<name>, or project://<project>/<name>). Returns its description, keywords, body, and revision; send the revision to memory_write, memory_edit, or memory_delete to change it.
```

Only when the session has the `global` runtime attached and the agent or the session overlay has an `allow` or `ask` rule for `memory`. `memory_search` needs no rule of its own: each result is checked against the `memory` rules for its URL instead.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `url` | `string` | yes | The memory URL. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "url": {
          "description": "The memory URL.",
          "type": "string"
        }
      },
      "required": [
        "url"
      ],
      "title": "ReadArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### `memory_write`

<!-- generated: tool memory_write -->

```text
memory_write(body: string, description?: string, keywords?: string[], revision?: string, url: string)
```

Description sent to the model:

```text
Create or replace one memory. Without `revision` the memory must not exist yet; to replace one, send the revision memory_read returned. A project:// memory needs a project that exists: projects are created from a client, not by writing memory. user:// categories cannot be created, only changed. Every agent already has an agent://<agent> memory, whose description the daemon sets and whose keywords always include the agent ID and its mode: leave `description` out for agent:// (any you send is replaced) and add keywords freely. Every project has a project://<project>/overview, shown in full to the project's sessions, whose description the daemon sets too: leave it out there as well. Give every other memory a one-line description, and each memory the keywords a later search would use; for a project, include what identifies it across machines: repository names, remote URLs, and `<machine>:<path>` strings for each place the work lives. Never save secrets, credentials, or personal data the user did not ask you to keep. Prefer editing an existing memory over adding a near-duplicate.
```

Only when `memory_read` is sent too. A delegated run may write only its own agent's memory, `agent://<agent>`.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `body` | `string` | yes | The memory's Markdown body; may be empty. |
| `description` | `string` | no | One line, 1 to 200 characters, shown in indexes and search results. Required except for agent:// and a project's overview, whose description the daemon sets. |
| `keywords` | `string[]` | no | Up to 32 keywords of 1 to 64 characters a later search would use; none when left out. |
| `revision` | `string` | no | The revision memory_read returned, to replace an existing memory; leave it out to create one. |
| `url` | `string` | yes | The memory URL. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "body": {
          "description": "The memory's Markdown body; may be empty.",
          "type": "string"
        },
        "description": {
          "description": "One line, 1 to 200 characters, shown in indexes and search results.\nRequired except for agent:// and a project's overview, whose\ndescription the daemon sets.",
          "type": [
            "string",
            "null"
          ]
        },
        "keywords": {
          "description": "Up to 32 keywords of 1 to 64 characters a later search would use;\nnone when left out.",
          "items": {
            "type": "string"
          },
          "type": "array"
        },
        "revision": {
          "description": "The revision memory_read returned, to replace an existing memory;\nleave it out to create one.",
          "type": [
            "string",
            "null"
          ]
        },
        "url": {
          "description": "The memory URL.",
          "type": "string"
        }
      },
      "required": [
        "url",
        "body"
      ],
      "title": "WriteArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### `memory_edit`

<!-- generated: tool memory_edit -->

```text
memory_edit(new: string, old: string, revision: string, url: string)
```

Description sent to the model:

```text
Replace the one occurrence of `old` in a memory's body with `new`, at the revision memory_read returned. Fails when `old` occurs zero or several times; include more context to make it unique. An agent:// memory keeps the description and keywords the daemon gives it, and a project's overview its description. Never save secrets.
```

Only when `memory_read` is sent too. A delegated run may write only its own agent's memory, `agent://<agent>`.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `new` | `string` | yes | Its replacement. |
| `old` | `string` | yes | Text that occurs exactly once in the body. |
| `revision` | `string` | yes | The revision memory_read returned. |
| `url` | `string` | yes | The memory URL. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "new": {
          "description": "Its replacement.",
          "type": "string"
        },
        "old": {
          "description": "Text that occurs exactly once in the body.",
          "type": "string"
        },
        "revision": {
          "description": "The revision memory_read returned.",
          "type": "string"
        },
        "url": {
          "description": "The memory URL.",
          "type": "string"
        }
      },
      "required": [
        "url",
        "old",
        "new",
        "revision"
      ],
      "title": "EditArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### `memory_delete`

<!-- generated: tool memory_delete -->

```text
memory_delete(revision: string, url: string)
```

Description sent to the model:

```text
Delete one memory at the revision memory_read returned. machine:// and project:// memories are deleted. user:// and agent:// memories cannot be deleted: for those, the call clears the body and keeps the file with its description and keywords, so the category or agent file stays.
```

Only when `memory_read` is sent too. A delegated run may write only its own agent's memory, `agent://<agent>`.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `revision` | `string` | yes | The revision memory_read returned. |
| `url` | `string` | yes | The memory URL. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "revision": {
          "description": "The revision memory_read returned.",
          "type": "string"
        },
        "url": {
          "description": "The memory URL.",
          "type": "string"
        }
      },
      "required": [
        "url",
        "revision"
      ],
      "title": "DeleteArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

## Runtime tools

### `list_runtimes`

<!-- generated: tool list_runtimes -->

```text
list_runtimes()
```

Description sent to the model:

```text
List the daemon's machines and runtimes, as they are now: each runtime's capabilities, whether it is connected or starts when used, its home directory, and this session's own attachments on it. Name a runtime capability from here in delegate_subagent's `capabilities`, as `<runtime id>/<capability name>`. A runtime that connects later shows up on the next call.
```

Only when the session has the `global` runtime attached and the agent or the session overlay has an `allow` or `ask` rule for `list_runtimes`, which `primary` and `all` agents have by default.

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "description": "`list_runtimes` takes no arguments.",
      "title": "ListRuntimesArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

`list_runtimes` reads the daemon's state at each call. Its output is grouped
by machine, the daemon's own first and the rest by name, and within a machine
by runtime ID; a runtime the daemon cannot place on a machine goes under
`<unplaced>`. `global` is never listed as a runtime: a `<global/>` element,
with a comment, says when the root of the caller's tree has it. Only the
calling session's own attachments are shown. A runtime that is not connected
keeps the machine, platform, and home the daemon last saw it with. The output
is never truncated.

```text
<runtimes>
  <!-- global is attached to this session's root, so every subagent in this tree has it, whether or not its capabilities list it. -->
  <global/>
  <machine name="laptop" platform="linux">
    <runtime id="local" connected="true" home="/home/u">
      <capability name="coding" takes_workspace="true" description="Read, write, edit, and run commands in a workspace"/>
      <attachment name="local_web" capability="coding" workspace="/home/u/web" status="ready"/>
    </runtime>
  </machine>
  <machine name="labpc" platform="linux">
    <runtime id="labpc" connected="false" starts_on_use="true" home="/home/u">
      <capability name="coding" takes_workspace="true" description="Read, write, edit, and run commands in a workspace"/>
    </runtime>
  </machine>
</runtimes>
```

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
| [`list_agents`](#list_agents) | *none* |
| [`cron_create`](#cron_create) | `cron` |
| [`cron_list`](#cron_list) | *none* |
| [`cron_update`](#cron_update) | `cron` |
| [`cron_delete`](#cron_delete) | `cron` |
| [`cron_run`](#cron_run) | `cron` |

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
  Its result says so: ``The user interrupted this run. Subagent `<handle>`
  keeps running; its <subagent_report> arrives when its turn ends.``,
  followed by the handle's note.

Every other interrupted call ends with a result that names who stopped it:
`Interrupted by the user before it finished.` for `run.cancel`, and for
other cancellations a wording that names their cause, such as
`Interrupted by engine shutdown before it finished.`
([background tools spec, section 14](../specs/background-tools.md#14-addendum-2026-10-23-interrupted-calls-name-their-cause)).
The engine waits at most 2 s for a call to stop on its own. A call still
running then ends as cancelled with `; cleanup deadline elapsed` added to
that result, its late result is dropped, and the run records `RunCancelled`
without waiting further. Calls in the background belong to the session, not the run, and
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

`read` numbers lines from 1, and its `offset` is the 1-based line (or
directory entry) to start at: `offset=1`, the default, is the first line, and
an `offset` of 0 is refused. `limit`, 2000 lines or entries when left out,
must be positive. An `offset` past the last line or entry is an error, except
that line 1 of an empty file is an empty page. The path is resolved when the
call is prepared; the content is read when it executes. A text line longer
than 4 KiB is cut and ends with a `... [line truncated: N more bytes]` marker.
A page that leaves lines or entries out ends with a continuation marker that
names the 1-based `offset` to read next: `[N more lines; continue with
offset=M]` (or `entries` for a directory) when the `limit` stopped it, and
`[page cut at the result size limit; continue with offset=M]` when it would
have exceeded the result size limit. The metadata reports the same `offset`
as `next_offset`. Media files are checked against the size limits before they
are read in full.

`memory_read` and `read_artifact` page the same way: the same 1-based
`offset`, the same `limit` and its default, the same numbering, line cut, page
cap, and continuation marker ([Memory tools](#memory-tools),
[read_artifact](#read_artifact)).

`write` takes the file's complete `content` and says whether it
`Created <path> (N bytes).` or `Replaced <path> (N bytes).`. `edit` requires
`oldString` to occur exactly once, or at least once with `replaceAll`, and
reports `Edited <path>: N replacement(s).`. When it occurs some other number
of times, the call fails with `` `<oldString>` occurs N times in <path>;
include more context so it occurs once or set replaceAll``, the echoed text
cut to 200 characters. A path that is not there fails `read` and `edit` with
`<path> does not exist`. All three refuse arguments they do not define.
Preparation checks the replacement against the current
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
Read a file or a directory. A text file comes back as numbered lines, `N: text`, counting from 1. `offset` is the 1-based line to start at (default 1) and `limit` the most lines to return (default 2000). A line longer than 4 KiB is cut and ends with `... [line truncated: N more bytes]`, and a page stops before it passes the result size limit (about 2 MiB). A page that leaves lines out ends with a note such as `[120 more lines; continue with offset=2001]`: read again from that offset. An offset past the last line is an error. A directory lists its entries, one per line with `/` after a folder's name, paged by the same 1-based `offset` and `limit`. An image, video, or other media file the model accepts is attached instead.
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `filePath` | `string` | yes | Path to the file or directory. Relative paths resolve against the session working directory. |
| `limit` | `integer` | no | The most lines, or directory entries, to return. Defaults to 2000. |
| `offset` | `integer` | no | The 1-based line, or directory entry, to start at. Defaults to 1. |

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
          "description": "The most lines, or directory entries, to return. Defaults to 2000.",
          "format": "uint",
          "minimum": 0,
          "type": [
            "integer",
            "null"
          ]
        },
        "offset": {
          "description": "The 1-based line, or directory entry, to start at. Defaults to 1.",
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
Write a whole file: create it, or replace everything it holds with `content`, overwriting it without a check. Missing parent folders are created. The write is atomic: the file holds either its old content or all of the new. The result says whether the file was created or replaced. To change part of a file, use `edit`.
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `content` | `string` | yes | Everything the file will hold. |
| `filePath` | `string` | yes | Path to the file. Relative paths resolve against the session working directory. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "content": {
          "description": "Everything the file will hold.",
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
Replace exact text in a UTF-8 text file, atomically. `oldString` is matched exactly, whitespace and indentation included, and must occur exactly once in the file: when it occurs zero or several times the edit fails before it is approved and changes nothing, so include more surrounding text to make it unique, or set `replaceAll` to replace every occurrence. The line numbers `read` shows are not part of the file. Text files only; use `write` to create a file or replace all of it.
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `filePath` | `string` | yes | Path to the file. Relative paths resolve against the session working directory. |
| `newString` | `string` | yes | The text that replaces it. |
| `oldString` | `string` | yes | The exact text to replace, whitespace and indentation included; not empty. It must occur exactly once in the file unless `replaceAll` is set. |
| `replaceAll` | `boolean` | no | Replace every occurrence of `oldString` instead of exactly one. Defaults to false. Default: `false`. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "filePath": {
          "description": "Path to the file. Relative paths resolve against the session working directory.",
          "type": "string"
        },
        "newString": {
          "description": "The text that replaces it.",
          "type": "string"
        },
        "oldString": {
          "description": "The exact text to replace, whitespace and indentation included; not\nempty. It must occur exactly once in the file unless `replaceAll` is\nset.",
          "type": "string"
        },
        "replaceAll": {
          "default": false,
          "description": "Replace every occurrence of `oldString` instead of exactly one.\nDefaults to false.",
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
| Result | `file_excerpt` of the page (`start_line` is the 1-based `offset`, `total_lines` the file's), `table` (`name`, `type`) of a directory page, or `media` of the retained attachment | `diff` against the text the write replaced | `diff` built from the ranges the edit replaced |

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
| `timeout` | `integer` | no | Milliseconds before the command is killed; 0 or less never times out. Default: `120000`. |

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
          "description": "Milliseconds before the command is killed; 0 or less never times out.",
          "format": "int64",
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
line, numbered from 1; its 1-based offset 6 starts at the body. Follow the
continuation marker, or `next_offset`, for successive pages. Metadata is separate and is not paged.

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
stream="<stream>", offset=...)` hint, where the digest identifies the manifest
and `offset` is the 1-based line of the first line the preview left out, as
`read_artifact` takes it. (The stream's `next_offset` in `retained_output`
stays the zero-based index of that line.) Single-output hints omit `stream`
and the heading. A partial-line byte cut points back to that line, so readback
does not skip unseen text. Aggregate preview construction also
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
arguments. `read` and `read_artifact` cut each line after 4 KiB and stop a page
before it passes the 2 MiB output limit, ending it with the `offset` to read
next; a `get_subagent_result` page above that limit fails with a
resource-limit tool error. No page is silently truncated. The
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
Read retained tool output by line, as `read` reads a file. A truncated result or elided history names the artifact (and stream) to read in a read_artifact hint, with the 1-based `offset` of the first line it left out: read from there. Lines come back numbered, `N: text`, counting from 1. `offset` is the 1-based line to start at (default 1) and `limit` the most lines to return (default 2000). A line longer than 4 KiB is cut and ends with `... [line truncated: N more bytes]`, and a page stops before it passes the result size limit (about 2 MiB). A page that leaves lines out ends with a note such as `[120 more lines; continue with offset=2001]`: read again from that offset. An offset past the last line is an error.
```

Always, whatever the agent's permission rules: it reads only output retained by the session's own tree.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `artifact` | `string` | yes | The artifact's 64-character lowercase SHA-256 digest, as a truncation hint names it. |
| `limit` | `integer` | no | The most lines to return. Defaults to 2000. |
| `offset` | `integer` | no | The 1-based line to start at. Defaults to 1. |
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
          "description": "The most lines to return. Defaults to 2000.",
          "format": "uint",
          "minimum": 0,
          "type": [
            "integer",
            "null"
          ]
        },
        "offset": {
          "description": "The 1-based line to start at. Defaults to 1.",
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
contain their complete individual text. A compaction retry artifact holds the
elided result's text as it was, or, for content that is not plain text, a
line-structured form: a `[tool content: <kind>]` header line, then each part
under its own `[part N: text|json|file ...]` line, text keeping its own line
breaks, JSON pretty-printed over several lines, and a file named by its media
type and filename without its bytes. Its marker says so and exposes a usable
ID without requiring durable elision events. Artifacts earlier builds stored
are read as they are: their non-text content is one line of serialized
tool-content JSON, of which a page shows the first 4 KiB.

Without `stream`, `read_artifact` reads stored text as-is, including manifest
JSON; it does not merge streams. `stream` selects a declared name from the generic
manifest. This is content-based and works for `results`, `diagnostics`, or any
other valid declared name, not just Bash streams. Unknown names and selection on
non-manifests are errors. Stored `ArtifactReference` values keep the internal
`artifact://sha256/<digest>` form, which is not a `read_artifact` argument.

A page reads as a `read` page of a file does, without the `<path>` wrapper:
lines numbered from 1 (`N: text`), a 1-based `offset` (default 1, and 0 is
refused), a positive `limit` (default 2000, with no maximum), each line cut
after 4 KiB with a `... [line truncated: N more bytes]` marker, the page
stopped before it passes the result size limit, and a continuation marker
(`[N more lines; continue with offset=M]`, or `[page cut at the result size
limit; continue with offset=M]`) when lines follow it, reported again as the
1-based `next_offset` in the metadata. An `offset` past the last line is an
error. Bytes that are not UTF-8 show as replacement characters. Missing and
corrupt artifacts produce tool errors.

```json
{"artifact":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stream":"results","offset":1,"limit":200}
```

### Display

| | `read_artifact` |
|---|---|
| Hint | `file_read` |
| Title | `Read artifact <12 hex>[/<stream>]` |
| Arguments | `key_value` of `artifact`, and `stream`, `offset`, and `limit` when given |
| Preview | – |
| Result | `file_excerpt` of the page: `path` the title's `<12 hex>[/<stream>]`, no `language`, `start_line` the 1-based `offset`, `total_lines` the artifact's |

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
`[… 200 lines omitted. Read more: get_subagent_result(session_id="explore_1a2b3c4d", offset=21)]`;
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
Read the current status and latest result of a subagent you delegated, such as the full output a <subagent_report> cut short. This call returns immediately; it never waits. Use the subagent's handle, e.g. "explore_1a2b3c4d". Only your own subagents are visible. The result comes back as numbered lines, `N: text`, counting from 1, as `read` returns a file: `offset` is the 1-based line to start at (default 1) and `limit` the most lines to return (default 2000); a line longer than 4 KiB is cut, and a page that leaves lines out ends with a note such as `[120 more lines; continue with offset=2001]`. A <subagent_report> cut short names the offset to continue from.
```

Only when the agent can delegate to at least one subagent at this depth.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `limit` | `integer` | no | The most lines to return. Defaults to 2000. Default: `2000`. |
| `offset` | `integer` | no | The 1-based line of the result to start at. Defaults to 1. Default: `1`. |
| `session_id` | `string` | yes | Handle (agent_type + 8 hex, e.g. "coder_9f8e7d6b") or full UUID of one of your subagents. |

??? note "Parameters JSON Schema"

    ```json
    {
      "additionalProperties": false,
      "properties": {
        "limit": {
          "default": 2000,
          "description": "The most lines to return. Defaults to 2000.",
          "maximum": 4294967295,
          "minimum": 1,
          "type": "integer"
        },
        "offset": {
          "default": 1,
          "description": "The 1-based line of the result to start at. Defaults to 1.",
          "maximum": 4294967295,
          "minimum": 1,
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
| Arguments | `key_value` of `agent`, `description`, `resume` and `inherit context` when set, and `prompt` | `key_value` of `session`, and `offset` (when not 1) and `limit` when given | `key_value` of `name`, and `arguments` when not empty |
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

They follow the [filesystem tools](#filesystem-tools) where they can.
`memory_read` pages a memory's body as `read` pages a file: under the
metadata header, the body's lines numbered from 1, a 1-based `offset`
(default 1), a `limit` (default 2000), the same 4 KiB line cut, page cap,
continuation marker, and error for an `offset` past the last body line.
`memory_write` reports `Created <url> (N bytes).` or `Replaced <url> (N
bytes).`, `memory_edit` `Edited <url>: 1 replacement.`, and a memory that is
not there is `<url> does not exist`. `memory_edit` takes `oldString` and
`newString`, as `edit` does, and preparation checks that `oldString` occurs
exactly once in the body, so an edit that cannot apply fails before anyone is
asked to approve it, with `` `<oldString>` occurs N times in the body of
<url>; include more context so it occurs once`` (the echoed text cut to 200
characters). What stays different on purpose: an edit matches the body only
and has no `replaceAll`; a replacing write needs the session to have read the
memory as it is now, and is refused otherwise with an `operation_changed`
error; a bad memory URL is an `invalid_url` error; a delete is final; the
permission actions are `memory` and `memory_write` by URL; and the write
tools run one at a time.

### `memory_search`

<!-- generated: tool memory_search -->

```text
memory_search(layer?: "user" | "agent" | "machine" | "project", limit?: integer, query: string)
```

Description sent to the model:

```text
Search memory: notes saved in earlier sessions about the user, agents, machines, and projects. Terms are matched case-insensitively against each memory's keywords, description, and URL segments (user, agent, machine, or project, then the names after it), never its body; results are ranked by keyword hits first. Each result shows a memory's URL and description, whether it is related to this session, whether you may change it, and its age, then its keywords; a result you need approval to read shows only its first two lines. `layer` narrows the search to `user`, `agent`, `machine`, or `project` memories.
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
memory_read(limit?: integer, offset?: integer, url: string)
```

Description sent to the model:

```text
Read one memory by its URL (user://<category>, agent://<agent>, machine://<machine>/<name>, or project://<project>/<name>). Returns whether it is related to this session and whether you may change it, when it was created and modified, its size and its size limit, its description and keywords, and then its body as numbered lines, `N: text`, counting from 1. `offset` is the 1-based body line to start at (default 1) and `limit` the most lines to return (default 2000). A line longer than 4 KiB is cut and ends with `... [line truncated: N more bytes]`, and a page that leaves lines out ends with a note such as `[120 more lines; continue with offset=2001]`: read again from that offset. An offset past the last body line is an error. The line numbers are not part of the memory.
```

Only when the session has the `global` runtime attached and the agent or the session overlay has an `allow` or `ask` rule for `memory`. `memory_search` needs no rule of its own: each result is checked against the `memory` rules for its URL instead.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `limit` | `integer` | no | The most body lines to return. Defaults to 2000. |
| `offset` | `integer` | no | The 1-based body line to start at. Defaults to 1. |
| `url` | `string` | yes | The memory URL. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "limit": {
          "description": "The most body lines to return. Defaults to 2000.",
          "format": "uint",
          "minimum": 0,
          "type": [
            "integer",
            "null"
          ]
        },
        "offset": {
          "description": "The 1-based body line to start at. Defaults to 1.",
          "format": "uint",
          "minimum": 0,
          "type": [
            "integer",
            "null"
          ]
        },
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
memory_write(body: string, description?: string, keywords?: string[] | string, new?: boolean, reason?: string, url: string)
```

Description sent to the model:

```text
Create or replace one memory, following the memory instructions in the system prompt. To replace a memory, read it first. A new memory like one that exists is refused with the near ones listed: edit one of those instead, or, if it is new, write it again with `new: true` and a one-line `reason`. Give each memory a one-line description that states its fact (agent:// memories and project overviews keep the daemon's: leave it out there) and the keywords a later search would use. A project:// memory needs a project that exists.
```

Only when `memory_read` is sent too and some memory URL is both readable (an `allow` or `ask` `memory` rule matches it) and writable (an `allow` or `ask` `memory_write` rule matches it). A delegated run whose agent document has no memory keys gets its own agent's memory, `agent://<agent>`, by default.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `body` | `string` | yes | The memory's Markdown body; may be empty. |
| `description` | `string` | no | One line, 1 to 200 characters, that states the memory's fact; shown in indexes and search results. Required except for agent:// and a project's overview, whose description the daemon sets. |
| `keywords` | `string[] \| string` | no | Up to 32 keywords of 1 to 64 characters a later search would use, as a list or one comma-separated string; none when left out. |
| `new` | `boolean` | no | True to create a memory that looks like ones that exist, after checking it is new; needs `reason`. |
| `reason` | `string` | no | With `new`: one line saying why it is not one of the near memories. |
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
          "description": "One line, 1 to 200 characters, that states the memory's fact; shown\nin indexes and search results. Required except for agent:// and a\nproject's overview, whose description the daemon sets.",
          "type": [
            "string",
            "null"
          ]
        },
        "keywords": {
          "anyOf": [
            {
              "items": {
                "type": "string"
              },
              "type": "array"
            },
            {
              "type": "string"
            }
          ],
          "description": "Up to 32 keywords of 1 to 64 characters a later search would use,\nas a list or one comma-separated string; none when left out."
        },
        "new": {
          "description": "True to create a memory that looks like ones that exist, after\nchecking it is new; needs `reason`.",
          "type": "boolean"
        },
        "reason": {
          "description": "With `new`: one line saying why it is not one of the near memories.",
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
memory_edit(newString: string, oldString: string, url: string)
```

Description sent to the model:

```text
Replace the one occurrence of `oldString` in a memory's body with `newString`, following the memory instructions in the system prompt. `oldString` is matched exactly against the body only, never the description or keywords, and must occur exactly once: when it occurs zero or several times the edit fails before it is approved and changes nothing, so include more surrounding text to make it unique. There is no replace-all. The line numbers memory_read shows are not part of the body.
```

Only when `memory_read` is sent too and some memory URL is both readable (an `allow` or `ask` `memory` rule matches it) and writable (an `allow` or `ask` `memory_write` rule matches it). A delegated run whose agent document has no memory keys gets its own agent's memory, `agent://<agent>`, by default.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `newString` | `string` | yes | The text that replaces it. |
| `oldString` | `string` | yes | The exact text to replace; it must occur exactly once in the body. |
| `url` | `string` | yes | The memory URL. |

??? note "Parameters JSON Schema"

    ```json
    {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "additionalProperties": false,
      "properties": {
        "newString": {
          "description": "The text that replaces it.",
          "type": "string"
        },
        "oldString": {
          "description": "The exact text to replace; it must occur exactly once in the body.",
          "type": "string"
        },
        "url": {
          "description": "The memory URL.",
          "type": "string"
        }
      },
      "required": [
        "url",
        "oldString",
        "newString"
      ],
      "title": "EditArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### `memory_delete`

<!-- generated: tool memory_delete -->

```text
memory_delete(url: string)
```

Description sent to the model:

```text
Delete one memory. Nothing is kept: the deletion is final.
```

Only when `memory_read` is sent too and some memory URL is both readable (an `allow` or `ask` `memory` rule matches it) and writable (an `allow` or `ask` `memory_write` rule matches it). A delegated run whose agent document has no memory keys gets its own agent's memory, `agent://<agent>`, by default.

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
      "title": "DeleteArgs",
      "type": "object"
    }
    ```

<!-- /generated -->

### Display

Each memory tool call is one row. Its title names the memory's URL, which
the TUI and the web client draw as a link that opens the memory in the
Memory panel or view, a click target of its own; the rest of the row
expands and collapses it. A write's, an edit's, and a delete's diff is
built as `write`'s and `edit`'s are (five lines of context, real line
numbers), labelled with the memory's URL, in `markdown`, over the memory
file without the `created` and `modified` stamp lines the daemon rewrites
on every save, as the memory editors show it.

| | `memory_search` | `memory_read` | `memory_write` | `memory_edit` | `memory_delete` |
|---|---|---|---|---|---|
| Hint | `search` | `file_read` | `file_write` | `file_edit` | `file_write` |
| Title | `Search memory` | `Read memory <url>` | `Write memory <url>` | `Edit memory <url>` | `Delete memory <url>` |
| Arguments | `key_value` of `query`, `layer`, and `limit` | `key_value` of `offset` and `limit` when the call gives them | `key_value` of `reason` when given; without a preview, also `description`, `keywords`, and `body` | – ; without a preview, `key_value` of `oldString` and `newString` | – |
| Preview | – | – | `diff` against the memory preparation finds (`created` when there is none) | `diff` of the replacement applied to the memory preparation read | `diff` of the memory's removed text (`deleted`), when it exists |
| Result | a `link` per memory found, its URL with its description as the label, or `No memories match.` | `file_excerpt` of the body's page, as `read` shows a file's: `path` the URL, `language` `markdown`, `start_line` the 1-based `offset`, `total_lines` the body's | `diff` against the memory the write replaced (`created` for a new one) | `diff` built from the range the edit replaced, or a line diff of the whole file when the daemon also rewrote its frontmatter (an `agent://` file, a project's overview) | `diff` of the text the memory held (`deleted`) |

A `link` block whose `url` is a memory URL (`user://`, `agent://`,
`machine://`, or `project://`) opens that memory in the clients.

## Runtime tools

### `list_runtimes`

<!-- generated: tool list_runtimes -->

```text
list_runtimes()
```

Description sent to the model:

```text
List the daemon's machines and runtimes, as they are now: each runtime's capabilities, whether it is connected or starts when used, its home directory, the ACP agents it offers (external coding agents such as Claude Code; a session runs one on the attached runtime that offers it), and this session's own attachments on it. Name a runtime capability from here in delegate_subagent's `capabilities`, as `<runtime id>/<capability name>`. A runtime that connects later shows up on the next call.
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

### `list_agents`

<!-- generated: tool list_agents -->

```text
list_agents(for?: "delegate" | "cron")
```

Description sent to the model:

```text
List the agents you can pick: for `delegate` the subagents delegate_subagent can start from here, for `cron` the agents a new-session cron job can run. Each shows its mode, description, presets, models, and the permission rules its document declares, with the actions left to the daemon's defaults on one `defaults:` line; the defaults themselves are listed once at the end. An ACP agent (an external coding agent such as Claude Code, marked `acp`) shows the runtimes that offer it and whether it runs here: it runs on the one attached runtime that offers it, and not with none or several. Leave `for` out to list both.
```

Only when the session has the `global` runtime attached and the run is offered `delegate_subagent` or a cron tool. It has no permission of its own.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `for` | `"delegate" \| "cron"` | no | List only the agents usable for this |

??? note "Parameters JSON Schema"

    ```json
    {
      "additionalProperties": false,
      "properties": {
        "for": {
          "description": "List only the agents usable for this",
          "enum": [
            "delegate",
            "cron"
          ],
          "type": "string"
        }
      },
      "type": "object"
    }
    ```

<!-- /generated -->

`list_agents` reads the agent registry of the calling run's preset. For
`delegate` it lists the run's delegation targets; for `cron` every enabled
`primary` or `all` agent, the ones a new-session job can run. Each agent
shows its mode, description, presets, models, what it is usable for, and
the permission keys its document declares, each in full with `${agent_id}`
written as the agent's ID; the actions it names no key for, which get the
daemon's defaults, share one `defaults:` line. An agent usable both as a
delegation target and for a new-session job shows its rules as a root run
and as a subagent when they differ, each block with its own `defaults:`
line. After the agents comes the defaults section, once: what the defaults
are for a root run and for a subagent (as the listed agents need them),
which ones only some modes get, and what `${session_machine}`,
`${session_project}`, and `${workspace_dir}` stand for. The result ends
with `Actions not listed are denied.` ([cron spec](../specs/cron.md)
section 8.1).

## Cron tools

The cron tools manage the daemon's scheduled prompts
([cron spec](../specs/cron.md)). Every one but `cron_list` is checked under
`cron`, with the target kind of each job it touches as a resource:
`session`, `subagent`, or `new`; `cron_update` that changes a job's kind is
checked for both. `cron_list` shows only the jobs whose kind the rules allow
or ask for. A target an agent names is `"self"`, a session below its own,
or a new-session template; anything else is refused whatever the rules say.
Agents never set a job's permission mode or pin its model.

### `cron_create`

<!-- generated: tool cron_create -->

```text
cron_create(delivery?: "queue" | "steer", max_runs?: integer, name?: string, prompt: string, schedule: object | object, target?: string | object)
```

Description sent to the model:

```text
Schedule a prompt: once after a delay ({"in": "20m"}) or on a five-field cron schedule ({"cron": "0 3 * * *"}). `target`: "self" (the default) fires into this session; a subagent's handle (e.g. "explore_1a2b3c4d") fires into that subagent's session, which must be below this one; {"new": {...}} starts a new root session per fire, whose agent, runtimes, and project default to this session's. A new session's agent must be able to run as a root (call list_agents with for="cron"). `delivery` (in-session jobs): "queue" (default) waits for the current turn, "steer" joins it. Schedules are in the daemon host's local time. Each fire arrives marked as a <scheduled_job>, never as the user, and is not user consent. Prefer cron_update on an existing job to creating a near duplicate. max_runs defaults to 1, so a job fires once unless told otherwise; 0 runs until the job is deleted or disabled.
```

Only when the session has the `global` runtime attached and the agent or the session overlay has an `allow` or `ask` rule for `cron`, which a root run of any agent but an internal one has by default; a delegated run has none unless its agent document grants it.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `delivery` | `"queue" \| "steer"` | no |  |
| `max_runs` | `integer` | no | Cron schedules only: how many fires before the job finishes, default 1; 0 or less runs until the job is deleted or disabled |
| `name` | `string` | no | A short name, at most 80 characters |
| `prompt` | `string` | yes | What to do at each fire |
| `schedule` | `object \| object` | yes | {"cron": "<minute hour day-of-month month day-of-week>"} in host local time, or {"in": "<n>s\|m\|h\|d"} to fire once after a delay (1s to 365d) |
| `target` | `string \| object` | no | "self" (the default), a subagent's handle, or {"new": {...}} |

??? note "Parameters JSON Schema"

    ```json
    {
      "additionalProperties": false,
      "properties": {
        "delivery": {
          "enum": [
            "queue",
            "steer"
          ],
          "type": "string"
        },
        "max_runs": {
          "description": "Cron schedules only: how many fires before the job finishes, default 1; 0 or less runs until the job is deleted or disabled",
          "maximum": 4294967295,
          "type": "integer"
        },
        "name": {
          "description": "A short name, at most 80 characters",
          "type": "string"
        },
        "prompt": {
          "description": "What to do at each fire",
          "type": "string"
        },
        "schedule": {
          "description": "{\"cron\": \"<minute hour day-of-month month day-of-week>\"} in host local time, or {\"in\": \"<n>s|m|h|d\"} to fire once after a delay (1s to 365d)",
          "oneOf": [
            {
              "additionalProperties": false,
              "properties": {
                "cron": {
                  "type": "string"
                }
              },
              "required": [
                "cron"
              ],
              "type": "object"
            },
            {
              "additionalProperties": false,
              "properties": {
                "in": {
                  "type": "string"
                }
              },
              "required": [
                "in"
              ],
              "type": "object"
            }
          ]
        },
        "target": {
          "description": "\"self\" (the default), a subagent's handle, or {\"new\": {...}}",
          "oneOf": [
            {
              "type": "string"
            },
            {
              "additionalProperties": false,
              "properties": {
                "new": {
                  "additionalProperties": false,
                  "properties": {
                    "agent": {
                      "description": "The agent each new session runs; this session's agent when omitted",
                      "type": "string"
                    },
                    "permission_rules": {
                      "description": "Rules added to each new session, e.g. {\"action\": \"bash\", \"resource\": \"git *\", \"effect\": \"allow\"}",
                      "items": {
                        "properties": {
                          "action": {
                            "type": "string"
                          },
                          "effect": {
                            "enum": [
                              "allow",
                              "ask",
                              "deny"
                            ],
                            "type": "string"
                          },
                          "resource": {
                            "type": "string"
                          },
                          "runtime": {
                            "type": "string"
                          }
                        },
                        "required": [
                          "action",
                          "resource",
                          "effect"
                        ],
                        "type": "object"
                      },
                      "type": "array"
                    },
                    "project": {
                      "description": "A project's name; this session's project when omitted",
                      "type": "string"
                    },
                    "runtimes": {
                      "description": "Runtimes besides global, as list_runtimes names them; this session's when omitted",
                      "items": {
                        "additionalProperties": false,
                        "properties": {
                          "capability": {
                            "type": "string"
                          },
                          "runtime_id": {
                            "type": "string"
                          },
                          "workspace": {
                            "type": "string"
                          }
                        },
                        "required": [
                          "runtime_id"
                        ],
                        "type": "object"
                      },
                      "type": "array"
                    }
                  },
                  "type": "object"
                }
              },
              "required": [
                "new"
              ],
              "type": "object"
            }
          ]
        }
      },
      "required": [
        "prompt",
        "schedule"
      ],
      "type": "object"
    }
    ```

<!-- /generated -->

### `cron_list`

<!-- generated: tool cron_list -->

```text
cron_list(target?: "session" | "subagent" | "new")
```

Description sent to the model:

```text
List the daemon's cron jobs your permissions let you see: id, name, schedule, next fire (local time and UTC), target, whether enabled, and the last fire's outcome. Pass `target` (session, subagent, or new) to list one kind.
```

Only next to the other cron tools. It needs no rule of its own: it lists the jobs whose target kind the `cron` rules allow or ask for.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `target` | `"session" \| "subagent" \| "new"` | no |  |

??? note "Parameters JSON Schema"

    ```json
    {
      "additionalProperties": false,
      "properties": {
        "target": {
          "enum": [
            "session",
            "subagent",
            "new"
          ],
          "type": "string"
        }
      },
      "type": "object"
    }
    ```

<!-- /generated -->

### `cron_update`

<!-- generated: tool cron_update -->

```text
cron_update(delivery?: "queue" | "steer", enabled?: boolean, id: string, max_runs?: integer, name?: string, prompt?: string, schedule?: object | object, target?: string | object)
```

Description sent to the model:

```text
Change a cron job: any member of cron_create, and `enabled` to disable or re-enable it (enabling resets its failure count). A new schedule or target computes the next fire from now. Schedules are in the daemon host's local time. Each fire arrives marked as a <scheduled_job>, never as the user, and is not user consent. Prefer cron_update on an existing job to creating a near duplicate. max_runs defaults to 1, so a job fires once unless told otherwise; 0 runs until the job is deleted or disabled.
```

Only when the session has the `global` runtime attached and the agent or the session overlay has an `allow` or `ask` rule for `cron`, which a root run of any agent but an internal one has by default; a delegated run has none unless its agent document grants it.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `delivery` | `"queue" \| "steer"` | no |  |
| `enabled` | `boolean` | no |  |
| `id` | `string` | yes | The job's ID, e.g. "k3v9q2xa" |
| `max_runs` | `integer` | no | Cron schedules only: how many fires before the job finishes, default 1; 0 or less runs until the job is deleted or disabled |
| `name` | `string` | no |  |
| `prompt` | `string` | no |  |
| `schedule` | `object \| object` | no | {"cron": "<minute hour day-of-month month day-of-week>"} in host local time, or {"in": "<n>s\|m\|h\|d"} to fire once after a delay (1s to 365d) |
| `target` | `string \| object` | no | "self", a subagent's handle, or {"new": {...}}; unchanged when omitted |

??? note "Parameters JSON Schema"

    ```json
    {
      "additionalProperties": false,
      "properties": {
        "delivery": {
          "enum": [
            "queue",
            "steer"
          ],
          "type": "string"
        },
        "enabled": {
          "type": "boolean"
        },
        "id": {
          "description": "The job's ID, e.g. \"k3v9q2xa\"",
          "type": "string"
        },
        "max_runs": {
          "description": "Cron schedules only: how many fires before the job finishes, default 1; 0 or less runs until the job is deleted or disabled",
          "maximum": 4294967295,
          "type": "integer"
        },
        "name": {
          "type": "string"
        },
        "prompt": {
          "type": "string"
        },
        "schedule": {
          "description": "{\"cron\": \"<minute hour day-of-month month day-of-week>\"} in host local time, or {\"in\": \"<n>s|m|h|d\"} to fire once after a delay (1s to 365d)",
          "oneOf": [
            {
              "additionalProperties": false,
              "properties": {
                "cron": {
                  "type": "string"
                }
              },
              "required": [
                "cron"
              ],
              "type": "object"
            },
            {
              "additionalProperties": false,
              "properties": {
                "in": {
                  "type": "string"
                }
              },
              "required": [
                "in"
              ],
              "type": "object"
            }
          ]
        },
        "target": {
          "description": "\"self\", a subagent's handle, or {\"new\": {...}}; unchanged when omitted",
          "oneOf": [
            {
              "type": "string"
            },
            {
              "additionalProperties": false,
              "properties": {
                "new": {
                  "additionalProperties": false,
                  "properties": {
                    "agent": {
                      "description": "The agent each new session runs; this session's agent when omitted",
                      "type": "string"
                    },
                    "permission_rules": {
                      "description": "Rules added to each new session, e.g. {\"action\": \"bash\", \"resource\": \"git *\", \"effect\": \"allow\"}",
                      "items": {
                        "properties": {
                          "action": {
                            "type": "string"
                          },
                          "effect": {
                            "enum": [
                              "allow",
                              "ask",
                              "deny"
                            ],
                            "type": "string"
                          },
                          "resource": {
                            "type": "string"
                          },
                          "runtime": {
                            "type": "string"
                          }
                        },
                        "required": [
                          "action",
                          "resource",
                          "effect"
                        ],
                        "type": "object"
                      },
                      "type": "array"
                    },
                    "project": {
                      "description": "A project's name; this session's project when omitted",
                      "type": "string"
                    },
                    "runtimes": {
                      "description": "Runtimes besides global, as list_runtimes names them; this session's when omitted",
                      "items": {
                        "additionalProperties": false,
                        "properties": {
                          "capability": {
                            "type": "string"
                          },
                          "runtime_id": {
                            "type": "string"
                          },
                          "workspace": {
                            "type": "string"
                          }
                        },
                        "required": [
                          "runtime_id"
                        ],
                        "type": "object"
                      },
                      "type": "array"
                    }
                  },
                  "type": "object"
                }
              },
              "required": [
                "new"
              ],
              "type": "object"
            }
          ]
        }
      },
      "required": [
        "id"
      ],
      "type": "object"
    }
    ```

<!-- /generated -->

### `cron_delete`

<!-- generated: tool cron_delete -->

```text
cron_delete(id: string)
```

Description sent to the model:

```text
Delete a cron job. Sessions its fires started stay.
```

Only when the session has the `global` runtime attached and the agent or the session overlay has an `allow` or `ask` rule for `cron`, which a root run of any agent but an internal one has by default; a delegated run has none unless its agent document grants it.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | `string` | yes | The job's ID, e.g. "k3v9q2xa" |

??? note "Parameters JSON Schema"

    ```json
    {
      "additionalProperties": false,
      "properties": {
        "id": {
          "description": "The job's ID, e.g. \"k3v9q2xa\"",
          "type": "string"
        }
      },
      "required": [
        "id"
      ],
      "type": "object"
    }
    ```

<!-- /generated -->

### `cron_run`

<!-- generated: tool cron_run -->

```text
cron_run(id: string)
```

Description sent to the model:

```text
Run a cron job now. An in-session job fires into its session as a scheduled fire would. A new-session job runs as your subagent with the job's agent, rules, and runtimes and the prompt as its first input: in the foreground (default) the call waits for its first turn and returns the result; in the background it returns the subagent's handle at once and its turns arrive as <subagent_report>s. A run now counts toward neither max_runs nor the failure count.
```

Only when the session has the `global` runtime attached and the agent or the session overlay has an `allow` or `ask` rule for `cron`, which a root run of any agent but an internal one has by default; a delegated run has none unless its agent document grants it.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | `string` | yes | The job's ID, e.g. "k3v9q2xa" |

??? note "Parameters JSON Schema"

    ```json
    {
      "additionalProperties": false,
      "properties": {
        "id": {
          "description": "The job's ID, e.g. \"k3v9q2xa\"",
          "type": "string"
        }
      },
      "required": [
        "id"
      ],
      "type": "object"
    }
    ```

<!-- /generated -->

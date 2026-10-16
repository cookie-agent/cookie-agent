# Runtime Protocol

The runtime protocol connects the daemon to a *runtime*: a source of tools and
workspace context that runs outside the engine (see the
[runtimes specification](../specs/runtimes.md)). The daemon's built-in `local`
coding runtime speaks it on a task of the daemon's own process over an
in-memory frame pair; a configured [`[runtimes.<id>]`](../guide/runtimes.md)
template runs a command, typically `ssh host cookie runtime coding --stdio
--workspace {workspace}`, and speaks it over the child's stdin and stdout. The
local and remote paths are one code path. The global runtime does not speak
it; the engine calls it in-process.

The wire types live in `cookie_agent_runtime_api::protocol`, the JSON-RPC peer
both ends run in `cookie_agent_runtime_api::rpc`, the coding runtime's server
in `cookie_agent_coding_runtime::server`, and the daemon's client in the
engine's `runtimes::remote` module.

## Framing

- **JSON-RPC 2.0**, one message per frame: a frame of an in-memory transport,
  or one line of newline-delimited JSON over a byte stream such as a child's
  stdio (`rpc::LineTransport`).
- **Both ends send requests.** The daemon drives tools; a runtime reports
  progress and background jobs back while a call runs. Request IDs are
  unsigned integers chosen by the sender.
- **Bounded.** No frame exceeds 48 MiB (`protocol::MAX_FRAME_BYTES`), which fits
  an execute reply carrying the largest attachment as base64 next to a full
  tool result. A sender refuses to send a larger message; a receiver that gets
  one closes the connection.
- **Strict.** Every message and every params or result object rejects unknown
  fields. A frame that is not a valid request, notification, or reply closes
  the connection. A request whose params do not match its method is answered
  with `-32602`, and the connection stays up.

## Version

The protocol version is an integer, currently **3**
(`protocol::RUNTIME_PROTOCOL_VERSION`). The daemon sends its version in
`runtime/initialize`; a runtime that speaks another version fails the request
with code `-32010` and a message naming both versions, and the daemon checks
the version in the reply the same way. There is no negotiation.
Version 3 removed `tool/stdin` and the `stdin` flag of `tool/execute`: tools
no longer take input while they run.

## Lifecycle

1. The daemon connects (in-process: a new frame pair and a server task; a
   configured template: it runs the command in its own process group, a job
   object on Windows, with the daemon's environment plus the template's `env`)
   and sends `runtime/initialize`, all within the template's
   `startup_timeout_ms`. Every other request before it is answered with
   `-32012`; a second initialize with `-32011`. A child's stderr is kept as a
   bounded tail (the last 32 lines, at most 4 KiB) that failure reasons end
   with; nothing but protocol messages may appear on its stdout.
2. Sessions use the instance: the daemon prepares, executes, and releases
   calls, and asks for `AGENTS.md` context at each root run's admission. A
   coding runtime reports its MCP servers' status as it changes
   (`mcp/changed`) and a changed tool list (`runtime/tools_changed`).
3. The daemon sends `runtime/shutdown` when it shuts down, waits up to the
   template's `shutdown_grace_ms` for the answer, and closes the transport
   (a child's stdin); a child that has not exited after another grace is
   killed with its process group. The runtime answers, cancels every running
   call and background job, and stops. It also stops when its transport
   ends.

When the connection is lost while the instance is in use, every call in
flight fails with `runtime_unavailable` and every running background job
completes as failed (`runtime … exited before the job finished`). The
instance becomes `reconnecting`: its tools stay published, calls fail fast
with `runtime_unavailable`, and the daemon starts it again up to 3 times,
after 1 s, 2 s, and 4 s, sending `runtime/initialize` with the canonical
workspace the instance reported. A restart that reports the same identity
(runtime ID, kind, attachment name, workspace) revives the instance under a
new generation: its tools are listed again, and a call published before the
restart fails as `operation_changed`. When every attempt fails, the instance
becomes `failed`, its tools leave the session's tool list, and clients are
told the reason. A failed instance starts again when a session that uses it
is created or opened, or on `runtimes.reconnect`. Calls are never retried
transparently.

## Methods

| Method | Direction | Kind | Purpose |
|---|---|---|---|
| `runtime/initialize` | daemon → runtime | request | Version, runtime ID, workspace, settings; returns identity, tools, skills |
| `runtime/tools_changed` | runtime → daemon | notification | The whole new tool list |
| `runtime/context` | daemon → runtime | request | `AGENTS.md` context for a run's preset |
| `runtime/shutdown` | daemon → runtime | request | Stop gracefully |
| `tool/prepare` | daemon → runtime | request | Prepare one turn's calls to this runtime |
| `tool/revalidate` | daemon → runtime | request | Check a prepared call can still run as approved |
| `tool/execute` | daemon → runtime | request | Run a prepared call; returns its terminal result |
| `tool/release` | daemon → runtime | notification | Discard a prepared call that will not run |
| `tool/progress` | runtime → daemon | request | Display and output deltas of an executing call |
| `tool/cancel` | daemon → runtime | notification | Cancel an executing call |
| `job/started` | runtime → daemon | request | An executing call reserves a background job |
| `job/progress` | runtime → daemon | request | Output of a background job |
| `job/completed` | runtime → daemon | notification | A background job finished with its report |
| `job/kill` | daemon → runtime | notification | Kill a background job |
| `mcp/changed` | runtime → daemon | notification | The status of the runtime's MCP servers, and the ones that failed |
| `mcp/reconnect` | daemon → runtime | request | Reconnect one MCP server, or retry every failed one |

An unknown request is answered with `-32601`; an unknown notification is
ignored.

### `runtime/initialize`

| Params field | Meaning |
|---|---|
| `version` | The daemon's protocol version. |
| `runtime_id` | The template's runtime ID, `[a-z][a-z0-9_]{0,31}`. |
| `workspace` | The workspace path the daemon asks for, when the template has one, exactly as a session gave it: the runtime expands a leading `~` and resolves it on its own machine. Optional: a runtime started for one workspace (`--workspace`) uses its own, and refuses one that resolves elsewhere. |
| `settings.tool_output` | The `[tool_output]` preview limits: `max_lines`, `max_bytes`, `head_lines`, `tail_lines`. |
| `settings.agent_md` | `max_bytes`: files larger than this are skipped. At most the `AGENTS.md` event bound. |
| `settings.mcp_servers` | The template's MCP servers: `name` and `config`, the server's table as the [`[mcp.servers.<name>]` schema](../guide/mcp.md#accepted-fields) reads it. |

A runtime rejects settings it does not support with `-32011`, as it does a
workspace it cannot open; a runtime that hosts no MCP servers rejects a
nonempty `mcp_servers`. A coding runtime parses each server strictly and rejects the request when one does not parse or enables OAuth,
which stays with the daemon's global runtime.

A coding runtime connects every enabled server before it answers: the reply
comes once each server's first connection attempt and tool listing have
finished, successfully or not, so the template's `startup_timeout_ms` must
cover them. A server that fails publishes no tools; its status says why. The
servers' tools join the declared tools as `<server>_<tool>`, with permission
`mcp`, after the runtime's built-in tools, which keep their names: a server
whose tool would take another tool's name fails, as in the global runtime.
OAuth is never used: a remote server connects with its static headers only.

| Result field | Meaning |
|---|---|
| `version` | The runtime's protocol version. |
| `kind` | The runtime's kind, a self-declared label such as `global` or `coding`: a lowercase ASCII letter followed by up to 15 lowercase letters, digits, or hyphens. The daemon shows it to clients and in approval reviews and never branches on it; version 1 allowed only `global` or `coding`. |
| `attachment_name` | The name sessions attach the instance under: `<runtime ID>_<workspace basename>`, sanitized, for a coding runtime. |
| `workspace` | The canonical workspace on the runtime's machine; absent for a runtime without one. `AGENTS.md` is loaded only from runtimes that report one. |
| `platform` | Optional: the operating system, as Rust's `std::env::consts::OS` names it, for a runtime where it matters. Shown in the system prompt's `<runtimes>` block. Version 1 required it and also carried `shell`. |
| `tools.tools` | Tool declarations: `name`, `collision` (`prefix` or `fallback`), `permission`, `description`, `parameters` (JSON Schema), `concurrency`, `result_truncation`, `output`. `permission` is the action the tool is checked under: a label matching `[a-z][a-z0-9_]{0,31}`, either one the daemon's own tools use (such as `bash` or `read`, which takes file paths) or the runtime's own (such as `browse`). An action no rule names is denied, so its tools stay hidden until an agent grants it. A tool without `permission` needs none: it is always published and every call is allowed without rules or approval, and its prepared calls report no `permission` either. Version 1 allowed only the nine built-in actions, and versions before 3 required `permission`. |
| `tools.prompt_sections` | System prompt sections (`title`, `body`) the tools contribute. |
| `skills.skills` | Project skills: the `SKILL.md` text, `source`, absolute `path` and `base_dir` on the runtime's machine, and a sample of `supporting_files`. The daemon parses the text with the same rules as local discovery. |
| `skills.discoveries` | Every `SKILL.md` found, with `precedence_winner`. |
| `mcp_servers` | The status of each MCP server after its first connection attempt, in name order: `name`; `state`, one of `connected`, `connecting`, `reconnecting`, `failed`, `disabled`, or `disconnected`; `message`, why it failed; and `tools`, its tool names. Absent when the runtime hosts none. |

### `runtime/tools_changed`

Params are a new `tools` object as in the initialize result. The daemon uses
it from the next model turn on.

### `mcp/changed`, `mcp/reconnect`

`mcp/changed` carries `servers`, the status of every MCP server the runtime
hosts as in the initialize result, and `failures`, the servers that failed to
connect or dropped and did not reconnect since the last report, each with
`server` and `reason`. The runtime sends it after its initialize reply,
whenever a status changes; a failure may arrive in a report of its own. The
daemon shows the statuses in `mcp.server.list` and passes each failure to the
clients viewing a session attached to the instance.

When a connected server drops, the runtime restarts it up to 3 times, after
1 s, 2 s, and 4 s. Meanwhile it is `reconnecting`, its tools stay declared,
and calls fail with `runtime_unavailable`. A successful attempt restores the
server with its current tool list; when every attempt fails, it becomes
`failed`, a `runtime/tools_changed` drops its tools, and `mcp/changed`
reports the failure.

`mcp/reconnect` params: `server`, optional. With a name, the runtime closes
that server's connection and connects it again; without one, it retries every
failed server. It answers `{}` once the attempts have finished, or
`{"error": …}` for an unknown or disabled server or a failed attempt; the new
status follows in `mcp/changed`. While one of the instance's servers is
`failed`, the daemon sends it without a name when a session that uses the
instance is created or opened, or when the instance is attached or retried,
and the session's next run waits for the answer; it names a server for
`mcp.server.reconnect`.

### `runtime/context`

Params: `preset`, the run's agent preset (optional; must be a valid agent ID).
Result: `agent_md`, the `AGENTS.md` entries in prompt order, and `skipped`,
files over the size limit. The runtime applies the lookup to its workspace:
the preset's `.cookie-agent/agents/<preset>/AGENTS.md`, else
`.cookie-agent/agents/AGENTS.md`, then `<workspace>/AGENTS.md`. A file that
exists but cannot be read fails the request with `-32014` and `data.path`.

### `tool/prepare`

Params: `session`, `run`, `turn` (the agent, model, adapter, adapter family,
and model capabilities of the turn), and `calls` (`id`, `name`, `arguments`;
at most 256). The runtime prepares a turn's calls together, so calls on one
target chain. The result has one entry per call, tagged by `status`:

- `prepared`: `prepared_id`; `operation`, the prepared operation identity;
  `policy_labels`, one per operation resource; `normalized_arguments`;
  `permission`, the permission action; `permission_resource`, the resource of
  the normalized arguments; `presentation`, the call's title and display
  argument; and `serialization_key`, lowercase hex, when calls sharing it must
  run one at a time.
- `failed`: `error` and the `presentation` to show for the failed call.

The runtime keeps a prepared call under its `prepared_id` until it is executed
or released. Concurrency is declared per tool, not per call.

### `tool/revalidate`, `tool/release`

Both take `prepared_id`. Revalidate answers `{}` or `{"error": …}`; release is
a notification.

### `tool/execute`

| Params field | Meaning |
|---|---|
| `prepared_id` | The call to run; executing consumes it. |
| `session`, `run`, `turn` | As for prepare. |
| `background` | The call may start background jobs. |

The result's `outcome` is tagged by `status`: `completed` with the terminal
`output` (single or streamed), the persisted `result`, and `failed`; or
`failed` with an `error`. Next to it:

- `media`: the bytes of every attachment the result references, as `sha256`
  and base64 `data`, at most 25 MiB in all. The runtime gives each retained
  attachment the reference the daemon's content-addressed artifact store will
  give it; the daemon stores the bytes, checks each digest and reference, and
  rejects a result that references media it did not receive.
- `launched_jobs`: the jobs the call reserved and launched. The daemon
  releases the others.

While a call runs, the runtime sends its progress with `tool/progress`
(`prepared_id`, and `progress`: `tool_call_id`, `message`, `display`, and
`output` chunks, under the 64 KiB and 8-stream delta bounds). It waits for
each answer, so output reaches the daemon's capture in order, the daemon's
capture limits reach the tool, and every chunk precedes the execute reply.
The daemon owns capture, previews, and artifact retention.

`tool/cancel` cancels the call; it still answers its execute request. The
runtime registers an execution as the request arrives, so a cancellation sent
right after execute reaches it.

### Background jobs

A call that starts a background job (such as `bash` with `background`)
reserves it with `job/started` (`prepared_id`, `job`); the daemon reserves its
side, which owns the job's output capture and its completion message, and
answers `{}` or an error. The job's output arrives with `job/progress` (`job`,
`progress`), and its end with `job/completed` (`job`, `report`: `summary`,
`element`, `attributes`, `body`). The daemon commits the completion as it does
for an in-process job: only once the starting call is committed as completed.
When the starting call never commits, is reverted, or its session is deleted,
the daemon sends `job/kill`.

## Errors

| Code | Meaning |
|---|---|
| `-32601` | Unknown method. |
| `-32602` | Params do not match the method. |
| `-32603` | Internal error. |
| `-32010` | Protocol version mismatch. |
| `-32011` | The runtime rejects the initialize request or its settings. |
| `-32012` | A request before `runtime/initialize`. |
| `-32013` | Unknown prepared call or job. |
| `-32014` | Reading the runtime's context failed; `data.path` names the file. |
| `-32015` | The reply would exceed the frame bound. |

Tool-level failures travel inside results as `{"kind": …, "message": …}`, where
`kind` is the tool error kind (`failed`, `operation_changed`,
`resource_limit`, `permission_denied`, `timeout`, `runtime_unavailable`, …).

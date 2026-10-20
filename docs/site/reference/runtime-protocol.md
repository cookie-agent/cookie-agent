# Runtime Protocol

The runtime protocol connects the daemon to a *runtime*: a source of tools and
workspace context that runs outside the engine (see the
[runtimes specification](../specs/runtimes.md)). The daemon's built-in `local`
coding runtime speaks it on a task of the daemon's own process over an
in-memory frame pair; a configured [`[runtimes.<id>]`](../daemon/runtimes.md)
template runs a command, typically `ssh host cookie runtime serve --stdio`,
and speaks it over the child's stdin and stdout. The
local and remote paths are one code path. The global runtime does not speak
it; the engine calls it in-process.

The wire types live in the Apache-2.0 protocol crate's
`cookie_agent_protocol::runtime_protocol` and the JSON-RPC peer both ends run
in `cookie_agent_protocol::peer`, so the plugin SDK shares them;
`cookie_agent_runtime_api::protocol` and `::rpc` re-export them. The coding
runtime's server is in `cookie_agent_coding_runtime::server` and the daemon's
client in the engine's `runtimes::remote` module. Plugins declare and run
their tools with this protocol's tool messages over their own connection
(see [Plugin development](../development/plugins.md#tools)).

## Connections

The daemon reaches a runtime in one of two ways, and speaks the same
protocol over either:

- **Spawned**: it runs a template's command and frames the child's stdio,
  one message per line, as above.
- **Dial-in**: the runtime opens a WebSocket to the daemon's
  `/runtime?id=<runtime ID>` endpoint with the daemon's bearer token in an
  `Authorization: Bearer` header, one message per text frame. The daemon
  then sends `runtime/initialize` exactly as for a spawned runtime, with no
  MCP servers: a dial-in runtime hosts its own. An ID the daemon configures,
  or one already connected, is refused by closing the connection.

## Framing

- **JSON-RPC 2.0**, one message per frame: a frame of an in-memory transport,
  or one line of newline-delimited JSON over a byte stream such as a child's
  stdio (`peer::LineTransport`).
- **Both ends send requests.** The daemon drives tools; a runtime reports
  progress back while a call runs. Request IDs are chosen by the sender and
  may be integers or strings, as JSON-RPC 2.0 allows; a reply echoes its
  request's ID. Both ends of this repository send integers.
- **The envelope is information.** Members JSON-RPC does not define are
  ignored.
- **Bounded.** No frame exceeds 48 MiB (`runtime_protocol::MAX_FRAME_BYTES`), which fits
  an execute reply carrying the largest attachment as base64 next to a full
  tool result. A sender refuses to send a larger message; a receiver that gets
  one closes the connection.
- **Information and commands.** Every message is information except the
  ones a runtime sends that ask the daemon to act: `tool/progress`,
  `tool/detachable`, and the `producer/*` requests. Information ignores
  members its reader does not know, at any depth, and reads an enum value it
  does not know as that enum's fallback (see [Open enums](#open-enums)), so
  either end may be newer. A command is checked against its schema before
  it is decoded: a member, or an enum value or tag, the schema does not list
  is refused with `-32602`, `data.code` `unknown_field` or `unknown_variant`,
  and `data.field` set to its JSON path, such as `progress.display.type`. A
  sender gates new fields of a command on the receiver's version. The one
  exception is a display block of a kind the daemon does not know in a
  `tool/progress` replacement: it is only shown, so the daemon drops it with
  a diagnostic instead (see [Display blocks](#display-blocks)).
- **Unreadable messages.** A frame that is not JSON, or not a request,
  notification, or reply, closes the connection. A request whose params do
  not match its method is answered with `-32602` (`invalid_params`, with
  serde's message as `detail`), and the connection stays up. A notification
  that cannot be read cannot be answered: the receiver reports it (the daemon
  in its log, a runtime on its standard error) and ignores it. The daemon
  also reports and ignores `runtime/tools_changed` from a runtime: only a
  plugin sends it.

### Open enums

An enum value a reader does not know reads as the fallback below. The
fallbacks are chosen so that an older reader stays safe:

| Value | Where | An unknown value reads as |
|---|---|---|
| `collision` | tool declarations | `prefix` |
| `concurrency` | tool declarations | `exclusive`, the most conservative |
| `result_truncation` | tool declarations | `bounded`: the daemon still bounds the result |
| tool error `kind` | prepare, revalidate, and execute results; `mcp/reconnect` | `failed`, a generic tool failure |
| MCP server `state` | `capability/open` results, `mcp/changed` | `disconnected`: no tools, and no retry as a failure |
| media `delivery` | `tool/prepare` and `tool/execute` turns | `unknown`: the tool does not attach that kind |
| display block `type` | prepare presentations, execute results, `tool/progress` replacements | dropped, with a diagnostic (see [Display blocks](#display-blocks)) |
| media `kind` | `tool/prepare` and `tool/execute` turns | a label: a tool does not attach a kind it does not know |

These are closed by design, since every reader must understand each value: a
prepare result's and an execute outcome's `status`, an output's and an
output declaration's `kind`, and a skill's `source`. The commands `producer/send`'s `mode` and
`producer/recovery/complete`'s outcome are closed as every command is. The
plugin protocol adds its own (see
[Plugin development](../development/plugins.md#compatibility)).

## Version

The protocol version is a semver string, currently **1.1.0**
(`cookie_agent_protocol::RUNTIME_PROTOCOL_VERSION`). The daemon sends its
version in `runtime/initialize` and the runtime answers with its own. Each end
reads the other's `protocol_version` from the raw JSON before it decodes
anything else, so a peer of another version is answered with
`unsupported_protocol`, naming both versions, rather than with a decoding
error of a shape the two do not share; a hello without a readable version
fails as `invalid_params`. Two versions are compatible when their majors are
equal: a minor only adds, so the two ends use what the lower minor includes,
and the patch never matters (see [Compatibility](compatibility.md)). A `0.x`
build from before the freeze, and a build from before semver versions, which
sends an integer, are refused the same way.

Both ends also send `features`: what their configuration enables, never what
a version already implies. None are defined yet for runtimes; an end ignores
a feature it does not know, and the daemon says so in its log.

Versions before 0.7.0 were integers. 3 removed `tool/stdin` and the `stdin`
flag of `tool/execute`: tools no longer take input while they run. It also
replaced the `job/*` messages and the `background` flag and `launched_jobs`
of `tool/execute` with `tool/detachable`, and added `background` and
`background_default` to tool declarations. 4 added producer messaging: the
`producer_messaging` capability, the `producer/*` requests, and the recovery
handshake (see [Producer messaging](#producer-messaging)). 5 added
capabilities: one connection serves every workspace and capability of a
runtime. `runtime/initialize` lost `workspace` and reports `capabilities`;
`capability/open` opens an instance for a workspace and returns what
`runtime/initialize` used to (identity, tools, skills, MCP servers);
`capability/tools_changed` replaced `runtime/tools_changed`; and
`tool/prepare`, `runtime/context`, `mcp/changed`, and `mcp/reconnect` name
their `instance` (see the [runtime capabilities spec](../specs/runtime-capabilities.md)).
6 removed the tool result's `truncation`, which nothing set: a result's
retained output is `retained_output`. 0.7.0 made the version a semver string,
added `engine_version` and `features` to the handshake, and moved errors to
the registry's codes (see [Errors](#errors)). 0.8.0 replaced the turn's
adapter family and model capabilities with `turn.model` (see
[`tool/prepare`](#toolprepare)). 1.0.0 froze the protocol: no `0.x` build
connects to a 1.x one. 1.1.0 added the optional `machine` of
`runtime/initialize`'s result, which the daemon groups runtimes into
machines by.

## Capabilities and instances

A runtime offers **capabilities**, such as `coding`. Sessions attach each one
separately, as `{ runtime_id, capability, workspace? }`. The daemon opens a
capability for a workspace (or none, for a capability that takes none) with
`capability/open`; the result is an **instance**, under a runtime-chosen
handle unique on the connection. One instance per (capability, canonical
workspace) is shared by every session attached to it, and stays open for the
connection's life. Tool calls, context, and MCP reports name the instance
they belong to; prepared IDs are unique per connection, so the messages
about one prepared call need no instance.

## Machines

Every runtime belongs to one **machine**, the computer it runs on. The
daemon assigns a runtime to a machine when it connects, from the `machine`
it reports in `runtime/initialize`: a machine the user pinned the runtime ID
to comes first, then the machine that stands for the reported ID, then a new
machine named after the host name. A runtime that reports no machine gets a
machine of its own, named after its runtime ID. The user regroups, renames,
and merges machines from any client; a runtime never learns its machine's
name. See the [memory spec](../specs/memory.md#3-machines).

## Lifecycle

1. The daemon connects (in-process: a new frame pair and a server task; a
   configured template: it runs the command in its own process group, a job
   object on Windows, with the daemon's environment plus the template's `env`)
   and sends `runtime/initialize`, all within the template's
   `startup_timeout_ms`. It connects once per runtime, when a session first
   needs one of its capabilities. Every other request before it is answered
   with `not_initialized`; a second initialize with `already_initialized`. A child's stderr is kept
   as a bounded tail (the last 32 lines, at most 4 KiB) that failure reasons
   end with; nothing but protocol messages may appear on its stdout.
2. The daemon opens an instance with `capability/open` for each capability
   and workspace its sessions attach, each within `startup_timeout_ms`.
3. Sessions use the instances: the daemon prepares, executes, and releases
   calls, and asks for `AGENTS.md` context at each root run's admission. A
   coding runtime reports each instance's MCP servers' status as it changes
   (`mcp/changed`) and a changed tool list (`capability/tools_changed`).
4. The daemon sends `runtime/shutdown` when it shuts down, waits up to the
   template's `shutdown_grace_ms` for the answer, and closes the transport
   (a child's stdin); a child that has not exited after another grace is
   killed with its process group. The runtime answers, cancels every running
   call, backgrounded ones included, and stops. It also stops when its transport
   ends.

When the connection is lost while it is in use, every call in flight fails
with `runtime_unavailable`; a backgrounded call reports that failure as its
background result. Every instance of the runtime becomes `reconnecting`: its
tools stay published, calls fail fast with `runtime_unavailable`, and the
daemon connects the runtime again up to 3 times, after 1 s, 2 s, and 4 s, and
opens each instance again with the canonical workspace it reported. One new
connection serves them all. An instance that comes back with the same
identity (runtime ID, capability, kind, attachment name, workspace) is
revived under a new generation: its tools are listed again, and a call
published before the restart fails as `operation_changed`. When every
attempt fails, the instance becomes `failed`, its tools leave the session's
tool list, and clients are told the reason. A failed instance starts again
when a session that uses it is created or opened, or on
`runtimes.reconnect`. Calls are never retried transparently.

## Methods

| Method | Direction | Kind | Purpose |
|---|---|---|---|
| `runtime/initialize` | daemon → runtime | request | Version, runtime ID, settings; returns the capabilities |
| `capability/open` | daemon → runtime | request | Open a capability for a workspace; returns the instance's identity, tools, skills |
| `capability/tools_changed` | runtime → daemon | notification | An instance's whole new tool list |
| `runtime/context` | daemon → runtime | request | An instance's `AGENTS.md` context for a run's preset |
| `runtime/shutdown` | daemon → runtime | request | Stop gracefully |
| `tool/prepare` | daemon → runtime | request | Prepare one turn's calls to an instance |
| `tool/revalidate` | daemon → runtime | request | Check a prepared call can still run as approved |
| `tool/execute` | daemon → runtime | request | Run a prepared call; returns its terminal result |
| `tool/release` | daemon → runtime | notification | Discard a prepared call that will not run |
| `tool/progress` | runtime → daemon | request | Display and output deltas of an executing call |
| `tool/cancel` | daemon → runtime | notification | Cancel an executing call |
| `tool/detachable` | runtime → daemon | notification | An executing call reached its detach point |
| `mcp/changed` | runtime → daemon | notification | The status of an instance's MCP servers, and the ones that failed |
| `mcp/reconnect` | daemon → runtime | request | Reconnect one of an instance's MCP servers, or retry every failed one |
| `producer/recovery/start` | daemon → runtime | notification | Producer messaging is open; restore tracked work, then report |
| `producer/recovery/complete` | runtime → daemon | request | The runtime's recovery outcome |
| `producer/register` | runtime → daemon | request | Register a named producer for an attached session |
| `producer/send` | runtime → daemon | request | Send a message through a registered producer |
| `producer/unregister` | runtime → daemon | request | Close a registration; its messages stay |
| `producer/discard` | runtime → daemon | request | Discard one of the runtime's waiting messages |

An unknown request is answered with `-32601`; an unknown notification is
ignored.

### `runtime/initialize`

| Params field | Meaning |
|---|---|
| `protocol_version` | The daemon's protocol version (see [Version](#version)). |
| `engine_version` | The daemon's own version, for logs. |
| `features` | What the daemon's configuration enables for the runtime; none are defined yet. |
| `runtime_id` | The runtime's ID, `[a-z][a-z0-9_]{0,31}`. |
| `settings.tool_output` | The `[tool_output]` preview limits: `max_lines`, `max_bytes`, `head_lines`, `tail_lines`. |
| `settings.agent_md` | `max_bytes`: files larger than this are skipped. At most the `AGENTS.md` event bound. |
| `settings.mcp_servers` | The template's MCP servers: `name` and `config`, the server's table as the [`[mcp.servers.<name>]` schema](../extend/mcp.md#accepted-fields) reads it. Each instance of a capability that hosts MCP servers runs them in its workspace. |

A runtime rejects settings it does not support with `initialize_rejected`; a runtime that
hosts no MCP servers rejects a nonempty `mcp_servers`. A coding runtime
parses each server strictly and rejects the request when one does not parse
or enables OAuth, which stays with the daemon's global runtime.

| Result field | Meaning |
|---|---|
| `protocol_version` | The runtime's protocol version. |
| `features` | Optional: what the runtime's configuration enables; none are defined yet. |
| `platform` | Optional: the operating system, as Rust's `std::env::consts::OS` names it, for a runtime where it matters. Shown in the system prompt with the runtime's machine. |
| `machine` | Optional, since 1.1.0: the machine the runtime runs on, `{ id, host_name? }`. `id` is a stable, opaque ID of 1 to 128 printable ASCII bytes; runtimes reporting one ID belong to one machine. `host_name` (at most 255 bytes) names a machine the daemon has not seen. `cookie runtime serve` reports the digest `cookie` uses as its machine ID and the host name; a runtime that cannot tell leaves it out, and the daemon reads a malformed one as left out. See [Machines](#machines). |
| `capabilities` | What the runtime offers: `name` (`[a-z][a-z0-9_]{0,31}`, unique within the runtime); `kind`, a self-declared label such as `coding` (a lowercase ASCII letter followed by up to 15 lowercase letters, digits, or hyphens) that the daemon shows to clients and in approval reviews and never branches on; `takes_workspace`; and an optional `description`. The coding runtime offers `coding`, which takes a workspace. |
| `producer_messaging` | Optional, default `false`: the runtime sends producer messages to the sessions attached to its instances (see [Producer messaging](#producer-messaging)). |

### `capability/open`

| Params field | Meaning |
|---|---|
| `capability` | One of the runtime's capabilities. |
| `workspace` | The workspace path, exactly as a session gave it, required exactly when the capability takes one: the runtime expands a leading `~` and resolves it on its own machine. A runtime started with `--root` refuses one outside its roots. |

The runtime answers `capability_open_failed` for a capability it does not
offer or a workspace it cannot open. Opening an instance that is already open (the same
capability and canonical workspace) returns it. A coding instance connects
every enabled MCP server before it answers: the reply comes once each
server's first connection attempt and tool listing have finished,
successfully or not, so the template's `startup_timeout_ms` must cover them.
A server that fails publishes no tools; its status says why. The servers'
tools join the declared tools as `<server>_<tool>`, with permission `mcp`,
after the runtime's built-in tools, which keep their names: a server whose
tool would take another tool's name fails, as in the global runtime. OAuth is
never used: a remote server connects with its static headers only.

| Result field | Meaning |
|---|---|
| `instance` | The instance's handle on this connection. |
| `attachment_name` | The name sessions attach the instance under: `<runtime ID>_<workspace basename>`, sanitized, for a coding instance. |
| `workspace` | The canonical workspace on the runtime's machine; absent for a capability without one. `AGENTS.md` is loaded only from instances that report one. |
| `tools.tools` | Tool declarations: `name`, `collision` (`prefix`, `override`, or `fallback`; see [collisions](../specs/runtimes.md#63-collisions)), `permission`, `description`, `parameters` (JSON Schema), `concurrency`, `result_truncation`, `output`, and optionally `background` and `background_default`. `background` says the tool supports the background: the daemon adds a `background` argument to its schema, strips it before `tool/prepare`, and may detach its calls (see [Background calls](#background-calls)); `background_default` is that argument's default and is only valid with `background`. `permission` is the action the tool is checked under: a label matching `[a-z][a-z0-9_]{0,31}`, either one the daemon's own tools use (such as `bash` or `read`, which takes file paths) or the runtime's own (such as `browse`). An action no rule names is denied, so its tools stay hidden until an agent grants it. A tool without `permission` needs none: it is always published and every call is allowed without rules or approval, and its prepared calls report no `permission` either. Version 1 allowed only the nine built-in actions, and versions before 3 required `permission`. |
| `tools.prompt_sections` | System prompt sections (`title`, `body`) the tools contribute. |
| `skills.skills` | Project skills: the `SKILL.md` text, `source`, absolute `path` and `base_dir` on the runtime's machine, and a sample of `supporting_files`. The daemon parses the text with the same rules as local discovery. |
| `skills.discoveries` | Every `SKILL.md` found, with `precedence_winner`. |
| `mcp_servers` | The status of each MCP server after its first connection attempt, in name order: `name`; `state`, one of `connected`, `connecting`, `reconnecting`, `failed`, `disabled`, or `disconnected`; `message`, why it failed; and `tools`, its tool names. Absent when the instance hosts none. |

### `capability/tools_changed`

Params: `instance` and its new `tools` object as in the open result. The
daemon uses it from the next model turn on. (A plugin, whose connection has
one implicit instance, sends `runtime/tools_changed` with the `tools` object
alone.)

### `mcp/changed`, `mcp/reconnect`

`mcp/changed` carries `instance`; `servers`, the status of every MCP server
the instance hosts as in the open result; and `failures`, the servers that
failed to connect or dropped and did not reconnect since the last report,
each with `server` and `reason`. The runtime sends it after its open reply,
whenever a status changes; a failure may arrive in a report of its own. The
daemon shows the statuses in `mcp.server.list` and passes each failure to the
clients viewing a session attached to the instance.

When a connected server drops, the runtime restarts it up to 3 times, after
1 s, 2 s, and 4 s. Meanwhile it is `reconnecting`, its tools stay declared,
and calls fail with `runtime_unavailable`. A successful attempt restores the
server with its current tool list; when every attempt fails, it becomes
`failed`, a `capability/tools_changed` drops its tools, and `mcp/changed`
reports the failure.

`mcp/reconnect` params: `instance`, and `server`, optional. With a name, the
runtime closes that server's connection and connects it again; without one,
it retries every failed server of the instance. It answers `{}` once the
attempts have finished, or `{"error": …}` for an unknown or disabled server
or a failed attempt; the new status follows in `mcp/changed`. While one of
the instance's servers is `failed`, the daemon sends it without a name when a
session that uses the instance is created or opened, or when the instance is
attached or retried, and the session's next run waits for the answer; it
names a server for `mcp.server.reconnect`.

### `runtime/context`

Params: `instance`, and `preset`, the run's agent preset (optional; must be a
valid agent ID). Result: `agent_md`, the `AGENTS.md` entries in prompt order, and `skipped`,
files over the size limit. The runtime applies the lookup to the instance's
workspace:
the preset's `.cookie-agent/agents/<preset>/AGENTS.md`, else
`.cookie-agent/agents/AGENTS.md`, then `<workspace>/AGENTS.md`. A file that
exists but cannot be read fails the request with `context_failed` and
`data.path`.

### `tool/prepare`

Params: `instance`, the instance whose tools the calls name; `session`,
`run`, `turn`; and `calls` (`id`, `name`, `arguments`; at most 256). `turn`
is `{ agent, model }`, where `model` is `{ model, adapter, input_media }`:
the model's key, its adapter label (such as `openai-responses`), and the
media kinds a tool may return to it. Each `input_media` entry is
`{ kind, mime_types, max_bytes, delivery }`: `kind` is `image`, `pdf`,
`audio`, or `video`; the limits are effective, already the lowest of the
model's capability and its adapter's delivery limit; and `delivery` is
`tool_result` (an attachment of the result) or `user_message` (a user message
after the result, for an adapter whose tool results cannot carry the kind). A
kind not listed is not accepted: a tool reports that it cannot attach the
file. A plugin's prepare carries no `instance`. The runtime prepares a turn's calls together, so calls on one
target chain. The result has one entry per call, tagged by `status`:

- `prepared`: `prepared_id`; `normalized_arguments`, the arguments as the
  call will run them (not `null`); `permission`, the permission action, which
  must be the one the tool declares, absent for a tool that needs none;
  `permission_resource`, the resource of the normalized arguments, absent for
  a call checked under the action alone; `presentation`, how clients show the
  call (see below); and `serialization_key`, lowercase hex, when calls sharing
  it must run one at a time.
- `failed`: `error` and the `presentation` to show for the failed call.

`presentation` is `{ title, hint?, primary_argument?, arguments?, preview? }`:
the title, an optional `hint` label for icons and grouping (such as `shell` or
`file_edit`; see the [compatibility spec](../specs/compatibility-v1.md#122-presentation-v2)),
the argument clients show next to it, and display blocks for the arguments
and for what the call will do. The daemon adds the tool's identity and fits
the blocks to the display budget.

The runtime keeps a prepared call under its `prepared_id` until it is executed
or released. Concurrency is declared per tool, not per call.

#### Approval identity

The daemon builds each prepared call's approval identity; a runtime never
states one, and a member it sends for one is ignored. The identity binds what
the runtime declared to the tool the daemon knows the call goes to, the
runtime attachment, plugin, or MCP server and the name it knows the tool by,
so no runtime can state another tool's identity:

- the normalized arguments' digest, over their JSON with object members in
  key order;
- one capability: the permission action (`tool` for a tool that needs none)
  with operation `<tool name>:call`;
- one resource of that action, `<action>:<SHA-256 of the resource>`
  (`<action>:unscoped` without one), bound to the tool's identity. A `bash`
  resource approves by command prefix and every other resource exactly. A
  `bash` command's and a path action's (`read`, `write`) binding lasts only
  for the daemon's process, since the runtime resolves it against its own
  machine at preparation; every other binding survives a restart, so a call
  of such a tool may be approved for its whole session tree;
- the execution context: the tool's identity and the instance's workspace.

Two calls are the same operation, for tree grants and the doom-loop guard,
exactly when these agree.

### `tool/revalidate`, `tool/release`

Both take `prepared_id`. Revalidate answers `{}` or `{"error": …}`; release is
a notification.

### `tool/execute`

| Params field | Meaning |
|---|---|
| `prepared_id` | The call to run; executing consumes it. |
| `session`, `run`, `turn` | As for prepare. |
| `background` | `true` when the model started the call in the background; omitted otherwise. A tool may use it to word its own results; it detaches the same way either way. |

The result's `outcome` is tagged by `status`: `failed` with an `error` the
call could not run past, or `completed` with the call's result:

| Member | Meaning |
|---|---|
| `output` | What the model reads, tagged by `kind`: `single` with the whole `text`, `named` with one text per declared stream, or `streamed` when the call's progress already sent it. |
| `title` | Optional: the result's title, at most 1 KiB without control characters; the tool's name when absent. |
| `display` | Optional: what clients show, a list of display blocks (see [Display blocks](#display-blocks)). A `single` output without one shows its text. |
| `metadata` | Optional: structured data the model reads with the output. |
| `attachments` | Optional: files the result carries, each `{ mime_type, filename?, byte_length, sha256, reference }`, at most 256 with the files of `additional_messages`. |
| `additional_messages` | Optional: at most 4 messages added to the model's history after the result, each `{ role, content }`. |
| `failed` | Optional, default `false`: the call failed, and the model reads its output as the failure. |

The daemon builds the result a session stores from it: it captures the
output, previews and retains it, records whether the call moved to the
background, and fits the display. Next to the outcome:

- `media`: the bytes of every attachment the result references, as `sha256`
  and base64 `data`, at most 25 MiB in all. The runtime gives each retained
  attachment the reference the daemon's content-addressed artifact store will
  give it; the daemon stores the bytes, checks each digest and reference, and
  rejects a result that references media it did not receive.

While a call runs, the runtime sends its progress with `tool/progress`
(`prepared_id`, and `progress`: `tool_call_id`, `message`, an optional
`display` delta, `append { index, text, stream? }` or `replace { blocks }`, and
`output` chunks, under the 64 KiB and 8-stream delta bounds). It waits for
each answer, so output reaches the daemon's capture in order, the daemon's
capture limits reach the tool, and every chunk precedes the execute reply.
The daemon owns capture, previews, and artifact retention.

`tool/cancel` cancels the call; it still answers its execute request. The
runtime registers an execution as the request arrives, so a cancellation sent
right after execute reaches it.

### Display blocks

A prepared call's `presentation`, an execute result's `display`, and a
`tool/progress` `replace` carry display blocks, typed units such as `text`,
`code`, `diff`, or `terminal` that clients render as they choose (see the
[compatibility spec](../specs/compatibility-v1.md#121-display-blocks)). The
daemon strips control characters a block may not carry and fits each list to
64 KiB, cutting whole lines, hunks, or rows. A runtime sends only the kinds
the daemon's version includes: a block of a kind the daemon does not know is
dropped, and the session records a `diagnostic` of kind
`unknown_display_block` whose source names the runtime (`runtime:<id>`) or
plugin (`plugin:<name>`), once per call for its progress and its result.

### Background calls

The daemon owns the background (see the [background tool calls
spec](../specs/background-tools.md)); a runtime only says when a call may be
detached. When an executing call of a tool declared with `background` reaches
its detach point, the runtime sends `tool/detachable` (`prepared_id`, an
optional `note` for the immediate result, such as a process ID, and
`reports_itself`, omitted when `false`). The daemon
may then detach the call: it commits an immediate result for it and keeps
waiting for its `tool/execute` reply, whose result it reports to the session
as a `<background_result>`. Nothing changes for the runtime: the call keeps
running, its progress keeps arriving with `tool/progress`, and `tool/cancel`
stops it, which is how a revert, a deleted session, or `stop_background`
reaches it. A detached call does not outlive the runtime's connection.

A call whose work reports its own results once detached sets
`reports_itself` (the daemon's `delegate_subagent` does, since its subagent
reports every turn). The daemon then only stops waiting: it drops the call
instead of awaiting its reply and sends no `<background_result>`, and
cancelling the run that waited for it ends the call without cancelling the
work. For a runtime's call, the daemon then sends no `tool/cancel`: the call
keeps running in the runtime, and whatever it reports later goes through its
[producer messaging](#producer-messaging).

### Producer messaging

A runtime that declares `producer_messaging` may put messages into the model
input of the sessions attached to its instances, as plugins do (see the
[external producers spec](../specs/external-producers.md)). It produces as
`external { source, name }`: the daemon derives `source`,
`runtime:<runtime ID>`, from the connection, and the runtime names each
producer it registers.

1. After `runtime/initialize`, the daemon sends `producer/recovery/start`.
   The runtime may send producer requests from then on; earlier ones are
   rejected. It restores whatever external work it tracks and answers
   `producer/recovery/complete` with `outcome` `{"status": "ready"}` or
   `{"status": "failed", "message": …}`. Until then its source is
   `starting`, and goal reminders of the sessions attached to it wait.
2. `producer/register` (`session_id`, `name`: `[a-z0-9][a-z0-9_.-]{0,63}`)
   returns `producer_id`. `producer/send` (`session_id`, `producer_id`,
   `mode` `steer` or `queue`, `idempotency_key`, `description`, `body`)
   returns `message_id` once the message is durably accepted; an identical
   retry returns the same receipt. `producer/unregister` (`session_id`,
   `producer_id`) and `producer/discard` (`session_id`, `message_id`) return
   `{}`. The params and results are the protocol crate's `Producer*` and
   `Recovery*` types, which the plugin protocol shares.
3. Every request may only address a session attached to one of the
   runtime's instances; any other fails with `producer_rejected`. The body must be non-empty and at
   most [`[producers] max_body_bytes`](../engine/producers.md).
4. Registrations belong to the connection. They end when it does or when the
   session detaches the runtime; a reconnected runtime registers again, and
   its retried sends deduplicate against what it sent before.

The coding runtime declares producer messaging when started with
`ServeOptions::producer_messaging`; its tools then reach these requests
through `ToolExecutionContext::producers`.

## Errors

Errors follow the scheme of the [compatibility spec](../specs/compatibility-v1.md#6-errors),
in both directions. A protocol error uses a standard JSON-RPC code; every
other error is `-32000` with a stable snake-case `data.code`. Each carries a
`message` a user can understand, and in `data` an optional `hint` (what to
do next) and `detail` (technical detail, such as a serde error or a path).
Receivers branch on `data.code`, never on `message`. The codes come from the
protocol crate's registry (`cookie_agent_protocol::errors`):

| Code | `data.code` | Meaning |
|---|---|---|
| `-32601` | `method_not_found` | Unknown method; `data.method` names it. |
| `-32602` | `invalid_params` | Params do not match the method; `detail` says why. |
| `-32602` | `unknown_field`, `unknown_variant` | A command carried a member, or an enum value or tag, its receiver does not know; `data.field` is its JSON path (see [Framing](#framing)). |
| `-32603` | `internal_error` | The receiver failed in a way it cannot describe further. |
| `-32002` | `unsupported_protocol` | The peer speaks an incompatible version; `data.supported_version` and `data.peer_version` name both (see [Version](#version)). |
| `-32000` | `already_initialized` | A second `runtime/initialize`. |
| `-32000` | `initialize_rejected` | The runtime rejects the initialize request or its settings. |
| `-32000` | `not_initialized` | A request before `runtime/initialize`. |
| `-32000` | `capability_open_failed` | `capability/open` for a capability the runtime does not offer or a workspace it cannot open; `data.capability` and `data.workspace` name what was asked. |
| `-32000` | `unknown_handle` | Unknown prepared call or instance. |
| `-32000` | `context_failed` | Reading the runtime's context failed; `data.path` names the file. |
| `-32000` | `reply_too_large` | The reply would exceed the frame bound. |
| `-32000` | `producer_rejected` | The daemon rejected a producer request: messaging is not open on the connection, the session is not attached to the runtime, or the operation failed. |

When the daemon shows a runtime's error to a user, such as a runtime that
fails to start or a call whose `tool/execute` request fails, it keeps the
runtime's `message` (and `detail`) and names the runtime.

Tool-level failures travel inside results as `{"kind": …, "message": …}`, where
`kind` is the tool error kind (`failed`, `operation_changed`,
`resource_limit`, `permission_denied`, `timeout`, `runtime_unavailable`, …).

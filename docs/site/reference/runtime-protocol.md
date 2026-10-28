# Runtime Protocol

The runtime protocol connects the daemon to a *runtime*: a source of tools and
workspace context that runs outside the engine (see the
[runtimes specification](../specs/runtimes.md)). The daemon's built-in `local`
coding runtime speaks it on a task of the daemon's own process over an
in-memory frame pair; a configured [`[runtimes.<id>]`](../runtimes/templates.md)
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
  MCP servers: a dial-in runtime hosts its own. A dial-in runtime gets 30
  seconds to answer `runtime/initialize` and each `capability/open`, and 3
  seconds to answer `runtime/shutdown`. See [Dial-in refusals](#dial-in-refusals)
  for how the daemon refuses one. Either end that ends the connection sends a close
  frame, normal closure (1000), and waits up to 2 seconds for the other's
  answer; a runtime that stops does so before it exits. The daemon reads a
  normal closure, going away (1001), or a close frame without a code as the
  runtime disconnecting, and another code, or a connection that ended
  without a close frame, as a failure with that code or error.

### Dial-in refusals

Since 1.2.0 the daemon says why it refuses a runtime that dials in, and
whether dialing in again can succeed (`runtime_protocol::dial_in`). A
request it can refuse from the request alone is answered before the
WebSocket upgrade with an HTTP status and the reason as the plain-text body:

| Status | Reason | Permanent |
|---|---|---|
| 400 | The runtime ID does not match `[a-z][a-z0-9_]{0,31}`. | yes |
| 401 | The bearer token is missing or wrong. | yes |
| 403 | The ID is reserved (`global`, `local`) or names a `[runtimes.<id>]` template the daemon configures, or the request carries an `Origin` header. | yes |
| 409 | A runtime is connected under the ID, or being accepted under it, now. The daemon may not have noticed yet that an earlier connection of the same runtime ended. | no |

A runtime that fails `runtime/initialize` after the upgrade, such as one of
another major version or one that does not answer in time, sees the daemon
close the connection with code 1011 and the reason as the close frame's
reason, cut to the frame's 123 bytes. That refusal is not permanent.

A runtime stops dialing in after a permanent refusal and dials in again,
after a wait, after any other, including a status this table does not list,
such as one a proxy answers with. `cookie runtime serve --daemon` prints the
reason either way, exits with an error on a permanent refusal, and checks
its own ID first, so a malformed or reserved one never reaches the daemon.
A daemon before 1.2.0 closes the connection after the upgrade without a
reason for an ID it refuses, which a runtime can only treat as transient.

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
| MCP server `state` | `capability/open` and `runtime/initialize` results, `mcp/changed`, `runtime/mcp_changed` | `disconnected`: no tools, and no retry as a failure |
| MCP server `scope` | the same | `workspace`: hosted by an instance |
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

The protocol version is a semver string, currently **1.11.0**
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
machines by. 1.2.0 made the daemon's answers to a refused dial-in part of
the protocol: HTTP statuses with the reason before the upgrade, close code
1011 with the reason after it, and which of them are permanent (see
[Dial-in refusals](#dial-in-refusals)). Its messages did not change; the
version records the contract a runtime may now rely on to stop dialing in.
1.3.0 added the optional `home` of `runtime/initialize`'s result: the
runtime's home directory, which clients show paths under as `~/…` and
permission rules expand a leading `~` against (see [Home](#home)).
1.4.0 added `runtime/ping`, which the daemon sends a quiet connection to
tell it from a dead one (see [Liveness](#liveness)); any answer counts, so
an older runtime's `-32601` does too.
1.5.0 added runtime-scoped MCP servers (see
[Runtime-scoped MCP servers](#runtime-scoped-mcp-servers)): an MCP server
report's optional `scope`; the optional `mcp_servers` of
`runtime/initialize`'s result, every server the runtime is configured with;
`runtime/mcp_changed` and `runtime/mcp_reconnect`; and a runtime's own tools,
outside any instance, which it declares with `runtime/tools_changed` and the
daemon prepares without an `instance`. A runtime keeps a daemon older than
1.5.0 working by hosting every server per workspace for it.
1.6.0 added an MCP server report's optional `filtered_tools`: how many of
the tools the server lists its `include_tools` and `exclude_tools` left
out. The runtime applies the filters itself, from the server's table, so
no message carries them.
1.7.0 added ACP agents (see [ACP agents](#acp-agents)): the optional
`acp_agents` of `runtime/initialize`'s settings and result,
`runtime/agents_changed`, the `agent/*` relay messages, and the MCP bridge
messages; and the display block `plan`.
1.8.0 added the skill source `official`, the daemon's official skills; a
runtime reports its project skills as before.
1.9.0 added an MCP server report's optional `server_tools`: every tool the
server lists, by the server's own name (the name its `include_tools` and
`exclude_tools` match) and in its order, each with `filtered: true` when the
filters left it out. Clients list them in their machines views.
1.10.0 added `runtime/settings_changed`, the daemon's notification of a
runtime's whole new settings, which a runtime applies without reconnecting
(see [Settings changes](#settings-changes)). A runtime that dials in now
gets its MCP servers and ACP agents from the daemon's `[runtimes.<id>]` as a
template does; a `cookie runtime serve` of this version no longer reads its
own machine's `[runtimes.local]`.
1.11.0 added, for [deferred tools](tools.md#deferred-tools), an MCP server
report's optional `instructions` (what the server sent when it connected,
at most 2,048 characters, absent when its configuration sets `instructions =
false`) and a `server_tools` entry's `always_load: true` when the server's
`always_load` or `always_load_tools` sends the tool up front. A report's
`tools` lists the unfiltered `server_tools` entries' published names in the
same order.

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

## Home

A runtime may report its **home directory** in `runtime/initialize`. Paths
are stored canonical and absolute everywhere, and the home is only how they
are shown and how permission rules name them:

- **Display.** Clients show a path of the runtime, such as an attachment's
  workspace, as `~` for the home itself and `~/rest` under it, and absolute
  otherwise. A runtime that reports no home is fine: clients show its paths
  absolute. The daemon passes the home on as `home` in `runtimes.list` (see
  the [protocol reference](protocol.md#runtimes)).
- **Permission rules.** A rule pattern `~` or `~/…` expands against the home
  of the runtime the tool runs on, so `read: ~/notes/**` means the notes
  under the home on that runtime's machine. On a runtime that reports no home
  the pattern stays literal and matches no absolute path (see
  [Agents](../extend/agents.md#paths-under-the-home-directory)).

The coding runtime expands a path argument's leading `~` against the same
canonical home, so the path a tool reads and the rule that governs it agree.

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
   (`mcp/changed`) and a changed tool list (`capability/tools_changed`), and
   its runtime-scoped servers' status (`runtime/mcp_changed`) and tools
   (`runtime/tools_changed`).
4. The daemon sends `runtime/shutdown` when it shuts down, waits up to the
   template's `shutdown_grace_ms` for the answer, and closes the transport
   (a child's stdin); a child that has not exited after another grace is
   killed with its process group. The runtime answers, cancels every running
   call, backgrounded ones included, and stops. It also stops when its transport
   ends.

## Liveness

A connection can die without closing: a dial-in machine that sleeps or
loses its network leaves a half-open WebSocket, and a hung `ssh` keeps a
spawned runtime's pipes open. So the daemon watches every runtime
connection it keeps (an in-process `local` runtime's aside):

- Once nothing has arrived on a connection for **15 s**, the daemon sends
  `runtime/ping`. Any frame counts as hearing from the runtime: the answer,
  an error answer such as an older runtime's `-32601`, or any other message.
- Once nothing has arrived for **45 s**, the daemon ends the connection
  ("nothing arrived from it for 45 s"), exactly as if it had closed: calls
  in flight fail with `runtime_unavailable`, a dial-in runtime shows as
  disconnected, and its instances reconnect, then fail, as below.
- Every request has a deadline. `tool/prepare`, `tool/revalidate`, and
  `runtime/context` get **30 s**; `mcp/reconnect` gets 6 minutes, since it
  may wait for a server to start or a user to sign in; `runtime/initialize`
  and `capability/open` get the template's `startup_timeout_ms`, or 30 s for
  a dial-in runtime; `runtime/shutdown` gets the shutdown grace. A request
  past its deadline fails as `runtime_unavailable` ("it did not answer
  `tool/prepare` within 30 s"), and the connection becomes suspect: the
  daemon pings it at once and ends it unless something arrives within
  **10 s** ("it did not answer a liveness check within 10 s").
  `tool/execute` has no deadline of its own, since a call runs as long as
  its tool allows, but a dead connection ends it within the 45 s above.
- Before a session gets an instance that is already open, as creating a
  session, attaching a runtime, or delegating with capabilities does, the
  daemon checks the connection: one heard from in the last 15 s passes;
  another must answer a ping within 10 s, or it is ended and the instance is
  not handed out.

A runtime answers `runtime/ping` with `{}` at once, before
`runtime/initialize` too. A dial-in runtime also watches its end: the
`cookie runtime` WebSocket sends a ping frame once the connection has been
quiet for 15 s, which the daemon answers with a pong, and dials in again once
nothing, a pong included, has arrived for 45 s. When a runtime dials in
while its old connection still looks open, the daemon refuses it as
connected (409) and checks the old connection at once, so a retry gets in
within about 10 s.

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
`runtimes.reconnect`. A runtime that dials in is not dialed by the daemon:
an attempt succeeds only once the runtime has connected again, and a runtime
that dials in after its instances failed reopens them. Calls are never retried transparently.

## Methods

| Method | Direction | Kind | Purpose |
|---|---|---|---|
| `runtime/initialize` | daemon → runtime | request | Version, runtime ID, settings; returns the capabilities |
| `capability/open` | daemon → runtime | request | Open a capability for a workspace; returns the instance's identity, tools, skills |
| `capability/tools_changed` | runtime → daemon | notification | An instance's whole new tool list |
| `runtime/context` | daemon → runtime | request | An instance's `AGENTS.md` context for a run's preset |
| `runtime/shutdown` | daemon → runtime | request | Stop gracefully |
| `runtime/settings_changed` | daemon → runtime | notification | The runtime's whole new settings, as `runtime/initialize` carries them; since 1.10.0 (see [Settings changes](#settings-changes)) |
| `runtime/ping` | daemon → runtime | request | Answer `{}` at once; since 1.4.0 (see [Liveness](#liveness)) |
| `tool/prepare` | daemon → runtime | request | Prepare one turn's calls to an instance |
| `tool/revalidate` | daemon → runtime | request | Check a prepared call can still run as approved |
| `tool/execute` | daemon → runtime | request | Run a prepared call; returns its terminal result |
| `tool/release` | daemon → runtime | notification | Discard a prepared call that will not run |
| `tool/progress` | runtime → daemon | request | Display and output deltas of an executing call |
| `tool/cancel` | daemon → runtime | notification | Cancel an executing call |
| `tool/detachable` | runtime → daemon | notification | An executing call reached its detach point |
| `mcp/changed` | runtime → daemon | notification | The status of an instance's MCP servers, and the ones that failed |
| `mcp/reconnect` | daemon → runtime | request | Reconnect one of an instance's MCP servers, or retry every failed one |
| `runtime/tools_changed` | runtime → daemon | notification | The runtime's own tools, those of its runtime-scoped MCP servers; since 1.5.0 |
| `runtime/mcp_changed` | runtime → daemon | notification | The status of every MCP server the runtime is configured with, and the runtime-scoped ones that failed; since 1.5.0 |
| `runtime/mcp_reconnect` | daemon → runtime | request | Reconnect one runtime-scoped MCP server, or retry every failed one; since 1.5.0 |
| `producer/recovery/start` | daemon → runtime | notification | Producer messaging is open; restore tracked work, then report |
| `producer/recovery/complete` | runtime → daemon | request | The runtime's recovery outcome |
| `producer/register` | runtime → daemon | request | Register a named producer for an attached session |
| `producer/send` | runtime → daemon | request | Send a message through a registered producer |
| `producer/unregister` | runtime → daemon | request | Close a registration; its messages stay |
| `producer/discard` | runtime → daemon | request | Discard one of the runtime's waiting messages |
| `runtime/agents_changed` | runtime → daemon | notification | Every ACP agent the runtime hosts now; since 1.7.0 |
| `agent/start` | daemon → runtime | request | Start an ACP agent's process; since 1.7.0 |
| `agent/stop` | daemon → runtime | request | Stop an ACP agent's process; since 1.7.0 |
| `agent/message` | both | notification | One ACP message to or from an agent's process; since 1.7.0 |
| `agent/exited` | runtime → daemon | notification | An agent's process ended; since 1.7.0 |
| `agent/mcp_open` | daemon → runtime | request | Open an MCP bridge socket for one of an agent's sessions; since 1.7.0 |
| `agent/mcp_close` | daemon → runtime | notification | Close a bridge; since 1.7.0 |
| `agent/mcp` | both | notification | One MCP message of a bridge connection; since 1.7.0 |
| `agent/mcp_closed` | runtime → daemon | notification | A bridge connection ended; since 1.7.0 |

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
| `settings.acp_agents` | Optional, since 1.7.0: the runtime's ACP agents (`[runtimes.<id>.acp.agents]`, of a template or of a runtime that dials in, or `[runtimes.local]` for the daemon's own runtime): `id`, `command`, `env`, `description`, `enabled`. A runtime given none hosts none, unless a program embedding it gave it agents of its own (see [ACP agents](#acp-agents)). |
| `settings.mcp_servers` | The template's MCP servers: `name` and `config`, the server's table as the [`[mcp.servers.<name>]` schema](../extend/mcp.md#accepted-fields) reads it, with its `scope`. Each instance of a capability that hosts MCP servers runs the workspace-scoped ones in its workspace; the runtime runs the runtime-scoped ones once, in its home directory (see [Runtime-scoped MCP servers](#runtime-scoped-mcp-servers)). A server's `cwd`, when relative, resolves against that directory. |

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
| `home` | Optional, since 1.3.0: the home directory of the user the runtime runs as, a canonical absolute path on its machine (`/home/ada`, `C:\Users\ada`), at most 4096 bytes without control characters. The coding runtime reports its user's home, canonicalized. The daemon reads one that is not an absolute path as left out. See [Home](#home). |
| `capabilities` | What the runtime offers: `name` (`[a-z][a-z0-9_]{0,31}`, unique within the runtime); `kind`, a self-declared label such as `coding` (a lowercase ASCII letter followed by up to 15 lowercase letters, digits, or hyphens) that the daemon shows to clients and in approval reviews and never branches on; `takes_workspace`; and an optional `description`. The coding runtime offers `coding`, which takes a workspace. |
| `producer_messaging` | Optional, default `false`: the runtime sends producer messages to the sessions attached to its instances (see [Producer messaging](#producer-messaging)). |
| `acp_agents` | Optional, since 1.7.0: the ACP agents the runtime hosts, `{ id, description, enabled }` each, in ID order. Absent when it hosts none. |
| `mcp_servers` | Optional, since 1.5.0: every MCP server the runtime is configured with, in name order, as server reports (see [`capability/open`](#capabilityopen)) with a `scope`: the runtime-scoped ones with their status now (usually `connecting`), the workspace-scoped ones as configured, `disconnected` (or `disabled`) with no tools. Absent when it has none. |

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
A server that fails publishes no tools; its status says why. It also waits
for the runtime-scoped servers' first attempt, and sends their report and
tools before its reply. The servers'
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
| `mcp_servers` | The status of each MCP server after its first connection attempt, in name order: `name`; `state`, one of `connected`, `connecting`, `reconnecting`, `failed`, `disabled`, or `disconnected`; `message`, why it failed; `tools`, its tool names; since 1.5.0, `scope`, `workspace` here (absent reads as `workspace`); since 1.6.0, `filtered_tools`; and since 1.9.0, `server_tools`, every tool it lists by its own name with `filtered` set for those its filters left out. Absent when the instance hosts none. Runtime-scoped servers are not among them. |

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

### Runtime-scoped MCP servers

Since 1.5.0 a template server may set `scope = "runtime"`. The runtime
starts such servers once, when it accepts `runtime/initialize`, in its
user's home directory unless they set a `cwd`, and does not hold up its
reply for them: they are usually `connecting` in `mcp_servers`. Every
instance of the runtime shares them until it stops, and every
`capability/open` waits for their first attempt, which each server's
`timeout_ms` bounds.

Their tools are the runtime's own, in no instance: the runtime declares
them, `<server>_<tool>` with permission `mcp`, with `runtime/tools_changed`
(params: the `tools` object, as a plugin sends it) whenever they change, and
the daemon prepares them with a `tool/prepare` that carries no `instance`.
The daemon offers them to a session once per runtime, however many of the
runtime's instances it attaches, composed under the runtime's ID, and their
calls run without a workspace.

`runtime/mcp_changed` carries `servers`, every configured server as in the
initialize result's `mcp_servers`, and `failures`, the runtime-scoped ones
that failed since the last report (`server`, `reason`). The runtime sends it
whenever a runtime-scoped server's status changes. The daemon lists the
runtime-scoped servers in `mcp.server.list` without a workspace, and a
workspace-scoped one no instance hosts as configured, so clients show a
connected runtime's servers before any workspace opens; it passes each
failure to the clients viewing a session attached to any instance of the
runtime. They reconnect as instance servers do, reported with
`runtime/mcp_changed` and `runtime/tools_changed`.

`runtime/mcp_reconnect` params: `server`, optional. With a name, the runtime
reconnects that runtime-scoped server; without one, it retries every failed
one. It answers like `mcp/reconnect`. While one is `failed`, the daemon
sends it without a name when a session that uses any instance of the runtime
is created or opened, and the session's next run waits for the answer.

A daemon older than 1.5.0 knows no tools outside an instance, so a runtime
that sees one in `runtime/initialize` hosts every server per workspace.

### Settings changes

Since 1.10.0 the daemon sends `runtime/settings_changed` (`settings`, the
whole object `runtime/initialize` carries) to a connected runtime whenever
the daemon's configuration of it changes: its `[runtimes.<id>]` MCP servers
or ACP agents, `[tool_output]`, or `[agent_md]`. It sends it only to a
runtime that answered `runtime/initialize` with 1.10.0 or later; an older
one gets the change when it next connects. A runtime that connected while
the configuration changed gets the newer settings right after its
initialize reply.

The runtime applies them as `runtime/initialize` would, by name: a server or
agent that is new starts, one that is gone stops, one whose definition
changed restarts, and the others keep running. Every instance takes the
workspace-scoped servers, including one that opens meanwhile; the
runtime-scoped ones follow `runtime/mcp_changed` and `runtime/tools_changed`
as usual, and the instances' own `mcp/changed` and
`capability/tools_changed`. Changed agents are reported with
`runtime/agents_changed`. Settings it cannot apply leave the runtime as it
was, and it says why on its standard error.

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
file. A plugin's prepare carries no `instance`, nor does one of a runtime's
own tools (see [Runtime-scoped MCP servers](#runtime-scoped-mcp-servers)). The runtime prepares a turn's calls together, so calls on one
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
right after execute reaches it. A cancelled call should stop within 2 s: the
daemon waits that long for the execute reply of a call its run cancelled,
then ends the call as cancelled and drops the reply when it comes. The coding
runtime and the plugin SDK drop a call that has not stopped by then and
answer its execute request with a failure.

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

### ACP agents

Since 1.7.0 a runtime hosts ACP agents, external coding agents that take a
session's turns over the Agent Client Protocol (see the
[ACP agents spec](../specs/acp-agents.md)). The runtime starts and relays
them; the daemon speaks ACP to them.

- **Declarations.** The runtime reports its agents in `runtime/initialize`'s
  `acp_agents`, and again with `runtime/agents_changed` (`agents`, the whole
  list) when they change, such as after `runtime/settings_changed`. The daemon lists each under its own ID; runtimes
  that report the same ID offer the same agent.
- **Processes.** `agent/start` (`agent`) runs the agent's `command` without a
  shell (on Windows, a bare program name also finds a `.cmd` or `.bat`
  launcher on `PATH`) in the runtime's home directory, in its own process group (a job
  object on Windows), with the runtime's environment and the agent's `env`;
  it answers `{}` once the process runs, and at once when it runs already.
  `agent/stop` (`agent`) kills it. The runtime answers `agent_failed` for an
  agent it does not host, a disabled one, or one it cannot start.
- **Relay.** Each line the agent writes to its standard output is one
  `agent/message` (`agent`, `message`, the line's JSON) to the daemon,
  unchanged; each `agent/message` from the daemon becomes one line of its
  standard input. A line that is not JSON is kept with its standard error.
  When the process ends the runtime sends `agent/exited` (`agent`, `status`
  when it exited with one, `message` when it did not, and `stderr`, the tail
  of its standard error: the last 32 lines, at most 4 KiB). The agents'
  processes stop with the connection.
- **MCP bridge.** `agent/mcp_open` (`agent`) opens a bridge socket for one of
  the agent's sessions: a Unix socket in a directory only the runtime's user
  may enter, or a named pipe on Windows that refuses remote clients. It
  answers `{ bridge, command }`: a handle unique on the connection, and the
  stdio MCP server the daemon hands the agent, `cookie runtime mcp-bridge
  --socket <path>` on the runtime's machine. Each connection to the socket
  is numbered; each line it sends is an `agent/mcp` (`bridge`, `connection`,
  `message`) to the daemon, and each `agent/mcp` from the daemon becomes a
  line to that connection. `agent/mcp_closed` reports a connection that
  ended; `agent/mcp_close` (`bridge`) removes the socket, as does the end of
  the agent's process.

These messages are information: the daemon reads the agent's messages
tolerantly, and an `agent/*` message from a plugin is ignored.

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
| `-32000` | `agent_failed` | `agent/start` or `agent/mcp_open` for an ACP agent the runtime does not host, a disabled one, one it could not start, or a bridge it could not open; `data.agent` names the agent (since 1.7.0). |
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

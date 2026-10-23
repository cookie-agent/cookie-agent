# Runtimes [runtimes]

A *runtime* is where a session's tools run. It offers one or more
*capabilities*, and a session attaches each one it uses separately. The
daemon's built-in `local` runtime offers `coding` (`read`, `write`, `edit`,
`bash`, `webfetch`) for any directory on the daemon's machine, and the
`global` runtime offers `global`: top-level MCP servers and plugins. A
`[runtimes.<id>]` table configures another runtime, typically on another
machine over ssh, so a session on your laptop can read, edit, and run
commands in a repository on that machine. The
[runtimes specification](../specs/runtimes.md) and the
[runtime capabilities specification](../specs/runtime-capabilities.md)
describe the model; this page covers configuration and use.

## Example: a repository on another machine over ssh

Install the same Cookie Agent version on the remote machine (the runtime
protocol versions must be compatible: their majors must match) and make sure `ssh labpc cookie --version`
works without a password prompt. Then add a template to
`~/.cookie-agent/config.toml` on the machine that runs the daemon:

```toml
[runtimes.labpc]
command = ["ssh", "-o", "BatchMode=yes", "labpc", "cookie", "runtime", "serve", "--stdio"]
startup_timeout_ms = 20000
```

Start `cookie`, open `/runtime`, press `a`, type `labpc` and press Enter,
then type a path on the remote machine such as `~/cookie-agent` and press
Enter. Before the first prompt this chooses the runtimes the new
session starts with; in an existing session it attaches `labpc` to it. The
daemon runs the command once, the remote
`cookie runtime serve` resolves the path on its own machine and opens its
`coding` capability there, and the session gains an attachment named after
the runtime ID and the workspace's directory name, here
`labpc_cookie_agent`. Tools that two attachments offer are prefixed with the
attachment name, so the model sees `local_web_bash` and
`labpc_cookie_agent_bash`; detach `local` (`d` on its row) to keep the bare
names for the remote runtime only.

Attaching another workspace on `labpc` opens it over the same connection:
one `cookie runtime serve` process serves every workspace and every session
that uses `labpc`.

## Capabilities

A runtime reports its capabilities when it connects. Each has a name, a
self-declared kind, and whether it takes a workspace. `runtimes.list` and
the `/runtime` panel show them once the runtime has connected. When a
runtime offers one capability, attaching it needs no capability name; when
it offers several, choose one (`/runtime` lists each capability of each
runtime as its own entry). Each attached capability is its own attachment, with its own
name and tools.

| Runtime | Capability | Workspace | Offers |
|---|---|---|---|
| `global` | `global` | none | top-level MCP servers, plugins, the [memory](../sessions/memory.md) tools, the fallback `webfetch` |
| `local` | `coding` | any directory on the daemon's machine; the daemon's own directory when none is named | `read`, `write`, `edit`, `bash`, `webfetch`, project skills, `AGENTS.md` |
| `cookie runtime serve` | `coding` | any directory on its machine, or under its `--root` directories | the same |

## Configuration

`runtimes` maps a runtime ID to a template; it is empty by default.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `command` | array of strings | required | The program and its arguments. |
| `env` | table of strings | `{}` | Variables added to the daemon's environment for the process. |
| `startup_timeout_ms` | positive integer | `10000` | Bound on starting the process and its answer to `runtime/initialize`, and on each capability opening, which follows its MCP servers' first connection attempts. |
| `shutdown_grace_ms` | positive integer | `3000` | How long a stopping runtime gets before its process group is killed. |
| `mcp.servers.<name>` | MCP server table | none | MCP servers the runtime hosts on its machine, one per workspace instance or, with `scope = "runtime"`, one for the runtime; see below. Their `include_tools` and `exclude_tools` filter tools on the runtime's machine ([choosing a server's tools](../extend/mcp.md#choosing-a-servers-tools)). |
| `acp.agents.<agent>` | ACP agent table | none | ACP agents the runtime starts on its machine; see [ACP agents](#acp-agents). |

- **Runtime IDs** match `[a-z][a-z0-9_]{0,31}` because they become tool name
  prefixes. `global` is reserved. `[runtimes.local]` configures the built-in
  local runtime and accepts only `mcp` and `acp` tables.
- **`command`** must be nonempty and its first element must name a program.
  One process serves every workspace, so the command names none. An element
  containing `{workspace}`, which older templates used, is rejected.
- **No shell is involved.** cookie starts the program directly. `ssh`
  itself, however, joins its arguments into one string that the remote
  login shell interprets, so arguments with spaces or shell metacharacters
  need quoting that survives that shell. A workspace path travels in the
  runtime protocol, not on the command line, and `cookie runtime serve`
  expands a leading `~` in it.
- **Environment.** The process inherits the daemon's environment, which `ssh`
  needs for `SSH_AUTH_SOCK` and similar variables, plus `env`. Names must be
  nonempty without `=` or NUL. `ssh` finds its configuration, keys, and
  `known_hosts` through that environment's `HOME`, so a daemon run with
  another `HOME` (a scratch or service home) needs
  `env = { HOME = "/home/me" }` in the template, or the files named on its
  command line as absolute paths, since no shell expands `~` there:
  `-F /home/me/.ssh/config`, `-i /home/me/.ssh/id_ed25519`, and
  `-o UserKnownHostsFile=/home/me/.ssh/known_hosts`.
- **MCP servers** are parsed with the [MCP server schema](../extend/mcp.md), and a
  server that enables OAuth (`oauth = true` or an `oauth` table) is rejected:
  OAuth credentials live on the daemon and are available to top-level `[mcp]`
  servers only. Static-header HTTP servers are allowed. See
  [template MCP servers](#template-mcp-servers).
- Templates declare no capabilities and accept no `plugins` table: the
  runtime reports its capabilities when it connects, and plugins run only in
  the global runtime.

Unknown keys and invalid values fail the configuration load, as everywhere in
`config.toml`. Changes take effect when the daemon next starts.

## ACP agents

A runtime hosts ACP agents, external coding agents that take a session's or
a subagent's turns over the Agent Client Protocol (see the
[ACP agents spec](../specs/acp-agents.md), and the
[ACP agents guide](../extend/acp-agents.md) for setting up Claude Code,
Codex, and Gemini CLI). Declare them exactly where MCP
servers go: `[runtimes.local.acp.agents.<agent>]` on the daemon's machine and
on a machine whose runtime dials in, and `[runtimes.<id>.acp.agents.<agent>]`
for a template.

```toml
[runtimes.local.acp.agents.claude]
command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
description = "Claude Code"

[runtimes.labpc.acp.agents.codex]
command = ["npx", "-y", "@agentclientprotocol/codex-acp"]
env = { CODEX_HOME = "/home/ada/.codex" }
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `command` | array of strings | required | The program and its arguments, run without a shell on the runtime's machine, in its home directory. |
| `env` | table of strings | `{}` | Variables merged over the runtime's environment. |
| `description` | string | `""` | Shown where the agent is listed when its daemon document gives none; at most 512 bytes without control characters. |
| `enabled` | boolean | `true` | A disabled agent is listed as unavailable. |

- **IDs.** The table key is an agent ID: lowercase letters, digits, and
  single inner hyphens. The daemon lists the agent as
  `<runtime>-<agent>`, the runtime ID's `_` spelled `-`: `local-claude`,
  `labpc-codex`, `client-myhost-codex`. A combination that is not an agent
  ID (a runtime ID with `__` or a trailing `_`), longer than 64 bytes, or
  that another runtime's agent or a daemon agent document already names is
  refused with an error naming both; for a runtime that dials in, the
  daemon skips the agent and says why in its log.
- **Availability.** An agent of `local` or of a template is available
  whenever the daemon runs, since the daemon starts its runtime on demand;
  an agent of a runtime that dials in is available while that runtime is
  connected, and listed as unavailable while it is away. A session or
  delegation with an unavailable agent is refused at once.
- **The daemon-side document.** Cookie settings come from an agent document
  of the daemon's ID, `~/.cookie-agent/agents/<runtime>-<agent>.md`, whose
  `acp` block marks it as the agent's (see
  [ACP agent documents](../extend/agents.md#acp-agent-documents)). Without
  one the agent has mode `all`, the runtime's description, and the default
  rules.
- **Changes.** The daemon's templates and `[runtimes.local]` apply when the
  daemon next starts, like the rest of `runtimes`. A runtime that dials in
  reads its machine's configuration again every few seconds and reports
  changed agents.
- **Processes.** Each agent runs one process per runtime, shared by its
  sessions, started on its first turn and stopped after
  [`[acp] idle_shutdown_ms`](../engine/acp.md) with no turn in progress, with
  its runtime, and with the daemon.

## Template MCP servers

The runtime process (`cookie runtime serve` on the remote machine, or the
daemon's process for `[runtimes.local]`) hosts the template's `mcp.servers`
itself, so commands, working directories, and URLs are resolved on the
runtime's machine. A server's [`scope`](../extend/mcp.md#scope-one-per-workspace-or-one-per-runtime)
says how many copies run:

- **`scope = "workspace"`** (the default): each `coding` instance starts its
  own copy for its workspace, in that workspace unless the server sets a
  `cwd`. Right for servers about a repository, such as a language server.
- **`scope = "runtime"`**: the runtime starts one copy when it starts, in
  its user's home directory unless the server sets a `cwd`, and every
  workspace of the runtime shares it until the runtime stops. Right for
  servers about the machine, such as a browser.

```toml
[runtimes.labpc]
command = ["ssh", "-o", "BatchMode=yes", "labpc", "cookie", "runtime", "serve", "--stdio"]
startup_timeout_ms = 30000

[runtimes.labpc.mcp.servers.rust_analyzer]
command = "rust-analyzer-mcp"

[runtimes.labpc.mcp.servers.browser]
command = "npx"
args = ["-y", "chrome-devtools-mcp@latest"]
scope = "runtime"

[runtimes.local.mcp.servers.docs]
command = "docs-mcp"
```

- **Connecting.** An instance connects every enabled workspace-scoped
  server before it answers the daemon, and waits for the runtime-scoped
  ones' first attempt, which the runtime started when it initialized
  without holding up its answer; so a session's first use of the instance
  lists their tools. `startup_timeout_ms` covers these first attempts; raise
  it for servers that start slowly. A server that fails publishes no tools,
  and clients viewing a session attached to the instance (any instance of
  the runtime, for a runtime-scoped server) get an error naming it.
- **Tools.** A workspace-scoped server's tools are the instance's tools,
  named `<server>_<tool>` like any [MCP tool](../extend/mcp.md#tool-names-and-permissions)
  and gated by the `mcp` permission. When another attachment offers the same
  name, they are prefixed with the attachment name like the built-in tools.
  A runtime-scoped server's tools are the runtime's own: a session gets them
  once however many of the runtime's workspaces it attaches, prefixed with
  the runtime ID when another tool takes the name, and they run without a
  workspace.
- **Drops and retries.** The runtime restarts a dropped server (1 s, 2 s,
  4 s) with its tools still published, and drops them when every attempt
  fails. A failed server is retried when a session that uses the instance is
  created or opened, when the instance is attached or retried (`r` in
  `/runtime`), and with `r` on the server in `/mcp`.
- **Status.** `/mcp` lists the servers of every instance as
  `<runtime> at <workspace>`, a runtime-scoped server once as
  `<runtime>, runtime-wide`, and, while no instance of a connected runtime
  hosts it, a workspace-scoped one as `<runtime>, in each workspace`, which
  "starts with a workspace" (client protocol 1.13.0). The `/runtime` panels
  list each attachment's servers, the runtime's shared ones included, with
  their state, tool count, and failure, and the machines view lists every
  server of each connected runtime, so a runtime's servers show before any
  session opens a workspace of it. They cannot be edited there; change the
  template and restart the daemon. A dial-in runtime's own servers
  (`[runtimes.local.mcp]` on its machine) are listed too, with the source
  `runtime machine config`, since client protocol 1.12.0.

## Instances and workspaces

A capability instance is one capability of a runtime opened for one
workspace (or none), shared by every session attached to it. The daemon
passes the workspace path exactly as it was typed and never resolves it on
its own machine; the runtime reports the canonical path on its machine, and
that path identifies the instance. Two requests that resolve to the same
directory, such as `~/cookie-agent` and `/home/me/cookie-agent`, share one
instance. A session records the reported path, and reopening the session
asks for that path. A new session's directory is the reported path of its
first workspace too, so both spellings list the session under one directory
(see [Where sessions live](../concepts/sessions.md#where-sessions-live)).

A runtime may report its home directory when it connects; `cookie runtime
serve` and the built-in `global` and `local` report their user's, canonical.
`runtimes.list` gives it as `home`. Clients show the runtime's paths under it
as `~/…` and absolute without one, and permission rules starting with `~/`
expand against it (see
[Paths under the home directory](../extend/agents.md#paths-under-the-home-directory)).

## Lifecycle and failures

- **Start.** A runtime's process starts when a session first needs one of
  its capabilities, and stays up until the daemon exits; its instances open
  as sessions attach them. Creating a session, attaching a runtime, and
  delegating with `capabilities` wait until the runtime is actually online,
  `runtime/initialize` and `capability/open` answered, before anything is
  created. A start that fails or exceeds `startup_timeout_ms` publishes no
  tools and creates nothing: the session, attachment, or subagent is
  refused with the reason, and clients viewing an affected session get an
  error naming the runtime and the reason. `runtimes.list` shows why the
  runtime last failed to connect.
- **Liveness.** A connection can die without closing, as a hung `ssh` leaves
  it. The daemon pings a runtime it has heard nothing from for 15 s
  (`runtime/ping`) and ends the connection once nothing has arrived for
  45 s, which counts as a drop (below). Every request has a deadline:
  preparing and revalidating a call and fetching `AGENTS.md` context get
  30 s, and a runtime that misses one fails the call as
  `runtime_unavailable` and has its connection checked at once (10 s to
  answer). See [Liveness](../reference/runtime-protocol.md#liveness).
- **Reuse.** An instance that is already open is handed to a new session,
  attachment, or subagent only over a live connection: one heard from in the
  last 15 s, or that answers a ping within 10 s. When it does not, the daemon
  ends the connection and waits, up to 2 minutes, for the runtime to be
  started again (see **Drop**) before it gives the instance out, or refuses
  with the reason.
- **Refused workspaces.** A runtime that is up can still refuse to open a
  capability, for example for a workspace outside its `--root` directories.
  The error then reads
  ``runtime `labpc` could not open `coding` for workspace `/tmp`: workspace /tmp is outside the directories this runtime serves``
  rather than saying the runtime failed to start. Attaching, creating a
  session, and retrying (`runtimes.reconnect`) record nothing when the
  instance never opened: no attachment, and no `failed` instance in
  `runtimes.list`.
- **Diagnostics.** The process's standard error is captured into a bounded
  buffer (its last 32 lines, at most 4 KiB). Failure reasons end with that
  tail, so an `ssh` or `cookie runtime` error shows up in the TUI. The
  daemon does not copy the process's standard error to its own as it
  arrives, but when a session is created, opened, or starts a run while one
  of its attachments is unavailable, the daemon writes
  ``cookie-agent: session <id>: runtime `<attachment>` is unavailable: <reason>``
  to its standard error, and that reason ends with the tail.
- **Drop.** When the process exits, its pipe closes, or it stops answering,
  calls in flight fail
  with `runtime_unavailable` (a backgrounded call reports the failure as its
  background result), and the daemon starts the process again up to 3 times
  after 1 s, 2 s, and 4 s, reopening every instance over the new
  connection. Meanwhile the instances show as `reconnecting`, their tools
  stay published, and new calls fail fast with `runtime_unavailable`. A
  successful restart re-lists their tools under a new instance generation (a
  call the model planned before the restart fails as `operation_changed`).
  When every attempt fails, their tools are dropped, they become `failed`,
  and clients get an error.
- **Retry.** A failed instance is retried when a session that uses it is
  created or opened, and on demand with `r` in `/runtime` (or
  `runtimes.reconnect`).
- **Shutdown.** When the daemon exits it sends `runtime/shutdown`, waits up to
  `shutdown_grace_ms`, and then kills the process group (a job object on
  Windows).

## `cookie runtime serve`

```sh
cookie runtime serve --stdio [--root <dir>]...
cookie runtime serve --daemon [<url>] [--id <runtime ID>] [--token <token>] [--root <dir>]...
```

With `--daemon` it dials in instead (see
[Runtimes that dial in](#runtimes-that-dial-in)); the rest of this section
describes `--stdio`.

Serves this machine's runtime over standard input and output with the
[runtime protocol](../reference/runtime-protocol.md): the `coding` capability
for every workspace the daemon opens. `--root` (repeatable) limits `coding`
to workspaces under those directories; a root that does not exist fails at
start. It needs no provider configuration and reads no `config.toml`; a
leading `~` in a workspace or root is expanded, and paths are resolved on the
machine it runs on. It reports its user's home directory, canonicalized, and
expands a tool's `~/…` path argument against the same home. Standard output carries only protocol messages and
diagnostics go to standard error. It exits on `runtime/shutdown`, when
standard input closes, or on a stop signal (`SIGINT`, `SIGTERM`, `SIGHUP`). A template normally runs it; running it by hand is
only useful for debugging.

## Runtimes that dial in

A runtime can also run on its own and connect to the daemon, instead of the
daemon starting it. It needs no template: it names itself, and sessions
attach it like any other runtime while it is connected.

```sh
# On the daemon's machine: a listener the runtime reaches.
cookie daemon --listen 0.0.0.0:7419
```

```sh
# On labpc: dial in, named after the host, and stay connected.
cookie runtime serve --daemon ws://laptop:7419/ws --token <token>
```

- **Daemon.** `--daemon` takes the daemon's client URL, or, without one,
  `[server] url` of this machine's `config.toml`, else the local daemon that
  table describes. `--token` is the daemon's token, `[server] token` in its
  `config.toml`, where the daemon keeps it so it survives restarts.
  `COOKIE_DAEMON_TOKEN` and `[server] token` on labpc work too; flags
  override the configuration. The runtime does not pass the token on to the
  processes it starts.
- **Identity.** `--id` is the runtime ID, `[a-z][a-z0-9_]{0,31}`; without
  it, the host name, sanitized (`host_` in front when it does not start with
  a letter). It must not be `global`, `local`, or a configured
  `[runtimes.<id>]`, and only one runtime may be connected under an ID at a
  time.
- **Refusals.** The daemon says why it refuses a runtime, and the runtime
  prints it. A refusal that retrying cannot fix, a configured or reserved
  ID or a wrong token, ends `cookie runtime serve --daemon` with an error:

  ```text
  cookie: cannot dial in to ws://laptop:7419/ws as `labpc`: the daemon refused the runtime (HTTP 403): runtime `labpc` is configured on the daemon ([runtimes.labpc]) and cannot dial in
  ```

  Any other, such as an ID a runtime is still connected under (the daemon
  may not have noticed yet that an earlier connection dropped), is printed
  and retried with the same backoff as a drop:

  ```text
  cookie runtime: cannot dial in to ws://laptop:7419/ws as `labpc`: the daemon refused the runtime (HTTP 409): runtime `labpc` is already connected; retrying in 1 s
  ```

  A dropped connection prints the reason too, such as
  ``disconnected from ws://laptop:7419/ws: the daemon closed the connection with code 1011: runtime `labpc` failed to start: …``
  for a runtime of an incompatible version. See
  [Dial-in refusals](../reference/runtime-protocol.md#dial-in-refusals).
- **Reconnecting.** The runtime reconnects after every drop, waiting
  1 s, 2 s, 4 s, and so on up to 30 s. While it is away, its instances go
  `reconnecting` and then `failed`, and their tools leave the sessions' tool
  lists; when it dials in again, the daemon reopens them over the new
  connection. `runtimes.list` shows it with lifetime `dial_in`, and, while
  it is away, why it disconnected.
- **Connections that die without closing.** A machine that sleeps, or
  loses or changes its network, can leave the WebSocket half-open: neither
  end hears a close. Both ends watch for that. The daemon pings a runtime it
  has heard nothing from for 15 s and drops it once nothing has arrived for
  45 s (``runtime `labpc` disconnected: nothing arrived from it for 45 s``);
  calls in flight then fail with `runtime_unavailable`, and a request the
  runtime does not answer in time (30 s for preparing a call) fails the
  call and has the connection checked at once. The runtime sends WebSocket
  pings once its connection has been quiet for 15 s and dials in again once
  nothing, a pong included, has arrived for 45 s
  (``disconnected from ws://laptop:7419/ws: … nothing arrived from the daemon for 45 s``).
  A runtime that dials in while the daemon still counts its old connection
  as connected is refused (HTTP 409) and the daemon checks the old
  connection at once, so the next attempt gets in.
- **Offline means refused.** While the runtime is not connected, creating a
  session with it, attaching it, delegating to it with `capabilities`, and
  creating a session in a project whose defaults name it are refused at
  once, naming the runtime and when the daemon last heard from it:
  ``runtime `labpc` is not connected (last seen 2026-10-22T18:12:51Z)``.
  The daemon never waits for a dial-in runtime to come back, and never hands
  out an instance of a connection that does not answer: it is checked first
  (see **Reuse** under [Lifecycle and failures](#lifecycle-and-failures)). A
  runtime the daemon remembers from before it restarted, but that has not
  dialed in since, is refused the same way. It stops
  on a stop signal (`SIGINT`, `SIGTERM`, `SIGHUP`), so it runs as a
  service; see [Running a runtime as a service](runtime-service.md).
- **Stopping.** A stopping runtime closes its WebSocket with a close
  handshake (normal closure). The daemon records that as
  ``runtime `labpc` disconnected: the other end closed the connection``,
  in `runtimes.list` and in its instances' reasons as they go `reconnecting`
  and then `failed`; a runtime that vanished without one, such as one that
  was killed or lost its network, shows the transport's error instead, for
  example ``Connection reset without closing handshake``.
- **MCP servers.** It hosts its own machine's MCP servers: those of
  `[runtimes.local.mcp]` in that machine's `config.toml`, a
  `scope = "runtime"` one from the moment it connects.
- **Roots.** `--root` limits its workspaces, as for `--stdio`.
- **Clients on its machine.** Plain `cookie` and `cookie attach` on the
  runtime's machine use it for their directory when the daemon runs
  elsewhere, such as in a container; see
  [A daemon already running](persistent.md#a-daemon-already-running).

## Machines

Every runtime belongs to a **machine**, the computer it runs on. The model
is told which machine each attachment's runtime runs on, so it knows which
tools share files (see the [system prompt](../reference/system-prompt.md#runtimes)).
The daemon groups runtimes into machines on its own: runtimes that report
the same machine ID, such as `global`, `local`, and a `cookie runtime serve`
on the daemon's own computer, share a machine, and a computer it has not
seen gets a machine named after its host name. A runtime that reports no
machine ID gets a machine of its own, named after its runtime ID. Machines
are kept in `~/.cookie-agent/machines.json`, which only the daemon writes;
there is no configuration for them.

Regroup them by hand when the automatic grouping is wrong, for example when
a container reports its own machine ID. In the TUI, the Machines view of
the `/runtime` panel (press `m`, or click its tab) lists each machine with
its runtimes; select a machine to rename (`n`), merge (`g`), or forget
(`f`) it, or a runtime to move it to another machine (`v`) or unpin it
(`u`). In the web client's **Machines & runtimes** view, drag a runtime
onto another machine's card, or use its **Move to…** (see
[Web](../clients/web.md#managing-the-daemon)); the view also adds an ssh
template or shows the dial-in command. From a terminal, `cookie machine`
does the same and works against a
remote daemon like `cookie config`:

```sh
cookie machine list                  # machines, their runtimes, and memories
cookie machine rename labpc lab      # a machine's memory folder moves with it
cookie machine assign box labpc      # pin runtime `box` to machine `labpc`
cookie machine unassign box          # follow the machine ID it reports again
cookie machine merge box labpc       # move `box`'s IDs, pins, and memories
cookie machine forget old-laptop     # only with no connected runtimes or memories
cookie machine forget-runtime roamer # forget where an offline runtime ran
```

A runtime that has connected once stays on its machine while it is offline,
across restarts too: the registry records where it ran, and `cookie machine
list` shows it under `offline` with when it was last seen. Removing a
template from `config.toml` drops its record at the next reload; a dial-in
runtime's record stays until `forget-runtime`. See
[Offline runtimes](../concepts/machines.md#offline-runtimes).

A change applies to connected runtimes at once; sessions see the new
machine names from their next run.

Each machine has its own memories, `machine://<machine>/<name>`, in the
folder `~/.cookie-agent/memory/machines/<machine>` (see
[Memory](../sessions/memory.md)): renaming a machine moves its folder, merging moves its
memories into the other machine's and fails when both have a memory of one
name, and a machine can be forgotten only once its folder is empty. A
session's model sees an index of the memories of the machines its
attachments run on.

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

Install the same cookie agent version on the remote machine (the runtime
protocol versions must be compatible: before the protocol's `1.0.0`, their
major and minor must match) and make sure `ssh labpc cookie --version`
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
| `mcp.servers.<name>` | MCP server table | none | MCP servers each `coding` instance hosts on the runtime's machine; see below. |

- **Runtime IDs** match `[a-z][a-z0-9_]{0,31}` because they become tool name
  prefixes. `global` is reserved. `[runtimes.local]` configures the built-in
  local runtime and accepts only an `mcp` table.
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
  nonempty without `=` or NUL.
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

## Template MCP servers

Every `coding` instance hosts the template's `mcp.servers` itself: the
runtime process (`cookie runtime serve` on the remote machine, or the
daemon's process for `[runtimes.local]`) starts each server for each
workspace it opens, so commands, working directories, and URLs are resolved
where the repository is.

```toml
[runtimes.labpc]
command = ["ssh", "-o", "BatchMode=yes", "labpc", "cookie", "runtime", "serve", "--stdio"]
startup_timeout_ms = 30000

[runtimes.labpc.mcp.servers.rust_analyzer]
command = "rust-analyzer-mcp"

[runtimes.local.mcp.servers.docs]
command = "docs-mcp"
```

- **Connecting.** An instance connects every enabled server before it
  answers the daemon, so its tools are listed when a session first uses the
  instance. `startup_timeout_ms` covers these first attempts; raise it for
  servers that start slowly. A server that fails publishes no tools, and
  clients viewing a session attached to the instance get an error naming it.
- **Tools.** The servers' tools are the instance's tools, named
  `<server>_<tool>` like any [MCP tool](../extend/mcp.md#tool-names-and-permissions)
  and gated by the `mcp` permission. When another attachment offers the same
  name, they are prefixed with the attachment name like the built-in tools.
- **Drops and retries.** The runtime restarts a dropped server (1 s, 2 s,
  4 s) with its tools still published, and drops them when every attempt
  fails. A failed server is retried when a session that uses the instance is
  created or opened, when the instance is attached or retried (`r` in
  `/runtime`), and with `r` on the server in `/mcp`.
- **Status.** `/mcp` lists the servers of every instance as
  `<runtime> at <workspace>`. They cannot be edited there; change the
  template and restart the daemon.

## Instances and workspaces

A capability instance is one capability of a runtime opened for one
workspace (or none), shared by every session attached to it. The daemon
passes the workspace path exactly as it was typed and never resolves it on
its own machine; the runtime reports the canonical path on its machine, and
that path identifies the instance. Two requests that resolve to the same
directory, such as `~/cookie-agent` and `/home/me/cookie-agent`, share one
instance. A session records the reported path, and reopening the session
asks for that path.

## Lifecycle and failures

- **Start.** A runtime's process starts when a session first needs one of
  its capabilities, and stays up until the daemon exits; its instances open
  as sessions attach them. A start that fails or exceeds
  `startup_timeout_ms` publishes no tools; the attach is rejected, and clients
  viewing an affected session get an error naming the runtime and the
  reason. `runtimes.list` shows why the runtime last failed to connect.
- **Diagnostics.** The process's standard error is captured into a bounded
  buffer (its last 32 lines, at most 4 KiB). Failure reasons end with that
  tail, so an `ssh` or `cookie runtime` error shows up in the TUI. Nothing of
  it is written to the daemon's own terminal.
- **Drop.** When the process exits or its pipe closes, calls in flight fail
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
machine it runs on. Standard output carries only protocol messages and
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

```toml
# On labpc, ~/.cookie-agent/config.toml: the daemon to dial in to.
[server]
url = "ws://laptop:7419/ws"
token = "<the daemon's token>"
```

```sh
# On labpc: dial in, named after the host, and stay connected.
cookie runtime serve --daemon
```

- **Daemon.** `--daemon` takes the daemon's client URL, or, without one,
  `[server] url` of this machine's `config.toml`, else the local daemon that
  table describes. The token is `--token`, `COOKIE_DAEMON_TOKEN`, or
  `[server] token`; flags override the configuration. The daemon keeps its
  own token in its `config.toml`, so it survives restarts.
- **Identity.** `--id` is the runtime ID, `[a-z][a-z0-9_]{0,31}`; without
  it, the host name, sanitized (`host_` in front when it does not start with
  a letter). It must not be `global`, `local`, or a configured
  `[runtimes.<id>]`, and only one runtime may be connected under an ID at a
  time.
- **Reconnecting.** The runtime reconnects after every drop, waiting
  1 s, 2 s, 4 s, and so on up to 30 s. While it is away, its instances go
  `reconnecting` and then `failed`, and their tools leave the sessions' tool
  lists; when it dials in again, the daemon reopens them over the new
  connection. `runtimes.list` shows it with lifetime `dial_in`. It stops
  on a stop signal (`SIGINT`, `SIGTERM`, `SIGHUP`), so it runs as a
  service; see [Running as a service](../clients/tui.md#running-as-a-service).
- **MCP servers.** It hosts its own machine's MCP servers: those of
  `[runtimes.local.mcp]` in that machine's `config.toml`.
- **Roots.** `--root` limits its workspaces, as for `--stdio`.
- **Clients on its machine.** Plain `cookie` and `cookie attach` on the
  runtime's machine use it for their directory when the daemon runs
  elsewhere, such as in a container; see
  [A daemon already running](../clients/tui.md#a-daemon-already-running).

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
(`u`). From a terminal, `cookie machine` does the same and works against a
remote daemon like `cookie config`:

```sh
cookie machine list                  # machines, their runtimes, and memories
cookie machine rename labpc lab      # a machine's memory folder moves with it
cookie machine assign box labpc      # pin runtime `box` to machine `labpc`
cookie machine unassign box          # follow the machine ID it reports again
cookie machine merge box labpc       # move `box`'s IDs, pins, and memories
cookie machine forget old-laptop     # only with no connected runtimes or memories
```

A change applies to connected runtimes at once; sessions see the new
machine names from their next run.

Each machine has its own memories, `machine://<machine>/<name>`, in the
folder `~/.cookie-agent/memory/machines/<machine>` (see
[Memory](../sessions/memory.md)): renaming a machine moves its folder, merging moves its
memories into the other machine's and fails when both have a memory of one
name, and a machine can be forgotten only once its folder is empty. A
session's model sees an index of the memories of the machines its
attachments run on.

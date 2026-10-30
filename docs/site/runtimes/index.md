# Runtimes

A *runtime* is where a session's tools run. It offers one or more
*capabilities*, and a session attaches each one it uses separately. The
daemon's built-in `local` runtime offers `coding` (`read`, `write`, `edit`,
`bash`, `webfetch`) for any directory on the daemon's machine, and the
`global` runtime offers `global`: top-level MCP servers and plugins. Other
runtimes run elsewhere, typically on another machine, so a session on your
laptop can read, edit, and run commands in a repository on that machine. The
[runtimes specification](../specs/runtimes.md) and the
[runtime capabilities specification](../specs/runtime-capabilities.md)
describe the model; [Runtimes and capabilities](../concepts/runtimes.md)
introduces it.

This page covers what every runtime shares: capabilities, instances and
workspaces, lifecycle and failures, and machines. The rest of this section
covers the two ways to add a runtime, keeping one connected, and what a
runtime hosts:

- [Runtime templates](templates.md): `[runtimes.<id>]` tables, whose
  runtimes the daemon starts itself, such as over ssh, and
  `cookie runtime serve --stdio`.
- [Runtimes that dial in](dial-in.md): runtimes that run on their own and
  connect to the daemon.
- [Running a runtime as a service](service.md): keeping a runtime that dials
  in connected, on Linux, macOS, and Windows.
- [MCP servers and ACP agents](tools.md): the servers and agents a runtime
  hosts on its machine.

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
| [`cookie runtime serve`](templates.md#cookie-runtime-serve) | `coding` | any directory on its machine, or under its `--root` directories | the same |

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
  its capabilities, and stays up until the daemon exits or a client
  disconnects it (below); its instances open
  as sessions attach them. Creating a session, attaching a runtime, and
  delegating with `capabilities` wait until the runtime is actually online,
  `runtime/initialize` and `capability/open` answered, before anything is
  created. A start that fails or exceeds
  [`startup_timeout_ms`](templates.md#configuration) publishes no
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
- **Connect and disconnect by hand.** A runtime the daemon starts from a
  template can be connected without a session, to try that it starts:
  **Connect** in the web's Machines & runtimes view, `c` in the TUI's
  machines view, or `runtimes.connect`. **Disconnect** (`d`,
  `runtimes.disconnect`) stops it: its instances become `stopped` with no
  reconnect, and opening a session that attaches it leaves it down. The next
  session, attachment, run, or delegation that needs it starts it again.
  Disconnecting is refused while a run of a session that attaches the
  runtime, or a background call on it, is going on.
- **Shutdown.** When the daemon exits it sends `runtime/shutdown`, waits up to
  [`shutdown_grace_ms`](templates.md#configuration), and then kills the
  process group (a job object on Windows).

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

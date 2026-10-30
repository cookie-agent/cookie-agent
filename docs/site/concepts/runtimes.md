# Runtimes and capabilities

A *runtime* is where a session's tools run. It offers one or more
*capabilities*, each a named set of tools with a self-declared kind, and says
whether the capability works in a workspace directory. A session *attaches*
the capabilities it uses; each attachment brings its tools into the session.

## The runtimes there are

| Runtime | Where it runs | Capability | Offers |
|---|---|---|---|
| `global` | inside the daemon, always | `global`, no workspace | top-level MCP servers, plugins, the memory tools, a fallback `webfetch` |
| `local` | inside the daemon | `coding`, any directory on the daemon's machine | `read`, `write`, `edit`, `bash`, `webfetch`, project skills, `AGENTS.md` |
| `[runtimes.<id>]` | a process the daemon starts from a template, typically `ssh host cookie runtime serve --stdio` | whatever it declares; `cookie runtime serve` offers `coding` | the same as `local`, on that machine |
| dial-in | a process that connects to the daemon's `/runtime` route, such as `cookie runtime serve --daemon` | the same | the same |

A new TUI session attaches `global` and `local`'s `coding` at the client's
directory; a new web session starts with `global` alone, and its runtime
cards, above the composer, pick others for the first message to create it
with. The runtimes panel (`/runtime`) attaches and detaches others between
runs, and in the TUI before the first prompt too. See
[Runtimes](../runtimes/index.md) for templates, dial-in, and lifecycles.

Every runtime other than the built-in ones speaks the
[runtime protocol](../reference/runtime-protocol.md), so a runtime of your own
can offer any tools; see [Write a runtime](../development/runtimes.md).

## Attachments and tool names

Each attachment has a session-unique name: `global` for the global runtime,
and for a workspace the runtime ID and the directory's name, such as
`local_web` or `labpc_api` (with `_2`, `_3`, … when taken).

**One machine offers each tool once.** A session may not have two
attachments on one machine that offer a tool of the same name: two
`coding` attachments of one machine, such as `local_web` and `local_api`,
or the daemon's `local` and a TUI's `client_<host>` on the same laptop,
would give the model two spellings of one computer. The second is refused,
with the attachment that holds the machine named; another directory there
is reached by absolute path. Capabilities whose tools differ may share a
machine. A container or WSL runtime the daemon groups with its host
overlaps the host's `coding`; give it a machine of its own with
[`cookie machine unassign`](../clients/cli.md#cookie-machine).

**Tools are routed by machine.** A session that has `global`, or
attachments on several machines, publishes each runtime tool once under its
own name, with a `machine` argument choosing where it runs: `bash`,
`read`, `write`, `edit`, a runtime's MCP tools, and any tool a runtime of
your own offers. `machine` is optional; without it a call runs on the
tool's default machine, the first attached machine that offers it, which
the tool's definition and the system prompt name. A call that names a
machine without the tool fails with where the tool is offered. The default
stays put until the next compaction, so attaching another machine changes
no tool definition, and the model's cached conversation stays valid. A
session created without `global` on one machine publishes its tools as
their runtime declares them, without `machine`.

A tool can instead declare that it is only a fallback, like the `global`
runtime's `webfetch`, which hides whenever an attachment offers one. A
runtime tool never takes the name of a built-in or `global` tool such as
`delegate_subagent`; one that would is left out with a configuration
diagnostic, and so is one that declares its own `machine` argument.

Permission rules can target one runtime's tools with `action@runtime`; see
[Permissions](permissions.md#rules-for-one-runtime). A routed call is
checked against the runtime of the machine it runs on.

## Attaching from a session

A run attaches a machine to its own session with `attach_machine`, naming a
runtime capability from `list_runtimes` (`labpc/coding`) and the directory
to work in, and detaches one with `detach_machine`. Its tools join the
session at once, and the result says where they run and carries the
workspace's `AGENTS.md`. Both are checked as the `attach` action on the
target runtime, with the workspace as resource (`attach@labpc`): root runs
are allowed by default and subagents denied, and an agent document's own
`attach` key replaces that. `detach_machine` is refused while a background
call runs on the attachment. Clients attach and detach between runs, as
before.

## Moving files between machines

`transfer` copies a file or a folder to one of the session's machines, from
another of them or from an artifact of the session, such as a file the user
sent: each user file reaches the model with a label naming its artifact,
`[file report.zip, 3.2 MB, artifact 5f2c81d0]`. The machines need not reach
each other: the daemon relays the bytes between their runtimes and keeps none
of them, so a large copy adds only its summary to the session log. The source
is checked as `read` on its machine and the destination as `write` on its
own, with the paths as the file tools label them. The model never writes the
artifact store; it only copies out of it. See
[`transfer`](../reference/tools.md#transfer).

## Workspaces and homes

A workspace is stored as the runtime reports it: canonical and absolute on the
runtime's machine. A runtime may also report its home directory. Clients then
show its paths as `~/…` under that home, the runtime's and not their own, so a
remote `/home/lab/api` reads `~/api` only on the runtime whose home is
`/home/lab`; a runtime that reports none shows its paths absolute. Permission
rules written `~/…` expand against the same home (see
[Paths under the home directory](../extend/agents.md#paths-under-the-home-directory)).
A session's directory is its first workspace (see
[Where sessions live](sessions.md#where-sessions-live)).

## Instances

Attaching a capability at a workspace opens an *instance* of it. One runtime
process serves every instance of every session that uses it, and an instance
reports its status: `starting`, `ready`, `reconnecting`, `failed`, or
`stopped`. A dropped runtime is reconnected; a session whose instance stays
down shows an error row. The daemon notices a connection that died without
closing within 45 s, and never creates a session, attaches a runtime, or
delegates to one that is offline: a dial-in runtime that is not connected is
refused at once, and one the daemon starts must come up first. See
[Lifecycle and failures](../runtimes/index.md#lifecycle-and-failures).

## A client's machine

When a TUI attaches to a daemon that runs on another machine, its sessions
still need tools in the TUI's directory. Plain `cookie` and `cookie attach`
pick, in order: the daemon's own `local` runtime when the daemon is on this
machine; else a connected runtime on this machine that takes a workspace;
else the TUI dials in as `client_<host name>`, serving only its directory, for
as long as it runs. See
[Persistent daemon](../daemon/persistent.md#choosing-the-runtime-for-a-directory).

## Delegating to runtimes

A root session decides where its subagents work. With `global` attached it
has `list_runtimes`, which lists every machine and runtime, and its
`delegate_subagent` takes `capabilities`: the runtime capabilities the
child works through, such as `labpc/coding` at `~/src/api`. The daemon starts
those runtimes before it creates the child. Without `capabilities` a child
inherits its parent's attachments, and only the root can choose (a subagent
that may attach reaches another machine with `attach_machine`). A root with
only `global` has no workspace at all until it attaches one, and names one
for each subagent it delegates, which lets it orchestrate work across
machines. `delegate@<runtime>` rules
limit where it may delegate. See
[Delegating to runtimes](../sessions/delegation.md).

## Machines

Each runtime reports the machine it runs on, and the daemon groups runtimes by
machine, remembering where a runtime ran while it is offline; see
[Machines](machines.md).

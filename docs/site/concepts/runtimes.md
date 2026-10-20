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

A new session attaches `global` and `local`'s `coding` at the client's
directory. The runtimes panel (`/runtime`) attaches and detaches others, before
the first prompt or between runs. See [Runtimes](../daemon/runtimes.md) for
templates, dial-in, and lifecycles.

Every runtime other than the built-in ones speaks the
[runtime protocol](../reference/runtime-protocol.md), so a runtime of your own
can offer any tools; see [Write a runtime](../development/runtimes.md).

## Attachments and tool names

Each attachment has a session-unique name: `global` for the global runtime,
and for a workspace the runtime ID and the directory's name, such as
`local_web` or `labpc_api` (with `_2`, `_3`, … when taken). The model sees a
tool by its bare name while one attachment offers it. When several do, each
copy is published as `<attachment>_<tool>`, such as `local_web_bash` and
`labpc_api_bash`, and the bare name disappears. A tool can instead declare
that it overrides others of its name, or that it is only a fallback, like the
`global` runtime's `webfetch`, which hides whenever `local` offers one. A
runtime tool never takes the name of a built-in tool such as
`delegate_subagent`. See
[Example: a repository on another machine](../daemon/runtimes.md#example-a-repository-on-another-machine-over-ssh).

Permission rules can target one runtime's tools with `action@runtime`; see
[Permissions](permissions.md#rules-for-one-runtime).

## Instances

Attaching a capability at a workspace opens an *instance* of it. One runtime
process serves every instance of every session that uses it, and an instance
reports its status: `starting`, `ready`, `reconnecting`, `failed`, or
`stopped`. A dropped runtime is reconnected; a session whose instance stays
down shows an error row. See
[Lifecycle and failures](../daemon/runtimes.md#lifecycle-and-failures).

## A client's machine

When a TUI attaches to a daemon that runs on another machine, its sessions
still need tools in the TUI's directory. Plain `cookie` and `cookie attach`
pick, in order: the daemon's own `local` runtime when the daemon is on this
machine; else a connected runtime on this machine that takes a workspace;
else the TUI dials in as `client_<host name>`, serving only its directory, for
as long as it runs. See
[Persistent daemon](../daemon/persistent.md#running-as-a-service).

## Machines

Each runtime reports the machine it runs on, and the daemon groups runtimes by
machine; see [Machines](machines.md).

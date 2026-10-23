# MCP servers and ACP agents on runtimes

A runtime hosts MCP servers and ACP agents on its own machine. Declare them
in the runtime's `mcp.servers` and `acp.agents` tables: under
`[runtimes.<id>]` for a [template](templates.md), under `[runtimes.local]`
for the daemon's machine, and under `[runtimes.local]` in the `config.toml`
of a machine whose [runtime dials in](dial-in.md).
[MCP servers](../extend/mcp.md) and [ACP agents](../extend/acp-agents.md)
cover the servers and agents themselves; this page covers hosting them on a
runtime.

## MCP servers

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

# Runtimes [runtimes]

A *runtime* is where a session's tools run: the daemon's built-in `local`
coding runtime works in the directory `cookie` started in, and the `global`
runtime hosts top-level MCP servers and plugins. A coding runtime can host MCP
servers of its own, on its own machine. A `[runtimes.<id>]` table
configures another way to start coding runtimes, typically on another machine
over ssh, so a session on your laptop can read, edit, and run commands in a
repository on that machine. The [runtimes specification](../specs/runtimes.md)
describes the model; this page covers configuration and use.

## Example: a repository on another machine over ssh

Install the same cookie agent version on the remote machine (the runtime
protocol version must match exactly) and make sure `ssh labpc cookie --version`
works without a password prompt. Then add a template to
`~/.cookie-agent/config.toml` on the machine that runs the daemon:

```toml
[runtimes.labpc]
command = ["ssh", "-o", "BatchMode=yes", "labpc", "cookie", "runtime", "coding", "--stdio", "--workspace", "{workspace}"]
startup_timeout_ms = 20000
```

Start `cookie`, open `/runtime`, press `a`, choose `labpc`, Tab to the
workspace path, type a path on the remote machine such as `~/cookie-agent`,
and press Enter. The daemon runs the command, the remote `cookie runtime`
resolves the path on its own machine, and the session gains an attachment
named after the runtime ID and the workspace's directory name, here
`labpc_cookie_agent`. Tools that both runtimes offer are prefixed with the
attachment name, so the model sees `local_web_bash` and
`labpc_cookie_agent_bash`; detach `local` (`d` on its row) to keep the bare
names for the remote runtime only.

## Configuration

`runtimes` maps a runtime ID to a template; it is empty by default.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `command` | array of strings | required | The program and its arguments. |
| `env` | table of strings | `{}` | Variables added to the daemon's environment for the process. |
| `startup_timeout_ms` | positive integer | `10000` | Bound on starting the process and its answer to `runtime/initialize`, which follows its MCP servers' first connection attempts. |
| `shutdown_grace_ms` | positive integer | `3000` | How long a stopping runtime gets before its process group is killed. |
| `mcp.servers.<name>` | MCP server table | none | MCP servers each instance hosts on its own machine; see below. |

- **Runtime IDs** match `[a-z][a-z0-9_]{0,31}` because they become tool name
  prefixes. `global` is reserved. `[runtimes.local]` configures the built-in
  local runtime and accepts only an `mcp` table.
- **`command`** must be nonempty and its first element must name a program.
  An element exactly equal to `{workspace}` makes the template take a
  workspace: attaching it and creating a session with it require a workspace
  path, which replaces that element. A template without the placeholder takes
  no workspace, and supplying one is an error. `{workspace}` inside a longer
  element, such as `--workspace={workspace}`, is rejected.
- **No shell is involved.** cookie starts the program directly and passes the
  workspace as one argument. `ssh` itself, however, joins its arguments into
  one string that the remote login shell interprets, so a remote path with
  spaces or shell metacharacters needs quoting that survives that shell. A
  leading `~` reaches the remote shell unquoted and is expanded there;
  `cookie runtime` also expands a leading `~` itself.
- **Environment.** The process inherits the daemon's environment, which `ssh`
  needs for `SSH_AUTH_SOCK` and similar variables, plus `env`. Names must be
  nonempty without `=` or NUL.
- **MCP servers** are parsed with the [MCP server schema](mcp.md), and a
  server that enables OAuth (`oauth = true` or an `oauth` table) is rejected:
  OAuth credentials live on the daemon and are available to top-level `[mcp]`
  servers only. Static-header HTTP servers are allowed. See
  [template MCP servers](#template-mcp-servers).
- Templates declare no kind and accept no `plugins` table: the runtime reports
  its kind when it starts, and plugins run only in the global runtime.

Unknown keys and invalid values fail the configuration load, as everywhere in
`config.toml`. Changes take effect when the daemon next starts.

## Template MCP servers

Every instance of a template hosts the template's `mcp.servers` itself: the
runtime process (`cookie runtime coding` on the remote machine, or the
daemon's process for `[runtimes.local]`) starts each server, so commands,
working directories, and URLs are resolved where the repository is.

```toml
[runtimes.labpc]
command = ["ssh", "-o", "BatchMode=yes", "labpc", "cookie", "runtime", "coding", "--stdio", "--workspace", "{workspace}"]
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
  `<server>_<tool>` like any [MCP tool](mcp.md#tool-names-and-permissions)
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

A runtime instance is one running process per runtime ID and workspace,
shared by every session attached to it. The daemon passes the workspace path
exactly as it was typed and never resolves it on its own machine; the runtime
reports the canonical path on its machine, and that path identifies the
instance. Two requests that resolve to the same directory, such as
`~/cookie-agent` and `/home/me/cookie-agent`, share one instance. A session
records the reported path, and reopening the session asks for that path.

## Lifecycle and failures

- **Start.** An instance starts when a session that uses it is created or
  opened, or when it is attached, and stays up until the daemon exits. A start
  that fails or exceeds `startup_timeout_ms` publishes no tools; the attach is
  rejected, and clients viewing an affected session get an error naming the
  runtime and the reason.
- **Diagnostics.** The process's standard error is captured into a bounded
  buffer (its last 32 lines, at most 4 KiB). Failure reasons end with that
  tail, so an `ssh` or `cookie runtime` error shows up in the TUI. Nothing of
  it is written to the daemon's own terminal.
- **Drop.** When a running instance's process exits or its pipe closes, calls
  in flight fail with `runtime_unavailable` (a backgrounded call reports the
  failure as its background result), and the daemon restarts the process up to 3 times after
  1 s, 2 s, and 4 s. Meanwhile the instance shows as `reconnecting`, its tools
  stay published, and new calls fail fast with `runtime_unavailable`. A
  successful restart re-lists its tools under a new instance generation (a
  call the model planned before the restart fails as `operation_changed`).
  When every attempt fails, its tools are dropped, it becomes `failed`, and
  clients get an error.
- **Retry.** A failed instance is retried when a session that uses it is
  created or opened, and on demand with `r` in `/runtime` (or
  `runtimes.reconnect`).
- **Shutdown.** When the daemon exits it sends `runtime/shutdown`, waits up to
  `shutdown_grace_ms`, and then kills the process group (a job object on
  Windows).

## `cookie runtime coding`

```sh
cookie runtime coding --stdio --workspace <path>
```

Serves one coding runtime for `<path>` over standard input and output with the
[runtime protocol](../reference/runtime-protocol.md). It needs no provider
configuration and reads no `config.toml`; a leading `~` in the path is
expanded, and the path is resolved on the machine it runs on. Standard output
carries only protocol messages and diagnostics go to standard error. It exits
on `runtime/shutdown` or when standard input closes. A template normally runs
it; running it by hand is only useful for debugging.

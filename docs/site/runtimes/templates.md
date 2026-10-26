# Runtime templates [runtimes]

A `[runtimes.<id>]` table in `~/.cookie-agent/config.toml` is a *runtime
template*: the daemon starts the runtime's process itself when a session
first needs it, typically `cookie runtime serve --stdio` on another machine
over ssh. [Runtimes](index.md) covers what every runtime shares, such as its
lifecycle and failures; [Runtimes that dial in](dial-in.md) start on their
own instead, and [MCP servers and ACP agents](tools.md) covers what a
template's `mcp` and `acp` tables host.

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

## Configuration

`runtimes` maps a runtime ID to a template; it is empty by default.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `command` | array of strings | none | The program and its arguments. Without it, the table holds the settings of the runtime that dials in with its ID (see below). |
| `env` | table of strings | `{}` | Variables added to the daemon's environment for the process. |
| `startup_timeout_ms` | positive integer | `10000` | Bound on starting the process and its answer to `runtime/initialize`, and on each capability opening, which follows its MCP servers' first connection attempts. |
| `shutdown_grace_ms` | positive integer | `3000` | How long a stopping runtime gets before its process group is killed. |
| `mcp.servers.<name>` | MCP server table | none | MCP servers the runtime hosts on its machine, one per workspace instance or, with `scope = "runtime"`, one for the runtime; see [MCP servers](tools.md#mcp-servers). Their `include_tools` and `exclude_tools` filter tools on the runtime's machine ([choosing a server's tools](../extend/mcp.md#choosing-a-servers-tools)). |
| `acp.agents.<agent>` | ACP agent table | none | ACP agents the runtime starts on its machine; see [ACP agents](tools.md#acp-agents). |

- **Runtime IDs** match `[a-z][a-z0-9_]{0,31}` because they become tool name
  prefixes. `global` is reserved. `[runtimes.local]` configures the built-in
  local runtime and accepts only `mcp` and `acp` tables.
- **A table without `command`** is no template: the daemon starts nothing
  for it, and it holds the MCP servers and ACP agents of the runtime that
  [dials in](dial-in.md) with its ID. Like `[runtimes.local]`, it accepts
  only `mcp` and `acp`; `env` or a timeout without `command` is rejected.
- **Live changes.** A runtime's `mcp` and `acp` tables reach it at once when
  the configuration is applied, whether the daemon started it or it dialed
  in: added servers and agents start, removed ones stop, changed ones
  restart, and the others keep running. `command`, `env`, and the timeouts
  take effect when the runtime next starts, which the configuration status
  reports as a restart needing to apply `runtimes`.
- **`command`** must be nonempty and its first element must name a program.
  One process serves every workspace, so the command names none. An element
  containing `{workspace}`, which older templates used, is rejected.
- **No shell is involved.** cookie starts the program directly; on
  Windows, a bare program name such as `npx` also finds its `.cmd` or
  `.bat` launcher on `PATH`. `ssh`
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
  [MCP servers](tools.md#mcp-servers).
- Templates declare no capabilities and accept no `plugins` table: the
  runtime reports its capabilities when it connects, and plugins run only in
  the global runtime.

Unknown keys and invalid values fail the configuration load, as everywhere in
`config.toml`. Changes take effect when the daemon next starts.

## `cookie runtime serve`

```sh
cookie runtime serve --stdio [--root <dir>]...
cookie runtime serve --daemon [<url>] [--id <runtime ID>] [--token <token>] [--root <dir>]...
```

With `--daemon` it dials in instead (see
[Runtimes that dial in](dial-in.md)); the rest of this section
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

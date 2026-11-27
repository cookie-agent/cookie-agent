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
| `env` | table of strings | `{}` | Variables added to the daemon's environment for the process: its [shell environment](../engine/shell_environment.md) when one was captured. |
| `startup_timeout_ms` | positive integer | `10000` | Bound on starting the process and its answer to `runtime/initialize`, and on each capability opening, which follows its MCP servers' first connection attempts. |
| `shutdown_grace_ms` | positive integer | `3000` | How long a stopping runtime gets before its process group is killed. |
| `mcp.servers.<name>` | MCP server table | none | MCP servers the runtime hosts on its machine, one per workspace instance or, with `scope = "runtime"`, one for the runtime; see [MCP servers](tools.md#mcp-servers). Their `include_tools` and `exclude_tools` filter tools on the runtime's machine ([choosing a server's tools](../extend/mcp.md#choosing-a-servers-tools)). |
| `acp.agents.<agent>` | ACP agent table | none | ACP agents the runtime starts on its machine; see [ACP agents](tools.md#acp-agents). |
| `shell_environment` | table | none | The [`[shell_environment]`](../engine/shell_environment.md) keys this runtime replaces: how it captures the user's shell environment on its machine. |
| `scope` | `"shared"` or `"session"` | `"shared"` | Who shares the process: every session, or each session tree its own (see [one runtime per session tree](#one-runtime-per-session-tree)). |
| `idle_stop_after` | duration string or seconds | `"0"` shared, `"30m"` per session | How long the process runs with nothing using it before the daemon stops it, as `"30m"`, `"2h"`, or a number of seconds; zero or less never stops it. |
| `stop_command` | array of strings | none | Run on the daemon's machine after every stop of the process, such as `docker rm -f` of its container. |
| `delete_command` | array of strings | none | With `scope = "session"`: run on the daemon's machine once the session tree that owned a process is deleted, such as `docker volume rm` of its volume. |

- **`scope`, `idle_stop_after`, `stop_command`, and `delete_command`** need
  `command`: only runtimes the daemon starts can be per session or stop
  when idle.
- **Runtime IDs** match `[a-z][a-z0-9_]{0,31}` because they become tool name
  prefixes. `global` is reserved. `[runtimes.local]` configures the built-in
  local runtime and accepts only `mcp`, `acp`, and `shell_environment` tables.
- **A table without `command`** is no template: the daemon starts nothing
  for it, and it holds the MCP servers and ACP agents of the runtime that
  [dials in](dial-in.md) with its ID. Like `[runtimes.local]`, it accepts
  only `mcp`, `acp`, and `shell_environment`; `env` or a timeout without
  `command` is rejected.
- **Live changes.** A runtime's `mcp`, `acp`, and `shell_environment` tables reach it at once when
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

## One runtime per session tree

A template whose process should exist only for one piece of work, such as a
docker container per session, sets `scope = "session"`:

```toml
[runtimes.sandbox]
command = ["docker", "run", "--rm", "-i", "--name", "cookie-{session_id}",
           "-v", "cookie-{session_id}:/work", "sandbox-img",
           "cookie", "runtime", "serve", "--stdio"]
scope = "session"
idle_stop_after = "30m"
stop_command = ["docker", "rm", "-f", "cookie-{session_id}"]
delete_command = ["docker", "volume", "rm", "cookie-{session_id}"]
startup_timeout_ms = 60000
```

- **One process per session tree.** A root session that uses `sandbox`
  starts its own process, and the subagents it delegates to, with
  `capabilities` or their own attachments, share it. Another root session
  starts another. The runtime ID stays `sandbox` everywhere: attachments,
  permission rules, `list_runtimes`, and project defaults.
- **`{session_id}`** is replaced, inside any element of `command`,
  `stop_command`, `delete_command`, and the values of `env`, with the ID of
  the root session, such as `01a1946d-d1a0-7063-9890-004906b59348`, which is
  a valid container and volume name. The process and the hooks also get it
  as `COOKIE_SESSION_ID`. A shared template that uses `{session_id}` is
  rejected.
- **It starts on use,** when a run, an attachment, a new session, or a
  delegation needs it, not when a session is opened. A cold image pull can
  take longer than the `10000` ms default of `startup_timeout_ms`.
- **It stops** after `idle_stop_after` with no run of the tree and no
  background call using it, when the session tree is deleted, when a
  client disconnects the runtime, and when the daemon exits. The next use
  starts a fresh process, so state carries over only through what the
  command mounts, here the volume named after the session.
- **`stop_command`** runs after every stop, once the process has exited or
  been killed after `shutdown_grace_ms`. cookie stops the process by closing
  its standard input, which ends `cookie runtime serve` inside the container
  and so the container, but a killed `docker run` client leaves its
  container running, and so does a daemon that crashed. The daemon
  remembers the processes it started and runs `stop_command` for those
  still recorded when it next starts.
- **`delete_command`** runs once the root session is deleted (deleting its
  project deletes it too), after the process has stopped.
- **Hooks** run without a shell, with the daemon's environment plus `env`,
  for at most 60 seconds each. A hook that fails is reported in the
  daemon's log and never holds up a stop or a deletion.
- **One machine.** Each new container would report a machine ID of its
  own, so the daemon places every process of the template on one machine
  named after it, `sandbox`, whose [memories](../concepts/memory.md) hold
  notes about the image. A shared template with `idle_stop_after` or
  `stop_command` is placed the same way.

A shared template can be temporary as well: `idle_stop_after` and
`stop_command` work on it, and it starts again at its next use.

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

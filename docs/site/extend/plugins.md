# Plugins [plugins]

Plugins are local programs that extend Cookie Agent from their own process.
A plugin can declare tools, intercept engine hooks, watch session events,
publish to a bus other plugins hear, and send messages to sessions. Install a
plugin by getting its executable from its author and adding it to your
configuration.

To write a plugin, see [Plugin development](../development/plugins.md).

## Configure a plugin

Configure each plugin under `[plugins.<name>]` in the user `config.toml`
(`~/.cookie-agent/config.toml`). Names must be nonempty, at most 128 bytes,
and contain no control characters.

```toml
[plugins.issues]
command = ["/opt/cookie-plugins/issues", "--stdio"]
env = { ISSUES_URL = "https://issues.example.com" }
cwd = "/workspace"
enabled = true
producer_messaging = false
interception_timeout_ms = 2000
startup_timeout_ms = 10000
shutdown_grace_ms = 3000
```

`command` is the program and its arguments, as for a
[runtime](../daemon/runtimes.md). The plugin's process inherits the daemon's
environment, like an MCP stdio server, and `env` adds to it, overriding
inherited variables of the same name. A plugin therefore also sees provider
API keys and other secrets the daemon reads from its environment.

Each enabled plugin starts with the daemon. Set `enabled = false` to keep a
plugin configured without starting it. Configuration changes take effect when
the daemon next starts. Plugins run their hooks in the order the file lists
them.

| Key | Type | Default | Description |
|---|---|---|---|
| `command` | array of strings | *(required)* | The program and its arguments. |
| `env` | map of strings | empty | Additions to the inherited environment; a listed variable overrides the inherited value. |
| `cwd` | string | *(none)* | The process's working directory. |
| `enabled` | boolean | `true` | Whether the plugin starts with the daemon. |
| `producer_messaging` | boolean | `false` | Lets the plugin register producers and send messages to sessions. |
| `interception_timeout_ms` | integer | `2000` | How long one hook may take. |
| `startup_timeout_ms` | integer | `10000` | How long the plugin may take to start and answer `plugin/initialize`. |
| `shutdown_grace_ms` | integer | `3000` | How long the plugin may take to exit before it is killed. |

Plugin entries reject unknown fields. `command` must name a program,
`cwd` must be nonempty when set, and every timeout must be greater than zero.
These rules apply to disabled entries too.

Plugins are defined only in the user `config.toml`; a
`<cwd>/.cookie-agent/config.toml` is never read.

## Plugin tools

A plugin's tools join the daemon's `global` runtime, next to its MCP
servers' tools, and reach every session that keeps `global` attached. A tool
keeps the name the plugin gives it unless another tool has that name; then
each is published under its source's prefix, such as `issues_search` for the
`issues` plugin's `search`. A plugin may declare a tool an override instead:
it keeps the bare name and the other tool is prefixed. The daemon's base
tools, such as `skill`, are never overridden.

A plugin tool is approved like any other tool. Its plugin names the
permission action it needs, such as `issue_read`, and the resource a call
works on; a tool may need no permission. Allow the action in an agent's
policy or a session overlay:

```yaml
permissions:
  issue_read: allow
  issue_close:
    "*": ask
```

The plugin's author should document its actions. See
[Permissions](agents.md#permissions) for policy precedence and session
overrides.

## Status and restarts

Open `/plugins` in the TUI to see every configured plugin: its state, the
tools and hooks it declares, why it last failed, and what it last wrote to
standard error. Press `r` to restart the selected plugin.

| State | Meaning |
|---|---|
| `disabled` | Configured with `enabled = false`. |
| `starting` | The process is starting or answering `plugin/initialize`. |
| `connected` | Running. |
| `reconnecting` | It exited or its connection dropped; the daemon restarts it after 1 s, 2 s, and 4 s. |
| `failed` | Every attempt failed. It stays failed until you restart it. |
| `stopped` | Stopped with the daemon. |

A failing plugin never stops the daemon or other plugins. While a plugin is
not connected, its tools are not published and its hooks are skipped, unless
the plugin declared that a hook blocks when it fails: a policy plugin may,
so that what it guards (a tool call, an input, a fork) is refused while it is
down instead of going on unchecked. A refusal names the plugin; restart it
from `/plugins`. A
plugin's standard error is kept as a short tail (the last 32 lines, at most
4 KiB) rather than shown on the terminal.

At shutdown, the daemon asks each plugin to stop, waits `shutdown_grace_ms`,
and then kills its process group.

## Producer messages

A plugin with `producer_messaging = true` can send messages to any session:
`steer` messages reach the next safe model request, and `queue` messages
start or join a later run. Enable it only for plugins you trust to talk to
your models. Sending to an idle session can start a run. A send is
acknowledged once the message is durably accepted, not once a model has
read it or an external action has finished.

A plugin can discard a message it sent while the message is still waiting.
Once a model request claims the message, discard is rejected until the claim
is released; consumed messages cannot be discarded.

When it starts, and after every restart, a producing plugin restores its
pending work from its own storage and then reports itself ready or failed.
`session.producers` shows each plugin's readiness as `starting`, `ready`,
`failed`, or `disabled`. A goal waits while a plugin it hears from is
starting, and work a failed or disabled plugin owned is unknown, never
complete. The daemon cannot tell a plugin with no pending work from one
that lost its state, so recovery is only as complete as the plugin's own
records. See [Producers](../engine/producers.md).

## Trust

Plugin commands are trusted local code, not a sandbox. A plugin runs with
your user's rights, sees the daemon's environment, and its hooks see and may
change prompts, tool calls, and model requests. Tool permissions govern its
tools, not its hooks. See
[security boundaries](../daemon/security.md#process-boundary).

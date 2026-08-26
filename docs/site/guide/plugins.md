# Plugins

Plugins are executable processes that extend cookie agent without loading code
into the engine process. Install a plugin by obtaining its executable from its
author and configuring the executable path. Plugin logs are written to the
engine's standard error stream.

For SDK usage, extension hooks, event delivery, publishing, and protocol
contracts, see [Plugin development](../development/plugins.md).

## Configure and enable a plugin

Configure each plugin under `[plugins.<name>]` in user or workspace
`config.toml`. A workspace entry replaces a user entry with the same name.

```toml
[plugins.example]
command = "/opt/cookie-plugins/example"
args = ["--stdio"]
env = { EXAMPLE_MODE = "local" }
cwd = "/workspace"
enabled = true
interception_timeout_ms = 2000
startup_timeout_ms = 10000
shutdown_grace_ms = 3000
tool_timeout_ms = 30000
```

`command` is required. `args` and `env` default to empty collections, `enabled`
defaults to `true`, and `cwd` is optional. All timeout values must be positive.
A disabled entry is not started but is still validated.

Plugin processes receive only the variables in `env`; the engine clears its
inherited environment, including `PATH`, before adding those configured values.
Configure `PATH` explicitly when the plugin itself needs it.

Each enabled plugin starts when the engine opens. Set `enabled = false` to keep
an installed plugin configured without starting it. Configuration changes take
effect when the engine next starts.

## Allow plugin tools

Plugin tools remain hidden until the selected agent's policy or a session
overlay allows or asks for the plugin's declared permission and resource. The
plugin author should document these names. For example:

```yaml
permissions:
  plugin:
    "issue_read *": allow
    "issue_delete *": ask
```

The first rule allows `issue_read` for any primary resource; the second asks
before `issue_delete`. See [Permissions](permissions.md) for policy precedence
and session overrides.

## Status and restart behavior

Plugin state progresses through `disconnected`, `connecting`, and `connected`,
or to `failed` with a diagnostic reason. A plugin failure does not stop the
engine or other plugins. Crashed plugins remain failed until the engine
restarts.

During shutdown, the engine requests plugin shutdown, waits for the configured
grace period, and then terminates the process if needed.

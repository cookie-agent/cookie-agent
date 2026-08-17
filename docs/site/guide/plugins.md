# Plugins

Plugins are executable processes that extend cookie agent without loading code into the engine
process. Each enabled plugin starts eagerly when `Engine::open` runs and communicates over
newline-delimited JSON-RPC 2.0 on standard input and standard output. Plugin logs go to the
engine's standard error stream.

## Configuration

Configure each plugin under `[plugins.<name>]` in user or workspace `config.toml`. A workspace
entry replaces a user entry with the same name.

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
```

`command` is required for every plugin entry. `args` and `env` default to empty collections,
`enabled` defaults to `true`, and `cwd` is optional. All timeout values must be positive. A
disabled entry is not started but is still validated. Plugin processes receive only the
variables in `env`; the engine clears its inherited environment, including `PATH`, before adding
those configured values. Configure `PATH` explicitly when the plugin itself needs it.

## Protocol

The extension protocol version is the semantic-version string `0.0.1`. Before version 1.0,
cookie agent requires an exact version match. A plugin reporting any other value is refused and
its status contains the reported mismatch.

The engine sends `plugin/initialize` with the protocol version, engine version, and engine
capabilities. The plugin returns its exact protocol version, configured plugin name, plugin
version, capabilities, and tool declarations. Tool names must be unique within the plugin and
use `snake_case`. The reported plugin name must exactly match the configuration entry.

Stage 3 also supports `plugin/ping` for liveness and sends the `plugin/shutdown` notification
during engine shutdown. The names `plugin/tools/call`, `plugin/resources/list`,
`plugin/resources/read`, `plugin/events/subscribe`, and `plugin/events/publish` are reserved for
later protocol stages and are not implemented.

## Lifecycle

Plugin state progresses through `disconnected`, `connecting`, and `connected`, or to `failed`
with a diagnostic reason. Spawn failures, handshake timeouts, malformed responses, version or
name mismatches, declaration errors, name collisions, unexpected EOF, and process exits affect
only that plugin; the engine and other plugins continue running.

Discovered tool names are claimed in the same global namespace as built-in and MCP tools. A
collision fails the plugin. Claims are removed immediately when its process exits or standard
output closes. During shutdown, the engine sends `plugin/shutdown`, closes plugin standard input,
waits for `shutdown_grace_ms`, and then terminates the process if needed. Shutdown remains bounded
when initialization is still pending.

## Current limitation

This stage discovers declarations only. Claimed names are reserved against collisions, but
plugin tools are not exposed to model sessions and cannot execute. Resource access and event
streaming are also deferred to later stages.

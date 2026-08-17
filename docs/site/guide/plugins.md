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
tool_timeout_ms = 30000
```

`command` is required for every plugin entry. `args` and `env` default to empty collections,
`enabled` defaults to `true`, and `cwd` is optional. All timeout values must be positive. A
disabled entry is not started but is still validated. Plugin processes receive only the
variables in `env`; the engine clears its inherited environment, including `PATH`, before adding
those configured values. Configure `PATH` explicitly when the plugin itself needs it.

## Protocol

The extension protocol version is the semantic-version string `0.0.2`. Before version 1.0,
cookie agent requires an exact version match: additive method or schema changes bump the patch
version, and plugins must update before connecting to the new engine. A plugin reporting any
other value is refused and its status contains the reported mismatch.

The engine sends `plugin/initialize` with the protocol version, engine version, and engine
capabilities. The plugin returns its exact protocol version, configured plugin name, plugin
version, capabilities, and tool declarations. Tool names must be unique within the plugin and
use `snake_case`. The reported plugin name must exactly match the configuration entry.

The protocol also supports `plugin/ping` for liveness and sends the `plugin/shutdown`
notification during engine shutdown.

## Tools

Each tool declaration contains its verbatim `name`, description, JSON Schema parameters,
verbatim `permission_name`, and an optional `primary_resource_param`. Names are not prefixed or
rewritten. The primary resource parameter supplies the approval display argument and call
resource when present.

At execution time the engine sends `plugin/tools/call` with the tool name, session and invocation
IDs, arguments, resource, and an optional cancellation token. The plugin returns `content` and
`is_error`. A JSON-RPC error, transport failure, timeout, cancellation, or plugin exit fails the
tool call. `is_error` remains structured tool-result metadata, parallel to MCP tool results.

Plugin tools use the fail-closed permission pipeline. They are hidden until the agent policy or a
session overlay has an `allow` or `ask` rule for the `plugin` action and declared permission. The
permission resource is the declared permission name followed by the primary resource when one is
present. For example:

```yaml
permissions:
  plugin:
    "issue_read *": allow
    "issue_delete *": ask
```

Skill `allowed-tools` entries `Plugin` and `Plugin(name:*)` govern plugin tools in the same way as
the existing MCP group. A pinned call to a connected but disallowed plugin tool reports that the
tool is not enabled instead of treating it as undiscovered.

## Lifecycle

Plugin state progresses through `disconnected`, `connecting`, and `connected`, or to `failed`
with a diagnostic reason. Spawn failures, handshake timeouts, malformed responses, version or
name mismatches, declaration errors, name collisions, unexpected EOF, and process exits affect
only that plugin; the engine and other plugins continue running.

Discovered tool names are claimed in the same global namespace as built-in and MCP tools. A plugin
that collides with either category fails. When plugins collide with each other, the last plugin to
finish registration wins that tool; the earlier plugin remains connected for its other tools and
publishes a status diagnostic. The winner's permission declaration applies. Claims and listings
are removed immediately when the owning process exits or standard output closes, and prepared
calls fail revalidation after removal. Crashed plugins stay failed until engine restart.

Each call is bounded by `tool_timeout_ms`, which defaults to 30000. During shutdown, the engine
sends `plugin/shutdown`, closes plugin standard input, waits for `shutdown_grace_ms`, and then
terminates the process if needed. Shutdown remains bounded when initialization is still pending.

## Current limitation

Plugin resource methods and session event streaming remain deferred to stage 5. Stage 4 does not
send session events to plugins.

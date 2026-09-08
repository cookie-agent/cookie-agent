# Plugins [plugins]

Plugins are executable processes that extend cookie agent without loading code
into the engine process. Install a plugin by obtaining its executable from its
author and configuring the executable path. Plugin logs are written to the
engine's standard error stream.

For SDK usage, extension hooks, event delivery, publishing, and protocol
contracts, see [Plugin development](../development/plugins.md).

## Configure and enable a plugin

Configure each plugin under `[plugins.<name>]` in user or workspace
`config.toml`. A workspace entry replaces a user entry with the same name.
The map defaults to empty. Names must be nonempty, at most 128 bytes, and contain
no control characters. The following is a complete illustrative `config.toml`;
the executable and working directory must exist when the engine starts.

```toml
[plugins.example]
command = "/opt/cookie-plugins/example"
args = ["--stdio"]
env = { EXAMPLE_MODE = "local" }
cwd = "/workspace"
enabled = true
producer_messaging = false
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

## Producer messages

Plugins that send messages to the model must explicitly declare producer support.
Enable it only for trusted plugins with `producer_messaging = true`; this setting
defaults to `false` independently of plugin tool permissions. A plugin must register
for the destination session before sending. Registrations may outlive a turn and
must be explicitly closed by the plugin. There is no registration expiry.

Each message chooses `steer` (the next safe model request) or `queue` (a subsequent
run). Sending to an idle session can start a run. A successful send acknowledges
durable acceptance, not model execution or completion of an external action.
Retries use the plugin's stable message key; changing the configured plugin name
changes its durable deduplication identity.

A plugin can explicitly discard its own waiting message by session and message
receipt, even after unregistering or reconnecting under the same configured name.
The session actor durably claims a message before request preparation and hooks.
That reservation removes the message from waiting, and discard rejects until the
claim is released, even if no network request has been sent. A claim is not proof
that the provider received or executed a request. After failed preparation or
cancellation, releasing the claim may return an unconsumed message to waiting;
consumed messages cannot return or be discarded. Repeated discard of an already
discarded message is harmless. Discard does not close a producer registration,
cancel a run, or undo effects of a message already delivered. A rejected discard
does not establish exactly-once execution or external effects.

The old session-publishing API cannot authorize model-bound messages. Plugins
using that API must migrate to explicit producer registration and sends. Ordinary
observational bus publication is unaffected.

## Allow plugin tools

Plugin tools remain hidden until the agent's policy or a session overlay has
any `allow` or `ask` rule for the `plugin` action; the gate is action-level and
does not match individual resource patterns. Calls still check the plugin's
declared permission and resource, and unmatched calls are denied. The plugin
author should document these names. For example:

```yaml
permissions:
  plugin:
    "issue_read *": allow
    "issue_delete *": ask
```

The first rule allows `issue_read` for any primary resource; the second asks
before `issue_delete`. See [Permissions](agents.md#permissions) for policy precedence
and session overrides.

## Status and restart behavior

Plugin state progresses through `disconnected`, `connecting`, and `connected`,
or to `failed` with a diagnostic reason. A plugin failure does not stop the
engine or other plugins. Crashed plugins remain failed until the engine
restarts.

Producer readiness is tracked separately as `starting`, `ready`, `failed`, or
`disabled`, and is inspectable through `session.producers`. During startup the
plugin restores pending work from its own storage or external service and registers
fresh producer IDs before explicitly completing recovery. Recovery has no timeout;
an indefinitely `starting` plugin can hold goal continuations indefinitely.

Failed or disabled producer plugins leave external work **unknown**, not complete.
The runtime surfaces recovery diagnostics and goal readiness remains blocked;
neither state finishes goal checklist items. Accepted messages recover from the
session log independently of plugin restoration. The engine cannot distinguish a
plugin with no pending work from one that lost its own state, so restart recovery
is only as complete as the plugin's own durable records.

During shutdown, the engine requests plugin shutdown, waits for the configured
grace period, and then terminates the process if needed.

## Accepted fields

Plugin definitions are layered by plugin name. A workspace definition replaces
the complete same-name user definition without merging nested fields. The
replacement keeps that user's position in authored order, which is also the
interception order; workspace-only plugins append in workspace-authored order.
Plugin authors can use the
[development guide](../development/plugins.md) for protocol and tool contracts.

| Key | Type | Default | Description |
|---|---|---|---|
| `command` | string | *(required)* | Plugin executable. |
| `args` | array of strings | empty | Command arguments. |
| `env` | map of strings | empty | Complete child environment; inherited variables are cleared. |
| `cwd` | string | *(none)* | Child working directory. |
| `enabled` | boolean | `true` | Whether the plugin starts with the engine. |
| `producer_messaging` | boolean | `false` | Opt in to the producer messaging protocol capability, allowing the plugin to register producers and send model-bound messages. |
| `interception_timeout_ms` | integer | `2000` | Positive timeout for interception requests. |
| `startup_timeout_ms` | integer | `10000` | Positive timeout for initialization. |
| `shutdown_grace_ms` | integer | `3000` | Positive graceful shutdown period before termination. |
| `tool_timeout_ms` | integer | `30000` | Positive timeout for each plugin tool call. |

Plugin entries reject unknown fields. `command` must be present and nonempty,
`cwd` must be nonempty when set, and every timeout must be greater than zero.
These rules apply even when `enabled = false`; disabling an entry prevents
startup but does not bypass configuration validation.

The child receives only the variables in `env`. It does not inherit the
engine's environment, including `PATH`; use an absolute `command` and configure
`PATH` explicitly when the executable or its children need it.

Plugin commands are trusted local code, not a sandbox. Review workspace plugin
configuration before opening the workspace. Tool permissions do not prevent
configured startup or independently authorize interception hooks. See
[security boundaries](security.md#process-boundary).

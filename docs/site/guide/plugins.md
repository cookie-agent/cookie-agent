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

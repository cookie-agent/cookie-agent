# Plugin Development

A plugin is a local program the daemon starts and talks to over its standard
input and output: newline-delimited JSON-RPC 2.0, one message per line. A
plugin can:

- declare **tools**, which it prepares and runs with the
  [runtime protocol](../reference/runtime-protocol.md)'s tool messages;
- **intercept** engine hooks, such as a tool call or a model request;
- **stream** session events and a bus other plugins publish to;
- **produce** messages to any session.

This page covers the Rust SDK and the plugin protocol. To install or
configure a plugin, see the [user guide](../guide/plugins.md). The protocol's
design and history are in the [plugins spec](../specs/plugins-v1.md).

## Rust SDK

The `cookie_agent_plugin_sdk` crate (Apache-2.0) handles the handshake,
framing, tool preparation, hooks, and producer requests. Until it is
published, add it as a path dependency:

```toml
[dependencies]
cookie_agent_plugin_sdk = { path = "../cookie-agent/crates/plugin_sdk" }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt"] }
```

A plugin with one tool:

```rust
use cookie_agent_plugin_sdk::{PermissionAction, PluginServer, ToolDecl, ToolFailure, ToolOutput};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), cookie_agent_plugin_sdk::PluginError> {
    let mut echo: ToolDecl = serde_json::from_value(json!({
        "name": "echo",
        "description": "Echo text back to the model",
        "parameters": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        }
    }))?;
    echo.permission = Some(PermissionAction::new("echo").expect("valid action"));
    PluginServer::builder("echo", env!("CARGO_PKG_VERSION"))
        .tool(echo, |_call, arguments| async move {
            let text = arguments["text"]
                .as_str()
                .ok_or_else(|| ToolFailure::new("text is required"))?;
            Ok(ToolOutput::success(text))
        })
        .run_stdio()
        .await
}
```

The name given to `builder` must be the plugin's configured name. The SDK
derives the plugin's capabilities from what is registered and validates each
tool's name and parameter schema when the server is built.

- **Tools.** `tool(declaration, handler)` registers a tool whose calls are
  approved under its declared permission with no resource;
  `tool_with_resource(declaration, resource, handler)` also derives each
  call's permission resource, such as a path or an issue number, from its
  arguments. A handler gets a `ToolContext` (the session, run, and call IDs,
  whether the model asked for the background, cancellation, `progress`, and
  `detachable`) and the call's arguments, and returns a `ToolOutput` or a
  `ToolFailure`. `ToolOutput::error` is a result the model should see as a
  failure. The result's display, which clients show, is the output's text as
  one `text` display block.
- **Hooks.** One builder method per hook, named after it, such as
  `tool_before_call`; helpers like `allow()`, `block(reason)`, `modify`,
  `replace`, `append_system_prompt`, `inject_message`, and
  `cancel_compaction` build results.
- **Events.** `on_event` and `on_bus_event` receive streamed notifications.
  A handler may publish one bus event with `context.emit_bus(...)` after
  `enable_bus_publishing()`.
- **Producers.** `enable_producers()` and, optionally, `on_recovery(...)`.
  See [Producers](#producers).

The examples `echo_plugin.rs` and `producer_plugin.rs` in
`crates/plugin_sdk/examples` build with
`cargo check -p cookie_agent_plugin_sdk --examples`.

## Handshake

The daemon starts the plugin's `command` in its own process group (a job
object on Windows), with the daemon's environment plus the plugin's `env`,
and sends `plugin/initialize` within `startup_timeout_ms`:

```json
{"jsonrpc":"2.0","id":1,"method":"plugin/initialize","params":{
  "protocol_version":2,"engine_version":"0.3.0",
  "capabilities":{"producer_messaging":false}}}
```

`capabilities.producer_messaging` is true when the plugin's configuration
enables producers. The plugin answers with the same protocol version, its
configured name, its own version, its capabilities, and its tools:

```json
{"jsonrpc":"2.0","id":1,"result":{
  "protocol_version":2,"name":"issues","version":"1.2.0",
  "capabilities":{"producer_messaging":false,"subscribe_events":false,
    "subscribe_bus":false,"publish_bus":false,"intercept":["tool_before_call"]},
  "tools":{"tools":[],"prompt_sections":[]}}}
```

The protocol version is the integer `2` and must match exactly; a different
version or name fails the start. Capabilities are fixed for the connection;
`tools` is the runtime protocol's `RuntimeTools`.

Request IDs are integers. Every frame is at most 48 MiB, parsed strictly,
and a malformed frame ends the connection. Standard output carries only
protocol messages; standard error is kept as a bounded tail (the last 32
lines, at most 4 KiB) that clients see in plugin status.

At shutdown the daemon sends the `plugin/shutdown` notification, waits
`shutdown_grace_ms`, and kills the process group.

## Lifecycle

A plugin is `starting`, `connected`, `reconnecting`, `failed`, `disabled`,
or `stopped`. When it exits or its connection drops, the daemon starts it
again after 1 s, 2 s, and 4 s, and marks it `failed` when every attempt
fails. A failed plugin stays failed until a client restarts it
(`plugins.restart`); a restart of a connected plugin stops and starts it.
Each start is a new connection: the plugin answers `plugin/initialize`
again, and a producing plugin recovers again.

Clients read status with `plugins.list` and are told of changes with
`plugins.changed` ([client protocol](../reference/protocol.md)). A status
carries the plugin's state, why it last failed, its tools and hooks, its
dropped-notification count, and its standard error tail.

## Tools

A plugin's tools are a runtime's tools. It declares them in the initialize
result and later with `runtime/tools_changed`, and the daemon runs them with
the same client it uses for a runtime:

| Message | Direction | Purpose |
|---|---|---|
| `tool/prepare` | daemon → plugin | Prepare one turn's calls: the plugin reports each call's approval identity, normalized arguments, permission and resource, and presentation. |
| `tool/revalidate` | daemon → plugin | Check a prepared call is still valid before it runs. |
| `tool/execute` | daemon → plugin | Run a prepared call; the result is its output and persisted result, whose `display` is a list of display blocks. |
| `tool/release` | daemon → plugin | Forget a prepared call that will not run. |
| `tool/cancel` | daemon → plugin | Cancel an executing call. |
| `tool/progress` | plugin → daemon | Report a running call's status and output, and optionally a `display` delta (`append` or `replace`) of its live display. |
| `tool/detachable` | plugin → daemon | The call may now move to the background. |
| `runtime/tools_changed` | plugin → daemon | Replace the plugin's tool list. |

Their params and results are in the
[runtime protocol reference](../reference/runtime-protocol.md#methods); the
types are in the protocol crate's `runtime_protocol` module. A declaration
has a `name`, `description`, and JSON Schema `parameters`, and optionally a
free-form `permission` action (a tool may need none), `collision`,
`concurrency`, `result_truncation`, an `output` of one or up to 8 named
streams, and `background` support. The SDK fills in `tool/prepare`; a
plugin written without it must compute each call's approval identity as a
runtime does.

**Names.** Plugin tools join the `global` runtime, but each plugin composes
as its own source, prefixed with the plugin's name. A tool keeps its name
unless another tool has it. Then the declared `collision` decides: with
`prefix` (the default), every tool of that name is published under its
source's prefix (`<plugin>_<tool>`, `<attachment>_<tool>`); with `override`,
the tool keeps the bare name and the others are prefixed (or hidden, if they
are `fallback`). Two overrides of one name are both prefixed, with a
diagnostic. Base-layer
names, such as `skill`, are never taken. While a plugin is not connected its
tools are not published.

**Permissions.** A call is approved under the action its declaration names
and the resource its preparation reports, like a runtime tool; the agent
policy names the action (`issue_read: allow`). A tool that declares no
permission runs without approval.

## Hooks

A plugin lists the hooks it handles in `capabilities.intercept`; it is never
sent another. Each hook is a request `plugin/intercept/<hook>`:

| Hook | Runs | May |
|---|---|---|
| `tool_before_call` | after a call passes policy and approval | allow, block, or modify the arguments |
| `tool_after_result` | after a call finishes, before it is committed | replace the model-facing content |
| `agent_before_start` | at a run's start | append to or replace the system prompt, inject a message |
| `session_before_compact` | before compaction | add or replace its instructions, or cancel it |
| `user_before_input` | before root user input or steering is committed | allow, transform it, or handle it without a run |
| `model_before_request` | before each user-facing model attempt | replace the messages, adjust inference parameters |
| `provider_before_request` | before the normalized request is sent | replace it |
| `provider_after_response` | after the response status arrives | observe the status |
| `message_end` | after an assistant message is assembled | replace its content |
| `model_before_select` | before a run's model selection changes, or a fallback | block it |
| `session_before_fork` | before a fork | block it |
| `session_before_revert` | before a revert | block it, or override its instructions |

Hooks run in the order the configuration lists the plugins, each within its
`interception_timeout_ms`. A modification is validated before the next
plugin sees it, and an invalid one is skipped with a diagnostic. A hook that
times out, whose plugin is not connected or exits, or that answers with an
error or a malformed result **fails open**: the operation goes on, later
plugins still run, and the session records a `diagnostic` of source
`plugin:<name>` (`interception_timeout`, `interception_crash`, or
`interception_rejected`).
A block stops later hooks.

`tool_before_call` and `tool_after_result` carry the call's `origin`:
`{"kind":"base"}` for a daemon base tool, or
`{"kind":"runtime","attachment":"local","runtime_id":"local"}` for a
runtime's or plugin's tool. `permission_name` is `null` for a tool that
needs no permission. A hook cannot change a call's permission or resource,
never sees calls policy denied, and never grants permission.

Internal agents (title, approval, compaction) are never intercepted.
`user_before_input` runs for root sessions only; the other agent hooks also
run for delegated sessions.

## Events and the bus

With `subscribe_events`, a plugin receives `plugin/event` notifications:
`{ session_id, context_id, seq, event, timestamp }`, where `event` is the
stored `EventPayload` ([events](../reference/events.md)). Durable events
arrive after they are appended, in sequence order per session. Live-only
output (tool progress, text and reasoning deltas, native compaction) arrives
as it happens and carries the `seq` of the durable event it follows. There
is no replay and no filter.

With `subscribe_bus`, it receives `plugin/bus_event` notifications that
other plugins publish: `{ session_id, context_id, plugin, name, payload }`.
A plugin never receives its own bus events.

Each plugin has a queue of 1024 notifications. When it is full, a new
notification evicts the oldest of the lowest class it outranks: live output
first, then ordinary events, and a tool's termination only when nothing else
is queued. Drops are counted in the plugin's status and recorded as
diagnostics. A slow plugin never delays the daemon or another plugin.

## Publishing

A plugin with `publish_bus` publishes with the `plugin/emit` notification:
`{ session_id, context_id, name, payload }`. The `context_id` must come from
a notification or hook request the daemon sent it for that session; each one
may be used once. A notification's context expires five seconds after it is
written to the plugin, a hook's when the hook is answered, and every hook
call gets its own. The daemon answers with a `plugin/emit_result`
notification: `{ name, bus, reason }`, with `bus` `published`, `dropped`, or
`rejected`.

A published event goes to the other plugins subscribed to the bus and to
clients subscribed to the session (`events.plugin`); it is not stored and
never reaches a model. Payloads are at most 256 KiB, names 128 control-free
characters, and a whole event 272 KiB; each plugin may publish 40 events per
second and 4 MiB per minute per session.

## Producers

A plugin sends messages to sessions as a producer, with the runtime
protocol's producer methods and types:

| Method | Direction | Params → result |
|---|---|---|
| `producer/register` | plugin → daemon | `{ session_id, name }` → `{ producer_id }` |
| `producer/send` | plugin → daemon | `{ session_id, producer_id, mode, idempotency_key, description, body }` → `{ message_id }` |
| `producer/unregister` | plugin → daemon | `{ session_id, producer_id }` → `{}` |
| `producer/discard` | plugin → daemon | `{ session_id, message_id }` → `{}` |
| `producer/recovery/start` | daemon → plugin | notification, `{}` |
| `producer/recovery/complete` | plugin → daemon | `{ outcome }` → `{}` |

Producing needs both the plugin's `producer_messaging` capability and its
configuration's `producer_messaging = true`. A plugin may produce to any
session. The daemon names its source `plugin:<name>` from the connection, so
a plugin cannot speak for another. A plugin may run any number of named
producers (`[a-z0-9][a-z0-9_.-]{0,63}`), each with its own idempotency keys;
the SDK's `register_producer` registers `default`.

Each send chooses `steer` (the next safe model request) or `queue` (a later
run), and carries a description of at most 1024 bytes for display. The body
is at most [`[producers] max_body_bytes`](../engine/producers.md). The
answer follows durable acceptance; a retry with the same key and the same
body, description, and mode returns the original message ID, and a missing
answer is commit-uncertain. Discard works on a waiting message the plugin's
source sent in that session, under any producer name, even after unregister
or a restart. A producer ID lasts only as long as the connection.

After each successful start the daemon sends `producer/recovery/start`. The
plugin restores its pending work from its own storage, registers producers
and sends as needed, and then completes recovery with
`{"status":"ready"}` or `{"status":"failed","message":"..."}`. There is no
deadline; goals wait for a starting source, and a failed or disabled source
leaves its work unknown. The SDK runs `on_recovery` and completes recovery
when it returns, or reports ready at once when there is none. The external
producers spec has the whole contract:
[external producers](../specs/external-producers.md).

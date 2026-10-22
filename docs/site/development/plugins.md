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
configure a plugin, see the [user guide](../extend/plugins.md). The protocol's
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
use cookie_agent_plugin_sdk::{
    DisplayBlock, PermissionAction, PluginServer, Presentation, ToolDecl, ToolFailure, ToolOutput,
};
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
            Ok(ToolOutput::success(text).display(DisplayBlock::text(text)))
        })
        .prepare_presentation("echo", |call| {
            Presentation::new("Echo")
                .primary_argument(call.arguments["text"].as_str().unwrap_or_default())
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
  arguments. The daemon builds each call's approval identity from them. A
  handler gets a `ToolContext` (the session, run, and call IDs, whether the
  model asked for the background, cancellation, `progress`,
  `progress_display`, and `detachable`) and the call's arguments, and
  returns a `ToolOutput` or a `ToolFailure`.
- **Results.** `ToolOutput` is a builder: `ToolOutput::success(text)` or
  `ToolOutput::error(text)` (a result the model should see as a failure),
  then `title`, `display` (one display block; `displays` for several),
  `metadata`, `attachment(mime_type, filename, bytes)`, and `failed`. The
  model reads the text and the metadata; clients show the display blocks,
  or the text when there are none. The SDK sends an attachment's bytes with
  the result.
- **Presentation.** `prepare_presentation(tool, |call| …)` returns a
  `Presentation` for each prepared call of `tool`: a title, a `hint` for
  icons (such as `shell` or `fetch`), a `primary_argument`, and display
  blocks of its `argument`s and a `preview` of what it will do. Without it a
  call is titled with the tool's name and shows its permission resource.
- **Progress.** `progress(message, output)` reports a status line and
  output text; `progress_display(message, delta)` changes the call's live
  display with a `DisplayDelta` (`Append { index, text, stream }` or
  `Replace { blocks }`).
- **Hooks.** One builder method per hook, named after it, such as
  `tool_before_call`; helpers like `allow()`, `block(reason)`, `modify`,
  `replace`, `append_system_prompt`, `inject_message`, and
  `cancel_compaction` build results, and `transform_input`,
  `rewrite_input_text`, `replace_messages`, `rewrite_history_text`,
  `replace_message_content`, and `rewrite_message_text` build the
  replacements of `user_before_input`, `model_before_request`, and
  `message_end` (the `rewrite_*` ones change text parts and keep every
  other part as sent). `on_hook_error(hook, ExtensionHookOnError::Block)`
  makes a failure of that hook block its operation (see [Hooks](#hooks)).
- **Events.** `on_event` and `on_bus_event` receive streamed notifications;
  `event_patterns(["run_*", ...])` narrows the event types `on_event` gets.
  A handler may publish one bus event with `context.emit_bus(...)` after
  `enable_bus_publishing()`. `on_unknown` receives notifications the SDK does
  not know or cannot read (see [Compatibility](#compatibility)).
- **Producers.** `enable_producers()` and, optionally, `on_recovery(...)`.
  See [Producers](#producers).

The examples `echo_plugin.rs` and `producer_plugin.rs` in
`crates/plugin_sdk/examples` build with
`cargo check -p cookie_agent_plugin_sdk --examples`. The SDK exposes the
protocol version it speaks as `PLUGIN_PROTOCOL_VERSION`; its crate version's
major follows the protocol's major.

## Reference plugin

`examples/plugins/python/` holds a plugin in plain Python with no
dependencies, written against the protocol rather than the SDK:

- `cookie_plugin.py` is a small helper for any Python plugin: framing, the
  handshake and its version check, tool preparation and execution, hooks,
  events, and producer requests, single-threaded, keeping what the daemon
  sends while a handler waits for an answer.
- `reference_plugin.py` uses it: a tool, `word_count`, with a presentation, a
  live display, and a result of display blocks and metadata; a
  `tool_before_call` hook with `on_error = "block"`; an event subscription
  by pattern (`run_*`, `tool_call_*`); and a producer that queues a message
  once a call has finished.
- `check_reference_plugin.py` plays the daemon through a scripted
  conversation and validates every message both sides send against the
  [published schemas](../reference/schemas.md#published-schemas), with a
  standard-library validator for the subset of JSON Schema they use. CI runs
  it, and an engine test runs the plugin against the daemon.

To try it, add it to `config.toml`:

```toml
[plugins.reference]
command = ["python3", "/path/to/cookie-agent/examples/plugins/python/reference_plugin.py"]
producer_messaging = true
```

and allow its permission action in an agent (`text_read: allow`).

## Handshake

The daemon starts the plugin's `command` in its own process group (a job
object on Windows), with the daemon's environment plus the plugin's `env`,
and sends `plugin/initialize` within `startup_timeout_ms`:

```json
{"jsonrpc":"2.0","id":1,"method":"plugin/initialize","params":{
  "protocol_version":"1.4.0","engine_version":"0.3.0",
  "features":["producer_messaging"],"interception_timeout_ms":2000}}
```

`features` lists what the plugin's configuration enables: today only
`producer_messaging`, sent when `producer_messaging = true`. A version never
implies a feature. `interception_timeout_ms` is the plugin's
`interception_timeout_ms`: how long the daemon waits for a hook's answer. The
plugin answers with its own protocol version, its configured name, its own
version, its features, its capabilities, and its tools:

```json
{"jsonrpc":"2.0","id":1,"result":{
  "protocol_version":"1.4.0","name":"issues","version":"1.2.0","features":[],
  "capabilities":{"producer_messaging":false,"subscribe_events":["run_*","tool_call_*"],
    "subscribe_bus":false,"publish_bus":false,
    "intercept":[{"hook":"tool_before_call","on_error":"block"}]},
  "tools":{"tools":[],"prompt_sections":[]}}}
```

The protocol version is a semver string, currently `1.4.0`
(`PLUGIN_PROTOCOL_VERSION`, which the SDK re-exports). Each end reads the
other's `protocol_version` from the raw JSON before anything else, and refuses
an incompatible one with `unsupported_protocol` naming both versions: the
majors must be equal, so no `0.x` build from before the freeze connects. A
minor only adds, so the two ends use what the lower minor includes and the
patch never matters (see [Compatibility](../reference/compatibility.md)). A
different name also fails the start. No plugin feature is defined
yet; the daemon ignores one it does not know and shows it in the plugin's
status. Capabilities are fixed for the connection; `tools` is the runtime
protocol's `RuntimeTools`. The SDK exposes what the handshake said about the
daemon to handlers as `PluginContext::daemon()`: its protocol version,
`engine_version`, features, and interception timeout. Before
`plugin/initialize` the SDK refuses every request with `not_initialized`.

Request IDs may be integers or strings (the daemon sends integers). Every
frame is at most 48 MiB, and a frame that is not a JSON-RPC request,
notification, or reply ends the connection. See [Compatibility](#compatibility)
for what each end does with what it does not know. Standard output carries only
protocol messages; standard error is kept as a bounded tail (the last 32
lines, at most 4 KiB) that clients see in plugin status.

At shutdown the daemon sends the `plugin/shutdown` notification, waits
`shutdown_grace_ms`, and kills the process group.

## Compatibility

The plugin protocol follows the runtime protocol's
[rules](../reference/runtime-protocol.md#framing):

- **Information is read tolerantly.** Everything the daemon sends a plugin
  (initialize params, hook params, events, bus events, `plugin/emit_result`,
  and the `tool/*` requests), and what a plugin answers or declares
  (initialize and hook results, tool declarations, `runtime/tools_changed`),
  ignores members its reader does not know.
- **Commands are strict.** `plugin/emit`, `tool/progress`, `tool/detachable`,
  and the `producer/*` requests are refused when they carry a member or an
  enum value the daemon does not know: a request with `-32602`,
  `unknown_field` or `unknown_variant`, and the member's JSON path in
  `data.field`; a notification with a note in the plugin's status. Gate new
  fields on the daemon's version (`PluginContext::daemon()`).
- **Not from a plugin.** The daemon ignores the runtime-only
  `capability/tools_changed` and `mcp/changed` from a plugin, with a note in
  its status.
- **Open enums.** Besides the runtime protocol's: an entry of
  `capabilities.intercept` naming a hook the daemon does not know, one it
  cannot read, and one repeating a hook are skipped, each with a note in the
  plugin's status, and the plugin still starts; an `on_error` it does not
  know reads as `block`; a feature it does not
  know is ignored the same way; an unknown `bus` status of
  `plugin/emit_result` reads as `dropped`; a hook's tool `origin` of an
  unknown `kind`, and an event of an unknown type, read as `Open::Unknown`;
  an unknown `model_before_select` `source` reads as `Unknown`. A hook
  result's `action` and a message `role` are closed: an unknown one makes
  the answer malformed, which is a failure of the hook (see
  [Hooks](#hooks)).
- **Errors** follow the runtime protocol's [scheme](../reference/runtime-protocol.md#errors):
  a standard code, or `-32000` with `data.code`, and a message a user can
  understand. When a plugin fails to start or a call to it fails, the daemon
  shows the plugin's message and names the plugin.

The SDK does its part: a hook whose params carry members it does not know
still runs; an event of a type it does not know reaches `on_event` as
`Open::Unknown`; a notification it does not know, or an event or bus event
it cannot read, goes to `on_unknown` when the plugin registers it, and is
otherwise ignored (one it cannot read with a line on standard error, which
the daemon shows as the plugin's `stderr`). `emit_bus` waits for its
`plugin/emit_result` at most the daemon's `interception_timeout_ms` and then
resolves as `dropped`. A `ToolFailure` with a blank message is sent as one
saying the tool failed without saying why.

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
| `tool/prepare` | daemon → plugin | Prepare one turn's calls: the plugin reports each call's normalized arguments, permission and resource, and presentation. The daemon builds the call's approval identity from them. |
| `tool/revalidate` | daemon → plugin | Check a prepared call is still valid before it runs. |
| `tool/execute` | daemon → plugin | Run a prepared call; the result is its output, optional title, display blocks, metadata, and attachments, and whether it failed. The daemon builds the stored result from it. |
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
streams, and `background` support. The SDK fills in `tool/prepare`. A plugin
written without it reports no approval identity: the daemon builds it from
the call's permission, resource, and normalized arguments and from the
plugin's name and the tool's, as it does for a runtime's call (see
[Approval identity](../reference/runtime-protocol.md#approval-identity)).

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

A plugin lists the hooks it handles in `capabilities.intercept`, each as
`{ "hook": "<name>", "on_error": "allow" | "block" }` (`allow` when
`on_error` is absent); it is never sent another. Each hook is a request
`plugin/intercept/<hook>`:

| Hook | Runs | May |
|---|---|---|
| `tool_before_call` | after a call passes policy and approval | allow, block, or modify the arguments |
| `tool_after_result` | after a call finishes, before it is committed | replace the model-facing content |
| `agent_before_start` | at a run's start | append to or replace the system prompt, inject a message |
| `session_before_compact` | before compaction | add or replace its instructions, or cancel it |
| `user_before_input` | before root user input or steering is committed | allow, transform it, or handle it without a run |
| `model_before_request` | before each user-facing model attempt | replace the messages, adjust inference parameters |
| `message_end` | after an assistant message is assembled | replace its content |
| `model_before_select` | before a run's model selection changes, or a fallback | block it |
| `session_before_fork` | before a fork | block it |
| `session_before_revert` | before a revert | block it, or override its instructions |

Hooks run in the order the configuration lists the plugins, each within its
`interception_timeout_ms`. A modification is validated before the next
plugin sees it, and an invalid one is skipped with a diagnostic. A block
stops later hooks.

A hook call **fails** when it times out, when its plugin is not connected or
exits, or when the plugin answers with an error or with a result that does
not decode (an unknown `action` included). The session records a
`diagnostic` of source `plugin:<name>` and kind `interception_timeout`,
`interception_crash`, or `interception_rejected`, and the entry's `on_error`
decides what happens next:

- **`allow`** (the default): the hook fails open. The operation goes on as
  if the plugin had not intercepted it, and later plugins still run.
- **`block`**: the operation is blocked as if the plugin had blocked it, and
  later plugins do not run. Declare it for a hook that enforces a policy, so
  that the policy holds while the plugin is down.

| Hook | With `block`, a failure |
|---|---|
| `tool_before_call` | fails the call before it runs; the model reads "Tool call blocked: plugin `<name>` could not check this: …" |
| `tool_after_result` | fails the call after it ran, withholding its result; the model reads "Tool result withheld: …" |
| `agent_before_start`, `user_before_input`, `model_before_select` (a run's model) | refuses `run.start`, `run.steer`, or `session.submit_input` |
| `model_before_select` (a fallback) | ends the run without falling back |
| `model_before_request`, `message_end` | abandons the model attempt, without a model error, and fails the run |
| `session_before_compact` | refuses `session.compact`; an automatic compaction is skipped, as when the plugin cancels it |
| `session_before_fork`, `session_before_revert` | refuses the fork or revert |

A refused request fails with the error `plugin_hook_failed`, whose data names
the `plugin` and the `hook` and whose `detail` says what went wrong. A failed
run's error says the same.

### What hooks see

Hook params never carry the daemon's internal or provider types. Three hooks
see views of them:

- **`model_before_request`** gets `messages`, a list of
  `{ "role": "system" | "user" | "assistant" | "tool", "content": [part] }`,
  and `model`, `{ provider_id, model_id, adapter }`. A part is
  `{"type":"text","text":…}`,
  `{"type":"file","artifact":{sha256,byte_length,media_type,filename?}}`,
  `{"type":"tool_call","id":…,"name":…,"arguments":{…}}`, or
  `{"type":"tool_result","tool_call_id":…,"content":"…","is_error":false}`.
  A tool result's content is its text (JSON values as JSON text), and the
  files it carries follow it as `file` parts. The view leaves out reasoning,
  provider metadata, replay state, and extension parts. With
  `{"action":"replace","messages":[…]}`, a message returned exactly as it was
  sent keeps all of that; any other message is built from its parts alone (an
  assistant message so built has no reasoning or replay state). A `file` part
  must name a file of the request, by its `sha256`; a replacement cannot add
  files. A system message holds only text, a user message text and files, and
  a tool message results and the files after them.
- **`message_end`** gets `content`, the assistant message's parts: `text`,
  `reasoning` (`{"type":"reasoning","text":…}`), `tool_call`, and `file`
  (metadata only), the same shapes as above. With
  `{"action":"replace","content":[…]}`, a part returned exactly as sent keeps
  its metadata; others are built from their fields. Parts the view leaves out
  (sources, provider-run tool results, approvals, extensions) stay after the
  part they followed, or end the message when that part is gone. A `file`
  must be one of the message's own.
- **`user_before_input`** gets `content`, the input as user content, as the
  [input-lane events](../reference/events.md) store it: a nonempty list of
  `text` parts and `file` parts, whose `artifact` is metadata only. With `{"action":"transform","content":[…]}`, a
  plugin may change, reorder, or remove parts, but every file part must be
  one of the input's, unchanged, and the result must not be blank.

Parts are open: a part of a type the SDK does not know reads as
`Open::Unknown`, and the daemon refuses a replacement that holds one. A
replacement the daemon refuses is skipped with an `invalid_modification`
diagnostic, as before.

`agent_before_start` gets `prompt_context`:

| Member | Meaning |
|---|---|
| `input` | The user content that starts the run, after `user_before_input`; `null` when a producer's message starts it. |
| `system_prompt` | The agent's composed system prompt: its own prompt with the runtimes', tools', and skills' sections, and what earlier plugins changed. |
| `injected_messages` | The messages earlier plugins injected (`{ role, content }`), in order. |

Each plugin sees what the plugins before it made of the prompt.

`tool_before_call` and `tool_after_result` carry the call's `origin`:
`{"kind":"base"}` for a daemon base tool, or
`{"kind":"runtime","attachment":"local","runtime_id":"local"}` for a
runtime's or plugin's tool. The SDK reads it as `Open<ExtensionToolOrigin>`,
so an origin of a kind it does not know reaches the hook as `Unknown`. `permission_name` is `null` for a tool that
needs no permission. A hook cannot change a call's permission or resource,
never sees calls policy denied, and never grants permission.

Internal agents (title, approval, compaction) are never intercepted.
`user_before_input` runs for root sessions only; the other agent hooks also
run for delegated sessions.

## Events and the bus

`capabilities.subscribe_events` lists the event types a plugin receives, as
patterns: a type such as `run_finished`, a prefix ending in `*` such as
`tool_call_*`, or `*` alone for every type. An empty list subscribes to
nothing; the daemon skips a pattern it cannot read, with a note in the
plugin's status. For each event a pattern selects, the plugin receives a
`plugin/event` notification: `{ session_id, context_id, seq, event,
timestamp }`, where `event` is the event's payload under the
[event contract](../reference/events.md), which the SDK reads as
`Open<EventPayload>`: an event type it does not know arrives as `Unknown`.
Durable events arrive after they are appended, in sequence order per
session. Live-only events (`text_delta`, `reasoning_delta`,
`internal_agent_text_delta`, `tool_call_progress`,
`native_compaction_started`, and `native_compaction_finished`) are selected
by their types like any other, so `*` includes them and `run_*` does not;
they arrive as they happen, are never stored, and carry the `seq` of the
durable event they follow. There is no replay. The SDK's `on_event`
subscribes to `*` unless `event_patterns([...])` narrows it.

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
`rejected`. The SDK's `emit_bus` waits for it at most the plugin's
`interception_timeout_ms` and then resolves as `dropped`.

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

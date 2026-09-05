# Plugin Development

Plugins are executable processes that extend cookie agent without loading code
into the engine process. They communicate with the engine over newline-delimited
JSON-RPC 2.0 on standard input and standard output.

This page documents authoring with the Rust SDK and the extension protocol. To
install, enable, or configure an existing plugin, see the
[user plugin guide](../guide/plugins.md).

## Rust SDK

Rust plugins can use the official `cookie_agent_plugin_sdk` workspace crate. Until the SDK is
published separately, add it as a path dependency together with `serde_json` and Tokio:

```toml
[dependencies]
cookie_agent_plugin_sdk = { path = "../cookie-agent/crates/plugin_sdk" }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt"] }
```

Register handlers with `PluginServer`; the SDK validates tool parameter schemas and handles the
initialize, ping, shutdown, framing, and JSON-RPC messages:

```rust
use cookie_agent_plugin_sdk::{PluginServer, ToolDecl, ToolOutput};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), cookie_agent_plugin_sdk::PluginError> {
    PluginServer::builder("echo", "0.1.0")
        .tool(
            ToolDecl {
                name: "echo".into(),
                description: "Echo text back to the model".into(),
                parameters: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
                permission_name: "echo".into(),
                primary_resource_param: None,
            },
            |_ctx, request| async move {
                let text = request.arguments["text"].as_str().unwrap_or_default();
                Ok(ToolOutput::success(text))
            },
        )
        .run_stdio()
        .await
}
```

Event and bus handlers are registered with `on_event` and `on_bus_event`. A handler can publish
once from its automatically tracked context grant with
`ctx.emit_bus(event.session_id, "name", payload)`. Bus publishing is off by default
and is enabled explicitly with `enable_bus_publishing`; it is non-model traffic.
The SDK no longer exposes `enable_session_publishing` or `emit_session`. Model
messages use the explicit producer API below. Each bus handler context is scoped to its triggering token, so
concurrent callbacks for one session cannot consume each other's grants. Notification grants expire
four seconds after the SDK reader decodes the frame, so inbound queue delay consumes the local
window. The one-second safety margin assumes the engine-to-SDK delivery interval is under one
second; the engine remains authoritative, and a timing rejection is surfaced as
`ExtensionEmitStatus::Rejected`.

### Producer SDK

Opt in with `PluginServer::builder(...).enable_producers()`. From a tool, event,
or recovery callback, call `ctx.register_producer(session_id).await` to obtain a
`ProducerHandle`. Keep the handle for the lifetime of external work; it is not a
one-shot context grant and remains usable after the triggering callback or turn.

- `handle.send(message, ProducerDeliveryMode::{Steer, Queue}, key).await` returns
  the durable `ProducerMessageId` receipt.
- `handle.steer(message, key).await` and `handle.queue(message, key).await` are
  per-send conveniences. Construct a validated key with
  `ProducerIdempotencyKey::new(stable_message_key)`.
- `handle.id()` and `handle.session_id()` expose the runtime registration and
  destination. Do not persist registration IDs for reuse after reconnect.
- `handle.unregister().await` explicitly closes the registration. Zero-send
  unregistration is valid. Dropping a handle does not unregister it, and
  unregistration never removes already accepted messages. The method borrows the
  handle, so a rejected close can be retried; the engine rejects sends after close.
- `handle.discard(message_id).await` discards a waiting message in the handle's
  session and remains usable after `handle.unregister().await`. Discard does not
  unregister the producer, and dropping a handle never discards messages.
- `ctx.discard_producer_message(session_id, message_id).await` discards by durable
  receipt without a registration, including during recovery on a new connection
  with the same configured plugin name.

Use `.on_recovery(|ctx| async move { ... })` to restore from plugin-owned storage
or external services. The callback returns `RecoveryResult`: `Ok(())` reports
ready; `Err(RecoveryFailure)` reports failed. `PluginError` converts into
`RecoveryFailure`, so producer calls can use `?`. The SDK sends a separate
`plugin/recovery/complete` request after the callback returns, without a deadline.
The callback can await registrations and sends while restoring. Producer-capable
plugins without a recovery callback immediately report ready when recovery starts.

The SDK services responses independently of callbacks. It bounds ordinary
callbacks at 64 concurrent handlers, producer requests at 128 pending replies,
the inbound queue at 32 frames, and the outbound queue at 128 frames. Recovery
has its own single callback slot. Callback saturation rejects requests explicitly
and drops observational notifications with a stderr diagnostic; it never drops a
producer ACK. Disconnect fails pending producer calls with `TransportClosed`.
Missing send ACKs remain commit-uncertain: register again after reconnect and
retry the same body, mode, and stable key.

The workspace example `crates/plugin_sdk/examples/producer_plugin.rs` compiles
with `cargo check --locked -p cookie_agent_plugin_sdk --example producer_plugin`.
It reads an optional `COOKIE_SESSION_ID` from its configured environment and shows
registration, a stable-key steer, explicit unregister, and recovery completion.
With `COOKIE_DISCARD_MESSAGE_ID` also set to a saved message receipt, it instead
discards that owned waiting message during recovery without registering a producer.
Consumed or currently claimed receipts fail recovery visibly rather than reporting a
successful discard.

Interception handlers use builder methods named after every hook, including `user_before_input`,
`model_before_request`, `provider_before_headers`, `provider_before_request`,
`provider_after_response`, `message_end`, `model_before_select`, `session_before_fork`, and
`session_before_revert`. Helpers cover allow/block, replacement, prompt append/replacement,
message injection, compaction cancellation, and instruction override. Tool, subscription, explicitly enabled publishing,
and interception capabilities are derived by the SDK; plugin authors do not construct capability
flags or set the extension protocol version. See [Protocol](#protocol) for delivery, grant, quota,
and hook-chaining details.

## Protocol

The extension protocol version is the semantic-version string `0.0.5`. Before version 1.0,
cookie agent requires an exact version match: additive method or schema changes bump the patch
version, and plugins must update before connecting to the new engine. A plugin reporting any
other value is refused and its status contains the reported mismatch.

The host advertises `producer_messaging: true` on a connection only when its
configuration opts in and the runtime producer handler is installed. The session
protocol remains 16, and the crate/package version is unchanged. Event history
remains additive and versionless.

The engine sends `plugin/initialize` with the protocol version, engine version, and engine
capabilities. The plugin returns its exact protocol version, configured plugin name, plugin
version, capabilities, and tool declarations. Tool names must be unique within the plugin and
use `snake_case`. The reported plugin name must exactly match the configuration entry.

During `plugin/initialize`, this executable must declare every capability it uses. A tool-only
plugin sets `tools: true`, leaves subscription and publishing flags false, and returns an empty
`intercept` array plus its tool declarations. The agent document must then enable each declared
permission and resource, for example:

```yaml
permissions:
  plugin:
    "issue_read *": allow
    "issue_delete *": ask
```

The protocol also supports `plugin/ping` for liveness and sends the `plugin/shutdown`
notification during engine shutdown.

The initialize result declares `subscribe_events`, `subscribe_bus`, `publish_bus`,
`publish_session_events`, `producer_messaging`, and an `intercept` hook-name array in addition to tool and resource
capabilities. Capabilities are fixed for the process lifetime.

## Producer messaging contracts

Producer messaging requires explicit plugin capability opt-in and
`plugins.<name>.producer_messaging = true` in configuration, both off by default.
The engine derives the stable owner from the configured plugin name and checks
the live connection's ownership; senders cannot choose or impersonate an owner.

| Wire method | Direction | Parameters | Result |
|---|---|---|---|
| `plugin/producer/register` | Plugin to engine | `ExtensionProducerRegisterParams { session_id }` | `ExtensionProducerRegisterResult { producer_id }` |
| `plugin/producer/send` | Plugin to engine | `ExtensionProducerSendParams { session_id, producer_id, mode, idempotency_key, body }` | `ExtensionProducerSendResult { message_id }` |
| `plugin/producer/unregister` | Plugin to engine | `ExtensionProducerUnregisterParams { session_id, producer_id }` | `ExtensionProducerUnregisterResult {}` |
| `plugin/producer/discard` | Plugin to engine | `ExtensionProducerDiscardParams { session_id, message_id }` | `ExtensionProducerDiscardResult {}` |
| `plugin/recovery/start` | Engine to plugin notification | `ExtensionRecoveryStartParams {}` | None; no request ID or response |
| `plugin/recovery/complete` | Plugin to engine | `ExtensionRecoveryCompleteParams { outcome }` | `ExtensionRecoveryCompleteResult {}` |

Producer operations and recovery completion are JSON-RPC requests with strict
parameters/results. Recovery start is a notification created by
`extension_recovery_start_notification()` using `PLUGIN_RECOVERY_START_METHOD`.
It never enters the engine's deadlineful `PendingRequest` map. `outcome` is
`{ "status": "ready" }` or `{ "status": "failed", "message": "..." }`.
The SDK starts restoration asynchronously without blocking its reader; completion
is a separate explicit request on that connection after restoration from
plugin-owned storage/services. There is no recovery deadline or start ACK.
Registration during restoration must not wait for goal readiness or eager session
adoption. There are no timers, replay methods, or engine-owned recovery blobs.

`ProducerId` is a fresh runtime UUID, never a durable identity. Each send chooses
`ProducerDeliveryMode` (`steer` or `queue`). Its sender-chosen
`ProducerIdempotencyKey` is 1-256 control-free bytes. A live, owned registration is
required even for retries; closed/foreign registrations and closing sessions reject
sends. Zero-send unregister is valid and unregister never removes accepted messages.

Discard addresses a message receipt, not a registration. It requires a live,
producer-capable connection, but the message belongs to the stable plugin owner
in that session. The original registration may already be closed, and the current
connection epoch need not match the epoch that accepted the message. Runtime
rejects foreign-owner or wrong-session receipts and consumed messages. A durable
claim by the session actor reserves a message and removes it from waiting before
request preparation and hooks. Discard rejects while that claim is held, even
before any network request is sent; the claim is not proof that a provider received
or executed the request. If preparation fails or the attempt is cancelled, releasing
the claim may return an unconsumed message to waiting, where discard is available
again. Consumed messages do not return to waiting. Repeating a discard of an
already-discarded owned message succeeds. Discard never cancels a model request
or undoes model execution or external effects.

The ACK follows durable acceptance only. Identical retries scoped to
`(session, stable producer owner, idempotency_key)` return the original message ID;
different body or mode rejects key reuse. Missing ACKs are commit-uncertain. These
contracts make no exactly-once claim about model calls or external effects.

`PluginRecoveryStatus` has exactly `starting`, `ready`, `failed`, `disabled`.
Readiness inspection is runtime-only through `session.producers`. Failed/disabled
plugins require `plugin_diagnostic` (`recovery_failed`/`recovery_disabled`) and a
TUI notice; unrecovered external work remains unknown, not completed. Accepted
messages recover independently of registration or plugin readiness. The engine
cannot distinguish no pending work from lost plugin state.

Model-bound messages require explicit producer sends. `plugin/emit` is for
non-model publication; the runtime rejects the legacy durable model-history route.
No implicit registration authorizes an emission. Pure bus publication is unchanged.

### Transport/runtime worker boundary

The plugin transport exposes the following crate-private Rust callback boundary in
`engine::plugin`, following the existing `PluginEmitHandler` pattern. These are
runtime handoff signatures, not additional protocol wire types. The core runtime
installs the callback before starting plugins.

```rust
use std::{future::Future, pin::Pin, sync::Arc};
use cookie_agent_protocol::*;

pub(crate) struct PluginConnectionAuthority {
    pub plugin: String,
    pub connection_epoch: u64,
}

pub(crate) enum PluginProducerRequest {
    Register(ExtensionProducerRegisterParams),
    Send(ExtensionProducerSendParams),
    Unregister(ExtensionProducerUnregisterParams),
    Discard(ExtensionProducerDiscardParams),
    RecoveryComplete(ExtensionRecoveryCompleteParams),
}

pub(crate) enum PluginProducerResponse {
    Register(ExtensionProducerRegisterResult),
    Send(ExtensionProducerSendResult),
    Unregister(ExtensionProducerUnregisterResult),
    Discard(ExtensionProducerDiscardResult),
    RecoveryComplete(ExtensionRecoveryCompleteResult),
}

pub(crate) type PluginProducerHandler = Arc<
    dyn Fn(PluginConnectionAuthority, PluginProducerRequest)
        -> Pin<Box<dyn Future<Output = Result<PluginProducerResponse, JsonRpcError>> + Send>>
        + Send + Sync,
>;
```

The host derives `plugin` from the authenticated configured name and assigns a
fresh, non-reused epoch per connection within its lifetime. Neither value is
accepted from request parameters. Bind registrations and recovery readiness to
name + epoch; reject stale-connection sends/completions. A disconnect must revoke
only that epoch's registrations, not a replacement connection's registrations.
Durable deduplication uses `ProducerOwner::Plugin { plugin }` with session and
idempotency key, never the connection epoch. Mode remains per send.
For `Discard`, runtime checks the current authenticated epoch and the message's
stable owner and session, not an active producer registration or its original epoch.

Decode each method into its exact protocol parameters, invoke the handler without
blocking the transport reader, and correlate the matching typed response with the
original JSON-RPC ID. Serialize only the response's inner protocol result, never
the private enum. The runtime returns `Send` success only after durable acceptance.
Recovery start uses the control notification path, not the lossy event stream or
`PendingRequest`; restoration is not wrapped in a timeout. Registration during
restoration must remain serviceable. Do not implement protocol aliases or implicit
producer registration to authorize `emit`.

Runtime integration helpers in `PluginRegistry` (transport implementation):

- `set_producer_handler(PluginProducerHandler)` installs the exact callback above;
  absent handlers reject requests, including recovery completion.
- `producer_recovery_states() -> Vec<PluginRecoveryState>` returns configured or
  declared producer plugins, including disabled/failed entries.
- `producer_connection_is_current(plugin: &str, epoch: &u64) -> bool` checks live
  capability-authorized connection ownership. Check it inside actor operations,
  including register, send, unregister, discard, and recovery completion.
- `complete_producer_recovery(&PluginConnectionAuthority,
  &ExtensionRecoveryOutcome) -> Result<(), JsonRpcError>` is called by the runtime
  `RecoveryComplete` handler before actor reconciliation and its typed ACK. Exact
  repeated outcomes are idempotent; stale epochs and changed outcomes reject.
- `subscribe_producer_changes() -> tokio::sync::watch::Receiver<u64>` provides
  coalescing wakeups on handshake, completion, and disconnect, including supervisor
  cancellation. Subscribe before starting plugins and reconcile the initial snapshot
  as well as every change. Runtime must inspect current recovery states, revoke
  registrations whose owner name/epoch is no longer current, reconcile loaded
  sessions, and emit `recovery_failed`/`recovery_disabled` diagnostics. This is a
  state watch, not an event log; no engine event enum or callback variants change.

Producer handlers run in at most 32 concurrent tasks per connection. Overload
returns an explicit JSON-RPC error. Accepted handler tasks await the bounded control
queue for `Control::ReplyFrame`; ACKs are never silently dropped. The single host
loop remains the stdin writer. Recovery start is written once immediately after
the successful handshake on this same reliable writer, outside `PendingRequest`.

## Event streaming

A plugin with `subscribe_events: true` receives `plugin/event` notifications containing
`session_id`, physical `seq`, the raw `EventPayload` JSON as `event`, and `timestamp`. Delivery
starts after initialization and has no replay. Events are sent only after their session append is
durable; a buffered session's newly persisted prefix is sent in sequence after atomic publication.
Per-session sequence order is preserved, while cross-session ordering is unspecified.

Durable `tool_call_progress` events include optional sanitized bash output chunks,
so event-subscribed plugins observe live tool output through this same stream.
Model text and reasoning deltas are still not exposed to plugins. Chunk bursts do
not consume plugin publication quotas, which apply in the plugin-to-engine
direction. They use one 1024-entry FIFO delivery queue. When full, admission
evicts the oldest lowest-priority record: chunk first, then ordinary non-chunk,
and terminal only when the queue contains nothing else. Accepted records always
append at the tail, so retained events preserve per-session sequence order while
tool termination survives chunk and ordinary floods. Drops are counted separately
by chunk, ordinary, or terminal class in their diagnostic message.

This stream is observational and not durable. Each plugin has an independent bounded 1024-message
queue. A full queue drops delivery for that plugin, increments its dropped-event status counter,
and records a session diagnostic. It cannot delay session persistence, another plugin, or the
engine. There is no event-type filter in protocol 0.0.5; replay and filtered subscriptions remain
future work.

Plugins with `subscribe_bus: true` also receive non-durable `plugin/bus_event` notifications.
These real-time notifications have the source plugin, session, name, and arbitrary JSON payload,
and carry no cursor or delivery guarantee.

## Event publishing

`plugin/emit` publishes `{ session_id, context_id, name, payload }`. The session and opaque context
token must exactly match a recent engine notification or request delivered to that plugin. This
keeps interleaved session A and B callbacks correlated without mutable ambient session state;
mismatches are rejected and diagnosed. Context tokens are short-lived one-shot grants: the first
emit atomically consumes the token, replay is rejected, request tokens are revoked when the
request ends, and notification tokens expire five seconds after wire delivery. Queued controls
carry a lifetime duration; grants are activated immediately around successful host wire delivery,
so queue delay does not consume their lifetime. Cancellation before dequeue records the token as
spent and prevents later activation. Unknown tokens never use the plugin-supplied
session as a diagnostic target; only a known triggering session may receive a durable diagnostic.
With `publish_bus`, the
engine emits an `EngineEvent::PluginEvent`, forwards it to other subscribed plugins, and sends an
`events.plugin` notification only to RPC connections subscribed to that session.
The legacy `publish_session_events` model-history route is rejected; use
`plugin/producer/send` with a live registration for model-bound messages. Existing
`plugin_event_added` history retains its replay semantics, but new `plugin/emit`
calls do not authorize model messages.

Payload JSON is limited to 256 KiB, names to 128 control-free characters, and the complete
serialized event to 272 KiB. Each plugin/session pair may publish 40 events per second and 4 MiB
per minute; another plugin or session has independent quotas. `plugin/emit_result` reports separate
`bus` and `durable`
statuses as `published`, `dropped`, or `rejected`, with a reason when applicable. Oversized bus
or rate-limited bus payloads are dropped and durable payloads are rejected. Repeated violations
and stream drops increment a coalescing counter keyed by session, plugin, kind, and message. The
message key is control-normalized and truncated to 200 characters. At most 256 detailed keys are
retained per batch; additional distinct messages coalesce into an exact `(overflow)` counter per
session/plugin/kind. The map is swapped and flushed every 100 ms off the session append path;
increments cannot be lost to queue saturation. Diagnostic appends have a 250 ms timeout and engine
shutdown allows at most five seconds for draining before aborting the flusher and marking affected
plugin statuses as incomplete. A plugin
never receives its own
published event through either subscription, preventing accidental feedback loops; other plugins
receive it normally.

## Interception

Plugins register hook names in `capabilities.intercept`. The complete 0.0.5 set is
`tool_before_call`, `tool_after_result`, `agent_before_start`, `session_before_compact`,
`user_before_input`, `model_before_request`, `provider_before_headers`,
`provider_before_request`, `provider_after_response`, `message_end`, `model_before_select`,
`session_before_fork`, and `session_before_revert`. An unlisted hook is never delivered. Hooks run synchronously in configured
registration order. User-file order is retained; workspace entries replace same-name user entries
in place, while new workspace plugins append in workspace order. Each hook has
`interception_timeout_ms` (2000 by default); timeout, process
exit, or a full plugin queue fails open, records a diagnostic, and allows remaining hooks to run.

`plugin/intercept/tool_before_call` runs only after the original operation passes policy and any
user/model approval. It may allow, block, or return modified arguments. Each modification is
schema-validated and re-prepared through the pinned provider before later hooks see it; its
permission capabilities, resources, and labels must remain identical to the approved operation.
A block short-circuits later hooks and returns a tool error to the
model, so the turn continues without executing the tool. Modified arguments are validated against
the pinned `ToolSpec` JSON Schema. Hooks cannot alter `permission_name` or `resource`, denied calls
are never disclosed to plugins, and an allow hook never grants permission.

`plugin/intercept/tool_after_result` observes the tool result and may replace its content before
termination is committed. Streamed tool chunks are display previews and are
superseded by the committed terminal content, including plugin replacements.
`agent_before_start` may append to or replace the system prompt and may
inject a role-preserving text message. Prompt replacements and appends compose in plugin order.
Accepted injections are committed as `message_injected` during run setup, before the submitted
input, so restart, replay, fork, and versionless projection see the same message.

`user_before_input` runs for initial root-session input and active-run steering before
the corresponding input commit, including `cookie run`. It may allow, transform to non-empty text,
or handle the input without starting or steering an agent run. A real final transform commits the
transformed text and writes an adjacent `user_input_transformed` audit event containing both
original and transformed values. A no-op transform, including a chain that returns to the original
text, writes no audit event. Delegated sessions and subagent steering do not receive this hook.

`model_before_request` runs for every user-facing root or delegated agent model attempt after
history assembly and before provider conversion. Internal title, approval, and compaction agents
do not receive it. It receives role-tagged messages whose `content` is Oven's complete
serialized role-specific content, the resolved model, attempt ID, and inference parameters. A full
replacement becomes the request sent to the model and the request used for prompt-size accounting.
Replacement history and inference controls must pass Oven's structural and model-capability
validation before later plugins see them. Parameter adjustments apply with either `keep` or
`replace`; `replace` requires a complete message list. Prompt-cache markers are placed only after
the final validated model-hook result.

Pinned Oven adapters in 0.0.5 do not expose their adapter-assembled HTTP headers or raw provider
JSON. Accordingly, `provider_before_headers` receives an empty map; requesting a non-empty `set` or
`delete` mutation records an `unsupported_capability` diagnostic and does not mutate the HTTP
request. `provider_before_request` operates on Oven's normalized request JSON, not the adapter's raw
wire body. Replacement validation guarantees a JSON object, successful normalized-request
deserialization, and Oven's model invariants. `provider_after_response` is observe-only, runs after
the response head and before stream consumption, and exposes HTTP status with an empty header map.
It never receives body data.

`message_end` receives the complete assembled assistant content after streaming and before
`model_turn_committed`. It may replace content but not the assistant role. The replacement is
validated as a complete persisted turn and becomes the durable turn; already emitted text and
reasoning deltas remain historical stream records. TUI and replay projections
replace accumulated partials with that committed content.

`model_before_select`, `session_before_fork`, and `session_before_revert` may block their operation
with a user-facing reason. Model selection interception occurs when the configured selection is
first used to start a run, when a later run changes selection, and before a fallback transition. It
does not run for unchanged selections, draft UI changes before run start, skill-scoped model
overrides, or internal-agent model choices. A blocked model selection retains the current selection.
Revert may also chain an instruction override. `session_before_compact` may append instructions,
replace the current instructions, or cancel compaction with a reason; cancellation leaves the engine
and session usable and the RPC returns that reason.

All mutation chains pass only validated current state to the next plugin. Invalid modifications are
diagnosed and skipped. Timeout, crash, malformed response, or queue failure is fail-open per plugin,
and later plugins still run. Hooks receive the same short-lived context grant used by plugin emit.
Except for root-only `user_before_input`, user-facing agent hooks also run for delegated sessions.
Internal title, approval, and compaction agents remain outside extension interception.

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
present. The [user plugin guide](../guide/plugins.md#allow-plugin-tools) shows the corresponding
agent permission configuration.

[Skill `allowed-tools`](../guide/skills.md#invocation-and-permissions) entries `Plugin` and
`Plugin(name:*)` govern plugin tools in the same way as the existing MCP group. A pinned call to a
connected but disallowed plugin tool reports that the tool is not enabled instead of treating it
as undiscovered.

## Lifecycle

Plugin state progresses through `disconnected`, `connecting`, and `connected`, or to `failed`
with a diagnostic reason. Spawn failures, handshake timeouts, malformed responses, version or
name mismatches, declaration errors, name collisions, unexpected EOF, and process exits affect
only that plugin; the engine and other plugins continue running.

Discovered tool names share one namespace with built-in and MCP tools. Built-in names are reserved,
and MCP claims take precedence over plugin claims; a plugin collision fails, and a later lazy MCP
claim preempts the plugin. When plugins collide with each other, the last plugin to finish
registration wins that tool; the earlier plugin remains connected for its other tools and publishes
a status diagnostic. The winner's permission declaration applies. Claims and listings are removed
immediately when the owning process exits or standard output closes, and prepared calls fail
revalidation after removal. Crashed plugins stay failed until engine restart.

Each call is bounded by `tool_timeout_ms`, which defaults to 30000. During shutdown, the engine
sends `plugin/shutdown`, closes plugin standard input, waits for `shutdown_grace_ms`, and then
terminates the process if needed. Shutdown remains bounded when initialization is still pending.

## Current limitation

Plugin resource methods remain deferred. Protocol 0.0.5 event subscriptions have no replay or
per-event-type filters.

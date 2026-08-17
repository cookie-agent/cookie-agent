# Plugins

Plugins are executable processes that extend cookie agent without loading code into the engine
process. Each enabled plugin starts eagerly when `Engine::open` runs and communicates over
newline-delimited JSON-RPC 2.0 on standard input and standard output. Plugin logs go to the
engine's standard error stream.

## Configuring a plugin

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
`ctx.emit_bus(event.session_id, "name", payload)` or
`ctx.emit_session(event.session_id, "name", payload)`. Bus and durable session publishing are off
by default and are enabled explicitly with `enable_bus_publishing` and
`enable_session_publishing`, respectively. These set process-lifetime protocol capabilities. When
both are enabled, every emitted event is offered to both routes; `emit_bus` and `emit_session`
return the selected route's status. Each handler context is scoped to its triggering token, so
concurrent callbacks for one session cannot consume each other's grants. Notification grants expire
four seconds after the SDK reader decodes the frame, so inbound queue delay consumes the local
window. The one-second safety margin assumes the engine-to-SDK delivery interval is under one
second; the engine remains authoritative, and a timing rejection is surfaced as
`ExtensionEmitStatus::Rejected`.

Interception handlers use builder methods named after every hook, including `user_before_input`,
`model_before_request`, `provider_before_headers`, `provider_before_request`,
`provider_after_response`, `message_end`, `model_before_select`, `session_before_fork`, and
`session_before_revert`. Helpers cover allow/block, replacement, prompt append/replacement,
message injection, compaction cancellation, and instruction override. Tool, subscription, explicitly enabled publishing,
and interception capabilities are derived by the SDK; plugin authors do not construct capability
flags or set the extension protocol version. See [Protocol](#protocol) for delivery, grant, quota,
and hook-chaining details.

## Protocol

The extension protocol version is the semantic-version string `0.0.4`. Before version 1.0,
cookie agent requires an exact version match: additive method or schema changes bump the patch
version, and plugins must update before connecting to the new engine. A plugin reporting any
other value is refused and its status contains the reported mismatch.

The engine sends `plugin/initialize` with the protocol version, engine version, and engine
capabilities. The plugin returns its exact protocol version, configured plugin name, plugin
version, capabilities, and tool declarations. Tool names must be unique within the plugin and
use `snake_case`. The reported plugin name must exactly match the configuration entry.

The protocol also supports `plugin/ping` for liveness and sends the `plugin/shutdown`
notification during engine shutdown.

The initialize result declares `subscribe_events`, `subscribe_bus`, `publish_bus`,
`publish_session_events`, and an `intercept` hook-name array in addition to tool and resource
capabilities. Capabilities are fixed for the process lifetime.

## Event streaming

A plugin with `subscribe_events: true` receives `plugin/event` notifications containing
`session_id`, physical `seq`, the raw `EventPayload` JSON as `event`, and `timestamp`. Delivery
starts after initialization and has no replay. Events are sent only after their session append is
durable; a buffered session's newly persisted prefix is sent in sequence after atomic publication.
Per-session sequence order is preserved, while cross-session ordering is unspecified.

This stream is observational and not durable. Each plugin has an independent bounded 1024-message
queue. A full queue drops delivery for that plugin, increments its dropped-event status counter,
and records a session diagnostic. It cannot delay session persistence, another plugin, or the
engine. There is no event-type filter in protocol 0.0.4; replay and filtered subscriptions remain
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
`events.plugin` notification only to RPC connections subscribed to that session. With
`publish_session_events`, it also appends
`plugin_event_added` to the session log. The durable event is ordinary branch data: it survives
reopen, enters model history and compaction, and is removed from the visible branch by revert in
the same way as other events.

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

Plugins register hook names in `capabilities.intercept`. The complete 0.0.4 set is
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
termination is committed. `agent_before_start` may append to or replace the system prompt and may
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

Pinned Oven adapters in 0.0.4 do not expose their adapter-assembled HTTP headers or raw provider
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
reasoning deltas remain historical stream records.

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
Internal title, approval, and compaction agents remain outside extension interception. Streaming
output chunks are not part of this protocol stage.

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
present. The `issue_read *` rule above allows that declared permission for any primary resource.

[Skill `allowed-tools`](skills.md#invocation-and-permissions) entries `Plugin` and
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

Plugin resource methods remain deferred. Protocol 0.0.4 event subscriptions have no replay or
per-event-type filters.

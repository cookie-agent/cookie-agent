# Protocol Reference

The daemon exposes JSON-RPC 2.0 over a WebSocket at `/ws`. This build speaks
client protocol `1.34.0`; the [changelog](protocol-changelog.md) lists what
each version changed. A client and a daemon connect when their majors are
equal: a minor only adds, so the two use what the lower minor includes, and
the patch never matters. [Compatibility](compatibility.md) states the rules
every change to the protocol follows.

## Handshake and authentication

A client must call `handshake` before any other method:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "handshake", "params": {
    "protocol_version": "1.54.0",
    "client": { "kind": "web", "instance_id": "<uuid>" },
    "token": "<the daemon's token>",
    "features": [] } }
```

The reply:

```json
{ "protocol_version": "1.54.0", "engine_version": "0.3.0",
  "origin": "client:web-<id>", "machine_id": "<opaque>", "features": [] }
```

- **`protocol_version`** is the sender's semver version, a
  `"MAJOR.MINOR.PATCH"` string. The daemon reads it from the raw params before
  anything else: a client of another major (a `0.x` build from before the
  freeze included), or a build from before semver versions that sends an
  integer, is answered
  with `-32002` `unsupported_protocol`, whose data names both
  `supported_version` and `peer_version`. A client only sends a method or a
  member the daemon's version includes.
- **`client.kind`** names the client program: a lowercase letter followed by
  up to 15 lowercase letters, digits, or hyphens (the shipped clients use
  `tui`, `cli`, and `web`). **`client.instance_id`** is a UUID identifying one running
  client; a client keeps it across reconnects so it can recognize events it
  caused. Any number of clients may connect to one daemon at once.
- **`origin`** is `client:<kind>-<id>`, where `<id>` is the instance id as 32
  lowercase hex digits. The daemon stamps that origin on every event the
  connection's requests cause, so each client can tell its own actions from
  another client's. A repeated handshake on the same connection keeps the
  first identity.
- **`engine_version`** is the daemon's own version, for display and logs;
  **`machine_id`** an opaque identity of the daemon's machine when it has one,
  so a client can tell whether it runs on the same machine.
- **`features`** list what each side's configuration enables. None are
  defined yet; an unknown feature is ignored.

**Authentication.** A client authenticates with the daemon's bearer token
(`[server] token`) in one of two places:

- the `Authorization: Bearer <token>` header of the WebSocket upgrade, which
  the native clients send; a wrong header is refused with HTTP 401;
- or `token` in the handshake, which a browser must use, since it cannot set
  headers on a WebSocket.

Until a connection without the header has sent the right token, the daemon
answers only `handshake`; any other request gets `-32003` `unauthenticated`.
A handshake with a wrong or missing token gets `-32003`, and the connection
closes. A connection that has not authenticated within 10 seconds is closed.
A connection that authenticated with the header but has not handshaken gets
`-32001` `handshake_required` for anything but `handshake`.

**Web origins.** `/ws` accepts an upgrade request from any web origin, or
with no `Origin` header (every native client); the token is the guard.
Dial-in runtimes on `/runtime` always authenticate with the header, and a
request there with an `Origin` header is refused with HTTP 403. Refusals there name their
reason in the response body or the close frame (see
[Dial-in refusals](runtime-protocol.md#dial-in-refusals)).

## Decoding

The daemon and its clients read what they receive by its role (see the
[compatibility spec](../specs/compatibility-v1.md) section 4):

- **Results, notifications, and the JSON-RPC envelope are information.** A
  reader ignores members it does not know, and request IDs may be integers or
  strings. Every enum in them is open: a value a newer daemon added reads as
  the fallback its type documents (for example an unknown permission effect as
  `ask`, an unknown approval status as `cancelled`), or as `unknown` for
  states a client only shows (session status, plugin state, MCP server state,
  runtime instance status, producer source status, runtime change reasons).
  An unknown `sessions.changed` change reads as `resync`, an unknown
  `events.subscription` message type is ignored, and an unknown
  `session.submit_input` or `session.recall_input` outcome fails that call
  with a readable error.
- **A notification a client cannot read is dropped.** The shared client
  reports it as `ClientDelivery::Diagnostic`, and no call in flight fails. A
  response it cannot read fails only its own call. A frame that is not JSON,
  or not a request, notification, or reply, ends the shared client's
  connection; the daemon instead answers such a frame with `parse_error` or
  `invalid_request` and reads on.
- **Events.** A client reads stored events with an open payload: an event of
  a type this build does not know, or one it cannot read, is delivered in its
  place as `ClientDelivery::UnknownEvent`, which advances the session's
  sequence and changes nothing else (the TUI shows a dim row in its debug
  view). A replay page does not fail for it.
- **Display blocks.** The daemon sends each connection only the display-block
  kinds its client protocol version includes: a block of a newer kind arrives
  as a `text` block of its canonical text rendering, and a block of a kind the
  daemon itself does not know (from a log a newer build wrote) is left out of
  what it sends, while the log keeps it. Every kind of this build exists from
  the first minor, so today no block is downgraded.
- **Params are commands.** An unknown or mistyped member fails with `-32602`
  and `data.code` `unknown_field` (or `unknown_variant` for a value or tag),
  with `data.field` naming the JSON path. A client sends a method or member
  only if the daemon's version includes it; `-32601` for a method the client
  expected reads as "the daemon is too old for" that method.

## Error diagnostics

`ModelErrorSummary.response_body` is an optional `DiagnosticText` that preserves
the supplied content, with terminal/Unicode format controls removed or replaced
and a maximum of 4,096 UTF-8 bytes, including any truncation marker. Newlines and
tabs are permitted. It is used for terminal,
fallback, and internal-agent model failures. A failed `run_finished` keeps its short `error`
and optionally includes `model_error` and `resolved_model`. Missing fields in old
events default to null and are omitted when serialized; this is an additive
event change without a protocol version bump.

## Errors

Every application error is `-32000` with a registry `data.code`, a `message`
a user can understand, and optional `data.hint` and `data.detail`; the
[error reference](errors.md) lists every code with its fields. The daemon
builds `message` and `hint` from the registry, so a code always reads the
same way, and keeps its own description of the cause (bounded and
terminal-safe, without secret-pattern redaction) as `detail`. Clients branch
only on `data.code`, show `message` and then `hint`, and show `detail` on
request; a code a client does not know reads as a generic error, whose
`message` and `hint` are already readable. `JsonRpcError` Debug formatting
redacts data. No request headers or credential dumps are added. See
[Error details](../clients/tui.md#error-details) for display and upstream limits.

## Tool-emitted messages

`PersistedToolResult` carries optional `additional_messages`, alongside its
independent display and output references. The field is an ordered array of at most four messages. Each message has role
`system` or `user` and one or more ordered `text` or `file` content parts. Empty
arrays are omitted on the wire; event validation bounds text and attachment
counts and validates every referenced artifact.

The engine materializes these messages from the persisted terminal result, so
replay and restart are deterministic. The paired tool result is placed first,
then messages are inserted in array order before the next assistant turn.
User-role messages retain their role. System-role messages are represented as
user turns whose first text part is the deterministic marker
`[tool-emitted system message; materialized as user history]`; their original
text and file parts follow in order. This translation keeps the initial system
prefix and its cache breakpoint stable because provider families disagree on
mid-history system semantics. Terminal result events use origin
`engine:tool-result`.

Elision treats a tool result and its `additional_messages` as one unit. If the
parent result is elided, none of its emitted messages enter model history and
the marker reports how many were lost. Retried or replayed termination events
replace the call record rather than applying first-attempt messages twice.
Attachments in compacted history are not recoverable from the checkpoint.

## Session mechanics

The JSON-RPC session layer lives in the `protocol` crate so the server, the TUI,
and the CLI share one implementation of the protocol mechanics.

- **Transport.** `protocol::Transport` is a frame-level channel:
  `send(MessageFrame)` / `recv() -> Option<MessageFrame>`, with `MessageFrame`
  either a `Text` string or a `Value`. It carries no JSON-RPC semantics. The
  `protocol` crate provides the `InProcessStream` pair and, behind its
  `websocket` feature, the client `WebSocketTransport`; the daemon's axum
  accept path in the `server` crate implements the same trait server-side.
- **ServerProtocol.** The server contract is one async method per RPC plus
  `connected`. The `server` crate implements it over `Engine`. `protocol::serve`
  drives one complete session over a transport: it authenticates the
  connection (by the transport, or by the handshake's token), rejects every
  method before the handshake with `-32003` or `-32001`, correlates requests by
  id, dispatches to the implementation, and delivers notifications through a
  `ServerContext`. After the handshake, requests on one connection run
  concurrently (at most 64 in flight) and each response is written when it is
  ready, so responses can arrive out of request order. Notifications from work
  a request started, such as the live tail of `events.subscribe`, are written
  only after that request's response. A connection holds one live tail per
  session: subscribing again replaces it, and the replaced tail's queued
  notifications are written before the new tail's.
- **Client.** The client side is the concrete `protocol::Client`. Its connection task correlates requests by id, demuxes
  notifications into an ordered `ClientDelivery` stream, injects cursor replays
  and gap recovery before buffered live notifications, fetches replays in pages
  of 2,000 events and delivers each replay whole once its final page arrives,
  wipes secret-bearing
  serialized frames, and fails outstanding calls on shutdown.
- **TUI and CLI.** The TUI client is a thin adapter re-exporting the server's
  `Client` wrapper; the CLI uses the same client. Both still work without the
  `tui` feature.

## Methods

Params are commands: a member a method's params schema does not list is
refused with `-32602` and `data.code` `unknown_field`, whose `field` is the
member's JSON path (`selection.model.extra`). That holds inside types session
history also stores, such as a run selection or a permission rule, which
ignore unknown members when they are read as history.

`run.start` accepts optional `reset_fallback` (default `false`). The server
normally trims a requested earlier model's suffix to the latest successfully
committed fallback in this session, matching exact model/variant identity and
agent/preset scope. Set `reset_fallback: true` for an explicit picker/model reset.
The accepted effective selection and suffix are recorded in `run_started`;
existing committed-turn events preserve progress without a new event type.
Idempotent redelivery resolves against the state preceding the original run.

| Method | Parameters | Result summary |
|---|---|---|
| `handshake` | `protocol_version`, `client` identity, optional `token`, `features` | The daemon's `protocol_version` and `engine_version`, the connection's event `origin`, `machine_id` when the daemon has one, and `features` (see [Handshake and authentication](#handshake-and-authentication)) |
| `runtime.snapshot.get` | Empty object | The daemon's current [catalog view](#the-catalog-view) |
| `provider.connect` | Provider, the `expected_revision` of the catalog view, setup/auth values, client ID | Durable connection, effective auth, the new catalog view, replay state |
| `provider.disconnect` | Provider, the `expected_revision` of the catalog view, expected connection generation, client ID | Disconnect receipt (ID and time), effective auth, the new catalog view, replay state |
| `provider.auth.begin` | `provider_id`, one of its `sign_in_methods` as `method`, and `redirect_uri`: `http://127.0.0.1:<port><redirect_path>` the client catches or the user pastes back, or null for the daemon to catch it on its own machine (since 1.49.0; see [Provider sign-in](#provider-sign-in)) | `authorization_url` to open in a browser and the `redirect_uri` the sign-in uses |
| `provider.auth.complete` | `provider_id` and `callback_url`, the address the browser landed on (since 1.49.0) | The signed-in `account` (`email`, `sign_in_required`); the provider is connected and the catalog view republished |
| `provider.auth.cancel` | `provider_id` (since 1.49.0) | `provider_id`; the sign-in in progress and the daemon's listener for it end |
| `config.get` | Empty object | The documents on disk (`config.toml`, `agents/<id>.md`, `agents/<preset>/<id>.md`), each with its `text` and `fingerprint`; their `revision`; `empty_presets`; and `status` |
| `config.apply` | `expected_revision`, `changes`: `{"put": {path, text, expected_fingerprint?}}`, `{"delete": {path, expected_fingerprint}}`, `{"set": {key, value}}` (one key of `config.toml` by its path of names; `null` removes it) | The new `revision` and `status`. A refusal changes nothing and is the error `config_apply_refused`, whose `reason` is `conflict`, `invalid_configuration`, or `invalid_change` and whose `detail` says why |
| `config.reload` | Empty object | `status`: `applied_revision`, `restart_required` (`server`, `plugins`, `runtimes`), and `error` when the configuration on disk failed validation and was not applied, and `diagnostics` (since 1.18.0): what the applied configuration names that does not exist yet, such as a delegation target no agent has |
| `daemon.restart` | Optional `when`: `idle` (default) or `now` (since 1.19.0) | `scheduled` (whether this call scheduled the restart rather than changed one already scheduled), `active_runs`, and `background_calls` (since 1.35.0, omitted when zero); a daemon that cannot relaunch itself answers `restart_unsupported` (see [Restarting the daemon](#restarting-the-daemon)) |
| `onboarding.get` | Empty object (since 1.22.0) | Whether the daemon is set up: `ready`, `missing` (`provider`, `default_model`, `small_model`), `providers_connected`, and the configured `default_model` and `small_model` as `{ model, variant? }`. Derived from the live configuration and runtime; nothing is stored, so setting up by hand counts the same |
| `onboarding.preview` | Optional `source`: `{"type": "release"}` (the default: the daemon's version's release, else that version's newest prerelease with a kit, else the latest) or `{"type": "archive", "data": "<base64>"}` (since 1.22.0) | The starter kit's `kit_version`, `cookie_version`, where it came from (`origin`), and per file its `path` and `action` (`write`, `skip` as identical, `write_new` beside a changed file) with a `diff` for changed files. Writes nothing; a failed download or a kit that does not match its manifest or does not load is `onboarding_kit_unavailable` |
| `onboarding.run` | `install_kit`, optional `source`, optional `default_model` and `small_model` (a model key or `{ model, variant }`, checked as `config.apply` checks them) (since 1.22.0) | The kit files `written`, `skipped`, and `written_new` (the kit's version beside them as `<path>.new`), the configuration `settings` set, and the new `status`. Refused with `onboarding_incomplete`, naming what is `missing`, when the daemon would not be set up afterwards; nothing is written then |
| `session.create` | Run selection, optional `permission_mode`, optional `permission_rules` (the initial session overlay, at most 256 rules), optional `runtimes` (`[{ runtime_id, capability?, workspace? }]`), optional `global` (default `true`), optional `cwd` (the client's directory), optional `project` (since 1.8.0; see [Projects](#projects)), optional `agent_options` (since 1.26.0; see [ACP agents](#acp-agents)) | Session metadata with skipped-event diagnostics, its `attachments`, and its `project`, and the session's `runtimes` |
| `session.list` | Optional cwd identity, optional `project` (a project ID, or `"none"` for sessions in no project; since 1.8.0) | Root session metadata list with skipped-event diagnostics |
| `session.get` | Session ID | Session metadata with skipped-event diagnostics, and the session's `runtimes` |
| `session.goal.get` | Session ID | Required nullable `goal: GoalState` |
| `session.goal.set` | Session ID, objective, optional `selection: RunSelection` | Activated `goal: GoalState` |
| `session.goal.lifecycle` | Session ID, goal ID, expected revision, pause/resume/cancel action, optional `selection` for resume only | Updated `goal: GoalState` |
| `session.producers` | Session ID | Runtime `producers` and `sources` (producer source recovery) inspection |
| `session.usage` | Session ID | Token/request rollup, cache hit rate, optional estimated cost, and per-model breakdown |
| `session.tree_usage` | Session ID | The rollup of the whole tree the session belongs to and `session_count`; a session or delegation record of the tree that is gone or damaged is `tree_usage_session_missing` or `tree_usage_delegation_corrupt` |
| `session.children` | Session ID | Direct child summaries |
| `session.tree` | Session ID | Recursive session tree |
| `session.resume` | Session ID | Resumed session metadata with skipped-event diagnostics, and the session's `runtimes` |
| `session.rename` | Session ID, client rename ID, set/clear/reset change | Session metadata and client ID |
| `session.project.set` | Root session ID, required nullable `project` | The session's metadata; it moved into the project, or out of any with `null` (see [Projects](#projects)) |
| `session.agent_option.set` | `session_id`, `option_id`, `value` | The session's ACP agent options after the change; `session/set_config_option` (or `session/set_mode` for an agent that offers modes but no mode option) with the agent, resuming its ACP session when it is not live (see [ACP agents](#acp-agents)) |
| `session.permission.get` | Session ID | Effective permissions per action and the tree's current mode |
| `session.permission.set` | Session ID, action, optional `runtime` selector (refused on `message` and `skill`, as in agent documents), wildcard pattern, effect | Effective permissions after the overlay change |
| `session.permission.clear` | Session ID, action, optional `runtime` selector, wildcard pattern | Effective permissions after the overlay change |
| `session.permission.preview` | Run selection, optional overlay `rules` | Effective permissions a session created with them would have; creates nothing |
| `session.runtime.attach` | Session ID, `runtime_id`, optional `capability`, `workspace` when the capability takes one | The new `attachment` and the session's `runtimes` |
| `session.runtime.detach` | Session ID, attachment `name` | The session's `runtimes` |
| `runtimes.list` | Empty object | Every `runtimes` entry (built-in, configured, and dial-in, with lifetime, connection, capabilities, and machine) and the opened `instances` with status (see [Runtimes](#runtimes)) |
| `runtimes.reconnect` | `runtime_id`, optional `capability`, `workspace` when the capability takes one | The `instance` afterwards, as `runtimes.list` reports it |
| `runtimes.agents.list` | `runtime_id`; since 1.37.0 | `agents`: the runtime's `[runtimes.<id>.acp.agents]` declarations, or for `global` (since 1.43.0) the `[acp.agents]` ones every runtime offers (see [Runtime tables](#runtime-tables)) |
| `runtimes.agents.set` | `runtime_id`, `agent`, `definition` (`command`, `env`, `description`, `enabled`) | `agents` afterwards |
| `runtimes.agents.remove` | `runtime_id`, `agent` | `agents` afterwards |
| `runtimes.agents.rename` | `runtime_id`, `agent`, `new_agent` | `agents` afterwards, the `moved` documents, and the `stale_templates` that still name `agent` |
| `machines.list` | Empty object | Every `machines` entry, in name order (see [Machines](#machines)) |
| `machines.rename` | `machine`, new `name` | The `machines` afterwards; the machine's memory folder moves with it |
| `machines.assign` | `runtime` ID, `machine` | The `machines` afterwards; the runtime is pinned to the machine, created when the name is new |
| `machines.unassign` | `runtime` ID | The `machines` afterwards; the runtime's pin is gone |
| `machines.merge` | `from`, `into` | The `machines` afterwards; `from`'s IDs, pins, and memories moved to `into` |
| `machines.forget` | `machine` | The `machines` afterwards, without the machine |
| `machines.forget_runtime` | `runtime` ID | The `machines` afterwards; the daemon no longer remembers where the offline runtime ran |
| `memory.list` | Optional `layer`, and at most one of `agent`, `machine`, and `project` | The `memories`, valid or not, and the `ignored` entries of the memory folder (see [Memory](#memory)) |
| `memory.read` | Memory `url` | The file's whole `text`, its `revision`, and why it is `invalid` when it is |
| `memory.write` | Memory `url`, whole-file `text`, `revision` of the file it replaces | The new `revision`, and a `notice` when an `agent://` file kept the daemon's description or keywords; without `revision` the memory must not exist |
| `memory.delete` | Memory `url`, `revision` | Empty object |
| `projects.list` | Empty object | Every `projects` entry, oldest first (see [Projects](#projects)); an older daemon sends them in name order |
| `project.get` | `project` ID | The `project` and its `agents_md` (`{ text, revision }`) when it has one |
| `project.create` | `name`, optional `emoji`, optional `defaults` | The new `project` |
| `project.update` | `project` ID, `revision`, optional `name`, `emoji` (`null` clears), `defaults` (`null` clears), `agents_md` (`""` removes) | The `project` afterwards |
| `project.delete` | `project` ID, `revision` | `deleted_sessions` and `deleted_memories` |
| `crons.list` | Empty object | Every cron job with its state and history, in creation order, and whether the daemon is `firing` (see [Cron jobs](#cron-jobs)) |
| `crons.create` | `prompt`, `schedule`, `target`, optional `name`, `max_runs`, `delivery`, `permission_mode`, `max_consecutive_failures`, `enabled` | The new `job` |
| `crons.update` | Job `id`, any member of `crons.create` (`null` clears `name`, `permission_mode`, `max_consecutive_failures`) | The `job` afterwards |
| `crons.delete` | Job `id` | Empty object; the sessions its fires started stay |
| `crons.run` | Job `id` | The `job` afterwards and the `session_id` the fire went to, absent when it was skipped |
| `notifications.get` | Empty object | The daemon's VAPID `public_key`, whether it sends pushes (`enabled`), and the `subscriptions` it pushes to (since 1.30.0; see [Push notifications](#push-notifications)) |
| `notifications.subscribe` | A browser's push subscription: `endpoint`, `p256dh`, `auth`, and a `label` | The `subscription`; the same endpoint keeps its id |
| `notifications.unsubscribe` | Subscription `id` | Empty object |
| `notifications.test` | Subscription `id` | `status` (`sent`, `gone`, or `failed`) and an optional `detail` |
| `client.presence` | `session`: the session this connection shows the user, or `null` when the user is not there | Empty object |
| `session.unread.set` | `session_ids` (1 to 256 root sessions) and `unread` (since 1.51.0; see [Unread sessions](#unread-sessions)) | `changed`: the sessions whose state changed, each also announced by `sessions.changed`; `skipped`: the IDs it left, each with its `reason` (`session_not_found`, or `not_a_root_session` with its `root_session_id`) |
| `plugins.list` | Empty object | The configured `plugins`, in hook order |
| `plugins.restart` | Plugin `name` | Empty object, once the restart is under way |
| `session.set_permission_mode` | Any session ID in the target tree, `auto_approve`/`auto_approve_n`/`auto_approve_y`/`ask`/`yolo`; updates the runtime-only tree mode | Empty object |
| `skills.list` | Session ID; optional since 1.22.0, without it the daemon's own user and official skills | Discovered skills with source (`user`, `project`, or `official` since 1.22.0), precedence, visibility, permission effect, and an official skill's `version` |
| `skills.get` | Session ID, skill name, arguments | Permission-checked rendered preview and descriptor |
| `skills.update` | Empty object (since 1.22.0) | Refreshes the official skills now: `enabled` (`false` with `[skills] official = false`), the `skills` kept (`name`, `version`), the ones `updated` and `removed`, and the `error` when the refresh failed and the cached ones stayed |
| `session.compact` | Session ID, required nullable focus | Whether a checkpoint was committed |
| `session.compact.cancel` | Session ID | Whether a running manual compaction was cancelled |
| `session.held.resume` | Session ID | `resumed`: whether the session was [held](#held-sessions); its pending producer messages then run (since 1.34.0) |
| `session.revert` | Session ID, positive `through_seq`, `expected_last_seq` | Updated session metadata |
| `session.delete` | Root session ID, positive `expected_last_seq` | `deleted`: every session removed, the root first (see [Delete](#delete)) |
| `session.fork` | Session ID, positive `through_seq`, `client_fork_id` | New session ID |
| `run.start` | Session ID, client run ID, selection, [`content`](#user-content) | Run ID |
| `run.steer` | Run ID, [`content`](#user-content) | `accepted` boolean, admission `user_input_seq` |
| `session.select_model` | Session ID, selection | Whether the session's selection changed |
| `session.submit_input` | Session ID, [`content`](#user-content), `steer` or `queue` delivery, optional selection, client run ID, optional `reset_fallback` | `started` run ID, `steered` run ID and seq, `queued` seq, or `handled` reason |
| `session.recall_input` | Session ID, seq of a pending or queued input, any client's since 1.31.0 | `recalled` content, `gone`, or (before 1.31.0, for another client's input) `not_owner` |
| `artifact.upload.begin` | Session ID, `byte_length`, optional `filename` | `upload_id`, `chunk_bytes` (see [Media](#media)) |
| `artifact.upload.chunk` | `upload_id`, `offset`, base64 `data` of at most `chunk_bytes` bytes | Empty object |
| `artifact.upload.finish` | `upload_id`, `sha256` of every byte sent | The stored `artifact` (`ArtifactRef`), and `image` `{ width, height }` for an image |
| `artifact.upload.cancel` | `upload_id` | Empty object |
| `artifact.read` | Session ID, `sha256`, `offset`, `length` (at most 1 MiB) | Base64 `data`, `total_bytes`, `media_type` |
| `run.cancel` | Run ID | `cancelled` boolean |
| `run.tool_background` | Run ID, tool call ID | `accepted` boolean: false unless the call is an executing call of a background-capable tool; it detaches at its detach point (see [Background calls](tools.md#background-calls)) |
| `session.background.list` | Session ID | The session's background `calls` |
| `session.background.stop` | Session ID, `job` | Empty, once the call has ended; it still reports to the model, as stopped by the user. An unknown or finished job is an error |
| `events.subscribe` | Session ID, optional cursor and `cursor_timestamp`, optional `limit` | Stored events after the cursor, or `stale_cursor`; starts notifications on the final page (see [Subscriptions](events.md#subscriptions)) |
| `events.unsubscribe` | Session ID | Whether this connection had a live tail for it |
| `approval.list` | Root session ID, optional status | Approval records and tree grants |
| `approval.respond` | Approval identity/revision/fingerprint, client ID, decision, optional rejection feedback | Updated approval record |
| `elicitation.respond` | Session ID, `elicitation_id`, `action` (`accept`, `decline`, or `cancel`), and `content`, the form's values by field name, for `accept` of a form | Empty object, once the answer is handed to the agent; `elicitation_not_pending` or `elicitation_answer_invalid` otherwise (see [Questions](#questions)) |
| `mcp.server.list` | Empty object | Every MCP server the daemon and its runtimes host: its `runtime_id`, `workspace`, `name`, `source`, `definition`, `state`, `tool_count`, `server_tools` (since 1.36.0), and `message` (see [MCP servers](#mcp-servers)) |
| `mcp.server.add` | `name`, `definition`, optional `runtime_id` (since 1.37.0) | The new server as `mcp.server.list` reports it |
| `mcp.server.edit` | `name`, `definition`, optional `runtime_id` | The server afterwards |
| `mcp.server.remove` | `name`, optional `runtime_id` | `null` once the server is gone |
| `mcp.server.set_enabled` | `name`, `enabled`, optional `runtime_id` | The server afterwards |
| `mcp.server.rename` | `name`, `new_name`, optional `runtime_id`; since 1.37.0 | The server under its new name |
| `mcp.server.reconnect` | `name`, optional `runtime_id` and `workspace` of a server a runtime hosts | The server afterwards |
| `mcp.server.persist` | `name`, `target` (`user_file`) | The server afterwards, written to `config.toml` |
| `mcp.auth.begin` | `server`, the client's `redirect_uri` (loopback, or `https` since 1.11.0) | `authorization_url` to open in a browser |
| `mcp.auth.complete` | `server`, the `callback_url` the browser was redirected to | The `server` |
| `mcp.auth.cancel` | `server` | The `server` |

### User content

`run.start`, `run.steer`, and `session.submit_input` take the user's input as
`content`, a nonempty list of parts, and `session.recall_input` hands the
recalled input back the same way:

```json
"content": [
  { "type": "text", "text": "What is wrong with this layout?" },
  { "type": "file", "artifact": { "sha256": "…", "byte_length": 482113,
      "media_type": "image/png", "filename": "screenshot.png" } }
]
```

A `text` part carries text; a `file` part names an artifact of the session
tree's store by its `ArtifactRef`, as an upload returned it. The input-lane
events store the same content (see [Events](events.md)).

### Media

A client attaches a file by uploading it into the session tree's artifact
store and naming the result in a `file` part. A client that attaches before
its first prompt creates the session first; the upload publishes a session
that was still only in memory.

1. **`artifact.upload.begin`** `{ session_id, byte_length, filename? }`
   returns `{ upload_id, chunk_bytes }`. `chunk_bytes` is 1 MiB. A
   `byte_length` above the largest attachment cap (25 MiB) is refused with
   `media_too_large` whose `kind` is `file`. The upload belongs to this
   connection: another connection cannot see it, it is dropped when the
   connection closes, and a connection may have at most 8 in progress
   (`too_many_uploads`). Its bytes stream into a temporary file of the store,
   never into memory.
2. **`artifact.upload.chunk`** `{ upload_id, offset, data }` appends `data`
   (standard base64, at most `chunk_bytes` decoded bytes) at `offset`, which
   must be exactly the bytes received so far (`upload_offset_mismatch` names
   `expected_offset` otherwise; the upload waits for the right chunk). A chunk
   past `byte_length` is `upload_size_mismatch`. Requests run concurrently, so
   send a chunk once the previous one is answered.
3. **`artifact.upload.finish`** `{ upload_id, sha256 }` checks the digest
   (`digest_mismatch`), recognizes the media type from the content alone (a
   filename never decides it), validates the content as tool attachments are
   validated (structure, the 20 MiB cap or 25 MiB for video, and for images at
   most 16384 pixels a side and 16 Mi pixels), and stores the blob under its
   digest. Since 1.26.0 content that is no media file but UTF-8 without NUL
   bytes is a text file, `text/plain` of kind `text`, up to 1 MiB: an ACP
   agent may take it, a model never does. It returns `{ artifact: ArtifactRef, image? }`; put `artifact` in a
   `file` part as it is. An upload whose digest or content is refused
   (`digest_mismatch`, `media_type_unsupported`, `media_too_large`) is
   dropped; one finished before all of its bytes arrived
   (`upload_size_mismatch`) stays in progress.
4. **`artifact.upload.cancel`** `{ upload_id }` drops an upload in progress.

An unknown or another connection's `upload_id` is `upload_not_found`.

**Leases.** A finished upload is kept for an hour while no event references
it, then removed. Once an input that names it is submitted, its event keeps
it, as tool attachments are kept: a revert that removes every reference
removes it after its lease, and a fork takes it into the fork's tree. A
daemon restart forgets leases; an upload no event referenced then stays in
the store.

**Admission.** `run.start`, `run.steer`, and `session.submit_input` check
every `file` part against the model the input reaches: the run's selected
model (for a steer, the running run's; for a queued input, the model of the
selection it will start with). The part must name an artifact of the tree's
store whose content is what the reference says (`artifact_not_found`, or
`media_type_unsupported` when the content is not that type), of a kind the
model accepts (`media_unsupported` with `kind` and `model`), of a media type
it lists (`media_type_unsupported`), within its byte and image limits
(`media_too_large`, with the image's `width` and `height` and the model's
limits), and the parts of one kind must not outnumber `max_count`
(`media_limit_exceeded`). These are the model's
[`input_media`](#the-catalog-view); a client that checks them first never
meets these errors. A refused input records nothing. A model the input does
not reach this way, such as a fallback or a model the session moves to later,
sees a placeholder for each file it does not accept
([Media reads](tools.md#media-reads) describes the wording). A run of an
[ACP agent](#acp-agents) checks none of this: the agent gets each file it
takes (its `AgentRuntime.input_media`) and a note naming the others.

**`artifact.read`** `{ session_id, sha256, offset, length }` returns
`{ data, total_bytes, media_type }`: up to `length` (at most 1 MiB) bytes
from `offset` of any artifact of the session's tree (user uploads, tool
attachments, and the `full` content of truncated display blocks), base64
encoded, fewer at the end, and none at `offset = total_bytes`. `media_type`
is recognized from the content: a media type, `text/plain` for UTF-8 text,
or `application/octet-stream`. A blob is verified against its digest when it
is first read. An artifact the store does not have is `artifact_not_found`.

Model, agent, and catalog discovery uses `runtime.snapshot.get`. Skills use the
session-aware `skills.list` and `skills.get` methods because visibility depends
on the governing run policy and session overlay. `session.list` returns root
sessions only: a delegated child is selected as part of its root's tree, not as
a top-level entry, and stays reachable via `session.tree`, `session.children`,
`session.get`, and `session.resume`.

### Runtimes

A session uses the tools and workspace context of the
[runtime capabilities](../specs/runtime-capabilities.md) it is attached to. A
`RuntimeAttachment` is `{ name, runtime_id, capability, kind, workspace? }`:
the session-unique attachment `name` (also the tool prefix on name
collisions), the `runtime_id`, the runtime's `capability` the session uses,
the capability's `kind` (a label the runtime declares, such as `global` or
`coding`; the daemon never branches on it), and the canonical `workspace` the
runtime reported, if any. The session create, get, and resume results carry the
session's attachments in attach order; clients follow later changes through the
`runtime_attached` and `runtime_detached` events of the session. Since 1.7.0
every `SessionMeta` also carries them as `attachments`, kept current on attach
and detach, so `session.list` and `sessions.changed` show each session's
runtimes and workspaces without a `session.get`. Metadata an older build
wrote has none until the session records an event; a 1.6 daemon sends none.

`session.create` attaches `global` unless `global` is `false`, then each entry
of `runtimes` (`runtime_id`, optional `capability`, optional `workspace`) in
order; without `runtimes` it attaches `local`'s `coding` capability at `cwd`,
or at the daemon's working directory without one. `cwd` is the client's
directory as it names it. One daemon serves sessions of many directories, and
`session.list` with a `cwd_identity` shows one directory's. A session's
`cwd_identity` is decided once, when it is created, after its attachments
have opened: it is the canonical `workspace` that the first attachment with a
workspace reported, on its runtime's machine, never the client's text. So a
web client's `~/x` and a TUI's `/home/ada/x` attaching one workspace give
one identity. A session with no workspace attachment records `cwd`
canonicalized on the daemon's machine, or as given when that fails (the
daemon's working directory without `cwd`). Delegated sessions copy their
parent's, and sessions created before 1.7.0 keep the identity they stored.
An empty `runtimes` list creates a
session with no coding runtime. Instances that are not running are started
first, and a failure creates no session and records no instance. No session
is created over a runtime that is not online: a dial-in runtime that is not
connected, the one a project's defaults name included, is refused at once
with ``runtime `<id>` is not connected (last seen <time>)``, and an instance
that is already open is used only over a connection that answers (see
[Liveness](runtime-protocol.md#liveness)). `session.runtime.attach` follows
the same rules.

`session.runtime.attach` and `session.runtime.detach` are rejected while the
session has an active run (`session running`). Attach connects the runtime
and opens the capability's instance when needed, and records nothing when
that fails: neither an attachment nor, for an instance that never opened, a
`failed` instance in `runtimes.list`. The error says which step failed:
``runtime `<id>` failed to start: <reason>`` when the runtime could not be
reached, and ``runtime `<id>` could not open `<capability>` for workspace
`<workspace>`: <reason>`` with the runtime's own reason when it refused the
workspace, such as one outside the directories it serves. Without `capability` it attaches the runtime's only capability;
a runtime that offers several asks for one, naming them. The attachment's
name is the name the instance reports (`global`, or
`<runtime ID>_<workspace basename>` for a `coding` instance), suffixed `_2`,
`_3`, ... when the session already has an attachment of that name; attaching
an instance a session already has is an error. A capability that takes a
workspace requires `workspace`, except `local`'s `coding`, which opens at the
daemon's working directory without one; a capability that takes none
refuses one. Besides `global` and `local`, the runtimes are the configured
[`[runtimes.<id>]`](../runtimes/templates.md) tables and the
[dial-in runtimes](../runtimes/dial-in.md) that have
connected since the daemon started; attaching any other runtime ID is an
`unknown runtime` error, and attaching a dial-in runtime that is not
connected now fails. The daemon passes `workspace`
to the runtime exactly as given; the attachment records the canonical path
the runtime reported, and two spellings of one workspace share an instance.

`runtimes.list` returns each runtime as a `RuntimeListing`: its `runtime_id`;
its `lifetime` (`builtin` for `global` and `local`, `spawned` for a
template, `dial_in` for a runtime that connected on its own); whether the daemon is `connected` to it now; its `capabilities`
(`name`, `kind`, `takes_workspace`, `description`), empty until it has
connected once; its `platform`; its `machine` (see [Machines](#machines));
since 1.7.0 its `home`, the home directory it reported (runtime protocol
1.3.0, absent when it reported none), which `global` and `local` report as
the daemon's; and a `message` saying why its last attempt to connect failed, or, for a
dial-in runtime that went away, ``runtime `<id>` disconnected: <reason>``,
where a runtime that stopped cleanly gives `the other end closed the
connection`. Clients store and compare paths canonical and absolute and only show them
shorter: a path of a runtime with a `home` reads `~` for the home itself and
`~/rest` under it, and a path of a runtime without one stays absolute
(`cookie_agent_client_state::paths::display_path`). Each instance has its `runtime_id`, `capability`, reported
`attachment_name`, `kind`, `workspace`, and `platform` once started, its
`status` (`starting`, `ready`, `reconnecting`, `failed`, or `stopped`), and a
failure `message`.

`runtimes.reconnect` retries an instance now: a `failed` instance is started
again, under a new generation, and keeps its identity; an instance that is not
running is started; a `ready` or `reconnecting` one is left as it is.
`capability` and `workspace` follow the attach rule, and for a failed instance it is the
`workspace` that `runtimes.list` reports. A start that fails is an error with
the reason, worded as for attach. An instance `runtimes.list` already showed
stays `failed` with the new reason, which `runtimes.changed` reports; one it
did not show stays unrecorded. Retrying `global`
also retries its failed MCP servers.

### Machines

Every runtime belongs to one **machine**, the computer it runs on (see the
[memory spec](../specs/memory.md#3-machines)). The daemon keeps its machines
in a registry, `machines.json` in its data folder, which only the
`machines.*` methods change. A runtime is assigned when it connects, in this
order: the machine that pins its runtime ID; the machine that stands for the
machine ID it reported in the [runtime protocol](runtime-protocol.md#machines);
a new machine for that ID, named after the runtime's sanitized host name with
`-2`, `-3`, … added on a clash; and, for a runtime that reported no ID, a new
machine pinning it, named after its runtime ID. `global` and `local` belong to
the daemon's own machine. Machine names are a lowercase ASCII letter followed
by up to 31 lowercase letters, digits, `_`, or `-`. `runtimes.list` gives each
runtime's `machine`: the one it is assigned to while connected, else the one
it is pinned to or the daemon last saw it on.

Since 1.10.0 the registry remembers where each spawned or dial-in runtime
ran: when it connects, `machines.json` records the machine ID, platform, and
home it reported, and when the daemon last saw it connect or disconnect. A
runtime that is not connected is placed by its pin, else by that record, so
it keeps its machine while it is offline and across restarts; only a runtime
that never connected has no machine. `runtimes.list` gives such a runtime its
remembered `machine`, `platform`, and `home`, with `connected: false`. A
record goes with `machines.forget_runtime`, with `machines.forget` of its
machine, and, for a configured runtime, when a configuration reload or a
restart no longer has its `[runtimes.<id>]` table; a dial-in runtime's stays
until it is forgotten.

`machines.list` returns each machine as a `MachineInfo`: its `name`; the
machine `ids` it stands for; the runtime IDs `pinned` to it; the `connected`
runtimes assigned to it, each with its `runtime_id`, `assigned_by`
(`pinned` or `machine_id`), and `platform`; since 1.10.0 the `remembered`
runtimes that are not connected, each with its `runtime_id`, `assigned_by`
(`pinned` or `last_seen`), `platform`, `home`, and `last_seen` time; the
`platform` its connected runtimes agree on; and its number of `memories`, the
memory files in its memory folder.

Each change answers with the machines afterwards, moves the connected
runtimes it affects at once, and sends `machines.changed`; a session's
system prompt names the new machine from its next run. A change holds the
memory lock while it rewrites the registry and moves memory folders, and
fails, changing nothing, when it cannot:

- `machines.rename` fails with `machine_name_taken` when another machine, or
  a memory folder that belongs to none, has the name.
- `machines.assign` removes the runtime's pin from any other machine.
- `machines.unassign` of a runtime no machine pins is `runtime_not_pinned`.
- `machines.merge` fails with `machine_memory_conflict` when both memory
  folders hold a file of one name, and with `machine_merge_self` for one
  machine.
- `machines.forget` fails with `machine_in_use` while a connected runtime
  belongs to the machine or its memory folder holds anything. Remembered
  runtimes do not keep it; their records go with it.
- `machines.forget_runtime` fails with `runtime_still_connected` while the
  runtime is connected, and with `runtime_not_remembered` when the registry
  keeps no record of it. A pin stays.

An unknown machine is `machine_not_found`; a registry or folder that cannot
be read or written is `machine_store_failed`.

### Memory

The daemon's memory is Markdown files with `description` and `keywords`
frontmatter, in four layers, each file addressed by a memory URL:
`user://<category>`, `agent://<agent>`, `machine://<machine>/<name>`, and
`project://<project>/<name>` (see the [memory guide](../sessions/memory.md)
and the [memory spec](../specs/memory.md#5-memory-urls)). The `memory.*`
methods act on the files for the user, with no session; a change never
refreshes the memory snapshot of an open session, which a later run takes
when it would anyway.

`memory.list` returns each file as a `MemoryInfo`: its `url`, `layer`
(`user`, `agent`, `machine`, or `project`), `group` (the machine or project
of a `machine://` or `project://` memory), `revision`, `size` in bytes, and
either its `description` and `keywords` or the reason it is `invalid`. An
`agent://` file whose agent the agent registry no longer has is
`orphaned`. `ignored` lists, by path in the memory folder, what is not a
memory file of the layout (a file without `.md`, a name that breaks the
naming rules, something nested too deep) with the reason. `layer` keeps one
layer; `agent`, `machine`, or `project` keeps one agent's file, one
machine's memories, or one project's, and only one of the three may be
given. A filter that contradicts `layer` or another filter is `-32602`.

A **revision** is the first 16 hex digits of the SHA-256 of the file's
bytes. `memory.write` takes the whole file text, frontmatter included, and
checks it like the model's `memory_write`: it must parse, follow the file
format, and fit its kind's `[memory]` limit (a memory already over it may
only shrink; otherwise `memory_refused`). Without `revision` it creates the
memory, and fails with `memory_exists` when it exists; with `revision` it
replaces the memory at that revision, and fails with
`memory_revision_mismatch` when the file changed since, or
`memory_not_found` when it is gone. Both errors carry the file's current
`revision`. `memory.delete` needs the revision as well. Unlike the model,
the user may create and delete `user://` categories and delete `agent://`
files; an `agent://` file is created only for an agent the agent registry
has (`memory_refused`).

The daemon keeps an `agent://` file for every agent that is not internal
and owns its `description` and two keywords, the agent ID and its mode (see
the [memory guide](../sessions/memory.md#agent-memory)). A `memory.write` of
such a file keeps them: when the text gives another description or leaves a
daemon keyword out, the daemon puts its own frontmatter in place, keeps the
body and any other keywords, and says what it kept in the result's `notice`
(since 1.6.0). A file whose agent the registry no longer has keeps the
description it has.

A URL that is not a memory URL is `memory_url_invalid`; text that is not a
valid memory is `memory_invalid` with the reason; a malformed `revision` is
`-32602`. Another process holding the memory lock is `memory_busy`, and a
file that cannot be read or written is `memory_store_failed`. Each change
sends `memory.changed` to every connection; changes another daemon sharing
the data folder makes are not announced.

A `project://` URL must name a project that exists: a write to another is
`memory_refused`, naming the projects there are (since 1.8.0). Every project
has `project://<name>/overview`, whose description the daemon owns, kept by a
write and reported in its `notice` like an agent file's.

### Projects

Since 1.8.0 the daemon keeps projects: named bodies of work a session may
belong to (see [Projects](../concepts/projects.md) and the
[projects spec](../specs/projects.md)). `projects.list` returns each project
as a `ProjectInfo`: its opaque `id`, which sessions refer to it by; its
`name`, which follows the memory name rule and is the name of its memory
folder; an optional `emoji`; its `revision`; its `defaults`; whether it
`has_agents_md`; and the counts of its root `sessions` and its `memories`.

`defaults` is `{ preset?, agent?, global, runtimes }`: `global` and
`runtimes` (`[{ runtime_id, capability?, workspace? }]`) have the shape of
`session.create`'s members of the same names; `agent` and `preset` name an
agent and its preset as a `RunSelection` does. `project.create` and
`project.update` check them like `session.create`'s `runtimes`, except that
the runtimes need not be connected, and refuse an agent the daemon does not
list in that preset, or one that cannot run as root (`project_invalid`). An
`emoji` is exactly one emoji: one extended grapheme cluster of at most 32
bytes that Unicode marks as an emoji, not ASCII, whitespace, or a control
character.

A project's **revision** is the first 16 hex digits of the SHA-256 of its
`project.toml` and its `AGENTS.md`; `project.update` and `project.delete`
send the revision last read, and a project changed since is
`project_changed` with its current `revision`. `project.get` returns the
`AGENTS.md` text with the revision of the file alone. A name another project
has is `project_name_taken`; an unknown ID is `project_not_found`.

- **Sessions.** `session.create` with `project` records it with the
  creation (`session_project_set`); an unknown ID is `project_not_found`,
  checked before anything is attached. When the creation omits `runtimes`
  and the project's defaults name runtimes or leave out `global`, the
  session attaches the defaults' `global` and `runtimes`; an explicit
  `runtimes`, even an empty one, wins. The daemon never applies the default
  `agent` and `preset`: the creation's selection is the client's to set.
- **Membership.** `SessionMeta.project` is the project a session belongs to:
  a root's own, a delegated session's root's. A session whose project no
  longer exists has none. `session.project.set` moves a root session, also
  while it runs, which takes effect at its next run; a delegated session is
  `project_root_session_only`. Moving announces `sessions.changed`
  `upserted`, and never changes the session's attachments or agent.
- **Rename.** `project.update` with `name` moves the memory folder under the
  memory lock before it rewrites the project; when the move fails nothing
  changes. Old `project://<old>/…` URLs no longer resolve.
- **Delete.** `project.delete` is `project_sessions_running`, naming the
  root sessions in `session_ids`, while any session tree of the project is
  busy. Otherwise it deletes every session tree of the project as
  `session.delete` does (each announced as `removed`), then the project's
  whole memory folder, non-memory files included, then the project, and
  returns the counts.

Each change sends `projects.changed` to every connection. A failure to read
or write a project is `project_store_failed`.

### Cron jobs

Since 1.15.0 the daemon keeps cron jobs: prompts it fires on a schedule, into
an existing session or into a new session per fire (see the
[cron spec](../specs/cron.md)). A `CronJob` is its daemon-assigned `id`
(eight lowercase letters and digits), an optional `name`, the `prompt`, the
`schedule`, `max_runs`, the `target`, `delivery` for an in-session job,
`permission_mode` for a new-session job, an optional
`max_consecutive_failures`, `enabled` with a `disabled_reason` (`finished`,
`failed` with `failures`, or `disabled` `by` the user or an agent),
`created_by` (`"user"` or `{ agent: { agent, session } }`), `created_at`, and
its state: `next_fire`, `runs`, `consecutive_failures`, `last_fire`, and the
`history` of its latest fires.

- **Schedule.** `{ cron: "<minute hour day-of-month month day-of-week>" }`
  in the daemon host's local time, day of month and day of week combining
  with OR when both are restricted; `{ in: "<n>s|m|h|d" }` (1s to 365d) on
  create and update, which the daemon stores as `{ at: <time> }`; or
  `{ at: <time> }` in the future. `max_runs` is a whole number from 1 or
  `"unlimited"`, 1 when omitted, for cron schedules only; `crons.create`
  and `crons.update` also take a whole number of 0 or less for
  `"unlimited"` (1.42.0), which jobs report as `"unlimited"`. A job that reaches
  it is kept disabled as `finished` when you created it, and deleted when an
  agent did.
- **Target.** `{ session: <root session ID> }`, `{ subagent: <delegated
  session ID> }`, or `{ new: <template> }`. A template is `{ agent, preset?,
  model?, permission_rules?, runtimes?, global?, cwd?, project? }`, checked
  as `session.create` would check it; its agent must be able to run as a
  root. `delivery` is `queue` (default) or `steer`; `permission_mode`
  defaults to `auto_approve_n`.
- **Fires.** Each `history` entry is a `CronFire`: `scheduled_at` (absent
  for a run now), `fired_at`, `run_now`, the `outcome` (`pending`,
  `delivered`, `skipped` with a `reason`, or `failed` with an `error`),
  `missed` for a fire skipped because the daemon was down, the `session_id`
  it went to, and the `run_id` and `run_end` (`{ status, at }`) of the run
  its prompt landed in. A skip `reason` is `previous_fire_not_delivered`,
  `previous_run_still_running`, `daemon_was_down`, `interrupted`,
  `firing_disabled`, `daemon_restarting` (since 1.19.0), or `session_held`
  (since 1.34.0), an open set.
- **Run sessions.** A new-session fire's session records the job in
  `SessionMeta.cron` (`{ job_id, fired_at }`, also on its `session_created`
  event) and is otherwise ordinary.
- **Waiting.** A session targeted by an enabled job with a next fire lists
  `scheduled_job` in `SessionMeta.waiting_on`.

`crons.run` fires a job now, as a scheduled fire would; it is recorded as a
run now and counts toward neither `max_runs` nor the failure count. Each
change and fire sends `crons.changed` to every connection. An unknown job is
`cron_not_found`, a refused value `cron_invalid`, and a failure to read or
write `crons.json` `cron_store_failed`.

### Push notifications

Since 1.30.0 the daemon sends Web Push notifications to the browsers clients
registered ([push notifications spec](../specs/push-notifications.md)): when
a root session settles (its run ended and nothing but a recurring cron job
can start another), when an approval or an ACP agent's question waits for
the user, and when the persistent daemon goes down for a `daemon.restart`
(to every device but the one whose connection asked: the one whose last
`notifications.subscribe` came over it) and comes back up. `[notifications]`
configures them.

- **Subscribing.** A client subscribes the browser with the daemon's
  `public_key` (`applicationServerKey`), then sends the subscription's
  `endpoint` (an `https://` URL), `p256dh` and `auth` keys (base64url), and
  a `label` of 1 to 80 characters to `notifications.subscribe`. A
  `PushSubscription` as clients see it is `{ id, label, push_service,
  created_at, last_sent_at?, last_error? }`; the endpoint and keys are never
  sent back. A subscription the push service reports gone is removed.
- **Presence.** `client.presence` names the session a connection shows the
  user; the daemon pushes nothing for that session's tree while any
  connection shows it, and since 1.51.0 it marks the tree's root read. It
  lasts until the next call or the connection's end.
- **Errors.** An unknown subscription is `push_subscription_not_found`, a
  refused value `push_subscription_invalid`, and a push folder the daemon
  cannot read or write `push_unavailable`. Each subscription change sends
  `notifications.changed` to every connection.

### Restarting the daemon

Since 1.19.0, `daemon.restart` restarts the daemon: it relaunches its own
executable with the same arguments and environment, so a restart applies the
`restart_required` parts of the configuration status and runs a binary
upgraded on disk. Any client holding the token may call it.

- **`when: "idle"`** (the default) waits until no run is active in any
  session and no background call is pending: since 1.35.0 a call still
  running in the background, or one whose result is not yet committed,
  holds the restart too, since the shutdown would end it without a
  report. Meanwhile a new run (`run.start`, `session.submit_input` on an
  idle session, a producer's wake) is refused with `daemon_restarting`,
  queued input and producers' messages wait, and a cron fire is recorded as
  skipped with the reason `daemon_restarting`. A run's delegated children
  still start, as part of the turn being waited for.
- **`when: "now"`** interrupts the running turns as a stop does; they end
  as cancelled and the restarted daemon closes out anything left. Background
  calls end without a report.
- **A second call** while a restart is scheduled changes its `when`; `now`
  wins. Its `scheduled` is `false`.
- **`daemon.restarting`** (`when`, `active_runs`, `background_calls` since
  1.35.0, optional `reason`) reaches every connection when the restart is
  scheduled and whenever either count changes while it waits.
- **The restart** stops the daemon as a shutdown does (runs, plugins,
  runtimes, and MCP servers stop and the session logs and stores are
  flushed), sends the replies already queued, and closes every client
  connection with WebSocket close code 1012 and the reason `restarting`.
  Clients reconnect as after any dropped connection; the handshake's
  `engine_version` tells them when the build changed.
- **`restart_unsupported`** answers a daemon that cannot relaunch itself:
  only `cookie daemon` can, and an engine embedded in a test harness or
  another program cannot.

### Plugins

`plugins.list` returns every configured [plugin](../extend/plugins.md) in the
order its hooks run. A `PluginInfo` is `{ name, state, reason?, tools,
hooks, subscribe_events, subscribe_bus, publish_bus, producer_messaging,
dropped_events, stderr? }`: its `state` (`disabled`, `starting`,
`connected`, `reconnecting`, `failed`, or `stopped`), why it failed or last
disconnected, the tools it declares and the names of the hooks it
intercepts, what it streams and
publishes, whether its producer messaging is open, how many notifications it
dropped for falling behind, and the tail of its standard error.

`plugins.restart` starts a plugin again now: a `failed` or reconnecting
plugin is started at once, and a `connected` one is stopped and started. An
unknown or disabled plugin is an error (`plugin request rejected`). Status
changes arrive as `plugins.changed`.

### ACP agents

Since 1.17.0 the daemon lists the external coding agents its runtimes host
over the Agent Client Protocol (see the
[ACP agents spec](../specs/acp-agents.md)) with its own agents:

- An `AgentDescriptor` of the catalog view's `agents` has the optional
  `runtime` (`runtime_id`, `agent`, the agent's own ID on the runtime, and
  `available`) for an ACP agent exactly one runtime offers, and, since
  1.21.0, `runtimes`, every runtime that offers it. Its ID is the key of the
  runtimes' declarations (`claude`); a session runs it on the one runtime it
  attaches that offers it, so clients show it as runnable for a draft or
  session with exactly one of those attached. Its
  `resolved_fallback` is the synthetic model `acp/<agent ID>`: a run selects
  it like any agent, and the agent chooses its own model. An offering
  runtime that is away, or has the agent disabled, is listed with
  `available: false`; an agent no runtime can run is `runnable_as_root:
  false`. Creating a session, starting a run, or delegating with it is
  refused at once when the session's runtimes cannot run it (none of them
  offers it, two do, or the one that does is away), and attaching a second
  runtime that offers a session's ACP agent is refused.
- The event `acp_session_started` records the ACP session the agent talks
  to the session in (`agent`, `runtime_id`, `runtime_agent`,
  `acp_session_id`, `cwd`, `protocol_version`, `agent_info`, and `how`:
  `new`, `resumed`, or `loaded`). `acp_session_updated` carries what changed
  of the agent's `commands` (its slash commands: `name`, `description`,
  `input_hint`), `options` (its config options: `id`, `name`, `category`,
  `current_value`, `values`), and `plan` (a `plan` display block); each
  member is the whole new value, and clients keep the latest of each.
  Choosing a command sends `/<name> <input>` as the prompt's text.
- Since 1.20.0, `acp_session_started` has `steering` when the agent takes
  messages into its running turn; without it, a message sent to steer waits
  for the agent's next turn, and clients label the Steer action as
  queueing. `acp_session_updated` may carry, each shown once where it
  arrives, a `notice` (`severity` `info`/`warning`/`error`, `title`,
  optional `description`), which was also how a compaction the agent
  reports reached clients before 1.41.0; `steered` (`user_inputs`, the sequence numbers
  of the `user_input_submitted` events, and `producer_messages`): input
  delivered into the running turn, which clients mark on the message; and
  `tool_presentation` (`tool_call_id`, `presentation`): a new title, hint,
  primary argument, arguments, and preview for one of the agent's tool
  calls, which replace the call's own (its tool identity stays).
- Since 1.41.0, `acp_session_updated` may carry `compaction`
  (`compaction_id`, `state` `in_progress`/`completed`/`failed`/`cancelled`,
  and `summary` when it completed with one): a compaction of the agent's
  own conversation, recorded once as it starts and once as it ends.
  Clients show it as they show the session's own compaction; a failure
  also carries a warning `notice` saying why. It may also carry
  `session_message` (`direction` `received`/`sent`, `peer`, the other
  session as the daemon named it then, and `text`): a message between the
  agent's sessions, such as a subagent's task or its report, which clients
  show as they show a producer message. Before 1.41.0 both were `notice`s.
- Since 1.50.0, `acp_session_updated` may carry `background_ended`
  (`tool_call_id`, `tool`, `display`, `started_at`, and `exit_code` and
  `signal` when the agent reports them): a command of the agent's that ran
  in the background after its tool call ended, such as Claude Code's
  background shell, has ended. It is recorded where the end arrives,
  between runs too, before the run the end may wake, and clients show it
  as they show a background call's report.
- Since 1.52.0, `acp_session_updated` may carry `prompt_context`: the text
  blocks the session's first prompt to the agent carried ahead of the
  input (the tool instructions, the skill listing, the attachments, the
  memory snapshot turn, and the agent document's body), recorded as the
  prompt is sent. Clients show it as a collapsed row.
- Since 1.23.0, `acp_session_updated` may carry `context` (`used`, and
  `size` when the agent reports its window): the agent's context use from
  its latest `usage_update`, recorded as it changes. The agent's turns
  report no usage, so clients show `used` as the session's context, of a
  window of `size` tokens, until a model's turn reports its own.
- Since 1.23.0, an option of `options` may have `kind: boolean` (left out
  for a select): an on/off option whose `current_value` is `true` or
  `false` and whose `values` are `true` (On) and `false` (Off), so a client
  that reads every option as a select can still set it.
  `session.agent_option.set` takes `"true"` or `"false"` for it, and the
  daemon sends the agent a boolean.
- The agent's turns are model turns of `acp/<agent ID>`; the calls it
  reports are tool calls whose `tool.source` is `acp` (`agent`,
  `runtime_id`), and the calls it makes of cookie's tools through the MCP
  bridge are ordinary tool calls of its turn.
- `session.agent_option.set` changes one of the session's options and
  answers the options after the change.
- Since 1.26.0 each entry of an agent's `runtimes` (and `runtime`) has
  `options`, the config options a new ACP session of the agent starts with
  on that runtime, as the daemon last saw them with the agent document's
  `acp.mode`, `acp.config`, and `models` applied, and `input_media`, the files the
  agent takes in a prompt (`image`, `audio`, and `text` entries, as
  `input_media` on a model); both are empty until the agent has started
  there. A draft offers those options, the `model`-category one as its
  model and the `thought_level` one as its variant, and sends its choices
  as `session.create`'s `agent_options` (`[{ option_id, value }]`, at most
  64, each option once; refused for an agent that is not an ACP agent).
  The session records them as `acp_session_updated.starting_options`, and
  each new ACP session of the session applies them after `acp.config`
  and the document's `models`;
  one the agent does not offer then is skipped with a diagnostic.
- The display block `plan` (`entries`, each `content`, `status`
  `pending`/`in_progress`/`completed`, optional `priority`
  `high`/`medium`/`low`) reaches a client older than 1.17.0 as its text.

### Questions

Since 1.25.0 an agent of a run can ask the user a question: an ACP agent's
`elicitation/create` (see the [ACP agents spec](../specs/acp-agents.md)).
The run waits on it, and clients show it as waiting on the user.

- `elicitation_requested` (run-bound) carries `elicitation_id`, `agent` (who
  asks), the optional `tool_call_id` it belongs to, `message`, and exactly
  one of `form` and `url`. A `form` has an optional `title` and
  `description` and `fields`, each with `name` (its member in the answer),
  optional `title` and `description`, `required`, and `input`, tagged by
  `type`: `text` (`min_length`, `max_length`, `pattern`, which is shown but
  not checked, `format` `email`/`uri`/`date`/`date-time`, `default`),
  `choice` (`choices`, each `value`, `title`, optional `description`;
  `default`), `multi_choice` (`choices`, `min_items`, `max_items`,
  `default`), `number` and `integer` (`minimum`, `maximum`, `default`), and
  `boolean` (`default`). An input of a type a client does not know cannot be
  filled in; a question with neither `form` nor `url` can only be declined
  or cancelled. A `url` question asks the user to open the page (show the
  whole URL and its host first); accepting means the user agreed to open
  it.
- `elicitation.respond` answers it: `accept` with `content`, the form's
  values by field name (text and choices as strings, multi-choices as
  arrays of strings, numbers, booleans), `decline`, or `cancel`. The daemon
  checks an accepted form against its fields and refuses a value that does
  not fit with `elicitation_answer_invalid`, leaving the question waiting.
- `elicitation_resolved` records the answer: `elicitation_id`, `action`,
  `content` for an accepted form, `answered_by` (`user`; `unattended`, a
  decline for a session whose permission mode is `auto_approve_n` or a
  headless run; `run_ended`, a cancel as the run was cancelled or ended;
  `completed`, an accept when the agent reported that the URL's interaction
  finished; `unsupported`, a decline of a form with a required input this
  daemon cannot show), and the daemon's `reason` when it answered. Clients
  stop showing a question when it is resolved or its run finishes.

### Provider sign-in

A provider whose descriptor lists `sign_in_methods` is connected by a browser
sign-in instead of typed credentials (since 1.49.0); a sign-in method is
never in `auth_methods`, so older clients do not offer it, and
`provider.connect` refuses one with `provider_sign_in_rejected`. The daemon
holds the PKCE verifier, does the code exchange, and keeps the tokens; only
the single-use authorization code crosses the connection.

1. The client binds a loopback listener on `redirect_path` and calls
   `provider.auth.begin` with its `http://127.0.0.1:<port><redirect_path>`.
   A client that cannot listen (a web page) names any port and asks the user
   to paste the address the browser fails to load; a page served from
   loopback, on the daemon's machine, passes null and lets the daemon listen.
2. The user signs in at `authorization_url`.
3. The client hands the address the browser landed on to
   `provider.auth.complete`. When the daemon listened itself,
   `provider.auth.completed` reports the outcome instead.

The connected provider's `sign_in_account` names the account, and
`sign_in_required` turns true when the sign-in can no longer refresh; the
user signs in again with the same flow. `provider.disconnect` signs out and
revokes the sign-in. Every failure is `provider_sign_in_rejected`, whose
`reason` starts with the sign-in's own code: `sign_in_denied`,
`sign_in_not_eligible`, `sign_in_scope_missing`, `sign_in_state_mismatch`,
`sign_in_expired`, `sign_in_not_pending`, `sign_in_client_rejected`,
`invalid_redirect_uri`, `invalid_callback`, `sign_in_required`,
`sign_in_failed`, `sign_in_unsupported`, `sign_in_method_unknown`, or
`sign_in_method_not_connectable`.

### The catalog view

`runtime.snapshot.get` returns, and `runtime.changed` carries, a
`ModelCatalogView`:

```json
{ "revision": "<opaque>",
  "catalog": { "source": "network", "stale": false, "age": "current",
               "provider_quarantine_count": 0, "model_quarantine_count": 0,
               "last_error": null },
  "providers": [ … ], "models": [ … ], "agents": [ … ] }
```

- **`revision`** is one opaque string: it changes whenever anything the view
  shows changes. Clients compare it and send it back, never parse it.
  `provider.connect` and `provider.disconnect` take the revision of the view
  their form was filled from as `expected_revision`; a view whose catalog (for
  a connect) or provider state (for a disconnect) has changed since is
  refused with `catalog_revision_conflict` or `runtime_revision_conflict`,
  and the client reads the view again.
- **`catalog`** says where the model catalog stands: its `source`
  (`network`, `cache`, or `bootstrap`), whether it is `stale`, its `age`
  (`current`, `older_than_seven_days`, or `older_than_thirty_days`), the
  records it quarantined, and its nullable `last_error`.
- **`providers`, `models`, and `agents`** are client-facing descriptors only:
  no frozen model bindings, recipe identities, fingerprints, compiler versions,
  or the internal revisions and counters the view's `revision` stands for.
  `models` lists only selectable models; an agent's `resolved_fallback` names
  model selections. Since 1.22.0 an agent also has `unset_models`, the
  configuration fields (`default_model`, `small_model`) its `models` entries
  name that are not set, omitted when empty; those entries are skipped.
- **`input_media`** on every model says what it accepts in user input, one
  entry per kind (an open label: `image`, `pdf`, `audio`, or `video`; and
  `text`, which only an ACP agent's `AgentRuntime.input_media` lists):

  ```json
  { "kind": "image", "mime_types": ["image/jpeg", "image/png"],
    "max_bytes": 7864320, "max_dimension": 16384, "max_pixels": 16777216,
    "max_count": 20 }
  ```

  The limits are effective: the lowest of the model's catalog capability, the
  delivery limits of its adapter family for a user turn, and the attachment
  caps (20 MiB, or 25 MiB for video). `mime_types` lists the model's types an
  upload can be recognized as from its content; `max_dimension` and
  `max_pixels` are present for images. A kind absent from the list is not
  accepted: clients offer attachments only for the kinds of the session's
  selected model and check sizes before uploading.

### Model availability

Unusable models are visible instead of silently dropped. Each
`ProviderDescriptor` carries:

- `model_counts`: `available`, `quarantined`, `unsupported`, `needs_setup`, and
  `needs_credentials` counts over the provider's catalog rows (deprecated,
  disabled, and non-text models are not counted).
- `unavailable_models`: one entry per unusable model with `id`,
  `display_name`, `kind` (the four unavailable kinds above), and a nullable
  `reason` (the quarantine code or compiler reason). The list is strictly
  sorted by `id` and is populated only for configured (authored or stored)
  providers; unconfigured providers report counts alone.
- `documentation_url` and `environment`: the models.dev `doc` URL and `env`
  variable names. They are display hints; cookie never reads those variables.


## Steering

### Goal/producer integration handoff

The engine implements these methods through its per-session actor. Goal mutations
are root-only; producer inspection is available for any owned session.
Tool calls declare their output channels, and results carry terminal output
references and a display independent of their output.

| Surface | Public contract | Implementation owner |
|---|---|---|
| Goal projection | `GoalState`, `GoalItem`, `GoalStatus`, `GoalId` | Engine; TUI renders durable events |
| Goal read | `SessionGoalGetParams/Result`, `get_session_goal` | Engine; TUI/clients call async API |
| Activation | `SessionGoalSetParams/Result`, `set_session_goal` | User-only root command, rejects active/paused goal |
| Lifecycle | `SessionGoalLifecycleParams/Result`, `change_session_goal_lifecycle`, `GoalLifecycleAction` | User-only root pause/resume/cancel; reject stale/terminal goal |
| Model tools | `GoalGetParams/Result`, `GoalSetParams`, `GoalUpdateParams/Result` | Root-only `goal_get`/`goal_set`/`goal_update`, frozen at run admission |
| Runtime inspection | `SessionProducersParams/Result`, `session_producers`, `ProducerRegistration`, `ProducerSourceState` | Session runtime snapshot, not global catalog snapshot or persistence |
| Producer messaging | `Producer{Register,Send,Discard,Unregister}Params/Result` | `producer/*` requests of plugins and runtimes; SDK mirrors register/send/discard/unregister |
| Recovery handshake | `RecoveryStartParams`, `RecoveryCompleteParams/Result`, `RecoveryOutcome` | `producer/recovery/start` daemon notification (no deadline/response); completion request |

Goal activation accepts an optional `selection: RunSelection`, including agent,
model, variant, and preset. The engine validates and persists this selection
before scheduling a wake, so it survives waiting for producers and restarting.
Lifecycle requests accept `selection` only with `action: "resume"`; pause and
cancel reject a supplied selection. Resuming without it preserves the goal's
previous selection. If activation omitted it and no later resume supplied one,
automatic runs use the latest persisted run selection, falling back to the
session's creation selection. None of these changes mutate an already-running
turn's frozen selection or attribution.

`GoalItem { description, finished }` has no item ID.
`GoalUpdateParams { items }` replaces the entire ordered checklist through the
session actor: the last accepted update wins for the current/latest session goal
at actor acceptance. Model updates have no `goal_id`, `expected_revision`, or
lost-update protection. An older run's update can intentionally affect a newly
activated active or paused goal; updates reject if the current goal is absent or
terminal. Internal `GoalId` and engine-owned `GoalState.revision` remain for durable
state and reminder identity. User lifecycle RPC controls retain
`SessionGoalLifecycleParams.goal_id` and `expected_revision` and reject stale
identities and revisions. `GoalSetParams { objective, items? }` lets the model
start a goal when none is active or paused, as `session.goal.set` does; no
model tool can change an objective or issue lifecycle commands. Empty lists preserve
the active/paused lifecycle and can bootstrap an active goal without completion.
The goal tools declare no permission: they are published to every root run
(an ACP agent's only while the goal is active or paused), and every call is
allowed. There is no permission action or goal configuration for them.

To preserve strictly increasing event revisions, an all-finished update appends
checklist revision `r+1`, then completion revision `r+2`, and returns the final
state. Recovery repairs a missing completion event after a committed all-finished
checklist. Empty checklists never complete through this rule.

See [Events](events.md#goal-and-producer-contracts) for admission/commit
coverage and owned-message discard, and [Plugin development](../development/plugins.md#producers)
for method names, idempotency, recovery, and the no-implicit-registration requirement.

### What a session waits on

Since 1.9.0 every `SessionMeta` carries `waiting_on`: what will start the
session's next run without the user, each kind once and in this order, and
omitted when nothing will. It is an open enum; a client reads a kind it does
not know as something the session waits on.

| Kind | The session waits on | Source |
|---|---|---|
| `goal` | Its goal, which is active: goal mode keeps scheduling runs. A paused, completed, or cancelled goal schedules none. Root sessions only. | The fold of the goal events, so the metadata cache and a rebuild of it keep it |
| `background_call` | One of its background calls, whose `<background_result>` starts a run | The daemon's live background calls |
| `subagent` | A direct child running a turn on its own (one `session.background.list` lists), a background delegation still queued or running, or a direct child that itself waits on a background call, an external producer, or a subagent (recursively), so it can still wake and report; its `<subagent_report>` starts a run | The daemon's live runs, delegations, and its children's live waits |
| `external_producer` | A plugin or runtime that holds a live producer registration for the session and may send to it | The daemon's live registrations |

The last three end with the daemon: a restart kills background calls and
drops registrations, so they are never written to the metadata cache and a
client never shows one a restart ended. A delegated session reports its own
background calls, subagents, and producers; its parent reports its turns as
`subagent`. Accepted producer messages that have not started a run yet are
not listed: a message to an idle session starts its run at once, and one
queued behind a running run starts the next.

`waiting_on` is reported whatever `status` says. A session list shows a
session that is `running` as running, else one that waits on something as
waiting, else a failed, cancelled, or interrupted last run as a problem, and
nothing for an idle or completed one; `cookie_agent_client_state::activity`
implements that order. `sessions.changed` `upserted` announces every change:
a goal activated, paused, resumed, completed, or cancelled; a background
call detached or reported; a child's turn started or ended; a delegation or
external registration made or dropped; a child started or stopped waiting
on something live. A session the daemon evicted from memory keeps reporting
what it waits on. A 1.8 daemon sends no `waiting_on`.

### Unread sessions

Since 1.51.0 a root session's `SessionMeta` carries `unread: true` (left out
when false) while it has a finished turn, an approval, or a question the
user has not seen ([spec](../specs/unread-sessions.md)). The daemon marks a
root unread when it settles (the case that sends a settle push), or when an
approval in its tree escalates to the user or an ACP agent there asks a
question, while no connection reports presence (`client.presence`) on a
session of its tree; `[notifications]` decides only whether a push goes
out. A `client.presence` naming any session of the tree marks the root
read, and `session.unread.set` marks roots either way. The state lives in
`<data_root>/unread.json` and survives restarts; it is not in the session
log, so it moves neither `last_event_seq` nor the activity times.
Delegated sessions are never unread. Each change sends `sessions.changed`
`upserted`. A 1.50 daemon sends no `unread` and answers
`session.unread.set` with `-32601`.

### Held sessions

Since 1.34.0 a session whose run the user stopped (`run.cancel`) while
something could start the next one on its own, or whose runs failed more
than `[model_retry] standard_retries` times in a row, is
**held** ([spec](../specs/held-sessions.md)). Nothing starts a run of it on
its own: its pending producer messages wait, goal reminders stop, a plugin's
or runtime's `producer.send` is refused with `producer_rejected`, a cron
fire into it is skipped as `session_held`, and mail from any agent but its
parent fails with `send_message:recipient_held`. The engine's own reports
(subagent turns, delegations, background calls) are accepted and wait.

`SessionMeta.held` carries the reason while it lasts, from the latest
`automatic_runs_held` event. It ends with `automatic_runs_resumed`, written
by `session.held.resume` or by the parent's mail to its subagent, or with
any input the user submits; the failure count starts over then. A run that
a daemon restart interrupted is neither a stop nor a failure: it resumes as
before. Clients show the hold and offer **Resume** in place of Send; a
session held for failures sends a push notification, one the user stopped
does not.

### Where a root's subagents work

Since 1.10.0 a root session's `SessionMeta` carries `delegated_attachments`:
the attachments its subagents were created with that it records neither as
its own `attachments` nor as delegated before, in the order they brought
them, `global` aside, and the same runtime, capability, and workspace once.
It is the fold of the root's `delegated_attachments_added` events (see
[Events](events.md)), so the metadata cache keeps it. It only grows: a child
that finishes or is evicted, and a detach on the root, leave it as it is. A
client builds a root's badges from `attachments` followed by
`delegated_attachments` (`cookie_agent_client_state::badges`), and a child's
from its own `attachments` only. A delegated session's list is empty, and a
1.9 daemon sends none.

### User steering

`run.steer` requires an active target run. It immediately appends
`user_input_admitted` and returns `{ "accepted": true }`; admission does not
change model history. At each completed tool batch and no-tool completion
boundary, all pending inputs are promoted FIFO as separate
`user_input_submitted` events before the next request. A no-tool boundary with
no pending input completes the run.

`run.steer` also returns the admission's `user_input_seq`. Terminal run events
void anything still pending.

### The session's model selection

A session has one model selection (agent, model, variant, preset) that every
client shares. `session.select_model` validates a selection as a run start would
and appends a runless `session_model_selected` event under the caller's origin;
selecting what the session already has records nothing and returns
`{ "changed": false }`. `cookie_agent_protocol::SessionModelState` folds the
session's selection from its events: the creation selection, then each run's
selection and successful fallback, with an explicit pick replacing it and
dropping any fallback the session had settled on. A run started without a
selection (`session.submit_input` with `selection` omitted, or a queued input
queued that way) uses it, and clients keep their drafts at it, so the latest
pick from any client is what the next message runs on. A request that names its
own selection still runs on that one.

### Submitting from several clients

`session.submit_input` is how clients send a message without first finding out
whether the session is running. On the session's actor it either starts a run
(`{ "outcome": "started", "run_id" }`), or, while a run is active, follows
`delivery`:

- `steer` admits the input as `run.steer` does:
  `{ "outcome": "steered", "run_id", "user_input_seq" }`.
- `queue` appends a runless `user_input_queued` event holding the input, its
  selection, and `client_run_id`: `{ "outcome": "queued", "queued_seq" }`.

A run that starts or ends while the request is in flight only changes which
outcome happens; the daemon retries a few times before answering with a
`session_running` conflict. An omitted `selection` means the session's current
selection. A plugin that handles the input yields `{ "outcome": "handled",
"reason" }`. Retrying the same `client_run_id` with the same input returns the
original run or queue entry; different input is an `idempotency_conflict`.

When a run completes normally, the daemon starts a run with the oldest queued
input under the origin of the client that queued it; that run's `run_started`
carries the entry's `client_run_id`, which removes it from the queue. A failed,
cancelled, or interrupted run voids every queued input, and the daemon appends
`user_input_queue_voided` when a queued input cannot start at all. Clients
reduce the queue with `cookie_agent_protocol::QueuedInputs`, as the daemon does.

`session.recall_input` takes back one pending steered input or one queued input
by its seq. Since 1.31.0 any client may, another client's input too: the recall
is recorded under the origin of the client that asked, and the content goes back
to it. A daemon before 1.31.0 lets only the client whose origin admitted it, and
answers another client `{ "outcome": "not_owner" }`. A pending input is withdrawn with
`user_input_recalled` and a queued one with `user_input_queue_recalled`; both
return `{ "outcome": "recalled", "content" }`, the input's content for editing. An input that was already promoted,
started, or voided returns `{ "outcome": "gone" }`.

`run.start` on a session whose run is active fails with code `-32602` and
`RunStartConflict` data whose `code` is `session_running`, so a client can fall
back to `session.submit_input`.

## Revert and fork

`session.revert` is idle-only. The target must be a positive existing
sequence, and `expected_last_seq` must be the session's current last event
sequence: a session that has moved on since the client looked (another client's
turn, say) is refused with the error `stale_revert`, whose data carries
`current_last_seq`, so a revert never
deletes what the requester has not seen. The engine removes every event after it from the log (a run left with
nothing but its `run_started` goes too), closes any run, tool call, internal
agent, or approval the cut left open, and deletes the subagent sessions created
by removed delegations; a running one refuses the revert. Later appends reuse
the freed sequence numbers. Subscribers receive a `rewound` notification (see
[Subscriptions](events.md#subscriptions)).

`session.fork` takes a `client_fork_id`: while the daemon runs, a retry with the
same key and target returns the first fork instead of making another, and the
same key with a different target is an `idempotency_conflict`.
`session.fork` may read an active source but requires a persisted prefix that
contains a submitted user message. The fork copies that prefix exactly under a
new session ID, closes any copied in-flight run locally, appends ` (fork)` to
the title, and continues with new physical sequences.

## Delete

`session.delete { session_id, expected_last_seq }` deletes a root session's
whole tree from disk: the root, every delegated session under it, their
events and metadata, and the tree's artifacts. Nothing is kept for undo, so
clients ask the user first. It is new in 1.5.0: a client sends it only to a
daemon of minor 5 or later, and reads `-32601` as "the daemon is too old to
delete sessions".

`expected_last_seq` is the root's `last_event_seq` from the metadata the user
confirmed. The delete is refused, changing nothing, when:

- the session does not exist (`session_not_found`);
- it is a delegated session (`not_a_root_session`, with its
  `root_session_id`): a tree is deleted through its root;
- a session of the tree has an active run, a compaction, or a subagent
  waiting for a slot (`session_tree_running`, with the busy
  `running_session_id`);
- another cookie process holds the tree's `owner.lock`
  (`session_owned_elsewhere`);
- the root's last event is no longer `expected_last_seq`
  (`session_changed`, with `current_last_seq`).

The checks run in this order, so a running tree is `session_tree_running`
even when its guard is stale too, as it is while a run streams past the
listed `last_event_seq`; `session_changed` means the tree is idle and moved.

A tree this daemon does not hold is deleted under its lock without being
opened, so nothing is appended to it first. The tree's background calls are
stopped without a report and its uploads in progress are dropped. The daemon
moves the tree's directory aside to a `.deleted-<root>-<id>` tombstone in the
work directory, which commits the delete, forgets the tree, sends
`sessions.changed` `removed` for each deleted session (the root first) to
every connection, and removes the tombstone; a daemon that stopped in between
removes the tombstone when it next opens the directory. Each connection's
subscriptions to the deleted sessions end with them: no further
`events.subscription` arrives for them and `events.unsubscribe` answers
`false`. The result is `{ "deleted": [<root>, <descendants>…] }`.

## Notifications

| Notification | Payload |
|---|---|
| `runtime.changed` | The previous view's `revision`, the complete new [catalog view](#the-catalog-view), and the change reasons, sorted and without repeats |
| `events.subscription` | One stored event, live-only stream output, a session sequence gap, or a rewind |
| `events.plugin` | Session-scoped non-durable plugin event |
| `sessions.changed` | One session index change: `upserted`, `removed`, `approvals_changed`, `permission_mode_changed`, or `resync` |
| `mcp.changed` | MCP server configuration or status changed; optional `failures` |
| `runtimes.changed` | A runtime template was registered or an instance's status changed; optional `failures` |
| `background.changed` | A subscribed session's background calls changed: `session_id` and its current `calls` |
| `plugins.changed` | A plugin's status changed; an empty object |
| `machines.changed` | The machine registry changed, or a runtime joined a new machine; an empty object |
| `memory.changed` | A memory file changed through the daemon; an empty object |
| `projects.changed` | A project was created, changed, or deleted; an empty object |
| `crons.changed` | A cron job was created, changed, fired, or deleted; an empty object |
| `notifications.changed` | A push subscription was added, changed, or removed (since 1.30.0); an empty object |
| `daemon.restarting` | A restart is scheduled: `when` (`idle` or `now`), the `active_runs` and `background_calls` (since 1.35.0, omitted when zero) it still waits for, and an optional `reason` to show (since 1.19.0; see [Restarting the daemon](#restarting-the-daemon)) |
| `onboarding.changed` | `onboarding.get`'s answer changed (since 1.22.0): the new answer, after a configuration apply or reload, a provider connected or disconnected, or a model becoming available or not |
| `provider.auth.completed` | A sign-in whose redirect the daemon caught itself ended (since 1.49.0): `provider_id`, and the `account` or the `error` |

A background call in `session.background.list` and `background.changed` is
`{ job, tool, tool_call_id, display?, started_at, detached_at, by, agent_task? }`:
the job ID, the tool's name as the model called it, the call it came from,
its display argument, when the call started and when it moved to the
background, and who moved it (`model` or `user`). `agent_task: true` (since
1.40.0) marks an ACP agent's own command still running after its call ended:
`job` is its terminal, and `session.background.stop` refuses it, since only
the agent can stop it. The list lives in the daemon, so it
never shows a call a daemon restart killed. `background.changed` reaches only
connections subscribed to the session; one that falls behind gets every
subscribed session's list again.

### Daemon-wide changes

`runtime.changed`, `sessions.changed`, `mcp.changed`, `runtimes.changed`, `plugins.changed`, `machines.changed`, `memory.changed`, `projects.changed`, `crons.changed`, `notifications.changed`, `daemon.restarting`, and `onboarding.changed` reach every connection
from its handshake on, whether or not it subscribed to any session, so each
client sharing a daemon stays current with what the others do. Clients ignore
notification methods they do not know.

`sessions.changed` carries one change, tagged by `change`:

| `change` | Fields | Sent when |
|---|---|---|
| `upserted` | `session` (`SessionMeta`) | A session, root or delegated, is created, first persisted, or reverted, or its status, title, attachments, delegated attachments, project, what it waits on (`waiting_on`), or whether it is unread (`unread`, since 1.51.0) change |
| `removed` | `session_id` | A revert deleted a delegated session, or `session.delete` deleted a session of a tree; the connection's subscription to it has ended |
| `approvals_changed` | `root_session_id`, `session_id` | An approval in any session of the tree was requested, escalated, decided, or cancelled; `approval.list` for the root shows the result |
| `permission_mode_changed` | `root_session_id`, `mode`, `changed_by` | `session.set_permission_mode` changed the tree's mode; `changed_by` is the caller's origin |
| `resync` | none | This connection fell more than 256 changes behind and missed some; refetch `session.list`, approvals, and permission modes |

The permission mode is runtime-only and has no event, so `permission_mode_changed`
is the only way other clients learn of a change.

`runtime.changed` notifications form a chain through `previous_revision`. A
connection that falls more than 64 notifications behind receives one with a
null `previous_revision`, the current snapshot, and the reason `resync`; a
client that holds a different revision refetches `runtime.snapshot.get` rather
than trusting either. A status `mcp.changed` is an empty object;
bursts coalesce into one notification, and a client refetches `mcp.server.list`.
`runtimes.changed` and `plugins.changed` work the same way for
`runtimes.list` and `plugins.list`, `machines.changed` for
`machines.list` and `runtimes.list`, `memory.changed` for
`memory.list`, `projects.changed` for `projects.list`, `crons.changed`
for `crons.list`, and `notifications.changed` for `notifications.get`.

### Connection failures

When an MCP server fails to connect, or drops and does not reconnect, and when
a runtime instance fails to start, the daemon sends one notification for it
with a `failures` list to each connection subscribed (`events.subscribe`) to a
session that uses it. Each entry names the affected `session_ids` among that
connection's subscriptions:

| Notification | `failures` entry |
|---|---|
| `mcp.changed` | `runtime_id` hosting the server (`global` for top-level `[mcp]`), `workspace` of the hosting instance when its runtime has one, `server`, `reason`, `session_ids` |
| `runtimes.changed` | `runtime_id`, `capability`, `workspace` when the capability takes one, `reason`, `session_ids` |

Subscribing to a session also sends the failures that already affect it, so a
client that opens a session after its servers failed still learns why their
tools are missing. Failures are live notifications, not session events; the
TUI shows each as an error row in every named session it displays.
`failures` is omitted when empty.

### MCP servers

`mcp.server.list` returns the servers of every runtime: the global runtime's
top-level `[mcp]` servers first, then those each running coding runtime
instance hosts, from its `[runtimes.<id>]` table in the daemon's
configuration, a dial-in runtime's included (since runtime protocol 1.10.0;
an older dial-in runtime hosts `[runtimes.local.mcp]` of its own machine). Each entry has the hosting
`runtime_id`, the hosting instance's `workspace` (absent for `global`),
`name`, `source`, `definition`, `state`, `tool_count`, `message`, and
`auth_in_progress`. Since 1.36.0 it also has `server_tools`: every tool the
server lists, by the server's own name (what `include_tools` and
`exclude_tools` match) and in its order, as `{ name, filtered }`, with
`filtered: true` (omitted when false) for a tool its filters left out;
absent while the server has listed nothing and from a runtime older than
runtime protocol 1.9.0. `source` is `user_file` (the daemon's `config.toml`),
`runtime` (a global server changed in the running daemon only), or, since
1.12.0, `machine`: defined on the hosting runtime's machine (an older
dial-in runtime's own servers), which the
daemon does not read, so its `definition` is empty (no `command` or `url`).
`source` is an open enum; a 1.11 client reads `machine` as `runtime`. Before
1.12.0 a dial-in runtime's own servers were left out of the list. Since
1.13.0 an entry of a runtime other than `global` may carry `scope`,
`runtime` or `workspace` (an open enum; an unknown value reads as
`workspace`; see [scope](../extend/mcp.md#scope-one-per-workspace-or-one-per-runtime)):
a `runtime` server is one process its runtime shares across its
workspaces, listed once without a `workspace`; a `workspace` server is
listed for each instance that hosts it, with the instance's `workspace`,
and, while no instance of a connected runtime hosts it, once without one,
`disconnected` with the message "starts when a workspace opens".
`state` is `connected`, `connecting`, `reconnecting`,
`failed`, `needs_auth`, `disabled`, or `disconnected`. A `reconnecting` server
dropped and is being restarted: its tools stay published and calls fail with
the `runtime_unavailable` failure code until it reconnects or, after every
attempt failed, becomes `failed` and its tools are dropped (see
[MCP](../extend/mcp.md#connection-lifecycle)). The `mcp.server.*` mutations
act on the global runtime's servers, or, since 1.37.0, with `runtime_id`,
on another runtime's (see [Runtime tables](#runtime-tables)); `mcp.auth.*`
act on the global runtime's. `mcp.server.reconnect
{ name, runtime_id?, workspace? }` reconnects a server of any runtime and
waits for the attempt: without `runtime_id` (or with `global`) a top-level
server, otherwise the one the instance of `runtime_id` at `workspace` hosts,
as `mcp.server.list` names them; without `workspace`, a runtime-scoped
server of `runtime_id` (a waiting workspace-scoped one fails: it starts with
a workspace).

### Runtime tables

Since 1.37.0 clients change a runtime's `[runtimes.<id>]` table in the
daemon's `config.toml`: its MCP servers with the `mcp.server.*` mutations
and a `runtime_id` other than `global`, and its ACP agent declarations
with `runtimes.agents.*`. Each change is one configuration edit, written
and applied as `config.apply` writes (it fails `config_conflict` when the
file changed meanwhile), and a connected runtime gets it at once (runtime
protocol 1.10.0, `runtime/settings_changed`). A runtime server's
`definition` carries `scope` (`runtime` or `workspace`), which a `global`
server refuses. A server another dial-in runtime of an older cookie defines
on its own machine (`source: machine`) is not in the file and cannot be
changed.

- **`mcp.server.rename`** moves the server's table in one edit. A `global`
  server changed in the running daemon only, without `mcp.server.persist`,
  is refused, and a `global` server's OAuth sign-in does not follow its new
  name.
- **Agents every runtime offers.** Since 1.43.0, `runtimes.agents.*` with
  `runtime_id` `global` change the top-level `[acp.agents]` table: the ACP
  agents every runtime but `global` offers, each run on the runtime's
  machine. A runtime's own declaration of the same key replaces it there,
  with another command or `enabled = false` to leave it off; removing that
  one offers every runtime's again. A `global` declaration is `available`
  while some runtime runs it. Before 1.43.0 a daemon refuses `global`.
- **`runtimes.agents.list`** returns each declaration's `agent` (its key and
  the ID of the agent it offers), `definition`, `available` (the runtime
  reported it and a session that attaches the runtime can run it now), and
  `documents`: the agent documents that run it, the one of its own ID and
  the templates whose `acp.agent` names it, as `config.get` paths.
- **`runtimes.agents.rename`** moves the declaration. When no other runtime
  declares the old key, the same edit moves `agents/<agent>.md` to
  `agents/<new_agent>.md` (`moved`); templates whose `acp.agent` names the
  old key are not rewritten, and `stale_templates` lists them. Sessions keep
  the agent ID they recorded.
- **`runtimes.agents.remove`** leaves the agent's documents in place.

The shared client maps these to `ClientDelivery` variants (`Live`, replay
deliveries, `PluginEvent`, `RuntimeChanged`, `RecoveryFailed`), so a UI
consumes one ordered stream and never parses raw JSON-RPC frames.

Tool display comes from `tool_call_progress` and `tool_call_terminated`
events; retained output is read through its artifact references. Raw tool
output is not streamed to clients.

`text_delta`, `reasoning_delta`, and `tool_call_progress` are live-only:
`events.subscription` delivers them as `transient` messages carrying the
durable `after_seq` they follow instead of a sequence of their own, and they
are never stored or replayed. `model_output_started` durably marks where each
streamed part began. See
[live-only stream output](events.md#live-only-stream-output).

The MCP OAuth callback runs on the client, so a daemon reached through a
tunnel or on another machine can still be authorized from the client's
browser. `mcp.auth.begin` requires `redirect_uri`, where the browser lands:
the client's own loopback callback, plain `http` to a loopback IP literal or
`localhost` (`http://127.0.0.1:<port>/<path>`), or, since 1.11.0, an `https`
address such as the web client's own page with `?oauth=callback`. Any path
and query are allowed; user info and fragments are refused. Before 1.11.0
the daemon accepts only loopback IP literals with a port and no query, so a
client offers an `https` redirect only to a 1.11 daemon. With dynamic client
registration the daemon registers a client for the redirect of each flow
(as `native` for a loopback redirect, `web` for an `https` one); a configured
`client_id` is used as is, and its authorization server must allow the
redirect. The client then sends the browser's redirect to
`mcp.auth.complete { server, callback_url }`; the daemon keeps the PKCE
verifier, exchanges the code, and stores the tokens, which never cross the
connection. A `callback_url` that is not a redirect to the pending
`redirect_uri` (its scheme, host, port, path, and every query parameter of
its own) with a `code` or `error` parameter is rejected and leaves the flow
waiting. Any other completion, including a `state` mismatch, ends it. A
`begin` with the pending `redirect_uri` returns the same authorization URL;
one with a different `redirect_uri` replaces the pending flow.

A client can configure permissions before a session exists.
`session.create` accepts an optional `permission_mode` for the new tree and
optional `permission_rules`, which are validated like any overlay (no duplicate
action, runtime selector, and pattern, at most 256, and no runtime selector on
`message` or `skill`) before the session is created and committed as
its first `session_permission_overlay_set` event, so the first run already sees
them. `session.permission.preview { selection, rules }` returns the effective
permissions such a session would have, computed from the selection's frozen
agent policy exactly as `session.permission.get` computes them for a live
session.

Background jobs report through the `tool { tool_call_id }` producer owner (the
completion report of a job a tool call started, such as a `bash` call with
`background: true`), and `producer_message_accepted` may carry
`retained_output` naming the job's captured output artifacts.

Tool output that fits its preview is stored only in the result's output:
`RetainedToolStream.reference` and `sha256` are absent for such a stream, and
`RetainedToolOutput.reference` is absent when no stream needed an artifact.
`RetainedToolStream.dropped_bytes` is present when output past the 128 MiB
capture limit was not retained. A result always carries its whole
`display`: the ranges of `output` a session log stores display text as
([Events](events.md)) never reach clients.

Session metadata includes additive `skipped_events` entries with the physical
sequence (or source line number when no sequence was readable) and a safe reason.
An empty array means the event log loaded without skipped records. Optional-field
degradations remain available to the engine as load diagnostics but are not
reported as skipped events.

See [Events](events.md) for event payloads and durability semantics.

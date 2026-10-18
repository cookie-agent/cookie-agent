# Protocol Reference

The daemon exposes JSON-RPC 2.0 over an authenticated WebSocket at `/ws`.
Protocol 49 is current-only. A client must call `handshake` before any other
method:

```json
{ "protocol_version": 49, "client": { "kind": "tui", "instance_id": "<uuid>" } }
```

`client.kind` names the client program: a lowercase letter followed by up to 15
lowercase letters, digits, or hyphens (the shipped clients use `tui` and `cli`).
`client.instance_id` is a UUID identifying one running client; a client keeps it
across reconnects so it can recognize events it caused. Any number of clients
may connect to one daemon at once.

The response is `{ "protocol_version": 42, "origin": "client:<kind>-<id>" }`,
where `<id>` is the instance id as 32 lowercase hex digits. The daemon stamps
that origin on every event the connection's requests cause, so each client can
tell its own actions from another client's. A repeated handshake on the same
connection keeps the first identity. Protocol 34 added the required `client`
member and the `origin` result. Protocol 35 replaced `run.recall_steer` with
`session.recall_input` and added `session.submit_input` with queued input (see
[Submitting from several clients](#submitting-from-several-clients)), and
protocol 36 added the `resync` runtime change reason. Protocol 37 made
`session.revert` require `expected_last_seq` and `session.fork` require
`client_fork_id`, and added `session.compact.cancel`. Protocol 38 added
`session.select_model` and the `session_model_selected` event. Protocol 39
removed the workspace configuration layer: the `workspace` agent document source
and the `workspace_file` MCP configuration source and target are gone.
Protocol 40 added runtime attachments (see [Runtimes](#runtimes)):
`session.create` takes `runtimes` and `global`, the session create, get, and
resume results carry the session's `runtimes`, `session.runtime.attach`,
`session.runtime.detach`, `runtimes.list`, and `runtimes.reconnect` are new,
`runtimes.changed` is a new notification, permission rules, overlay mutations, approval requests, and
tree grants carry a runtime selector or runtime ID, and `delegate_subagent`
accepts `runtimes`. In the same version the `lazy` MCP server setting and the
`lazy-not-connected` state were removed, MCP servers gained the `reconnecting`
state and their hosting `runtime_id` and `workspace` in `mcp.server.list`,
`mcp.server.list` lists the servers coding runtimes host,
`mcp.server.reconnect` takes an optional `runtime_id` and `workspace`, and
`mcp.changed` and `runtimes.changed` gained `failures` (see
[Connection failures](#connection-failures)). Protocol 41 made a runtime's
`kind` a free-form label the runtime declares instead of `global` or
`coding`, and made `PermissionAction` a free-form label (`[a-z][a-z0-9_]{0,31}`)
instead of the nine built-in actions, so permission
rules, overlays, grants, approval requests, and `session.permission.get` can
name a runtime's own actions; `session.permission.get` lists the built-in
actions, then the actions of the session's tools, then any other action a rule
names. Protocol 42 removed `run.tool_stdin`: tools no longer take input while
they run. It added background calls: `run.tool_background`,
`session.background.list`, `session.background.stop`, and the
`background.changed` notification, and the `subagent { session_id }`
producer owner of the `<subagent_report>` a subagent sends its parent after
every turn. It also made tool permissions optional: a tool may declare no
permission, so it is always published and every call allowed, and the
`permission_name` the `tool_before_call` plugin hook receives is `null` for
such a tool. Protocol 43 replaced the `plugin { plugin }` producer owner
with `external { source, name }`: `source` is `plugin:<name>` or
`runtime:<attachment name>`, and `name` is the producer's own label, so a
plugin or runtime may run any number of named producers (see the [external
producers spec](../specs/external-producers.md)). `session.producers` returns
`sources` (the recovery state of each producer source) in place of
`plugin_recovery`, and a source that cannot recover records a
`producer_diagnostic` event (now a `diagnostic`). Protocol 44 added `plugins.list`,
`plugins.restart`, and the `plugins.changed` notification (see
[Plugins](#plugins)), and the `engine_diagnostic` event (now a `diagnostic`), which replaces the
`plugin_diagnostic` automatic compaction used to record under the name
`engine:auto-compact`. It removed the built-in `plugin` permission action:
plugin tools declare their own actions, and a rule naming `plugin` is an
ordinary label that governs nothing. Protocol 45 added runtime capabilities (see the
[runtime capabilities spec](../specs/runtime-capabilities.md)): a
`RuntimeAttachment` and a `runtime_attached` event name the `capability`
attached; `session.create` runtime entries, `session.runtime.attach`, and
`runtimes.reconnect` take an optional `capability`; `runtimes.list` returns
`runtimes` (each runtime's lifetime, connection state, and capabilities) in
place of `templates`, and its instances and `runtimes.changed` failures name
their capability; `session.create` takes the client's `cwd`; the
handshake reply carries the daemon's `machine_id`; and `runtimes.list` shows
runtimes that dial in (`dial_in`). A runtime now
produces as `runtime:<runtime ID>` rather
than `runtime:<attachment name>`. Protocol 46 removed
`SetupFieldDescriptor.safe_to_project` (every setup value is shown and
projected), added the `provider.connect` error codes
`custom_provider_not_store_backed`, and removed the connect code
`quarantined_provider` and the disconnect code `invalid_provider`, which
nothing produced. Protocol 47 added `config.reload` and the snapshot's
`config_revision` (part of `runtime_revision`); a reload publishes
`runtime.changed` with `config_reloaded`, and `agent_reloaded` when an agent
document changed. Protocol 48 added `config.get` and `config.apply`, and
`mcp.server.persist` writes through `config.apply`'s path (persisting a
runtime removal deletes the server's table). Protocol 49 changed session
history (see [Events](events.md)): it removed the legacy events
`session_reverted`, `context_rehydrated`, `plugin_event_added`,
`tool_stdin_submitted`, `delegate_finished`, and the positional
`user_input_recalled`; `user_input_recalled_v2` took the plain name
`user_input_recalled`; `runtime_attached.capability`, the `agent_md_loaded`
`attachment`, and the runtime ID of approval requests and tree grants became
required; and the legacy decoders of older logs are gone, so sessions written
before protocol 49 no longer open. It merged the event set: `diagnostic`
replaces `plugin_diagnostic`, `producer_diagnostic`, `engine_diagnostic`, and
`agent_md_skipped`; `run_finished` the four run terminal events;
`internal_agent_finished` the four internal-agent terminal events;
`delegation_progressed` `delegation_started`, `delegation_run_started`,
`delegation_run_attached`, and `delegate_queued`; `delegation_finished`
absorbs `delegate_finished_v2`; and `approval_step` and `approval_finalized`
replace the six approval lifecycle events. Every event type is a state or an
audit event, and stored records of audit events carry `"skippable": true`; a
session holding a state event this build does not know is render-only, and
requests that would change it fail with `session_requires_newer_engine`.
`session_created` carries `history_format`. Tool results lost `truncation`,
which nothing set. The input-lane events store `content` (user content: text
and file parts) instead of `input`, `user_input_transformed` stores
`original_content`, and `message_injected` and producer message bodies are
text-only user content. Older clients are rejected.

The unreleased MCP approval methods and their `pending_approval` and `rejected`
server states were removed before any release. They are not compatibility
members of protocol 49.

## Error diagnostics

`ModelErrorSummary.response_body` is an optional `DiagnosticText` that preserves
the supplied content, with terminal/Unicode format controls removed or replaced
and a maximum of 4,096 UTF-8 bytes, including any truncation marker. Newlines and
tabs are permitted. It is used for terminal,
fallback, and internal-agent model failures. A failed `run_finished` keeps its short `error`
and optionally includes `model_error` and `resolved_model`. Missing fields in old
events default to null and are omitted when serialized; this is an additive
event change without a protocol version bump.

Generic engine RPC failures include bounded, terminal-safe `data.detail` without
application-level secret-pattern or sensitive-field redaction. Clients
explicitly render selected diagnostic data such as message, reason, cause, path,
and revision information; `JsonRpcError` Debug formatting continues to redact
data. No request headers or credential dumps are added. See
[Error details](../guide/run.md#error-details) for display and upstream limits.

## Tool-emitted messages

Protocol 16 introduced optional `additional_messages` to `PersistedToolResult`,
preserved in protocol 48 alongside independent display and output references. The
field is an ordered array of at most four messages. Each message has role
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
  drives one complete session over a transport: it rejects every method before
  the exact-version handshake with error code `-32001`, correlates requests by
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

`run.start` accepts optional `reset_fallback` (default `false`). The server
normally trims a requested earlier model's suffix to the latest successfully
committed fallback in this session, matching exact model/variant identity and
agent/preset scope. Set `reset_fallback: true` for an explicit picker/model reset.
The accepted effective selection and suffix are recorded in `run_started`;
existing committed-turn events preserve progress without a new event type.
Idempotent redelivery resolves against the state preceding the original run.

| Method | Parameters | Result summary |
|---|---|---|
| `handshake` | `protocol_version`, `client` identity | Server protocol version, the connection's event `origin`, and `machine_id`, an opaque identity of the daemon's machine when it has one |
| `runtime.snapshot.get` | Empty object | One coherent runtime snapshot |
| `provider.connect` | Provider, expected catalog revision, setup/auth values, client ID | Durable connection, effective auth, snapshot, replay state |
| `provider.disconnect` | Provider, expected revisions/generation, client ID | Disconnect receipt, effective auth, snapshot, replay state |
| `config.get` | Empty object | The documents on disk (`config.toml`, `agents/<id>.md`, `agents/<preset>/<id>.md`), each with its `text` and `fingerprint`; their `revision`; `empty_presets`; and `status` |
| `config.apply` | `expected_revision`, `changes`: `{"put": {path, text, expected_fingerprint?}}`, `{"delete": {path, expected_fingerprint}}`, `{"set": {key, value}}` (one key of `config.toml` by its path of names; `null` removes it) | The new `revision` and `status`. A refusal changes nothing and is error `-32040` with data `{code: conflict \| invalid_configuration \| invalid_change, message}` |
| `config.reload` | Empty object | `status`: `applied_revision`, `restart_required` (`server`, `plugins`, `runtimes`), and `error` when the configuration on disk failed validation and was not applied |
| `session.create` | Run selection, optional `permission_mode`, optional `permission_rules` (the initial session overlay, at most 256 rules), optional `runtimes` (`[{ runtime_id, capability?, workspace? }]`), optional `global` (default `true`), optional `cwd` (the client's directory) | Session metadata with skipped-event diagnostics, and the session's `runtimes` |
| `session.list` | Optional cwd identity | Root session metadata list with skipped-event diagnostics |
| `session.get` | Session ID | Session metadata with skipped-event diagnostics, and the session's `runtimes` |
| `session.goal.get` | Session ID | Required nullable `goal: GoalState` |
| `session.goal.set` | Session ID, objective, optional `selection: RunSelection` | Activated `goal: GoalState` |
| `session.goal.lifecycle` | Session ID, goal ID, expected revision, pause/resume/cancel action, optional `selection` for resume only | Updated `goal: GoalState` |
| `session.producers` | Session ID | Runtime `producers` and `sources` (producer source recovery) inspection |
| `session.usage` | Session ID | Token/request rollup, cache hit rate, optional estimated cost, and per-model breakdown |
| `session.children` | Session ID | Direct child summaries |
| `session.tree` | Session ID | Recursive session tree |
| `session.resume` | Session ID | Resumed session metadata with skipped-event diagnostics, and the session's `runtimes` |
| `session.rename` | Session ID, client rename ID, set/clear/reset change | Session metadata and client ID |
| `session.permission.get` | Session ID | Effective permissions per action and the tree's current mode |
| `session.permission.set` | Session ID, action, optional `runtime` selector, wildcard pattern, effect | Effective permissions after the overlay change |
| `session.permission.clear` | Session ID, action, optional `runtime` selector, wildcard pattern | Effective permissions after the overlay change |
| `session.permission.preview` | Run selection, optional overlay `rules` | Effective permissions a session created with them would have; creates nothing |
| `session.runtime.attach` | Session ID, `runtime_id`, optional `capability`, `workspace` when the capability takes one | The new `attachment` and the session's `runtimes` |
| `session.runtime.detach` | Session ID, attachment `name` | The session's `runtimes` |
| `runtimes.list` | Empty object | Runtime `templates` and live `instances` with status |
| `runtimes.reconnect` | `runtime_id`, optional `capability`, `workspace` when the capability takes one | The `instance` afterwards, as `runtimes.list` reports it |
| `plugins.list` | Empty object | The configured `plugins`, in hook order |
| `plugins.restart` | Plugin `name` | Empty object, once the restart is under way |
| `session.set_permission_mode` | Any session ID in the target tree, `auto_approve`/`auto_approve_n`/`auto_approve_y`/`ask`/`yolo`; updates the runtime-only tree mode | Empty object |
| `skills.list` | Session ID | Discovered skills with source, precedence, visibility, and permission effect |
| `skills.get` | Session ID, skill name, arguments | Permission-checked rendered preview and descriptor |
| `session.compact` | Session ID, required nullable focus | Whether a checkpoint was committed |
| `session.compact.cancel` | Session ID | Whether a running manual compaction was cancelled |
| `session.revert` | Session ID, positive `through_seq`, `expected_last_seq` | Updated session metadata |
| `session.fork` | Session ID, positive `through_seq`, `client_fork_id` | New session ID |
| `run.start` | Session ID, client run ID, selection, input | Run ID |
| `run.steer` | Run ID, input | `accepted` boolean, admission `user_input_seq` |
| `session.select_model` | Session ID, selection | Whether the session's selection changed |
| `session.submit_input` | Session ID, input, `steer` or `queue` delivery, optional selection, client run ID, optional `reset_fallback` | `started` run ID, `steered` run ID and seq, `queued` seq, or `handled` reason |
| `session.recall_input` | Session ID, seq of a pending or queued input | `recalled` text, `gone`, or `not_owner` |
| `run.cancel` | Run ID | `cancelled` boolean |
| `run.tool_background` | Run ID, tool call ID | `accepted` boolean: false unless the call is an executing call of a background-capable tool; it detaches at its detach point (see [Background calls](tools.md#background-calls)) |
| `session.background.list` | Session ID | The session's background `calls` |
| `session.background.stop` | Session ID, `job` | Empty, once the call has ended; it still reports to the model, as stopped by the user. An unknown or finished job is an error |
| `events.subscribe` | Session ID, optional cursor and `cursor_timestamp`, optional `limit` | Stored events after the cursor, or `stale_cursor`; starts notifications on the final page (see [Subscriptions](events.md#subscriptions)) |
| `events.unsubscribe` | Session ID | Whether this connection had a live tail for it |
| `approval.list` | Root session ID, optional status | Approval records and tree grants |
| `approval.respond` | Approval identity/revision/fingerprint, client ID, decision, optional rejection feedback | Updated approval record |

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
`runtime_attached` and `runtime_detached` events of the session.

`session.create` attaches `global` unless `global` is `false`, then each entry
of `runtimes` (`runtime_id`, optional `capability`, optional `workspace`) in
order; without `runtimes` it attaches `local`'s `coding` capability at `cwd`,
or at the daemon's working directory without one. `cwd` is the client's
directory as it names it: the session records it as its `cwd_identity`, so
one daemon serves sessions of many directories and `session.list` with a
`cwd_identity` shows one directory's. An empty `runtimes` list creates a
session with no coding runtime. Instances that are not running are started
first, and a failure creates no session.

`session.runtime.attach` and `session.runtime.detach` are rejected while the
session has an active run (`session running`). Attach connects the runtime
and opens the capability's instance when needed, and records nothing when
that fails. Without `capability` it attaches the runtime's only capability;
a runtime that offers several asks for one, naming them. The attachment's
name is the name the instance reports (`global`, or
`<runtime ID>_<workspace basename>` for a `coding` instance), suffixed `_2`,
`_3`, ... when the session already has an attachment of that name; attaching
an instance a session already has is an error. A capability that takes a
workspace requires `workspace`, except `local`'s `coding`, which opens at the
daemon's working directory without one; a capability that takes none
refuses one. Attaching a runtime ID without a template is an
`unknown runtime` error. Besides `global` and `local`, the runtimes are the
configured [`[runtimes.<id>]`](../guide/runtimes.md) tables. The daemon passes `workspace`
to the runtime exactly as given; the attachment records the canonical path
the runtime reported, and two spellings of one workspace share an instance.

`runtimes.list` returns each runtime as a `RuntimeListing`: its `runtime_id`;
its `lifetime` (`builtin` for `global` and `local`, `spawned` for a
template, `dial_in` for a runtime that connected on its own); whether the daemon is `connected` to it now; its `capabilities`
(`name`, `kind`, `takes_workspace`, `description`), empty until it has
connected once; its `platform`; and a `message` saying why its last attempt
to connect failed. Each instance has its `runtime_id`, `capability`, reported
`attachment_name`, `kind`, `workspace`, and `platform` once started, its
`status` (`starting`, `ready`, `reconnecting`, `failed`, or `stopped`), and a
failure `message`.

`runtimes.reconnect` retries an instance now: a `failed` instance is started
again, under a new generation, and keeps its identity; an instance that is not
running is started; a `ready` or `reconnecting` one is left as it is.
`capability` and `workspace` follow the attach rule, and for a failed instance it is the
`workspace` that `runtimes.list` reports. A start that fails is an error with
the reason, and is also reported in `runtimes.changed`. Retrying `global`
also retries its failed MCP servers.

### Plugins

`plugins.list` returns every configured [plugin](../guide/plugins.md) in the
order its hooks run. A `PluginInfo` is `{ name, state, reason?, tools,
hooks, subscribe_events, subscribe_bus, publish_bus, producer_messaging,
dropped_events, stderr? }`: its `state` (`disabled`, `starting`,
`connected`, `reconnecting`, `failed`, or `stopped`), why it failed or last
disconnected, the tools and hooks it declares, what it streams and
publishes, whether its producer messaging is open, how many notifications it
dropped for falling behind, and the tail of its standard error.

`plugins.restart` starts a plugin again now: a `failed` or reconnecting
plugin is started at once, and a `connected` one is stopped and started. An
unknown or disabled plugin is an error (`plugin request rejected`). Status
changes arrive as `plugins.changed`.

### Model availability

Protocol 24 (runtime snapshot schema 6) makes unusable models visible instead
of silently dropping them. `snapshot.models` still lists only selectable
models. Each `ProviderDescriptor` additionally carries:

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

`catalog_state.age` is `current`, `older_than_seven_days`, or
`older_than_thirty_days` for the selected catalog body, alongside the existing
`stale` flag, quarantine counts, and nullable `last_error`.

## Steering

### Goal/producer integration handoff

The engine implements these methods through its per-session actor. Goal mutations
are root-only; producer inspection is available for any owned session.
Goal/producer request shapes remain unchanged. Session protocol 18 added declared
tool-output channels, terminal output
references, and independent UI display.

| Surface | Public contract | Implementation owner |
|---|---|---|
| Goal projection | `GoalState`, `GoalItem`, `GoalStatus`, `GoalId` | Engine; TUI renders durable events |
| Goal read | `SessionGoalGetParams/Result`, `get_session_goal` | Engine; TUI/clients call async API |
| Activation | `SessionGoalSetParams/Result`, `set_session_goal` | User-only root command, rejects active/paused goal |
| Lifecycle | `SessionGoalLifecycleParams/Result`, `change_session_goal_lifecycle`, `GoalLifecycleAction` | User-only root pause/resume/cancel; reject stale/terminal goal |
| Model tools | `GoalGetParams/Result`, `GoalUpdateParams/Result` | Root-only `goal_get`/`goal_update`, frozen at run admission |
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
identities and revisions; no
model tool can set an objective or issue lifecycle commands. Empty lists preserve
the active/paused lifecycle and can bootstrap an active goal without completion.
The engine decides existing tool permission mapping: `goal_get` uses `read`,
and `goal_update` uses `write`, both with resource `goal:current`. Ordinary
permission rules still apply. There is no new permission action or goal configuration.

To preserve strictly increasing event revisions, an all-finished update appends
checklist revision `r+1`, then completion revision `r+2`, and returns the final
state. Recovery repairs a missing completion event after a committed all-finished
checklist. Empty checklists never complete through this rule.

See [Events](events.md#goal-and-producer-contracts) for admission/commit
coverage and owned-message discard, and [Plugin development](../development/plugins.md#producers)
for method names, idempotency, recovery, and the no-implicit-registration requirement.

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
by its seq. Only the client whose origin admitted it may do so: another client
gets `{ "outcome": "not_owner" }`. A pending input is withdrawn with
`user_input_recalled` and a queued one with `user_input_queue_recalled`; both
return `{ "outcome": "recalled", "input" }`. An input that was already promoted,
started, or voided returns `{ "outcome": "gone" }`.

`run.start` on a session whose run is active fails with code `-32602` and
`RunStartConflict` data whose `code` is `session_running`, so a client can fall
back to `session.submit_input`.

## Revert and fork

`session.revert` is idle-only. The target must be a positive existing
sequence, and `expected_last_seq` must be the session's current last event
sequence: a session that has moved on since the client looked (another client's
turn, say) is refused with code `-32602`, message `stale revert`, and
`SessionRevertConflict` data carrying `current_last_seq`, so a revert never
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

## Notifications

| Notification | Payload |
|---|---|
| `runtime.changed` | Previous revision, complete snapshot, sorted change reasons |
| `events.subscription` | One stored event, live-only stream output, a session sequence gap, or a rewind |
| `events.plugin` | Session-scoped non-durable plugin event |
| `sessions.changed` | One session index change: `upserted`, `removed`, `approvals_changed`, `permission_mode_changed`, or `resync` |
| `mcp.changed` | MCP server configuration or status changed; optional `failures` |
| `runtimes.changed` | A runtime template was registered or an instance's status changed; optional `failures` |
| `background.changed` | A subscribed session's background calls changed: `session_id` and its current `calls` |
| `plugins.changed` | A plugin's status changed; an empty object |

A background call in `session.background.list` and `background.changed` is
`{ job, tool, tool_call_id, display?, started_at, detached_at, by }`: the job
ID, the tool's name as the model called it, the call it came from, its
display argument, when the call started and when it moved to the background,
and who moved it (`model` or `user`). The list lives in the daemon, so it
never shows a call a daemon restart killed. `background.changed` reaches only
connections subscribed to the session; one that falls behind gets every
subscribed session's list again.

### Daemon-wide changes

`runtime.changed`, `sessions.changed`, `mcp.changed`, `runtimes.changed`, and `plugins.changed` reach every connection
from its handshake on, whether or not it subscribed to any session, so each
client sharing a daemon stays current with what the others do. Clients ignore
notification methods they do not know.

`sessions.changed` carries one change, tagged by `change`:

| `change` | Fields | Sent when |
|---|---|---|
| `upserted` | `session` (`SessionMeta`) | A session, root or delegated, is created, first persisted, or reverted, or its status or title changes |
| `removed` | `session_id` | A revert deleted a delegated session |
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
`runtimes.list` and `plugins.list`.

### Connection failures

When an MCP server fails to connect, or drops and does not reconnect, and when
a runtime instance fails to start, the daemon sends one notification for it
with a `failures` list to each connection subscribed (`events.subscribe`) to a
session that uses it. Each entry names the affected `session_ids` among that
connection's subscriptions:

| Notification | `failures` entry |
|---|---|
| `mcp.changed` | `runtime_id` hosting the server (`global` for top-level `[mcp]`), `workspace` of the hosting instance when its runtime has one, `server`, `reason`, `session_ids` |
| `runtimes.changed` | `runtime_id`, `workspace` when the template takes one, `reason`, `session_ids` |

Subscribing to a session also sends the failures that already affect it, so a
client that opens a session after its servers failed still learns why their
tools are missing. Failures are live notifications, not session events; the
TUI shows each as an error row in every named session it displays.
`failures` is omitted when empty.

### MCP servers

`mcp.server.list` returns the servers of every runtime: the global runtime's
top-level `[mcp]` servers first, then those each running coding runtime
instance hosts from its template's `mcp` table. Each entry has the hosting
`runtime_id`, the hosting instance's `workspace` (absent for `global`),
`name`, `source`, `definition`, `state`, `tool_count`, `message`, and
`auth_in_progress`. `state` is `connected`, `connecting`, `reconnecting`,
`failed`, `needs_auth`, `disabled`, or `disconnected`. A `reconnecting` server
dropped and is being restarted: its tools stay published and calls fail with
the `runtime_unavailable` failure code until it reconnects or, after every
attempt failed, becomes `failed` and its tools are dropped (see
[MCP](../guide/mcp.md#connection-lifecycle)). The `mcp.server.*` mutations
and `mcp.auth.*` act on servers of the global runtime. `mcp.server.reconnect
{ name, runtime_id?, workspace? }` reconnects a server of any runtime and
waits for the attempt: without `runtime_id` (or with `global`) a top-level
server, otherwise the one the instance of `runtime_id` at `workspace` hosts,
as `mcp.server.list` names them.

The shared client maps these to `ClientDelivery` variants (`Live`, replay
deliveries, `PluginEvent`, `RuntimeChanged`, `RecoveryFailed`), so a UI
consumes one ordered stream and never parses raw JSON-RPC frames.

Protocol 23 removed the `events.tool_output_snapshot`, `events.tool_output_delta`,
and `events.tool_output_gap` notifications. Tool display comes from
`tool_call_progress` and `tool_call_terminated` events; retained output is read
through its artifact references.

Protocol 25 makes `text_delta`, `reasoning_delta`, and `tool_call_progress`
live-only: `events.subscription` delivers them as `transient` messages carrying
the durable `after_seq` they follow instead of a sequence of their own, and they
are never stored or replayed. `model_output_started` durably marks where each
streamed part began. See
[live-only stream output](events.md#live-only-stream-output).

Protocol 27 moves the MCP OAuth callback to the client, so a daemon reached
through a tunnel or on another machine can still be authorized from the
client's browser. `mcp.auth.begin` requires `redirect_uri`, the client's own
loopback callback (`http://127.0.0.1:<port>/<path>` or `http://[::1]:<port>/<path>`
with no query, fragment, or credentials). The client then sends the browser's
redirect to the new `mcp.auth.complete { server, callback_url }`; the daemon
keeps the PKCE verifier, exchanges the code, and stores the tokens, which never
cross the connection. A `callback_url` that is not a redirect to the pending
`redirect_uri` with a `code` or `error` parameter is rejected and leaves the
flow waiting. Any other completion ends it. A `begin` with the pending
`redirect_uri` returns the same authorization URL; one with a different
`redirect_uri` replaces the pending flow. Protocol 26 shipped only in a nightly
build and is not reused.

Protocol 30 lets a client configure permissions before a session exists.
`session.create` accepts an optional `permission_mode` for the new tree and
optional `permission_rules`, which are validated like any overlay (no duplicate
action and pattern, at most 256) before the session is created and committed as
its first `session_permission_overlay_set` event, so the first run already sees
them. `session.permission.preview { selection, rules }` returns the effective
permissions such a session would have, computed from the selection's frozen
agent policy exactly as `session.permission.get` computes them for a live
session.

Protocol 31 adds background jobs. `ProducerOwner` gains `tool { tool_call_id }`
for the completion report of a job a tool call started (a `bash` call with
`background: true`), and `producer_message_accepted` gains an optional
`retained_output` naming the job's captured output artifacts. Older clients
cannot decode the new owner variant, so the version moves.

Protocol 32 stops storing tool output that fits its preview anywhere but the
result output. `RetainedToolStream.reference` and `sha256` become optional and
are absent for such a stream, and `RetainedToolOutput.reference` is absent when
no stream needed an artifact. `RetainedToolStream` also gains `dropped_bytes`,
present when output past the 128 MiB capture limit was not retained. Older
clients require the now-optional fields, so the version moves.

Protocol 33 stops sending a tool result's display twice. When `display` occurs
verbatim in `output`, `PersistedToolResult` carries `display_span`, the
`[start, end)` byte range of `output`, in its place. Older clients reject the
unknown field, so the version moves.

Session metadata includes additive `skipped_events` entries with the physical
sequence (or source line number when no sequence was readable) and a safe reason.
An empty array means the event log loaded without skipped records. Optional-field
degradations remain available to the engine as load diagnostics but are not
reported as skipped events.

See [Events](events.md) for event payloads and durability semantics.

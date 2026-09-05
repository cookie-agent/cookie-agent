# Protocol Reference

The daemon exposes JSON-RPC 2.0 over an authenticated WebSocket at `/ws`.
Protocol 16 is current-only. A client must call `handshake` with
`{ "protocol_version": 16 }` before any other method.

The unreleased MCP approval methods and their `pending_approval` and `rejected`
server states were removed before any release. They are not compatibility
members of protocol 16.

## Tool-emitted messages

Protocol 16 adds optional `additional_messages` to `PersistedToolResult`. The
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
  `server` crate provides `WebSocketTransport` and `InProcessStream`
  implementations; the daemon's axum accept path implements the same trait
  server-side.
- **ServerProtocol.** The server contract is one async method per RPC plus
  `connected`. The `server` crate implements it over `Engine`. `protocol::serve`
  drives one complete session over a transport: it rejects every method before
  the exact-version handshake with error code `-32001`, correlates requests by
  id, dispatches to the implementation, and delivers notifications through a
  `ServerContext`.
- **ClientProtocol.** The client contract is implemented by the shared
  `protocol::Client`. Its connection task correlates requests by id, demuxes
  notifications into an ordered `ClientDelivery` stream, injects cursor replays
  and gap recovery before buffered live notifications, wipes secret-bearing
  serialized frames, and fails outstanding calls on shutdown.
- **TUI and CLI.** The TUI client is a thin adapter re-exporting the server's
  `Client` wrapper; the CLI uses the same client through the `ClientProtocol`
  trait. Both still work without the `tui` feature.

## Methods

| Method | Parameters | Result summary |
|---|---|---|
| `handshake` | `protocol_version` | Server protocol version |
| `runtime.snapshot.get` | Empty object | One coherent runtime snapshot |
| `provider.connect` | Provider, expected catalog revision, setup/auth values, client ID | Durable connection, effective auth, snapshot, replay state |
| `provider.disconnect` | Provider, expected revisions/generation, client ID | Disconnect receipt, effective auth, snapshot, replay state |
| `session.create` | Run selection | Session metadata with skipped-event diagnostics |
| `session.list` | Optional cwd identity | Session metadata list with skipped-event diagnostics |
| `session.get` | Session ID | Session metadata with skipped-event diagnostics |
| `session.goal.get` | Session ID | Required nullable `goal: GoalState` |
| `session.goal.set` | Session ID, objective, optional `selection: RunSelection` | Activated `goal: GoalState` |
| `session.goal.lifecycle` | Session ID, goal ID, expected revision, pause/resume/cancel action, optional `selection` for resume only | Updated `goal: GoalState` |
| `session.producers` | Session ID | Runtime `producers` and `plugin_recovery` inspection |
| `session.usage` | Session ID | Token/request rollup, cache hit rate, optional estimated cost, and per-model breakdown |
| `agent.usage` | Agent ID | Rollup across turns attributed to that agent |
| `usage.global` | Empty object | Rollup across all project sessions |
| `session.children` | Session ID | Direct child summaries |
| `session.tree` | Session ID | Recursive session tree |
| `session.resume` | Session ID | Resumed session metadata with skipped-event diagnostics |
| `session.rename` | Session ID, client rename ID, set/clear/reset change | Session metadata and client ID |
| `session.set_permission_mode` | Any session ID in the target tree, `auto_approve`/`auto_approve_n`/`auto_approve_y`/`ask`/`yolo`; updates the runtime-only tree mode | Empty object |
| `skills.list` | Session ID | Discovered skills with source, precedence, visibility, and permission effect |
| `skills.get` | Session ID, skill name, arguments | Permission-checked rendered preview and descriptor |
| `session.compact` | Session ID, required nullable focus | Whether a checkpoint was committed |
| `session.revert` | Session ID, positive `through_seq` | Updated session metadata |
| `session.fork` | Session ID, positive `through_seq` | New session ID |
| `run.start` | Session ID, client run ID, selection, input | Run ID |
| `run.steer` | Run ID, input | `accepted` boolean |
| `run.recall_steer` | Run ID | Required nullable recalled text |
| `run.cancel` | Run ID | `cancelled` boolean |
| `run.tool_stdin` | Run ID, tool call ID, optional data, EOF flag | `accepted` boolean |
| `events.subscribe` | Session ID, optional cursor | Initial stored events; starts notifications |
| `approval.list` | Root session ID, optional status | Approval records and tree grants |
| `approval.respond` | Approval identity/revision/fingerprint, client ID, decision, optional rejection feedback | Updated approval record |

Model, agent, and catalog discovery uses `runtime.snapshot.get`. Skills use the
session-aware `skills.list` and `skills.get` methods because visibility depends
on the governing run policy and session overlay.

## Steering

### Goal/producer integration handoff

The engine implements these methods through its per-session actor. Goal mutations
are root-only; producer inspection is available for any owned session.
The session wire remains 16: these additive methods do not alter the
handshake or existing required request/result fields. The independently versioned
plugin extension advances to `0.0.5`; package versions are unchanged.

| Surface | Public contract | Implementation owner |
|---|---|---|
| Goal projection | `GoalState`, `GoalItem`, `GoalStatus`, `GoalId` | Engine; TUI renders durable events |
| Goal read | `SessionGoalGetParams/Result`, `get_session_goal` | Engine; TUI/clients call async API |
| Activation | `SessionGoalSetParams/Result`, `set_session_goal` | User-only root command, rejects active/paused goal |
| Lifecycle | `SessionGoalLifecycleParams/Result`, `change_session_goal_lifecycle`, `GoalLifecycleAction` | User-only root pause/resume/cancel; reject stale/terminal goal |
| Model tools | `GoalGetParams/Result`, `GoalUpdateParams/Result` | Root-only `goal_get`/`goal_update`, frozen at run admission |
| Runtime inspection | `SessionProducersParams/Result`, `session_producers`, `ProducerRegistration`, `PluginRecoveryState` | Session runtime snapshot, not global catalog snapshot or persistence |
| Plugin messaging | `ExtensionProducer{Register,Send,Discard,Unregister}Params/Result` | Engine request handlers; SDK mirrors register/send/discard/unregister |
| Recovery handshake | `ExtensionRecoveryStartParams`, `ExtensionRecoveryCompleteParams/Result`, `ExtensionRecoveryOutcome` | `plugin/recovery/start` engine notification (no deadline/response); plugin completion request |

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
coverage and owned-message discard, and [Plugins](../development/plugins.md#producer-messaging-contracts)
for method names, idempotency, recovery, and the no-implicit-registration requirement.

### User steering

`run.steer` requires an active target run. It immediately appends
`user_input_admitted` and returns `{ "accepted": true }`; admission does not
change model history. At each completed tool batch and no-tool completion
boundary, all pending inputs are promoted FIFO as separate
`user_input_submitted` events before the next request. A no-tool boundary with
no pending input completes the run.

`run.recall_steer` removes the newest pending input (LIFO), appends
`user_input_recalled`, and returns its text. It returns `{ "recalled": null }`
without an event if the lane is empty. Terminal run events void anything still
pending.

## Revert and fork

`session.revert` is idle-only and appends a `session_reverted` marker. The target
must be a positive existing physical sequence. The physical log remains
append-only; branch-derived transcript, context, title, usage, and approvals use
the visible prefix plus events after the marker.

`session.fork` may read an active source but requires a persisted prefix that
contains a submitted user message. The fork copies that prefix exactly under a
new session ID, closes any copied in-flight run locally, appends ` (fork)` to
the title, and continues with new physical sequences.

## Notifications

| Notification | Payload |
|---|---|
| `runtime.changed` | Previous revision, complete snapshot, sorted change reasons |
| `events.subscription` | One stored event or a session sequence gap |
| `events.plugin` | Session-scoped non-durable plugin event |
| `events.tool_output_snapshot` | Stream and retained output snapshot |
| `events.tool_output_delta` | Tool call, stream, byte offset, data |
| `events.tool_output_gap` | Tool call, stream, next available offset |

The shared client maps these to `ClientDelivery` variants (`Live`, replay
deliveries, output stream events, `RuntimeChanged`, `RecoveryFailed`), so a UI
consumes one ordered stream and never parses raw JSON-RPC frames.

Session metadata includes additive `skipped_events` entries with the physical
sequence (or source line number when no sequence was readable) and a safe reason.
An empty array means the event log loaded without skipped records. Optional-field
degradations remain available to the engine as load diagnostics but are not
reported as skipped events.

See [Events](events.md) for event payloads and durability semantics.

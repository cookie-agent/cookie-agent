# Protocol Reference

The daemon exposes JSON-RPC 2.0 over an authenticated WebSocket at `/ws`.
Protocol 12 is current-only. A client must call `handshake` with
`{ "protocol_version": 12 }` before any other method.

The unreleased MCP approval methods and their `pending_approval` and `rejected`
server states were removed before any release. They are not compatibility
members of protocol 12.

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
| `session.usage` | Session ID | Token/request rollup, cache hit rate, optional estimated cost, and per-model breakdown |
| `agent.usage` | Agent ID | Rollup across turns attributed to that agent |
| `usage.global` | Empty object | Rollup across all project sessions |
| `session.children` | Session ID | Direct child summaries |
| `session.tree` | Session ID | Recursive session tree |
| `session.resume` | Session ID | Resumed session metadata with skipped-event diagnostics |
| `session.rename` | Session ID, client rename ID, set/clear/reset change | Session metadata and client ID |
| `session.set_permission_mode` | Session ID, `auto_approve`/`auto_approve_n`/`auto_approve_y`/`ask`/`yolo` | Empty object |
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

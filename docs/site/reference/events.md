# Event Reference

A session's history is `events.jsonl`: one JSON record per line, appended in
order. Subscriptions deliver the same records. The format follows the
[compatibility specification](../specs/compatibility-v1.md#8-session-history):
events and fields are added without a version bump, and only a change that
cannot be made additively bumps the history format.

## Records

```json
{ "engine_version": "0.5.0", "origin": "user", "session_id": "…",
  "run_id": null, "seq": 1, "timestamp": "…", "skippable": true,
  "payload": { "type": "…" } }
```

| Member | Meaning |
|---|---|
| `engine_version` | The writing engine's binary semver: diagnostic metadata, not a compatibility gate |
| `origin` | Who authored this record (below) |
| `session_id` | The session |
| `run_id` | Required and nullable: the run a run-scoped event belongs to, `null` for a runless one |
| `seq` | A positive physical sequence, strictly increasing down the file |
| `timestamp` | When the record was written |
| `skippable` | Present and `true` exactly on [audit events](#state-and-audit-events) |
| `payload` | The event, tagged by `type` |

`origin` is either `user` or `<class>:<slug>`, where `class` is `engine`,
`plugin`, or `client` and `slug` matches `[a-z0-9][a-z0-9-]{0,63}`. It
identifies the author of that individual event, not the component described
by its payload. For example, a diagnostic about a plugin can have origin
`engine:plugin-host`. Events caused by a client request carry that
connection's handshake origin, `client:<kind>-<instance id>` (see
[the handshake](protocol.md)); the headless CLI's in-process runs use
`client:headless`. A run cancelled through `run.cancel` records the caller's
origin on its `run_finished`, and the engine's origin when it cancels on its
own. When tool-emitted additional messages are materialized as persisted
turns, their origin is `engine:tool-result`: the engine authors the durable
turn from a validated tool result, and the tool name remains payload context.

The first record is always `session_created`. It carries `history_format`
(currently `1`, the `HISTORY_FORMAT` constant of `cookie_agent_protocol`),
and the optional `short_id`, the tree-unique model-facing handle.

## Reading a log

Writers are strict: every appended record passes validation, including a
`skippable` that agrees with its payload's class. Readers handle each line on
its own:

- A fully readable record loads normally.
- A known event with an absent or type-mismatched optional field loads with
  that field defaulted and an in-memory degradation diagnostic.
- A record with a broken required field, corrupt JSON, an unknown envelope
  member, or a failed validation is skipped with a diagnostic.
- **An event type this build does not know** is skipped silently when its
  record says `skippable`. Without `skippable` it is a state event from a
  newer build: the record is skipped with a diagnostic naming its type, and
  the session is **render-only**. It opens and can be viewed and subscribed,
  but nothing appends to it: `run.start`, `run.steer`, `session.submit_input`,
  revert, fork, compaction (manual or automatic), title generation, and every
  other change fail with error code `session_requires_newer_engine`, and
  adopting it repairs nothing.
- Sequence numbers must increase strictly. Gaps are retained and reported,
  one bounded diagnostic per contiguous missing range.

The first record decides whether the log opens at all. Without a readable
`session_created` the session cannot open: the engine reports an
unsupported-era or corrupt-log error. A `session_created` without
`history_format` was written before history formats and is not read. A
higher `history_format` than this build's fails with
`session_requires_newer_engine`; a lower one has no conversion yet and fails
as unsupported.

CI compares the generated `EventPayload` schema with the committed baseline in
`crates/protocol/compat/events/` (see [Schemas](schemas.md)). A new optional
field must be safe to ignore, since its absence means the old behaviour; a
field whose absence would be misread needs a new event type; and a new value
of an open enum needs a fallback that is safe for older readers.

Terminal tool-result truncation and `tool_output_elided` references use the
[retained-output readback lifecycle](tools.md#retained-tool-output).

## State and audit events

Every event type is a **state** event or an **audit** event. A state event is
read by the projection, model history, delegation recovery, goals, producers,
approvals, or usage; an audit event changes none of them, so an older reader
may skip it. Writers stamp `skippable` on audit events. Live-only events are
never stored.

| Event | Class | Run scope | Records |
|---|---|---|---|
| `session_created` | state | runless | The session's creation: origin, handle, workspace, selection, agent, revisions, `history_format` |
| `session_permission_overlay_set` | state | runless | The session's permission overlay |
| `runtime_attached`, `runtime_detached` | state | runless | The session's [runtime attachments](#runtime-attachments) |
| `skill_loaded` | state | either | A loaded skill body |
| `skill_invocation_noted` | audit | either | A repeat of an identical skill load |
| `session_title_committed` | state | either | A title change |
| `agent_md_loaded` | state | run | The `AGENTS.md` context of one attachment |
| `diagnostic` | audit | either | A notice about the engine, a plugin, or a runtime |
| `goal_activated`, `goal_checklist_revised`, `goal_lifecycle_changed` | state | runless or run | The session goal |
| `producer_message_accepted`, `producer_message_discarded` | state | runless or run | A producer message's body; its withdrawal |
| `producer_message_admitted`, `producer_messages_claimed`, `producer_messages_released`, `producer_message_consumed` | state | run | A producer message's delivery |
| `message_injected` | state | run | A message `agent_before_start` injected |
| `user_input_admitted` | state | run or runless | A steer waiting for the next turn |
| `user_input_submitted` | state | run | Input that became model history |
| `user_input_transformed` | audit | run | A `user_before_input` rewrite of direct input |
| `user_input_recalled` | state | run or runless | A withdrawn admission |
| `user_input_applied` | state | run | Where an admitted input took effect |
| `user_input_queued`, `user_input_queue_recalled`, `user_input_queue_voided` | state | runless | The [queued-input lane](#the-queued-input-lane) |
| `session_model_selected` | state | runless | The session's model selection |
| `delegated_context_seeded` | state | runless | A child's inherited context |
| `run_started`, `run_finished` | state | run | A run's start and end |
| `model_attempt_started`, `attempt_abandoned`, `model_fallback` | state | run | A model attempt's lifecycle |
| `model_request_prepared`, `model_tools_published`, `model_output_started`, `model_replay_evaluated` | audit | run | Request and streaming bookkeeping |
| `model_turn_committed`, `model_usage_recorded` | state | run | A committed turn and its usage |
| `tool_call_started`, `tool_call_terminated`, `tool_output_elided`, `tool_call_linked` | state | run | A tool call's lifecycle |
| `delegation_reserved`, `delegation_progressed`, `delegation_finished` | state | run | A [delegation](#delegation) on the parent |
| `delegate_child_terminated` | state | runless | A delegation ended the child before it ran |
| `approval_requested`, `approval_finalized`, `tree_approval_grant_committed` | state | run | An [approval](#approvals) |
| `approval_step` | state; audit for `doom_loop` | run | An approval step |
| `internal_agent_started`, `internal_agent_usage_recorded`, `internal_agent_finished`, `internal_agent_fallback` | state | run | An internal agent invocation |
| `context_checkpoint_committed` | state | run | A [compaction checkpoint](#compaction-checkpoints) |
| `text_delta`, `reasoning_delta`, `internal_agent_text_delta`, `native_compaction_started`, `native_compaction_finished`, `tool_call_progress` | live-only | run | [Stream output](#live-only-stream-output) |

## Live-only stream output

`text_delta`, `reasoning_delta`, `internal_agent_text_delta`,
`native_compaction_started`, `native_compaction_finished`, and
`tool_call_progress` are live-only. The engine delivers them to event
subscribers and plugins as they stream but never appends them to
`events.jsonl` or to a session's in-memory log, and a stored record carrying
one fails validation. Their durable record is the committed turn
(`model_turn_committed`), the tool termination (`tool_call_terminated`, with its
retained output), and the internal agent's terminal event. A replay or a reload
therefore sees finished turns only.

A view that attaches while output streams does not start from a fragment: the
daemon keeps each session's output still in flight in memory, and a tail
registered by a subscription without a cursor (one that replayed the whole log)
first receives it as live-only messages positioned after the durable tip it
replayed to. A subscription that resumes from a cursor does not, since its
reader may already hold part of that output; its later deltas continue it. Each open attempt's text and reasoning arrive as
one `text_delta` or `reasoning_delta` per uninterrupted run of a kind, in
order; each running tool as one `tool_call_progress` carrying its latest
`message` and the display so far (capped at 64 KiB); a running provider-native
compaction as its `native_compaction_started`; and a streaming compaction
summary as one `internal_agent_text_delta`. Later deltas append as usual, and
the durable record replaces it all as before. Output a daemon restart cut off
is not kept.

`internal_agent_text_delta` (`invocation_id`, `text`) streams the compaction
agent's summary text. Other internal agents do not stream.
`native_compaction_started` and `native_compaction_finished` (`resolved_model`)
bracket a provider-native compaction call, whether or not it succeeds; a
committed `context_checkpoint_committed` is the durable record of a success.

`model_output_started` (`attempt_id`, `kind`: `text` or `reasoning`) is the
durable trace of streaming. The engine appends it before the first delta of each
streamed part: an attempt's first output and every switch between text and
reasoning. Views use it to keep an attempt that is already streaming ahead of
inputs admitted meanwhile, and to time thinking identically live and on replay
(each reasoning part runs from its mark to the next mark or the commit).

A stored record of a live-only payload fails validation, so the reader skips it
with a diagnostic like any other invalid record.

## Goal and producer contracts

Goal and producer state is projected independently from model summaries. The engine
serializes mutations, message acceptance, and admission through the session actor.
Goal activation and producer acceptance persist an otherwise buffered session,
even before its first model request.

| Event | Payload fields | Run scope |
|---|---|---|
| `goal_activated` | `goal_id`, `objective`, `revision` | Runless allowed |
| `goal_checklist_revised` | `goal_id`, `items`, `revision` | Runless allowed |
| `goal_lifecycle_changed` | `goal_id`, `status`, `revision` | Runless allowed |
| `producer_message_accepted` | `message_id`, `producer_owner`, `mode`, `idempotency_key`, `description`, `body`; optional `reminder`, `retained_output` | Runless allowed, including during an active run |
| `producer_message_admitted` | `message_id` | Requires the destination run |
| `producer_messages_claimed` | Nonempty `message_ids` | Requires the admission run; envelope `seq` identifies the claim |
| `producer_messages_released` | Positive `claim_seq` | Requires the claimed run |
| `producer_message_consumed` | `message_id`, `run_id` | Requires matching envelope `run_id` |
| `producer_message_discarded` | `message_id`; optional `producer_owner`, optional `reminder` | Runless allowed |

`GoalId` and `ProducerMessageId` are durable UUID identities. `GoalItem` contains
only `description` and `finished`, with no item ID. Writer validation rejects blank
objectives/descriptions. Revision is an engine-owned `u64`, including zero;
strictly increasing revisions, overflow rejection, matching goal identity, and
lifecycle transitions are enforced by the engine projection. Model updates replace
the entire ordered checklist through the session actor: the last accepted update
wins for the current/latest session goal at actor acceptance, without a
model-supplied `goal_id`, `expected_revision`, or lost-update protection. An older
run's update can intentionally affect a newly activated active or paused goal;
updates reject if the current goal is absent or terminal. The engine records the
selected goal's ID and revision in durable events. User lifecycle RPC controls
retain `SessionGoalLifecycleParams.goal_id` and `expected_revision`, including
stale-identity and stale-revision rejection. Empty checklist revisions
preserve the active/paused lifecycle. Completion requires nonempty all-finished
items, including while paused.

A stored checklist item carrying an `id` does not decode.

`ProducerOwner` is tagged by `type`: `external { source, name }` (a plugin's
or a runtime's named producer),
`delegation { invocation_id }`, `goal { goal_id }`,
`goal_control { goal_id }`, `agent { session_id }`, `tool { tool_call_id }`, or
`subagent { session_id }` (a child's `<subagent_report>`, keyed by its run ID). It identifies a specific
stable owner, not a volatile registration or merely an owner kind. Accepted events
are the sole authoritative message body/mode storage, including deferred queue
sends while a run is active. Registration records are never session events.
`GoalControl { goal_id }` owns engine-authored pause/cancel steering, not goal
continuation reminders. These accepted messages survive goal-controller teardown
and reminder invalidation. Accepted agent-owned messages may carry optional
`agent_hop` metadata for the messaging loop guard. It is absent on non-agent
producers and legacy agent mail, is not rendered in the agent-visible envelope,
and does not participate in idempotency comparison.
`Tool { tool_call_id }` owns the single `<background_result>` of that tool
call once it ran in the background. Its accepted message carries the call's
captured output as `retained_output`, the same shape a tool result uses, and the body
previews it the way a tool result does. Recording the reference on the event is what keeps
those artifacts alive; plugins cannot supply `retained_output`.

The optional accepted-event
`reminder: GoalReminderIdentity { goal_id, revision, kind }` identifies a goal
reminder; if present, its goal must match the goal owner. `kind` is the required
`started` or `continuation`. Its
body must contain the full objective and checklist, including finished items and
revision. Each continuation attempt has a fresh message ID/idempotency key even
at an unchanged revision. A consumed reminder cannot suppress later continuation.
New goals use `started` until a same-goal reminder is covered by committed model
input; acceptance, claims, discarded startup messages, and lifecycle-control
notifications alone do not switch subsequent reminders to `continuation`.

Producer descriptions are compact, human-facing text; the full `body` remains
model-facing. Legacy history without a description defaults to empty, allowing
clients to fall back to a truncated body.

`producer_message_admitted` links the accepted body to a run without duplicating
user-input storage or changing existing input events. Its physical sequence is
covered by `model_turn_committed.input_through_seq`. Admission is not consumption:
append `producer_message_consumed` only after a committed turn covers the admission
in that same run. Recovery repairs missing consumption markers from committed
coverage; an uncommitted API attempt remains retryable. Queue acceptance does not
create a forbidden runless pending user input during an active run.

`producer_messages_claimed` durably reserves admitted messages at the actor
boundary before request preparation/dispatch. The claim's envelope sequence is
its token, not evidence that the provider received input. Projection requires
unique accepted, admitted, unconsumed, undiscarded message IDs belonging to the
claim's run. Claims cover compaction, hooks, streaming, and model commit.
`producer_messages_released` references the claim sequence in the same run and
removes that claim. Failed or cancelled attempts release their reservation;
recovery also releases surviving claims. Once all claims are released, an
unconsumed, undiscarded message returns to waiting and the queue strip. Consumed
messages remain consumed after release. Claim/release is not consumption and
does not establish exactly-once model execution.

Discard is separate from consumption and is a generic durable producer-message
event, not restricted to goal reminders. `discard(session, message_id)` checks
the caller's ownership against the accepted message, including after its producer
unregisters. Owned already-discarded retries are idempotent; cross-owner discard
is rejected. Only unclaimed waiting messages qualify, including admitted messages
not yet claimed. Consumed or claimed messages reject discard, even before actual
provider dispatch; absence of a consumption marker is not proof that a message
is retractable. The actor serializes discard against claim acquisition.
Durable discard removes the message from pending
delivery/readiness and the queue strip, and replay must not restore it as pending.
Goal reminder invalidation still checks its identity; it does not authorize
discarding another owner's real message.

Projection requires a supplied `producer_owner` to exactly match the accepted
owner; a supplied `reminder` must also match. Historical reminder-only discards
remain readable: when owner is absent, a matching reminder identity is required.
An absent owner is not generic discard authority, and a wrong supplied owner
cannot fall back to legacy reminder matching. Both forms reject consumed or
claimed messages. Claimed messages are hidden from the waiting strip until
release makes them eligible again; in-flight input cannot be retracted.

Missing/corrupt optional reminder metadata follows the existing best-effort
reader, with degradation diagnostics. Engine projection must quarantine invalid
goal-owned reminder records rather than reclassifying them as real sends.
Cross-event ownership, at-most-once consumption, and commit coverage are engine
invariants, not properties a single-event decoder can verify.

Goal state and unconsumed messages must survive compaction. Fork copies durable
branch data, never registrations; revert reconciles only the visible branch.
Failed/disabled recovery notices are runless `diagnostic` events whose `source`
is the producer source and whose `kind` is `recovery_failed` or
`recovery_disabled`, not persisted readiness/registration records.

## Event details

`model_replay_evaluated` records, for one model attempt, the replay decision
for each history entry of its request. Every attempt re-evaluates the whole
history, so a long run would otherwise write a list that grows with every
turn. When `base_attempt_id` is present, the event names an earlier attempt of
the same run, with the same `resolved_model`, whose full list is an exact prefix
of this attempt's list; `ordered_decisions` then holds only the decisions after
that prefix. The full list is the base's full list followed by
`ordered_decisions`. Without `base_attempt_id`, `ordered_decisions` is the full
list, which is what a run's first evaluation writes, as does any attempt whose
model changed or whose earlier decisions changed (compaction, a pinned turn,
a late tool result). Readers that ignore the field see only new decisions for
such an attempt.

`agent_md_loaded` is a run-scoped root-session event stamped
`engine:agent-md`. It contains one or two `AGENTS.md` entries a coding runtime
discovered in its workspace, each with a bounded display source, retained
content, truncation flag, and original byte size, and the `attachment` naming
that runtime's attachment. A run writes one event per attachment that
returned a file. Missing or disabled context produces no event. History uses
only the latest run's events, the latest per attachment, and replays their
entries as one provenance-delimited user turn. Compaction pins that turn
alongside loaded skill bodies. See
[System Prompt Composition](system-prompt.md#agentsmd-context-turn).

`diagnostic { source, kind, message, count?, detail? }` is a notice about the
engine, a plugin, or a runtime: something it turned off, could not do, or
dropped. `source` is `engine:<area>`, `plugin:<name>`, or `runtime:<id>`;
`kind` is a snake-case label (at most 64 bytes) that readers show whatever it
is; `message` is the nonempty text to show; `count` (at least 2 when present)
says how many occurrences the record coalesces; and `detail` is display-only
JSON of at most 16 KiB. Nothing but a view reads it. A diagnostic about a run
belongs to that run; any other is runless. The kinds written today:

| Source | Kind | Meaning |
|---|---|---|
| `plugin:<name>` | `interception_timeout`, `interception_crash` (the plugin was not connected or exited during the hook), `interception_rejected` (it answered with an error or a malformed result), `hook_blocked`, `invalid_modification`, `oversized_event`, `event_drop`, `context_mismatch`, `rate_limited` | A plugin's operational notice, coalesced by `count` |
| `plugin:<name>`, `runtime:<id>` | `recovery_failed`, `recovery_disabled` | A producer source cannot restore the external work it tracks; goal continuation is held |
| `engine:auto-compact` | `auto_compaction_disabled` | Automatic context compaction failed three times in a row and stays off for the rest of the run |
| `engine:agent-md` | `agent_md_too_large` | An `AGENTS.md` file is over the size limit and contributes no context; run-scoped, with `detail` `{ path, byte_length, attachment }` |


### Delegation

`delegated_context_seeded` is a runless creation event containing the bounded,
text-only user/assistant turns copied by `inherit_context`; it must precede the
child's first run and deterministically rebuilds the same initial model history
after restart.

The `delegation_*` records are durable control events on the parent session,
in the parent run. `delegation_reserved` contains the complete current delegate
request, the child agent snapshot (whose selected suffix is the child's
models), one cache strategy per entry of that suffix, its runtime revisions
when they changed (see [Runs and internal agents](#runs-and-internal-agents)),
the request fingerprint, and optional typed staged-skill provenance. The child
records its attachments, the root session's, with `runtime_attached` events at
creation. `delegation_progressed { invocation_id, stage }` records how far the
delegation got; `stage` is `started { child_session_id }` (the child session
was published), `run_started { child_run_id }` or `run_attached { child_run_id }`
(a new child run, or attachment to an already-running resumed child), or
`queued { position? }` (the child waits for a background slot). A stage this
build does not know changes nothing.

`delegation_finished { invocation_id, child_session_id, child_run_id?, status,
reason?, short_id?, preview, total_lines }` records the terminal status, the
optional recovery reason, and the child references, together with the teaser
transcripts render: `short_id` is the child's handle and `preview` the report as
[`[subagent_output]`](../engine/subagent_output.md) previews it, whole or its
first and last lines around an omission marker (at most 64 KiB; empty when no
child run started). One record per invocation lets repeated resumes of one
child each finish once. It is not model history: a foreground delegation's
result reaches the parent's model as its tool result, and a background one's as
the steer producer message the engine durably accepts for it (see
[Goal and producer contracts](#goal-and-producer-contracts)), so the result is
in history exactly once. Full output remains available through
`get_subagent_result`.

`delegate_child_terminated` is written runless to the *child* session when a
delegation ends it before it ran (cancelled or failed); it sets the child's own
status.

Engine startup projects delegation state while opening session logs. A reserved
or started delegation without a terminal event resumes or rolls back according
to its child and parent state. Recovery trusts the stored request fingerprint
and never computes it again; it is compared only when the same invocation is
reserved again. If best-effort reading skips a corrupt
delegation event, that delegation is not recovered, the parent session records
the diagnostic, and unrelated delegations remain recoverable.

### Runs and internal agents

`run_started` freezes the run's agent snapshot. The snapshot's
`fallback_chain` lists the agent's models in fallback order, and
`selected_suffix_start` is where the run starts in it: the run's model is
that entry (its exact selection, variant included, equals the run
selection's), and its fallbacks are the entries after it. `session_created`
and `delegation_reserved` carry snapshots of the same shape, and the snapshot
has no schema version.

`session_created` records the seven revisions of the runtime the session was
created against as `revisions` (`runtime_revision`, `catalog_revision`,
`provider_state_revision`, `model_revision`, `agent_revision`,
`recipe_registry_revision`, and `manifest_revision`). `run_started` and
`delegation_reserved` carry `revisions` only when the run or the delegated
child was frozen against different ones than the latest the log recorded
before it; without the field, an event was frozen against those latest
revisions. Readers fold them in log order.

Each chain entry, like a model fallback of a frozen internal agent, is a
stored model binding `{ selection, provider_id, provider_model_id, adapter,
resolved, selection_fingerprint, blueprint_fingerprint, frozen }`. `adapter`
is the protocol adapter the binding speaks (such as `oven.openai.responses`),
and `resolved` is the model reference the run's attempts record:
`model_attempt_started` and `model_fallback` must name the `resolved` of the
entry at their fallback index, counted from `selected_suffix_start`.
`frozen` holds the complete binding (descriptor, request defaults, provider
options, credential and setup bindings, static headers) as an opaque value.
Loading a log never decodes it. The engine decodes it only to run the
binding again: manual compaction, title generation, and the history view of
the latest run, and the model fallbacks of that run's internal agents. When
this build cannot read it, that operation fails with
`unreadable_model_binding`; the session still opens, and a new run resolves
its models afresh as always.

`run_finished { outcome }` ends a run. `outcome` is `completed { final_text? }`,
`failed { error, model_error?, resolved_model? }`, `cancelled { reason? }`, or
`interrupted { reason? }`; an outcome this build does not know reads as
interrupted. `internal_agent_finished { invocation_id, internal_run_id, kind,
outcome }` ends an internal agent invocation the same way, with
`completed { result }`, `failed { failure }`, `cancelled { reason? }`, or
`interrupted { reason? }`. `run_started.internal_agents` freezes the run's
internal agent definitions, one per kind, and an
`internal_agent_usage_recorded` of the run names the agent frozen for its
kind.

### Approvals

`approval_requested` stores the request's `operation_fingerprint`, which the
writer computes from the prepared operation and readers trust as written.
An approval's lifecycle after its `approval_requested` is a series of
`approval_step { approval_id, step }` records and one
`approval_finalized { approval_id, outcome }`. `step` is
`evaluated { decision }` (the internal decision pipeline decided),
`escalated { reason_code }` (it waits for the user),
`user_decision { client_response_id, decision, feedback? }` (a client answered;
`feedback` only with `reject`), or `doom_loop { operation_fingerprint,
repetitions }` (the same operation was requested at least four times in a row);
a step this build does not know changes nothing. `outcome` is
`decided { decision }`, the final decision of policy, a grant, the reviewer, or
the user, or `cancelled { reason_code }`, the engine withdrawing the approval
(`request_cancelled`, `approval_expired`, `operation_changed`, or
`prepared_capability_lost`), which reads as a system decision that expired or
cancelled the request. An outcome this build does not know reads as cancelled,
so the operation is not approved.

### Runtime attachments

The runless `runtime_attached` and `runtime_detached` events record the
session's [runtime attachments](../specs/runtimes.md#5-session-attachments).
`runtime_attached` carries the session-unique attachment `name`, the
`runtime_id`, the `capability` attached, the `kind` label the capability
declared (such as `global` or `coding`), and, when the instance has one, the
canonical `workspace` it reported.
`runtime_detached` carries the `name` it removes. The attachment set is the fold
of these events in log order; a session's creation writes one
`runtime_attached` per initial attachment together with `session_created`, so
both reach disk with the session's first persistence. A session created with
no attachment records no attachment event. Delegated children record their own
attachments.

### Other events

The runless `session_permission_overlay_set` event's `overlay`
contains the complete, validated set of unique `(action, resource)` rules for
the session. The latest visible event wins. Because it is an ordinary branch
event, restart replay, revert, and fork recover the same overlay without a
sidecar file.

`model_usage_recorded` follows a committed provider turn and
contains the positive `model_turn_seq`, agent ID, resolved model identity, and
Oven's normalized usage fields: inclusive input/output totals plus optional
uncached input, cache-read input, cache-write input, text output, and reasoning
output counts. A reported reasoning count of zero on a turn that streamed
reasoning text is recorded as unknown, together with the text output count
derived from it. The optional `estimated_cost_pico_usd` stamps the engine-selected
price for that request: a number is priced, present `null` is unpriced, and an
absent field identifies an older event.
Usage rollups preserve stamped prices exactly and apply the current pricing
configuration or catalog only to legacy records where the field is absent. A
present `null` stamp is durably unpriced and is never recomputed.
Event-log validation requires its run, agent, model, and turn to
match and rejects duplicate records for one turn. Session, agent, and global
usage projections rebuild from these events after restart, revert, and fork.

`model_request_prepared` follows request assembly and all accepted model/provider request hooks.
Its `prompt_fingerprint` hashes the authoritative normalized request sent to the provider, while
`model_attempt_started` remains the earlier cancellation and lifecycle boundary.

`model_tools_published` comes just before `model_request_prepared` for the same
attempt and lists the `tool_names` sent with the request, after request hooks.
It is recorded only when the names differ from the session's latest visible
record, so an unchanged tool set adds nothing to later attempts. Validation
allows at most 1,024 names, each nonempty, at most 256 bytes, and free of
control characters; a request whose names do not fit is still sent, without
a record. The TUI shows the latest record as a collapsible tools block under
the system prompt.

`tool_call_started` records the single/named output declaration, including
arbitrary named streams, and `background: true` when the call's tool supports
the background, so a client may offer to move the running call there
(`run.tool_background`); the field is absent otherwise and in older logs.
`tool_call_progress` is live-only. It contains a control-free status `message`
and optional UI-only `display`. Display deltas append; terminal `result.display`
replaces them.
Each live message/display field is bounded at 1 KiB, their cumulative per-call
budget is 64 KiB, and final display has an independent 64 KiB bound. TUI state is
bounded and never reconstructs display by merging authoritative streams.
When the final display occurs verbatim in the result `output` (a single-output
tool's whole output, or a shell's only stream), the result carries
`display_span`, a `[start, end)` byte range of `output`, instead of a second
copy in `display`; decoding restores `display` from it.

Authoritative output chunks go to runtime capture before completion, not into
unbounded event records. Terminal `retained_output` contains ordered per-stream
byte/line counts, truncation offsets, and an incomplete flag. Only a stream
that outgrew its preview has an artifact (`reference` and `sha256`); a stream
the result output holds whole has neither, and `retained_output.reference`
(the single stream's artifact or the generic manifest) is absent when no
stream needed one. Failed/cancelled calls retain accepted output
with their terminal status and error. Self-paginating tools omit output retention
and fail instead when a requested page exceeds the 2 MiB result bound.
`tool_output_elided` records which retained output compaction elided.

During cancellation cleanup, a progress record is considered retained once its
append command enters the session actor mailbox; actor FIFO then orders it before
the later terminal command even if persistence has not completed yet. Cleanup
deadline diagnostics count only display progress records that never entered that
mailbox; authoritative chunks already accepted into capture remain retained.

For a batch of tool calls from one model turn, `tool_call_started` events are
written in model content order before execution begins. Calls may then execute
in parallel, so progress and `tool_call_terminated` events from different calls
may interleave, and terminations commit in completion order. The ordering
guarantee is per call: its start precedes all of its progress events,
which precede its single termination. Consumers must correlate calls by
`tool_call_id` rather than cross-call event position. A failed termination
carries one of the [tool failure codes](tools.md#tool-call-failures).

`internal_agent_usage_recorded` attributes each completed
internal model request to its internal run, internal agent ID, kind, and resolved
model. It carries the same optional `estimated_cost_pico_usd` stamp. The same
session, agent, and global projections include these records.

`skill_loaded` and `skill_invocation_noted` persist skill use. `skill_loaded`
stores the rendered body, skill name, source path, arguments, base directory,
and at most ten supporting-file paths. It is pinned across context checkpoints.
An identical repeat uses the shorter `skill_invocation_noted` payload.

`user_input_admitted` normally belongs to an active run. A steer accepted for a
queued delegated child is runless and requires that the session have no active
run. It is persisted immediately and enters the next run's pending-input FIFO,
including when a terminal child is queued for resume. A queued cancellation
appends a runless `user_input_recalled` naming each such admission so those
inputs cannot leak into a later resume. Promotion appends the ordinary run-owned
`user_input_submitted` event, so transcript rendering and model history use the
same path as interactive run steering.

`message_injected` is a run-owned role and text message accepted from `agent_before_start`. It is
written during run setup and projected into model history, so reopen, fork, revert, and compaction
use the injected message without consulting the plugin again. `user_input_transformed` is the
run-owned audit record written immediately before `user_input_submitted` when `user_before_input`
changes direct input; it contains both `original_input` and the non-empty committed `input`.

Running-resume rollback and `session.recall_input` append
`user_input_recalled` with the exact `user_input_seq` of its admission (in the
same run, or runless for a runless admission). Replay removes that specific
pending input and preserves the others.

`session_model_selected` (`selection`) is the session-level model pick a client
made with `session.select_model`; it is runless and its origin names the client.
Later runs that name no selection of their own use it.

### The queued-input lane

The queued-input lane is session-level, so its events are runless.
`user_input_queued` holds an input waiting for the running run to complete, with
the `selection` and `client_run_id` of the run it will start; its origin is the
client that queued it. The entry leaves the queue when a `run_started` carries
its `client_run_id`, when `user_input_queue_recalled` names its `queued_seq`, or
when every entry is voided by a `run_finished` that did not complete,
or by `user_input_queue_voided` (written when a queued input cannot start). A
voided entry goes back to the client that queued it.

## Compaction checkpoints

`context_checkpoint_committed` carries a `commit` containing the text summary or
opaque native checkpoint, `boundaries`, and `budgets`. Internal summaries cover
the active pre-compaction history before the recent messages retained unchanged
after the summary; the two do not overlap. Active history is the currently assembled context,
not a reload of events replaced by earlier checkpoints. The original saved log
is preserved; retained recent messages are replayed from it. System and tool
context comes first, followed by pinned `AGENTS.md`, loaded skills, the summary,
and the recent messages. See [Compaction](../guide/compaction.md) for selection
and fit rules.

The additive retention fields are optional in saved events:

| Field within `commit` | Type | Legacy absent value | Meaning |
|---|---|---|---|
| `boundaries.recent_from_seq` | optional `u64` | `None` | Inclusive starting sequence of the retained original-message suffix. No value means no retained suffix. |
| `budgets.keep_recent_tokens` | `u64` | `0` | Effective recent-history token budget for this checkpoint, not the raw configured request or a count of retained tokens. |

The effective budget is at most `min(configured keep_recent_tokens,
input_budget / 4)` and is further limited by actual post-checkpoint available
space. Complete tool-call/result groups can leave the retained suffix smaller
than that budget; an oversized newest indivisible group yields no tail rather
than exceeding the budget. An absent legacy retention boundary does not cause
the reader to infer a suffix from current configuration.

Native compacted windows retain their existing replay behavior, with no
independently appended recent-message suffix. Neither checkpoint path rereads
files.

## Subscriptions

`events.subscribe` returns retained events after the optional cursor and
starts `events.subscription` notifications. With `limit`, it returns at most
that many events; when more remain it sets `has_more: true` and starts no
notifications, and the client asks again with the last returned `seq` as the
cursor. Only the final page (no `has_more`) subscribes, atomically with its
snapshot, so events appended between pages arrive in a later page and none are
missed. Without `limit` the first response is the final page. Paging a session
this process does not own parses its log once and serves later pages from
memory while the file is unchanged.

`events.unsubscribe` ends the connection's live tail for a session: no further
`events.subscription` or `events.plugin` notification for it is sent, and the
shared client drops messages already in flight and stops recovering it until it
subscribes again. A session no connection has a tail on can be evicted from the
daemon's memory, so a long-lived client releases sessions it no longer shows.
The TUI keeps the sessions of its eight most recently viewed trees subscribed,
so switching back is instant, and releases the others once they have no run and
no pending or queued input; it keeps their last state and catches up from it
when one is reopened.

A connection holds at most one live tail per session. Subscribing a session
the connection already follows (reopening it, or a recovery replay) replaces
its tail in the same step as the new snapshot: notifications the old tail had
already queued are written first, then the new tail's, and nothing reaches
both. Durable events carry a `seq` a client can drop a copy by, but transient
output does not, so two tails would show every streamed delta twice.

A notification is tagged as a stored event:

```json
{ "type": "event", "event": { "engine_version": "0.1.0", "seq": 43 } }
```

live-only stream output, which has no sequence of its own:

```json
{
  "type": "transient",
  "event": {
    "session_id": "...",
    "run_id": "...",
    "after_seq": 43,
    "timestamp": "...",
    "payload": { "type": "text_delta", "attempt_id": "...", "text": "..." }
  }
}
```

or a gap indicating that the subscriber must rebuild its disposable projection:

```json
{
  "type": "gap",
  "session_id": "...",
  "last_delivered_seq": 42
}
```

Stored sequences stay gap-free across a streamed turn, so replay cursors only
ever point at durable events. A transient event belongs right after the durable
event `after_seq`: it never advances the cursor, is never replayed, and is
never a reason to recover. The shared client delivers one only when its
`after_seq` equals the subscription cursor, including output buffered while a
replay is in flight; anything else is stale or outruns a durable event the view
has not seen, and is dropped. The engine drops transient output for a
subscriber whose queue is nearly full rather than spending the slot reserved
for a gap.

A revert cuts the log and the next appends reuse the removed sequence
numbers, so live subscribers receive a rewind:

```json
{ "type": "rewound", "session_id": "...", "through_seq": 41 }
```

A subscriber drops everything it holds past `through_seq`; later events follow
on from it. The shared client also replays the session in full. A reader that
was not subscribed when the cut happened cannot tell a reused sequence from the
event it replaced, so a replay request may carry `cursor_timestamp`, the
timestamp of the event at `cursor` as the reader saw it. When the session's
event at `cursor` has a different timestamp (or no longer exists), the response
is `{ "events": [], "stale_cursor": true }`, no tail is registered, and the
reader must replay from the start. The shared client sends it on every
recovery replay.

Tool stdout and stderr use separate snapshot, delta, and gap notifications.
Offsets are byte offsets, and clients use a snapshot after a gap before applying
later deltas. The ordinary event subscription also includes live-only
`tool_call_progress` output chunks and the durable terminal events without a
separate filter.

# Goal Mode and Producer Messaging (Design Spec)

Status: **implemented; approved product contract**. This document is the
governing specification for session goal mode and the unified producer
messaging registry. The prerequisite compaction, permissions, and symlink
work was reviewed and pushed as commits `4cf56157` and `9ccf8fa6` before
implementation began. See [Staging plan](#9-staging-plan).

This spec does not reopen the approved product decisions. Where a genuine
ambiguity or contradiction remains, it is listed under
[Open technical risks](#10-open-technical-risks) instead of being silently
resolved here.

Non-goals: no inferred goals, no goal inheritance into subagents, no
deadlines/watchdogs/expiry for producers, no Codex-style extra budgets,
blocker counters, or additional goal states, no new public scheduler
priorities, no mandatory third message queue.

## 1. Goal mode (root session)

### 1.1 Activation

Goal mode is activated **explicitly only**, by the user typing
`/goal <objective>` in the root session's TUI (see
`crates/tui/src/ui/slash.rs`, `COMMANDS`). The engine never infers a goal from
prompt content. Only root sessions can have a goal; the slash command in a
delegated/child session is rejected with an explanatory error.

Activation and retained lifecycle shortcuts:

- `/goal <objective text>` — activate goal mode with the given objective.
  Errors if a goal is already `active` or `paused` (the user must
  cancel or complete the current goal first).
- Bare `/goal` shows usage requiring an objective; it does not report status.
- `/goal status` is removed and rejected, not interpreted as an objective.
- `/goal pause` — transition `active -> paused`.
- `/goal resume` — transition `paused -> active`.
- `/goal cancel` — transition `active|paused -> cancelled`.

The primary lifecycle controls are the goal bar's Pause/Resume and Cancel
buttons; slash lifecycle shortcuts are secondary. A persistent one-line goal
bar sits above the message composer whenever the selected session has a goal.
It is hidden when no goal exists and remains visible with read-only status for
completed or cancelled goals. Clicking the objective description opens a detail
modal containing the full objective, checklist, and lifecycle status. This is
a read-only checklist view, not an editor; the root maintains checklist items
through `goal_update { items }`.

The producer queue strip remains separate, above the goal bar and composer.
This persistent goal surface replaces command-generated status reports and
ephemeral goal progress in the general status row. Internal goal IDs, revisions,
and lifecycle RPC identity/concurrency checks are unchanged.

Lifecycle states are exactly: `active`, `paused`, `completed`, `cancelled`.
`completed` and `cancelled` are terminal. The user owns the objective and the
lifecycle; the model cannot change either through tools (see 1.3).

### 1.2 Durable goal state

Goal state is durable session event history (`crates/protocol/src/event.rs`)
and is **independent of summaries and compaction**: compaction
(`crates/engine/src/runtime/compaction.rs`, `ContextCheckpointCommit`) never
elides goal events from the projection, and the goal projection is rebuilt
from events, not from summary text. The objective and the full checklist are
re-injected into goal-continuation reminders (4.3), so compaction cannot
strand the goal.

New events (additive; event history is versionless and read best-effort per
`AGENTS.md`):

- `GoalActivated { goal_id, objective, revision }`
- `GoalChecklistRevised { goal_id, items: Vec<GoalItem>, revision }`
- `GoalLifecycleChanged { goal_id, status, revision }`

`GoalItem = { description, finished }`; checklist items have no IDs.
`GoalState.revision` is an engine-owned, strictly increasing per-goal counter
for durable state, user lifecycle controls, and reminder identity. Each activation
has a distinct durable `goal_id`; replacing a terminal goal cannot make an
old lifecycle RPC or reminder valid again merely by reusing a revision number.
Model checklist updates are not bound to a goal ID.

### 1.3 Model tools: `goal_get` and `goal_update`

After activation, the root run's tool set gains two tools. They are
**root-only**: delegated child sessions never see them and there is no goal
inheritance. Because tool prompt sections are composed only at run admission
and frozen into `run_started` (`AGENTS.md`; `crates/engine/src/runtime/tool_prompts.rs`),
the tools appear/disappear at the **next safe run admission** after a
lifecycle change — never mid-run.

- `goal_get` — returns objective, status, revision, and the full checklist.
- `goal_update { items }` — replaces the entire ordered checklist of the
  current/latest session goal when the engine actor accepts the update.
  Each item contains only `description` and `finished`. The session actor
  serializes replacements; the last accepted update wins. The model supplies
  neither `goal_id` nor `expected_revision`, and there is no model-facing
  lost-update protection. An update from an older run can intentionally
  replace the checklist of a newly activated active or paused goal. Updates
  reject if the current goal is absent or terminal, even from tools still
  present in an already-admitted run.

User lifecycle RPC controls retain
`SessionGoalLifecycleParams.goal_id` and `expected_revision`; a stale lifecycle
request is rejected. These checks are separate from model checklist replacement.

Rules:

- The model **cannot** set, pause, resume, or cancel through tools. Lifecycle
  transitions are user-only, except the completion rule below.
- When `goal_update` leaves the checklist **non-empty and every item
  finished**, the engine records `completed` (via `GoalLifecycleChanged`)
  with the new revision. An **empty checklist is never vacuous completion**:
  it is accepted as a revision but preserves the current active or paused
  lifecycle state. An active goal with an empty checklist remains eligible
  for a reminder that asks the root to establish the checklist.
- **Paused behavior (explicit design judgment):** while paused, `goal_get`
  and `goal_update` remain available at the next admission and updates are
  preserved, including an all-finished update that records `completed`.
  Refusing writes would add a new user-facing feature surface; recording is
  the minimal behavior. Recording completion while paused schedules **no**
  continuation — pause and completion both suppress goal wakes (4.2), so
  there is nothing to resume. The model still cannot unpause or cancel.
- Tool guidance (system-prompt/tool-description text) instructs the model
  that evidence and audit are **root-authoritative**: the root should verify
  work directly or via subagents before marking items finished, and the root
  checklist is the authoritative record over any subagent's self-report.

## 2. Unified producer registry (runtime-only)

### 2.1 Model

The engine owns a per-session **producer registry**: a runtime-only,
inspectable vector of registration records. Nothing about registrations is
durable; restart semantics are in section 5.

Operations (separate calls, mode is **per send**, not per registration):

- `register(session) -> producer_id`
- `send(producer_id, message, mode: steer | queue) -> ack`
- `unregister(producer_id)`
- `discard(session, message_id)` (owned accepted message, not registration)

Semantics:

- Registering and unregistering without any send is valid.
- `send` to a closed (unregistered) or foreign (wrong owner / wrong session)
  registration is rejected.
- A registered producer may send **after its turn ends**; sending to an idle
  session wakes it (section 3). Sending to a session being evicted/closed is
  rejected.
- There are **no deadlines, watchdogs, or expiry**. Producers are good-faith
  owners of their registration lifecycle; a leaked registration only affects
  goal-readiness evaluation (4.2) and is surfaced in inspection.

### 2.2 Ownership and authorization

Every registration has one authenticated owner. Durable `ProducerOwner` variants
are `Plugin { plugin }`, `Delegation { invocation_id }`, `Goal { goal_id }`, and
`GoalControl { goal_id }` (`crates/protocol/src/producer.rs`). Plugin connection
authority remains runtime-only. `GoalControl` identifies engine-authored lifecycle
steering, separate from the goal controller's continuation reminders. The
destination is fixed at registration and checked on every send.

Plugins must declare a new protocol capability to register producers (the
current plugin request surface rejects unknown requests — see
`crates/engine/src/plugin.rs` `PendingRequest` and the capability model in
`docs/site/development/plugins.md`; an extension protocol bump beyond `0.0.4`
is required). Only **live, owned** registrations authorize post-turn
model-message sends. The existing plugin `emit_session` path
(`enable_session_publishing`, `ctx.emit_session`) must **not** remain a
bypass for model-bound messages: emitting content that enters model history
requires an explicitly supplied live producer registration. The engine must
not create an implicit registration to authorize an otherwise unregistered
emission. Legacy `emit_session` is restricted to non-model content or removed
in favor of the explicit producer API. Pure
bus events (`emit_bus`) are unaffected — non-model bus traffic is separate.

### 2.3 Message persistence and acknowledgment

Message *registrations* are volatile; accepted *messages* are durable.

- `send` returns its ACK **only after** the engine appends a durable
  `ProducerMessageAccepted { message_id, producer_owner, mode,
  idempotency_key, body }` event to the session log. Senders must treat a
  missing/failed ACK as commit-uncertain and retry with the same
  `idempotency_key` (a stable sender-chosen message ID); the admission path
  deduplicates on `(session, stable_producer_owner, idempotency_key)`.
  The stable owner identifies the specific plugin, delegation invocation,
  or goal, not just its kind or volatile registration ID. A retry requires
  a live owned registration; identical retries return the original receipt,
  while reuse with different content or mode is rejected. This is a
  modest at-least-once design; the engine makes **no exactly-once claim**
  about external side effects or model execution.
- `unregister` does **not** remove accepted, unconsumed messages.
- After a crash, accepted-but-unconsumed, undiscarded messages are recovered and admitted
  independently of whether their producer re-registers (5.4).

### 2.4 Owned-message discard

`discard(session, message_id)` authorizes against the authenticated caller's
ownership of the durable accepted message, not a live registration. An owner
can discard its own waiting message even after unregistering. Cross-owner
discard is rejected; a retry for an owned already-discarded message succeeds
idempotently. This is separate from unregistering or recalling user input.

Only **unclaimed waiting messages** can be discarded, including admitted messages
not yet claimed. The actor serializes discard against durable claim acquisition
before request preparation/dispatch: consumed or claimed messages reject discard,
even if no provider request has been sent yet. A claim is a reservation, not proof
of provider dispatch. It remains held through compaction, hooks, streaming, and
model commit. Failed or cancelled attempts release claims; recovery releases
surviving claims. Once all claims are released, unconsumed, undiscarded messages
return to waiting. In-flight input cannot be retracted, and discard cannot undo
model or external actions. See `crates/engine/src/runtime/producer_claims.rs`.

Successful discard is durable through `ProducerMessageDiscarded`, for real
producer messages as well as goal reminders. Discarded messages no longer
participate in delivery or pending-work readiness and are removed from the
queue strip; replay must not restore them as pending. Goal reminder invalidation
continues to check reminder identity and never discards another owner's work.

## 3. Delivery modes and admission

The existing machinery — the per-session actor mailbox
(`crates/engine/src/actor.rs`), `UserInputAdmitted` / `UserInputSubmitted` /
`pending_inputs` (`crates/engine/src/runtime/mailbox.rs`), and the residency
rule that forbids runless pending inputs while a run is active
(`crates/engine/src/runtime/residency.rs`,
`has_runless_pending_inputs`) — retains its existing user-input rules.
Producer delivery extends it with separate events:

- `ProducerMessageAccepted` durably stores the body and delivery mode, even
  when a run is active.
- `ProducerMessageAdmitted` references that message from a particular run.
  Its sequence participates in committed model input coverage.
- `ProducerMessagesClaimed` reserves admitted messages; its event sequence
  identifies the claim. `ProducerMessagesReleased` releases that reservation.
- `ProducerMessageConsumed` records committed consumption;
  `ProducerMessageDiscarded` records generic owned-message discard, including
  pending goal reminder invalidation.

User-input event shapes are unchanged. Producer messages do not enter the
user composer restoration lane or become void merely because a run ends.
Pending and admitted producer `steer`/`queue` messages do appear in the existing
auto-show/hide queue strip above the goal bar and composer. Visibility does not
grant user composer recall. Claimed entries are hidden until release returns
them to waiting, unless already consumed or discarded.

### 3.1 STEER

A steer message is inserted into the **next safe model API request**,
including within an active run's tool loop: it joins the existing pending
steer lane and is promoted per `PromotePendingInputs`
(`crates/engine/src/runtime/mailbox.rs`). It never cancels an in-flight API
request and never splits a tool-call/tool-result pair. A steer sent to an
idle session starts a normal run.

### 3.2 QUEUE

A queue message starts a **subsequent** normal run. While a run is active it
is accepted and persisted but **not delivered mid-run**; when the session is
idle it starts a normal run. This requires additive events plus an admission
extension: today runless admits are forbidden while active, so queue admits
during an active run must be recorded as accepted-but-deferred rather than
rejected or voided (`void_runless_pending_inputs`,
`crates/engine/src/runtime/delegation.rs`). Both modes wake generically —
this is not goal-specific machinery.

### 3.3 Subagent results

Subagent results are **always steer**. Background delegation completions
register a producer **before** starting the async work, `send(.., steer)` on
finish, then explicitly `unregister`. Foreground (blocking) delegation keeps
its tool result as the single result channel and must **not** additionally
emit a separate message. The result must be materialized exactly once: the
current `DelegateFinishedV2` projection path
(`crates/engine/src/runtime/delegation.rs`,
`crates/engine/src/model_history.rs`) is unified with producer steer delivery
so a background completion cannot appear both as a tool-style result and as
a steered message.

## 4. Goal wake controller

### 4.1 Registration

While a goal is `active`, the goal controller holds a producer registration
on the root session. When evaluating readiness (4.2), the controller excludes
**only its own registration ID** — any other live producer blocks the wake.

### 4.2 Readiness condition

A goal continuation reminder is armed when **all** hold:

1. goal is `active` and the checklist is empty or has unfinished items;
2. the session is recovered and the root is idle (no active run);
3. no pending real steer, queue, or user messages exist;
4. no **other** live producer registration exists;
5. no goal reminder is already pending.

The check-and-enqueue is atomic inside the session actor and is **rechecked
at run admission** (`crates/engine/src/runtime/admission.rs`), so real work
that arrives between arming and admission always wins. An active goal run
can still receive real steer messages normally.

### 4.3 Reminder identity and content

The reminder message carries the **full objective and the entire checklist**
(finished and unfinished items, with revision). Reminders have an internal
identity `(goal_id, revision)` allowing coalescing and invalidation of a
pending reminder — no public
scheduler priority and no mandatory third queue. Reconciliation is triggered
on: run end, message send/consume, producer unregister, goal
activation/resume, and session recovery. A consumed reminder does not block
another reminder for the same revision after its run ends: making no
checklist edit is not completion. Durable send idempotency uses a fresh
message ID for each continuation attempt, distinct from the pending
reminder's coalescing key.

### 4.4 Pause / cancel / complete

`pause`, `cancel`, and `completed` each unregister the goal producer and
invalidate any pending reminder, stopping future goal continuations.

User pause/cancel also durably queues a root notification in `steer` mode
before the relevant producer registration closes if **any** of these hold:

- the root has an active run;
- the root has other live producer registrations, excluding the goal's own
  registration, including producers for pending subagent results;
- the root has queued real messages (not goal reminders).

The notification enters the next safe model API request, without cancelling an
in-flight request or splitting tool-call/tool-result pairs. It tells the model
that the goal was paused or cancelled, to stop autonomous pursuit of that old
goal, and to follow the user's directions. Closing the registration does not
remove the accepted notification. Its accepted owner is
`ProducerOwner::GoalControl { goal_id }`, not `Goal`; it is not a reminder and
survives goal-controller teardown and reminder invalidation.

For an idle root waiting on other producers or queued real messages, ordinary
`steer` semantics can wake a normal root run. This does **not** resume goal
reminders: the goal remains paused or cancelled. A fully idle root with no
other live producers and no queued real messages persists the lifecycle change
without an announcement run; the goal's own registration or pending reminder
alone does not qualify for notification. Steering does not abort an active run
or delegations, and cannot undo actions already emitted. The separate
interrupt/cancel-run path (`/cancel`, `RunCancelled`, `RunInterrupted`) is
unchanged. This notification adds no run binding to `goal_update { items }`:
updates still target the current session goal at actor acceptance.

## 5. Restart and recovery

1. Registrations vanish on restart. Durable goal state and accepted messages
   are restored from the event log; delegations are reconciled by the
   existing recovery path (`crates/engine/src/runtime/recovery.rs`).
2. Producer-capable plugins reconstruct their pending work during their
   recovery/init phase from their **own storage or the external services they
   front**, then register fresh runtime IDs. Logs alone are currently
   live-only, and this spec deliberately adds **no general replay API**, no
   engine-managed durable producer records, and no plugin recovery blobs
   (both rejected at product level).
3. Plugin recovery uses an explicit engine request/callback with an explicit
   completion notification — **no timers**. The engine tracks per-plugin
   startup readiness statuses (`starting`, `ready`, `failed`, `disabled`).
   Session adoption stays lazy so engine↔plugin callbacks cannot deadlock.
4. **Readiness barrier:** a producer-capable plugin may register during
   restoration without waiting for goal readiness, but the goal controller
   holds its auto-registration/wake until the readiness barrier completes
   (all tracked producer-capable plugins report `ready`). Failed or disabled
   tracked producers hold goal readiness and are diagnosed rather than being
   interpreted as completed external work.
   An `inactive` (paused/completed/cancelled or absent) goal needs no
   barrier. Accepted messages from before the crash are recovered and
   admitted independently of any producer re-registering.
5. **Failed/disabled plugins must not silently prove work done.** If a
   producer-capable plugin ends `failed` or `disabled`, the engine surfaces
   that status (diagnostic event + TUI notice) and goal readiness treats
   unrecovered external work as unknown rather than complete. Documented
   limitation: the engine cannot distinguish "plugin had no pending work"
   from "plugin lost its state" — this is weaker than the user-facing
   assumption that restarts are transparent, and it is called out in the
   user docs.

## 6. Durable events, projection invariants, and history transforms

Events (all in `crates/protocol/src/event.rs`, with corresponding entries in
`docs/site/reference/events.md` and generated schemas):

| Event | Payload (summary) |
| --- | --- |
| `GoalActivated` | `goal_id`, `objective`, `revision` |
| `GoalChecklistRevised` | `goal_id`, `items`, `revision` |
| `GoalLifecycleChanged` | `goal_id`, `status`, `revision` |
| `ProducerMessageAccepted` | `message_id`, `producer_owner`, `mode`, `idempotency_key`, `body`, optional `reminder` |
| `ProducerMessageAdmitted` | `message_id`; enclosing event carries the destination `run_id` |
| `ProducerMessagesClaimed` | Nonempty `message_ids`; run-scoped, envelope `seq` identifies the claim |
| `ProducerMessagesReleased` | Positive `claim_seq`; run-scoped to the same run as the referenced claim |
| `ProducerMessageConsumed` | `message_id`, `run_id` |
| `ProducerMessageDiscarded` | `message_id`; optional `producer_owner`, optional `reminder` |

Projection invariants:

- Goal revision strictly increases per activation and is assigned by the
  engine. Model checklist replacements are actor-serialized, last accepted
  update wins for the current goal at actor acceptance, with no model goal ID
  or expected-revision check. Updates reject absent or terminal current goals;
  user lifecycle RPCs still reject stale goal IDs and `expected_revision` values.
- `completed` requires a non-empty, all-finished checklist.
- Every `ProducerMessageAccepted` is consumed at most once; consumed
  messages are excluded from `pending_inputs`. Consumption is durable only
  when a committed model turn records that message in its input coverage,
  not merely when it is selected for an API request. Recovery reconciles a
  committed turn whose consumption marker was not yet appended. An
  uncommitted API attempt can be repeated after a crash; this is not an
  exactly-once guarantee for provider calls or external effects.
- Reminder coalescing never reorders or drops **real** (non-reminder)
  messages.
- Durable discard is distinct from consumption. Discarded messages are excluded
  from delivery, pending-work readiness, and the queue strip. Ownership checks
  survive unregister; eligibility is checked atomically against claim acquisition,
  not inferred merely from the absence of a consumption marker. Claimed messages
  are nondiscardable and hidden from the waiting strip, including before dispatch.
- Claims reference unique accepted/admitted, unconsumed, undiscarded messages
  belonging to the claim's run. Release identifies a claim by envelope sequence
  and must match that run. Release removes only that reservation; when all claims
  are gone, unconsumed, undiscarded messages return to waiting. It does not undo
  committed consumption or prove that a provider request did or did not execute.
- Discard projection strictly matches a supplied owner against the accepted
  message and any supplied reminder against its identity. Historical discards
  without an owner require a matching reminder; a wrong supplied owner cannot
  use that legacy fallback. Both forms reject consumed or claimed messages.

History transforms:

- **Compaction** (`ContextCheckpointCommit`): goal events and unconsumed
  `ProducerMessageAccepted` events survive compaction in the projection; the
  reminder re-injects objective+checklist so the model never depends on
  pre-compaction goal text.
- **Fork**: a forked session copies the goal projection and, for an `active`
  goal, creates a fresh goal producer registration; runtime registrations
  never cross the fork. Unconsumed accepted messages are forked with the
  log; consumed or durably discarded ones are not replayed as pending. Compaction
  likewise preserves the discard projection alongside accepted-message state.
- **Revert** (`SessionReverted`): goal and message events after the revert
  point are discarded per the existing contract, which may resurrect an
   earlier goal state or unconsumed messages. Goal and delegation-owned
   registrations are reconciled with the surviving projection; plugin
   registrations cannot be reconstructed from history and remain subject to
   explicit plugin lifecycle handling. Revert retains the engine's existing
   active-run restrictions; it does not gain permission to rewrite active
   execution merely because goal pause is non-interrupting.

## 7. APIs, permissions, and surfaces

- **Protocol/SDK**: extension protocol `0.0.5` adds the producer capability,
  plugin requests `plugin/producer/register`, `plugin/producer/send`,
  `plugin/producer/unregister`, and `plugin/recovery/complete`, plus the
  deadline-free `plugin/recovery/start` notification. The SDK exposes
  explicit producer handles and removes the legacy session-publishing
  helpers. See `docs/site/development/plugins.md` for the public contract.
- **Permissions**: `goal_get`/`goal_update` follow the existing tool
  permission model (`crates/engine/src/permissions.rs`); they are root-only
  by construction, not by permission config, and are not configurable
  capability surface. `goal_get` matches `read` on `goal:current`;
  `goal_update` matches `write` on `goal:current`. Explicit activation does
  not grant a permission exception. The producer capability is off by
  default for plugins.
- **Run-freeze integration**: tool set and tool-prompt changes apply only at
  run admission; goal lifecycle events never mutate `history[0]`
  (`AGENTS.md` rules; `crates/engine/src/runtime/tool_prompts.rs`).
- **TUI** (`crates/tui/src/ui/slash.rs`, `crates/tui/src/state`):
  explicit `/goal <objective>` activation with matching usage/help, plus a
  persistent one-line goal bar above the composer. Its clickable objective
  opens a detail modal with the full checklist and status, without checklist
  editing. Pause/Resume and Cancel buttons are the primary lifecycle controls;
  terminal goals retain a read-only bar and detail view, while absent goals
  hide the bar. Notifications for goal completion and failed/disabled
  producer-capable plugins remain separate from this persistent surface.
  Pending/admitted producer steer and queue messages share the existing queue
  strip above the goal bar and composer, using its existing automatic show/hide
  behavior.
  Claimed entries are hidden until release restores waiting, unless consumed
  or discarded. Discarded entries are removed/hidden; producer messages cannot be recalled
  into the user composer. Discard is an owner-authorized producer API, not a
  new composer recall action.
- **Inspection**: runtime snapshot exposes the producer registry records
  (owner kind, session, age) for debugging; this is read-only.

## 8. Testing and acceptance matrix

Engine tests (`crates/engine/src/runtime_tests.rs`), protocol round-trips
(`crates/protocol/src/tests.rs`), TUI tests, and docs build
(`./scripts/build-docs.sh`, strict) must cover at minimum:

1. `/goal` grammar: explicit objective activation; bare `/goal` shows objective
   usage without a status report; `/goal status` is rejected without activation.
   Retained lifecycle shortcuts, child-session rejection, and activation conflicts
   remain covered.
2. Lifecycle: pause/resume/cancel transitions; terminal-state rejection of
   further transitions; user-only lifecycle enforcement against tool calls.
3. Checklist: items contain only `description` and `finished`; full ordered
   replacements take only `items`, and the last accepted update wins for the
   current goal. An older run's update can affect a newly activated active or
   paused goal. Engine revisions still increase; stale user lifecycle goal IDs
   and revisions, and updates with no current active/paused goal, are rejected. An empty
   checklist never completes or unpauses a goal, and can bootstrap an active
   goal; all-finished completes.
4. Pause semantics: updates preserved while paused; all-finished-while-paused
   records completion with no continuation; no future continuations after
   pause/cancel. Each notification condition independently qualifies: an active
   root run, another live producer (excluding goal self), or queued real messages.
   Steering is durable before the producer registration closes and enters the
   next safe model request; idle roots with other producers or real messages can
   wake a normal run without resuming goal reminders. A fully idle root persists
   lifecycle without an announcement run; goal self/reminders alone do not qualify.
   In-flight requests and delegations are not aborted, tool pairs remain intact,
   and already-emitted actions are not undone. Notification guidance stops
   autonomous old-goal pursuit without adding run binding to checklist updates.
5. Tools appear/disappear only at run admission (freeze).
6. Producer registry: zero-send unregister; closed/foreign send rejection;
   post-turn send wakes idle session; mode is per-send.
7. Persistence: ACK-after-durable-accept; retry with same idempotency key
   dedupes; crash recovery admits accepted-unconsumed messages without a
   live producer; unregister keeps queued messages.
8. Delivery: steer lands mid-tool-loop without splitting tool pairs or
   cancelling in-flight API; queue-while-active is persisted and deferred;
   idle queue starts a run.
9. Subagent results: background completion steered exactly once (no
   `DelegateFinishedV2` duplication); foreground result not duplicated.
10. Goal readiness: each readiness conjunct individually blocks; own
    registration excluded; atomic check + admission recheck; reminder
    coalescing/invalidation by goal ID and revision; real work ahead of
    reminders; repeated reminders at an unchanged unfinished revision.
11. Restart: registrations gone; goal restored; readiness barrier holds auto
    wake; failed/disabled plugin surfaced and does not complete goal items;
    lazy adoption does not deadlock.
12. Compaction/fork/revert invariants from section 6.
13. Producer discard: owned waiting messages can be discarded before or after
    unregister, including admitted-but-unclaimed messages; own
    already-discarded retries are idempotent. Reject cross-owner, consumed, and
    claimed/in-flight messages, including claims before provider dispatch. Race
    discard against claim acquisition; replay durable discard
    without redelivery or a stale pending-work blocker.
14. Queue strip: pending/admitted producer steer and queue messages appear in
    the existing strip above the goal bar and composer, participate in automatic
    show/hide,
    and disappear on discard. Producer entries cannot be recalled into the
    user composer. Claims hide entries; release restores only unconsumed,
    undiscarded waiting entries after all claims end. In-flight input cannot be
    retracted. Cover failed/cancelled attempts and recovery release.
15. Goal bar: persist one line while the selected session has a goal; hide when
    absent and retain terminal read-only status. Clicking the objective opens
    the full checklist/status modal without editing controls. Pause/Resume and
    Cancel buttons invoke the existing lifecycle controls with identity/revision
    checks. Session switching and narrow layouts keep the queue strip, goal bar,
    and composer separate, without overlap or ephemeral status-row substitution.

## 9. Staging plan

The prerequisite compaction/permissions/symlink work was reviewed, committed,
and pushed before these implementation stages:

1. Protocol events + projection (section 6) with tests; no behavior change.
2. Producer registry core + admission extension (sections 2–3) including
   queue-while-active persistence; plugin capability + SDK surface.
3. Delegation integration: background completions via registry, single
   materialization of results (3.3).
4. Goal mode: durable state, tools, slash commands, wake controller
   (sections 1, 4, 7).
5. Recovery/readiness barrier + TUI progress/notifications (sections 5, 7).
6. Docs: events reference, plugin development, user guide; bindings regen.

## 10. Open technical risks

1. **Queue-while-active admission** is the deepest change: the residency
   invariant (`has_runless_pending_inputs`, `void_runless_pending_inputs`)
   currently forbids or voids runless admits during an active run, and the
   run-end path voids still-pending steered inputs
   (`crates/tui/src/state/mod.rs` mirrors this). Queue messages must be
   exempted from voiding without weakening the user-input invariants.
2. **Single materialization of background results**: merging the
   `DelegateFinishedV2` path with producer steer delivery risks either
   duplicate model-history content or a lost result on the crash boundary
   between "accepted" and "materialized"; the ACK/consume events must be the
   single source of truth.
3. **Plugin emit migration**: routing existing `emit_session` model-bound
   content through registrations is a breaking extension-protocol change for
   current plugins; the bypass must close without stranding non-model uses.
4. **Readiness barrier vs. plugin latency**: holding goal auto-wake until
   plugin recovery completes can delay continuations arbitrarily if a plugin
   hangs in `starting`; the no-timers decision means the engine cannot force
   a timeout, only surface the status.
5. **Revert/compaction interaction** for accepted-but-unconsumed messages
   relies on best-effort reading of versionless history; malformed or
   partially reverted message events need defined discard behavior (reuse
   the existing `DiscardedInvalidPayload` pattern).
6. **Paused-completion judgment** (1.3) is recorded as minimal behavior; if
   product later wants "paused blocks completion", that is a new decision,
   not a bug fix.
7. **Historical checklist decoding**: treatment of older stored items that
   contain an `id` is pending verification of the core implementation. This
   spec does not promise a particular compatibility decoder or migration;
   session history remains subject to the versionless best-effort contract.

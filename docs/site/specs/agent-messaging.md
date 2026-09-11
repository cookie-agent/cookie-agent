# Agent Messaging (Subagent-to-Subagent Communication)

Status: draft proposal; not approved for implementation. Do not implement until
explicitly authorized.

## Goals

- Let any agent in a delegation tree send a durable message to any other agent
  in the **same tree** (parent ↔ child, sibling ↔ sibling, child → grandparent).
- Delivery semantics: the recipient receives the message either **steered** into
  its next safe model-request boundary (if running) or **queued** durably (if
  idle or queued). Messaging a finished agent **wakes** it: the message is
  admitted durably and the session resumes with it as input.
- Model-visible tool surface, permission-gated, observable by the TUI and
  plugins.

## Non-Goals (v1)

- Cross-daemon / cross-process agent mesh. The engine is in-process; one daemon
  owns all sessions.
- Arbitrary cross-session messaging outside the delegation tree. No discovery or
  authorization basis exists for that yet.
- Request/response RPC between agents. Async mail only; replies are new
  messages.
- Broadcast topics / pub-sub. Can be layered later on the same envelope.

## Reuse Inventory (verified against source)

| Need | Existing mechanism |
|---|---|
| Durable inbox + delivery modes | Producer messaging: `ProducerMessageAccepted/Admitted/Claimed/Released/Consumed/Discarded` events (`crates/protocol/src/event.rs:1901-1945`); `ProducerDeliveryMode::{Steer, Queue}` (`crates/protocol/src/producer.rs`) |
| Idempotency | Dedup on `(session, ProducerOwner, idempotency_key)` (`crates/engine/src/runtime/producers.rs`) |
| Precedent for an agent-owned producer | `ProducerOwner::Delegation { invocation_id }` — the background-completion `<subagent_notification>` channel (`crates/engine/src/runtime/delegation.rs:3054`) |
| Authorization graph | `SessionOrigin::Delegated { root_session_id, parent_session_id, … }` (`crates/protocol/src/event.rs:150-160`) + `DelegationEventStore` — the authoritative tree |
| Tool surface template | `DelegateToolProvider` (`crates/tools/src/delegate.rs`) — closest mirror for a new provider |
| Client visibility | Producer events are durable → flow through `events.subscribe` cursor replay for free |
| Plugin observation / mediation | `plugin/event` (all durable events); `tool_before_call` intercept |

**Gaps to close:** `ProducerOwner` has no agent variant, and the delegate tools
hard-gate messaging to direct parent→child via `ensure_subagent_owned`
(`delegation.rs:2102`).

## Concepts

### Addressing

`send_message` targets a **raw `session_id` only** — the same addressing model
as `steer_subagent`. No labels, aliases, or special targets (`parent`, `root`).

There is **no discovery mechanism**. An agent learns a peer's session ID only
because the parent disclosed it — in the delegation prompt, in a steering
message, or via delegation tool results (a parent naturally knows its
children's IDs from `delegate_subagent`). The parent therefore controls the
communication graph: siblings can only talk if the parent chose to introduce
them, and an agent cannot enumerate the tree to find targets on its own.

Every inbound message envelope carries `message_id` and `from.session_id`, so
a recipient can reply by sending to `from.session_id` even if the parent never
introduced it to the sender.

### Message Envelope

The durable body (`ProducerMessageAccepted.body`) is JSON, rendered into the
recipient prompt as a user-turn materialization (repo convention: tool-emitted
system input materializes as a user turn):

```xml
<agent_message>
{
  "message_id": "...",          // ProducerMessage id
  "from": { "session_id": "...", "agent_type": "explore" },
  "body": "…markdown text…"
}
</agent_message>
```

The recipient is implicit — the envelope only ever appears in the session it
was delivered to.

## Tool Surface

New `MessageToolProvider` (provider id `builtin.message`), modeled on
`DelegateToolProvider`, registered via `try_register_tool_provider` in
`crates/cookie_agent/src/main.rs` after engine open. (The post-open registration
hook at `runtime.rs:1874` exists precisely for providers that need an `Engine`
handle.)

| Tool | Args | Behavior |
|---|---|---|
| `send_message` | `to: <session_id>, body, mode?` (`steer`/`queue`, default `steer`) | Resolve the recipient session, check permission, register/find the sender's agent-producer on the recipient session, send with idempotency key derived from `(sender session, run, tool_call)` (same derivation pattern as `invocation_id`) |

Delivery is fully described by `mode`: `steer` (default) joins the recipient's
in-flight run the same way `steer_subagent` does; `queue` admits the message
durably and it is claimed into the recipient's prompt at the next safe
boundary. There is no inbox tool — agents never need to poll.

### Results, errors, and retries

A successful `send_message` result means **durably accepted** (decided): the
`ProducerMessageAccepted` event is persisted in the recipient's event log, so
delivery survives a daemon restart. It does *not* mean the recipient has seen
or consumed the message. The result carries:

- `message_id` — the durable producer message ID,
- `mode` — the effective delivery mode,
- `recipient_state` — the recipient's state at send time (`running`,
  `queued`, `waking_finished`).

Errors are explicit tool errors, one per case, so the sending model can react
predictably (retry, give up, or report to its parent):

- `unknown_session` — malformed or nonexistent session ID,
- `not_tree_peer` — session exists but `root_session_id` differs,
- `self_send` — sender targeted its own session,
- `permission_denied` — matched a deny rule or no rule (`ask` follows the
  existing approval flow instead of erroring),
- `invalid_body` — empty or over `max_body_bytes`,
- `inbox_full` — recipient has `max_pending_per_session` pending agent
  messages (see Configuration),
- `engine_shutdown` — send raced daemon shutdown.

Retries are safe: the idempotency key derives from `(sender session, run,
tool_call)`, so a retried call returns the **original** `message_id` instead
of delivering twice (existing producer dedup behavior).

### `steer_subagent` removal (v1)

`send_message` is a strict superset of `steer_subagent` (decided): steering a
running child is `send_message(mode=steer)`; steering a queued child is the
same durable `UserInputAdmitted` admission, prepended before the first run
starts; finished children, unreachable by `steer_subagent`, become reachable
via wake. In v1 the model-visible `steer_subagent` tool is removed from
`DelegateToolProvider`; `delegate_subagent`, `get_subagent_result`, and
`cancel_subagent` remain. The underlying `SessionCommand::Steer` machinery
stays — it is reused internally by `send_message` and by user steering
(`run.steer`). Consequences:

- Parent→child authority is now expressed entirely through `message`
  permission rules on the `child` relationship (e.g. `"child": allow` in the
  agent doc).
- **Intentional breaking change** (decided): combined with deny-by-default,
  the out-of-box default agent can delegate subagents but can no longer
  steer them until the user adds `message: { "child": allow }` to their
  agent doc. This is accepted as-is and must be called out in the migration
  notes; no built-in exemption.
- Agent docs, prompt sections, and guides referencing `steer_subagent` are
  updated in the same change.

`ToolResultTruncationPolicy` and concurrency settings mirror the delegate tools.

## Engine Changes

1. **New `ProducerOwner::Agent { session_id }` variant**
   (`protocol/src/producer.rs:17`). Additive payload change; the versionless,
   best-effort event log tolerates it and old readers degrade gracefully.
2. **Authorization predicate** beside `ensure_subagent_owned`:
   `ensure_tree_peer(sender, recipient)` — both sessions are tree peers iff
   their stored `SessionOrigin` metadata carries the same `root_session_id`.
   That is the entire contract (decided): no live-registry walk, no ancestry
   or taint validation. Known, accepted limitation: after a parent reverts
   history, "ghost" sessions from the reverted branch remain reachable by
   agents that hold their session IDs. Sender identity is always derived from
   the executing tool context, never from model-supplied arguments.
   The recipient may be in **any state**, including finished. `Running`
   recipients get steer/queue delivery; `Queued`/idle recipients get a durable
   admission claimed at start; `Finished`/`Completed` recipients are **woken**
   — the message is admitted as new input and the session resumes, following
   the queued-steer pattern (delivered as `UserInputAdmitted`).
3. **Engine API**: `engine.send_agent_message(invocation) -> AgentMessageHandle`
   in a new `messaging_api.rs`, internally reusing `ProducerCommand::Send`.
   Sender-side auto-registration of its `Agent` producer on the recipient,
   mirroring how background monitors register `ProducerOwner::Delegation` on
   parents (`delegation.rs:1669`).
4. **Loop safety**: the engine tracks an internal hop count per message chain
   (carried in producer metadata, not rendered in the agent-visible envelope)
   and rejects sends exceeding `config.runtime.messaging.max_hops`. The
   default is unlimited (`max_hops <= 0` disables the guard); operators who
   want loop protection can opt in. Per-pair rate window (default: max 4
   unacknowledged messages per directed pair) to stop ping-pong storms
   between two agents.
5. **Eviction interplay**: messages to an idle-evicted actor must spawn/wake its
   actor (`spawn_actor`, `mailbox.rs:782`). The existing producer path already
   wakes idle sessions; confirm the claim path covers this during
   implementation.

## Permissions

A **new `PermissionAction::Message`**, dedicated to `send_message` (decided) —
messaging is not overloaded onto `Delegate`, so steering a child and messaging
an agent remain independently controllable. The cost is known and bounded:
`protocol/src/agent.rs:110` enum, `engine/src/permissions.rs:135`
(`action_for_permission_name`), agent-doc parsing, TS bindings check, docs.

Rule shape (agent-doc frontmatter):

```yaml
permissions:
  message:
    "child": allow      # my delegated children
    "parent": allow     # the agent that delegated me
    "sibling": ask      # other children of my parent
    "*": deny           # any other same-tree peer (grandparent, uncle, ...)
```

- The permission resource label is the **recipient's relationship to the
  sender**: `parent`, `child`, `sibling`, or `*` (catch-all for any other
  same-tree peer). The label is derived from stored `SessionOrigin` metadata
  at send time and matched by the existing `PermissionPipeline` +
  `WildcardPattern`. This is purely a labeling step — there is **no
  relationship-based authority check** anywhere; permissions alone decide.
- **Deny by default, including built-in agents.** Built-in agent definitions
  ship without `message` rules, so deny-by-default applies and the
  `send_message` tool is not even visible to the model (`tool_visible`
  requires an allow/ask rule). Messaging is strictly opt-in: an agent doc must
  declare rules for the tool to appear.
- `ask` routes through the existing approval store and TUI approval flow.

## Configuration

```toml
[runtime.messaging]
enabled = true
max_hops = 0                  # <= 0 means unlimited (default); hop counting is internal metadata
max_body_bytes = 32768        # tighter than the 256 KiB plugin cap; agent mail should be terse
max_pending_per_session = 32  # pending agent mail per recipient; overflow → send rejected
max_inflight_per_pair = 4
```

**Inbox overflow** (decided): `max_pending_per_session` counts only **pending
agent mail** (accepted/admitted, not yet claimed-or-consumed) — plugin,
delegation-notification, and goal producer messages do not count against it,
and no agent message is ever discarded to make room. When the cap is reached,
`send_message` fails with the `inbox_full` tool error and the new message is
not admitted; the sender may retry later. No silent drops.

## Observability, TUI, Plugins

- **TUI**: message lifecycle events are durable, so `events.subscribe` already
  delivers them. Both delivery modes surface as producer events — `steer` and
  `queue` sends alike appear as `ProducerMessage*` rows (`owner == Agent`) in
  the transcript (mirroring how `DelegateQueued`/`DelegateFinished` reduce in
  `tui/src/state/mod.rs:2183-2224`), so users see steering and queueing
  exactly like existing producer traffic. Tree view gains a "messages"
  indicator.
- **Plugins**: zero new hooks needed — `plugin/event` observes all mail
  traffic; `tool_before_call` can gate/audit sends; `plugin/producer/send`
  already lets external systems inject into any session (precedent for
  external→agent mail).
- **Audit**: bodies are durable in the recipient's event log, so full mail
  history is reconstructable per session. A `session.mail` debug RPC could be
  added later; it would require the `PROTOCOL_VERSION` bump ritual — keep it
  out of v1.

## Resolved Decisions

1. **Scope: tree-only, equality check only.** Two sessions are tree peers iff
   their stored `SessionOrigin.root_session_id` values are equal. No live
   registry walk, ancestry validation, or taint reconciliation; sessions left
   over from a reverted branch remain messageable by agents holding their IDs.
   Any-session addressing is out of scope (it would need a global agent
   registry and new ownership rules) and conflicts with the no-discovery model.
2. **Permission action: new `send_message` action.** Messaging gets its own
   `PermissionAction` variant rather than reusing `Delegate`, so messaging
   permissions are controlled independently of delegation. The cost is
   accepted: `protocol/src/agent.rs:110` enum,
   `engine/src/permissions.rs:135` (`action_for_permission_name`), agent-doc
   parsing, TS bindings check, docs.
3. **Finished agents are messageable.** Sending to a `Finished`/`Completed`
   session wakes it: the message is admitted durably and the session resumes
   with the message as input, enabling follow-ups without the parent
   re-delegating with `resume_session_id`.
4. **`steer_subagent` is removed in v1.** `send_message` is a strict superset:
   it covers steering running and queued children and additionally reaches
   finished children (wake), siblings, and ancestors. Only the model-visible
   tool is removed; the engine's steer machinery stays and is reused.
   Parent→child authority moves to `Message` permission rules.
5. **Permissions: relationship labels, deny by default.** The `message`
   resource label is the recipient's relationship to the sender (`parent`,
   `child`, `sibling`, `*`), matched by ordinary wildcard rules — no
   relationship-based authority checks in the engine. Built-in agents ship
   without `message` rules: deny-by-default hides the tool entirely, so
   messaging is opt-in per agent doc. Combined with decision 4, the default
   agent loses out-of-box subagent steering until the user adds
   `message: { "child": allow }` — an accepted breaking change, documented in
   migration notes.
6. **Send result = durably accepted.** Success means the message is persisted
   (`ProducerMessageAccepted`) and returns `message_id`, effective `mode`, and
   `recipient_state`; failures are explicit tool errors; retries return the
   original `message_id` via idempotency dedup.
7. **Inbox overflow rejects the sender.** `max_pending_per_session` counts
   pending agent mail only; overflow fails the send with `inbox_full` and
   nothing is admitted or silently discarded.

## Phased Rollout

- **Phase 1**: `ProducerOwner::Agent`, `send_message` with both `steer`
  (default) and `queue` modes, tree-scoped auth, relationship-labeled
  `Message` permissions (deny-by-default, tool hidden without rules),
  `steer_subagent` tool removal with doc/prompt migration. TUI read-only
  visibility.
- **Phase 2**: rate/hop guards, `ask` approval UX.
- **Phase 3**: plugin mediation recipes, optional broadcast-to-children
  (`to: "children"`), cross-daemon story if it ever becomes real.

## Implementation Notes

Deliberately shallow — the mechanisms already exist; do not re-specify them:

- **Wake lifecycle**: waking a finished session is a fresh run on the same
  session, reusing the existing queued-steer machinery (durable
  `UserInputAdmitted` admission, run starts with the message as input). The
  completed delegation is not reopened; no new completion notification fires
  unless the woken agent sends one.
- **Delivery semantics**: `steer` and `queue` follow the existing producer and
  `SessionCommand::Steer` machinery unchanged in all recipient states. Races
  with completion or compaction resolve the same way they do for
  `steer_subagent` today.
- **Revert and fork**: mail events follow the event log's existing revert and
  fork behavior. No special handling, no cross-branch dedup or wake
  suppression.
- **Hop counting**: only meaningful when `max_hops > 0`; counting and
  rejection rules are defined when the guard is implemented (Phase 2).
- **Compatibility**: new `ProducerOwner::Agent` and `PermissionAction::Message`
  variants appear in live RPC responses, so `PROTOCOL_VERSION` is bumped and
  TS/JSON-schema bindings are regenerated in the same change, per the repo's
  version-bump ritual.

## Acceptance Criteria

Phase 1 is correct when all of the following hold, covered by tests:

1. An agent with `message: { "child": allow }` can `send_message` a running
   child (steer joins the next safe boundary) and a queued child (durable
   admission claimed at run start); an agent without `message` rules does not
   see the tool.
2. Sibling→sibling and child→parent sends work when rules allow them;
   `parent`/`child`/`sibling`/`*` labels resolve correctly from origin
   metadata.
3. A message to a finished session wakes it: the session resumes with the
   message as input; the completed delegation is untouched.
4. Tree authorization: equal `root_session_id` succeeds; mismatched roots fail
   with `not_tree_peer`; self-send fails with `self_send`.
5. Result contract: success returns `message_id`, effective `mode`, and
   `recipient_state`; a retried tool call returns the original `message_id`
   and delivers once; restart before delivery still delivers.
6. Limits: `invalid_body` at size boundaries, `inbox_full` at
   `max_pending_per_session` (agent mail only), deny-by-default visibility.
7. `steer_subagent` is gone from the tool surface; docs, prompt sections, and
   guides no longer reference it; the breaking change is in migration notes.
8. TUI renders `steer` and `queue` deliveries as `ProducerMessage*` mail rows
   in both live and replayed streams.
9. `PROTOCOL_VERSION` bumped; `check-bindings.sh` and docs stay in sync.

# Agent Messaging (Subagent-to-Subagent Communication)

Status: implemented. This page is the governing contract for agent-to-agent
messaging through the `send_message` tool. User-facing settings live in
[Agent Messaging [messaging]](../engine/messaging.md); the tool contract is
summarized in the [tool reference](../reference/tools.md#agent-messaging).

## Goals

- Let any agent in a delegation tree send a durable message to any other agent
  in the **same tree** (parent ↔ child, sibling ↔ sibling, child → grandparent).
- Delivery semantics: all delivery is producer-backed. The recipient receives
  the message either **steered** into its next safe model-request boundary (if
  running) or **queued** durably (if idle or queued). Messaging an idle or
  finished agent **wakes** it through the producer reconcile path: the message
  is admitted durably and the session resumes with it as input.
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

## Concepts

### Addressing

`send_message` targets a **raw `session_id` only** — the same raw-session
addressing model the removed `steer_subagent` tool used. No labels, aliases, or
special targets (`parent`, `root`).

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
was delivered to. Hop metadata (the loop-safety chain count described under
[Guards](#guards)) is internal producer metadata and is **not** rendered in the
envelope.

## Tool Surface

`MessageToolProvider` (provider id `builtin.message`), modeled on
`DelegateToolProvider`, is registered via `try_register_tool_provider` in
`crates/cookie_agent/src/main.rs` after engine open.

| Tool | Args | Behavior |
|---|---|---|
| `send_message` | `to: <session_id>, body, mode?` (`steer`/`queue`, default `steer`) | Resolve the recipient session, check permission, register/find the sender's agent-producer on the recipient session, send with idempotency key derived from `(sender session, run, tool_call)` (same derivation pattern as `invocation_id`) |

Model-facing arguments are exactly `to`, `body`, and `mode`; the argument
object is strict. The recipient argument is named `to`; it was renamed from
`recipient_session_id` during implementation with no alias — see
[Migration notes](#migration-notes).

Delivery is fully described by `mode`: `steer` (default) joins the recipient's
in-flight run at the next safe boundary through producer delivery; `queue`
admits the message durably and it is claimed into the recipient's prompt at
the next safe boundary. There is no inbox tool — agents never need to poll.

### Results, errors, and retries

A successful `send_message` result means **durably accepted** (decided): the
`ProducerMessageAccepted` event is persisted in the recipient's event log, so
delivery survives a daemon restart. It does *not* mean the recipient has seen
or consumed the message. The result carries:

- `message_id` — the durable producer message ID,
- `mode` — the effective delivery mode,
- `recipient_state` — the recipient's state at send time (`running`,
  `queued`, `waking_finished`).

Failures are explicit tool errors with stable codes in the `send_message:`
namespace, one per case, so the sending model can react predictably (retry,
give up, or report to its parent):

- `send_message:disabled` — `[runtime.messaging] enabled = false`, even when
  permission rules allow the send,
- `send_message:invalid_arguments` — missing, unknown, or wrongly typed
  arguments, including the removed `recipient_session_id` spelling,
- `send_message:invalid_body` — empty or over `max_body_bytes`,
- `send_message:unknown_session` — malformed or nonexistent session ID,
- `send_message:not_tree_peer` — session exists but `root_session_id` differs,
- `send_message:self_send` — sender targeted its own session,
- `send_message:inbox_full` — recipient has `max_pending_per_session` pending
  agent messages (see Configuration),
- `send_message:max_hops_exceeded` — the hop guard rejected the send (see
  [Guards](#guards)),
- `send_message:inflight_full` — the per-pair in-flight window is full (see
  [Guards](#guards)),
- `send_message:engine_shutdown` — send raced daemon shutdown.

Permission failures are not messaging-specific: a denied send follows the
generic permission-denied flow, and an `ask` rule routes through the existing
approval flow instead of erroring.

Retries are safe: the idempotency key derives from `(sender session, run,
tool_call)`, so a retried call returns the **original** `message_id` instead
of delivering twice (existing producer dedup behavior).

### `steer_subagent` removal

`send_message` is a strict superset of the removed `steer_subagent` tool
(decided): steering a running child is `send_message(mode=steer)`; steering a
queued child durably accepts the message and it is claimed when the child's
run starts; finished children, unreachable by `steer_subagent`, are reachable
via wake. The model-visible `steer_subagent` tool is removed from
`DelegateToolProvider`; `delegate_subagent`, `get_subagent_result`, and
`cancel_subagent` remain. The underlying `SessionCommand::Steer` machinery
stays for user steering (`run.steer`) only — `send_message` does not use it.
Consequences:

- Parent→child authority is expressed entirely through `message` permission
  rules on the `child` relationship (e.g. `"child": allow` in the agent
  doc).
- **Intentional breaking change** (decided): combined with deny-by-default,
  the out-of-box default agent can delegate subagents but cannot steer them
  until the user adds `message: { "child": allow }` to their agent doc. This
  is accepted as-is; see [Migration notes](#migration-notes). There is no
  built-in exemption.
- Agent docs, prompt sections, and guides were updated in the same change.

`send_message` uses bounded result truncation and `ToolConcurrency::Parallel`;
guard checks and acceptance are serialized inside the recipient actor.

## Engine Behavior

1. **`ProducerOwner::Agent { session_id }` variant**
   (`protocol/src/producer.rs`). Additive payload change; the versionless,
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
   admission claimed at start; `Finished`/`Completed` recipients are **woken**.
3. **Engine API**: `engine.send_agent_message(invocation) -> AgentMessageHandle`
   in `messaging_api.rs`, internally using the atomic
   `ProducerCommand::SendAgentMessage` path. Sender-side auto-registration of its
   `Agent` producer on the recipient mirrors how background monitors register
   `ProducerOwner::Delegation` on parents.
4. **Wake and delivery are producer-backed everywhere.** Idle and finished
   recipients wake through the producer reconcile path — the same reconcile
   that restarts delivery of accepted-but-unconsumed producer mail after idle
   eviction or restart — never through `SessionCommand::Steer` or a runless
   `UserInputAdmitted`. Messaging an idle-evicted actor therefore reuses the
   existing producer wake machinery (`spawn_actor`) with no special casing.
5. **Guards** — see [Guards](#guards).

### Guards

Two loop-safety guards reject sends at send time; both are opt-out:

- **Hop guard.** The engine tracks an internal hop count per message chain,
  carried in producer metadata and never rendered in the agent-visible
  envelope. A send that would exceed `runtime.messaging.max_hops` fails with
  `send_message:max_hops_exceeded`. The default is unlimited: `max_hops <= 0`
  disables the guard.
- **Per-pair in-flight window.** At most `max_inflight_per_pair`
  unacknowledged messages per directed pair (default 4) to stop ping-pong
  storms between two agents. A pair message is **unacknowledged** while it is
  accepted, admitted, claimed, or released, and **acknowledged** once it is
  consumed or discarded. A full window rejects the send with
  `send_message:inflight_full`. `max_inflight_per_pair = 0` disables the
  guard.

## Permissions

A dedicated **`PermissionAction::Message`** gates `send_message` (decided) —
messaging is not overloaded onto `Delegate`, so steering a child and messaging
an agent remain independently controllable.

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
max_hops = 0                  # <= 0 disables the hop guard (default); hop counting is internal metadata
max_body_bytes = 32768        # tighter than the 256 KiB plugin cap; agent mail should be terse
max_pending_per_session = 32  # pending agent mail per recipient; overflow → send rejected
max_inflight_per_pair = 4     # 0 disables the per-pair in-flight window
```

**Inbox overflow** (decided): `max_pending_per_session` counts only **pending
agent mail** (accepted, admitted, claimed, or released, but not yet consumed or
discarded) — plugin, delegation-notification, and goal producer messages do
not count against it, and no agent message is ever discarded to make room. When
the cap is reached, `send_message` fails with the `send_message:inbox_full`
tool error and the new message is not admitted; the sender may retry later. No
silent drops.

## Observability, TUI, Plugins

- **TUI**: message lifecycle events are durable, so `events.subscribe` already
  delivers them. Both delivery modes surface as producer events — `steer` and
  `queue` sends alike appear as `ProducerMessage*` rows (`owner == Agent`) in
  the transcript, so users see steering and queueing exactly like existing
  producer traffic.
- **Plugins**: zero new hooks — `plugin/event` observes all mail traffic;
  `tool_before_call` can gate/audit sends; `plugin/producer/send` already lets
  external systems inject into any session (precedent for external→agent
  mail).
- **Audit**: bodies are durable in the recipient's event log, so full mail
  history is reconstructable per session.

## Migration notes

There is no separate changelog; these notes are the migration record for the
messaging rollout.

1. **`steer_subagent` is removed.** The model-visible tool no longer exists.
   Its replacements:

   | Former call | Replacement |
   |---|---|
   | Steer a running child | `send_message { to, body, mode: "steer" }` (the default mode) |
   | Steer a queued child | `send_message { to, body }`; the message is durably accepted and claimed when the child's run starts |
   | Reach a finished child | Newly possible: `send_message` wakes finished sessions through producer reconcile |

   Parent→child authority moves from the `delegate` action to the `message`
   action with relationship labels. Agents that previously steered children
   need an explicit rule such as `message: { "child": allow }`; the default
   agent ships without one, so out-of-box subagent steering is denied until
   the rule is added. User steering through `run.steer` is unchanged.
2. **Recipient argument renamed without an alias.** The `send_message`
   recipient argument is `to`; the earlier `recipient_session_id` spelling is
   not accepted and fails with `send_message:invalid_arguments`. Because tool
   argument objects are strict, old calls fail closed, and prepared-operation
   grants issued under the old argument identity do not carry over — a
   previously granted "allow for the session tree" must be approved again
   under the new identity.
3. **Protocol version 19.** `ProducerMessageAccepted` gained the optional,
   internal `agent_hop` field in live RPC responses and durable events, so
   `PROTOCOL_VERSION` was bumped and the TS/JSON-schema bindings regenerated in
   the same change, per the repo's version-bump ritual.

## Resolved Decisions

1. **Scope: tree-only, equality check only.** Two sessions are tree peers iff
   their stored `SessionOrigin.root_session_id` values are equal. No live
   registry walk, ancestry validation, or taint reconciliation; sessions left
   over from a reverted branch remain messageable by agents holding their IDs.
   Any-session addressing is out of scope (it would need a global agent
   registry and new ownership rules) and conflicts with the no-discovery model.
2. **Permission action: dedicated `Message` action.** Messaging gets its own
   `PermissionAction` variant rather than reusing `Delegate`, so messaging
   permissions are controlled independently of delegation.
3. **Finished agents are messageable.** Sending to a `Finished`/`Completed`
   session wakes it through the producer reconcile path: the message is
   admitted durably and the session resumes with the message as input,
   enabling follow-ups without the parent re-delegating with
   `resume_session_id`.
4. **`steer_subagent` is removed.** `send_message` is a strict superset: it
   covers steering running and queued children and additionally reaches
   finished children (wake), siblings, and ancestors. Only the model-visible
   tool is removed; the engine's steer machinery stays for user steering.
   Parent→child authority moves to `Message` permission rules.
5. **Permissions: relationship labels, deny by default.** The `message`
   resource label is the recipient's relationship to the sender (`parent`,
   `child`, `sibling`, `*`), matched by ordinary wildcard rules — no
   relationship-based authority checks in the engine. Built-in agents ship
   without `message` rules: deny-by-default hides the tool entirely, so
   messaging is opt-in per agent doc. Combined with decision 4, the default
   agent loses out-of-box subagent steering until the user adds
   `message: { "child": allow }` — an accepted breaking change, documented in
   [Migration notes](#migration-notes).
6. **Send result = durably accepted.** Success means the message is persisted
   (`ProducerMessageAccepted`) and returns `message_id`, effective `mode`, and
   `recipient_state`; failures are explicit `send_message:`-namespaced tool
   errors plus the generic permission-denied flow; retries return the original
   `message_id` via idempotency dedup.
7. **Inbox overflow rejects the sender.** `max_pending_per_session` counts
   pending agent mail only (including claimed and released mail until it is
   consumed or discarded); overflow fails the send with
   `send_message:inbox_full` and nothing is admitted or silently discarded.
8. **Guards are enforced and opt-out.** The hop guard (`max_hops`) and the
   per-pair in-flight window (`max_inflight_per_pair`) reject overshooting
   sends with `send_message:max_hops_exceeded` and `send_message:inflight_full`.
   `max_hops <= 0` and `max_inflight_per_pair = 0` disable their guard. Hop
   counting is internal metadata, never part of the delivered envelope, and a
   pair message counts as unacknowledged from acceptance until it is consumed
   or discarded.

## Phased Rollout

- **Phase 1** (implemented): `ProducerOwner::Agent`, `send_message` with both
  `steer` (default) and `queue` modes, tree-scoped auth, relationship-labeled
  `Message` permissions (deny-by-default, tool hidden without rules),
  `steer_subagent` tool removal with doc/prompt migration. TUI read-only
  visibility.
- **Phase 2** (implemented): the hop guard and per-pair in-flight window are
  enforced with the semantics under [Guards](#guards); `ask` approvals use the
  existing approval flow.
- **Phase 3** (future): plugin mediation recipes, optional broadcast-to-children
  (`to: "children"`), cross-daemon story if it ever becomes real.

## Acceptance Criteria

The implementation is correct when all of the following hold, covered by tests:

1. An agent with `message: { "child": allow }` can `send_message` a running
   child (steer joins the next safe boundary) and a queued child (durable
   admission claimed at run start); an agent without `message` rules does not
   see the tool.
2. Sibling→sibling and child→parent sends work when rules allow them;
   `parent`/`child`/`sibling`/`*` labels resolve correctly from origin
   metadata.
3. A message to a finished session wakes it through producer reconcile: the
   session resumes with the message as input; the completed delegation is
   untouched.
4. Tree authorization: equal `root_session_id` succeeds; mismatched roots fail
   with `send_message:not_tree_peer`; self-send fails with
   `send_message:self_send`.
5. Result contract: success returns `message_id`, effective `mode`, and
   `recipient_state`; a retried tool call returns the original `message_id`
   and delivers once; restart before delivery still delivers.
6. Limits and guards: `send_message:invalid_body` at size boundaries,
   `send_message:inbox_full` at `max_pending_per_session` (agent mail only),
   `send_message:max_hops_exceeded` and `send_message:inflight_full` at their
   configured limits with `max_hops <= 0` / `max_inflight_per_pair = 0`
   disabling each guard, and deny-by-default visibility.
7. `steer_subagent` is gone from the tool surface; docs, prompt sections, and
   guides no longer reference it; the breaking change is in
   [Migration notes](#migration-notes).
8. TUI renders `steer` and `queue` deliveries as `ProducerMessage*` mail rows
   in both live and replayed streams.
9. `PROTOCOL_VERSION` bumped to 19; `check-bindings.sh` and docs stay in sync.

## Implementation Notes

Deliberately shallow — the mechanisms already exist; do not re-specify them:

- **Wake lifecycle**: waking an idle or finished session reuses the producer
  reconcile path: accepted-but-unconsumed mail makes the session deliverable,
  the actor is spawned if evicted, and a fresh run starts on the same session
  with the message as input. No `SessionCommand::Steer` or runless
  `UserInputAdmitted` is involved. The completed delegation is not reopened;
  no new completion notification fires unless the woken agent sends one.
- **Delivery semantics**: `steer` and `queue` follow the existing producer
  machinery unchanged in all recipient states. Races with completion or
  compaction resolve the same way they do for other producer traffic.
- **Revert and fork**: mail events follow the event log's existing revert and
  fork behavior. No special handling, no cross-branch dedup or wake
  suppression.
- **Hop counting**: meaningful only when `max_hops > 0`; the count travels as
  internal producer metadata between chained sends and is rejected at
  `send_message:max_hops_exceeded` when it would exceed the limit.
- **Compatibility**: `ProducerMessageAccepted` gained the optional, internal
  `agent_hop` field in live RPC responses and durable events, so
  `PROTOCOL_VERSION` was bumped to 19 and TS/JSON-schema bindings regenerated
  in the same change, per the repo's version-bump ritual.

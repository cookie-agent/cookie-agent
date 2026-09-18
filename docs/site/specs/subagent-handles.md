# Subagent Handles (Short Session References)

Status: draft proposal. Not implemented. This document records the design
decisions for replacing raw 36-character UUID session references in the
model-facing delegation tools with short, tree-unique **handles**. It amends
the addressing contract in [Agent messaging](agent-messaging.md) and the tool
contracts in the [tool reference](../reference/tools.md).

## Problem

Subagent session references on the model-facing path are full 36-character
hyphenated UUIDv7 strings, surfaced verbatim in tool results,
`<subagent_notification>` envelopes, and `<agent_message>` envelopes, and
resolved by strict exact-match parsing only. Models — especially smaller or
faster ones — frequently fail to reproduce these strings exactly, producing
serde parse errors with no recovery guidance.

Prior art, from the research that informed this design:

- **Claude Code** uses short slug-prefixed subagent IDs (`a<slug>-<16 hex>`,
  not UUIDs), delivers them to the model pre-wrapped as a copy-paste fragment,
  steers away from ID typing via push delivery and human names, and appends the
  live list of running agents to ID-lookup error messages so a failed lookup
  self-repairs in one step.
- **opencode** uses 30-char opaque base62 session IDs with exact-match lookup
  only, and its issue tracker documents repeated failures where weaker models
  fabricate UUID-shaped `task_id` values. Its response was to remove ID-based
  polling from the happy path entirely (results are pushed).

Lessons adopted: short self-describing handles; push stays primary; resolution
errors must list live candidates; a reference that does not resolve must never
silently fall through to creating a fresh subagent (the opencode failure
class).

## Goals

- Let the model reference any session in its own delegation tree with a short
  handle it can reliably copy: `<agent_type>_<8 hex>` (≤ 25 characters).
- Guarantee handle uniqueness within the session tree at generation time, with
  a bounded regeneration loop.
- Keep the internal `SessionId` (UUIDv7) and all persistence layouts unchanged.
  The handle is a presentation-and-resolution layer, not a storage change.
- Make resolution failures self-repairing: an unresolvable reference produces
  an error that lists the caller's live subagents with their handles,
  descriptions, and statuses.
- Scope every resolution to the caller's own tree. A handle or prefix from
  another tree, or a fabricated value, never resolves.

## Non-Goals

- Changing `SessionId`, directory names, event-log envelopes, approval
  fingerprints, run IDs, TS/JSON schema bindings, or the WebSocket RPC surface.
- Push/pull architecture changes. The existing `<subagent_notification>` push
  path stays primary; this spec fixes the ergonomics of the pull tools.
- Cross-tree addressing, agent discovery, or human-assigned aliases.
- Migration of pre-change sessions (see Compatibility).

## Handle Format

```
<agent_type>_<8 lowercase hex>
explorer_1a2b3c4d
coder_9f8e7d6b
```

Grammar: `^[a-z0-9](?:[a-z0-9]|-(?=[a-z0-9])){0,15}_[0-9a-f]{8}$`

- `agent_type` is the agent's existing slug identity from the `identity` crate
  (`^[a-z0-9](?:[a-z0-9]|-(?=[a-z0-9]))*$`, max 64 chars). It is already
  LLM-friendly and makes handles self-describing.
- The slug portion in the handle is capped at 16 characters. Custom agent
  types may be longer; uniqueness rests on the hex suffix, and the full type
  name remains available in metadata.
- 8 hex characters = 32 bits of entropy. Per-tree birthday bound: a tree of
  500 sessions has ≈1.5% cumulative collision probability before the
  regeneration loop; the check-then-regenerate loop makes effective collisions
  negligible.

## Generation And Tree Uniqueness

**Invariant**: every session has a `short_id`, unique among all sessions in
its tree (root plus all descendants).

- **Single persistence/generation point**: both root sessions
  (`crates/engine/src/runtime/sessions.rs`, `create_session`) and delegated
  children (`crates/engine/src/runtime/admission.rs`, `create_child`) open
  their own event log with `Event::SessionCreated`. The handle is generated
  at `SessionCreated` time and carried as one **optional additive `short_id`
  field on `Event::SessionCreated`** — a single field covers all sessions
  uniformly, and replay rebuilds it from the session's first event.
- **Collision check**: walk the existing tree via `Engine::tree` /
  `Engine::children` (`crates/engine/src/runtime/sessions.rs:267-335`), which
  already force-loads persisted-but-not-materialized sessions
  (`ensure_tree_loaded`), and collect existing `short_id`s. Regenerate on
  collision. **Cap the loop at 16 attempts**, then fail loudly. Reaching the
  cap is unreachable in practice and indicates the uniqueness check itself is
  broken.
- Root sessions go through the same path so the "every session has a
  `short_id`" invariant holds uniformly; a new tree is empty, so the root's
  first handle is trivially unique.

## Persistence

The handle is generated, so it must be recorded; it is not derivable after the
fact.

- One **optional additive `short_id` field on `Event::SessionCreated`** (see
  Generation And Tree Uniqueness). Additive-optional changes pass the
  additive-schema checks in `crates/protocol/scripts/` without a
  `PROTOCOL_VERSION` bump.
- Old sessions (pre-change logs) have no `short_id`. They resolve by full UUID
  only. No migration, consistent with the no-silent-migration rule in
  `AGENTS.md`.

## Resolution Contract

All four ID-taking parameters — `get_subagent_result.session_id`,
`cancel_subagent.session_id`, `send_message.session_id`, and
`delegate_subagent.resume_session_id` — resolve in this order:

1. **Full UUID parse** → exact match. Unchanged current behavior; covers old
   sessions.
2. **Handle exact match** within the caller's tree (`explorer_1a2b3c4d`).
3. **Anything else → self-repairing error**: the error lists the caller's
   live subagents as `handle — description — status`, plus the full UUID of
   each. There is deliberately **no prefix matching**: resolution accepts
   exactly two forms, a full UUID or a complete handle, and nothing in
   between. A partial handle is just an error, with the candidate list as
   recovery.

Security: resolution is always scoped to the caller's own tree. A handle from
another tree or a fabricated value never resolves and never falls through to
creating a new subagent.

## Tool Signatures

Existing shapes are unchanged; only the ID parameters' semantics and
descriptions change. Descriptions carry the ergonomic load: they state the
format and give a worked example, so the model needs nothing from prior-turn
prose.

### `delegate_subagent`

```json
{
  "name": "delegate_subagent",
  "description": "Delegate a self-contained task to a specialist agent. Foreground (default) blocks until done. background=true returns immediately with a session handle; the result is pushed back automatically as a <subagent_notification>, and you can also fetch it with get_subagent_result. To continue an existing subagent, pass its resume_session_id (see the handle in its start/completion notice).",
  "parameters": {
    "type": "object",
    "properties": {
      "description": { "type": "string", "description": "Short (3-5 words) summary of the task" },
      "prompt": { "type": "string", "description": "Full task brief with objective, context, and deliverable" },
      "agent_type": { "enum": ["explore", "sub-architact", "sub-coder", "sub-debugger", "sub-designer", "sub-reviewer", "sub-writer"] },
      "background": { "type": "boolean", "default": false },
      "resume_session_id": {
        "type": "string",
        "description": "Optional. Handle or UUID of an existing subagent of yours to resume, e.g. \"explore_1a2b3c4d\". Only subagents you delegated are valid."
      },
      "inherit_context": { "type": "boolean", "default": false }
    },
    "required": ["description", "prompt", "agent_type"]
  }
}
```

### `get_subagent_result`

```json
{
  "name": "get_subagent_result",
  "description": "Check the status of a subagent you delegated. If it is still running, the result says so (use wait=true to block until it ends its turn). Once it has ended, the result contains the last assistant message the subagent emitted, along with its terminal status. Use the handle from the subagent's start or completion notice, e.g. \"explore_1a2b3c4d\". Only your own subagents are visible.",
  "parameters": {
    "type": "object",
    "properties": {
      "session_id": {
        "type": "string",
        "description": "Handle (agent_type + 8 hex, e.g. \"coder_9f8e7d6b\") or full UUID of one of your subagents."
      },
      "wait": { "type": "boolean", "default": false },
      "offset": { "type": "integer", "default": 0 },
      "limit": { "type": "integer", "default": 2000 }
    },
    "required": ["session_id"]
  }
}
```

### `cancel_subagent`

```json
{
  "name": "cancel_subagent",
  "description": "Cancel a subagent you delegated. Accepts the same handle or UUID forms as get_subagent_result.",
  "parameters": {
    "type": "object",
    "properties": {
      "session_id": { "type": "string", "description": "Handle or full UUID of one of your subagents." },
      "reason": { "type": "string" }
    },
    "required": ["session_id"]
  }
}
```

### `send_message`

```json
{
  "name": "send_message",
  "description": "Send a message to another agent in your session tree. Recipient accepts a subagent handle or a full UUID.",
  "parameters": {
    "type": "object",
    "properties": {
      "session_id": { "type": "string", "description": "Handle or full UUID of the recipient agent." },
      "body": { "type": "string" },
      "mode": { "enum": ["steer", "queue"], "default": "steer" }
    },
    "required": ["session_id", "body"]
  }
}
```

**No `pattern` constraint** on ID strings: the schema stays loose (plain
`string`) and the resolver is strict, so a malformed reference produces a
recoverable resolution error with the candidate list rather than a hard
schema failure. Loose schema, strict resolver, helpful error is the intended
layering.

## Surfacing

Every place that currently prints the 36-char UUID prints the handle instead,
plus a pre-wrapped fragment:

```
Subagent started. [subagent session explore_1a2b3c4d]
use get_subagent_result with session_id "explore_1a2b3c4d"

[subagent session explore_1a2b3c4d; completed; 79 lines; use get_subagent_result with session_id "explore_1a2b3c4d" for the full output]
```

- Metadata keeps the **full UUID** under a unified `session_id` key (also
  fixing the current `child_session_id` inconsistency on the cancel/failure
  paths), and adds the handle: `{"session_id": "<uuid>", "handle": "explore_1a2b3c4d", ...}`.
- `<subagent_notification>` and `<agent_message>` envelopes carry the handle
  alongside the UUID.
- The TUI `short_id` helper (`crates/tui/src/ui/pickers.rs`) switches from
  first-8-of-UUID to the real handle.

### Notification Parity

The pushed `<subagent_notification>` message carries the **same body the
foreground `delegate_subagent` tool result carries** — the preview, blank
line, and bracketed teaser with the pre-wrapped `get_subagent_result`
fragment, only with the handle in place of the UUID:

```
<subagent_notification>
<first ~20 lines of preview>

[subagent session explore_1a2b3c4d; completed; 79 lines; use get_subagent_result with session_id "explore_1a2b3c4d" for the full output]
</subagent_notification>
```

Rationale: the model learns one shape instead of two. Whatever it sees in a
foreground tool result, it sees verbatim in the background push.

This renders at two sites, both of which must produce byte-identical output
from the same persisted fields (the stored-body comparison used for dedup and
replay requires deterministic rendering):

- Runtime path: `render_background_completion`
  (`crates/engine/src/runtime/delegation.rs:3175-3184`).
- History replay path: the `DelegateFinished` / `DelegateFinishedV2`
  renderers (`crates/engine/src/model_history.rs:1084-1118`), used when
  rebuilding history for sessions whose completion predates the producer
  message or replaying a persisted log.

Both renderers share one formatting function keyed off the same teaser
fields (`preview`, `status`, `total_lines`, handle) so the two sites cannot
drift.

### Result Truncation

Both delegate result surfaces are already internally bounded — the teaser
preview is capped (`sanitize_safe_text`, 2048 bytes / 20 lines) — so both opt
out of the generic tool-result truncation pipeline:

- `delegate_subagent`'s terminal result opts out, matching `get_subagent_result`'s
  existing opt-out (`result_truncation_policy`, `crates/tools/src/delegate.rs`).
- The `<subagent_notification>` body is delivered as-is.

No surface in the delegation flow is truncated twice.

## Lifecycle Semantics

`get_subagent_result` reports **session liveness**, not delegation lifecycle.
This is a deliberate simplification of the current engine behavior (which
derives status from the delegation record and freezes the delegation run's
`final_text`): the implementer is free to restructure the implementation
however simplifies the code. **No backward compatibility with the current
semantics is required.**

Pinned contract:

- **Subagent running** (including queued/starting, and including a finished
  subagent that was woken into a new turn by a `send_message`): the tool
  reports that the subagent is still running and returns no final text. With
  `wait: true`, it blocks until the subagent ends its turn, then behaves as
  below.
- **Subagent ended**: the tool returns the **last assistant message emitted
  by the subagent** (the final text of its most recent turn), along with the
  terminal status. The message text is paginated by the existing `offset` /
  `limit` parameters, and this tool keeps its existing opt-out of tool-result
  truncation: a long final message is delivered across paginated reads in
  full, never silently truncated.
- A steer/wake into a finished child does not create a new delegation
  invocation, but it does put the session back into a running state: a
  subsequent `get_subagent_result` therefore reports "still running" until the
  steered turn ends, at which point the returned text is the last assistant
  message of that new turn.
- The tool is not a frozen snapshot of any particular run; it always reflects
  the session's current state.

If a future need arises to read earlier turns' output, that should be a new
tool or an explicit parameter (e.g. `run: "latest"`), not a silent change to
this contract.

## Compatibility

- `SessionId` remains UUIDv7 everywhere internally: directory names, event-log
  envelopes, approval fingerprints, `delegate:{invocation_id}` run IDs, TS
  bindings, WebSocket RPC.
- Stored model-history text from old sessions keeps showing the old UUID
  format; those references still resolve via step 1.
- Old sessions without a `short_id` resolve by full UUID only.

## Validation Plan

- Unit: generation-loop collision behavior and the 16-attempt cap; resolution
  order; ambiguity rejection; cross-tree isolation; old-UUID-only sessions.
- Snapshot tests (`insta`) for tool-result text and notification rendering.
- `crates/protocol/scripts/check-bindings.sh --check` for the additive schema
  field.
- Full `AGENTS.md` gates: locked build/test, fmt, clippy (stable + MSRV).

## Decisions Log

- **No prefix matching**: resolution accepts exactly two forms — a full UUID
  or a complete handle. A partial handle is an error with the candidate list
  as recovery.
- **Slug cap confirmed at 16 characters** for the agent-type portion of the
  handle; longer custom agent types keep the full name in metadata only.
- **Pagination kept**: `get_subagent_result` keeps `offset` / `limit` and its
  opt-out of tool-result truncation, now paginating the last assistant
  message rather than accumulated output.
- **Notification parity**: the `<subagent_notification>` body is the
  foreground `delegate_subagent` result body (teaser + pre-wrapped fragment,
  handle in place of UUID), rendered identically at the runtime
  (`render_background_completion`) and history-replay
  (`model_history.rs:1084-1118`) sites via one shared formatting function.
- **No double truncation**: `delegate_subagent`'s terminal result opts out of
  the generic tool-result truncation pipeline, like `get_subagent_result`
  already does — the teaser's internal preview cap is the only truncation.

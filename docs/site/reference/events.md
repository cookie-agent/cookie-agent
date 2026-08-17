# Event Reference

Session persistence and subscriptions write event schema 21 and reopen schemas
15-21. Every stored event
contains `event_schema_version`, `session_id`, required nullable `run_id`, a
positive physical `seq`, `timestamp`, and a tagged `payload`. Events requiring a
run ID reject a null envelope value.

## Payloads

| Category | Event payload types |
|---|---|
| Session | `session_created`, `session_reverted`, `session_permission_overlay_set`, `skill_loaded`, `skill_invocation_noted`, `session_title_committed`, `delegated_context_seeded` |
| User input | `user_input_admitted`, `user_input_submitted`, `user_input_recalled`, `user_input_applied` |
| Run | `run_started`, `run_completed`, `run_failed`, `run_cancelled`, `run_interrupted` |
| Model | `model_attempt_started`, `text_delta`, `reasoning_delta`, `attempt_abandoned`, `model_replay_evaluated`, `model_turn_committed`, `model_usage_recorded`, `model_fallback` |
| Tools | `tool_call_started`, `tool_call_progress`, `tool_call_terminated`, `tool_output_elided`, `tool_stdin_submitted`, `tool_call_linked`, `delegate_queued`, `delegate_finished`, `delegate_finished_v2` |
| Approvals | `approval_requested`, `approval_evaluated`, `approval_escalated`, `approval_user_decision_recorded`, `approval_finalized`, `approval_cancelled`, `approval_doom_loop_detected`, `tree_approval_grant_committed` |
| Internal agents | `internal_agent_started`, `internal_agent_usage_recorded`, `internal_agent_completed`, `internal_agent_failed`, `internal_agent_cancelled`, `internal_agent_interrupted`, `internal_agent_fallback` |
| Compaction | `context_checkpoint_committed`, `context_rehydrated`, `context_compaction_auto_disabled` |

`context_compaction_auto_disabled` is a legacy durable event retained for old
session logs. Current engines no longer emit it.

Schema 15 adds `delegate_queued` and `delegate_finished`. The completion event is
written to the parent run and carries `{ session_id, status, preview,
total_lines }`; `preview` is limited to the first 20 lines and 2 KiB. It becomes
model-visible at the next turn boundary, while full output remains available
through `get_subagent_result`.

Schema 16 adds `delegated_context_seeded` and `delegate_finished_v2`.
`delegated_context_seeded` is a runless creation event containing the bounded,
text-only user/assistant turns copied by `inherit_context`; it must precede the
child's first run and deterministically rebuilds the same initial model history
after restart. `delegate_finished_v2` adds the delegation `invocation_id` to the
same completion payload so repeated resumes of one child each receive exactly
one teaser. Current engines emit the V2 completion form.

Schema 18 adds the runless `session_permission_overlay_set` event. Its `overlay`
contains the complete, validated set of unique `(action, resource)` rules for
the session. The latest visible event wins. Because it is an ordinary branch
event, restart replay, revert, and fork recover the same overlay without a
sidecar file.

Schema 19 adds `model_usage_recorded`. It follows a committed provider turn and
contains the positive `model_turn_seq`, agent ID, resolved model identity, and
Oven's normalized usage fields: inclusive input/output totals plus optional
uncached input, cache-read input, cache-write input, text output, and reasoning
output counts. Event-log validation requires its run, agent, model, and turn to
match and rejects duplicate records for one turn. Session, agent, and global
usage projections rebuild from these events after restart, revert, and fork.

Schema 20 adds `internal_agent_usage_recorded`. It attributes each completed
internal model request to its internal run, internal agent ID, kind, and resolved
model. The same session, agent, and global projections include these records.

Schema 21 adds `skill_loaded` and `skill_invocation_noted`. `skill_loaded`
stores the rendered body, skill name, source path, arguments, base directory,
and at most ten supporting-file paths. It is pinned across context checkpoints.
An identical repeat uses the shorter `skill_invocation_noted` payload.

`user_input_admitted` normally belongs to an active run. A steer accepted for a
queued delegated child is runless and requires that the session have no active
run. It is persisted immediately and enters the next run's pending-input FIFO,
including when a terminal child is queued for resume. A queued cancellation
appends matching runless `user_input_recalled` events so those inputs cannot leak
into a later resume. Promotion appends the ordinary run-owned
`user_input_submitted` event, so transcript rendering and model history use the
same path as interactive run steering.

Running-resume rollback appends `user_input_recalled_v2` with the exact
`user_input_seq` of its admission. Replay removes that specific pending input
and preserves direct steers admitted after it. Legacy `user_input_recalled`
continues to recall the newest pending input.

## Subscriptions

`events.subscribe` returns all retained events after the optional cursor and
starts `events.subscription` notifications. A notification is tagged as either:

```json
{ "type": "event", "event": { "event_schema_version": 21 } }
```

or a gap indicating that the subscriber must rebuild its disposable projection:

```json
{
  "type": "gap",
  "session_id": "...",
  "last_delivered_seq": 42
}
```

Revert markers are delivered like every other physical event. Clients should
rebuild branch-derived state when one arrives; they must not assume sequence
numbers were truncated or reused.

Tool stdout and stderr use separate snapshot, delta, and gap notifications.
Offsets are byte offsets, and clients use a snapshot after a gap before applying
later deltas.

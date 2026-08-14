# Event Reference

Session persistence and subscriptions use event schema 15. Every stored event
contains `event_schema_version`, `session_id`, required nullable `run_id`, a
positive physical `seq`, `timestamp`, and a tagged `payload`. Events requiring a
run ID reject a null envelope value.

## Payloads

| Category | Event payload types |
|---|---|
| Session | `session_created`, `session_reverted`, `session_title_committed` |
| User input | `user_input_admitted`, `user_input_submitted`, `user_input_recalled`, `user_input_applied` |
| Run | `run_started`, `run_completed`, `run_failed`, `run_cancelled`, `run_interrupted` |
| Model | `model_attempt_started`, `text_delta`, `reasoning_delta`, `attempt_abandoned`, `model_replay_evaluated`, `model_turn_committed`, `model_fallback` |
| Tools | `tool_call_started`, `tool_call_progress`, `tool_call_terminated`, `tool_output_elided`, `tool_stdin_submitted`, `tool_call_linked`, `delegate_queued`, `delegate_finished` |
| Approvals | `approval_requested`, `approval_evaluated`, `approval_escalated`, `approval_user_decision_recorded`, `approval_finalized`, `approval_cancelled`, `approval_doom_loop_detected`, `tree_approval_grant_committed` |
| Internal agents | `internal_agent_started`, `internal_agent_completed`, `internal_agent_failed`, `internal_agent_cancelled`, `internal_agent_interrupted`, `internal_agent_fallback` |
| Compaction | `context_checkpoint_committed`, `context_rehydrated`, `context_compaction_auto_disabled` |

`context_compaction_auto_disabled` is a legacy durable event retained for old
session logs. Current engines no longer emit it.

Schema 15 adds `delegate_queued` and `delegate_finished`. The completion event is
written to the parent run and carries `{ session_id, status, preview,
total_lines }`; `preview` is limited to the first 20 lines and 2 KiB. It becomes
model-visible at the next turn boundary, while full output remains available
through `get_subagent_result`.

## Subscriptions

`events.subscribe` returns all retained events after the optional cursor and
starts `events.subscription` notifications. A notification is tagged as either:

```json
{ "type": "event", "event": { "event_schema_version": 15 } }
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

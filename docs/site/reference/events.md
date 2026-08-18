# Event Reference

Session persistence and subscriptions use a versionless event format. Every new
stored event contains `engine_version`, `session_id`, required nullable `run_id`,
a positive physical `seq`, `timestamp`, and a tagged `payload`. The
`engine_version` is the writing engine's binary semver; it is diagnostic metadata,
not a compatibility gate, and old records may omit it. Legacy
`event_schema_version` fields are ignored when reading.

Readers handle each JSONL line independently. Fully readable events load
normally. If a known event has an absent or type-mismatched optional field, that
field defaults and the event loads with an in-memory degradation diagnostic.
Unknown event tags, broken required fields, corrupt JSON, and unknown envelope
fields cause that event to be skipped. Sequence numbers must increase strictly,
but gaps are retained and reported rather than making the session unreadable.
Each contiguous missing sequence range produces one bounded diagnostic. An
unreadable initial `session_created` record is the exception: without creation
identity the session cannot open, so the engine reports an unsupported-era or
corrupt-log error.

Writers remain strict: every appended event passes validation. Event evolution
is additive only. Existing variant tags and fields are never removed or renamed,
existing required fields remain required, and new fields must be optional. New
behavior that cannot fit an optional field uses a new variant tag. CI compares
the generated `EventPayload` schema with the committed additive baseline.

## Payloads

| Category | Event payload types |
|---|---|
| Session | `session_created`, `session_reverted`, `session_permission_overlay_set`, `skill_loaded`, `skill_invocation_noted`, `session_title_committed`, `delegated_context_seeded` |
| Plugins | `plugin_event_added`, `plugin_diagnostic` |
| User input | `message_injected`, `user_input_admitted`, `user_input_submitted`, `user_input_transformed`, `user_input_recalled`, `user_input_recalled_v2`, `user_input_applied` |
| Run | `run_started`, `run_completed`, `run_failed`, `run_cancelled`, `run_interrupted` |
| Model | `model_attempt_started`, `model_request_prepared`, `text_delta`, `reasoning_delta`, `attempt_abandoned`, `model_replay_evaluated`, `model_turn_committed`, `model_usage_recorded`, `model_fallback` |
| Tools | `tool_call_started`, `tool_call_progress`, `tool_call_terminated`, `tool_output_elided`, `tool_stdin_submitted`, `tool_call_linked`, `delegate_queued`, `delegate_finished`, `delegate_finished_v2`, `delegate_child_terminated` |
| Delegation durability | `delegation_reserved`, `delegation_started`, `delegation_run_started`, `delegation_run_attached`, `delegation_finished` |
| Approvals | `approval_requested`, `approval_evaluated`, `approval_escalated`, `approval_user_decision_recorded`, `approval_finalized`, `approval_cancelled`, `approval_doom_loop_detected`, `tree_approval_grant_committed` |
| Internal agents | `internal_agent_started`, `internal_agent_usage_recorded`, `internal_agent_completed`, `internal_agent_failed`, `internal_agent_cancelled`, `internal_agent_interrupted`, `internal_agent_fallback` |
| Compaction | `context_checkpoint_committed`, `context_rehydrated`, `context_compaction_auto_disabled` |

`context_compaction_auto_disabled` is a legacy durable event retained for old
session logs. Current engines no longer emit it.

`plugin_event_added` is a runless plugin publication containing `plugin`, `name`, and arbitrary
JSON `payload`. Plugin-originated payloads are capped at 256 KiB, names at 128 characters, and the
complete serialized event at 272 KiB; per-plugin/session rate and byte quotas apply. The event is normal
branch content: it survives reopen and fork, is visible to model history and compaction, and is
tombstoned by revert. `plugin_diagnostic` records operational notices such as interception
timeouts, hook blocks, invalid modifications, oversized publications, and dropped stream counts.

Delegation persistence includes `delegate_queued` and `delegate_finished`. The completion event is
written to the parent run and carries `{ session_id, status, preview,
total_lines }`; `preview` is limited to the first 20 lines and 2 KiB. It becomes
model-visible at the next turn boundary, while full output remains available
through `get_subagent_result`.

Later delegation records add `delegated_context_seeded` and `delegate_finished_v2`.
`delegated_context_seeded` is a runless creation event containing the bounded,
text-only user/assistant turns copied by `inherit_context`; it must precede the
child's first run and deterministically rebuilds the same initial model history
after restart. `delegate_finished_v2` adds the delegation `invocation_id` to the
same completion payload so repeated resumes of one child each receive exactly
one teaser. Current engines emit the V2 completion form.

The `delegation_*` lifecycle records are durable control events on the parent
session. `delegation_reserved` contains the complete current delegate request,
child agent snapshot, selected model suffix, runtime revisions, request
fingerprint, and optional typed staged-skill provenance.
`delegation_started` records publication of the child session;
`delegation_run_started` and `delegation_run_attached` distinguish a new child
run from attachment to an already-running resumed child. `delegation_finished`
records the terminal status, optional recovery reason, and child session/run
references. The existing
`delegate_queued` and `delegate_finished_v2` events remain the model-visible
queue and result-summary records.

Engine startup projects delegation state while opening session logs. A reserved
or started delegation without a terminal event resumes or rolls back according
to its child and parent state. Recovery recomputes the reservation fingerprint;
a mismatch rejects the reservation. If best-effort reading skips a corrupt
delegation event, that delegation is not recovered, the parent session records
the diagnostic, and unrelated delegations remain recoverable. Legacy
`delegations.jsonl` files are ignored, so pre-release in-flight journal entries
do not cross this upgrade.

The runless `session_permission_overlay_set` event's `overlay`
contains the complete, validated set of unique `(action, resource)` rules for
the session. The latest visible event wins. Because it is an ordinary branch
event, restart replay, revert, and fork recover the same overlay without a
sidecar file.

`model_usage_recorded` follows a committed provider turn and
contains the positive `model_turn_seq`, agent ID, resolved model identity, and
Oven's normalized usage fields: inclusive input/output totals plus optional
uncached input, cache-read input, cache-write input, text output, and reasoning
output counts. The optional `estimated_cost_pico_usd` stamps the engine-selected
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

`tool_call_progress` contains the existing control-free `message` plus an optional
control-free `output_chunk`. Bash coalesces stdout and stderr for up to 50 ms or
4 KiB before emitting chunks; each durable chunk is bounded by the 1 KiB
`SafeDisplayText` limit. The display preview stops after 1 MiB per call and one
progress message marks the truncation. The terminal tool result and any
`tool_output_elided` artifact remain authoritative and retain their existing
bounds. Historical chunks may be replayed to event consumers, but projections
replace them when `tool_call_terminated` is reduced.

During cancellation cleanup, a progress record is considered retained once its
append command enters the session actor mailbox; actor FIFO then orders it before
the later terminal command even if persistence has not completed yet. Cleanup
deadline diagnostics count only progress records that never entered that mailbox.

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
appends matching runless `user_input_recalled` events so those inputs cannot leak
into a later resume. Promotion appends the ordinary run-owned
`user_input_submitted` event, so transcript rendering and model history use the
same path as interactive run steering.

`message_injected` is a run-owned role and text message accepted from `agent_before_start`. It is
written during run setup and projected into model history, so reopen, fork, revert, and compaction
use the injected message without consulting the plugin again. `user_input_transformed` is the
run-owned audit record written immediately before `user_input_submitted` when `user_before_input`
changes direct input; it contains both `original_input` and the non-empty committed `input`.

Running-resume rollback appends `user_input_recalled_v2` with the exact
`user_input_seq` of its admission. Replay removes that specific pending input
and preserves direct steers admitted after it. Legacy `user_input_recalled`
continues to recall the newest pending input.

## Subscriptions

`events.subscribe` returns all retained events after the optional cursor and
starts `events.subscription` notifications. A notification is tagged as either:

```json
{ "type": "event", "event": { "engine_version": "0.1.0", "seq": 43 } }
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
later deltas. The ordinary event subscription also includes durable
`tool_call_progress` output chunks and terminal events without a separate filter.

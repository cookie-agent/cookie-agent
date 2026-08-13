# Compaction

Compaction replaces a long session history with a checkpoint: a summarizer call
whose output becomes the context for the next request. It keeps long sessions
inside the model's context window without discarding the work done so far.

## The trigger threshold

Compaction is driven by `buffer_tokens`. The trigger threshold is:

```text
trigger_tokens = model_context_limit - buffer_tokens
```

`buffer_tokens` defaults to 33,000, so a model with a 200,000-token context
window triggers at 167,000 tokens. If the buffer exceeds the context limit, the
trigger becomes 0 and automatic compaction is disabled for that model.

The threshold is compared against two signals:

- **Post-check usage.** After each committed model turn, the reported input +
  output tokens are compared against the threshold. This is the authoritative
  signal.
- **Predictive pre-send estimate.** Before a request is sent, the engine
  estimates the serialized history size (bytes ÷ 4 as a token proxy) using a
  per-session learned estimator and compacts in advance when the estimate is
  close to the threshold.

## What happens when it triggers

1. **Tool-output elision.** Bulky tool outputs (8 KiB or more) from turns older
   than the last two are first replaced with content-addressed artifact
   references. If elision alone brings the estimated size under the threshold,
   no summarizer call is made — the session continues with the elided events and
   a `tool_output_elided` event is recorded for each.
2. **Summarizer call.** The internal `compaction` agent (see
   [Internal agents](agents.md#internal-agents)) receives the assembled history
   plus the fixed instruction, optionally extended with the user's focus text.
   It must return summary text only, at most `max_summary_bytes` (256 KiB by
   default). Non-text output is rejected.
3. **Checkpoint commit.** A `context_checkpoint_committed` event records the
   summary, its source boundaries, and the budget math (context limit, trigger
   threshold, input tokens before, estimated tokens after).
4. **Rehydration.** After the checkpoint, the engine re-reads up to 5 distinct
   files most recently opened by the `read` tool (32 KiB each, 128 KiB total,
   permission-checked against the owner policy) and appends a
   `context_rehydrated` event with their contents, so the fresh context still
   has the important file contents available.

## Anti-thrash latching

After a compaction, the engine runs a post-check on the next committed turn. If
usage is still at or above the trigger threshold — meaning the summary plus new
work is already too big — automatic compaction latches off for that session,
emits a `context_compaction_auto_disabled` event, and only manual compaction or
context-overflow recovery will run again. This prevents compacting in a loop.

## Configuration

```toml
[context_compaction]
auto = true            # enable automatic signals
buffer_tokens = 33000  # headroom below the context limit
max_summary_bytes = 262144  # 256 KiB, max 2 MiB
```

- `auto = false` disables the automatic post-check and predictive signals.
  Manual `/compact` and context-overflow recovery compaction remain available.
- `max_summary_bytes` caps the summary produced by the compaction agent and must
  be at most 2 MiB.

## Manual compaction

`/compact` forces a checkpoint for the selected idle session:

```text
/compact preserve the parser decisions and failing test evidence
```

The optional focus text is appended to the fixed compaction instruction so the
summary emphasizes the areas you care about. Steering remains available while
compaction runs; admitted pending inputs are promoted only after the checkpoint,
honoring any recalls made during compaction.

`session.compact` returns whether a checkpoint was actually committed. Manual
compaction always runs elision first and still skips the summarizer call if the
elided context is already under the threshold.

## Events

Compaction produces these event payloads:

- `tool_output_elided` — a bulky output was replaced with an artifact reference
- `internal_agent_started` / `internal_agent_completed` / `internal_agent_failed`
  / `internal_agent_fallback` — the compaction agent invocation
- `context_checkpoint_committed` — the checkpoint with boundaries and budgets
- `context_rehydrated` — file contents re-read into the fresh context
- `context_compaction_auto_disabled` — automatic compaction latched off

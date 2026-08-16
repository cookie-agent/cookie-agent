# Compaction

Compaction replaces a long session history with a checkpoint: a summarizer call
whose output becomes the context for the next request. It keeps long sessions
inside the model's context window without discarding the work done so far.

## Native provider compaction

OpenAI Responses and Azure OpenAI Responses models can opt into provider-native
compaction. The native operation runs first. If the adapter rejects the
assembled request, the provider call fails, or the returned opaque window is
invalid, Cookie Agent automatically runs the existing internal compaction agent.

Enable it on a managed provider model override with
`compaction = "openai-responses-compact"` or
`compaction = "azure-responses-compact"`. Cookie Agent derives the native model
capability from that setting. Other adaptors reject the setting. Azure also
requires the managed provider identity `azure.openai` plus `model`, `version`,
and `deployment_type` in the provider `setup` table so the window is scoped to
an explicit deployment.

A native checkpoint stores a bounded opaque provider window rather than summary
text. Events through the checkpoint boundary are omitted from normal history,
no framed summary message is inserted, and the window is attached to the next
request. Later native compactions are seeded with the previous window. The
window has a 32 MiB cap; `max_summary_bytes` applies only to text summaries.

## The trigger threshold

By default, compaction uses a proportional trigger:

```text
trigger_tokens = model_context_limit * percent / 100
```

`percent` defaults to 70, so a model with a 200,000-token context window
triggers at 140,000 tokens. Valid percentages are 1 through 99; 100 is rejected
because compaction at the model limit does not preserve useful request
headroom.

The fixed-buffer form preserves the earlier behavior:

```text
trigger_tokens = model_context_limit - buffer_tokens
```

This subtraction saturates at zero. If the buffer equals or exceeds the context
limit, the trigger becomes 0 and automatic compaction is disabled for that
model.

The threshold is compared against two signals:

- **Post-check usage.** After each committed model turn, the reported input +
  output tokens are compared against the threshold. This is the authoritative
  signal.
- **Predictive pre-send estimate.** Before a request is sent, the engine
  estimates the serialized history size (bytes ÷ 4 as a token proxy) using a
  per-session learned estimator and compacts in advance when the estimate is
  close to the threshold.

## What happens when it triggers

1. **Raw-context fit check.** The engine first assembles the unmodified history.
   It uses the session's calibrated tokens-per-byte estimate when available and
   otherwise estimates tokens as serialized bytes ÷ 4. A latest real usage value
   at or below the budget is accepted without estimating. For internal
   summarization, fit is checked against each resolved compaction model's context
   limit minus its effective output reserve. An undersized candidate is skipped
   through the normal fallback path; an unknown context limit uses 16,384 tokens.
   Agent documents do not cap this input budget. Native compaction uses the bound
   model's context limit minus the effective compaction output allowance. If that
   allowance is unknown, the engine reserves 20,000 tokens as conservative
   summary-output headroom.
2. **Overflow elision.** When the raw context exceeds that budget, or when a
   normal model request has already failed for context length, bulky tool outputs
   (8 KiB or more) from turns older than the last two are replaced with
   content-addressed artifact references. The context is then reassembled from
   the elided events. If elision brings an automatic compaction below its trigger
   threshold, no summarizer call is made.
3. **Native or summarizer call.** An opted-in Responses model first attempts
   native compaction. Otherwise, or after any native failure, the internal
   `compaction` agent (see
   [Internal agents](agents.md#internal-agents)) receives the assembled history
   plus the fixed instruction, optionally extended with the user's focus text.
   It must return summary text only, at most `max_summary_bytes` (256 KiB by
   default). Non-text output is rejected.
4. **Checkpoint commit.** A `context_checkpoint_committed` event records the
   text summary or opaque native window, its source boundaries, and the budget
   math (context limit, trigger threshold, input tokens before, estimated tokens
   after).
5. **Rehydration.** After the checkpoint, the engine re-reads up to 5 distinct
   files most recently opened by the `read` tool (32 KiB each, 128 KiB total,
   permission-checked against the owner policy) and appends a
   `context_rehydrated` event with their contents, so the fresh context still
   has the important file contents available.

## Configuration

```toml
[context_compaction]
auto = true
trigger = { percent = 70 }
max_summary_bytes = 262144  # 256 KiB, max 2 MiB
```

To reserve a fixed amount of headroom instead:

```toml
[context_compaction]
trigger = { buffer_tokens = 33000 }
```

The legacy top-level `buffer_tokens = 33000` spelling remains accepted as an
alias for the fixed trigger. It cannot be combined with `trigger`.

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
compaction uses raw history when it fits the compaction budget and uses
tool-output elision only as overflow recovery.

## Events

Compaction produces these event payloads:

- `tool_output_elided` — a bulky output was replaced with an artifact reference
- `internal_agent_started` / `internal_agent_completed` / `internal_agent_failed`
  / `internal_agent_fallback` — the compaction agent invocation
- `context_checkpoint_committed` — the checkpoint with boundaries and budgets
- `context_rehydrated` — file contents re-read into the fresh context

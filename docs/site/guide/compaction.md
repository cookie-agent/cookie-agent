# Compaction

Compaction reduces a long session history to a checkpoint. Internal compaction
summarizes the entire active history, including recent messages that will also be
retained unchanged after the summary. Older messages are replaced by the summary
in model context; the original saved log is preserved. Provider-native compaction
instead stores an opaque provider window.

Active history means the currently assembled context before compaction, including
any existing summary and the messages after it. It does not reload historical
events already replaced by earlier checkpoints.

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

This deployment-metadata requirement is specific to native compaction, not
ordinary Azure replay. Native windows retain their exact scope checks; the
[portable-block replay rules](providers.md#replay-and-cancellation) do not permit
moving a compaction window to an incompatible provider or deployment.

A native checkpoint stores a bounded opaque provider window rather than summary
text. Events through the checkpoint boundary are omitted from normal history,
no framed summary message is inserted, and the window is attached to the next
request. Later native compactions are seeded with the previous window. The
window has a 32 MiB cap; `max_summary_bytes` applies only to text summaries.
Native windows remain unchanged by recent-history retention: the engine does not
append an independent recent-message tail to them. Neither native nor internal
compaction rereads files or emits new `context_rehydrated` events.

## The trigger threshold

Triggers are measured against the model's **input budget**, the number of
tokens a request may occupy:

```text
input_budget = model_input_limit                     when the model declares one
input_budget = model_context_limit - output_reserve  otherwise
output_reserve = min(max_output_tokens, 32000, model_context_limit / 2)
```

Catalog models take the input limit from models.dev `limit.input` when it is
narrower than the context window (for example `gpt-5`: 400,000 context,
272,000 input); custom models declare it as `capabilities.input_tokens`.
`max_output_tokens` is the output cap the run sends (see
[agent limits](agents.md)), by default the model's full output limit. The
reserve is capped at 32,000 tokens so a large output limit does not claim a big
share of the window, and at half the context so a model whose output limit
equals its context window keeps a usable budget.

By default, compaction uses a proportional trigger:

```text
trigger_tokens = input_budget * percent / 100
```

`percent` defaults to 70, so a model with a 200,000-token context window and a
64,000-token output limit triggers at 117,600 tokens, and `gpt-5` at 190,400. Valid percentages are 1 through 99; 100 is rejected
because compaction at the model limit does not preserve useful request
headroom.

The fixed-buffer form preserves the earlier behavior:

```text
trigger_tokens = input_budget - buffer_tokens
```

This subtraction saturates at zero. If the buffer equals or exceeds the input
budget, the trigger becomes 0 and automatic compaction is disabled for that
model.

Before each model request, the engine compares the input + output tokens the
provider reported for the latest model turn against the threshold, and compacts
when they reach it. Usage recorded before the latest checkpoint is ignored, so a
fresh checkpoint waits for the next reported turn. Messages added since the
latest turn, such as a new prompt or steering input, are not counted until the
provider reports them, and they are kept verbatim only when they fit the recent
history budget (see step 4 below). If a request still exceeds the provider's
context length, the engine compacts once regardless of the threshold and
retries.

## What happens when it triggers

1. **Native attempt.** An opted-in Responses model first attempts native
   compaction. A successful native window goes directly to checkpoint commit,
   without selecting independent recent messages. Otherwise, or after any native
   failure, the engine uses internal summarization. Native compaction uses the
   bound model's context limit minus the effective compaction output allowance.
   If that allowance is unknown, the engine reserves 20,000 tokens as conservative
   summary-output headroom.
2. **Full-history trial.** Internal summarization first uses the entire active,
   unpruned history, including the recent messages selected for retention.
   The request keeps the session's own system prompt and tool definitions, so
   it stays a cache-friendly extension of the latest conversation turn; the
   compaction agent's prompt rides along in the trailing instruction message
   instead of replacing the session system prompt. A tool-call answer is
   rejected as non-text output and counts as a compaction failure.
    The provider is authoritative for internal-agent input size; there is no
    byte-based pre-flight admission gate. The session's calibrated estimator still
    sizes the retained recent history and the post-checkpoint budget.
3. **Context-fit retry.** A provider context-length failure permits one pruned
   retry. Other failures do not trigger pruning. Internal model bindings are
   tried in order, advancing after provider failures. The retry uses an in-memory
   copy of the summarizer input and drops the session tool definitions (cache
   affinity is already lost once tool outputs are rewritten):
   tool results are retained with `ArtifactStore::retain` and replaced by reference
   markers with a structured `read` hint using `filePath="artifact://<digest>"`.
   Saved `ArtifactReference` URIs remain unchanged. Possession of an
   artifact ID grants read access in the
   configured artifact store, without a session-ownership check. Structured tool
   content is retained as serialized JSON, not flattened into original output;
   the marker labels this format. All outputs of artifact `read` calls are replaced
   with `[artifact read output omitted for compaction]` in the private retry input,
   including named-stream reads. Detection uses the structured `filePath` argument;
   ordinary filesystem `read` results follow normal tool-output pruning.
   Both older and recent messages remain in the retry input. This does
   not emit durable `tool_output_elided` (`ToolOutputElided`) events or change the
   original saved log or recent retained message contents, even if the retry fails.
4. **Recent-history selection.** For internal summarization, the engine selects
   a contiguous suffix of original messages. Its effective token target is at
   most `min(keep_recent_tokens, context_limit / 4)`, using integer division,
   and is further limited by actual available space in the post-checkpoint
   request. System and tool context, pinned context, the summary, and the output
   reserve must still fit. Tool calls and their results are retained as complete
   groups, never split at the suffix boundary. If the newest indivisible group
   exceeds the target, no tail is retained; the engine does not exceed the
   target or substitute an older, noncontiguous group. `keep_recent_tokens = 0`
   disables the tail.
5. **Full-history summary.** The internal `compaction` agent (see
   [Internal agents](agents.md#internal-agents)) summarizes both older and recent
   messages; retaining recent messages does not exclude them from its input.
   Its fixed instruction may be extended with the user's focus text. It must
   return summary text only, at most
   `max_summary_bytes` (256 KiB by default); non-text output is rejected. The
   built-in compaction document sets no output cap of its own, so it inherits the
   owner run's `max_output_tokens`, bounded by the model's own output limit; the
   same inheritance applies to any authored internal-agent document that omits
   the limit. A nonzero `limits.max_output_tokens` in `compaction.md` overrides
   the inherited value. The suffix-selection output reserve equals that same
   effective cap.
   The built-in compaction document uses a flat 3-minute timeout. An authored
   `compaction.md` can override `limits.timeout_ms`; the configured value is
   honored exactly, while zero or omission uses the 30-second internal-agent
   default.
6. **Checkpoint commit.** A `context_checkpoint_committed` event records the
   text summary or opaque native window, source and recent-suffix boundaries,
   and the budget math, including the effective recent-history token budget.
   Retained messages come from saved history, not new file reads.

## Context after an internal checkpoint

The next request assembles context in this order:

1. System prompt and tool definitions.
2. Pinned `AGENTS.md` context.
3. Pinned loaded skill bodies.
4. Summary of the entire active pre-compaction history.
5. Recent original messages retained unchanged, followed by any new messages.

Pinned context is preserved separately from the suffix. With retention disabled,
or when the newest complete group cannot fit, the recent suffix is absent and
only the summary remains alongside pinned context. The summary may cover recent
messages even though those messages also remain unchanged after it. Native
checkpoints continue to use their opaque provider windows rather than this
summary-and-tail layout.

## Configuration

See [Context Compaction](../engine/context_compaction.md) for settings, defaults,
trigger forms, and inheritance.

## Manual compaction

Choosing `/compact` in the [command palette](run.md#command-palette) forces a
checkpoint for the selected idle session. The palette then asks for an
optional focus, for example:

```text
preserve the parser decisions and failing test evidence
```

Press Enter with the prompt empty to compact without a focus. The focus text is
appended to the fixed compaction instruction so the
summary emphasizes the areas you care about. Steering remains available while
compaction runs; admitted pending inputs are promoted only after the checkpoint,
honoring any recalls made during compaction.

`session.compact` returns whether a checkpoint was actually committed. Manual
compaction follows the same full-history trial and context-fit retry rules.
The internal summarizer receives both older and recent messages, including those
retained unchanged after the summary.

## Events

Compaction produces these event payloads:

- `internal_agent_started` / `internal_agent_completed` / `internal_agent_failed`
  / `internal_agent_fallback` — the compaction agent invocation
- `internal_agent_text_delta` (live-only) — the summary as it streams
- `native_compaction_started` / `native_compaction_finished` (live-only) — a
  provider-native compaction call

While compaction runs, the TUI shows a `🧹 compacting context…` row and the
bottom bar reads `compacting`. Expanded, the row streams the summary, or reads
`calling native compaction endpoint (model)` during a native call. When
compaction finishes, the row is replaced by the `context compacted` checkpoint
row, or removed if compaction failed. A failed native call hands the row to the
summarizer fallback.
- `context_checkpoint_committed` — the checkpoint with boundaries and budgets

The internal summarizer's in-memory pruning retry does not produce
`tool_output_elided` events.

`context_rehydrated` (`ContextRehydrated` in Rust) is legacy-only. Saved logs
containing it remain decodable and renderable, but new compactions never reread
files or emit this event, including on the native path. See the
[event reference](../reference/events.md#compaction-checkpoints) for checkpoint
retention fields and legacy defaults.

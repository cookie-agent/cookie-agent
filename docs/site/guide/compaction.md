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

1. **Native attempt.** An opted-in Responses model first attempts native
   compaction. A successful native window goes directly to checkpoint commit,
   without selecting independent recent messages. Otherwise, or after any native
   failure, the engine uses internal summarization. Native compaction uses the
   bound model's context limit minus the effective compaction output allowance.
   If that allowance is unknown, the engine reserves 20,000 tokens as conservative
   summary-output headroom.
2. **Full-history trial.** Internal summarization first uses the entire active,
   unpruned history, including the recent messages selected for retention.
   The local fit check evaluates the exact assembled summarizer input, including
   its instructions, against the resolved compaction model's context limit minus
   its effective output reserve. Summarizer admission uses a fixed byte-based
   estimate (serialized fit-projection bytes ÷ 4, rounded up), not the session's
   calibrated estimator used for compaction triggers and post-checkpoint budgeting.
   An unknown context limit uses 16,384 tokens. Agent documents do not cap this
   input budget.
3. **Context-fit retry.** A local fit rejection of that exact input or a provider
   context-length failure permits at most one pruned retry. Other failures do not
   trigger pruning. The retry uses an in-memory copy of the summarizer input:
   tool results are retained with `ArtifactStore::retain` and replaced by reference
   markers with a structured `read` hint using `filePath="artifact://<digest>"`.
   Saved `ArtifactReference` URIs remain unchanged. Possession of an
   artifact ID grants read access in the
   configured artifact store, without a session-ownership check. Structured tool
   content is retained as serialized JSON, not flattened into original output;
   the marker labels this format. All outputs of artifact `read` calls are redacted,
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
   built-in compaction agent allows 4,096 output tokens. Authored internal-agent
   documents that omit this limit retain the generic 2,048-token default.
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

`/compact` forces a checkpoint for the selected idle session:

```text
/compact preserve the parser decisions and failing test evidence
```

The optional focus text is appended to the fixed compaction instruction so the
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
- `context_checkpoint_committed` — the checkpoint with boundaries and budgets

The internal summarizer's in-memory pruning retry does not produce
`tool_output_elided` events.

`context_rehydrated` (`ContextRehydrated` in Rust) is legacy-only. Saved logs
containing it remain decodable and renderable, but new compactions never reread
files or emit this event, including on the native path. See the
[event reference](../reference/events.md#compaction-checkpoints) for checkpoint
retention fields and legacy defaults.

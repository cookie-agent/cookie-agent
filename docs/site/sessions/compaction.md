# Compaction

Compaction reduces a long session history to a checkpoint. Internal compaction
keeps a budgeted tail of recent messages unchanged and summarizes everything
before it; the summary replaces those older messages in model context, and the
original saved log is preserved. On a Responses provider, provider-native
compaction runs beside it and the checkpoint also stores the opaque window the
provider returned.

Active history means the currently assembled context before compaction, including
any existing summary and the messages after it. It does not reload historical
events already replaced by earlier checkpoints.

## Native provider compaction

Every model on a Responses adaptor compacts natively by default: OpenAI
Responses, whether the managed `openai` provider, an OpenAI-compatible gateway,
or a credential-less endpoint, and Azure OpenAI Responses when the provider's
`setup` table has `model`, `version`, and `deployment_type`, which scope the
window to an explicit deployment. Azure without them, and every other adaptor,
compacts with the summary alone. Set `compaction = "unsupported"` on a
[model override](../extend/providers.md#provider-fields), or on a custom model's
capabilities, to turn native compaction off for that model.

**Native compaction runs beside the summary.** When a checkpoint is due, the
engine calls the provider's compact endpoint and the internal compaction agent
at the same time:

- both succeed: the checkpoint holds the summary and the window;
- native fails: the checkpoint holds the summary alone, and the failure may mark
  the provider (below);
- the summary fails: nothing is committed, even when native succeeded. Every
  checkpoint carries a summary.

A failed native call costs only its own time, which overlaps the summary.
Cancelling compaction cancels both calls, and compaction hooks see one
compaction.

**The window serves only its own model.** A request whose model has the same
provider ID and model ID as the window, on the same adaptor, sends the window
with the messages after it. A variant change keeps the window. Every other
request, on another model, another provider, or a provider marked as rejecting
windows, uses the summary and its recent messages instead. Switching models in
the picker, or a fallback chain moving to its next model, therefore continues
on the summary. The window also keeps its exact scope (its endpoint and
account): a window whose scope the adaptor no longer accepts, after an endpoint
change for example, is passed over for the summary too.

A window has a 32 MiB cap; `max_summary_bytes` applies only to the summary. A
window keeps no independent recent-message tail. The next native compaction is
seeded with the window when the model can use it. Neither native nor internal
compaction rereads files.

**Older sessions.** Builds before client protocol 1.29.0 stored a window without
a summary. Such a checkpoint still serves its own model. On any other model the
engine first summarizes the events it covers from the saved log, once, and
commits that as a new checkpoint; the request then proceeds on it.

### The provider record

The daemon remembers, per provider ID, what native compaction answered, in
`native-compaction.json` in its data folder (`~/.cookie-agent` by default):

- **`unsupported`**: the compact endpoint answered definitively that the
  provider has no native compaction: HTTP 404, 405, or 501, or a refusal naming
  the compaction item (an adaptor that cannot build the compact request counts
  too). Native compaction is not attempted; stored windows still serve.
- **`rejects_windows`**: the provider refused a request that carried a window
  (below). Native compaction is not attempted, and requests use the summary
  instead of stored windows.

Transient failures (timeouts, rate limits, 5xx other than 501, network errors)
and authentication errors mark nothing. An entry expires after 7 days, and is
dropped when the provider's configuration (its source, endpoint, recipe, setup,
or authentication method) changes. There is no option to force native
compaction past an entry: delete the file, or change the provider's
configuration. The configuration status (`/config` in the TUI, `cookie config`,
and the web client's configuration view) lists each entry with its reason and
age.

**A window refused at request time.** When the provider answers a request that
carried a window with a 400 or 422 naming the compaction item or its encrypted
content (OpenAI answers an unusable compaction item with
`invalid_encrypted_content`; a gateway without compaction support typically
names the `compaction` item type or the `encrypted_content` field), that request
fails with the provider's error, as any refusal does, and the provider is
marked `rejects_windows`. The next request, in that session or any other, uses
the summary. A refusal that names neither marks nothing.

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
[agent limits](../extend/agents.md)), by default the model's full output limit. The
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

1. **Native call.** On a model that compacts natively and a provider the
   record does not mark, the native call starts beside the summary below and
   compacts the whole active history, without selecting recent messages.
   Native compaction uses the bound model's context limit minus the effective
   compaction output allowance; when the history exceeds it, bulky tool outputs
   are elided in the saved log first (`tool_output_elided`). If that allowance
   is unknown, the engine reserves 20,000 tokens as conservative
   summary-output headroom.
2. **Recent-history selection.** For internal summarization, the engine first
   selects a contiguous suffix of original messages to keep unchanged. Its
   effective token target is at most `min(keep_recent_tokens, input_budget / 4)`,
   using integer division, and is further limited by actual available space in
   the post-checkpoint request. System and tool context, pinned context, the
   summary, and the output reserve must still fit. Tool calls and their results
   are retained as complete groups, never split at the suffix boundary. If the
   newest indivisible group exceeds the target, no tail is retained; the engine
   does not exceed the target or substitute an older, noncontiguous group.
   `keep_recent_tokens = 0` disables the tail.
3. **Replaced-span trial.** The summarizer receives only the span the checkpoint
   replaces: the active, unpruned history up to the selected suffix, including
   any earlier summary. The suffix itself is never sent. The request keeps the
   session's own system prompt, pinned context, and tool definitions, so it is
   a cache-friendly prefix of the latest conversation request; the compaction
   agent's prompt rides along in a trailing instruction message instead of
   replacing the session system prompt. A tool-call answer is rejected as
   non-text output and counts as a compaction failure.
   The provider is authoritative for internal-agent input size; there is no
   byte-based pre-flight admission gate. The session's calibrated estimator still
   sizes the retained recent history and the post-checkpoint budget.
4. **Context-fit retry.** A provider context-length failure permits one pruned
   retry. Other failures do not trigger pruning. Internal model bindings are
   tried in order, advancing after provider failures. The retry uses an in-memory
   copy of the same replaced span and drops the session tool definitions (cache
   affinity is already lost once tool outputs are rewritten):
   tool results are retained with `ArtifactStore::retain` and replaced by reference
   markers with a structured `read_artifact` hint carrying `artifact="<digest>"`.
   Saved `ArtifactReference` URIs remain unchanged. The session tree can read
   the retained artifact back with `read_artifact`. Structured tool
   content is retained as serialized JSON, not flattened into original output;
   the marker labels this format. All outputs of `read_artifact` calls are replaced
   with `[artifact read output omitted for compaction]` in the private retry input,
   including named-stream reads. Detection uses the tool name;
   ordinary filesystem `read` results follow normal tool-output pruning.
   This does not emit durable `tool_output_elided` (`ToolOutputElided`) events or
   change the original saved log or the retained recent messages, even if the
   retry fails.
5. **Summary.** The internal `compaction` agent (see
   [Internal agents](../extend/agents.md#internal-agents)) writes a structured checkpoint
   with fixed sections (objective, requirements, decisions, work state, files,
   commands and errors, next step, important context). When the replaced span
   starts with an earlier internal summary, the prompt asks it to merge that
   summary into one updated checkpoint instead of starting fresh. An authored
   `compaction.md` can extend or replace this prompt with
   [placeholders](../extend/agents.md#internal-agent-prompt-placeholders), and the user's
   focus text is added to it. It must return summary text only, at most
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
   text summary, the native window beside it when the native call succeeded,
   source and recent-suffix boundaries, and the budget math, including the
   effective recent-history token budget. Retained messages come from saved
   history, not new file reads.

## Context after an internal checkpoint

The next request assembles context in this order:

1. System prompt and tool definitions.
2. Pinned `AGENTS.md` context.
3. Pinned loaded skill bodies.
4. Summary of everything before the retained suffix.
5. Recent original messages retained unchanged, followed by any new messages.

Pinned context is preserved separately from the suffix. With retention disabled,
or when the newest complete group cannot fit, the recent suffix is absent and
only the summary remains alongside pinned context. The summary and the retained
suffix do not overlap. A request on the window's own model uses the window
and the messages after it instead of this summary-and-tail layout.

## Configuration

See [Context Compaction](../engine/context_compaction.md) for settings, defaults,
trigger forms, and inheritance.

## Manual compaction

Choosing `/compact` in the [command palette](../clients/tui.md#command-palette) forces a
checkpoint for the selected idle session. The palette then asks for an
optional focus, for example:

```text
preserve the parser decisions and failing test evidence
```

Press Enter with the prompt empty to compact without a focus. The focus text is
added to the compaction prompt (appended, or placed where an authored
`compaction.md` puts `{{focus}}`) so the summary emphasizes the areas you care
about. Steering remains available while
compaction runs; admitted pending inputs are promoted only after the checkpoint,
honoring any recalls made during compaction. A message sent to an idle session
that is compacting, from this or another client, waits for the compaction to
finish and then starts its run.

To stop a manual compaction, press Ctrl-C (or Esc twice) while the session shows
`compacting`, as you would to cancel a run; `session.compact.cancel` does the
same over the protocol. A cancelled compaction commits no checkpoint.

`session.compact` returns whether a checkpoint was actually committed. Manual
compaction follows the same recent-history selection, replaced-span trial, and
context-fit retry rules.

## Troubleshooting

- **Native compaction stopped being used for a provider.** The provider record
  marked it. The configuration status (`/config` in the TUI, `cookie config`,
  or the web client's configuration view) names the provider, its state, the
  answer that marked it, and when. `unsupported` means the compact endpoint
  answered that it has none (for example HTTP 404 from a gateway that does not
  implement `/responses/compact`); `rejects_windows` means the provider refused
  a request that carried a compacted window, and requests now use the summary.
  The mark lifts after 7 days, when the provider's configuration changes, or
  when you delete `native-compaction.json` from the data folder (or its entry);
  compaction keeps working on the summary meanwhile.
- **A request failed with an encrypted-content error.**
  That request carried a window its provider refused; the provider is now
  marked `rejects_windows`, so send the message again and it goes out on the
  summary.

## Events

Compaction produces these event payloads:

- `internal_agent_started` / `internal_agent_finished` / `internal_agent_fallback`
  — the compaction agent invocation
- `internal_agent_text_delta` (live-only) — the summary as it streams
- `native_compaction_started` / `native_compaction_finished` (live-only) — a
  provider-native compaction call

While compaction runs, the TUI shows a `🧹 compacting context…` row and the
bottom bar reads `compacting`. Expanded, the row streams the summary, or reads
`calling native compaction endpoint (model)` while the native call runs beside
it. When compaction finishes, the row is replaced by the `context compacted`
checkpoint row, or removed if compaction failed.
- `context_checkpoint_committed` — the checkpoint with boundaries and budgets

The internal summarizer's in-memory pruning retry does not produce
`tool_output_elided` events.

See the [event reference](../reference/events.md#compaction-checkpoints) for
checkpoint retention fields.

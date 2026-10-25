# Context Compaction [context_compaction]

Complete `config.toml` using the defaults:

```toml
[context_compaction]
auto = true
trigger = { percent = 70 }
max_summary_bytes = 262144
keep_recent_tokens = 16384
```

For fixed headroom, replace `trigger` with `{ buffer_tokens = 33000 }`.
An omitted table inherits; an authored table replaces the lower table completely
and uses defaults for omitted fields. Unknown fields fail; see
[config.toml](configuration.md).

Controls the automatic context-limit behavior documented in
[Compaction](../sessions/compaction.md). Native compaction has no setting here: it runs beside the
summary on every Responses model unless the model sets
`compaction = "unsupported"` (see [native compaction](../sessions/compaction.md#native-provider-compaction)).

The built-in compaction agent has a flat 3-minute timeout. An authored
`compaction.md` can override `limits.timeout_ms`, which is honored exactly;
zero or omission uses the 30-second internal-agent default. The timeout does
not scale with serialized history size.

The internal summarizer receives only the span the checkpoint replaces: the
active, unpruned history up to the recent messages retained unchanged after the
summary, which are never sent to it. This is the currently assembled history,
not a reload of events replaced by earlier checkpoints. The request keeps the
session's own system prompt and tool definitions so it stays a cache-friendly
prefix of the latest conversation request; the compaction agent's prompt is
folded into the trailing instruction message instead of replacing the session
system prompt. A tool-call
answer is rejected as non-text output and counts as a compaction failure.
Internal agents have no local byte-based admission gate.
A provider context-length failure permits at most one pruned retry; other failures
do not trigger pruning. Bindings are tried in order. Automatic compaction stops
after three consecutive failures in a run and emits one diagnostic; a successful
checkpoint resets the counter. The retry changes only an in-memory copy of the
summarizer input, using `ArtifactStore::retain` and artifact-ID reference markers
for tool results and replacing output from `read_artifact` calls with
`[artifact read output omitted for compaction]`, detected by
the tool name. Ordinary filesystem reads are
pruned normally. The pruned retry also drops the session tool definitions:
cache affinity is already lost once tool outputs are rewritten, so fewer tools
means less to send. Markers include a `read_artifact` hint with `artifact="<digest>"`;
internal stored references retain their URI format. The session tree reads that
artifact back with `read_artifact`. Non-text tool content is retained
as serialized tool-content JSON and the marker identifies this format. It emits no
durable `tool_output_elided` events and preserves the original saved log and
recent retained message contents even if the retry fails.
After compaction, model context contains the summary and retained recent messages
alongside pinned context.

| Key | Type | Default | Description |
|---|---|---|---|
| `auto` | boolean | `true` | Enable automatic compaction signals (post-check usage and predictive pre-send estimation). Manual `/compact` and context-overflow recovery compaction remain available when `false`. |
| `trigger` | inline table | `{ percent = 70 }` | Trigger threshold selection. `{ percent = N }` uses `N%` of the model's input budget, where `N` must be from 1 through 99. `{ buffer_tokens = N }` subtracts positive `N` from the input budget, saturating at zero. The input budget is the model's input limit when it declares one, otherwise its context limit less `min(output cap, 32000, context / 2)` (see [compaction](../sessions/compaction.md#the-trigger-threshold)). |
| `buffer_tokens` | integer | unset | Legacy alias for `trigger = { buffer_tokens = N }`. Must be greater than zero and cannot be set together with `trigger`. |
| `max_summary_bytes` | integer | `262144` (`256 * 1024`) | Hard byte limit for a compaction summary produced by the internal `compaction` agent. Must be greater than zero and at most `2 * 1024 * 1024` (2 MiB). |
| `keep_recent_tokens` | integer (`u64`) | `16384` | Nonnegative token budget for recent original messages retained unchanged after an internal summary. The summarizer receives only the messages before them. `0` disables retention, so the summary covers the whole active history. The effective target is at most `min(keep_recent_tokens, input_budget / 4)`, further limited by actual post-checkpoint available space. Complete tool-call/result groups are never split; an oversized newest indivisible group leaves no recent messages retained. Configuration does not clamp the requested value. Native windows receive no independent recent messages. |

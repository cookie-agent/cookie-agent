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
[config.toml](../guide/configuration.md).

Controls the automatic context-limit behavior documented in
[Compaction](../guide/compaction.md).

The internal summarizer first receives the entire active, unpruned history,
including recent messages that will be retained unchanged after the summary.
This is the currently assembled history, not a reload of events replaced by
earlier checkpoints. A local fit rejection of the exact assembled summarizer input
or a provider context-length failure permits at most one pruned retry; other
failures do not trigger pruning. The retry changes only an in-memory copy of the
summarizer input, using `ArtifactStore::retain` and artifact-ID reference markers
for tool results and redacting all output from artifact `read` calls, detected by
the tool name plus its structured `filePath` URI. Ordinary filesystem reads are
pruned normally. Markers include a `read` hint with `filePath="artifact://<digest>"`;
internal stored references retain their URI format. Possession of that ID grants
read access without a session-ownership lookup. Non-text tool content is retained
as serialized tool-content JSON and the marker identifies this format. It emits no
durable `tool_output_elided` events and preserves the original saved log and
recent retained message contents even if the retry fails.
After compaction, model context contains the summary and retained recent messages
alongside pinned context.

| Key | Type | Default | Description |
|---|---|---|---|
| `auto` | boolean | `true` | Enable automatic compaction signals (post-check usage and predictive pre-send estimation). Manual `/compact` and context-overflow recovery compaction remain available when `false`. |
| `trigger` | inline table | `{ percent = 70 }` | Trigger threshold selection. `{ percent = N }` uses `N%` of the model context limit, where `N` must be from 1 through 99. `{ buffer_tokens = N }` subtracts positive `N` from the context limit, saturating at zero. |
| `buffer_tokens` | integer | unset | Legacy alias for `trigger = { buffer_tokens = N }`. Must be greater than zero and cannot be set together with `trigger`. |
| `max_summary_bytes` | integer | `262144` (`256 * 1024`) | Hard byte limit for a compaction summary produced by the internal `compaction` agent. Must be greater than zero and at most `2 * 1024 * 1024` (2 MiB). |
| `keep_recent_tokens` | integer (`u64`) | `16384` | Nonnegative token budget for recent original messages retained unchanged after an internal summary of the entire active history. These messages are also included in the summarizer's input. `0` disables retention, not summary coverage. The effective target is at most `min(keep_recent_tokens, context_limit / 4)`, further limited by actual post-checkpoint available space. Complete tool-call/result groups are never split; an oversized newest indivisible group leaves no recent messages retained. Configuration does not clamp the requested value. Native windows receive no independent recent messages. |

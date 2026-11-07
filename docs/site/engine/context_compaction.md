# Context Compaction [context_compaction]

Complete `config.toml` using the defaults:

```toml
[context_compaction]
auto = true
trigger = { percent = 70 }
max_summary_bytes = 262144
keep_recent_tokens = 16384

[context_compaction.cache_expiry]
mode = "off"
slack = "5m"
fallback_ttl = "30m"
min_tokens = 40000
min_cost_usd = 0.0
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
artifact back with `read_artifact`. Text content is retained as it was, so it
pages line by line. Non-text tool content is retained line-structured, a
`[tool content: <kind>]` header and each part under a `[part N: kind]` line
with its text's own line breaks and JSON pretty-printed, and the marker
describes this layout; artifacts earlier builds stored keep their one line
of serialized JSON. It emits no
durable `tool_output_elided` events and preserves the original saved log and
recent retained message contents even if the retry fails.
After compaction, model context contains the `AGENTS.md` and memory turns the
checkpoint read afresh, the summary, and the retained recent messages; a loaded
skill is one of those messages, or covered by the summary
([Compaction](../sessions/compaction.md#context-after-an-internal-checkpoint)).

| Key | Type | Default | Description |
|---|---|---|---|
| `auto` | boolean | `true` | Enable automatic compaction signals (post-check usage and predictive pre-send estimation). Manual `/compact` and context-overflow recovery compaction remain available when `false`. |
| `trigger` | inline table | `{ percent = 70 }` | Trigger threshold selection. `{ percent = N }` uses `N%` of the model's input budget, where `N` must be from 1 through 99. `{ buffer_tokens = N }` subtracts positive `N` from the input budget, saturating at zero. The input budget is the model's input limit when it declares one, otherwise its context limit less `min(output cap, 32000, context / 2)` (see [compaction](../sessions/compaction.md#the-trigger-threshold)). |
| `buffer_tokens` | integer | unset | Legacy alias for `trigger = { buffer_tokens = N }`. Must be greater than zero and cannot be set together with `trigger`. |
| `max_summary_bytes` | integer | `262144` (`256 * 1024`) | Hard byte limit for a compaction summary produced by the internal `compaction` agent. Must be greater than zero and at most `2 * 1024 * 1024` (2 MiB). |
| `keep_recent_tokens` | integer (`u64`) | `16384` | Nonnegative token budget for recent original messages retained unchanged after an internal summary. The summarizer receives only the messages before them. `0` disables retention, so the summary covers the whole active history. The effective target is at most `min(keep_recent_tokens, input_budget / 4)`, further limited by actual post-checkpoint available space. Complete tool-call/result groups are never split; an oversized newest indivisible group leaves no recent messages retained. Configuration does not clamp the requested value. Native windows receive no independent recent messages. |

## Before the cache expires

`[context_compaction.cache_expiry]` compacts an idle session shortly before
its prompt cache expires, so the next prompt caches a short context again
instead of the whole conversation (see
[Compaction before the cache expires](../sessions/compaction.md#before-the-cache-expires)).
It belongs to `[context_compaction]`: an authored `[context_compaction]`
table replaces the lower one with this table's defaults unless it sets
them too.

| Key | Type | Default | Description |
|---|---|---|---|
| `mode` | string | `"off"` | `"off"`; `"scheduled"`, only sessions a [scheduled job](../sessions/scheduling.md) wakes after their cache expires; or `"always"`, every idle session. |
| `slack` | duration string | `"5m"` | How long before expiry the compaction starts, at most a fifth of the cache's TTL: `min(slack, ttl / 5)`, so a ten-minute cache compacts two minutes before it expires. Must be positive. |
| `fallback_ttl` | duration string | `"30m"` | The TTL of a model whose TTL cookie neither sets nor is told: a provider's automatic caching, OpenAI-compatible endpoints, Gemini. A model's [`cache_ttl`](../extend/providers.md#managed-model-overrides) replaces it. Must be positive. |
| `min_tokens` | integer | `40000` | Compacts only a context of at least this many tokens (the latest turn's input plus output). `0` or less turns the floor off. |
| `min_cost_usd` | number | `0.0` | Compacts only a context whose caching again would cost at least this many dollars, at the model's cache-write rate or else its input rate. Models without prices never reach it. `0` or less turns the floor off. |

With both floors off, every idle session that meets the rest of the rule
compacts; with both on, reaching either one is enough. Durations take
`ms`, `s`, `m`, `h`, and `d` suffixes. A change applies at once, from the
scheduler's next look, within a minute.


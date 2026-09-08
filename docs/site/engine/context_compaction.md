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

| Key | Type | Default | Description |
|---|---|---|---|
| `auto` | boolean | `true` | Enable automatic compaction signals (post-check usage and predictive pre-send estimation). Manual `/compact` and context-overflow recovery compaction remain available when `false`. |
| `trigger` | inline table | `{ percent = 70 }` | Trigger threshold selection. `{ percent = N }` uses `N%` of the model context limit, where `N` must be from 1 through 99. `{ buffer_tokens = N }` subtracts positive `N` from the context limit, saturating at zero. |
| `buffer_tokens` | integer | unset | Legacy alias for `trigger = { buffer_tokens = N }`. Must be greater than zero and cannot be set together with `trigger`. |
| `max_summary_bytes` | integer | `262144` (`256 * 1024`) | Hard byte limit for a compaction summary produced by the internal `compaction` agent. Must be greater than zero and at most `2 * 1024 * 1024` (2 MiB). |
| `keep_recent_tokens` | integer (`u64`) | `16384` | Nonnegative token budget for the original recent-message suffix retained alongside an internal summary of the discarded prefix. `0` disables the tail. The effective target is at most `min(keep_recent_tokens, context_limit / 4)`, further limited by actual post-checkpoint available space. Complete tool-call/result groups are never split; an oversized newest indivisible group yields no tail. Configuration does not clamp the requested value. Native windows receive no independent tail. |

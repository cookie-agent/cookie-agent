# Delegation [delegation]

Bound delegated work and idle residency. Complete `config.toml`:

```toml
[delegation]
max_depth = 3
max_concurrency = 4
max_resident_subagents = 20
idle_eviction_after = "1h"
```

These limits do not authorize delegation. Configure eligible targets and tool
access in [Agent permissions](../guide/agents.md#tool-availability-and-delegation).
An omitted table inherits; an authored table replaces the lower table and uses
defaults for omitted fields. Unknown fields fail; see
[config.toml](../guide/configuration.md).

`max_resident_subagents` accepts zero. Duration values are a nonnegative integer
followed by one supported unit, without spaces or compound units; `"0s"` is
valid. Overflow is rejected. Residency is a soft threshold, not an admission cap.

| Key | Type | Default | Description |
|---|---|---|---|
| `max_depth` | integer | `3` | Maximum delegation depth below a root session. Must be greater than zero. |
| `max_concurrency` | integer | `4` | Maximum concurrently running root-level background delegations. Excess calls queue FIFO, up to `4 × max_concurrency`; a full queue rejects admission. Foreground and nested delegations bypass this queue. A value of `0` is rejected. |
| `max_resident_subagents` | integer | `20` | Soft trigger for resident delegated sessions. Above this count, the janitor evicts eligible idle children oldest-first until the count reaches the trigger or no eligible child remains. Recently active children may keep residency above the trigger. |
| `idle_eviction_after` | duration string | `"1h"` | Minimum time since a delegated session's last run ended before it can be evicted. Compact `ms`, `s`, `m`, `h`, and `d` suffixes are accepted. |

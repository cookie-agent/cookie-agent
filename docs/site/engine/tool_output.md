# Tool Output [tool_output]

Complete `config.toml` using the defaults:

```toml
[tool_output]
max_lines = 500
max_bytes = 12800
head_lines = 100
tail_lines = 100
```

An omitted table inherits. An authored table replaces the lower table, with
defaults for omitted fields; unknown fields fail. See
[config.toml](../guide/configuration.md) and the
[retained-output contract](../reference/tools.md#retained-tool-output).

Controls each output stream's inline preview. A stream within both `max_lines`
and `max_bytes` is shown whole. A longer one shows its first `head_lines` and
last `tail_lines` lines, splitting `max_bytes` between the two ends in the same
ratio, around a marker such as
`[… 1340 lines omitted. Read more: read(filePath="artifact://<digest>/stdout", offset=100)]`.
The marker counts bytes instead when no line break falls in the omitted part, and
its offset is the first line not fully shown. Full single output and every named
stream are retained by the runtime, with generic manifests for named streams.
Declaration order determines rendering order; only truncated streams get read
hints. Aggregate event bounds may further reduce previews with truthful hints.
The limits apply only to tools using the normal bounded policy. The
self-paginating `read` and `get_subagent_result` tools opt
out absolutely; configuration cannot re-enable truncation for them. The
`delegate_subagent` terminal result also opts out because its teaser preview
follows [`[subagent_output]`](subagent_output.md) instead.

| Key | Type | Default | Description |
|---|---|---|---|
| `max_lines` | integer | `500` | Longest stream, in lines, shown whole. Must be greater than zero. |
| `max_bytes` | integer | `12800` | Longest stream, in bytes, shown whole. Must be greater than zero. |
| `head_lines` | integer | `100` | Lines kept from the start of a truncated stream. When omitted it is 100, capped at the rest of `max_lines` after `tail_lines`, or at half of `max_lines` (rounded up) when both are omitted. |
| `tail_lines` | integer | `100` | Lines kept from the end of a truncated stream. When omitted it is 100, capped at the rest of `max_lines` after `head_lines`, or at half of `max_lines` (rounded down) when both are omitted. `head_lines + tail_lines` must be 1..=`max_lines`. |

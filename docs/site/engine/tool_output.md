# Tool Output [tool_output]

Complete `config.toml` using the defaults:

```toml
[tool_output]
max_lines = 2000
max_bytes = 51200
```

An omitted table inherits. An authored table replaces the lower table, with
defaults for omitted fields; unknown fields fail. See
[config.toml](../guide/configuration.md) and the
[retained-output contract](../reference/tools.md#retained-tool-output).

Controls each output stream's inline preview. Full single output and every named
stream are retained by the runtime, with generic manifests for named streams.
Declaration order determines rendering order; only truncated streams get read
hints. Aggregate event bounds may further reduce previews with truthful hints.
The limits apply only to tools using the normal bounded policy. The
self-paginating `read` and `get_subagent_result` tools opt
out absolutely; configuration cannot re-enable truncation for them.

| Key | Type | Default | Description |
|---|---|---|---|
| `max_lines` | integer | `2000` | Maximum preview lines per output stream. Must be greater than zero. |
| `max_bytes` | integer | `51200` (`50 * 1024`) | Maximum preview bytes per output stream. Must be greater than zero. |

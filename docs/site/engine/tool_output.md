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

Controls how much tool output is retained inline in a session before it is
truncated or replaced with artifact references.
The limits apply only to tools using the normal bounded policy. The
self-paginating `read`, `read_tool_result`, and `get_subagent_result` tools opt
out absolutely; configuration cannot re-enable truncation for them.

| Key | Type | Default | Description |
|---|---|---|---|
| `max_lines` | integer | `2000` | Maximum lines of tool output retained. Must be greater than zero. |
| `max_bytes` | integer | `51200` (`50 * 1024`) | Maximum bytes of tool output retained. Must be greater than zero. |

# Deferred Tools [deferred_tools]

On a model that loads [deferred tools](../reference/tools.md#deferred-tools),
the tools of these built-in groups are deferred: `agents`, `cron`, `goal`,
`memory`, and `runtimes`. The model sees their names in announcements and
loads them with `tool_search` when it needs them. A deferred group's
instructions, such as memory's, arrive with its first load instead of in the
system prompt. Complete `config.toml` to send a group up front instead:

```toml
[deferred_tools]
always_load = ["memory"]
```

An omitted table inherits; an authored table replaces the lower table and uses
defaults for omitted fields. Unknown fields fail. See
[config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `always_load` | array of strings | `[]` | Built-in groups sent up front: any of `agents`, `cron`, `goal`, `memory`, and `runtimes`, each at most once. Any other name fails. |

MCP servers and plugins have their own `always_load` keys: see
[MCP](../extend/mcp.md) and [plugins](../extend/plugins.md).

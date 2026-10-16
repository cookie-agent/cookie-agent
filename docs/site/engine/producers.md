# External Producers [producers]

Bound what plugins and runtimes may send into sessions as producers. Complete
`config.toml`:

```toml
[producers]
max_body_bytes = 65536
```

A plugin or a runtime produces under an `external { source, name }` owner:
the daemon derives `source` from the connection (`plugin:<name>` or
`runtime:<attachment name>`), and the sender picks a `name` for each
producer it registers. See the [external producers
spec](../specs/external-producers.md). An omitted table inherits; an authored
table replaces the lower table and uses defaults for omitted fields. Unknown
fields fail; see [config.toml](../guide/configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `max_body_bytes` | integer | `65536` | Largest body an external producer may send, in UTF-8 bytes. A larger or empty body fails the send; the body otherwise reaches the model as sent. Must be 1..=4194304. |

The limit covers external producers only. The engine's own producers (background
results, subagent reports, goal reminders) have their own bounds, and agent
mail is bounded by [`[messaging] max_body_bytes`](messaging.md).

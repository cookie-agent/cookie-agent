# Approval [approval]

Set the expiry for pending user approvals. Complete `config.toml`:

```toml
[approval]
timeout_ms = 30000
```

This does not select a permission mode or grant tools; those decisions belong to
[Agent permissions](../guide/agents.md#permissions). Expiry leaves an unattended
request unapproved. An omitted table inherits; an authored table replaces the
lower table and uses defaults for omitted fields. Unknown fields fail; see
[config.toml](../guide/configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `timeout_ms` | integer | `30000` | How long a user approval stays pending before it expires unattended. Must be greater than zero. |

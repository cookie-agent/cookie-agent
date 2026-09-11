# Agent Messaging [messaging]

Bound agent-to-agent mail (`send_message`) inside a delegation tree. Complete
`config.toml`:

```toml
[messaging]
enabled = true
max_hops = 0
max_body_bytes = 32768
max_pending_per_session = 32
max_inflight_per_pair = 4
```

Authority to send at all comes from `message` permission rules in agent
documents, not from this table; messaging is deny-by-default even when this
feature is enabled. See [Agent Messaging](../specs/agent-messaging.md). An
omitted table inherits; an authored table replaces the lower table and uses
defaults for omitted fields. Unknown fields fail; see
[config.toml](../guide/configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Master switch. When `false`, `send_message` fails with the `send_message:disabled` tool error even if permission rules allow the send. |
| `max_hops` | integer | `0` | Chain-length guard for message hops. Values `<= 0` mean unlimited. Parsed but not enforced in Phase 1; hop counting and rejection rules arrive with the Phase 2 guard. |
| `max_body_bytes` | integer | `32768` | Maximum UTF-8 size of a message body. Tighter than the 256 KiB plugin body cap because agent mail should be terse. A `0` value is accepted at parse time and rejects every send at runtime with `send_message:invalid_body`. |
| `max_pending_per_session` | integer | `32` | Pending agent mail per recipient session. Beyond the cap, new sends fail with `send_message:inbox_full` and nothing is admitted or discarded. Only `Agent`-owner producer messages count; plugin, delegation-notification, and goal mail never consume the budget. |
| `max_inflight_per_pair` | integer | `4` | Per-directed-pair in-flight window to stop ping-pong storms. Parsed but not enforced in Phase 1. |

`max_hops` and `max_inflight_per_pair` are intentionally parsed but unenforced
in Phase 1 so operators can author final-shaped configuration today.
`max_body_bytes` and `max_pending_per_session` boundaries are enforced at send
time; no value here is validated at load time beyond its type.

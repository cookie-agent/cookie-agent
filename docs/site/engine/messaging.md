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

All delivery is producer-backed: running recipients receive mail at their next
safe model-request boundary, and idle or finished recipients wake through the
producer reconcile path rather than user-steering machinery.

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Master switch. When `false`, `send_message` fails with the `send_message:disabled` tool error even if permission rules allow the send. |
| `max_hops` | integer | `0` | Chain-length guard for message hops. Hop counting is internal producer metadata and never appears in the delivered envelope. A send that would exceed the limit fails with `send_message:max_hops_exceeded`. Values `<= 0` disable the guard. |
| `max_body_bytes` | integer | `32768` | Maximum UTF-8 size of a message body. Tighter than the 256 KiB plugin body cap because agent mail should be terse. A `0` value is accepted at parse time and rejects every send at runtime with `send_message:invalid_body`. |
| `max_pending_per_session` | integer | `32` | Pending agent mail per recipient session. Beyond the cap, new sends fail with `send_message:inbox_full` and nothing is admitted or discarded. Only `Agent`-owner producer messages count; plugin, delegation-notification, and goal mail never consume the budget. |
| `max_inflight_per_pair` | integer | `4` | Per-directed-pair window of unacknowledged messages, to stop ping-pong storms. A pair message is unacknowledged while accepted, admitted, claimed, or released, and acknowledged once it is consumed or discarded. A full window fails the send with `send_message:inflight_full`. A value of `0` disables the guard. |

`max_hops` and `max_inflight_per_pair` are enforced at send time and disabled
by non-positive and zero values respectively, so operators can opt out of each
guard independently. `max_body_bytes` and `max_pending_per_session` boundaries
are enforced at send time; no value here is validated at load time beyond its
type.

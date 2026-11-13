# Webhooks [webhooks]

Limits for incoming webhooks, the URLs that turn HTTP requests into
messages in sessions (see [Webhooks](../sessions/webhooks.md) and the
[webhooks spec](../specs/webhooks.md)).
Complete `config.toml`:

```toml
[webhooks]
enabled = true
max_body_bytes = 1048576
inline_body_bytes = 16384
max_per_hour = 60
max_consecutive_failures = 3
backlog = 100
history = 50
```

An omitted table inherits; an authored table replaces the lower table and uses
defaults for omitted fields. Unknown fields fail; see
[config.toml](configuration.md). The address webhook URLs are built on, and
the port that serves them to the internet, are [`[public]`](public.md)
settings.

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Whether the daemon accepts webhook requests. Off, every webhook URL answers `404`; webhooks are kept and work again once it is on. |
| `max_body_bytes` | integer | `1048576` | The largest request body accepted, in bytes; a larger request answers `413`. Must be 1..=67108864. |
| `inline_body_bytes` | integer | `16384` | The most of a body the event's message shows inline, in bytes. A larger body is stored as an artifact the agent can read, and only its start is shown. Must be 1..=1048576. |
| `max_per_hour` | integer | `60` | Events a webhook accepts per rolling hour, unless it sets its own; more answer `429`. 0 or less means no limit. |
| `max_consecutive_failures` | integer | `3` | Failures in a row that disable a webhook, unless it sets its own. 0 or less never disables. |
| `backlog` | integer | `100` | Events a new-session webhook holds while as many of its sessions run as it allows; a full backlog answers `503`. Must be 1..=10000. |
| `history` | integer | `50` | Events each webhook's history keeps, oldest dropped first. Must be 1..=1000. |

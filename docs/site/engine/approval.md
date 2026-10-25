# Approval [approval]

Set the expiry for pending user approvals and how long the approval
reviewer's conversation stays in its provider's prompt cache. Complete
`config.toml`:

```toml
[approval]
timeout_ms = 30000
cache_ttl = "1h"
```

This does not select a permission mode or grant tools; those decisions belong to
[Agent permissions](../extend/agents.md#permissions). Expiry leaves an unattended
request unapproved. The internal approval evaluator's own model-call budget is
independent of this user-response window. An omitted table inherits; an authored
table replaces the lower table and uses defaults for omitted fields. Unknown
fields fail; see [config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `timeout_ms` | integer | `30000` | How long a user approval stays pending before it expires unattended, counted from when the call reaches you. Must be greater than zero. |
| `cache_ttl` | string | `"1h"` | The reviewer's prompt-cache lifetime: `"1h"`, `"30m"`, `"12h"`, `"5m"`, `"provider"` (the reviewer model's provider's own `cache` configuration), or `"off"` (no cache writes). See [The reviewer's cache](#the-reviewers-cache). |

## The reviewer's cache

The reviewer keeps one conversation per session and adds a turn per review,
so each request ends with a rolling cache marker that the next review reads
back. `cache_ttl` maps onto the provider of the model the reviewer runs on,
whatever the session's provider is. A lifetime the provider does not offer
becomes the nearest one it does, by ratio with ties going longer, and
`cookie config status` says so:

| Reviewer's provider | `1h` | `30m` | `12h` | `5m` | `provider` | `off` |
|---|---|---|---|---|---|---|
| Anthropic | rolling 1h; system and tools raised to 1h | 1h | 1h | rolling 5m | the provider's `cache` table and the session layer's defaults | no markers |
| Bedrock | 1h on models that take it, else 5m | as `1h` | as `1h` | rolling 5m | the provider's `cache` table | no cache points |
| OpenAI Responses with the GPT-5.6 controls | rolling breakpoint at 30m | 30m | 12h | 30m | the provider's `cache` table | explicit mode with no breakpoint |
| OpenAI Chat Completions, Azure, with the controls | rolling breakpoint at 30m | 30m | 30m | 30m | the provider's `cache` table | explicit mode with no breakpoint |
| OpenAI and Azure without the controls, OpenAI-compatible, Gemini, Cohere | no effect: the provider caches on its own | | | | | |

The value is read at each review, so a configuration change reaches the next
one. The title agent never sends a rolling marker.

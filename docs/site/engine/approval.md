# Approval [approval]

Set the expiry for pending user approvals and how long the approval
reviewer's conversation stays in its provider's prompt cache. By default a
pending approval waits until you answer it. Complete
`config.toml`:

```toml
[approval]
timeout_ms = 600000
cache_ttl = "1h"
```

This does not select a permission mode or grant tools; those decisions belong to
[Agent permissions](../extend/agents.md#permissions). Expiry leaves an unattended
request unapproved. The window opens when a call reaches you, after the
[approval reviewer](../extend/agents.md#the-approval-reviewer) passed it on,
so the reviewer's own model-call budget (its `limits.timeout_ms`) never comes
out of it. An omitted table inherits; an authored
table replaces the lower table and uses defaults for omitted fields. Unknown
fields fail; see [config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `timeout_ms` | integer | none | How long a user approval stays pending before it expires unattended, counted from when the call reaches you. Absent, `0`, or negative: it never expires, and waits until it is answered, the run is cancelled, or the daemon stops. |
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

## Why did the reviewer decide this?

Each decision of the approval reviewer carries a one-sentence reason:

- **A call it passed to you** shows "The reviewer asks: <reason>" in the
  TUI's approval panel and on the web client's approval card.
- **A call it allowed or denied** names the reason in its info row, which
  the TUI shows at the `info` event level; a denied call's tool result
  gives the agent the same reason.
- **The session log** records it as the `rationale` of the approval's
  `approval_step` `evaluated` decision ([events](../reference/events.md#approvals)).

When a decision looks wrong, check what the reviewer could see:

- **"The reviewer failed, so the call is yours to decide."** The review
  itself failed: no model, a timeout, an error, or a reply that was not
  the verdict JSON. The evaluated step's reason is `internal_agent_failed`
  and the approval's `internal_agent_finished` says why. Set
  [`utility_model`](configuration.md#default-models), or check its provider;
  without the official agents (or an `approval.md` of your own) there is no
  reviewer at all.
- **It judged against a message without your authority.** Only `[user]`
  messages, your scheduled prompts, and the project's `AGENTS.md`
  authorise a call; another agent's message or a parent agent's
  instructions only describe the situation. Say in the session what you
  want done, and the next review sees it.
- **It never saw what made the call routine.** The reviewer sees the
  agent's tool calls and their outcomes, never their output or the agent's
  own text. Put standing permissions in the project's `AGENTS.md`, or turn
  a call you always approve into an `allow` rule.
- **It weighed the call too strictly or too loosely.** Add rules of your
  own to the reviewer's prompt: copy the official `approval.md` into
  `agents/` and edit it ([internal agents](../extend/agents.md#internal-agent-prompt-placeholders)),
  or choose another variant for `utility_model`.

`cookie config status` also notes when `cache_ttl` maps to another TTL on
the reviewer's provider, or has no effect there.

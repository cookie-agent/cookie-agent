# Recovering from a thinking-binding mismatch

Status: **implemented** (2026-10-26; client protocol 1.33.0).

## 1. Problem

Anthropic binds each thinking block's `signature` to the conversation
prefix it was made in: the top-level `system`, the set of tools in
`tools`, and every earlier message. A request that replays a block whose
prefix changed is refused, on accounts the API enforces, before any output:

```text
messages.5.content.0: Invalid `signature` in `thinking` block. The block is
bound to a different conversation. Remove the block, or set
`thinking.block_binding.prefix_mismatch_behavior` to "drop_block". ...
The `tools` list differs from the one this block was created with.
```

Retrying the same body fails the same way. Cookie's history is append-only,
but its tools change between requests for good reasons (MCP servers
connecting, attachments, plugins, a build that changes a tool's schema),
and its system prompt can change between runs. On 2026-10-25 a build that
changed `memory_write`'s schema left a running session failing every
request.

Anthropic's controls (`thinking-binding-controls-2026-08-01` with
`prefix_mismatch_behavior: "drop_block"`) would have the API drop the
stale blocks, but a probe of 2026-10-26 showed that the user's gateway
forwards the beta header and strips `thinking.block_binding`, so a request
cannot ask for it there.

## 2. Decision

User decision of 2026-10-26: **recover when it happens** (the documented
strip-and-retry), with no prevention ahead of time and no `drop_block`.

## 3. Recovery

- **Detection.** A model attempt fails with `invalid_request` (HTTP 400)
  whose message contains `bound to a different conversation`, Anthropic's
  documented text for this check (a tampered signature reads differently
  and is not recovered).
- **The cut.** The attempt is abandoned as now, and the engine appends
  `native_replay_cut { through_seq }`: the reasoning of every assistant
  turn at or before `through_seq` (the last committed turn) is no longer
  replayed. That is a leading run of thinking blocks, which the API allows
  removing; each turn keeps its text and tool calls.
- **The retry.** The attempt is retried at once, without the model retry
  backoff and without using a `[model_retry]` retry. It is the only one for
  that attempt: if the retried request is refused again, the attempt fails
  as any refused request does (and the run fails, toward the held-sessions
  budget).
- **Later requests.** Context building leaves out the reasoning of turns
  at or before the latest cut, with the replay decision
  `discarded` (*reasoning before a thinking-binding mismatch is not
  replayed*), so every later request matches the prefix the new blocks
  were made in. Reasoning made after the cut replays normally; a second
  mismatch moves the cut forward.
- **The transcript** says so once per cut, as a diagnostic: *The provider
  refused the earlier reasoning (its tools or system prompt changed since);
  the request was sent again without it.*
- **Forks and reverts.** A fork copies the cut with the prefix. A revert to
  before it removes it, as any event.

## 4. Protocols and history

- `native_replay_cut` is an additive state event: client protocol 1.33.0,
  history format unchanged. A reader that does not know it opens the log
  render-only, as for any unknown state event, and shows the diagnostic.
- No plugin or runtime protocol change, and no oven-sdk change.

## 5. Tests

- A scripted Anthropic endpoint refusing a replay with the documented text:
  one cut, one immediate retry without the earlier reasoning, the turn
  completes, the diagnostic once.
- The retried request refused again: the attempt fails, one cut only.
- After the cut: later requests leave out reasoning at or before it and
  replay reasoning after it; a second mismatch moves the cut.
- A tampered-signature 400 (no *bound to a different conversation*): no
  recovery.
- Fork and revert across a cut.

## 6. After it lands

The `memory_write` schema that takes `keywords` as a list or one
comma-separated string (reverted by 04bcc7417, restored locally by
fca2518d1) ships with it: a running session that meets the changed schema
recovers on its next request.

## Implementation notes

- **Compaction** (2026-10-26, user report): a summary request the provider
  refuses for the same reason is tried once more with every reasoning part
  and native replay left out (`model_history::strip_reasoning`), keeping
  the tools; the internal agent returns the mismatch when every model
  refused for it, instead of a generic failure. A manual compaction now
  builds its tools as a run would (goal tools as at admission, delegation
  targets from the registry), as automatic compaction uses its run's, so
  its request stays a prefix of the conversation's.

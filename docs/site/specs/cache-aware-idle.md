# Cache-aware idle sessions: compaction before the cache expires

Status: **approved** (2026-11-02), with the user's decisions in
[section 2](#2-decisions-2026-11-02); phase 0 (a probe) still to run. Builds on [cron jobs](cron.md) (sections
4 and 5), [native compaction](native-compaction.md), [ACP agents](acp-agents.md)
(section 7, sessions and processes; section 15, compaction), and the
residency janitor (`crates/engine/src/runtime/sessions/residency.rs`).

## 1. Goal and scope

**Today:**

- **Residency.** The janitor checks every minute. It pages out a root
  session's tree once the whole tree has gone
  `[server] session_idle_eviction_after` (default `30m`) with no event, no
  live tail, and no pending work. Delegated sessions also have their own
  window, `[delegation] idle_eviction_after` (default `1h`). Paging out a
  session that runs an ACP agent sends `session/close`. Its next turn
  resumes or loads the ACP session, or starts a new one. The agent's
  process stops `[acp] idle_shutdown_ms` (default 5 minutes) after its last
  live session closes.
- **Compaction** happens for three reasons only: the context passes its
  size trigger, the user runs `/compact`, or a request overflows the
  context. Nothing looks at how long ago the provider last cached the
  session.
- **Cache TTLs** are known per request. Root sessions on Anthropic and
  Bedrock put a one-hour rolling marker on the conversation, and delegated
  sessions a five-minute one (`CacheTtls::defaults`). OpenAI's GPT-5.6
  controls cache for `30m` or `12h`, and `prompt_cache_retention = "24h"`
  keeps a cache for a day. Every other provider caches on its own terms.

**Two problems:**

1. **ACP agents do not compact while idle.** Interactive Claude Code
   compacts its own context as its one-hour cache nears expiry, but not
   over ACP (section 11.1). An idle Claude Code session's next turn
   re-caches its whole context.
2. **Idle sessions waiting on a scheduled job pay a full cache write.** Take
   a session with 150k tokens of context and a cron job that fires in 3
   hours. The cache has long expired by then, so the fire re-sends and
   re-caches all 150k tokens. On Opus, that write costs about $1.50 at the
   one-hour write price. Compacting just before the cache expires costs
   less:
   - reading the warm prefix: about $0.08;
   - the summary's output: about $0.13;
   - writing the smaller context (about 25k tokens) at the fire: about
     $0.25.

   That is roughly $0.45 in total. The compaction call also refreshes the
   one-hour system and tools markers, so a fire within the next hour still
   reads those from cache.

**This spec:**

- compacts an idle session shortly before its cache expires (default 5
  minutes before, at most a fifth of the TTL) when a scheduled job will wake it only after the cache is
  gone;
- does the same for ACP agents that declare a cache TTL, with the agent's
  `/compact`;
- adds a knob for it, and a per-model TTL for providers cookie cannot
  read one from.

**Out of scope:**

- keeping the cache warm with pings (section 6 explains why, and how it
  could fit later);
- predicting when the user will come back;
- changing `[acp] idle_shutdown_ms`;
- a residency window of their own for ACP sessions (D1, withdrawn).

## 2. Decisions (2026-11-02)

The user accepted D4, D6, D7, and D8 as proposed, withdrew D1 once phase
0 showed Claude Code does not compact while idle over ACP, made `off` the
default mode (D2), set the slack to five minutes capped at a fifth of the
TTL (D3), and asked for a token floor and a cost floor side by side (D5).
Section 10 records the answers to the remaining questions.

- **D1. ~~The ACP residency window~~ (withdrawn 2026-11-02).** The
  proposal was `[acp] session_idle_eviction_after`, default `90m`, so that
  Claude Code could compact itself before cookie closed its session. Phase
  0 showed it never does over ACP. Cookie compacts ACP sessions itself
  (D6), and reloading one leaves the provider's cache as it was. ACP
  sessions keep `[server] session_idle_eviction_after`. Section 3.
- **D2. The knob.** `[context_compaction.cache_expiry] mode` takes three
  values:
  - `off` (the default; a compaction the user did not ask for is opt-in);
  - `scheduled`: only sessions a scheduled job will wake
    after the cache expires;
  - `always`: every idle session, whether or not a job will wake it. This
    is what the user reports Claude Code does, and it pays for a summary of
    every session left idle.

  Section 4.
- **D3. Slack.** `slack = "5m"`, capped at 20% of the TTL:
  `effective_slack = min(slack, ttl / 5)`. A one-hour TTL compacts 5
  minutes before expiry, a ten-minute TTL 2 minutes before, and a
  five-minute TTL 1 minute before. The compaction starts at
  `expiry - effective_slack`, and the summary request has to start before
  expiry.
  The scheduler wakes at that time; it does not wait for the next one-minute
  janitor check.
- **D4. Unknown TTLs.**
  - `fallback_ttl = "30m"` for providers whose TTL cookie does not set and
    cannot read.
  - A model override `cache_ttl = "<duration>"` declares a TTL where cookie
    sets none. Where cookie sets the TTL itself (Anthropic and Bedrock
    markers, OpenAI's GPT-5.6 controls or 24h retention), its own value
    wins.

  Section 4.1.
- **D5. When compaction is worth it: two floors.** `min_tokens` (default
  `40000`) and `min_cost_usd` (default `0`). Either can be turned off with
  a value of 0 or less:
  - **both off:** every session that meets the other conditions is
    compacted;
  - **one on:** that floor alone decides;
  - **both on:** reaching either floor is enough.

  The cost floor applies only to models with prices; a model with no price
  never reaches it. So a positive `min_cost_usd` with `min_tokens = 0`
  compacts only priced models, and only above that cost. Section 4.2
  defines both measures.
- **D6. ACP sessions.** Cookie compacts an ACP session before expiry only
  when its agent declares a TTL (`[acp.agents.<id>] cache_ttl`). It does
  so with the agent's own `/compact` command. Without a declared TTL, the
  agent manages its own context.
  The Claude Code preset leaves `cache_ttl` unset, *if* the phase 0 probe
  confirms that Claude Code compacts on its own while idle. If the probe
  shows it does not, the preset sets `cache_ttl = "1h"` instead.
  *Phase 0 showed it does not (section 11.1), so the preset sets
  `cache_ttl = "1h"`. The Codex preset sets `cache_ttl = "30m"` (section
  11.2; user decision of 2026-11-02).*
- **D7. The checkpoint records why it was made.** Clients show
  "Compacted before the cache expired · next scheduled prompt 03:00".
  Section 4.4.
- **D8. Keep-warm pings stay out of v1.** Section 6.
- **D9. A cache badge in the session header** *(approved 2026-11-02)*. It
  shows how long the prompt cache stays warm, as section 4.5 describes,
  and the [preview](cache-aware-idle/preview.html) shows. The daemon
  computes the clock, and clients only count down. "Hot" gets a colour
  token of its own. The badge shows whenever a session has a clock, with
  any `mode`.

## 3. ACP residency (withdrawn)

ACP sessions page out like every other session, after
`[server] session_idle_eviction_after`. A session paged out between its
last turn and its compaction is loaded again at the due time (section
4.3). The compaction resumes or loads the ACP session, and the provider's
cache is still there. This section once proposed a longer window (D1).

## 4. Compacting before the cache expires

### 4.1 The cache clock

For each session, cookie keeps two values:

- **`last_request_at`:** when the session's own model loop last sent a
  request: the timestamp of its latest `model_usage_recorded`. Requests
  from internal agents do not count: the approval reviewer and the title
  agent use their own conversations.
- **`ttl`:** the cache TTL that request used. Section 7 stamps it on the
  usage event when the event is written, so a reload or a restart does not
  have to work it out again from the configuration. The table below shows
  where the TTL comes from.

| Binding | `ttl` |
|---|---|
| Anthropic, Bedrock | The resolved rolling marker: `5m` or `1h`. If the marker is `off`, the session has no clock and is skipped. |
| OpenAI and Azure Responses with the GPT-5.6 controls | The controls' `ttl`: `30m` or `12h`. |
| OpenAI with `prompt_cache_retention = "24h"` | `24h`. |
| ChatGPT sign-in (the `chatgpt` provider) | `30m`, the documented GPT-5.6+ default. The announced "forever" for Codex sign-ins is unconfirmed (section 11.2). A model override's `cache_ttl` replaces it. |
| Any other binding (OpenAI's in-memory retention, Gemini's implicit caching, OpenAI-compatible endpoints, DeepSeek, …) | The model override's `cache_ttl` if set, else `fallback_ttl`. |

A `cache_ttl` of `0s` or less, in a model override or an ACP agent's
table, means the cache never expires, so no compaction is due.

**`expiry = last_request_at + ttl`**. A new request resets the clock, since
a cache hit refreshes the TTL.

### 4.2 The rule

A session is **due** for compaction at `expiry - effective_slack` (D3) when all of these
hold:

1. **The mode allows it.**
   - Under `scheduled`, the session is the target of an enabled
     in-session job, and that job's earliest `next_fire` comes after
     `expiry`. A fire before `expiry` reads a warm cache, so there is
     nothing to save.
   - Under `always`, no job is needed.
2. **The session is idle.** It has no pending work, as the residency
   janitor defines it (`session_has_pending_work`), and its tree has no
   running session.
3. **It is not held** ([held sessions](held-sessions.md)). A held session
   skips its fires, so under `scheduled` nothing will wake it.
4. **The context is worth compacting** (D5). Two measures, each taken
   from the latest turn:
   - **tokens:** the reported input plus output tokens, the same measure
     the size trigger uses;
   - **cost:** what re-caching that context at the next request would cost:
     the tokens times the model's cache-write rate (`cache_write`), or its
     input rate when it has no cache-write rate *(user decision of
     2026-11-02)*, whatever `ttl` is. A model with neither has no cost.

   With `min_tokens` and `min_cost_usd` both 0 or less, this condition
   always holds. Otherwise, it holds when tokens reach a positive
   `min_tokens`, or when cost reaches a positive `min_cost_usd`.
5. **There is something new to compact.** No checkpoint has been
   committed since the latest turn.
6. **This idle period has not had a try yet.** A failed attempt is not
   repeated until the session runs again.
7. **The cache is still warm.** It is not yet `expiry` when the summary
   request would start. A timer that wakes late, after a suspend or a busy
   daemon, compacts nothing, as Claude Code's own idle compactor does
   (section 11.1).

**Why the default token floor is 40k.** With context `C`, a summary of output `O`, and a
compacted context `C'` of about 25k tokens (summary plus the 16k recent
tail), compaction pays when:

```text
C × write  >  C × read + O × output + C' × write
```

On Anthropic's one-hour prices (write 2×, read 0.1×, output 5× the input
price) and `O` around 5k, the break-even point is near `C = 40k`. The
cost measure prices those 40k tokens at the catalog's `cache_write` rate,
the five-minute one (1.25× input): about $0.25 on Opus, a matching
`min_cost_usd` for anyone who would rather set the floor by cost.

### 4.3 The scheduler

- **One timer task for the whole engine**, built like the cron scheduler.
  It keeps an in-memory map of `session → due_at`, sleeps until the
  earliest entry, and wakes early when the map changes.
- **These events recompute a session's entry:**
  - a run ending or a usage event being recorded;
  - a run starting, which drops the entry;
  - a job being created, updated, deleted, fired, enabled, or disabled;
  - a session being held, resumed, or deleted;
  - `config.reload`.
- **Residency.** An entry does not keep its session in memory, matching the
  cron spec's rule. At the due time the session is loaded again if it was
  paged out. A native root pages out at 30 minutes and its one-hour cache
  is due at 58, so this is the usual case. Loading reads only the disk.
  The cache lives at the provider, so it is still warm.
- **At daemon start:**
  - under `scheduled`, the scheduler computes entries for the targets of
    enabled in-session jobs, reading each target's latest usage event;
  - under `always`, it computes entries only for sessions that are in
    memory and for job targets. Sessions that paged out before a restart
    are not scanned.
- **At the due time** the scheduler checks the rule again under the
  session's residency lock, then compacts.

### 4.4 The compaction

- **The compaction itself** is the ordinary path that `session.compact`
  without a focus takes. The internal summary and, on Responses models,
  native compaction run side by side. The summary request is a prefix of
  the session's latest request, so it reads the warm cache.
- **Its origin is the system, not the user.** The checkpoint records why it
  was made, in a new optional field:

  ```json
  "reason": {
    "cache_expiry": {
      "expires_at": "…",
      "next_fire": "…",
      "job_id": "k3v9q2xa"
    }
  }
  ```

  Under `always`, `next_fire` and `job_id` are absent.
- **Clients** show the checkpoint row as "Compacted before the cache
  expired", followed by "· next scheduled prompt <local time>" when a job
  is waiting.
- **Input during compaction.** A user message or a job fire that arrives
  while it runs waits for it to finish, as with manual compaction. Cancel
  stops it the same way.
- **On failure:**
  - a runless diagnostic is recorded, kind `cache_expiry_compaction`;
  - rule 7 stops a retry;
  - it does not count toward automatic compaction's three-failure counter.
- **ACP sessions** with a declared `cache_ttl` compact through
  `compact_acp_session` (`/compact`). Cookie does not see their requests,
  so `last_request_at` is the end of their last run.
- **Cost** is recorded the same way as any compaction's
  (`internal_agent_usage_recorded`).

### 4.5 Showing the cache's heat (D9)

The [preview](cache-aware-idle/preview.html) shows every state below, in
both themes and both clients.

**The badge.** The session header (Quiet Ink D6) gains a badge between
the goal and the context ring:

- **The icon:** a flame (Lucide `Flame`) while the cache is warm. Once it
  has expired, a snowflake (`Snowflake`) with the words "cache expired".
- **The countdown:** minutes left, such as `41m`. Above 90 minutes it shows
  hours (`11h`), and in the last minute, seconds.
- **The colour grades the heat:** the share of `ttl` left decides it.
  - hot: more than half is left. The badge uses a new `hot` token
    (`#c2410c` light, `#fb923c` dark), since amber already means an
    approval is waiting.
  - warm: a fifth to a half is left. The same colour, faded.
  - cooling: less than a fifth is left. The countdown turns `warn`.

  The icon and the countdown say the same thing without the colour.
- **Marks:**
  - `~` before the countdown when the TTL is assumed (`fallback_ttl`,
    the `chatgpt` provider's `30m`) or declared (a model's or an ACP
    agent's `cache_ttl`), rather than set by cookie on the request;
  - an accent dot when a compaction before expiry is planned;
  - `∞` for a TTL of `0s` or less.
- **During a run:** the flame shows with no countdown.
- **While a compaction before expiry runs:** a spinner and "compacting".
- **No badge** when there is no clock: before the first request, or with
  the rolling marker `off`.
- **The badge shows in any `mode`, `off` included.** The clock is useful
  on its own.
- **Which session's clock:** the one shown. A subagent's clock shows when
  its session is opened.

**The tooltip** gives:

- the state and the time left;
- when the last request went out, the TTL and where it came from, and
  when the cache expires;
- when a job is scheduled, its time, and when the compaction will run.

**A click** opens the session panel's Usage tab. It gains a *Prompt
cache* card:

- a meter from the last request to expiry, with a tick at the planned
  compaction;
- the TTL and its source;
- the tokens cached;
- the next scheduled prompt;
- what caching again would cost, by section 4.2's cost measure;
- the `mode`, and the compaction's time when one is planned.

**The TUI.** The status line shows `🔥 41m` or `❄ cache expired` in the
same states, after the cost and before the context. When the line is too
narrow, it is dropped after the cost. Under `NO_COLOR`:

- the glyph and the countdown carry the state;
- bold marks cooling.

Clicking the cache part opens the usage panel.

**The clock comes from the daemon.** A session's view carries an optional
`cache_clock`:

- `last_request_at`;
- `ttl_secs`: `null` for never expiring;
- `ttl_source`: `set`, `declared`, or `assumed`;
- `compaction_at`, when a compaction before expiry is planned;
- `next_fire` and `job_id`, when a scheduled job decides it;
- `compacting`.

A transient `session_cache_clock` event sends a changed clock to those
watching. Clients compute only the countdown and its heat from
`last_request_at` and `ttl_secs`. They need no rules of their own.

## 5. Configuration

```toml
[context_compaction.cache_expiry]
mode = "off"            # off (default) | scheduled | always
slack = "5m"            # capped at ttl / 5
fallback_ttl = "30m"
min_tokens = 40000      # <= 0: no token floor
min_cost_usd = 0.0      # <= 0: no cost floor; priced models only

[acp.agents.claude]
cache_ttl = "1h"              # optional; turns on section 4 for this agent

[models.<provider>.<model>]
cache_ttl = "1h"              # optional; for providers cookie cannot read a TTL from
```

- **Durations** use the existing compact form (`ms`, `s`, `m`, `h`, `d`).
- **`cache_ttl = "0s"`** (or less) declares a cache that never expires.
  Section 4 then never compacts that model's or agent's sessions.
- **`slack`** must be positive. It applies as `min(slack, ttl / 5)`.
- **`min_tokens`** is an integer and **`min_cost_usd`** a number of
  dollars. Each is off at 0 or less; section 4.2, condition 4, combines
  them.
- **`mode = "off"`** turns section 4 off, for ACP agents too.
- **Per-agent settings.** `cache_expiry` follows
  `[context_compaction]`'s per-agent inheritance. An agent document's
  table can turn it off for that agent.

## 6. Keep-warm pings (not in v1)

**What a ping is.** Instead of compacting, cookie could send a minimal
request (the same prefix, one output token) shortly before expiry. That
reads the cache and resets its TTL.

**What it costs.** Each TTL period costs `C × read`, about 0.1× of a cache
write's 2×. Pinging until the fire costs about `(fire − now) / ttl × 0.1C`.

**When it pays:**

- **Cheaper than a rewrite** for gaps shorter than about 19 TTLs. With a
  one-hour TTL, that is a fire within about 19 hours.
- **Cheaper than compacting** only for short gaps, about 3 to 4 TTLs.
- **It keeps the whole context**, where compaction loses detail.

**Why it is left out.** It sends requests that do no work, at regular
intervals. Some providers' terms or rate limits may not welcome that. It
also needs a rule for choosing among three options, not two.

**How it could fit later.** The rule in 4.2 could become a choice of the
cheapest option: nothing, ping, or compact.

## 7. Protocol and history

- **`ModelUsageRecorded.cache_ttl_secs`:** optional, stamped when the event
  is written. Older logs lack it, and the TTL is resolved from the binding
  then.
- **`ContextCheckpointCommit.reason`:** optional; section 4.4.
- **Diagnostic kind** `cache_expiry_compaction`.
- **The session view's `cache_clock`** and the transient
  `session_cache_clock` event (section 4.5).
- **Configuration schema:** the keys in section 5.
- **Versions.** All additive, so this is a client protocol minor bump. The
  plugin protocol follows the event changes, as it does for every event
  change.

## 8. Tests

The tests use explicit values, never the defaults.

- **Due time:**
  - a job firing before expiry leaves the session with no entry;
  - a job firing after expiry makes it due at `expiry - effective_slack`,
    with `slack` above and below `ttl / 5`;
  - several jobs: the earliest `next_fire` decides.
- **Each skip in 4.2:**
  - the session is held;
  - it is running, or another session in its tree is;
  - each combination of the floors: both off, only tokens, only cost, and
    both, with sessions below and above each floor;
  - a positive cost floor on a model with no price;
  - the cost using the cache-write rate, and the input rate when the
    model has no cache-write rate;
  - a checkpoint is already newer than its latest turn;
  - its idle period already had a failed try;
  - its rolling marker is `off`.
- **The entry changes:**
  - a user turn before the due time drops it;
  - the next run sets it again.
- **TTL resolution.** One test per row of 4.1's table, and a declared
  `cache_ttl` losing to a TTL cookie sets itself.
- **A paged-out session** is loaded again at the due time and compacted.
  Its checkpoint carries `reason.cache_expiry`.
- **Restart.** Entries for job targets are computed again from the stamped
  `cache_ttl_secs`.
- **ACP compaction:**
  - with a declared `cache_ttl`, the agent's `/compact` runs at the due
    time, on a session that paged out too;
  - without one, nothing runs.
- **The cache clock and the badge** (section 4.5). These test what the
  badge is made from, never how it looks:
  - **the clock the daemon sends:** its source for each row of 4.1's
    table, `compaction_at` and `next_fire` when the rule plans a
    compaction, and a `session_cache_clock` event when a run, a job, or a
    compaction changes it;
  - **the client's arithmetic:**
    - the heat boundaries (a half and a fifth of the TTL, and expiry);
    - the countdown's units (minutes, hours above 90 minutes, seconds in
      the last minute);
    - no countdown during a run;
    - no badge without a clock;
  - **the TUI's status line:** the cache part drops after the cost.

## 9. Phases

0. **Probe.** Confirm over ACP that Claude Code (`claude-agent-acp`)
   compacts on its own while idle near its one-hour mark, and note when.
   The result settles D6.
1. **The cache clock and compaction before expiry**, engine and protocol
   (sections 4, 5, and 7).
2. **Clients and docs:**
   - the cache badge, its tooltip, and the Usage tab's card on the web;
   - the status line part and the usage panel in the TUI;
   - the checkpoint row in both clients;
   - the docs:
     `engine/context_compaction.md`, `engine/server.md`, the ACP agents
     guide, and `sessions/scheduling.md`.

## 10. Resolved questions (2026-11-02)

- **`always` after a restart:** it covers sessions in memory and job
  targets only. There is no scan of sessions that paged out before the
  restart.
- **Opting a job out:** not possible. The mode (default `off`) and
  per-agent inheritance are the only controls.
- **Native roots staying in memory until their cache expires:** no. Loading
  again is cheap, and the cache lives at the provider.
- **Compaction on a different model** (an authored `compaction.md` off
  `${parent_model}`): it goes ahead anyway. It still shrinks what the fire
  has to cache.
- **A watched session:** compacted anyway. Watching is not using.
- **Activity:** a compaction before expiry is not activity:
  - the idle clock for paging out counts from the session's last run
    (`root_tree_idle` ignores the compaction's events);
  - it does not move the session up in session lists;
  - it does not mark the session unread;
  - it sends no push notification.
- **Phase 0:** the probe runs before phase 1, and its result settles D6.

## 11. Implementation notes

### 11.1 Phase 0: Claude Code's idle compaction (2026-11-02)

**What was checked.** The JavaScript bundled in the Claude Code binary that
`@agentclientprotocol/claude-agent-acp` 0.100.0 runs: the
`@anthropic-ai/claude-agent-sdk-linux-x64` binary of SDK 0.3.316, the same
2.1.3xx line as the installed CLI. This was a reading of the code. No live
run was made: the code gates the feature by an environment check (below),
so a live run could only confirm the gate.

**There are two idle compactors.** Both are on by default, and the
`idleCompaction` setting (`false` in `settings.json`) turns both off. Both
also need auto-compact on.

- **The interactive REPL's** (`tengu_idle_compact_fired`):
  - **Only on a one-hour TTL:** the last request must have used `ttl: "1h"`.
  - **Timing:** it fires at 90% of the cache's life,
    `lastActivityAt + 0.9 × (expiresAt − lastActivityAt)`, which is 54
    minutes for a one-hour cache.
  - **Floor:** `max(100k, CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS ?? 200k)`
    context tokens.
  - **It refuses when:**
    - the cache is no longer warm, or the timer runs more than 60 s late;
    - the prefix changed;
    - a newer request went out;
    - the account is near its usage limit;
    - the session is busy, or held by another process;
    - the transcript has moved on;
    - the user was present recently.
  - **The note it leaves:** "Compacted while idle, before the prompt cache
    expired".
- **The headless one** (`SleepCompactor`, `tengu_sleep_compact_fired`) is
  the one in the stream-json loop the SDK drives. It enqueues `/compact`
  as a meta prompt:
  - **Timing:** at 90% of the TTL, as above.
  - **Floor:** `max(60k, CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS ?? 150k)`.
  - **When it is wired in:** only when `CLAUDE_CODE_SLEEP_COMPACT` is set
    *and* `isSleepCompactionHost()` holds, which needs `CLAUDE_CODE_REMOTE`.
    That makes it a feature of Claude Code's cloud sessions.
  - **Neither the SDK nor the ACP adapter sets these variables.**

**Result.** Under cookie's ACP agent, Claude Code never compacts on its own
while idle. Setting the two variables ourselves would also switch on
`CLAUDE_CODE_REMOTE`'s other behaviour, so we do not. Per D6, the Claude
Code preset declares `cache_ttl = "1h"`, and cookie drives `/compact`
before expiry (section 4.4).

**Their rule, for comparison:**

| | Claude Code | This spec |
|---|---|---|
| When it fires | 90% of the TTL | `min(5m, ttl / 5)` before expiry, which is 91.7% of a one-hour TTL |
| Which TTLs | One hour only | Any TTL |
| Floor | 150k to 200k tokens | `min_tokens`, default 40k |
| When it gives up | The timer is more than 60 s late, or the cache has lapsed | Section 4.2 |

### 11.2 Codex and ChatGPT sign-ins (2026-11-02)

**What Codex sends.** Codex 0.165.0, the build the ACP adapter runs, sends
a `prompt_cache_key` and nothing else about caching. It sets no
`prompt_cache_retention` and no `prompt_cache_options.ttl`; those names
appear in its binary only inside a bundled docs skill.

**What OpenAI says.** On 2026-10-20 Tibo Sottiaux, who leads Codex, wrote
on X that "for logged in users across Codex & Work we keep cache forever".
He added that resuming an old conversation is "guaranteed to hit the
cache". He posted it on the same day the Responses API gained its `12h`
lifetime. OpenAI's prompt caching guide does not mention it, and still
says "Cache entries are not stored indefinitely". One public report
describes cache misses after an overnight pause.

**Why there was no live measurement.** Codex's turns open several
connections and seem to warm the cache before the real request, so the
turn usage `codex exec` reports cannot show a miss. A direct call to the
backend with Codex's sign-in token was not made.

**What cookie does:**

- **The Codex preset** sets `cache_ttl = "30m"`, to be safe *(user
  decision of 2026-11-02)*. Cookie compacts an idle Codex session about 24
  minutes after its last turn: the slack is capped at a fifth of 30
  minutes, so 6 minutes before expiry. It uses the `compact` command that
  codex-acp 2.5.0 advertises.
- **The `chatgpt` provider**, which uses the same backend and sign-in,
  assumes `30m`, to be safe *(user decision of 2026-11-02)*. The announced
  "forever" is not relied on until it is documented. A model override's
  `cache_ttl` replaces it, `0s` for no expiry.


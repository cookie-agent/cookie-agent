# Webhooks

Status: **approved** (2026-11-12); phases 1 and 2 implemented (client protocol
1.95.0 and 1.96.0), the shared client layer, `cookie webhook`, and the user
guide ([Webhooks](../sessions/webhooks.md)) landed, the web and TUI views in
progress. The user's decisions are in
[section 2](#2-decisions-2026-11-12). Builds on [cron jobs](cron.md) (targets,
templates, delivery, history), [share links](share-links.md) (the public
listener and its port), [deferred tools in cookie](deferred-tools-cookie.md)
and [session tool groups](session-tool-groups.md) (the tool group).
It also amends cron: new-session cron jobs get thread keys and thread
instructions ([section 8](#8-threads-for-cron-jobs)).

## 1. Goal and scope

Any service that can send an HTTP request can wake an agent. A **webhook**
is a URL the daemon serves. Each request it accepts becomes a message in
an existing session, or starts a new one. Typical uses:

- GitHub: one session per issue or pull request, which reads each new
  event on it;
- CI: a failed build wakes the session that is working on the branch;
- monitoring alerts, form submissions, home automation, a phone shortcut.

**Out of scope:** answering the sender with the agent's reply (the request
is acknowledged as soon as it is stored); outgoing webhooks and
notifications; placeholders in the prompt (`{{body.issue.title}}`); handshakes
that need a computed reply, such as Slack's `url_verification` or Meta's
`hub.challenge`; Stripe's and Slack's signature schemes beyond what the
generic HMAC check covers ([section 6](#6-authenticating-a-request)).

## 2. Decisions (2026-11-12)

1. **Webhooks are their own feature**, not a kind of cron job: their own
   store, RPCs, tools, and views. Internally they share cron's target,
   template, and delivery code.
2. **Thread keys.** A new-session webhook may route by a key taken from
   each request (a JSON field or a header). The first event for a key
   starts a session, and later events with that key go into it.
3. **Agents may manage webhooks** through the `webhook` permission action.
   The tools form the `webhooks` tool group of the `global` runtime,
   deferred by default.
4. **Signature checks:** GitHub, Standard Webhooks, a generic HMAC-SHA256,
   and a bearer token. The secret URL alone also works.
5. **`[share] url` and `[share] listen` become `[public] url` and
   `[public] listen`.** The public port serves share links and webhooks.
   The user's `config.toml` is edited once this lands.
6. **Every client:** a web view, a TUI `/webhooks` panel, and
   `cookie webhook`.
7. **The URL token is 64 bits** (11 base64url characters), not 128: an
   online guesser cannot reach 2^63 tries, and a signature check makes the
   token's secrecy irrelevant.
8. **Cron jobs get thread keys too** (asked 2026-11-12): a new-session job
   may name a thread key made from the fire's local time, so its fires go
   into one session per day, week, or month, or into a single session.
   Webhooks and cron share one implementation of threads.
9. **Thread instructions:** a `thread_prompt` sent as a plain user
   message, unwrapped (user, 2026-11-12), at the start of every session a
   webhook or job creates, and again after each compaction of that session,
   beside the per-event `prompt`, which stays inside the event's block
   ([section 7.5](#75-thread-instructions)).

## 3. Vocabulary

| Term | Meaning |
|---|---|
| **Webhook** | A URL token, an optional prompt, a target, and its checks and limits. |
| **Request** | One HTTP request to a webhook's URL. |
| **Event** | A request that passed the checks and the filter and was stored. |
| **Delivery** | An event's message reaching a session. |
| **Thread** | The session a thread key maps to. |
| **Thread instructions** | Standing instructions a new session gets once, and again after each compaction. |

## 4. The webhook

```json
{
  "id": "w7k2m9qa",
  "name": "GitHub issues",
  "url_token": "Xq3vT9aLk2M",
  "thread_prompt": "You look after one GitHub issue. Keep its labels right, and when it closes, write a one-paragraph summary.",
  "prompt": "A new event on the issue: act on it if it needs anything.",
  "target": {
    "new": { "agent": "builder", "project": "cookie-agent", "...": "..." },
    "thread_key": { "json": "/issue/number" }
  },
  "delivery": "queue",
  "verify": { "github": { "secret": "set" } },
  "filter": [
    { "header": "X-GitHub-Event", "one_of": ["issues", "issue_comment"] },
    { "json": "/action", "one_of": ["opened", "created"] }
  ],
  "headers": ["X-GitHub-Event"],
  "max_per_hour": 60,
  "max_concurrent": 2,
  "permission_mode": "auto_approve_n",
  "max_consecutive_failures": null,
  "enabled": true,
  "created_by": "user",
  "created_at": "2026-11-12T18:00:00Z"
}
```

- **`id`:** eight lowercase letters and digits, as a cron job's. It names
  the webhook everywhere and never changes.
- **`url_token`:** 64 random bits, base64url, in the URL only. Rotating it
  changes the URL and keeps the webhook ([section 10](#10-client-protocol)).
- **`name`:** optional, at most 80 characters, as cron's.
- **`prompt`:** optional, at most 32 KiB: instructions sent with every
  event. Without one, the model gets only the event.
- **`thread_prompt`:** optional, at most 32 KiB, `new` targets only:
  instructions sent once at the start of each session the webhook creates
  and again after each of its compactions
  ([section 7.5](#75-thread-instructions)).
- **`target`:** as cron's (`{session}`, `{subagent}`, or `{new: <template>}`,
  the template of cron section 3.1), and for `new` an optional
  `thread_key` ([section 7.4](#74-thread-keys)).
- **`delivery`:** `queue` (default) or `steer`. For a `new` target it
  applies to events that go into an existing thread.
- **`verify`:** optional, [section 6](#6-authenticating-a-request). Secrets
  are never sent back; a read shows `"set"`.
- **`filter`:** optional conditions, all of which must hold
  ([section 5.2](#52-the-filter)).
- **`headers`:** extra request headers the model sees
  ([section 7.1](#71-the-message)).
- **`max_per_hour`:** events accepted per rolling hour. 0 or less means no
  limit. Unset, `[webhooks] max_per_hour`.
- **`max_concurrent`:** `new` targets only: how many of the webhook's
  sessions may run at once. 0 or less means no limit. Unset, 1.
- **`permission_mode`:** `new` targets only, default `auto_approve_n`, set
  only by the user, as cron's.
- **`max_consecutive_failures`:** as cron's. 0 or less never disables.
- **`enabled`**, with a **`disabled_reason`** when off: `failed <n> times
  in a row` or `disabled by <user or agent>`.
- **`created_by`:** `user`, or `{"agent": {agent, session}}`.

### 4.1 The URL

```text
POST <public base>/hooks/<url_token>
```

The public base is the share base, renamed: `[public] url`, else
`[server] page_url`, else the address the client reached the daemon at.
Clients warn about a loopback, private, or `.ts.net` base, as the share
dialog does. A tool result gives the full URL when the daemon knows a base,
and otherwise the path with a note that it needs the public address.

## 5. Receiving a request

The daemon serves `/hooks/{token}` on the main listener and on the public
port. The route takes no daemon token, page ticket, or `Origin` check.

### 5.1 The steps

In order. The first that fails decides the answer:

| Step | Failure | Answer |
|---|---|---|
| The method is `POST` | other methods | `405` |
| Webhooks are on, the token names a webhook, and it is enabled | otherwise | `404`, the same body for every case |
| The body is at most `[webhooks] max_body_bytes` (1 MiB) | read stops at the limit | `413` |
| The `verify` check passes | bad or missing signature, stale timestamp | `401` |
| The delivery ID is new ([section 5.3](#53-duplicates)) | seen before | `200`, nothing done |
| The filter matches | no match | `204` |
| Under `max_per_hour` | over | `429` with `Retry-After` |
| The backlog has room ([section 7.3](#73-new-session-targets)) | full | `503` with `Retry-After` |
| The event is written durably | storage error | `500` |

Then the daemon answers **`202`** with `{"event": "<event id>"}` and
delivers the event afterwards. A sender's timeout never depends on a model.

Answers carry `Cache-Control: no-store` and no body beyond a short JSON
object. A `404`, `401`, filtered, or duplicate request is counted on the
webhook (`rejected`, `filtered`, `duplicate`, with the last time each
happened) and not kept in its history, so noise from the internet cannot
push real events out.

### 5.2 The filter

Each condition names one value and the strings it may be:

```json
{ "header": "X-GitHub-Event", "one_of": ["issues"] }
{ "json": "/pull_request/draft", "one_of": ["false"] }
```

- `header` matches the header case-insensitively by name; `json` is a JSON
  Pointer (RFC 6901) into a JSON or form body ([section 7.1](#71-the-message)).
- A JSON string compares as itself, a number, boolean, or null as its JSON
  text. An object, array, or missing value matches nothing.
- `one_of` holds 1 to 64 strings. All conditions must hold.

### 5.3 Duplicates

Senders retry. The delivery ID is the first of: the check's own ID header
(`X-GitHub-Delivery` for `github`, `webhook-id` for `standard`), then
`Idempotency-Key`. A request with none is never a duplicate. The webhook
remembers its last 1,024 delivery IDs. The ID is also the producer
idempotency key, so a retry that races a delivery still lands once.

## 6. Authenticating a request

The URL token always applies. `verify` adds one check, each comparing in
constant time:

| `verify` | The sender signs | Checked |
|---|---|---|
| `{github: {secret}}` | `X-Hub-Signature-256: sha256=<hex HMAC-SHA256 of the body>` | the HMAC |
| `{standard: {secret}}` | [Standard Webhooks](https://www.standardwebhooks.com/): `webhook-id`, `webhook-timestamp`, `webhook-signature` (`v1,<base64>` over `id.timestamp.body`), secret `whsec_<base64>` | the HMAC, any of the listed signatures; timestamp within 5 minutes |
| `{hmac_sha256: {secret, header, encoding, prefix?}}` | `<header>: <prefix><HMAC-SHA256 of the body>` in `hex` or `base64` | the HMAC |
| `{bearer: {token}}` | `Authorization: Bearer <token>` | the token |

- **Secrets** are generated by the daemon when the create call gives none
  (32 random bytes; for `standard`, in the `whsec_` form). The creator gets
  the secret once in the create result. Afterwards only the user can read
  it (`webhooks.secret`) or replace it (`webhooks.rotate`).
- Secrets live in `webhooks.json`, which is written with mode `0600`. They
  are never logged or put in a model's context.

## 7. Delivering an event

### 7.1 The message

The model sees the event marked as coming from a webhook, never as the
user, and the request as untrusted data:

```text
<webhook_event webhook="w7k2m9qa" name="GitHub issues" event="e3j8x0pq" received_at="2026-11-12T18:04:11+01:00" delivery="7c1e…" thread="1234">
<instructions>
A new event on the issue: act on it if it needs anything.
</instructions>
<request method="POST" content_type="application/json" user_agent="GitHub-Hookshot/abc">
X-GitHub-Event: issues
</request>
<body bytes="183204" truncated="true" artifact="artifact://sha256/…">
{ …the first 16 KiB, pretty-printed… }
</body>
</webhook_event>
```

- **Instructions** are the webhook's `prompt`, left out when there is
  none. The `thread_prompt` is never in the block (section 7.5).
- **Headers** shown are `Content-Type`, `User-Agent`, the webhook's
  `headers`, and any header named `X-<anything>-Event`. `Authorization`,
  `Cookie`, and every signature header are never shown, even when listed.
- **The body:** JSON is pretty-printed. A form body
  (`application/x-www-form-urlencoded`) becomes a JSON object of its
  fields; a `payload` field holding JSON, as GitHub's form mode sends, is
  parsed. Other text is shown as it is. Binary content is not shown inline.
- **Large bodies:** a body over `[webhooks] inline_body_bytes` (16 KiB) is
  stored in the target session's artifact store. The block shows the start,
  cut at a line or UTF-8 boundary, and the artifact, which the model reads
  with `read_artifact`. Binary bodies are always stored this way.
- An event is not user input. It never answers an approval and never counts
  as consent. Tool descriptions and the marking say its content is
  external and may try to instruct the agent.

### 7.2 Existing-session targets

- Every event is delivered, through the producer path cron fires use, with
  the owner `External { source: "webhook:<id>", name: "event" }` and the
  delivery ID (or the event ID) as the idempotency key. Unlike cron, an
  event waiting to be delivered never causes the next to be skipped: each
  is its own message.
  `queue` and `steer` behave as cron section 4.2 describes.
- A paged-out session is reloaded by its event. A webhook never keeps a
  session resident, never keeps a parent waiting, and never holds a turn's
  settle push back: it may never fire, unlike a cron job with a next fire.

### 7.3 New-session targets

- An event that needs a new session creates a root session from the
  template, as a cron fire does (cron section 4.3), with the title
  `<name> · <thread key, or the local time>` and
  `SessionMeta::webhook = {webhook_id, event_id, thread_key?}`.
- **`max_concurrent`.** While that many of the webhook's sessions are
  running, an event that needs a new session waits in the webhook's
  **backlog**, oldest first, at most `[webhooks] backlog` events (100); a
  full backlog answers `503`. The backlog is written to disk and survives
  restarts.

### 7.4 Thread keys

`thread_key` is `{json: "<pointer>"}` or `{header: "<name>"}`.

- The key is the value as the filter reads it ([section 5.2](#52-the-filter)),
  at most 200 characters. An event without one gets a session of its own,
  as with no thread key.
- The webhook keeps a map from key to session. An event whose key has a
  session goes into it with the webhook's `delivery`, whatever
  `max_concurrent` says, since it starts no session. An event whose key has
  none starts one and records it.
- A thread whose session is deleted leaves the map; the key's next event
  starts a new session. The map is kept with the webhook and goes with it.
- Threads are one piece of code shared with cron ([section 8](#8-threads-for-cron-jobs)):
  a map from key to session, and "deliver into the key's session, or create
  one from the template and record it". Only where the key comes from
  differs.

### 7.5 Thread instructions

`thread_prompt` is a **plain user message**, with no wrapping, as if the
creator had typed it into the new session. The event follows it as its own
`<webhook_event>` message, and both are admitted into the same run.

A thread lives long, so its compactions fold the first message into a
summary, and instructions sent only there fade. `thread_prompt` is sent:

- **as the first message of every session** the webhook creates, thread
  key or not;
- **again before the first event after each compaction** of that session
  (a checkpoint of cookie's own, internal or native), the same text
  unchanged. The session records that it is due when the checkpoint
  commits, so a restart in between does not lose it;
- for an ACP agent's session, only at the start: cookie does not see the
  agent's own compactions.

The model sees only the text. The event's origin records where it came
from (`webhook <id>`, `thread_prompt`), so clients mark it "from webhook
GitHub issues" in place of the user's name, as share guests' labels are
shown: for people, never the model. It is admitted as new input, never as
the answer to a waiting approval or question.

**Who wrote it.** A user message is trusted as the user's. When an agent
created the webhook, its `thread_prompt` is text the user approved with
the creation (`webhook: ask`), as a delegated task's prompt is the
delegating agent's text in its child. A rule that sets `webhook: allow`
gives an agent that power without asking, which the permissions page says.

Without a thread key, every event starts a session, so the user message and
the event go together every time, and the split only names what each is
for. An existing-session
target has no `thread_prompt`; its session already has whatever
instructions it was given. Instructions that must hold in every request
whatever happens belong in the template's agent: an agent document's
system prompt is never compacted.

### 7.6 History, failures, and limits

- **History:** the last `[webhooks] history` events (50), each with when it
  arrived, its delivery ID, thread key, size, outcome (`delivered`,
  `waiting`, `failed` with the error), the session and run it reached, and
  how that run ended. The body is kept with each history entry for the
  clients' event view, and dropped with it.
- **Failures** count as for cron: a run an event started or joined ending
  `failed`, or an event that cannot be delivered (agent missing, runtime
  offline, creation failing). A completed run resets the count. At
  `max_consecutive_failures` (`[webhooks] max_consecutive_failures`, 3) the
  webhook is disabled and its URL answers `404` until it is enabled again.
- **Test events** (`webhooks.test`) skip the checks, filter, and limits, and
  are recorded as `test`. They count toward neither the rate nor failures.

## 8. Threads for cron jobs

This amends [cron](cron.md) sections 3, 4.3, and 8.

- **`thread_key`** on a `new` target: a time format, as jiff's `strftime`
  takes it, applied to the fire's scheduled time in the host's local time.
  At most 64 characters.

  | `thread_key` | Threads |
  |---|---|
  | `"%Y-%m-%d"` | one session per day: an hourly job adds to today's |
  | `"%G-W%V"` | one session per ISO week |
  | `"%Y-%m"` | one session per month |
  | `"main"` (no `%`) | one session for every fire, made again if deleted |

- **`thread_prompt`** on a `new` target, as for webhooks (section 7.5): a
  plain user message before the fire's `<scheduled_job>`, whose `prompt` is
  the per-fire text.
- **A fire** whose key has a session goes into it as an in-session fire
  (cron section 4.2) with the job's `delivery`, which a `new` target with a
  thread key now takes (default `queue`). A fire whose key has none creates
  the session as cron section 4.3 says, titled `<name> · <key>`, and records
  it. A run now (`crons.run`, `cron_run`'s in-session path) uses the key of
  the moment it runs, so it lands in the current thread.
- **Skips:** with a thread key, a fire is skipped only while the job's
  previous fire is still undelivered (the in-session rule). "Previous run
  still running" no longer applies, since queuing into a busy thread is
  what a thread is for. Without a thread key nothing changes.
- **`SessionMeta::cron`** gains `thread_key`. The job keeps the map in
  `crons.json`; deleting a thread's session drops it from the map, and
  deleting the job keeps its sessions.
- **Waiting and residency** are unchanged: a thread session is an ordinary
  root session, which a fire reloads.
- **Clients and tools:** the cron form, `cookie cron create|edit
  --thread-key --thread-prompt`, and `cron_create`/`cron_update` take both
  fields. The form previews which thread the next fire goes to ("next fire
  → the session for 2026-11-13").
- **Protocol:** `CronTemplate`'s target gains the optional `thread_key` and
  `thread_prompt`, and `delivery` is allowed on a `new` target with a key.
  Client protocol minor, in the same bump as webhooks.

## 9. Agents

### 9.1 Permissions

- **The action:** `webhook`; its resource is the target kind (`session`,
  `subagent`, `new`), as for `cron`. One action covers listing, creating,
  changing, and deleting.
- **Defaults:** root runs `webhook: ask`, since a webhook opens a public
  endpoint; delegated and internal runs none (the tools are hidden).
- **Targets an agent may name:** its own session, sessions below it, or
  `new`, as for cron. The user can target any session.
- **Agents cannot set** `permission_mode` or a model pin, and cannot read or
  rotate a secret after creation.

### 9.2 The `webhooks` tool group

The provider `builtin.webhooks` of the `global` runtime. Its group,
`webhooks` ("Webhooks: URLs that turn incoming HTTP requests into messages
in this session, a subagent's, or new sessions"), joins
`DEFERRED_BUILTIN_GROUPS`, so it is deferred by default and can be set on,
deferred, or off per session like any group.

| Tool | Arguments | Result |
|---|---|---|
| `webhook_create` | `name?`, `prompt?`, `thread_prompt?`, `target?` (`"self"`, a session handle, or `{new: {agent?, permission_rules?, capabilities?, project?}, thread_key?}`), `delivery?`, `verify?` (`{kind, secret?}`; for `hmac_sha256` also `header`, `encoding`, `prefix?`), `filter?`, `headers?`, `max_per_hour?`, `max_concurrent?` | The webhook, its URL, and the secret once |
| `webhook_list` | `target?` (a kind) | Each webhook the rules let it see: id, name, URL, target, enabled, counts, the last event's outcome |
| `webhook_update` | `id`, any field of `webhook_create`, `enabled?` | The webhook |
| `webhook_delete` | `id` | Confirmation |

`list_agents` is offered whenever `webhook_create` or `webhook_update` is,
as it is for the cron tools.

## 10. Client protocol

- **RPCs:**
  - `webhooks.list`: every webhook with its state, counts, and history
    (bodies left out), and the public base;
  - `webhooks.create`, `webhooks.update` (any field, `permission_mode`
    included), `webhooks.delete`;
  - `webhooks.rotate {id, what: "url" | "secret"}`: a new URL token or
    signing secret, returned once;
  - `webhooks.secret {id}`: the signing secret;
  - `webhooks.event {id, event}`: one history entry with its body;
  - `webhooks.test {id, body?, content_type?, headers?}`: delivers a test
    event;
  - `webhooks.changed`: notification, empty, coalesced like
    `crons.changed`.
- **Additions:** `SessionMeta::webhook`, folded from an optional `webhook`
  member of `session_created` (an additive member, as `cron` was);
  `PermissionAction::WEBHOOK`; `webhook_*` error codes;
  `ServerHello`'s public base keeps its meaning under the new config name.
- Client protocol minor, plugin protocol minor (`SessionMeta`). The runtime
  protocol is unchanged. The history format is unchanged, since
  `session_created` only gains a member.
- The guest socket of conversation shares (`/s/<id>/ws`) refuses every `webhooks.*`
  method, as its fail-closed list does for anything unlisted.

## 11. Configuration

```toml
[public]
url = "https://hooks.example.com"  # was [share] url
listen = "127.0.0.1:7420"          # was [share] listen; serves /s/ and /hooks/

[share]
enabled = true                     # unchanged

[webhooks]
enabled = true                 # false: every URL answers 404, webhooks are kept
max_body_bytes = 1048576
inline_body_bytes = 16384
max_per_hour = 60              # per webhook; 0 or less, no limit
max_consecutive_failures = 3   # 0 or less, never disable
backlog = 100
history = 50
```

`[share] url` and `[share] listen` are removed and refused as unknown keys,
as strict configuration requires. The configuration reference and the
sharing guide move them to `[public]`.

## 12. Storage

- `<data_root>/webhooks.json`, one versioned file of the webhooks, their
  state (counts, thread maps, recent delivery IDs, history metadata),
  written atomically under the memory lock as `crons.json` is, mode `0600`.
- `<data_root>/webhook-events/<id>/<event id>`: the bodies of events in the
  backlog or the history, deleted with their entry and with the webhook.
- **Exactly once:** an event is written (body first, then its entry) before
  the `202`. Delivery uses the producer idempotency key, so a restart
  between writing and delivering delivers it once. At start, events that
  are still pending are delivered or wait in the backlog again.

## 13. Deleting

- Deleting a session deletes the webhooks that target it or a session in
  its tree, and drops it from every thread map.
- Deleting a project deletes the new-session webhooks whose template names
  it.
- Deleting a webhook keeps its sessions, which stay marked with its ID.

## 14. Clients

- **Web:** a **Webhooks** view behind its own rail icon, in the style of
  Scheduled prompts. Each webhook row expands to its URL with a copy
  button and the public-address warning, its target, checks, filter,
  counts, and history; an event opens to its headers and body. Actions:
  create, edit, enable or disable, send a test event, rotate the URL or
  secret, reveal the secret, delete. The session ⋮ menu gains "Add a
  webhook" for the open session. Webhook sessions show a mark in the
  session list linking to their webhook.
- **TUI:** `/webhooks` opens a panel like `/cron`'s, with the same actions
  as buttons with hovers and a `key: value` form edited in `$EDITOR`. The
  palette's `/webhooks new` targets the session on screen. Webhook sessions
  get their own mark in the picker.
- **CLI:** `cookie webhook list | show <id> | create | edit <id> | rm <id> |
  enable <id> | disable <id> | rotate <id> --url|--secret | secret <id> |
  test <id> [--body <file>]`, with the token inline for a remote daemon.

## 15. Security notes

- The endpoint is public by design. The token gives about 2^63 guesses on
  average, the `404` is the same for every miss, and no answer says whether
  a token exists, so it cannot be enumerated.
- Request bodies are untrusted and go to an agent that may run tools in
  `auto_approve_n`. The marking and the tool description say so; a
  signature check and a narrow filter are recommended in the guide for any
  webhook whose URL leaves the user's hands, and its agent's permission
  rules decide what an event can make it do.
- Secrets and tokens never reach the model's context, logs, or events.
- The body limit is enforced while reading, before anything is stored.

## 16. Tests

- **Receiving:** each step of section 5.1 and its answer; the uniform
  `404` for unknown, disabled, and switched-off; the body limit while
  streaming; duplicates by each ID header; the filter's types and
  pointers; the rate limit's window; a full backlog.
- **Checks:** each `verify` kind against known vectors (GitHub's documented
  example, the Standard Webhooks test vectors), stale timestamps, multiple
  signatures, wrong encodings.
- **Delivery:** `queue` and `steer` into idle and busy sessions, several
  events queued at once all delivered; the `<webhook_event>` marking with
  instructions, filtered headers, form and JSON bodies, and a large body
  as an artifact; a paged-out target reloaded.
- **New sessions and threads:** `max_concurrent` and the backlog across a
  restart; thread keys routing to the same session, a missing key, a
  deleted thread session.
- **Thread instructions:** a plain user message with a webhook or cron
  origin, never answering an approval; sent as a new session's first message, again
  with the first event after a checkpoint (across a restart between the
  two), never after an ACP agent's start, refused on an existing-session
  target.
- **Cron threads:** each time format with explicit fire times (a day
  boundary, an ISO week across a new year, a fall-back fold), a constant
  key, a deleted thread session, the in-session skip rule, a run now landing
  in the current thread.
- **Exactly once** across a restart between the write and the delivery.
- **Failures and disabling;** test events counting for nothing.
- **Permissions:** the `ask` default for roots, hidden for delegated runs,
  targets outside the subtree refused, no mode or secret access for agents.
- **Tools:** the group deferred by default, `list_agents` alongside.
- **Config:** `[public]`, the removed `[share]` keys refused, `[webhooks]`
  boundaries; the public port serving `/hooks/` and nothing new besides.
- **Clients:** the panels, the session mark, `cookie webhook`.

## 17. Phases

1. **Engine, protocol, and HTTP:** threads and thread instructions as one
   shared piece, then cron's thread keys on it; the webhook store, the route
   on both listeners, checks, filter, duplicates, limits, delivery, backlog,
   history and failures, deletion, the RPCs and notification, `[public]`
   and `[webhooks]`.
2. **Agents:** the `webhook` action and defaults, the `webhooks` tool group,
   `list_agents`.
3. **Clients:** web, TUI, CLI, and the cron form's thread fields.
4. **Docs:** a webhooks guide (with GitHub and Cloudflare tunnel setups),
   the configuration, permissions, tools, protocol, and errors references,
   and the sharing guide's move to `[public]`. Then the user's
   `config.toml` moves its `[share]` keys to `[public]`.

## 18. Alternatives considered

- **A cron job with a webhook trigger:** less new surface, but schedules,
  `max_runs`, missed fires, and skips do not fit events, and users would
  look for webhooks under Scheduled prompts (decision 1).
- **A plugin with public paths** ([plugin web pages](plugin-web-ui.md))
  can already receive requests, but every use would need its own plugin,
  and agents could not create one.
- **Answering the sender with the agent's reply:** senders time out in
  seconds, and runs take minutes.

## 19. Implementation notes

### 19.1 Phase 1A: threads, thread prompts, and cron thread keys (2026-11-13)

Landed: the shared threads piece, thread prompts, and thread keys for
new-session cron jobs (sections 7.4, 7.5, and 8), with `cookie cron
--thread-key --thread-prompt` and the two fields in the TUI's form. Client
protocol 1.95.0, plugin protocol 1.47.0; the runtime protocol and the history
format are unchanged. The webhook store, route, and tools are later phases.

Where it is:

- **Wire:** `CronTemplate.thread_key` and `thread_prompt`, `CronJob.threads`
  (`CronThread`), `CronFire.thread_key`, and `SessionCron.thread_key` in
  `crates/protocol/src/cron.rs`; the thread prompt's origin in
  `crates/protocol/src/threads.rs` (`ThreadOwnerRef`, `thread_prompt_origin`,
  `thread_prompt_owner`).
- **Store side:** `crates/engine/src/threads.rs`: `ThreadOwner` (the owner a
  producer message or a thread prompt belongs to), `ThreadMap` (key →
  session with `last_used`, kept by each owner in its own store), and the
  checks of a time key format and a thread prompt. `crons.json` keeps a
  job's map as `threads` and each fire's `thread_key`.
- **Engine side:** `crates/engine/src/runtime/threads.rs`:
  `Engine::thread_target` (lock the key, use the mapped session while it is a
  live root, otherwise create and record one) and
  `submit_due_thread_prompts_direct`, called where producer messages are
  admitted.
- **Cron:** `runtime/cron/fire.rs` (`deliver_to_thread`, the key computed
  at the claim from the scheduled time, the in-session skip rule, thread
  titles, restart reconciliation), `runtime/cron/api.rs` (checks, delivery
  on a keyed `new` target, maps cleared when the format or kind changes,
  deleted sessions leaving maps), `runtime/cron/run.rs`.

For phase 1C, webhooks plug in by adding a `ThreadOwner::Webhook` variant
(and `ThreadOwnerRef::Webhook` with an `engine:webhook-<id>` origin), the
owner's arms in `ThreadOwner::of_producer` and `Engine::thread_prompt_of`,
a `ThreadMap` in the webhook store, and a call of `thread_target` whose
`lookup`, `create`, and `record` reach that store.

Decisions taken where the spec left room, and deviations:

- **How the thread prompt goes in.** It is not sent as a separate input. When
  a run admits a producer message (a fire's `<scheduled_job>`, later a
  webhook event) of an owner whose thread prompt is due in that session, the
  engine first appends `user_input_submitted` and `user_input_applied` with
  the thread prompt under the owner's origin, then the admission. So the
  thread prompt and the fire are always in the same run and request, in that
  order, whether the fire started the run (`queue` into an idle session, a
  new session) or joined a running one (`steer`), and a `queue` fire into a
  busy thread takes it with the run it starts after the turn. The thread
  prompt never answers an approval: it is not client input.
- **When it is due** is read from the session, not stored: the owner created
  the session (its `SessionMeta::cron` names the job) and has a thread
  prompt, and the session's model context holds no thread prompt of that
  owner from the latest checkpoint's window start on (the summary's kept
  range, or what follows a provider window). An ACP agent's run looks only
  at the session's first inputs, so it gets the prompt once. A restart
  between a checkpoint and the next fire needs nothing extra.
- **The origin** is `engine:cron-<job id>`, an engine origin of the existing
  `EventOrigin` grammar, so the wire and the history needed no new shape;
  `thread_prompt_owner` reads it back for clients.
- **The map** is capped at 1,024 threads, forgetting the least recently used
  (the spec set no bound; a daily key grows one entry a day). A forgotten
  thread's session stays.
- **Deletion and edits.** A deleted session leaves every job's map. A change
  of the target's kind or of its thread key format clears the map, since
  the old keys name no thread of the new format.
- **`cron_run` of a job with a thread key** fires into the thread of the
  moment, as `crons.run` does, rather than running as the caller's subagent
  (cron spec section 8.2 still holds for jobs without a key). The child of a
  job without a key has no producer message to lead with the thread
  prompt, so its task text starts with the thread prompt instead.
- **The key** comes from the fire's scheduled time (a run now's own time),
  computed when the fire is claimed and kept in its history entry, so a
  delivery after a restart uses the same thread.
- **Waiting and residency** are unchanged: a thread's session lists no
  `scheduled_job` wait, as the spec says.
- **Clients.** The TUI's `/cron` form has a `thread_key:` line and takes a
  thread prompt after a `--- thread prompt` line under the prompt;
  `client_state` (`CronDraft.thread_key`, `thread_prompt`, `thread_key_at`)
  shows the format, the next fire's thread, and the thread count in a job's
  details. The web's form only keeps the fields it does not show; its
  controls and the clients' mark for a thread prompt's origin are left to
  the clients phase.

### 19.2 Phase 1B: configuration and the request side (2026-11-12)

Landed: `[public]` and `[webhooks]`, and the request side as pure functions,
not yet wired to a route or a store.

- **Configuration** (`crates/config/src/runtime.rs`): `PublicConfig` (`url`,
  `listen`, validated as `[share]`'s were), `ShareConfig` reduced to
  `enabled`, and `WebhooksConfig` with `per_hour_limit()` and
  `failure_limit()` reading 0 or less as none. `max_body_bytes` is
  1..=64 MiB, `inline_body_bytes` 1..=1 MiB (so a message stays within the
  producer body limit), `backlog` 1..=10000, `history` 1..=1000.
  `[share] url` and `listen` fail as unknown keys. The settings form gains
  the `[public]` fields under Transfers and sharing and a Webhooks section.
- **The public listener** is `DaemonListener::serve_public_on` with
  `public_router` (`crates/server/src/service/websocket.rs`), which serves
  `/s/` today; `/hooks/` joins it there.
- **The request side** lives in the engine, `cookie_agent_engine::webhooks`,
  since delivery is the engine's and the server already depends on it:
  - `request`: `WebhookRequest` (method, headers as received, body),
    `parse_body` → `ParsedBody` (`Empty`, `Json`, `Form`, `Text`,
    `Binary`), `ValueSource` (`Header`, `Json` pointer) with `validate`,
    `read_value`, `thread_key`, and `delivery_id` with `DeliveryHeader`;
  - `verify`: `Verifier` (`GitHub`, `Standard`, `HmacSha256`, `Bearer`)
    with `validate`, `verify(request, now)`, `delivery_header`, and
    `secret_headers`; `generate_secret(SecretKind)`;
  - `filter`: `Condition` (a `ValueSource` and `one_of`), `validate`,
    `matches`;
  - `render`: `shown_body` → `ShownBody` (with `wants_artifact`), then
    `render_event(&EventMessage)` with the artifact reference the caller
    stored;
  - `ids`: `new_webhook_id`, `is_webhook_id`, `new_url_token`,
    `is_url_token`.
- **Dependencies:** none new in the lock file. The engine now names
  `aws-lc-rs` (already the server's, for the page ticket's HMAC) for
  HMAC-SHA256 and constant-time comparison, and `form_urlencoded` (already
  in the tree through `url`).

Decisions taken where the spec left room:

- **Bodies.** A body labelled `application/json` or `+json`, or unlabelled
  and starting with `{` or `[`, is JSON when it parses; a form field sent
  several times is an array of strings; a body that is UTF-8 without NUL is
  text, anything else binary. The inline limit applies to the shown text
  (pretty-printed for JSON), not the bytes received; the cut falls at a
  line break in the last half of the limit, else at a character boundary.
- **Defusing.** Closing tags of the message's own elements
  (`webhook_event`, `instructions`, `request`, `body`) inside the request
  are written `<\/name`, so a request cannot end its block; nothing else
  is escaped, so HTML and JSON read as sent. Header values and attributes
  have control characters made spaces, and header values are cut at 1000
  characters.
- **Hidden headers.** Besides `Authorization`, `Cookie`, and the signature
  headers, `Proxy-Authorization`, `Set-Cookie`, and `X-Hub-Signature` are
  never shown, and a `hmac_sha256` check's own header is hidden; such a
  check may not name `Authorization`, `Cookie`, or `Content-Type`.
- **Delivery IDs** follow section 5.3 strictly: `X-GitHub-Delivery` counts
  only with the `github` check and `webhook-id` only with `standard`. An ID
  or a thread key that is empty, too long (200 characters), or holds a
  control character counts as none.
- **Filters** hold at most 32 conditions, each `one_of` string at most 512
  characters; values compare exactly, case included.
- **Secrets** are 32 random bytes: base64url for `github`, `hmac_sha256`,
  and `bearer`, `whsec_` and standard base64 for `standard`, whose secret
  must decode to 24 to 64 bytes as that specification says. A secret is 1
  to 1024 characters without control characters. A verifier's `Debug`
  never prints it.
- **Standard Webhooks** accepts any of several space-separated `v1,`
  signatures and skips other versions; a header with no `v1,` signature is
  malformed. Every failure answers `401` alike.

### 19.3 Phases 1C and 2: the store, the route, delivery, and the tools (2026-11-13)

Landed: everything sections 4 to 13 describe but the clients. Client
protocol 1.96.0, plugin protocol 1.48.0; the runtime protocol and history
format 3 are unchanged (`session_created` only gained `webhook`).

Where it is:

- **Wire types:** `crates/protocol/src/webhook.rs` (`Webhook`,
  `WebhookTarget`, `WebhookTemplate`, `WebhookValueSource`,
  `WebhookCondition`, `WebhookVerify`, `WebhookEvent`, `WebhookCounts`, the
  params and results, `SessionWebhook`), `WebhookId` and
  `ProducerSource::webhook` in `identity.rs`, `ThreadOwnerRef::Webhook` in
  `threads.rs`, `PermissionAction::WEBHOOK` in `agent.rs`, the `webhook_*`
  error codes in `errors.rs`.
- **Store:** `crates/engine/src/webhooks/store.rs` (`webhooks.json` and
  `webhook-events/`, both private).
- **Requests:** `crates/engine/src/runtime/webhook/api.rs`; **receiving:**
  `receive.rs` (`Engine::receive_webhook`, `webhook_receives`,
  `webhook_body_limit`); **delivery:** `deliver.rs` (the pump, the backlog,
  sessions, artifacts, run outcomes, start).
- **Tools:** `crates/engine/src/webhook_tools.rs` (`builtin.webhooks`, the
  `webhooks` group); the default in `webhook_default_rules`
  (`crates/engine_permissions`).
- **Daemon:** `crates/server/src/service/webhook_route.rs`, on both routers
  in `websocket.rs`; the RPCs in `routes.rs`, the notification in
  `runtime_notifications.rs`, the errors in `rpc.rs`, and the guest
  socket's refusals in `guest.rs`.

Decisions taken where the spec left room, and deviations:

- **`thread_key` lives in the template** (`target.new.thread_key`), beside
  `thread_prompt`, as a cron template's does, rather than beside `new` as
  section 4's example shows. A webhook's template is otherwise a cron
  template, stored as one (its time-format `thread_key` unused).
- **Information types are open.** `WebhookValueSource`, `WebhookCondition`,
  `WebhookTemplate`, `WebhookVerify`, and `WebhookHeader` appear in reads,
  so they take unknown members as the compatibility rules require; a source
  or condition that names neither or both of `header` and `json` is refused
  when saved.
- **`verify` is one shape** for commands and reads, `{ kind, secret?,
  header?, encoding?, prefix? }`; reads never carry `secret` (section 4's
  `"set"` is its absence). An update with a `verify` of the same kind and no
  secret keeps the secret and returns none; another kind, or a given
  secret, replaces it and returns it. `hmac_sha256` needs `header` and
  defaults `encoding` to `hex`.
- **The URL.** `Webhook.path` is `/hooks/<url_token>` and
  `webhooks.list` gives `public_base` (`[public] url`, else `[server]
  page_url`), which clients put before it. The route answers `405` (with
  `Allow: POST`) before anything else, then `404` for an unknown, disabled,
  or switched-off URL, or a path that is no URL token, then reads the body
  up to `max_body_bytes` (`413`). Every answer has `Cache-Control:
  no-store` and a short JSON body: `{"event": "<id>"}`, `{"duplicate":
  true}`, or `{"error": "<reason>"}`. `Retry-After` is the seconds until the
  oldest event of the hour leaves the window (`429`), or 60 (`503`).
- **Counts.** Requests turned away are counted in memory and written with
  the webhook's next change, so internet noise never writes the file; a
  crash loses them, as share link views. They are not in the history and
  send no `webhooks.changed`. Accepted events count when stored.
- **Races.** The duplicate and rate checks run again under the store's lock
  when the event is written; a request that lost the race answers `200` as a
  duplicate.
- **Events** have 8-character IDs, unique within their webhook. The history
  keeps every pending or waiting event and the last `[webhooks] history`
  finished ones. Each keeps the request's method and headers but the hidden
  and check headers, beside its body file, so the backlog and a restart can
  render it again.
- **The pump.** One task per webhook at a time delivers its open events in
  arrival order, one event per step, after an event is stored, when a run
  of one of its sessions ends, when it is updated (so enabling it delivers
  what waited), and at start. A disabled webhook's events stay pending
  until it is enabled. An event of a key that already has a live thread
  goes into it whatever `max_concurrent` says; an earlier waiting event of
  the same key holds later ones back, so a key's events keep their order;
  once one event needing a new session waits, later ones do too.
- **Running sessions** for `max_concurrent` are the webhook's delivered
  sessions that are busy, or that hold one of its events no run has taken
  yet. A thread session the user is chatting in counts too.
- **Exactly once.** Delivery uses the event's delivery ID, else its own ID,
  as the producer key, so a pending event whose message was accepted before
  a restart is not delivered twice. A pending new-session event at start
  first looks for a session that records it (`SessionMeta.webhook`) before
  making one.
- **Artifacts.** A body the inline limit cuts, or a binary one, is written to
  the target session's tree as an artifact under the artifact store's
  publication lock, held until the message naming it is accepted, so a
  revert cannot collect it in between.
- **Failures.** A delivery that fails, or a run it landed in that fails,
  counts; a completed run resets the count; test events count for nothing.
  At the limit the webhook is disabled as `failed`, and its URL answers
  `404`.
- **Reviewer labels.** A webhook's producer messages are labelled
  `[webhook event <id>]` for the approval reviewer, as cron's are.
- **Producers.** A `webhook:` source is the daemon's own, like `cron:`
  (`ProducerSource::is_daemon_owned`): no connection can claim it, its
  short registration survives reconciliation, and it never adds a wait or
  holds back a settle push.
- **Tools.** `webhook_create`, `webhook_list`, `webhook_update`, and
  `webhook_delete`; targets resolve through the cron tools' resolution (the
  same `"self"`, handles below the caller, and template defaults), with
  `thread_key` a `{json}` or `{header}` source. The result of
  `webhook_create` gives the URL (or the path and a note when the daemon
  knows no public base) and the secret; the approval card never shows a
  given secret. `list_agents`'s `cron` use also lists the agents a
  new-session webhook can run, and is offered with the webhook tools.
- **`[deferred_tools] always_load`** takes `webhooks`.

What the clients need: a Webhooks view and `/webhooks` panel over
`webhooks.list`, refetched on `webhooks.changed`, with each webhook's URL
(`public_base` + `path`, or the address the client used), the secret shown
once from `webhooks.create` and `webhooks.rotate` and on demand from
`webhooks.secret`, an event's request from `webhooks.event`, and a test
button (`webhooks.test`); the session mark from `SessionMeta.webhook`; the
label of a thread prompt by its origin (`thread_prompt_owner`, now with
`Webhook`); the cron form's thread fields; `cookie webhook`; and a webhooks
guide.

# Webhooks

A webhook is a URL the daemon serves. Any service that can send an HTTP
request to it, such as GitHub, a CI system, a monitoring alert, a form, or a
phone shortcut, wakes an agent: each request the webhook accepts becomes a
message in a session that already exists, or starts a new session. You
create webhooks with `cookie webhook`; agents create them with the webhook
tools. The daemon keeps them in `webhooks.json` in its data directory. The
[webhooks spec](../specs/webhooks.md) has the full design.

Webhooks need a daemon of client protocol 1.96 or later; `cookie webhook`
refuses an older one with a message.

## A webhook

A webhook has an ID (eight lowercase letters and digits, which never
changes), an optional name, a URL, an optional prompt, a target, and its
checks and limits:

```sh
cookie webhook create --name "GitHub issues" \
  --agent builder --project cookie-agent \
  --thread-key json:/issue/number \
  --thread-prompt "You look after one GitHub issue. Keep its labels right, and when it closes, write a one-paragraph summary." \
  --prompt "A new event on the issue: act on it if it needs anything." \
  --check github \
  --filter header:X-GitHub-Event=issues,issue_comment
```

`create` prints the URL and, with a check, the signing secret, once.

### The URL

```text
POST <public address>/hooks/<token>
```

The token is 64 random bits (11 characters), so the URL cannot be guessed,
and it is the only thing that admits a request when the webhook has no
check. `cookie webhook rotate <ID> url` gives the webhook a new token; the
old URL stops working at once.

The public address is `[public] url`, else `[server] page_url`, else the
address your client reached the daemon at. For a service on the internet
to reach the URL, the daemon needs a public address. Put the
[public port](../engine/public.md) behind a tunnel, which serves share links
and webhooks and nothing else, never the daemon's control plane or its web
page:

```toml
[public]
url = "https://hooks.example.com"
listen = "127.0.0.1:7420"
```

```yaml
# cloudflared config.yml
ingress:
  - hostname: hooks.example.com
    service: http://127.0.0.1:7420
  - service: http_status:404
```

or, with Tailscale, `tailscale funnel --bg 7420`. See
[Remote access](../daemon/remote-access.md#public-share-links). Clients
warn when a URL's address is loopback, private, plain `http`, or a
`.ts.net` name the daemon was not given, since such a URL works only where
the daemon's page can be reached.

### Targets

A webhook delivers into one of:

- **a session:** a root session, or a delegated one. Every event is
  delivered; one waiting to be delivered never causes the next to be
  dropped. `queue` (the default) starts the session's next run with it,
  after the running turn if there is one; `steer` puts it into the running
  turn.
- **new sessions:** the daemon creates a root session from the webhook's
  template, the agent, a model pin, the project, permission rules and
  attachments, for each event, or for each thread (below). Their permission
  mode is `auto_approve_n` unless you set another; only you can set it.
  Each session's title is the webhook's name and its thread key, or the time
  of the event, and the session list marks it as a webhook's.

`--max-concurrent` limits how many of a webhook's sessions run at once (1
unless set; 0 or less for no limit). An event that needs a new session while
that many run waits in the webhook's backlog, oldest first, written to disk,
so it survives a restart. A full backlog (`[webhooks] backlog`, 100) answers
`503`, so the sender retries later.

### Threads

A new-session webhook with a thread key gives each value of the key its own
session: one session per GitHub issue, per pull request, per customer. The
key is a JSON field of the body (`json:/issue/number`, a
[JSON Pointer](https://www.rfc-editor.org/rfc/rfc6901)) or a header
(`header:X-Thread-Id`), at most 200 characters.

- The first event with a key starts a session and records it.
- Later events with that key go into that session, with the webhook's
  `queue` or `steer`, whatever `--max-concurrent` says, since they start no
  session.
- An event without the key starts a session of its own.
- Deleting a thread's session forgets it; the key's next event starts a new
  one.

Cron jobs have threads too, keyed by the time of the fire: see
[Scheduling prompts](scheduling.md#threads).

### The prompt and the thread prompt

- **`--prompt`** is sent with every event, inside the event's message.
- **`--thread-prompt`** is for what a session is for. It is sent once as a
  plain user message, as if you had typed it, at the start of each session
  the webhook creates, and again before the first event after each of the
  session's compactions, so a long thread does not lose it. An ACP agent's
  session gets it at the start only. Clients show it marked as from the
  webhook. It never answers a waiting approval or question.

Instructions that must hold in every request whatever happens belong in the
agent's own document: its system prompt is never compacted.

### What the agent sees

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

- JSON is pretty-printed. A form body becomes a JSON object of its fields,
  and a `payload` field holding JSON, as GitHub's form mode sends, is parsed.
- A body over `[webhooks] inline_body_bytes` (16 KiB) is stored in the
  session's artifacts; the message shows its start and names the artifact,
  which the agent reads with `read_artifact`.
- The headers shown are `Content-Type`, `User-Agent`, any `X-…-Event`
  header, and the ones `--show-header` adds. `Authorization`, `Cookie`, and
  signature headers are never shown.
- The event is marked as coming from a webhook, never from you. It never
  answers an approval and never counts as consent.

## Receiving a request

The daemon answers in this order; the first step that fails decides:

| Step | When it fails | Answer |
|---|---|---|
| The method is `POST` | another method | `405` |
| Webhooks are on, and the token names an enabled webhook | otherwise | `404`, the same for every case |
| The body is at most `[webhooks] max_body_bytes` (1 MiB) | larger | `413` |
| The check passes | a bad or missing signature, a stale timestamp | `401` |
| The delivery ID is new | a retry of a request already taken | `200`, nothing done |
| The filter matches | no match | `204` |
| Under the hourly limit | over it | `429` with `Retry-After` |
| The backlog has room | full | `503` with `Retry-After` |

Then it stores the event and answers `202` with the event's ID at once; the
agent's run never holds the sender up.

### Checks

Without a check, the URL alone admits a request. A check makes the sender
prove it holds a shared secret, so a leaked URL is not enough:

| `--check` | The sender sends |
|---|---|
| `github` | `X-Hub-Signature-256: sha256=<hex HMAC-SHA256 of the body>`, as GitHub does |
| `standard` | [Standard Webhooks](https://www.standardwebhooks.com/) headers (`webhook-id`, `webhook-timestamp`, `webhook-signature`), as Svix and many services do; the timestamp must be within 5 minutes |
| `"hmac_sha256 header=<name> encoding=hex\|base64 prefix=<text>"` | `<name>: <prefix><HMAC-SHA256 of the body>`, for any other service that signs its body this way |
| `bearer` | `Authorization: Bearer <token>` |

The daemon makes the secret when you give none, and shows it once;
`cookie webhook secret <ID>` prints it again, and
`cookie webhook rotate <ID> secret` replaces it. Agents see a secret only
in the result of the call that created it.

### Filters

A filter keeps events the agent does not need from costing a run. Each
condition names a header or a JSON field and the values it may have; all
conditions must hold:

```sh
--filter header:X-GitHub-Event=issues,pull_request \
--filter json:/action=opened,reopened
```

A JSON string compares as itself; a number, `true`, `false`, or `null` as
its JSON text. A request that does not match answers `204` and is only
counted.

### Duplicates

Services retry when they do not hear back. A request whose delivery ID the
webhook has seen among its last 1,024 answers `200` and does nothing. The ID
is `X-GitHub-Delivery` with the `github` check, `webhook-id` with
`standard`, else an `Idempotency-Key` header; a request with none is never
a duplicate.

### Limits, failures, and history

- **Hourly limit:** `--max-per-hour` events per rolling hour, else
  `[webhooks] max_per_hour` (60); 0 or less for no limit.
- **Failures:** an event whose delivery fails (the agent is gone, a runtime
  is offline) or whose run ends `failed` counts a failure; a completed run
  resets the count. After `--max-failures` in a row (else
  `[webhooks] max_consecutive_failures`, 3) the webhook is disabled and its
  URL answers `404` until you enable it again.
- **History:** the last `[webhooks] history` events (50), each with its
  outcome, its thread, the session and run it reached, and how that run
  ended. Requests refused, filtered, or duplicated are counted, not kept.

### Test events

`cookie webhook test <ID> --body event.json --content-type application/json
--header "X-GitHub-Event: issues"` delivers an event as if it had arrived,
skipping the check, the filter, and the limits. Test events are marked in
the history and count toward neither the hourly limit nor the failures.

## GitHub

1. Create the webhook with `--check github`, as in the example above, and
   copy its URL and secret.
2. In the repository's **Settings › Webhooks › Add webhook**, paste the URL
   as the Payload URL, choose `application/json`, paste the secret, and pick
   the events, such as Issues and Issue comments. Or with the GitHub CLI:

    ```sh
    gh api repos/OWNER/REPO/hooks --method POST \
      -f name=web -F active=true \
      -f 'events[]=issues' -f 'events[]=issue_comment' \
      -f 'config[url]=https://hooks.example.com/hooks/Xq3vT9aLk2M' \
      -f 'config[content_type]=json' \
      -f 'config[secret]=<secret>'
    ```

3. GitHub sends a `ping` event at once. With the filter above it answers
   `204`; the webhook's counts show it as filtered.

`json:/issue/number` keys issues and their comments; use
`json:/pull_request/number` for pull requests.

## Security

A webhook's URL is public by design, and its requests go to an agent that
may run tools without asking. So:

- Give any webhook whose URL leaves your hands a check, and a narrow
  filter.
- Treat the body as what it is, text from outside: the event's marking
  tells the agent it may try to instruct it. The webhook's agent and its
  permission rules decide what an event can make it do; a webhook that
  only reports needs no write access.
- Secrets and tokens never reach the model, the logs, or the session's
  events, and `webhooks.json` is readable by you alone.

## Managing webhooks

### Command line

[`cookie webhook`](../clients/cli.md#cookie-webhook) lists, shows, creates,
edits, enables, disables, rotates, tests, and deletes webhooks, here or on
a remote daemon.

### Terminal UI and web

A Webhooks view in the [web client](../clients/web.md#webhooks) and a
`/webhooks` panel in the [terminal UI](../clients/tui.md#webhooks) are
coming; until then, use `cookie webhook`.

### Deleting

Deleting a webhook keeps its sessions. Deleting a session deletes the
webhooks that deliver into it or into a session of its tree, and forgets it
as a thread. Deleting a project deletes the new-session webhooks whose
sessions go in it.

## Agents and the webhook tools

Agents manage webhooks with `webhook_create`, `webhook_list`,
`webhook_update`, and `webhook_delete`, the deferred `webhooks` tool group
of the `global` runtime. An agent may target its own session, a session
below it, or new sessions; it cannot set a permission mode or a model pin,
and it cannot read or rotate a secret after the call that created it.

### Permissions

The `webhook` action covers every webhook tool; its resource is the target
kind, `session`, `subagent`, or `new`. A root run asks by default
(`webhook: ask`), since a webhook opens a public URL; delegated runs do not
see the tools unless their rules grant them. A thread prompt is sent as your
message, so `webhook: allow` lets an agent write messages that read as yours
into new sessions without asking. See
[Webhook permissions](../extend/agents.md#webhook-permissions).

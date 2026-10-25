# Push notifications

Status: **draft** (2026-10-25), awaiting approval; the user's decisions of
2026-10-25 are in section 14.
Extends the [web client](web-client.md) (section 9, installable app),
[cron jobs](cron.md), [background tool calls](background-tools.md)
(`waiting_on`), the [approval reviewer](approval-reviewer.md), and
[ACP agents](acp-agents.md) (elicitation), and changes the default of
[`[approval] timeout_ms`](../engine/approval.md) (section 4.4).

## 1. Goal and scope

**Today a phone learns nothing until it is opened.** The web client is an
installable app with a network-only service worker (web client spec,
section 9), but a turn that ends, or an approval that waits, while the
phone is in a pocket or the tab is in the background goes unnoticed until
the user looks.

**This spec** has the daemon send a Web Push notification to the user's
phones and browsers on two occasions:

- **A root session settles** (section 3): its run ended and nothing will
  start another one without the user. Concretely:
  - a root session's run ends `completed` or `failed`;
  - no user input and no producer message (steer, queue, goal reminder) is
    waiting to start the next run;
  - nothing the session waits on will wake it: no running background call,
    no subagent that runs or can still wake, no external producer
    registration, no active goal, and no **one-time** cron job aimed at it.
    **Recurring** cron jobs do not count: a session with a nightly job
    would otherwise never notify.
- **A run needs the user** (section 4): an approval escalates to the user,
  or an ACP agent asks a question (elicitation), in the root session or in
  any subagent below it.

In scope:

- the engine's settle rule and its signals (sections 3 and 4);
- a Web Push sender in the daemon: VAPID keys, RFC 8291 payload encryption,
  delivery and cleanup (section 6);
- subscriptions over the client protocol (section 7);
- suppressing a push while the user is looking at the session (section 8);
- the web client: turning notifications on, the service worker's `push`
  and `notificationclick` handlers, and opening the session from a
  notification (section 9);
- `[notifications]` configuration (section 10);
- approvals that wait for the user until answered, by default
  (section 4.4).

Out of scope:

- notifying a delegated session's settling on its own: its turns reach the
  root, which notifies when it settles (its approvals and questions do
  notify, section 4);
- answering an approval from the notification itself (Approve and Deny
  buttons): the service worker has no daemon token, and iOS shows no
  notification actions; the click opens the approval instead;
- native mobile apps, email, or third-party relays (ntfy, Pushover);
- desktop notifications from the TUI.

## 2. Vocabulary

- **Root session:** a session whose origin is not `delegated`, including
  ACP-agent sessions and sessions a new-session cron job created.
- **Tree:** a root session and every session delegated below it.
- **Settled:** a root session with no active run, nothing waiting to start
  one, and nothing in its wake set (section 3.2).
- **Armed:** a root session whose last run ended in a way that should be
  reported once it settles (section 3.1).
- **One-time job:** an enabled cron job aimed at the session (in-session
  target) with an `at` schedule (what `in` becomes), or a `cron` schedule
  with `max_runs = 1` (the default). The agent waking itself later through
  one is the agent still at work, so it holds the push back.
  **Recurring job:** any other enabled job with a next fire time, a `cron`
  schedule with `max_runs` above 1 or `"unlimited"`, judged by its
  definition and never by how many runs it has left.
- **Needs you:** an approval whose `approval_step` is `escalated` (it waits
  for the user), or an ACP elicitation that is requested and not resolved.
- **Subscription:** a browser's `PushSubscription` (endpoint, `p256dh`, and
  `auth` keys) registered with the daemon, with a label.
- **Push service:** the browser vendor's relay the endpoint points at
  (`fcm.googleapis.com`, `web.push.apple.com`,
  `updates.push.services.mozilla.com`, …).

## 3. When a root session settles

### 3.1 Arming and disarming

The engine keeps, per root session and in memory only, whether it is armed.

- **Armed** when a run of the session ends `completed` or `failed`.
- **Not armed** by `cancelled` (someone stopped it, so someone is there) or
  `interrupted` (the daemon went down; the restart says nothing).
- **Disarmed** when a run starts, when user input is admitted, when the
  notification is sent (or suppressed, section 8), and when the session is
  deleted.

A restart forgets every armed flag. Nothing is written to the session log;
session history is unchanged.

### 3.2 The settle rule

An armed root session settles when **all** of these hold, checked inside
the session actor:

1. no run is active and none is being admitted;
2. no pending user input, and no accepted, undiscarded, unconsumed producer
   message (`steer` or `queue`, goal reminders included);
3. its **wake set** is empty.

The wake set is what `live_waiting_kinds` and the fold's `Goal` report as
`waiting_on`, with one difference: **recurring jobs count for nothing**,
here and in every subagent below the root. So:

| The session, or a subagent below it, has… | Wake set |
|---|---|
| an active goal | `goal` |
| a running background call | `background_call` |
| a subagent turn running, a background delegation queued or running, or a child that can still wake | `subagent` |
| a live external producer registration (plugin or runtime) | `external_producer` |
| a one-time job aimed at it | `scheduled_job` |
| only recurring jobs aimed at it | (nothing) |

The last two rows need a second view of the tree, because today a
subagent with any cron job makes its parent wait as `subagent`
(`refresh_live_waiting`, `crates/engine/src/runtime/producers.rs`).
`refresh_live_waiting` computes, beside `waiting_on`, a **`wakes_once`**
flag per session: what wakes the session counting only one-time jobs, and a
child counting for its parent only while its own `wakes_once` holds. It is
carried up the tree under the same `waking_children` discipline, in a
second map. `waiting_on` and what clients see do not change.

`Crons::waits_on` gains a companion, `waits_on_once(session)`, true when a
one-time job is aimed at `session`.

### 3.3 When the rule is checked

The engine checks an armed session:

- after a run's `RunFinished` is committed **and** the actor has decided
  whether queued input or producer messages start the next run at once, so
  a queued message never yields a notification for the turn before it;
- whenever `refresh_live_waiting` changes the root's `wakes_once`, so a
  turn that ended while a subagent or external producer was still live
  notifies when that producer goes away without sending (a producer that
  does send starts a run, which disarms and re-arms the session);
- when a one-time job aimed at the session, or below it, is deleted,
  disabled, or becomes recurring through `crons.update`.

When the rule holds the engine disarms the session and publishes a
`SessionSettled` signal (section 5).

### 3.4 Examples

- The root answers a question: run `completed`, nothing waits: **notify**.
- The root delegates in the background and ends its turn: wake set
  `subagent`, no push. The subagent's report starts a run; that run ends
  with nothing waiting: **notify once**, for the final turn.
- The root ends its turn with a job `in 2h` aimed at it: no push. Two
  hours later the fire runs a turn; it ends with nothing waiting:
  **notify**.
- The root ends its turn and has a nightly job with `max_runs =
  "unlimited"`: **notify** now, and after each nightly run that settles.
  A `cron` job with `max_runs = 3` behaves the same, its last run
  included.
- The user cancels a run: **no push**.
- Goal mode keeps continuing: no push until the goal completes, pauses,
  or is cancelled and the last run ends.

## 4. When a run needs the user

### 4.1 Approvals

- **Trigger:** an `approval_step` of type `escalated` is committed in any
  session of the tree. Approvals the internal pipeline or the approval
  reviewer decide (`evaluated`) never notify, so `auto_approve_y` and
  `auto_approve_n` sessions notify only for what they hand to the user.
- **Content:** the tool's display name and one-line summary from the call's
  `tool_call_started` presentation (`bash: cargo test -p engine`), the
  session the approval is in (the subagent's handle when it is not the
  root), and the expiry time when the escalation carries `expires_at`.
- **Several at once:** escalations in one tree within 1 second of each
  other (parallel tool calls) go out as one push, `3 approvals waiting`,
  naming the first.
- **The window.** By default an approval waits until it is answered
  (section 4.4). When `[approval] timeout_ms` is set, a push for an
  approval with less than 5 seconds left is not sent.

### 4.2 Agent questions (elicitations)

ACP agents (Claude Code, Codex, Gemini CLI) ask questions through
elicitation (`ElicitationRequested`, answered by `elicitation.respond`;
ACP agents spec, elicitation round of 2026-10-24).

- **Trigger:** an `ElicitationRequested` in any session of the tree.
- **Content:** the agent's name and the first 160 characters of its
  `message`, flattened to plain text; `Sign-in requested` for a URL
  elicitation without a message.
- Grouped with approvals in the same 1-second window.

### 4.3 What it does not change

A needs-you push never arms or disarms the settle rule: the run is still
running. When the approval is answered, expires, or the question resolves,
the run goes on, and its end arms the root as usual. The settle push uses
the same tag (section 5.2), so it replaces the needs-you push on the
phone.

### 4.4 Approvals wait until answered by default

`[approval] timeout_ms` defaults to 30 seconds today, counted from the
escalation, so a push would reach the phone with seconds left (user
decision, 2026-10-25: no expiry by default).

- **`timeout_ms` becomes optional, with no default, and accepts any
  integer.** Absent, or any value `<= 0`, an escalated approval never
  expires: its `escalated` step carries no `expires_at`, and it waits
  until a client answers it, the run is cancelled, or the daemon goes
  down (which interrupts the run, as today). A value `<= 0` is the
  explicit way to write "never", since TOML has no null (user decisions,
  2026-10-25). A positive number of milliseconds expires as today.
- **Engine.** `ApprovalConfig::timeout_ms` becomes `Option<i64>`; the
  "must be positive" check in `crates/config/src/runtime.rs` goes, and
  `approval_expiry` returns `None` for `None` and for any value `<= 0`. The rest already handles an
  approval without an expiry: `approval_expiry_wait` waits without end,
  and `approval_deadline_exhausted` is false.
- **Clients.** The TUI and the web client already treat a missing
  `expires_at` as no countdown and no expiry
  (`crates/client_state/src/lib.rs`, `crates/tui/src/ui/app/approvals.rs`).
  No wire change: `expires_at` is already optional.
- **What waiting longer means:**
  - the run stays running, so its session stays in memory, its root's
    tree stays busy, and a parent waiting on it keeps waiting;
  - a new-session cron run (default permission mode `auto_approve_n`)
    whose call escalates now waits for the user instead of being denied
    by expiry after 30 seconds; the job's later fires are skipped as
    `previous run still running` until it is answered or cancelled, and
    the push tells the user;
  - an in-session fire into a session blocked on an approval queues
    behind that run, as it does behind any running turn.
- **Docs.** The approval page (`docs/site/engine/approval.md`) and the
  configuration reference describe the optional key and values `<= 0`;
  tests cover absent, `0`, and `-1` (no expiry) and `1` (expiry), not the
  default.

## 5. The signals and the notification

### 5.1 Engine signals

The engine publishes on a broadcast channel, beside
`subscribe_index_changes`. These are daemon-internal, not protocol
messages.

```rust
pub enum PushSignal {
    Settled {
        root: SessionId,
        run_id: RunId,
        outcome: SettledOutcome, // Completed { final_text } | Failed { error }
        title: Option<SessionTitle>,
        project: Option<ProjectId>,
        cron: Option<SessionCron>,
        at: Timestamp,
    },
    NeedsYou {
        root: SessionId,
        session: SessionId,           // where the approval or question is
        handle: Option<String>,       // the subagent's short id, when not the root
        item: NeedsYouItem,           // Approval { approval_id, tool, summary, expires_at }
                                      // | Question { elicitation_id, agent, message }
        title: Option<SessionTitle>,  // the root's
        at: Timestamp,
    },
}
```

### 5.2 The payload

The daemon sends a small JSON payload; the service worker renders it.

```json
{
  "v": 1,
  "kind": "settled",
  "root": "01JB…",
  "session": "01JB…",
  "title": "Fix the flaky cron test",
  "body": "The test now waits on the fire's history entry instead of…",
  "outcome": "completed",
  "at": "2026-10-25T14:02:11Z"
}
```

- **`kind`:** `settled`, `approval`, `question`, or `test`.
- **`title`:** the root session's title, else `Session <short id>`. A
  new-session cron run's title already names its job. For `approval` and
  `question`, prefixed `Approval · ` or `Question · `.
- **`body`:**
  - `settled`, `completed`: the first 160 characters of the run's final
    text, Markdown markers and line breaks flattened to plain text, with
    `…` when cut; `Finished.` when the run ended with no text;
  - `settled`, `failed`: `Failed: ` and the run's safe error message, cut
    the same way;
  - `approval` and `question`: section 4, with `in <handle>` when the item
    is in a subagent and `expires 14:03` when it has an expiry.
- **`session`:** where the click goes: the root for `settled`, the
  approval's or question's session otherwise.
- The service worker shows it with:
  - `tag: "session:<root>"` and `renotify: true`, so a newer notification
    for the same tree replaces the older one;
  - `requireInteraction: true` for `approval` and `question` where the
    browser supports it;
  - `data: { session }`, for the click (section 9.3);
  - the app icon and badge already in the bundle.
- A payload kind or version the service worker does not know is shown with
  its `title` and `body` as they are; it never shows nothing (section 9.2).
- The encrypted record stays under the 4096-byte limit push services set:
  the payload is cut to at most 3 KiB before encryption.

## 6. Sending: Web Push in the daemon

### 6.1 VAPID keys

- On first use the daemon generates a P-256 key pair and stores the private
  key in `<data_root>/push/vapid.pk8` (PKCS#8, mode 0600). It is never sent
  to a client; its public key (uncompressed point, base64url) is what
  `notifications.get` returns.
- **Rotating** it is deleting the file: every subscription is then bound
  to a key the daemon no longer has, so the daemon drops them all, and each
  browser resubscribes on its next connect (section 9.1).
- The VAPID JWT (RFC 8292, ES256) carries `aud` = the endpoint's origin,
  `exp` = now + 12 h, and `sub` = the contact below. Tokens are cached
  per audience until an hour before `exp`.
- **The contact is derived, not asked for.** `sub` is the repository URL
  the binary was built from, `env!("CARGO_PKG_REPOSITORY")`
  (`https://github.com/cookie-agent/cookie-agent`; the server crate takes
  `repository.workspace = true` for it). It passes every push service's
  check, needs no setup, and sends nothing about the user. Rejected
  sources:
  - the daemon's own URL (the page origin, usually a `*.ts.net` name):
    unreachable for a push service, and it would tell Google, Apple, and
    Mozilla the tailnet's host names;
  - the user's email (git `user.email`, memory): personal data sent to
    third parties without asking.

  `[notifications] contact` overrides it for a user who wants to be the
  one contacted.
- **What the push services check** (checked 2026-10-25):
  - RFC 8292: `aud` and `exp` are required, `exp` at most 24 hours ahead;
    `sub` is optional and SHOULD be a `mailto:` or `https:` contact for the
    application server's operator.
  - Apple (*Sending web push notifications in web apps and browsers*)
    answers `403 BadJwtToken` when the subject claim is not a URL or
    `mailto:`, the audience is not the push service's origin, or the
    expiry is more than a day ahead, and asks for a JWT refreshed no more
    than once an hour. It needs no Apple Developer Program membership. So
    `sub` is always sent.
  - Nothing verifies the contact: Apple checks only its form. It is
    whom a push service would write to about this sender, so a user who
    wants to be that contact sets their own `mailto:`.

### 6.2 Encryption

RFC 8291 `aes128gcm` content encoding: an ephemeral P-256 key per message,
ECDH with the subscription's `p256dh`, HKDF-SHA-256 with its `auth`
secret, one AES-128-GCM record. Everything comes from `aws-lc-rs`, already
in the dependency tree through rustls (`agreement::ECDH_P256`, `hkdf`,
`aead::AES_128_GCM`, `signature::ECDSA_P256_SHA256_FIXED_SIGNING`), and
HTTP from `reqwest`, already in the tree through oven-sdk; the server crate
takes both as direct dependencies. No new crate is downloaded.
*(proposed; the alternative is the `web-push-native` crate, which brings
the RustCrypto P-256 stack.)*

### 6.3 Delivery

For each signal that passes the configuration (section 10) and presence
(section 8) checks, the notifier task in the server sends one `POST` to
every subscription, concurrently:

| Header | Value |
|---|---|
| `Authorization` | `vapid t=<jwt>, k=<public key>` |
| `Content-Encoding` | `aes128gcm` |
| `TTL` | 86400 (a phone offline for a day still gets it); for an approval that expires, the seconds left of its window, at least 1 |
| `Urgency` | `high` for `approval`, `question`, and a `failed` run; `normal` otherwise |
| `Topic` | 22 base64url characters hashed from the root's id, so the push service replaces an undelivered notification for the same tree |

Responses:

- `201`: delivered to the push service; `last_sent_at` updated.
- `404` or `410`: the subscription is gone; the daemon deletes it and
  announces `notifications.changed`.
- `429` and `5xx`: up to three retries with backoff (honouring
  `Retry-After`), within one minute, and never past an approval's expiry.
- Any other status, or a network error: `last_error` records it (status
  and the first 200 bytes of the body); the subscription stays.

The sender never blocks the engine: signals go through a bounded channel,
and when it is full the oldest signal is dropped with a warning.

The daemon needs outbound HTTPS to the push services. A daemon with no
route to the internet keeps working; its sends fail into `last_error`.

### 6.4 Storage

`<data_root>/push/subscriptions.json` (mode 0600), written atomically like
`crons.json`:

```json
{
  "subscriptions": [
    {
      "id": "p7c2m9qk",
      "endpoint": "https://web.push.apple.com/QG…",
      "p256dh": "BN…",
      "auth": "x3…",
      "label": "iPhone · Safari",
      "created_at": "2026-10-25T13:40:00Z",
      "last_sent_at": "2026-10-25T14:02:12Z",
      "last_error": null
    }
  ]
}
```

The file is strict like configuration: unknown fields are an error.

## 7. Client protocol

Additive, a minor client protocol bump.

| Method | Direction | Params → result |
|---|---|---|
| `notifications.get` | request | `{}` → `{ public_key, enabled, subscriptions: [Subscription] }` |
| `notifications.subscribe` | request | `{ endpoint, p256dh, auth, label }` → `{ subscription }` |
| `notifications.unsubscribe` | request | `{ id }` → `{}` |
| `notifications.test` | request | `{ id }` → `{ status: "sent" \| "gone" \| "failed", detail? }` |
| `notifications.changed` | notification | daemon → client: `{ subscriptions }` |
| `client.presence` | notification | client → daemon: `{ session: SessionId \| null }` (section 8) |

- **`Subscription`** as clients see it: `id`, `label`, `push_service` (the
  endpoint's host), `created_at`, `last_sent_at`, `last_error`. The
  endpoint and keys are never sent back.
- **`subscribe`** is idempotent by endpoint: the same endpoint updates the
  keys and label and keeps its id. The endpoint must be `https://`; the
  label is 1 to 80 characters.
- **`test`** sends a fixed payload (`kind: "test"`) to that subscription
  now, ignoring presence and `enabled`.
- The methods work whatever `[notifications] enabled` says, so a user can
  set up devices before turning sending on.

No plugin, runtime, or session-history change.

## 8. Not notifying the user who is watching

A push for the session the user is looking at is noise (user decision,
2026-10-25). Clients tell the daemon what they show:

- `client.presence { session }` names the session the client shows **and**
  that the user is there; `{ session: null }` says the user is not.
- **Web client:** the page sends the viewed session while
  `document.visibilityState` is `visible` and the window has focus, and
  `null` when it hides, blurs, or has seen no pointer, key, or touch input
  for 2 minutes. It resends on reconnect and on switching sessions.
- **TUI:** the same with the terminal's focus events (crossterm
  `FocusGained`/`FocusLost`) and the same 2-minute idle rule; a terminal
  without focus reporting never reports presence, so its user still gets
  pushes.
- A connection's presence ends with the connection.

The notifier suppresses a signal when any connection reports presence on a
session of the same tree: for `settled`, the engine then disarms the root
as if it had sent. Presence on another tree does not suppress: the user
may be in a different session on the desktop and still want the phone to
buzz.

## 9. The web client

### 9.1 Turning notifications on

**Settings › This browser › Notifications**, decided by pure logic beside
`crates/web/src/install.rs`, in these states:

| State | What it shows |
|---|---|
| No `PushManager`, or not a secure context | Needs HTTPS (as the install offer says) |
| iOS or iPadOS, not running from the Home Screen | Add to Home Screen first (iOS 16.4 and later push only to installed web apps) |
| Permission `denied` | Notifications are blocked in the browser's site settings |
| Not subscribed | **Turn on notifications** |
| Subscribed with this daemon's key | On; **Send test**, **Turn off**, the label (editable) |
| Subscribed with another key | Subscribed to another daemon or an older key; **Switch to this daemon** |

- **Turn on** runs from the click (browsers require a user gesture):
  `Notification.requestPermission()`, then
  `registration.pushManager.subscribe({ userVisibleOnly: true,
  applicationServerKey })` with the daemon's public key, then
  `notifications.subscribe`. The default label is the device and browser
  from the user agent (`Pixel · Chrome`, `iPhone · Safari`).
- **The subscribed daemon.** Turning on, switching, and resubscribing on
  connect store that daemon's WebSocket URL in the page's storage
  (`push_daemon_url`), beside the URLs and tokens it already keeps; a
  click uses it (section 9.3).
- **Turn off** unsubscribes in the browser, calls
  `notifications.unsubscribe`, and forgets `push_daemon_url`.
- **Switch** unsubscribes the old subscription and subscribes again. A
  browser holds one subscription per service worker, bound to one key, so
  a page origin can receive pushes from **one daemon at a time**; Settings
  says so.
- On every connect, when permission is granted and a subscription exists
  with this daemon's key, the page sends `notifications.subscribe` again,
  so an endpoint the browser rotated (`pushsubscriptionchange`) or a
  daemon that lost its file catches up.
- Below, **Devices** lists every subscription from `notifications.get`
  with its push service, last sent time, and last error, each removable.
- After installing on a phone, the one-time install banner's place offers
  **Turn on notifications** once, dismissible the same way.

### 9.2 The service worker

`sw.js` stays network-only (no Cache Storage; the fetch handler is
unchanged) and gains two handlers:

- **`push`:** parses the payload and calls
  `registration.showNotification(title, options)` inside
  `event.waitUntil`. It always shows a notification, falling back to
  `Cookie Agent` with no body for a payload it cannot parse: Safari revokes
  subscriptions that receive pushes without one, and Chrome shows its own
  "updated in the background" instead.
- **`notificationclick`:** closes the notification, then focuses a window
  of the app within the worker's scope and posts it
  `{ type: "open-session", session }`; with none open, it opens
  `./#session=<id>`.

The section 9 note in the web client spec and the
`the_service_worker_is_network_only` test are updated: the worker still
caches nothing.

### 9.3 Opening a session from a link

The page reads `#session=<id>` at launch, beside `#token=`, removes it from
the address bar the same way, and opens that session once connected (or
says it no longer exists).

The session belongs to the daemon that sent the push, which is the one in
`push_daemon_url` (section 9.1), not necessarily the one the page last
connected to: the page may have connected to another daemon since. So a
`#session=` launch connects to `push_daemon_url` instead of the remembered
URL, with the token stored for it; a running page connected elsewhere
switches its connection there first, as the connect screen does. With no
token stored for that URL, the connect screen opens with it filled in.
The daemon's own URL is not in the payload: a daemon does not know the
address the page reaches it by. A subagent's session opens in its root's tree,
where its approval or question waits. The `open-session` message does the
same in a running page.

When the daemon cannot be reached, the page shows its normal connection
error and forgets the session it was asked for: nothing is kept pending
for a later connection (user decision, 2026-10-25).

When the page shows a tree, it closes that tree's notifications
(`registration.getNotifications({ tag })`), so a stale approval does not
stay on the phone after it was answered on the desktop. Nothing is pushed
to retract a notification: a push that shows nothing is what section 9.2
avoids.

## 10. Configuration

```toml
[notifications]
enabled = true        # false stops every push; subscriptions and RPCs keep working
preview = "reply"     # "reply": the start of the final text or question; "status": only what happened
approvals = true      # notify for approvals and agent questions waiting for the user
cron_sessions = true  # notify when a root session a new-session cron job created settles
contact = "mailto:me@example.com"  # optional VAPID `sub` override; absent: the repository URL
```

- With `preview = "status"` the body says only `Finished.`, `Failed.`,
  `Approval waiting`, or `Question waiting`.
- `cron_sessions = false` drops only the settle push of those sessions;
  their approvals and questions still notify, since a run is blocked.
- Strict like all configuration: unknown keys, wrong types, a `preview`
  other than the two values, and a `contact` that is not `https:` or
  `mailto:` are errors.
- Live: `config.apply` and `config.reload` take effect for the next
  signal.
- The configuration reference documents the section; tests cover
  unknown fields and the boundaries, not the default values.

## 11. Security and privacy

- Only an authenticated client can add a subscription; the daemon token is
  as powerful as ever, and nothing new is reachable without it.
- The daemon POSTs only to `https://` endpoints a client registered. An
  authenticated client can already run commands, so the endpoint is not
  restricted to known push services (new browsers would break an
  allowlist).
- Payloads are end-to-end encrypted to the browser (RFC 8291); push
  services see the size, the time, the `Topic` hash, and the endpoint.
  The lock screen shows the title and preview, an approval's command line
  included: `preview = "status"` keeps them off it.
- A notification never answers anything: approving still takes an
  authenticated client.
- The VAPID private key and subscriptions are files of mode 0600 under the
  data root; neither is in a protocol response.
- The service worker has no token and makes no daemon request.

## 12. Documentation

- The web client guide (`docs/site/clients/web.md`): a **Phone
  notifications** section: HTTPS (for example `tailscale serve`),
  installing on iOS first, turning on, testing, what triggers a push, and
  the approval window.
- The configuration reference: `[notifications]`.
- The protocol reference and changelog: the `notifications.*` methods and
  `client.presence`.
- The web client spec: an addendum pointing here from section 9.
- The specs index: this page, under drafts until implemented.

## 13. Tests and phases

### 13.1 Tests

- **Approval expiry:** `[approval]` without `timeout_ms`, and with
  `timeout_ms = 0` or `-1`, escalates with no `expires_at` and stays
  pending past what used to be the window; `timeout_ms = 1` expires; a
  non-integer value is a configuration error.
- **Engine settle rule** (`crates/engine/src/runtime/tests/`), one test per
  row of section 3.2 and per example in 3.4, plus: `failed` notifies;
  `cancelled` and `interrupted` do not; a queued user message at run end
  does not notify for the earlier turn; an external producer that
  unregisters without sending notifies; a recurring job on a subagent does
  not keep the root from settling, a one-time job on one does; a `cron`
  job with `max_runs = 3` counts as recurring on its last run, one with
  `max_runs = 1` as one-time; deleting a one-time job settles an armed
  root; delegated sessions never signal `Settled`; `cron_sessions = false`
  drops new-session cron runs' settle signals but not their approvals.
- **Needs-you signals:** an escalated approval in the root and in a
  subagent signals with the right session and handle; an `evaluated`
  approval does not; parallel escalations within the window become one
  signal; with `timeout_ms` set, an approval with under 5 seconds left
  is not sent; an elicitation signals; `approvals = false` drops both; neither touches the
  armed flag.
- **Crypto:** the RFC 8291 appendix A example (fixed keys, salt, and
  plaintext) encrypts to its published record; a VAPID JWT verifies with
  the public key and carries `aud`, `exp`, and `sub`.
- **Sender** against a local test push service: `201`, `410` (subscription
  deleted, `notifications.changed` sent), `429` with `Retry-After`, `500`
  then `201`, a network error, no retry past an approval's expiry; headers
  as in section 6.3.
- **Protocol:** the new methods in the frozen corpus additions and the
  compatibility checker (`check-schema-additive.sh`).
- **Presence:** a connection showing a session of the tree suppresses (and
  disarms a settle); one showing another tree does not; a closed
  connection's presence ends.
- **Web:** the settings states of section 9.1 as pure logic; `#session=`
  parsing and removal; a `#session=` launch connects to `push_daemon_url`
  over a different remembered URL, and to the connect screen when no
  token is stored for it; turning off forgets `push_daemon_url`; the `sw.js` asset test extended to require the
  `push` handler to call `showNotification` on every path.
- **Manual:** an Android phone (Chrome) and an iPhone (installed app,
  iOS 16.4+) over `tailscale serve`: turn on, test, a real turn, an
  approval answered from the phone after the click, presence suppressing
  on the desktop.

### 13.2 Phases

Each phase lands, commits, and pushes on its own, after the user's go.

1. **Engine:** arming, `wakes_once`, `waits_on_once`, the settle checks,
  the needs-you signals, the `PushSignal` broadcast, `[notifications]`
  configuration, tests.
2. **Daemon push:** VAPID keys, encryption, the sender, the subscription
  store, the `notifications.*` methods and `notifications.changed`,
  `client.presence`, the protocol bump, tests.
3. **Web client:** Settings, the service worker handlers, `#session=` and
  `open-session`, closing shown notifications, presence, tests.
4. **TUI presence and docs:** focus reporting in the TUI, the
  documentation of section 12, the specs index, the manual check.

The approval default (section 4.4) needs no push code and may land first,
on its own.

## 14. Decisions (2026-10-25)

1. **What counts as waiting:** the wake set of section 3.2, not only
   accepted messages: running subagents, background calls, external
   producer registrations, an active goal, and one-time jobs hold the push
   back, as do queued input and producer messages.
2. **External producers** hold the push back, as they hold back goal
   readiness.
3. **Failed runs** notify, with `Urgency: high`.
4. **Preview:** the start of the reply by default (`preview = "reply"`).
5. **Presence** is in scope (section 8).
6. **Approvals** are in this spec (section 4.1). ACP agents' questions
   (elicitations) are in it too, under the same `approvals` setting
   (section 4.2).
7. **One-time jobs** are `at`/`in` schedules and `cron` schedules with
   `max_runs = 1`, judged by the definition: the agent waking itself
   through one is not a moment to notify.
8. **Approvals do not expire by default** (section 4.4): `[approval]
   timeout_ms` becomes optional, and absent or any value `<= 0` means an
   approval waits until it is answered.

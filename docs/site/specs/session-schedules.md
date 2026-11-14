# A session's schedules and webhooks

Status: **implemented** (2026-11-14). The
[preview](session-schedules/preview.html) shows the title bar's clock and
the Schedules tab; the user's decisions are in [section 2](#2-decisions).
Client only: no protocol change.

## 1. Goal

A session's cron jobs and webhooks are listed only in the Schedules and
Webhooks views, across every session. The session list's row marks a
session waiting on its cron job with a clock
([web polish](web-polish.md#changed-after-the-pass-2026-11-14)), but the
open session does not say when its next fire is, nor list what fires or
delivers into it.

## 2. Decisions

**Decided 2026-11-14** after the preview:

- **D1, a clock in the title bar.** While a job fires into the open
  session (a session or subagent target), a clock joins the title bar's
  live badges, before the context ring, with the time to the soonest next
  fire (`12m`), and `· 2` when several jobs do. It reads `queued` while a
  fire was delivered and waits behind the running turn (delivered, no run
  yet). Hover names the jobs and when; a click opens the session panel's
  Schedules tab. Jobs made in the session that start sessions of their own
  do not light it (Q1 as recommended).
- **D2, a Schedules tab, always** (the user: "should always show"). The
  session panel's tab lists, in three groups: the jobs that fire into this
  session; the jobs an agent made in it that start their own sessions; and,
  for a session a job started, that job. Each job shows its name, when it
  fires in words, its next fire, its runs, who made it, its switch, and a ⋯
  with **Run now**, **Edit** (in Schedules), and **Delete…**. Under them,
  **Schedule a prompt** (the form for this session) and **Open
  Schedules**. With none, it says so above the two buttons.
- **D3, a Webhooks tab, always** (the user: "a dedicated webhook panel").
  The same three groups for webhooks: those that deliver into this
  session, those an agent made in it, and the webhook whose event started
  it. Each webhook is a row as the Webhooks view draws it (its check's
  icon, name, tags, health line, switch, and a ⋯ with **Send a test
  event**, **Edit**, and **Delete…**); a click opens its page in Webhooks.
  Under them, **Add a webhook** (the form for this session) and **Open
  Webhooks**. Webhooks get no title bar badge: nothing counts down to an
  event (Q3).
- **D4, the terminal UI too** (Q4 as recommended): the bottom bar shows the
  clock and the next fire, giving way first when the bar is narrow, and
  `/cron` and `/webhooks` list the session on screen's own first.

## 3. Phones

Nine tabs do not fit 360 px at 44 px each. The tab strip scrolls sideways,
the open tab scrolled into view; the phone audit counts a target inside a
sideways scroller as reachable.

## 4. Tests

`client_state`: which jobs and webhooks fall in each group, and the
clock's soonest fire, count, and queued state. Browser: the tabs list a
session's job and webhook and their actions; the clock shows for a job
firing into the open session. TUI: the bottom bar's clock and its place in
the narrowing order; the panels' order.

## 5. Implementation notes

- The groups, the clock, and the terminal UI's order are
  `cookie_agent_client_state::session_schedules`; the tabs are
  `crates/web/src/views/session_schedules.rs`, the clock is in
  `views/live.rs`.
- The ⋮ menu gained **Schedules** and **Webhooks**, which open the tabs,
  beside **Schedule a prompt** and **Add a webhook**, which open the forms.
  A guest sees neither tab.
- A webhook row's ⋯ asks before New URL, New secret, and Delete on the
  webhook's page, as in the Webhooks view: from the session panel it opens
  that page first.
- The terminal UI now reads `crons.list` on connect and on every
  `crons.changed`, not only while `/cron` is open, for the bottom bar.


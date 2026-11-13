# The Webhooks view, polished, and titles for the sessions webhooks start

Status: **approved** (2026-11-13), being implemented in the phases of
[section 9](#9-phases). The user's decisions are in
[section 2](#2-decisions); [section 3](#3-questions) lists the questions
the draft raised, each answered as it assumed. The
[preview](webhooks-view/preview.html) shows every change to the view
beside a screenshot of it today. Builds on [webhooks](webhooks.md) and
the [web polish](web-polish.md) pass, whose view header, list rows, and
44 px phone targets it follows.

## 1. Goal and scope

The Webhooks view (`crates/web/src/views/webhooks.rs`) does everything
the [webhooks spec](webhooks.md#14-clients) asks of a client. Photographed
on 2026-11-13 with four webhooks, test events, and refused requests, it
falls short in two ways:

- **Setting one up** asks for syntax: the check
  (`hmac_sha256 header=… encoding=… prefix=…`), the thread key
  (`json:/issue/number`), and the filter (`header:X-GitHub-Event=issues`)
  are text fields described only by their placeholders.
- **Knowing how one is doing** takes reading: an open webhook is fifteen
  key–value lines over seven buttons, rejected requests are a count with
  no time or reason, and test events show nowhere in the row.

This spec polishes the view (W1–W5, H1–H6, web client only), and adds one
thing beyond it: a **title** for the sessions a webhook starts, which the
user or an agent sets (T, protocol, engine, tools, CLI, and every
client).

Out of scope: what a webhook does with a request, the store, and the
public listener.

## 2. Decisions

**Decided 2026-11-13**, after the [preview](webhooks-view/preview.html):

- **D1, every proposal of the preview** (W1–W5, H1–H6), an open webhook
  included as a page of its own (H2).
- **D2, presets are the web client's.** GitHub, Stripe, Bearer token, and
  Anything fill the form's fields; neither the protocol, the daemon, nor
  `cookie webhook` learns them.
- **D3, a title field, set when the webhook is made.** The user, in every
  client, and an agent, through `webhook_create` and `webhook_update`, give
  the sessions a webhook starts a title as plain text. Nothing in it is
  read from the requests: the user said not to fill it from a request's
  values, only to ask for it when the webhook is created or edited
  ([section 6](#6-titles-for-the-sessions-webhooks-start)).

## 3. Questions

The user took every assumed answer on 2026-11-13.

| # | Question | Answer | Alternative |
|---|---|---|---|
| Q1 | Telling a webhook's sessions apart | The title, then ` · ` and the thread key or the arrival time, as today's title follows the webhook's name, unless the webhook delivers into one session | The title alone, the same for every session |
| Q2 | An automatic title | `title_auto: true` leaves the title to the utility model after the first turn, as a session the user starts gets one | A set title only |
| Q3 | New-session cron jobs | The same `title` and `title_auto` | Webhooks only |

## 4. Bugs

- **W1, phone event rows.** At 390 px the row's *Open session* button keeps
  its width and the outcome wraps a syllable a line. Fixed by H3's two-line
  rows.
- **W2, test events count nowhere.** The counts leave tests out (rightly:
  they are not traffic), so a webhook with only tests says *no requests
  yet*. The row shows its last event instead (H1).
- **W3, rejections without when or why.** Rejected, filtered, duplicate,
  and rate-limited requests are counted and, by design, not kept. The
  counts carry each one's last time, and what each answer means follows
  from the check: the row and the Events tab say both (H1, H3).
- **W4, on or off said twice.** The *enabled* tag goes; a tag stays only
  for what the switch cannot say (*off after 5 failures*, *no check*).
- **W5, the local-URL note.** While `[public] url` is unset, one amber
  callout above the list says the URLs work only on this network, with a
  link to Settings › Configuration, instead of grey text in each webhook.

## 5. The view

- **H1, rows.** An icon by the preset the webhook matches (from its check:
  GitHub, Standard Webhooks, bearer, none), the name, a tag for *no
  check* or *off after n failures*, the target in words ("→ Deploy watch",
  a link; "→ new sessions · primary · one per `issue.number`"), and a
  health line: a dot (green: the last event delivered; amber: a rejection
  since; red: failing; grey: no events), the last event and its time, and
  the last rejection with its reason. The switch; ⋯ and right-click with
  Edit, Send a test event, and Delete.
- **H2, an open webhook is a page.** Opening a row shows the webhook alone
  (Back returns to the list, as an editor does, D14 of Quiet Ink): its
  icon, name, and check and target under it; the switch, **Send a test
  event**, and ⋯ (Edit, New URL…, New secret…, Delete…, the last three
  asking first as today). Three tabs:
  - **Setup**: the URL and the secret (masked, with Show and Copy), the
    steps for the sender of the matching preset (GitHub: Settings ›
    Webhooks › Add webhook, the content type, the events), and *what
    happens* in three sentences: where each event goes, what a new
    session is told first, what comes with every event.
  - **Events**: H3.
  - **Rules & limits**: the filter, the headers the agent sees, the
    delivery, the limits, and the title (section 6).
- **H3, events.** Counters for delivered, rejected, filtered, and
  rate-limited requests, each with its last time and its meaning. Each
  event is a row: a dot, the outcome in words, a *test* tag, the time
  (relative, absolute in the tooltip), and the session as a link. Opened,
  its request reads as the method and path, the headers as a table, and
  the body as pretty-printed JSON (as received when it is not JSON; its
  size when binary), with Copy.
- **H4, the form.** **Start from** GitHub, Stripe, Bearer token, or
  Anything (D2), which fill the check, filter, thread key, headers shown,
  and title. Four steps, then Advanced:
  1. **Name and target**: name; *a session* or *new sessions*; agent,
     model, and mode chips; **one session per**: a JSON field, a header,
     or each event; **session title** (section 6).
  2. **What the agent is told**: when a session starts; with every event.
  3. **Who may send it**: the check from a list (none, GitHub signature,
     Standard Webhooks, bearer token, custom HMAC, the last opening its
     header, encoding, and prefix fields); the secret, which cookie makes
     unless **Use my own…**.
  4. **Which requests count**: conditions built from a source (header or
     JSON field), its name, and its values as chips; **Edit as text**
     shows the lines `cookie webhook --filter` takes.
  5. **Advanced** (folded): delivery, headers the agent sees, events per
     hour, sessions at once, failures before off.
- **H5, after creating.** The new webhook's page opens on a **ready**
  card: the URL and the secret, each with a primary Copy, where to paste
  them for the preset, and **Send a test event**. The note above the list
  goes.
- **H6, phone.** An open webhook's title bar has ⋯; under it **Test** and
  **Copy URL** as two 44 px buttons; events as two-line rows that open
  their request on a tap.

## 6. Titles for the sessions webhooks start

Today a session a webhook starts is titled `<webhook> · <thread key>`, or
`<webhook> · <time>` without one (`session_title` in
`runtime/webhook/deliver.rs`), committed as a user title, so the
automatic title never replaces it.

- **`WebhookTemplate.title`** (optional, plain text, 1 to 96 characters,
  no control characters): what the sessions are called, given when the
  webhook is created or edited. It takes the webhook name's place in
  today's title, so the thread key or the arrival time still follows it
  (Q1): `Shop issues · 42`, `Nightly deploys · 2026-11-13 00:22`. Nothing
  is read from a request into it (D3). Without one, the title is today's.
- **`WebhookTemplate.title_auto`** (Q2, default `false`): the session is
  created untitled, as a session the user starts is, so the automatic
  title names it after its first turn (or its excerpt fallback, as
  `[session_title]` says). Any title committed at creation, automatic or
  not, would make the session ineligible for one
  (`AutomaticTitleEligibility`), so `title` is not used then. When
  `[session_title] generate_on_first_turn` is off, `title_auto` does
  nothing and `title` (or today's title) is committed as now.
- **Checked when saved.** `webhooks.create` and `webhooks.update` refuse a
  title that is empty, too long, or holds a control character, as a
  session's rename does.
- **Every surface asks for it.** The client protocol
  (`WebhookTemplate.title`, `title_auto`; a minor bump), the plugin
  protocol where it carries webhook templates (a minor bump), the
  `webhook_create` and `webhook_update` tools (`target.new.title`,
  `target.new.title_auto`, which their notes ask the agent to set for
  what the sessions will be about), `cookie webhook create/edit --title`
  and `--title-auto`, the TUI's `/webhooks` form, and the web form, in its
  first step beside the target, with the switch *Let the model name it*.
- **Cron (Q3).** A new-session cron job's template takes the same `title`
  and `title_auto`, with the job's label giving way to it as the webhook's
  name does.
- **Presets** suggest a title the user edits: GitHub `Issues`, Stripe
  `Payments`; the others none.

## 7. What does not change

How a request is checked, filtered, threaded, and delivered; the store's
history and counts (a rejected request is still not kept); sessions'
titles once created (a rename by the user still wins).

## 8. Tests

- Engine: a webhook's and a cron job's title in place of their name, the
  thread key or time after it, the fallback without one, refusals of a bad
  title, `title_auto` leaving the session eligible and doing nothing when
  generated titles are off.
- Protocol: the new members in the schemas and the compatibility corpus
  rules; client and plugin baselines regenerated.
- `client_state`: the health line (which outcome is newest, the reason a
  rejection gives by check), the preset a webhook matches, presets'
  fields, and the filter's parts to lines and back.
- Browser scenarios: `webhooks` rewritten for the page, the tabs, the
  ready card, and a preset; the phone audit covers the new controls.

## 9. Phases

1. **Bugs and rows**: W1–W5, H1.
2. **The page**: H2, H3, H5.
3. **The form**: H4 with the presets, without the title.
4. **Titles**: section 6 in the protocol, engine, tools, CLI, TUI, and the
   web form; cron's.
5. **Phone and docs**: H6; the webhooks guide, the web and TUI client
   pages, and the configuration and tool references.

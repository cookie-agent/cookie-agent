# Asking for a GitHub star

Status: **implemented** (2026-11-27; client protocol 1.106.0), with a
[preview](github-star/preview.html). The user's decisions are in
[section 7](#7-decisions-2026-11-27); [section 8](#8-implementation-notes)
says where the build adds to the draft.

## 1. Problem

Stars are how people find a project on GitHub, and someone who has used
cookie for a week is the person most likely to give one. Nothing asks them
today. A request should come at most once, should not cover the work, and
should make starring one click when the daemon's machine is already signed
in to GitHub.

## 2. What the user sees

Once the daemon has been in use for 7 days, the web page shows a card over
the bottom right corner of the transcript, above the composer. On a phone the
card spans the width above the composer.

> ☆ **Enjoying Cookie Agent?**
> A star on GitHub helps others find it.
> **[☆ Star as @login]** [Later]
> Don't remind me

- **Star** has one of two forms, chosen before the click (section 4):
  - **Star as @login** when the daemon can star for an account. The login is
    shown because the daemon's machine is often not the browser's, and the
    user should know whose star it is. The daemon stars the repository and
    the card says "Thanks for the star!" for two seconds, then closes.
  - **Star on GitHub ↗** when it cannot. A link to
    `https://github.com/cookie-agent/cookie-agent` in a new tab. The card
    says "Thanks! GitHub opened in a new tab." and closes.
- **Later**, and the card's ×, hide it for 7 days.
- **Don't remind me** hides it for good.
- If the daemon fails to star (a fine-grained token without the Starring
  permission, a network error), the card says "Couldn't star as @login.
  Star it on GitHub instead?" and offers the link, with **Close**. That is a
  second click: a new tab opened after waiting for the daemon is blocked by
  browsers, so the page cannot fall back to opening it by itself.

The card does not show to a guest of a conversation share, nor in the TUI.

## 3. The daemon's record

`~/.cookie-agent/star.json`, next to `crons.json`:

```json
{
  "first_used": "2026-11-27T09:12:00Z",
  "snoozed_until": null,
  "done": null
}
```

| Field | Meaning |
|---|---|
| `first_used` | Written when the daemon starts and the file is missing. Installs from before this change start counting when they first run it. |
| `snoozed_until` | Set to now + 7 days by **Later** or ×. |
| `done` | `{"how": "starred" \| "dismissed" \| "already", "at": <time>}`. Once set, the card never shows again and the daemon makes no more GitHub requests. |

**Star** writes `starred` whether the daemon starred or the page opened the
link, since the user's click is the answer either way. A file that cannot be
parsed is treated as `done` (logged once), so a broken file never makes the
card appear on every page load. Deleting the file starts the count again.

## 4. GitHub access

The daemon looks for a token only once the card would otherwise show (7
days passed, not done, not snoozed). Before that it makes no GitHub
requests. In order:

1. `GH_TOKEN`, then `GITHUB_TOKEN`, from the daemon's environment.
2. `gh auth token --hostname github.com`, with a 5 second timeout. Missing
   `gh`, a non-zero exit, or empty output means no token.

With a token, the daemon calls the GitHub REST API directly (`reqwest`, no
`gh` subprocess for the calls themselves):

| Call | Use |
|---|---|
| `GET /user` | The login shown on **Star as @login**. A 401 means no usable token. |
| `GET /user/starred/cookie-agent/cookie-agent` | 204: already starred, so write `done: already` and show nothing. 404: not starred. |
| `PUT /user/starred/cookie-agent/cookie-agent` | The star, on click. 204 is success; anything else is the failure of section 2. |

The token is held only for these calls. It is never logged, stored, or sent
to a client. The result of the two `GET`s is cached in memory for 24 hours,
so page loads do not reach GitHub each time. A network error counts as no
token: the card offers the link.

## 5. Protocol

Two methods and a notification for owner connections, in the next client
protocol minor version. Guest connections are refused, as they are for other
daemon-wide methods.

`star.status` → `StarStatus`:

```json
{ "show": true, "login": "octocat" }
```

`show` is false when the card must not show. `login` is present when the
daemon can star; the page shows **Star as @login**, and otherwise the link.
The page asks once per connection, after the handshake, and only from a
daemon whose version has the method.

`star.answer { "answer": "star" | "opened" | "later" | "dismiss" }` →
`{ "starred": bool }`:

| Answer | Daemon |
|---|---|
| `star` | Stars with the token, writes `done: starred` whatever the outcome, and returns whether GitHub accepted. |
| `opened` | The page opened the link: writes `done: starred`. |
| `later` | Writes `snoozed_until`. |
| `dismiss` | Writes `done: dismissed`. |

The daemon then sends `star.changed` to every owner connection, so the card
closes on other open pages too. An answer after `done` is already set
changes nothing and returns `starred: false`.

## 6. Code

- `engine`, as `crates/engine/src/star.rs` beside `crons.rs`: `star.json` load and save, the 7-day rule, token lookup, the three GitHub
  calls, the 24-hour cache.
- `protocol`: `star.status`, `star.answer`, `star.changed`, version bump,
  schema.
- `client_state`: `daemon_has_star(version)`, matching `daemon_can_restart`.
- `web`: a `StarCard` component in `views/star.rs`, mounted in the
  workspace for owner connections (next to `InstallBanner` in `app.rs`),
  positioned over the transcript as in the preview.
- Docs: a line in [Web](../clients/web.md) saying what the card is, how
  to turn it off for good, and that the daemon uses an existing GitHub
  token for it.

## 7. Decisions (2026-11-27)

- Web only, not the TUI. (User's request.)
- A card over the transcript's corner, not a strip across the top of the
  workspace like the install and restart banners.
- The 7 days count from the daemon's first start after this ships. Nobody
  else uses cookie yet, so existing installs need no backfill from session
  history.
- **Later** hides the card for 7 days. **Star** and **Don't remind me** end
  it for good, and so does finding that the account already starred.

## 8. Implementation notes

- The card's × answers `later`, as **Later** does.
- `star.answer` with an answer the daemon does not know is `-32602`.
- Guests are refused both methods and never get `star.changed`, through
  the guest guard's default.
- The engine keeps the state in `crates/engine/src/star.rs`; the page's
  card is `crates/web/src/views/star.rs`, mounted in the transcript's
  container so it sits above the composer.

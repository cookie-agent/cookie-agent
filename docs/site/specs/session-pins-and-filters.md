# Session pins and the session list's filter

Status: **implemented** (2026-11-01; client protocol 1.66.0). Builds on
[unread sessions](unread-sessions.md) and the listing of
[session storage v3](session-storage-v3.md) section 4.3.

## 1. Goal and scope

The user asked for three changes on 2026-11-01:

1. Below a desktop's width the web page opens on its session list, as a
   phone already did: a foldable phone, unfolded, opened on a new session.
2. The web session list's header gains a row under **New session** with the
   select, search, and filter buttons; search turns the row into its
   search box, with an animation in and out; the filter chooses how far
   back the list reaches (the last 7 days by default), which agents and
   which machines its sessions run on. The date always lists the pinned
   sessions: running ones, those waiting on background work, those a cron
   job will wake, unread ones, and those the user pinned.
3. The user can pin a root session from its row's **⋮** in the web page,
   and in the TUI; a pinned session shows with the sessions at work in
   both clients and is listed whatever the date.

Out of scope: pinning subagents (a tree is pinned by its root, as it is
marked unread), a custom date range (the window is a number of days, as
the daemon's listing takes it), and a filter in the TUI, whose picker keeps
its 30 days, its directory scope, and its project filter.

## 2. Widths

The web page has three widths (`crates/web/src/browser.rs`): a phone below
40rem, a tablet from 40rem, a desktop from 64rem (1024 px). Only a desktop
has room for the rail, the sessions sidebar (18rem), and a transcript side
by side; the tablet width shows the sessions as the page in place of the
transcript. Viewports measured in CSS pixels for the devices this covers:

| Device | Portrait | Landscape | Opens on |
|---|---|---|---|
| A phone (iPhone 15, Pixel 8) | 390–412 wide | 844–915 wide | the sessions |
| A foldable unfolded (Galaxy Z Fold 5–7, Pixel Fold, Honor Magic V) | 690–884 wide | 830–1104 wide | the sessions, and a new session at 1024 and wider |
| A tablet (iPad mini to iPad Air, Galaxy Tab S) | 744–834 wide | 1024–1194 wide | the sessions; a new session in landscape |
| A laptop or desktop | | 1024 and wider | a new session, the sidebar pinned |

The page opened on its sessions only on a phone (`narrow`, below 40rem);
it now does whenever the window is narrower than a desktop, and a tap on
the rail's Chat button there shows the sessions too
(`View::rail_opens_sidebar`).

## 3. Pins

- **State.** The daemon keeps the pinned roots in `<data_root>/pinned.json`
  (`{"sessions": [{"session_id", "since"}]}`), written like `unread.json`:
  a temporary file renamed over it, without a sync. The two files share
  one implementation (`engine_store::session::marks`). Pinning writes
  nothing to the session log, so it moves neither `last_event_seq` nor the
  activity times; deleting a session forgets its pin.
- **Wire.** `SessionMeta.pinned` (left out when false) and
  `session.pin.set { session_ids, pinned }` (1 to 256 roots), answered with
  `changed` and `skipped` as `session.unread.set` answers; each change is
  announced by `sessions.changed`.
- **Listing.** A listing with a window holds every pinned root whatever its
  age, as it holds unread and busy ones. Roots that an enabled cron job
  with a next fire time targets now count as busy for this too: a session
  that shows the cron clock in both clients' rows was left out once its
  last activity was older than the window.
- **Order.** Both clients list a pinned session in the first group, with
  the active ones (`client_state::rows::listed_first`).
  - **Web.** At the top of the list or of its project section. A folded
    section still shows its pinned sessions. A pin glyph before the title,
    and **Pin**/**Unpin** first in the row's **⋮**.
  - **TUI.** The `/sessions` picker groups its rows by day, so pinned
    sessions lead it under a **Pinned** heading, each with 📌 before its
    title. **Ctrl+T**, or the **Pin**/**Unpin** button on the picker's
    action row, pins or unpins the highlighted session, as Ctrl+K marks it
    read or unread.

## 4. The web list's header

- **Rows.** **New session** (and, with projects, **New project…**) on the
  first row. On the second: select, search, and filter, then what the
  filter shows ("Last 7 days", or "7 days · 2 filters" when agents,
  machines, or a workspace narrow it too). Select mode replaces both rows
  with its bar, as it replaced the first.
- **Search.** The search box stays in the page, clipped to the search
  button's place (measured as it opens and closes) and hidden. Opening it
  unclips it to the whole row (300 ms, ease-out) while the other buttons
  shrink and fade away from it and its × turns in; closing clips it back
  into the button as it fades, and the buttons return once it has mostly
  gone. Escape or × closes it and gives the focus back to the search
  button. With reduced motion, only opacity changes.
- **Filter.** A panel with the date (24 hours, 7 days, 30 days, 90 days,
  all time), the agents and the machines of the listed sessions as chips
  (none chosen: any), and the workspace for a daemon without projects
  (the select that stood under the buttons before). On a desktop it
  opens in place, a popover under the button that a click outside or
  Escape closes. Narrower, it drops down above the list, pushing it down.
  A dot on the filter button says the filter differs from its default;
  **Reset** goes back to it, **Done** closes it.
- **What applies where.** The date is the list's window
  (`client_state::session_window`), which the daemon answers with the
  pinned, unread, and busy sessions too; choosing another date reads the
  list again from the daemon, and **Show older** still adds a month at a
  time. The agents (`creation_selection.agent`) and the machines (as the
  row's badges name them, `client_state::badges::session_machines`) narrow
  what the list holds, pinned sessions included, and, like a search, hide
  the project sections they empty. The filter lives in the tab: a new page
  starts with 7 days.

## 5. Decisions

Made while implementing, for the user to revisit:

- The TUI keeps its 30-day window and gains no filter; the request named
  the web page.
- The date presets are fixed; no custom range.
- Agent and machine filters apply to pinned sessions too: only the date
  is exempted.
- The web's selection bar gains no Pin/Unpin; `session.pin.set` takes many
  sessions, so it can.

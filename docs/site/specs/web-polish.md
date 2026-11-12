# Web polish: a second pass after Quiet Ink

Status: **approved** (2026-11-12), being implemented in the phases of
[section 9](#9-phases). The user's decisions are in
[section 2](#2-decisions); [section 3](#3-questions) lists the questions the
draft raised, each answered as the draft assumed. The
[overview page](web-polish/preview.html) shows every change below beside a
live mockup, in both themes. Web client only: no client, plugin, or runtime
protocol change, no session history change, no `config.toml` change. Builds
on [Quiet Ink](quiet-ink.md), whose tokens, fonts, and decisions D1–D15
stand.

## 1. Goal and scope

On 2026-11-12 every view of the web client was photographed at desktop and
phone sizes in both themes: on a live daemon with a month of real sessions
(read-only), and in the scripted showcase (`scripts/web-screenshots.sh
--all`). Quiet Ink holds up; what is left falls into three kinds:

- **Bugs** (B1–B10, [section 4](#4-bugs)): things that are wrong, such as
  Escape not closing menus or the session list showing a different time from
  the one it orders by.
- **Consistency and density** (P1–P8, [section 5](#5-changes)): views that
  each lay themselves out differently, a sidebar that shows six sessions at
  900 px, a New session page that explains everything before the user can
  type, and monospace on names that are not code.
- **The effort control** (P9, [section 6](#6-the-effort-control)): one
  sliding pill for a model's variants and an ACP agent's effort, decided by
  the user.

Out of scope: the TUI (it gets none of these changes, though the shared
`client_state` functions this spec adds are written so it could), the
landing site and documentation theme, features, keys, and commands.

## 2. Decisions

**Decided 2026-11-12**, after the overview's first version and a preview of
three effort controls; Q1–Q12 of section 3 were approved the same day:

- **D1, the effort control is a sliding pill.** A draggable segmented
  control (look "C" of the three previewed: notched track, rising bars,
  sliding pill) chooses a native model's variant and an ACP agent's effort
  alike, at the foot of the model menu instead of inside each model's entry.
  [Section 6](#6-the-effort-control).
- **D2, the model chip shows the variant's name only.** "Opus 5.5 · High":
  the model's display name and the variant's, with no meter or bars.
- **D3, the spec came before the code.** The user asked for proposals with
  preview pages first, then this spec, then the implementation.

## 3. Questions

Each question lists the answer the draft assumed and the alternative the
overview also showed. **The user approved every assumed answer on
2026-11-12.**

| # | Question | Answer | Alternative |
|---|---|---|---|
| Q1 | The sidebar's views (P1) | Two columns, four rows | A collapsible "Workspace" group that remembers whether it is open |
| Q2 | A session row's second line (P1) | Only when its machine or folder differs from its project's usual ones | Never; machine and folder only in the tooltip |
| Q3 | New session (P2) | The composer first, under the heading "What should cookie work on?", with the chips of P2 | The runtime cards folded into one summary row above the composer; no heading |
| Q4 | The folder chip (P3) | The folder's name ("cookie_code") | The path shortened from the left ("…/Projects/cookie_code") |
| Q5 | The header's ⋮ (P4) | A menu anchored to ⋮ | The Session commands sheet, restyled |
| Q6 | One view header and list row (P5) | Yes, for every view | Per view, as today |
| Q7 | The agent editor (P5) | A **Form \| File** switch: a form for description, mode, models, and permissions; the raw file for the rest | The raw file only, in a better editor |
| Q8 | Machines (P5) | A heading per machine, its runtimes as rows; dragging still moves a runtime and ⋯ has "Move to…" | Cards, as today |
| Q9 | Settings (P6) | Settings takes over the sidebar while open, with "← Back to sessions" | The app sidebar stays; Settings' sections become tabs along the top |
| Q10 | The phone's composer (P7) | One chip, "claude · Opus 5.5 · Auto", opening one sheet | Three chips, only the active one labelled |
| Q11 | The phone's drawer (P7) | The views as one horizontally scrolling row of pills | The views behind one "Workspace" entry |
| Q12 | The pill's edge cases (P9) | Short names past five levels, a list past seven, a switch for two states, no warm colour for the user's own variants | Decided case by case |

## 4. Bugs

Each fix lands in phase 1 unless a change below replaces the code it would
touch (noted).

- **B1, Escape does not close sheets.** The shared `Sheet`
  (`crates/web/src/views.rs`, `fn Sheet`) has no key handler, so the agent,
  model, and mode menus, the Session commands sheet, and the session panel
  close only with × or a click outside, even with focus on their chip.
  Quiet Ink's D15 says menus close on Escape. Fix: the sheet takes Escape
  through the `Transient` layer that row menus and context menus already
  use (`workspace/nav.rs`), so Escape and Back close the innermost layer
  alike.
- **B2, the focus ring on composer chips is clipped.** The chip row clips
  overflow, so the 2 px outline at a 2 px offset shows as "( Auto )". Fix:
  the composer's chips draw an inset ring (`outline-offset: -2px`).
- **B3, the phone's New session hides its help text.** At 390 px the
  paragraph's first line sits under the Project select. Replaced by P2,
  which removes the paragraph; fixed in phase 1 anyway if P2 lands later.
- **B4, the phone composer's chips run under the send button.** Replaced by
  P7's single chip.
- **B5, Settings shows a configuration warning as raw text.** Diagnostics
  render with literal backticks in the accent colour, which reads as a link.
  Fix: diagnostics render their inline code and sit in an amber callout
  (P6), with a fix-it action where `config.apply` can remove or correct the
  key.
- **B6, tool headers show the command's backticks.** ACP agents title a
  call "Run \`cmd\`". Fix: a tool header's title renders inline code spans;
  P3 then styles the verb and argument apart.
- **B7, a session row's hover actions cover its title.** On hover, ⋮,
  rename, and delete sit on top of the title and the machine badge. Replaced
  by P1: hover swaps the row's time for ⋯, and right-click opens the context
  menu at the pointer (D15).
- **B8, the session list orders by one time and shows another.**
  `client_state::rows::order_sessions` orders by `ordering_activity` (the
  user's last input or delegation), but the row shows `last_activity`
  (`views.rs`, the row's `relative_time`), so "1d ago" sits above "22h ago".
  Fix: the row shows `ordering_activity`, and its tooltip keeps the last
  event's time ("last event 2 min ago").
- **B9, a tall blank block around "earlier events not loaded".** The loader
  row sits in about 200 px of empty space. Fix: one centred pill with the
  transcript's normal spacing (P3).
- **B10, the session panel's tabs wrap.** Seven tabs in the 380 px panel
  leave "Events" alone on a second line at 1440 px. Fixed by P4's tabs.

## 5. Changes

### P1. Sidebar

- The views (Agents, Skills, Memory, Assets, Machines, Schedules, MCP,
  Sharing) in **two columns** (Q1); "MCP servers" is labelled "MCP" in the
  sidebar only.
- **Search sits beside New session** as an icon button; the Sessions
  heading keeps select, filter, and new folder. The active filter shows as
  a chip ("7 days ▾") instead of truncated text.
- **One-line rows.** Title and a short time ("now", "22h", "1d"; the full
  date in the tooltip). The second line appears only when the session's
  machine or folder differs from the project's usual ones (Q2): the
  machine most of the project's sessions use and the project's workspace.
  A session without a project always has a second line.
- **State as a dot** left of the title: pulsing accent while running, solid
  accent when unread, amber while an approval waits. The spinner and the
  screen-reader-only state text stay for assistive technology.
- Read sessions draw their title in `fg-2`, unread ones in `fg` and
  semibold.
- **Hover swaps the time for ⋯** (B7). Right-click opens the same menu at
  the pointer; a long press on touch keeps the flat action layout of
  `b1ff06691`.

### P2. New session

- The page centres a heading (Q3) and the composer, as the session view's
  composer looks.
- Under the composer, one row of chips, each showing its current value:
  **project**, **runs on** ("yunhao-labserver · global + coding"),
  **folder** (the coding runtime's workspace, mono), and **tools & skills**
  ("31 tools · 2 skills").
- **Runs on** opens a popover with today's runtime cards grouped by
  machine, as rows with a switch; a coding runtime's row holds its folder
  field. Today's paragraph becomes one sentence at the popover's top.
- **Tools & skills** opens a popover: tool groups as tags, filled when
  offered up front and dashed when deferred, with that legend shown; skills
  as rows with switches. Clicking a tag cycles it as the runtime card's
  group controls do today.
- The Runtimes | Skills switch goes.
- On a phone the composer sits at the bottom and the chips scroll in one
  row above it (P7).

### P3. Session view

- **Header.** The project as a crumb before the title ("🍪 cookie-agent ›
  title"), as Quiet Ink's section 4 specified; it opens the project's
  sessions. Badges read "1 running" and "1 approval" at desktop widths and
  stay icon and count on a phone.
- **Tool headers** read verb (sans, medium), argument (mono, one line,
  ellipsis), and result on the right: exit code, `+n −n`, match count,
  duration. A failed call's result is red and its card's ring a faint red,
  so it shows in a closed fold. B6 lands first.
- **A fade under the composer**: 120 px from transparent to the page's
  colour behind the floating composer, so text no longer shows around it.
- **Composer chips shrink in order**: the folder chip first (name only,
  Q4; the full path in its tooltip and menu), then the mode chip to its
  icon. The send button never covers a chip. The agent chip drops "· on
  local" when the agent can run in one place only.
- **Earlier history** is one pill, "21 earlier events · Load" (B9); the
  pinned rows (system prompt, tools, memory) become one row of small pills.

### P4. Menus and the session panel

- **Chip menus open above their chip**, so the chip stays visible; names in
  the sans; IDs in the tooltip. The model menu gets a search field and, at
  its foot, the effort pill of P9 (D1).
- **⋮ opens a menu** (Q5): Rename, Share, Schedule, Agent preset; then
  "Open in the session panel" with live values (context and cost, runtimes,
  background calls); then the agent's own commands. Typing filters it, as
  the sheet's field does today.
- **The panel's tabs** show an icon each, with the label only on the active
  tab and counts as small badges, so seven fit on one line (B10). The Usage
  tab leads with three tiles (context, cost, cache hits) above its tables.
- Escape, a click outside, and Back close each of them (B1).

### P5. Workspace views

- **One view header** for Agents, Skills, Memory, Assets, Sharing,
  Machines, MCP servers, and Schedules: the title as the sidebar names it
  ("Machines", not "Machines & runtimes"; "Schedules", not "Scheduled
  prompts"), one sentence on what the view is for, and actions on the right
  with the primary one last. One content width, 880 px.
- **One list row**: an icon, the name in the sans with tags, one line of
  description, quiet meta on the right, and ⋯ on hover and right-click.
  Agents' Customize | Turn off | Replace buttons and Memory's always-shown
  trash move into ⋯.
- **Search and a segmented filter** under the header where a list grows:
  Agents (All, Primary, Subagents, ACP), Memory (All, You, Agents,
  Projects, Machines), Skills, Assets.
- **Agents** group into Yours, Official, and ACP agents; each row gives the
  description and the model it uses. The editor opens full width with a
  **Form | File** switch (Q7): the form edits description, mode, models
  (with fallbacks, each with its effort pill), and permissions, and writes
  the same document the file view shows; anything the form does not cover
  stays as written.
- **Machines** (Q8): a heading per machine (name, platform, how it
  connects, memories, ⋯), and its runtimes as rows (state dot, kind tag,
  status, sessions). The ACP agents a runtime offers move into its row's
  details. Dragging a row onto another machine's heading still moves it.
- **Schedules**: the schedule in words, the cron line in quiet mono, a
  switch per row, and the last runs below the list.
- **MCP servers**: one row per server with its state as a tag, the URL or
  command in mono, the tool count, and Reconnect.

### P6. Settings

- **Settings takes over the sidebar** while open (Q9): "← Back to
  sessions", then its sections; the connection and version at the foot.
  Back (D14) leaves Settings as the link does.
- **Each row**: label and description on the left with the faint config key
  under them, one control on the right. A model is one picker showing the
  model's and the variant's display names ("Claude Opus 5.5 · High") that
  opens the model menu with its pill, and a reset icon when the value
  differs from the default.
- **Fields show their real value**; a placeholder reads "not set" or
  "30m · default" in the faint colour.
- **Diagnostics** are amber callouts with rendered code (B5).

### P7. Phone

- **One composer chip** (Q10), "claude · Opus 5.5 · Auto", opening **one
  bottom sheet** with agent, model, the effort pill (at 44 px), permission
  mode, and folder. Nothing sits under the send button (B4).
- **Drawer**: New session as a full-width button; the views as one
  horizontally scrolling row of pills (Q11); the sessions take the rest,
  44 px a row.
- **New session**: the composer at the bottom, P2's chips in one scrolling
  row above it.
- Header badges show icon and count only; the sidebar button carries the
  unread dot.

### P8. Type and details

- **Monospace only for what the user would type**: code, commands, paths,
  config keys, IDs. Names (agents, memories, runtimes, permission modes,
  models) use the sans; this covers Agents, the mode menu, runtime cards,
  Memory, the New session footer, and Settings' model values.
- **Display names over IDs** wherever both exist; the ID goes to the tooltip
  and editors.
- **One meaning per tag colour**: accent for a kind (primary), `ok` for a
  healthy state (ready, connected), `warn` for something to look at
  (customized, waiting), neutral for the rest.
- **Times** are short and relative in lists and absolute in tooltips.

## 6. The effort control

**What it chooses.** For a native model, its variants: the model's
`variants` in its `variant_order`, with `default_variant` marked. For an
ACP agent, its effort option's values in the order the agent sends them.
The same component serves the model menu (desktop), the phone's composer
sheet, New session, Settings' model rows, the agent editor's models, and a
schedule's model.

**The form it takes**, chosen by a pure function in `client_state` (so the
TUI could use it later) from the levels on offer:

| Levels | Form |
|---|---|
| None (no variants, or no effort option) | Nothing: the menu's foot is hidden and the chip names the model only |
| Two, from a models.dev toggle (`off`, `on`) | A switch, "Thinking on" |
| 2 to 5 | The pill, with each level's display name |
| 6 or 7 | The pill, with short names (Minimal → Min, Medium → Med, X-High → XHi; a display name of up to four characters stays as it is), and the full name in the control's header |
| 8 or more | A list with a check on the current level |

**How it looks.** A rail in `sunken` with one segment per level and a
raised pill under the current one. Above it: the control's name ("Effort"
for ACP agents and effort variants, "Thinking budget" for budget variants,
"Variant" for the user's own), the current level, "default …", and Reset
once the level differs from the default. Below it: the level's description
when there is one (the ACP option's value description; cookie's own text
for models.dev efforts), and a quiet "Faster · cheaper" and "Deeper ·
slower" at the ends. A budget variant shows its token budget beside its
name ("Medium · 16K").

**Colour.** The pill runs from the accent toward `hot` as the level rises,
mixed in OKLCH so the middle stays vivid; the user's own (`explicit`)
variants keep the accent throughout, since cookie cannot know they are
ordered (Q12).

**Behaviour.** Dragging moves the pill and snaps it to the nearest segment;
clicking a segment jumps to it; the arrow keys step, Home and End go to the
ends. It is one `role="slider"` with `aria-valuetext` set to the level's
name. On a phone the segments are 44 px tall and `navigator.vibrate(4)`
ticks at each new segment where the browser offers it. Motion is 300 ms
with the page's spring curve, none under `prefers-reduced-motion`.

**Changing model** keeps the level when the new model or agent offers a
level with the same ID, and otherwise takes the new one's default.

**The chip** (D2) shows "model · variant" with display names, joined by
" · " (the ACP label's " • " changes to match); with no variant chosen and
no default, the model alone.

## 7. What does not change

The protocol, `config.toml`, session history, the reducer, every key and
command, what each view can do, the TUI, and Quiet Ink's tokens and
decisions. Tests keep the rule against pinning looks (`AGENTS.md`).

## 8. Tests

- `client_state`: the effort control's form for each row of the table in
  section 6, short names, and that changing model keeps or resets the level;
  the row time is `ordering_activity` (B8); the chip label's format (D2).
- Web unit tests (`scripts/test-wasm-client.sh`): Escape closes a sheet and
  the innermost `Transient` layer first (B1); diagnostics render code spans
  (B5); a tool title's code spans (B6).
- Browser scenarios (`scripts/web-e2e/scenarios.py`): the pill drags,
  clicks, and steps with the keyboard, and sets the session's variant; the
  phone's single chip opens its sheet; Settings' Back returns to the
  sessions. Scenarios that only pinned the old layout are shrunk or
  dropped, as the user allows for this suite.
- `scripts/web-screenshots.sh --all` runs to the end again:
  `showcase.py`'s `badges_and_titles` (line 593) clicks a selector the
  page no longer has.

## 9. Phases

Each phase is looked at in light and dark at desktop and phone sizes
(`scripts/web-screenshots.sh --all`) before its commit.

1. **Bugs**: B1, B2, B3, B5 (rendering), B6, B8, B9 as standalone fixes;
   `showcase.py` repaired.
2. **The effort control and menus**: P9, then P4 (chip menus, ⋮ menu, panel
   tabs; B10).
3. **Sidebar and session view**: P1 (B7) and P3.
4. **New session**: P2.
5. **Views and Settings**: P5, then P6 (B5's callout).
6. **Phone and type**: P7 (B4) and the P8 sweep.
7. **Pictures and docs**: the bundled screenshots, the install manifest's
   screenshots, and user docs that describe the views changed here.

## 10. Implementation notes

- **Phase 1 (2026-11-12).** B1: the sheet listens for Escape on the
  window; it leaves a key a field already took (a rename field cancels its
  edit) and a smaller overlay open over it (the `Transient` layer) alone,
  and gives the focus back to the chip that opened it. The composer's
  Escape no longer cancels the run while a sheet or menu is open, so
  Escape closes the menu instead. B2: the chips' ring is drawn inside them.
  B3 was not an overlap: after the page scrolled a little, the sticky
  Project row covered the paragraph's first line; P2 removes both, so it
  lands with P2. B5: diagnostics are callouts; the daemon's record lines
  (native compaction, deferred tools) are a quiet note under them, no
  longer in the accent colour. B5 and B6 share `format::code_spans` in
  `client_state`. B8: the row's time is `rows::ordering_activity`, its
  tooltip the last event's. B9: the row is 56 px whether it waits or reads
  (it was 144 px), so a page arriving still moves nothing. The showcase's
  long-press step clicks the flat Rename of `b1ff06691`.

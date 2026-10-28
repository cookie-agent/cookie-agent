# Unread sessions and selecting several sessions

Status: **approved** (2026-10-28), with the user's decisions of 2026-10-28 in
[section 9](#9-decisions). Builds on
[push notifications](push-notifications.md) (the settle rule, section 3,
and presence, section 8) and on session deletion
([web client](web-client.md), `session.delete`).

## 1. Goal and scope

**Today** a root session that settles while the user is away sends a push
notification, and nothing else marks it. Once the notification is gone, or
when notifications are off, the session lists do not say which sessions
have a turn the user has not seen.

**This spec:**

- marks a root session **unread** in the cases that send a push: it
  settled, or a run in its tree needs the user, while nobody was present on
  its tree (section 2);
- marks it **read** once a client is present on its tree, or when the user
  says so (section 3);
- shows a bubble on unread sessions in the TUI and the web (section 6);
- lets the web's session list select several root sessions and mark them
  read, mark them unread, or delete them (section 7).

Out of scope:

- unread state for delegated sessions: a subagent's turns reach its root,
  which is the one marked, as with pushes;
- a count of unread turns: a session is unread or it is not;
- selecting several sessions in the TUI (user decision): its picker marks
  one session at a time.

## 2. When a session becomes unread

A root session becomes unread when the engine publishes one of the push
signals for its tree (push notifications spec, section 5.1) and **no
connection reports presence on a session of its tree** (section 8 there).
These are the cases in which the daemon sends, or with notifications on
would send, a push:

- **`settled`** (section 3 there): a run of the session ended `completed`
  or `failed`, and nothing but recurring cron jobs can wake it. A
  `cancelled` run never marks it (someone stopped it, so someone was
  there), nor does an `interrupted` one.
- **`needs_you`** (section 4 there): an approval in the root or any
  subagent below it escalates to the user, or an ACP agent there asks a
  question (elicitation). The root is marked as the signal arrives, before
  the push gathers it with others; answering it elsewhere does not clear
  it, presence does.

It does not depend on `[notifications]`: with `enabled = false`,
`approvals = false`, or `cron_sessions = false` for a session a
new-session cron job created, no push goes out but the session is still
marked unread. The bubble is quiet; only the buzz is configurable.

## 3. When a session becomes read

- **Presence.** A client reports presence (`client.presence { session }`)
  on any session of the tree: the root becomes read. Presence is the rule
  every client already follows for pushes (`client_state::presence::shown`):
  the session on screen, in a window that is visible and focused, used in
  the last 2 minutes. So opening an unread session in a focused window
  marks it read; a session left open in a background tab, an unfocused
  window, or a terminal untouched for 2 minutes stays unread until the
  user comes back to it.
- **The user says so** (section 5): marking it read from a session list.
- **Deleting it** removes its state.

Marking a session unread by hand (section 5) is undone by the next presence
report on its tree, not by presence that was already reported: marking the
session on screen unread keeps it unread until the client next reports it
(switching away and back, focusing the window again, or coming back after
2 idle minutes).

## 4. Storage

The daemon keeps the unread roots in `<data_root>/unread.json`, written
atomically like `crons.json` and strict like configuration (unknown fields
are an error):

```json
{
  "sessions": [
    {"session_id": "019a…", "since": "2026-10-28T14:02:12Z"}
  ]
}
```

- **It survives daemon restarts** (user decision): a restart keeps every
  unread session unread.
- It is not the session log: reading a session writes no event, so it
  moves neither `last_event_seq` (which `session.delete`'s
  `expected_last_seq` checks) nor any activity time that orders the lists.
- A session that no longer exists at startup is dropped from the file.

## 5. Client protocol

Client protocol 1.51.0, additive:

- **`SessionMeta.unread`** (`bool`, left out when false): set on unread
  root sessions, never on delegated ones. False from an older daemon.
- **`sessions.changed` `upserted`** is sent for a session whose `unread`
  changed, as for its other metadata.
- **`session.unread.set { session_ids, unread }`** marks the root sessions
  `session_ids` (1 to 256) unread or read. It answers `{ changed, skipped }`:
  `changed` lists the sessions whose state changed (each also announced by
  `sessions.changed`); `skipped` lists, with a `reason` code, the IDs it
  left: `session_not_found`, or `not_a_root_session` with its
  `root_session_id`. Marking a session that already is in that state
  changes nothing and skips nothing. A daemon older than 1.51.0 answers
  `-32601`; clients then hide the actions and the bubble.

No plugin or runtime protocol change: plugins see sessions through events,
and this writes none.

## 6. The bubble

- **Web:** an unread root's row in the sidebar (and the phone's sessions
  page) shows a small dot in the accent colour before its title, and its
  title in bold. The dot's tooltip says "Unread: finished while you were
  away". A folded project section shows its unread sessions, as it shows
  its sessions at work. The rail's Chat icon carries a dot while any
  session is unread.
- **TUI:** an unread session's row in the session picker shows `●` before
  its title. The picker's action row gains **Mark read** / **Mark unread**
  for the highlighted session (it says which it will do), with a key beside
  Ctrl+R and Ctrl+D chosen at implementation; the button hovers as every
  clickable does.
- Both clients take the state from `SessionMeta.unread` only: a client
  never clears it locally; presence and `session.unread.set` do, through
  the daemon, so every client agrees.

## 7. Selecting several sessions on the web

- **Select.** The session list's header gains a **Select** button. In
  select mode each root row shows a checkbox (subagent rows none) and a
  click toggles it instead of opening the session; Shift+click selects the
  range from the last toggled row; a phone's long press on a row starts
  select mode with that row. **Select all** selects every root row the list
  shows (a folded section's shown rows included, the rest not). Escape or
  **Done** leaves select mode and clears the selection.
- **The selection bar** replaces the list's header while anything is
  selected: the count, **Mark read**, **Mark unread**, **Delete**, and
  **Done**.
- **Mark read / Mark unread** send one `session.unread.set` with the
  selection and keep it selected.
- **Delete** asks once: "Delete N sessions and their subagents? This
  cannot be undone." with the titles of up to five and "and N more". On
  confirm the page sends `session.delete` for each root, with the
  `last_event_seq` the row showed when the user confirmed, a few at a time.
  A tree that cannot be deleted is **skipped** and the rest are still
  deleted (user decision): running (`session_tree_running`), changed since
  it was selected (`session_changed`), or owned by another cookie process
  (`session_owned_elsewhere`). Afterwards the status says how many were
  deleted and names the skipped ones with why, and the skipped ones stay
  selected.
- Single-session actions (the row's trash and pencil) stay as they are.

## 8. Tests and phases

Tests:

- engine/server: a settle with no presence marks the root unread and
  announces it; a settle with presence on the root or a subagent does not;
  an escalated approval or an agent question in a subagent marks its root,
  without presence, as it arrives;
  a cancelled run never marks; presence on any session of the tree clears
  it; `[notifications] enabled = false` still marks; the state survives a
  daemon restart; a deleted session leaves the file; `session.unread.set`
  marks, unmarks, skips unknown and delegated IDs, and is idempotent;
- client state: `unread` reduces from `upserted`;
- TUI: the picker shows `●`, and its action marks read and unread;
- web e2e: a session that settles while the page is hidden shows the dot,
  which clears when the page shows it; select mode, range selection, mark
  read and unread, and a batch delete that skips a running session and
  deletes the rest.

Phases:

1. Daemon: the store, marking on settle, clearing on presence,
   `SessionMeta.unread`, and `session.unread.set` (client protocol
   1.51.0), with the protocol reference and changelog.
2. Clients: the bubble in both clients and the TUI picker's action.
3. Web: select mode and the batch actions.

## 9. Decisions

The user decided on 2026-10-28:

- an unread bubble for sessions whose finished turn the user has not seen,
  cleared once it is read; the web's session list selects several sessions
  to mark read, mark unread, or delete; no multi-select in the TUI;
- **read means presence**: a client showing the session in a visible,
  focused window used in the last 2 minutes, the rule that suppresses
  pushes, not merely having it open;
- unread state **survives daemon restarts**;
- a batch delete **skips running sessions** and deletes the rest, and a
  session changed since it was selected or owned by another process is
  skipped the same way;
- an approval or an agent question waiting for the user, while nobody is
  present, **marks the session unread** too;
- unread does not follow `[notifications]` (section 2): a session that
  sent no push because notifications, approval pushes, or cron-session
  pushes are off is still marked unread.

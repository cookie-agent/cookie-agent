# Responsive session loading and session storage v3

Status: **approved** (2026-10-31), with the user's decisions of
2026-10-31 in [section 7](#7-decisions); phase 1 in progress.

## 1. Problem

Opening a long session in the web client sometimes fails with
`subscription replay response timed out`, most often when the daemon's
machine is busy, and more often as sessions pile up.

How a transcript loads today:

- The client pages `events.subscribe` forward from the start of the log,
  at most 2,000 events or 4 MiB (`REPLAY_PAGE_MAX_BYTES`) a page, and each
  page must arrive completely within 5 s (`REPLAY_RESPONSE_TIMEOUT`,
  `crates/protocol_transport/src/client.rs`). The 5 s covers the
  daemon's work *and* the transfer.
- The client holds every page and shows the transcript only after the last
  one arrives.
- Before answering the first page the daemon completes the tree
  (`ensure_tree_for` → `load_tree`): it parses and folds the root's log and
  every child log under `subagents/`, one after another. A root idle for
  `session_idle_eviction_after` (30 min) pays this again.
- The WebSocket carries uncompressed JSON.

Measured on 2026-10-31 on yunhao-labserver, on copies of real sessions
(95 roots, 189 logs, 250 MB of logs in total):

| Measurement | Value |
|---|---|
| Largest log | 16.9 MB on disk, ~19 MB of JSON on the wire |
| Of its bytes: `tool_call_terminated` | 50% (p50 3 KB, p99 117 KB, max 640 KB per event) |
| `acp_session_updated` | 29%: each repeats the full option and command lists |
| Bookkeeping (`model_attempt_started`, `run_started` model settings, …) | ~8% |
| First page, idle machine | 0.21–0.26 s; later pages 20–90 ms |
| First page, 2 cores shared with 8 busy loops | 1.1–1.2 s; later pages 30–140 ms |
| gzip -1 / zstd -3 of that log | 3.4× / 4.1× smaller |
| Display-only view (section 3.1) | 5.3 MB, 31% of the log; ~1.6 MB gzipped |
| Last 500 display events | 810 KB before compression |

The daemon's CPU alone stays under 5 s even under contention. A 4 MiB page
needs more than ~7 Mbit/s to arrive in 5 s, which a tunnelled or wireless
link often does not have. Disk contention, which makes the cold first read
slower, was not measured.

## 2. Phase 1: no change to storage or history

### 2.1 WebSocket compression

The daemon accepts the standard `permessage-deflate` extension (RFC 7692)
on `/ws`. Every browser offers it on every WebSocket, so the web client
needs no change and the protocol no bump; every message is compressed,
live events included.

tungstenite (0.29, and 0.30, the newest) has no `permessage-deflate`, and
axum's WebSocket support is built on it, so `/ws` moves to a library that
has it. Measured on 2026-10-31 with a scratch `yawc` 0.5 server
(`with_balanced_compression`): Node 22 and Chrome 156 both negotiated
`permessage-deflate`, and a 4.13 MB page of real events crossed the
connection as 512 KB (8.1×).

The Cloudflare tunnel is not tested (waived on 2026-10-31). If a tunnel
drops the extension, the browser and daemon fall back to uncompressed
without an error.

Fallback, should `yawc` not work out: compression in the application. A
client lists
`compressed_frames` in `handshake`'s `features`; to it the daemon sends
any message over 16 KiB as a binary WebSocket message holding the gzip of
the JSON text, which the web client decompresses with
`DecompressionStream`. That is a client protocol minor bump.

### 2.2 Third-party notices

Moving `/ws` to `yawc` brings the first MPL-2.0 dependency. MPL-2.0 is
file-level copyleft: cookie's own license is unaffected, and a binary
release must say where `yawc`'s source is. `deny.toml` allows MPL-2.0 for
`yawc` only, like CC0-1.0 for `base16`.

The release ships a generated third-party notices file listing every
crate in the binary with its version, license, and source, which the MIT
and Apache-2.0 dependencies already require. The web client's and the
TUI's About views show cookie's license and these notices.

### 2.3 Pages and the timeout

- `REPLAY_PAGE_MAX_BYTES` drops from 4 MiB to 512 KiB, measured before
  compression.
- `REPLAY_RESPONSE_TIMEOUT` becomes 15 s per page. With 512 KiB pages that
  fails only below ~35 KB/s, against ~840 KB/s needed today.

### 2.4 The first page without the tree

`events.subscribe` on a root answers from the root's own log
(`get_log_only`). The tree load starts in the background, and anything
that needs the tree (`session.tree`, `session.children`, approvals of the
tree, a run) still waits for it as today. Implementation must check that
nothing the transcript shows depends on tree-load products (review L5 in
`SessionStore::get`).

### 2.5 Rendering as pages arrive

Folded into phase 2 (decided 2026-10-31). The client's replay contract
names the replay's final sequence in `ReplayStart` and rebuilds the view
at `ReplayEnd`, so rendering forward pages as they arrive would rework it
only for 3.2's newest-first loading to replace it. With 2.1, 2.3, and 2.4,
a replay no longer times out; 3.2 makes it show at once.

## 3. Phase 2: a display view

### 3.1 `view: "display"`

`events.subscribe` takes `view: "full" | "display"` (default `full`; the
TUI and web client send `display`). A display page:

- leaves out bookkeeping events: `model_attempt_started`,
  `model_tools_published`, `model_request_prepared`,
  `model_replay_evaluated`, `model_output_started`, `model_usage_recorded`;
- keeps only the latest `acp_session_updated` (in the header, 3.2);
- replaces a tool result's output over 4 KiB with its first 2 KiB, its
  byte length, and a reference the client reads in full when the user
  expands the row (phase 2: `event.read {session_id, seq}`, which returns
  the whole stored event; phase 3: `artifact.read` on the blob).

Before implementing it, check every event type `client_state`'s reducer
reads against this list. An event the reducer needs stays in the view,
or moves into the header.

### 3.2 The newest messages first

The first display request names `from: "tail"`. The answer holds:

- a **view header**: the session's current state that is latest-wins
  (title, agent, model and variant, ACP options and commands, permission
  mode, goal, usage totals), applied by the client *after* folding the
  page, so a setting from an older event never overrides it;
- the newest display events within a byte budget (default 256 KiB). The
  window starts at a run boundary (`user_input_submitted` or
  `run_started`). A run larger than the budget sends its first event, a
  gap marker naming how many events it skips, and its newest events;
- `older_cursor`, the seq before the window, or `null` at the start.

The live tail is registered with this first answer. Older history comes
from `events.page {session_id, before, view}`, which registers no tail, as
the user scrolls up. The client re-folds the events it holds when an older
page arrives: display events are small, so this is cheaper than a reducer
that can prepend.

Client protocol: a minor bump (`view`, `from`, the header, `events.page`,
`event.read`).

## 4. Phase 3: storage layout v3

### 4.1 Layout

```text
<data_root>/sessions/
  layout.json                 {"version": 3, "history_format": 2, "pending": []}
  activity/
    2026-10.jsonl             append-only: a root's list entry each time it changes this month (4.3)
  runtime-revisions.jsonl     moved up from the work-dir folder
  2026-10/                    UTC month of the root's UUIDv7 id
    <root-id>/
      metadata                unchanged
      owner.lock              unchanged
      events.jsonl            history format 2 (4.2)
      view.json               the view header of 3.2 at a seq, and the byte offsets of recent run starts
      blobs/<sha256>          replaces artifacts/
      subagents/
        index.json            also a summary per child, so the tree view reads no child log
        <child-id>/           the same shape; filed under the root, whatever its own month
```

- **No work-dir key.** The daemon opens one store, keyed today by its own
  launch directory, so sessions under other keys are invisible to it. Each
  session already records its own working directory, and projects group
  sessions.
- **The month comes from the id.** A root's folder is
  `<yyyy-mm>/<id>`, from the UUIDv7 timestamp, so resolving a root needs
  no lookup. A child is created after its root, so locating an unknown
  child scans roots from the child's month backwards and almost always
  stops in the first month.
- **Folders by creation, listing by activity.** A session's folder never
  moves: an old session resumed today stays in its creation month. What
  the session list needs, recent *activity*, comes from the activity
  journals (4.3), so listing reads neither the month folders nor any
  `metadata`.
- **`view.json`** lets a cold open answer 3.2 without parsing the log:
  read `view.json`, fold the few events after its seq if any, and read the
  window by seeking to a recorded run start. A view-only open does not
  make the session resident; a run still reads the whole log (the
  checkpoint resume rejected on 2026-10-14 stays rejected).

### 4.2 History format 2

The first real converter, `v1_to_v2` (`crates/engine_store/src/events/convert`):

- a tool result's output over 8 KiB moves to `blobs/<sha256>`; the event
  keeps the first 2 KiB, the length, and the digest;
- `acp_session_updated` records only the fields that changed since the
  previous one; option and command lists go to `blobs/` by digest;
- `run_started`'s frozen model settings chains go to `blobs/` by digest.

Expected effect: logs about 60% smaller, so every parse is faster,
including a resume for a run.

### 4.3 Listing recent sessions

Each `activity/<yyyy-mm>.jsonl` is an append-only journal of root
sessions' list entries (`SessionMeta`, ~1.6 KB) in the month the entry
was written (UTC):

- **Writes.** The daemon appends a root's entry when it is created, when
  its title, project, or status changes (a run starts or ends, it is held
  or resumed), and a tombstone (`{"session_id", "deleted": true}`) when it
  is deleted. Never on every event: `last_activity` is the time of the
  newest entry. Subagents have no entries; they come from the tree.
- **Reads.** A listing reads journals newest month first. Within a file
  the last line for a session wins; across files, a session already seen
  in a newer month is skipped, as is a tombstoned one. A window of the
  last 30 days reads at most the current and the previous month's
  journals, however many sessions the store holds.
- **Compaction.** The first write of a new month rewrites the previous
  month's journal once, keeping each session's last line (temp file and
  rename). Older journals are never written again.
- **In memory.** The daemon keeps the entries of the current and the
  previous month, so a default listing does no IO at all.

**Sessions that need the user are always listed.** Whatever the window,
a listing also holds every root that is:

- **unread** (`unread.json`, [unread sessions](unread-sessions.md)), or
- **busy**: its tree has a run going, an approval or an agent's question
  waiting, a background call running, or a subagent that can still wake
  it. These trees are resident (the idle janitor never evicts them), so
  the daemon knows them without IO.

A pinned root missing from the journals read so far is read from its own
`metadata`, whose path follows from its id: the IO is one small file per
pinned session, never a scan. A root that becomes unread or busy reaches
connected clients the way any session change does today, and a client
keeps it listed, outside its window or not, until it is neither. The
clients order the list as today: unread and busy sessions first.

`session.list` gains `active_since` (a timestamp) and `before` (a cursor
the previous answer returned as `older`, or `null` once nothing is
older). The web client and the TUI ask for the last 30 days and offer
"Show older", which reads one more month at a time. Without
`active_since` the answer holds every session, as today. Client
protocol: a minor bump.

Rejected: filing folders by last activity, which would move the folders
of open sessions; per-day folders, which change nothing for listing; and
a SQLite index, a new dependency and file format for a query the journals
answer by reading two small files.

### 4.4 A newer store

A daemon refuses to open a store whose `layout.json` version or
`history_format` is newer than it knows, and says which build it needs.

## 5. Migration

At startup, before it accepts connections, a daemon reads
`sessions/layout.json`:

- **Current** (`version` 3, `history_format` this build's, `pending`
  empty): nothing more. Later startups never scan sessions.
- **Older or missing:** the daemon migrates the whole store once, logging
  progress:
  1. Check free space for the converted logs (about the logs' size) and
     refuse with a clear message if it is short.
  2. For each root folder under every old work-dir key: take the tree's
     lock, convert every log in the tree into staged files and sync them
     (`convert_tree`), move the originals to `sessions/.backup-v2/<root-id>/`,
     put the staged logs in place, and rename the root folder to
     `<yyyy-mm>/<root-id>`. Each tree is one step; a crash between steps
     leaves every tree either old or done, and the next start continues.
  3. Write each root's entry, from its `metadata`, into the journal of
     the month of its `last_activity`, and move
     `runtime-revisions-v8.jsonl` up.
  4. Write `layout.json` with fsync and a directory fsync. This is the
     commit point.
  5. Delete `sessions/.backup-v2/` and the emptied work-dir folders.
- **A tree that fails to convert** (a corrupt log, an unknown record the
  converter cannot copy) stays untouched in its old folder and is listed
  in `pending` with the error. Every startup retries the pending trees,
  and the startup log and the configuration diagnostics name them. They
  are not listed until they convert.
- **Ids that are not UUIDv7**, if any, take their month from `metadata`'s
  creation time.

Estimated one-time cost for 250 MB of logs: about 5 s on an idle machine
and 20–30 s on a busy one. The web client reconnects by itself (its
backoff tops out at 10 s). Later format bumps use the same marker: one
migration at startup, then the marker moves up. Converters are never
removed, so an older store upgrades step by step in one pass.

## 6. Order of work

1. Phase 1 (2.1–2.4), one change each, measured against section 1's
   numbers on a scratch daemon with copies of real sessions.
2. Phase 2: the reducer audit of 3.1, then `view: "display"`, then
   `from: "tail"` and `events.page`.
3. Phase 3: the layout and migration (4.1, 5) with layout version 3 and
   history format 1, then history format 2 (4.2) through the same marker,
   then `view.json`.

## 7. Decisions

User decisions of 2026-10-31:

1. Phases 1 and 2 as proposed.
2. No work-dir key; roots filed by `yyyy-mm`.
3. Migrate the layout and convert every log once, eagerly, at daemon
   startup, and persist a store-wide marker so later startups skip the
   scan; no lazy background sweep.
4. Older binaries writing into a migrated store are not handled: the user
   is the only user before release, and every released daemon reads
   `layout.json` (4.4).
5. Originals go to `sessions/.backup-v2/` and are deleted once the
   migration completes.
6. Clients list the last 30 days by default from monthly activity
   journals; unread, running, and waiting sessions show on top whatever
   their age (4.3).
7. `yawc` for `permessage-deflate`, with an MPL-2.0 exception for it
   alone, a generated third-party notices file in releases, and the
   licenses in the web and TUI About views (2.1, 2.2). The Cloudflare
   tunnel test is waived.
8. Rendering pages as they arrive (2.5) is folded into phase 2's
   newest-first loading.

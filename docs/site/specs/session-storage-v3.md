# Responsive session loading and session storage v3

Status: **approved** (2026-10-31), with the user's decisions of
2026-10-31 in [section 7](#7-decisions); phases 1 and 2 implemented
(phase 2: client protocol 1.59.0, [3.4](#34-implementation-notes-2026-10-31));
phase 3's layout, listing, and migration (4.1, 4.3, 4.4, 5) implemented with
history format 1 ([section 5.1](#51-implementation-notes-2026-10-31)), then
history format 2 and `blobs/` (4.2) through the same marker
([section 4.2.1](#421-implementation-notes-2026-11-01)); `view.json`, a
newest-first open without the tree, and phase 2's wire leftovers (client
protocol 1.64.0) implemented
([section 4.5](#45-implementation-notes-viewjson-2026-11-01)).

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
connection as 512 KB (8.1×). The daemon compresses at deflate's fastest
level instead (2.6): the default level cost too much CPU on a busy machine.

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

- `REPLAY_PAGE_MAX_BYTES` drops from 4 MiB to 1 MiB, measured before
  compression.
- `REPLAY_RESPONSE_TIMEOUT` becomes 15 s per page. With 1 MiB pages that
  fails only below ~70 KB/s before compression, against ~840 KB/s needed
  today.

Measured on 2026-10-31 (implementation notes, 2.6): 512 KiB pages made a
long replay mostly round trips on a busy machine, and 2 MiB pages gained
little over 1 MiB.

### 2.4 The first page without the tree

`events.subscribe` answers every page but the final one from the
session's own log (`SessionStore::history_page`): the resident log, or the
parsed read-only log, which a later open for writing takes over while the
file is unchanged. Such a page completes no tree and opens nothing for
writing, so it never waits on the session's actor either. The tree load
starts on a blocking thread from a replay's first page, and the final
page, which registers the live tail and so opens the session as before,
waits for it there, off the async workers. Anything else that needs the
tree (`session.tree`, `session.children`, approvals of the tree, a run)
waits for it as today. History pages are the session's own events, so no
tree-load product can be missing from them (review L5 in
`SessionStore::get` concerns cached summaries, not logs).

### 2.5 Rendering as pages arrive

Folded into phase 2 (decided 2026-10-31). The client's replay contract
names the replay's final sequence in `ReplayStart` and rebuilds the view
at `ReplayEnd`, so rendering forward pages as they arrive would rework it
only for 3.2's newest-first loading to replace it. With 2.1, 2.3, and 2.4,
a replay no longer times out; 3.2 makes it show at once.

### 2.6 Implementation notes (2026-10-31)

Measured with a scratch daemon on copies of three real sessions, pinned to
two cores shared with eight busy loops, paging as the protocol client does.
Single runs on starved cores, so the times are rough:

| Build | Bytes sent | 16.9 MB log, whole replay | 12-subagent tree, first page | 20-subagent tree, first page |
|---|---|---|---|---|
| Before (main) | 26.8 MB | 1.6 s, 6 pages | 971 ms | 1,398 ms |
| 512 KiB pages, deflate level 6 | 5.96 MB | 3.9 s, 37 pages | 334 ms | 193 ms |
| 1 MiB pages, level 6 | 5.95 MB | 3.3 s, 19 pages | 441 ms | 210 ms |
| 2 MiB pages, level 6 | 5.95 MB | 3.4 s, 10 pages | 622 ms | 363 ms |
| **1 MiB pages, level 1 (shipped)** | **7.8 MB** | **2.0 s, 19 pages** | **339 ms** | **139 ms** |

Compression at the default level was most of the extra time on a local
connection; at the fastest level it still sends 3.4 times fewer bytes,
which is what a tunnel or a phone link pays for. The first page of a log
without subagents still parses that whole log (about 1 s here); phase 3's
`view.json` removes that.

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

### 3.3 Reducer audit (2026-10-31)

What `client_state`'s reducer (`reduce.rs`) and the TUI and web
transcripts read of each event the display view would drop or slim:

| Event | Read by | Decision |
|---|---|---|
| `model_attempt_started` | Opens the reply block a commit fills, with its model attribution; a commit without it shows no reply | **Stays** |
| `model_output_started` | The streamed thinking time a reply's thinking rows show | **Stays** (1–2.5% of a log) |
| `model_usage_recorded` | Each reply's cost, and an ACP reply's rate and context, in its footer; the clients' cost refresh | **Stays** |
| `model_replay_evaluated` | Rows: replayed and reconstructed entries are debug, an empty list info, a discard a warning | **Left out unless it discards** (a warning row, shown by default) |
| `model_tools_published` | The latest one's tool lists (`model_tools`) | **Latest wins**: a page keeps its last; the header the last before it |
| `model_request_prepared` | Nothing | **Left out** |
| `acp_session_updated` | Latest wins: `commands`, `options`, `starting_options`, `context`. Per event: `plan`, `compaction`, `prompt_context`, `notice`, `session_message`, `background_ended`, `steered`, `tool_presentation` (rows), and `context` (a reply's footer reads the context use at its usage) | **Slimmed**: the header holds the latest of the four settings; in a page a command list or starting options a later update replaces, and a tool presentation a later one of the same call replaces, are dropped, and an update left empty goes |
| `tool_call_terminated` | `display`, `output` (a failed call's partial output, the loop warning at its end), `additional_messages`, `background`, `retained_output`; never `metadata` | **Cut** above 4 KiB of output, display, and metadata together |

Other events the reducer ignores entirely (`delegation_reserved`,
`tool_round_started`, `prompt_blocks_recorded`, `tool_call_linked`, ...)
stay: the spec drops only bookkeeping, and they are small in the logs
measured. The latest-wins state the reducer folds from older events, which
the header must carry for a window that starts later, is: the session's
creation, the latest run (its agent and snapshot) and whether it ended,
memory, published tools, the ACP session and its four settings, the model
selection, the goal and its changes, runtime attachments still attached, a
hold nothing cleared, tools announced since the latest compaction, and
inputs still queued. Title, permission mode, and usage totals come from
`session.list`/`session.get` and `session.usage`, not the reducer.

### 3.4 Implementation notes (2026-10-31)

Shipped as client protocol 1.59.0: `view`, `from: "tail"`, `max_bytes`,
the result's `window` and `cut`, `events.page`, and `event.read`
([events reference](../reference/events.md#newest-first)). The view and
window logic is `cookie_agent_protocol::display_view`, shared by the
daemon and the clients. Deviations from 3.1 and 3.2, and why:

- **What the view drops** follows the audit (3.3), not the list in 3.1:
  `model_attempt_started`, `model_output_started`, and
  `model_usage_recorded` stay; `model_replay_evaluated` goes unless it
  discards something; `model_tools_published` keeps the last of a page.
  Tool results also lose their `metadata`, which no client reads and an
  ACP agent fills with the whole output again.
- **The cut** applies when output, display, and metadata together pass
  4 KiB, not output alone: a result in the golden history has a 1.3 KB
  output and a 64 KB display. The display is fit to 2 KiB with
  `fit_display_budget`, so it keeps its `omitted_*` counts. The cut is
  named in a `cut` list beside the events (`seq`, the whole result's
  `bytes`) instead of a member of the stored event: the stored types, the
  history format, and the plugin protocol stay as they were.
- **The header is events, folded first**, not a state applied after the
  page: the latest event of each setting before the window, an
  `acp_session_updated` stripped to the settings it is the latest for. A
  client folds them into a scratch projection and seeds the transcript's
  projection with its latest-wins state, so no row of an old event shows
  and no older setting overrides a newer one; one reducer serves both, and
  live events and later folds stay in sequence order. An older page
  carries the header for its own start.
- **`older_cursor` is the first held sequence** (what `events.page` takes
  as `before`), `null` when the window starts the log.
- **The gap** keeps, besides the run's start through its first input and
  its newest model attempts, the settings and what still waits among the
  events it skips: approvals and questions not answered, inputs admitted
  but not applied, and tool calls still running. Older pages have no gap:
  each starts at the earliest run or model attempt that fits.
- **The tail** is cut from the whole log an unlimited final page returns,
  so its registration, its in-flight output, and its race with appends
  are the final page's; no store code changed. `events.page` and
  `event.read` read the resident or the parsed read-only log, without the
  tree.
- **Clients** hold the window's events, older pages, the live events
  after them (whole but for a cut result), and live-only output still in
  flight, and fold them again when a page or a whole result arrives. A
  fold after a page numbers its rows so the rows already shown keep their
  ids, and with them what the user expanded and where the TUI's view is
  anchored. A row of history not loaded loads when it scrolls into view
  or is clicked; the web keeps the rows the user reads in place itself,
  as the browser's scroll anchoring does not act at the view's top.

Measured on 2026-10-31 with a debug build of a scratch daemon on copies of
two real sessions, over a local WebSocket (sizes before compression):

| Session | Whole replay | Newest first | All older pages |
|---|---|---|---|
| ACP, 16.9 MB log, 9,813 events | 18.8 MB, 19 pages, 4.5 s | 249 KB of events + 49 KB header, 1 answer, 0.13 s | 47 pages, 11.6 MB, 6.1 s |
| Native, 5.3 MB log, 2,037 events | 5.5 MB, 6 pages, 1.3 s | 187 KB + 39 KB header, 0.09 s | 21 pages, 5.6 MB, 1.9 s |

`event.read` of a cut result took 3–5 ms.

Left for later:

- ~~An older page repeats its header (47 KB in the ACP session, 2.3 MB over
  all its pages).~~ Done in 1.64.0: a delta of the header the client holds
  ([4.5](#45-implementation-notes-viewjson-2026-11-01)).
- ~~A tool result's `display` repeats its output on the wire, so results
  under the cut still take about twice their output.~~ Done in 1.64.0:
  ranges of the output ([4.5](#45-implementation-notes-viewjson-2026-11-01)).
- A window may start at an input steered into a running run, whose run
  started before it: the reply rows before it are older history.
- Live events are sent whole; only what a client holds is cut.

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
- option and command lists in `acp_session_updated` go to `blobs/` by
  digest; ACP tool presentations and context use are no longer stored at
  all ([store locks and durability](store-locks-and-durability.md),
  section 4.3);
- `run_started`'s frozen model settings chains go to `blobs/` by digest.

Expected effect: logs about 60% smaller, so every parse is faster,
including a resume for a run.

#### 4.2.1 Implementation notes (2026-11-01)

History format 2 with its converter `v1_to_v2`
([events reference](../reference/events.md#history-formats)). The wire
carries whole values; the one protocol change is that readers take a
`session_created` of any history format (client protocol 1.63.0; an older
client receives format 1, which its whole values are). Where the
implementation settles what 4.2 leaves open, or departs from it (the
coordinator extended 4.2 on 2026-11-01 with the tool result's other large
values and the ACP live-state rule, so the store converts once rather than
again for a format 3):

- **Records in memory stay whole.** The loader reads every value back from
  `blobs/` as it parses a line that names one, and the writer moves them out
  as it writes one, so everything that reads a record (model replay,
  compaction, the approval reviewer, `event.read`, both views of
  `events.subscribe`, plugins) is unchanged, and the display view cuts large
  outputs as before. A log is smaller on disk, but a parse that reads it
  whole still reads the blobs its records name: the faster parse 4.2
  expects comes for the lines that are not moved, and for a cold view once
  `view.json` lands. A line naming blobs is kept in memory as read (the
  slimmed line, about 2.3 KB for a moved output), not re-read from the file,
  so a fork copies it with its references.
- **What moves.** An output strictly longer than 8 KiB keeps its first
  2 KiB, floored to a character boundary, as `output`, with `output_bytes`
  and `output_blob`; a display stored as ranges of the output stays ranges
  of the whole output. A tool result's `display` and each part of its
  `metadata` longer than 8 KiB of JSON (an ACP agent's `raw_output` and
  `raw_input`, mostly) move whole, an object only once the large members
  inside it moved and it is still that long; `moved_values` names them by
  JSON pointer, restored last-moved first. The display view's cut is
  unchanged: it works on the whole records in memory. The ACP lists and the
  chains move when their JSON is
  512 bytes or more (`<member>_blob`): a shorter one is smaller than its
  reference. Besides `run_started`'s `agent.fallback_chain`, each internal
  agent's `fallbacks` moves too, the bulk of a run's settings (3.4 KB each in
  the store measured). `delegation_reserved`'s `child_agent` holds a chain
  as well and stays as it is: 4.2 names `run_started` only.
- **Old ACP live state** (`v1_to_v2` only; a format-2 writer stores none,
  [store locks and durability](store-locks-and-durability.md) 4.3): an
  `acp_session_updated` holding only a `tool_presentation`, a `context`, or
  both is live state. A call's last presentation goes into its
  `tool_call_terminated` as `presentation` when it differs from the start's
  (tool identity aside), also one that arrived after the end; a call with no
  end record keeps the record of its last presentation. Of the context,
  the record in effect at each `model_usage_recorded` (an ACP reply's
  footer reads the context there) and `run_finished`, and at the log's
  end, stays: one per run alone would change earlier replies' footers in a
  run of several prompts. Every other live-state record becomes a
  `record_omitted` at its sequence, run, and time (without engine version
  and origin): dropping the line would leave a hole in the sequence, which
  the loader reports as damage. Clients show no row for such a record and
  the display view leaves it out (`EventPayload::is_omitted_live_state`);
  a client before this build shows a debug row for each.
- **References are artifact URIs** (`artifact://sha256/<digest>`), so
  artifact liveness (a fork links the blobs its history names, a revert
  collects the ones only its dropped records named) covers blobs without a
  change, and `blobs/` holds the tree's artifacts as well: it replaced
  `artifacts/`. The conversion moves `artifacts/` to `blobs/` first (the
  folder at once, or file by file into a `blobs/` that exists, so it can be
  repeated); a tree no conversion reached, held by another process, is read
  from `artifacts/`.
- **Durability.** A record's blobs are written before it, unsynced, and the
  sync that covers the record syncs them and `blobs/` first, outside every
  lock (store locks and durability, section 4.1); a blob a sync of the log
  covered once is not synced again for it. A record whose blob is missing
  or does not match its digest (lost to a power cut before that sync, or
  removed by hand) is skipped with a diagnostic, never read short. A
  conversion and a log written whole (a fork, a buffered session's first
  persistence) sync their blobs before the log.
- **A root's publication** may now find `blobs/` beside `subagents/` in
  its scaffold, when a child published first and its records named blobs;
  the root's own `blobs/` is merged into it.
- **The migration** converts every tree in the month folders when
  `layout.json` records an older format (or there is none), each under its
  lock, where it is; with a current format it retries only the pending
  ones. The marker moves to format 2 after the last tree, so a start a
  crash cut short converts again and finds the trees it finished current;
  an original a crash left beside its log (`events.jsonl.v1.bak`) goes to
  `.backup-v2/` then too. The free-space check counts the logs to convert
  in month folders as well, and has a Windows implementation
  (`GetDiskFreeSpaceExW`). Unlike a work-dir tree, a month-folder tree that
  fails to convert stays listed, and is read through an in-memory
  conversion until it converts.
- **Golden logs** of format 2 are in `crates/engine/tests/fixtures/history/2/`;
  the `coding` scenario now reads a 19 KB file, whose output moves.

Measured on 2026-11-01 on a fresh scratch copy of this machine's store (97
roots and 174 logs in the v2 work-dir layout, all format 1), on a machine
with a load average of 40 to 70:

| | Before | After |
|---|---|---|
| Logs | 281.6 MB | 215.8 MB (−23.4%) |
| `artifacts/` / `blobs/` | 45.2 MB | 91.3 MB, 2,848 blobs |
| Store | 327.4 MB | 307.7 MB (−6.0%) |
| Largest log (`01a17973-…`) | 16.94 MB | 9.73 MB (−42.5%) |
| Largest log after (`01a189ee-…`) | 13.40 MB | 11.22 MB (−16.2%) |

| Conversion, release build, tmpfs | Time |
|---|---|
| v2 layout to layout 3 and format 2 | 97 trees in 8.5 s |
| Layout 3 at format 1 (written by the format-1 build) to format 2 | 97 trees in 7.0 s |

Every one of the 174 logs opens with no diagnostic, and every line reads
back as the format-1 original but for the live-state rule (40,415
`acp_session_updated` records became `record_omitted`, 11,467 calls'
ends took their final presentation) and `history_format`. Folded through
the client reducer, the original and the converted log give the same
transcript rows, tool states (each call's status and displayed
presentation), ACP view, reply metrics (footers, context), and context use
for all 174 (158,603 events); the comparison leaves out only the process's
revision counters and rows' cache versions, and a hash set's order.

Measured earlier the same day, before the additions above: on the NVMe
disk, syncs included, the conversion took 2.9 times as long as on tmpfs
(18.9 s against 6.5 s); a debug start killed after 83 of 97 trees resumed
with them current (39 ms), converted the other 14, and left no staged
file, backup, or `.backup-v2/`; the next start read only `layout.json`.

In the largest log, what is left is mostly tool results' values under
8 KiB (5.1 MB over 1,112 results), model turns (1.4 MB), and the 4,520
`record_omitted` lines (1.1 MB). Left for later: `delegation_reserved`'s
agent snapshot chains.

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

### 4.5 Implementation notes: `view.json` (2026-11-01)

Shipped on history format 2 (4.2.1), with the wire leftovers of 3.4 and
the TUI's open as client protocol 1.64.0. Where it settles what 4.1 and
3.2 leave open, or departs from them:

- **What `view.json` holds**: `{version, history_format, start, header}`.
  `start` is a run boundary (`run_started` or `user_input_submitted`) by
  `seq`, `timestamp`, and the byte `offset` of its line in `events.jsonl`;
  `header` is the header (3.2) of everything before it. Not a header at
  the tip plus several run starts, as 4.1 put it: a window needs the
  header at *its* start, and the header composes — the header of a log
  through a boundary, followed by the events after it, has the header of
  the log through them — so one boundary and its header answer every
  window that starts after it (`display_view::tail_window_after`, tested
  against the whole log's window for every boundary of the golden
  histories). The boundary is the last one before the newest 1 MiB of
  display events (`HISTORY_WINDOW_MAX_BYTES`, every request's budget), so
  every window starts after it, and stays so as the log grows; a log that
  fits whole has no cache.
- **Reading it.** A cold open parses the log from `offset` to its end, as
  the loader reads intact records (`events::read_records_from`): a line
  that names blobs has them read back first (`blobs::internalize`), then
  every line is typed as it is (`StoredEvent::decode_strict`). It uses the
  cache only when the line there is the record `start` names, whole; a
  revert, a rewritten or converted log, a missing blob, a line the loader
  would repair or skip, and a `history_format` or `version` other than this
  build's all fall back to parsing the whole log, as does a window that
  would reach before the boundary, from a larger budget. The conversion
  of a tree to a newer format removes its logs' caches as it puts each
  converted log in place; one it cannot remove names the older format.
  Tested on a format-1 history with an ACP agent's live state converted
  to format 2 (moved outputs, model settings chains, `record_omitted`):
  read from every line, the records are the loader's, and every window
  read from a run boundary is the whole log's.
- **Writing it.** After a run's `run_finished` is appended, when a session
  leaves memory, and after a cold open parsed a log this process owns;
  only when the boundary moved since this process last wrote it. Outside
  every lock, as a temporary file renamed over the old one, never synced.
  Nothing deletes it: a stale one fails the check above. About 37–62 KB
  for the sessions measured, mostly ACP command lists and agent snapshots.
- **No tree, no opening.** `from: "tail"` no longer goes through the final
  page. A session this process owns is answered from memory when it is
  resident, else from the cache or the parsed log, and its tail is
  registered under the session's slot, which every append takes, after a
  check that what was read is still the whole log: the same resident
  log, or, for a session not resident (which is appended to only once it
  is resident again), a file whose length and modification time did not
  change. A read that loses that race reads again; a fourth holds the
  slot throughout. A session this process does not own is answered the
  same way without a tail (it was `snapshot_events`, which completed the
  tree too). Either starts the tree load in the background.
- **Deviation: no adoption in the background.** Subscribing never adopted
  a session (a session this process does not own got a snapshot, and a
  client calls `session.resume`, which adopts it, then subscribes again
  live), so the open path adopts nothing now either.
- **The clients' open.** The web client sends `session.resume` and the
  subscription together and shows the snapshot at once. The TUI awaited
  `session.resume` before it subscribed, so its first paint waited for the
  tree and the adoption; it now subscribes first and classifies the
  session in the background, its composer taking input meanwhile as after
  a reconnect (the daemon takes the session for what is sent, or refuses
  it and the message goes back to the composer). A session the TUI just
  created is its own at once. So that a session the daemon already owns
  is not subscribed twice (once before the classification, once after),
  the answer that registered the tail says `live: true` (1.64.0), and both
  clients take such a subscription as live.
- **Older pages' headers** (1.64.0): `events.page` takes `header: "delta"`,
  which the clients send for older history (the header they hold is the
  one of `before`); the answer holds only the header events the client
  lacks or holds differently, and the client keeps the held events before
  the page's `older_cursor` and replaces those of the same sequence. That
  this gives the page's whole header follows from the same composition
  (tested for every pair of starts of the golden histories). A page read
  into a gap asks for `header: "none"`: clients never used its header.
- **Display text that repeats the output** (1.64.0): to a client of 1.64.0
  or newer, display-view answers send such a text (a `text`, `markdown`, or
  `code` block's, a `terminal` segment's, a `file_excerpt`'s lines) empty,
  with an `OutputSpan` `{seq, block, segment, start, end}` in the answer's
  `spans`; the client puts the text back before decoding, through the
  session log's own span decoding. Spans live beside the events, not in
  them, so neither the stored event types, the plugin protocol, nor the
  display blocks change; texts under 64 bytes stay. Live events and
  `event.read` stay whole.

Measured on 2026-11-01 with release builds of `main` (2c35cb728, history
format 2) and of this change, each on a fresh scratch copy of three
sessions of this machine's store that a start of this build had converted
to format 2 (its `view.json` written by `session.resume` and a session
leaving memory), over a local WebSocket, a Node client sending
`events.subscribe {from: "tail", view: "display"}`; medians of three (one
run each for a session paged out after `session.resume` and an idle
minute), and "busy" pins the daemon to two cores beside eight busy loops.
The answers' events and windows were byte for byte the same.

| First answer | Not owned (a fresh daemon) | Owned, paged out | With `session.resume` sent alongside (web, and the TUI now) |
|---|---|---|---|
| 20 subagents (1.6 MB root in format 2), idle | 408 → 51 ms | 406 → 43 ms | 397 → 73 ms |
| same, busy | 1,893 → 117 ms | 1,862 → 94 ms | 1,412 → 140 ms |
| 12 subagents (4.6 MB root), idle | 283 → 45 ms | 259 → 42 ms | 282 → 87 ms |
| same, busy | 1,262 → 110 ms | 1,161 → 86 ms | 1,403 → 107 ms |
| ACP, 9.3 MB root (16.9 MB in format 1), idle | 253 → 51 ms | 212 → 54 ms | 233 → 61 ms |
| same, busy | 807 → 91 ms | 830 → 87 ms | 1,035 → 159 ms |

The TUI's first paint was `session.resume` and then the answer: 270–490 ms
idle and 0.8–2.3 s busy; it is now the answer sent alongside, the last
column. `session.resume` itself takes as long as before (0.23–0.85 s
idle, 0.8–3.3 s busy): it loads the tree. Without a `view.json` (a
session no build wrote one for yet), the first answer skips only the
tree: 64–253 ms idle, 163–1,046 ms busy, the ACP log's full parse the
most.

On the wire, for a 1.64.0 client (a 1.63.0 client gets the same bytes as
before): the ACP session's newest-first answer went from 274 to 257 KB,
and its 35 older pages from 10.1 MB to 7.75 MB (their headers from
1.61 MB to 0.33 MB); the 20-subagent root's 8 pages from 2.01 to 1.69 MB
(headers 382 to 140 KB). The first older page now parses the whole log,
which the first answer no longer does.

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

### 5.1 Implementation notes (2026-10-31)

Layout version 3 with history format 1, so `view.json`, `blobs/`, and
`v1_to_v2` were still to come (format 2 and `blobs/`: 4.2.1). Where the
implementation settles what the sections above leave open, or departs from
them:

- **No conversion yet, so no backups.** Each tree moves by one rename.
  `convert_tree` still runs on it under its lock, converting logs of a
  format older than 1; only their originals go to `.backup-v2/<root-id>/`,
  deleted after the commit point. The free-space check counts only the
  logs that need converting, by `statvfs` on Unix; Windows has no check
  (a conversion that runs out of space fails its tree, which stays
  pending).
- **`layout.json`** is `{"version", "history_format", "build", "pending"}`:
  `build` is the version of the cookie build that wrote it, which a
  refusing older build names ("it needs cookie 0.5.0 or newer"). Each
  pending tree is `{"root_session_id", "path", "error"}`, `path` relative
  to `sessions/`. The startup also lists `sessions/` itself (one
  directory): it removes deletion tombstones, `.backup-v2/`, and v2
  work-dir folders left behind by an interrupted cleanup, but never looks
  inside a month folder.
- **Journal writes.** Besides creation (first publication), title,
  project, and status, an entry is appended when held, attachments, or an
  active goal change (everything the fold calls durable metadata), on a
  fork or revert, and when a root with events since its last entry
  leaves memory (eviction or shutdown), so the listed `last_event_seq`,
  which guards a delete, is current. A root resident in this daemon
  answers from its projection. Appends are not synced; compaction and the
  migration's journals are (temp file, fsync, rename). A power loss can
  lose the newest entries; a crashed process cannot. A deletion's
  tombstone folder now sits in `sessions/` (`.deleted-<root>-<id>`), and
  one found at startup also journals the deletion.
- **Cursors.** `before` and `older` are timestamps. A page with `before`
  and no `active_since` reaches back to the start of the newest month
  that holds older sessions, passing over months that hold none.
- **The migration's journals** are rebuilt from every root in a month
  folder (so a resumed migration counts the trees an earlier attempt
  moved); a retry of pending trees only appends theirs. The work-dir
  folders' `runtime-revisions-v8.jsonl` are merged into
  `sessions/runtime-revisions.jsonl`, each record once, and of two that
  map one runtime revision differently the first.
- **`cookie sessions upgrade`** loses `--all`: with no work-dir key it
  moves the store to this layout, as a start does, and converts every
  tree.
- **Measured** on a scratch copy of this machine's store (96 roots in four
  work-dir folders, 300 MB, a debug build, idle machine): 96 trees moved
  in 0.7 s, none pending; the next start read only `layout.json`;
  `session.list` over the 30-day window answered 94 of 96 sessions in
  about 40 ms, over WebSocket. Seven of those roots predate history
  formats and do not open, as before the migration.

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

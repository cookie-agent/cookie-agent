# Session store locks and durability

Status: **draft** (2026-10-31), awaiting the user's decisions in
[section 8](#8-open-decisions). Follows
[responsive session loading](session-storage-v3.md), whose phase 1 removed
the tree load from a replay's history pages but left the locks below in
every session command's path.

## 1. Problem

Every command a session's actor runs goes through
`Engine::send_command` (`crates/engine/src/runtime/messaging/mailbox.rs`):
a replay's final page, but also a running agent's every append, tool
result, and steer. It:

1. takes `residency_mutation`, one `tokio::sync::Mutex` for the whole
   engine;
2. under it, calls `ensure_session_owned` → `SessionStore::begin_write`,
   which completes the session's tree (`ensure_tree_for`) and then takes
   the store's mutation lock **exclusively** (`lock_mutation`) *before*
   `begin_write_locked` finds the session already owned and resident, as
   it nearly always is;
3. only then spawns or finds the actor and sends.

Appends hold the same mutation lock shared, with the session's slot, while
they write **and `fdatasync`** the record (`EventLogWriter::append`). So:

- **Every command waits for every append in flight to sync, in every
  session**, holding the engine-wide lock while it waits, so every other
  session's commands queue behind it. A waiting exclusive locker also makes
  new shared lockers wait, so appends stall too. On a busy disk a sync takes
  hundreds of milliseconds, and this convoys.
- **A cold open stops every writer.** Opening a session that is not
  resident (`begin_write_locked` → `open_owned_log`) parses its whole log
  under the exclusive lock (0.2 s idle, over 1 s busy, for a 17 MB log),
  and loading its tree (every subagent's log) runs under the engine-wide
  lock. Meanwhile no agent anywhere appends.
- **The idle janitor** holds the engine-wide lock while it drains a tree's
  actors before evicting it (`evict_idle_roots_with`,
  `evict_idle_subagents_with`).

And the store syncs far more than recovery needs. Measured on 2026-10-31
(`strace -f -e trace=fsync,fdatasync` on a scratch daemon):

| What | Syncs |
|---|---|
| A scripted turn (edit a file, run a check, reply; 37 events) | 15 of the session log, about 18 more: the new session's directory and `metadata`, `unread.json`, memory's read bookkeeping, the model catalog's metadata, the edited file and its directory |
| A real Claude Code (ACP) session, per run (31 runs) | 262 of the session log; 56% are `acp_session_updated`, the agent's option, command, and context snapshots |

Elsewhere, `crons.json` is synced with its directory on every cron fire,
`unread.json` on every read or unread change, and the coding runtime's
`write` and `edit` tools sync each file they write and its directory.

## 2. Goals

- No lock is held across a disk sync, a log parse, or a tree load.
- A command to a session that is already open takes no engine-wide or
  store-wide lock.
- One tree's cold open, tree load, or eviction never delays another tree.
- Far fewer syncs, with the crash guarantees in section 3 kept.

Not goals: changing the history format, the single-writer ownership model
(`owner.lock`), or what recovery repairs.

## 3. What a crash may lose

Today every record but six bookkeeping kinds is durable before it is
published. The rule this spec keeps instead:

1. **No torn or reordered records.** The log is always a prefix of what was
   written, as now (a torn last line is cut on open).
2. **Durable before an effect outside the log.** A record that something
   outside the log acts on is synced before that action:
   - the `model_turn_committed` holding tool calls, and each
     `tool_call_started`, before the tool runs: a crash that lost them
     would let a resumed run ask the model again and repeat the side
     effect;
   - `user_input_submitted` (and the other records a client's request
     returns as accepted) before the reply that says so;
   - permission grants and approval decisions before the call they allow;
   - delegation records before the child they start;
   - structural records: `session_created`, forks, reverts, deletion.
3. **Everything else within a second.** Any other record (tool results,
   `acp_session_updated`, titles, usage, memory and runtime records,
   internal agents) is durable by the next commit's sync or within
   `GROUP_WINDOW` (section 4.2), and may be lost in a crash before that.
   Recovery already treats the log's end as possibly short: open runs end
   as `Interrupted`, pending tool calls are terminated `Interrupted`, and
   nothing is re-run on its own.

## 4. Design

### 4.1 Syncs outside every lock

`EventLogWriter::append` writes the record (readers in this process see it
at once) and returns a **sync ticket**, the byte length written. The
session slot and the mutation lock are released before anyone waits on a
ticket.

### 4.2 Group commit

One syncer thread per store keeps the set of log files with unsynced
writes:

- A **commit** record (section 3, item 2) waits, after its locks are
  released, for a sync that covers its ticket. The syncer runs one at once
  for a file that has a waiter, covering every record written to it so
  far, and wakes all of that file's waiters. The record is published after
  that sync, as every synced record is today.
- Any other record marks its file dirty and is published at once (decision
  8.1). The next commit's sync covers it; failing that, the syncer syncs a
  dirty file `GROUP_WINDOW` (1 s, decision 8.3) after its first unsynced
  write.
- Bookkeeping stays as now: covered by the next sync.
- Closing a log (eviction, shutdown, release) syncs it first, as the writer
  does on drop today.

Expected effect: a turn's log syncs fall to its commit points (about one
per tool call, since a `model_turn_committed` and the `tool_call_started`
after it share one sync, and one per user input), plus at most one a second
for what happens between them. The ACP session above has about 37 commit
points per run (36 tool calls, 1.3 user inputs); with a window sync in
most gaps between tool calls, that is roughly 70 to 80 syncs per run
instead of 262, and none of them under a lock. The scripted turn would go
from 15 to about 5.

### 4.3 Commands without engine-wide locks

- **Fast path.** If the session is owned, resident, and has an actor,
  `send_command` sends to it directly. `begin_write` checks owned and
  resident before it takes the mutation lock, and returns `AlreadyOwned`
  without it.
- **Retired actors.** Eviction marks an actor retired, under its tree's
  residency lock, before draining it. A retired actor answers every
  command it receives with `Retired`, and the sender retries through the
  locked path, which reopens the session. So a command can never run on an
  evicted session.
- **One residency lock per tree.** `residency_mutation` becomes a lock per
  root, taken by the four places that take it today (`send_command`'s slow
  path, delegate recovery, and the two janitor passes), each of which knows
  the session's root. Eviction is already per tree: a tree goes whole or
  not at all, and one tree never evicts another's subagents.
- **Heavy work first, locks after.** The slow path completes the tree
  (off the async workers) before it takes any lock. A cold reopen parses
  the log without the mutation lock and takes it only to install the
  projection, re-checking that nothing installed one meanwhile.

### 4.4 Other files

| File | Today | Proposed |
|---|---|---|
| `crons.json` | synced with its directory on every write | synced for user changes (create, edit, delete); a fire's run state written atomically without sync |
| `unread.json` | synced on every change | written atomically without sync: a crash may mark a session read again |
| memory's read bookkeeping (`..reads.json`) | synced on every turn that reads memory | atomic without sync |
| model catalog metadata | synced | atomic without sync: a cache |
| grant journal | synced per grant | unchanged: a commit point (section 3) |
| files the `write` and `edit` tools write | file and directory synced | decision 8.2 |

## 5. Measurement first

Before 4.2 and 4.3, the daemon records how long commands wait for
`residency_mutation` and the mutation lock, and how long each log sync
takes, and logs any wait or sync over 250 ms with the session and the
lock. A run of the user's daemon on a busy machine then shows how much of
the stall each lock causes, and the same counters prove the change after.

## 6. Tests

- Sync counts (`EVENT_LOG_SYNCS`, extended to the syncer): a turn's syncs
  match its commit points; a burst of `acp_session_updated` syncs at most
  once per window.
- Commit ordering: a tool's effect never starts before its
  `tool_call_started` is synced (a sync hook that holds the syncer proves
  the tool waits).
- Crash tests: a log cut at any byte after a commit point reopens with
  every commit record before the cut, and recovery ends the run as
  `Interrupted`.
- Locks: a command to tree B completes while tree A's residency lock is
  held; a command racing an eviction is answered `Retired` and retried,
  never run on the evicted session (the janitor's pre-barrier hook); a
  cold reopen does not delay another session's appends.

## 7. Order of work

1. Measurement (section 5).
2. Syncs outside locks (4.1), then group commit (4.2).
3. The command fast path and retired actors (4.3), then per-tree locks and
   heavy work before locks.
4. Other files (4.4).

## 8. Open decisions

1. **When a non-commit record is published.** At once (recommended): it
   shows without delay, and a crash can take back what a client already
   showed; the client's stale-cursor check (`cursor_timestamp`) then sees
   the log changed under it and replays it in full, as after a revert.
   After its sync: a client never shows what a crash takes back, but a tool
   result or an ACP update can show up to `GROUP_WINDOW` late. Commit
   records are published after their sync either way.
2. **Files the agent writes.** Keep syncing each file the `write` and
   `edit` tools write (today), or write them like an editor does: staged
   and renamed into place without a sync. ext4 and most filesystems still
   never leave an empty file after a crash for a rename that replaces one,
   but the newest content can be lost.
3. **`GROUP_WINDOW`:** 1 s recommended: the most a crash can lose of what
   is not a commit point. A shorter window syncs more often (records that
   arrive seconds apart each get their own sync once the window is shorter
   than the gap); a longer one loses more on a crash.

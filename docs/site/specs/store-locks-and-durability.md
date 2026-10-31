# Session store locks and durability

Status: **draft** (2026-10-31), with the user's decisions of 2026-10-31 in
[section 8](#8-decisions) and the ones still open in
[section 9](#9-open-decisions). Follows
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

The number of syncs is not what hurts: their latency under those locks
is. Measured on 2026-10-31, by `strace -f -e trace=fsync,fdatasync` on a
scratch daemon and from the timestamps of the user's real logs (one sync
per synced record):

| What | Syncs |
|---|---|
| A scripted turn (edit a file, run a check, reply; 37 events) | 15 of the session log, about 18 more: the new session's directory and `metadata`, `unread.json`, memory's read bookkeeping, the model catalog's metadata, the edited file and its directory |
| A real Claude Code (ACP) session, per run (31 runs) | 262 of the session log; 56% are `acp_session_updated` |
| The same session over time | 0.11 a second while running on average, 6.7 a second in its busiest minute, 45 in its busiest second |
| The whole store | 12,000 to 23,000 a day; at most 92 in one second |

Most of a Claude Code session's records are the agent's live state rather
than its history. Of its 9,813 records, 4,584 are `acp_session_updated`
(29% of the log's bytes): 2,328 carry a tool call's refined presentation
(3.1 MB; every one between the call's start and its end) and 2,223 the
agent's context use (up to 468 in one run, none outside runs). The other
fields (commands, options, notices, steering, and the rest) arrive about
35 times in all.

Elsewhere, `crons.json` is synced with its directory on every cron fire,
`unread.json` on every read or unread change, and the coding runtime's
`write` and `edit` tools sync each file they write and its directory.

## 2. Goals

- No lock is held across a disk sync, a log parse, or a tree load.
- A command to a session that is already open takes no engine-wide or
  store-wide lock.
- One tree's cold open, tree load, or eviction never delays another tree.
- Syncs only where recovery needs them (section 3).
- The agent's live state stays out of the log (section 4.3).

Not goals: the single-writer ownership model (`owner.lock`), or what
recovery repairs. The history format changes only additively: an optional
field on `tool_call_terminated` (section 4.3).

## 3. What a crash may lose

A sync matters only when the machine crashes or loses power: a record the
daemon wrote survives the daemon's own crash, from the kernel's page cache.
Today every record but six bookkeeping kinds is synced before it is
published. The rule instead:

1. **The log stays a prefix.** Everything up to the last sync is durable.
   Of the records after it, the kernel may write back any subset in any
   order; opening a log already cuts it at its first damaged line, so what
   survives is everything up to some point.
2. **Commit points sync before they take effect.** A record that something
   outside the log acts on is synced before that action, and every record
   before it with it:
   - `tool_call_started`, before the tool runs (and with it the
     `model_turn_committed` holding the call): a crash that lost them
     would let a resumed run ask the model again and repeat the side
     effect;
   - `tool_call_terminated`, before the next model request: a crash that
     lost it would have recovery record a finished call as `Interrupted`,
     and the model, told "Tool interrupted; retained output is
     incomplete.", could run it again;
   - `user_input_submitted`, and every other record a request's reply
     confirms, before that reply;
   - permission grants and approval decisions, before the call they allow;
   - delegation records, before the child they start;
   - `session_created`, forks, reverts, deletion;
   - `run_finished`, so a run always ends durable;
   - every kind that is rare: syncing it costs nothing.
3. **Everything else rides along.** The six bookkeeping kinds,
   `model_turn_committed`, and the stored `acp_session_updated` fields are
   not synced themselves. The next commit point covers them, or closing the
   log does (eviction, shutdown, release, as the writer's drop does today).
   Within a run the next commit point is seconds away; between runs only
   rare ACP fields can wait for the log to close.

Recovery already treats the log's end as possibly short: open runs end as
`Interrupted`, pending tool calls are terminated `Interrupted`, and nothing
is re-run on its own.

## 4. Design

### 4.1 Syncs outside every lock

`EventLogWriter::append` writes the record under the session's slot and
the shared mutation lock, as now, so readers in this process see it at
once. A commit point's sync runs after both are released, on a duplicate
of the file's descriptor (`fdatasync` covers everything written to the
file so far, whoever wrote it), and the record is published after it.

A session publishes its records in order: a record appended while a commit
point ahead of it is still syncing is published after that commit point.
The append that records `tool_call_started` returns after its sync, so the
tool starts only then. When a turn starts several tools at once, all their
`tool_call_started` records are written first and one sync covers them.

### 4.2 Which records sync

`syncs_on_append` becomes an exhaustive `match` over the event kinds, with
no catch-all, so a new kind does not build until someone decides whether
it is a commit point. The classes are section 3's.

Expected effect, from the counts in section 1: the Claude Code session
syncs about once per tool call start and once per tool call end, plus a few
records per run (inputs, `run_finished`): about 80 syncs per run instead of
262, none under a lock. A short turn changes less: the scripted one goes
from 15 log syncs to about 12, since most of its records are once-per-run
ones (the input, the run's start and end, memory, the title), which stay
commit points.

### 4.3 The agent's live state is not history

ACP tool presentations and context use become live-only, like text deltas
and tool progress:

- **A new transient event, `acp_live_update`,** carries a
  `tool_presentation`, a `context`, or both. The ACP runner publishes it
  through `publish_transient` instead of appending `acp_session_updated`
  for those two fields, and the store's `in_flight` keeps the newest
  context and the newest presentation of each open call, so a client that
  joins mid-run gets them.
- **The final presentation is stored with the call's end.**
  `tool_call_terminated` gains an optional `presentation`, set when it
  differs from the one `tool_call_started` stored. Replaying a log shows
  the start's presentation, then the final one.
- **Context is stored once per run.** Before `run_finished`, the runner
  appends one `acp_session_updated` with `context` when it changed since
  the last stored value, so a reload shows the last use.
- **Everything else in `acp_session_updated` stays stored as it is**:
  commands, options, notices, steering, the plan, the fork point,
  compactions, messages between sessions, background ends, the prompt
  context. They are rare and they are history.

Older clients drop a transient event they do not know (the client
protocol's rule for live-only output), so they see a call's final
presentation when it ends and the context once per run. Client protocol: a
minor bump. Effect on the session in section 1: 4,584 ACP records become
about 65, and the log about 19% smaller (the final presentations still
travel in the end records). This replaces the ACP item of history format 2
in [session storage v3](session-storage-v3.md) (its section 4.2).

### 4.4 Commands without engine-wide locks

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

### 4.5 Other files

| File | Today | Proposed |
|---|---|---|
| `crons.json` | synced with its directory on every write | synced for user changes (create, edit, delete); a fire's run state written atomically without sync (decision 9.2) |
| `unread.json` | synced on every change | written atomically without sync: a crash may mark a session read again (decision 9.2) |
| memory's read bookkeeping (`..reads.json`) | synced on every turn that reads memory | atomic without sync |
| model catalog metadata | synced | atomic without sync: a cache |
| grant journal | synced per grant | unchanged: a commit point |
| files the `write` and `edit` tools write | file and directory synced | decision 9.1 |

## 5. Measurement first

Before 4.1 and 4.4, the daemon records how long commands wait for
`residency_mutation` and the mutation lock, and how long each log sync
takes, and logs any wait or sync over 250 ms with the session and the
lock. A run of the user's daemon on a busy machine then shows how much of
the stall each lock causes, and the same counters prove the change after.

## 6. Tests

- Sync counts (`EVENT_LOG_SYNCS`): a turn's syncs are its commit points; a
  burst of model turns and stored ACP fields between two of them adds none.
- Commit ordering: a tool starts only after its `tool_call_started` is
  synced (a sync hook that holds the sync proves the tool waits); a record
  appended during a commit point's sync is published after it.
- Crash tests: a log cut at any byte after a commit point reopens with
  every record up to that commit point, and recovery ends the run as
  `Interrupted`.
- Live state: a client that joins mid-call gets the call's newest
  presentation and the newest context from `in_flight`; a replay from the
  log shows the final presentation from `tool_call_terminated` and the
  context stored before `run_finished`; no `acp_session_updated` is stored
  for presentations or for context mid-run.
- Locks: a command to tree B completes while tree A's residency lock is
  held; a command racing an eviction is answered `Retired` and retried,
  never run on the evicted session (the janitor's pre-barrier hook); a
  cold reopen does not delay another session's appends.

## 7. Order of work

1. Measurement (section 5).
2. Syncs outside locks (4.1) and the commit-point classes (4.2).
3. ACP live state (4.3).
4. The command fast path and retired actors (4.4), then per-tree locks and
   heavy work before locks.
5. Other files (4.5).

## 8. Decisions

User decisions of 2026-10-31:

1. No sync window and no background syncer: syncs move outside the locks,
   only commit points sync, and everything else rides on the next sync.
   The measured rate is low; the stall is sync latency under the locks.
2. `tool_call_terminated` is a commit point, so a finished tool is never
   recorded as interrupted and repeated.
3. ACP tool presentations and context use are live-only
   (`acp_live_update`); the final presentation is stored in
   `tool_call_terminated` and the context once per run.

## 9. Open decisions

1. **Files the agent writes.** Keep syncing each file the `write` and
   `edit` tools write and its directory (today), or write them as editors
   do: staged and renamed into place without a sync. A replacing rename
   never leaves an empty file after a crash on ext4 and most filesystems,
   but the newest content can be lost.
2. **Cron and unread state without syncs (4.5).** A power loss could then
   fire a cron job again whose last run it forgot, or mark a session read
   again.
3. **The measurement step.** Ship the lock and sync timing (section 5) on
   its own first and run it on this machine for a few days before the
   rest, or build everything and measure before and after on a scratch
   daemon as for phase 1.

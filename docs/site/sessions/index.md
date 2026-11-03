# Sessions

Use `/sessions` to search and open saved sessions, or `/new` for a fresh root.
Selecting a saved session restores its visible transcript; submitting another
message continues it. A headless continuation uses:

```sh
cookie run --resume-session <session-id> "Continue the investigation"
```

See [Command line](../clients/cli.md#headless-runs) for selection overrides. For shortening model
context without removing the saved log, see [Compaction](compaction.md).

## Lifecycle and persistence

A new empty session exists only in memory. Its directory, metadata cache, and
event JSONL are created atomically when the first user message is submitted.
Closing an empty session leaves no persisted session.

Persisted sessions use an append-only JSONL history. New events are stamped
with the writing engine version for diagnostics, but opening a session does not
require an exact version match. The engine reads what a newer build wrote
tolerantly: it ignores members it does not know, skips unknown audit events,
quarantines records that do not agree with the rest of the log, reports what
it skipped in session metadata, and tolerates the resulting sequence gaps
([Event Reference](../reference/events.md#reading-a-log)). The derived
`metadata` file is only a cache: missing, stale, mismatched, or unreadable
cache content is rebuilt from the event history. Streamed text, reasoning, and tool progress are live-only:
they reach attached views but are never written to `events.jsonl`, whose
history holds finished turns and tool results. Reattaching to a session while
a reply streams shows its finished turns, then the reply from the next live
output on; a daemon that dies mid-reply leaves no partial reply on disk, and
the run reopens as interrupted. Every stored event is written before it is
published. Events that something outside the log acts on (a tool call's
start and end, input a request confirms, grants and approval decisions,
delegations, a run's end, and every rare kind) are commit points: each is
synced before its append returns and before it is published, so a tool runs
only once its start is on disk, and the starts of a turn's tool calls share
one sync. The sync runs outside the store's locks, so a slow disk holds up
only the session that waits on it. Per-attempt bookkeeping
(`model_attempt_started`, `model_tools_published`, `model_request_prepared`,
`model_replay_evaluated`, `model_output_started`, `model_usage_recorded`),
committed model turns, and an ACP agent's stored state ride along: the next
commit point's sync covers them, or closing the log does, so a system crash
can drop only such records after the last commit point, and the log always
reopens as a prefix of what was written. While a run is in flight, fold-ignored records do not rewrite the
metadata tip (`last_event_seq`, `last_activity`); it catches up at the run's
next projection change or terminal event, on eviction, and on shutdown.

## Where sessions are stored

Every session lives in one store, `~/.cookie-agent/sessions/`, whatever
directory the daemon was started in; each session records its own working
directory, and projects group sessions. A root session's folder is
`sessions/<yyyy-mm>/<session-id>/`, named for the month (UTC) its id was
created in, and never moves; its subagents live under its `subagents/`
folder. `sessions/layout.json` records the store's layout version (3) and
history format.

The session lists show the sessions active in the last 30 days in the TUI,
and in the web client the last 7 days, or as far back as its filter says.
Both also show every pinned, unread, or busy session (a run going, an
approval or a question waiting, a background call running, a subagent that
can still wake it, or a cron job that will) whatever its age. In the TUI,
**Older** (Ctrl+O in `/sessions`) adds a month at a time; the web client's
filter picks a longer date, and with nothing in its default 7 days it
lists the 7 days up to the newest session. The daemon answers from the activity
journals in `sessions/activity/<yyyy-mm>.jsonl`, which get a session's entry
when its title, project, or status changes, never for every event.

A daemon that finds a store laid out by an older build, with a folder per
directory (`sessions/<hash>-<name>/`), moves every session into its month
once, before it accepts connections, converting logs of an older history
format on the way; later starts find `layout.json` current and look at
nothing else. A session that cannot move, such as one another cookie
process holds, stays in its old folder: the daemon's log and the
configuration diagnostics (`/config` in the TUI, Settings in the web client)
name it, it is not listed, and every start tries again. A daemon refuses a
store a newer build laid out and names the build it needs.

Shutting the daemon down cleanly cancels whatever runs are in flight and waits,
under a short bound, for them to record `RunCancelled`; a run only comes back as
interrupted by daemon restart when the process died without that chance.

Reopening a session adopts it: the engine reconciles that session's interrupted
work and then waits, under a short bound, for the delegation recovery the
adoption scheduled. Background subagents whose runs died with the previous
process are therefore already settled — marked interrupted, reported to their
parent, and their concurrency slots released — by the time the session is usable
again.

Use `/new` to create a fresh root session and `/sessions` to search and switch
between sessions. Delegated sessions form a tree beneath the root that created
them. The Agents panel lists each session's subagents by their latest
user-initiated activity, newest first: a user input submitted or admitted, or a
`delegate_subagent` call started. Output and tool results never reorder it.
Session metadata carries this time as `last_agent_activity`, so a reopened
session orders its subagents the same way before any of them is replayed. Accepted runs retain their frozen model binding even if catalog,
configuration, or provider-store state changes later.

Several cookie processes may share one project directory, but a root session
and every delegated session beneath it are written by one process at a time:
ownership is taken for the whole tree, not for a single session. Reopening any
session of a tree another process still has open reports `session is owned by
another cookie process`, for the root and its subagents alike. Such a session
is still readable — the TUI shows it as a read-only snapshot with input
disabled — and becomes writable once the process holding the tree exits.
Adopting one session of a free tree claims the tree, while each session in it
still reconciles its own interrupted work the first time it is written.

## History formats

A session log records its history format. A build that reads a newer format
cannot open the session and asks for a newer cookie. A daemon of a newer
format converts every stored session once as it starts, before it accepts
connections, and records the format in `sessions/layout.json`, so later
starts skip the scan; the originals are deleted once every tree converted.
A session another cookie process held then is converted the first time this
one takes its tree for writing, keeping each original log beside the new one
as `events.jsonl.v<N>.bak`; until then a read-only view converts in memory
only. Format 2 keeps a tool result's large output, display, and metadata,
an ACP agent's option and command lists, and a run's model settings chains
in the tree's `blobs/` folder, and stores no ACP live state an older log
held ([History formats](../reference/events.md#history-formats)). To convert
every session without starting a daemon:

```sh
cookie sessions upgrade
```

It moves the store to this build's layout first, as a daemon's start does,
then converts every session. It reports each tree it converted, each it skipped because another cookie
process holds it (that process converts it itself), and each session it
cannot convert, such as one written before history formats, and exits
nonzero when there was one. The backups stay until you delete them.

## Titles

Root sessions may generate a title from their opening user messages according to
the [Session Title](../engine/session_title.md) configuration. A delegated session is titled immediately
from the `delegate_subagent` description instead. The description is bounded by
`session_title.max_chars`, and the delegated child does not run the title
internal agent.

## Revert

Revert is available only while the session is idle. It removes every event
after the selected positive sequence from `events.jsonl`; new events reuse the
freed sequence numbers. A run left holding nothing but its start goes with it,
and a run the cut falls inside is closed as interrupted, along with its open
tool calls, internal agents, and approvals. Subagent sessions created by the
removed delegations are deleted, together with their own subagents; a revert
that would delete a running subagent is refused. A revert cannot be undone:
fork first to keep the current branch.

The clients revert, and fork, at three points:

- **A user message.** The cut falls just before the message: the run it
  started goes, so the session ends with the reply before it, and the
  message's text goes back into the composer for editing. A fork here is
  an edit in a fork: the new session ends before the message, and the
  message's text waits in its composer.
- **The end of a reply.** The cut keeps the reply and drops what came
  after it: the next run, or a producer's message, a goal, or a compaction
  that started before the next run. Settings changed after the reply, such
  as the model or the permission mode, stay. The last reply has nothing
  after it to revert; a fork there copies the whole session.
- **A compaction.** The revert drops the compaction, cutting where the
  input it summarized ended, so the model reads the full conversation
  again. A compaction inside a run cuts into the run, which closes as
  interrupted.

In the TUI, click a past user message, or the `⋯` that ends a reply's
footer, choose **Revert** or **Fork**, and confirm a revert; a compaction's
row ends in a `[revert]` button. In the web client, icons under each
message and finished reply copy, revert, and fork (on a touch screen, a long
press on a message or a reply's **⋯** opens them as a menu), and a
compaction's row has a **Revert** button.

## Delete

Deleting a session removes its whole tree from disk: the root session, every
subagent session under it, their `events.jsonl` and `metadata`, and the
tree's artifacts (uploads and retained tool output). Only a root can be
deleted; a subagent goes with its root. Its background calls are stopped
without reporting. A delete cannot be undone: fork first to keep a copy,
since a fork holds its own copy of the artifacts.

The daemon refuses while any session of the tree is running or compacting,
while another cookie process owns the tree, and when the root gained events
since the client showed it, such as another client's new turn. It moves the
tree's directory aside first and then removes it, so a crash in between
leaves only a `.deleted-…` directory, which the next start removes and
drops from the session list. Every client is told the sessions are gone.

In the TUI, open `/sessions`, highlight the session, press Ctrl+D (or Delete
while the list has focus), and confirm with `y`. In the web client, use the
trash icon on the session's row. See
[`session.delete`](../reference/protocol.md#delete).

## Fork

Fork copies a persisted prefix into a new independent session, cutting where
a revert to the same point would (see [Revert](#revert)). It may read an
active source session. Copied
events retain their sequence numbers, timestamps, run IDs, and payloads, while
their envelope is rebound to the new session ID. Shared content-addressed
artifacts remain resolvable without copying their bytes.

A fork does not take its source's subagents. The conversation comes along,
including what the subagents reported, but the subagents stay the source's:
the fork starts with no children, `get_subagent_result` refuses the source's,
and the fork creates its own when it delegates. Each delegation record the fork leaves
behind is replaced by a `record_omitted` event (see
[Events](../reference/events.md#delegation)).

A fork of a root session is a new root, with a tree of its own, and may be taken
from a session another process owns. A fork of a delegated session stays inside
its source's tree, so it is published into that root's directory and needs that
tree's ownership: forking a subagent of a tree another process holds fails with
`session_owned_elsewhere` ("This session is open in another cookie process.").

In the TUI and the web client, choose **Fork** from a user message or from
the end of a reply. The new title receives ` (fork)`, and the new session is
selected; a fork from a user message has that message's text in its
composer.

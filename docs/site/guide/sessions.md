# Sessions

Use `/sessions` to search and open saved sessions, or `/new` for a fresh root.
Selecting a saved session restores its visible transcript; submitting another
message continues it. A headless continuation uses:

```sh
cookie run --resume-session <session-id> "Continue the investigation"
```

See [Run](run.md#headless-runs) for selection overrides. For shortening model
context without removing the saved log, see [Compaction](compaction.md).

## Lifecycle and persistence

A new empty session exists only in memory. Its directory, metadata cache, and
event JSONL are created atomically when the first user message is submitted.
Closing an empty session leaves no persisted session.

Persisted sessions use an append-only, versionless JSONL history. New events are
stamped with the writing engine version for diagnostics, but opening a session
does not require an exact version match. The engine loads known records
best-effort, reports skipped unsupported or corrupt records in session metadata,
and tolerates the resulting sequence gaps. The derived `metadata` file is only a
cache: missing, stale, mismatched, or unreadable cache content is rebuilt from
the event history. Streamed text, reasoning, and tool progress are live-only:
they reach attached views but are never written to `events.jsonl`, whose
history holds finished turns and tool results. Reattaching to a session while
a reply streams shows its finished turns, then the reply from the next live
output on; a daemon that dies mid-reply leaves no partial reply on disk, and
the run reopens as interrupted. Every stored event is synced before it is
published. While a run is in flight, fold-ignored records do not rewrite the
metadata tip (`last_event_seq`, `last_activity`); it catches up at the run's
next projection change or terminal event, on eviction, and on shutdown.

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

Logs written before protocol 28 may contain `session_reverted` markers. They
are still read: each hides what followed its target on the branch visible when
it was written.

In the TUI, click a past user message, choose **Revert**, and confirm. The TUI
targets the sequence immediately before that user message and restores the
message text to the composer for editing.

## Fork

Fork copies a persisted prefix containing at least one submitted user message
into a new independent session. It may read an active source session. Copied
events retain their sequence numbers, timestamps, run IDs, and payloads, while
their envelope is rebound to the new session ID. Shared content-addressed
artifacts remain resolvable without copying their bytes.

A fork of a root session is a new root, with a tree of its own, and may be taken
from a session another process owns. A fork of a delegated session stays inside
its source's tree, so it is published into that root's directory and needs that
tree's ownership: forking a subagent of a tree another process holds reports
`session is owned by another cookie process`.

In the TUI, choose **Fork** from a user message. That message is included in the
copied prefix, the new title receives ` (fork)`, and the new session is selected.

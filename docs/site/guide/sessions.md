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
the event history.

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
them. Accepted runs retain their frozen model binding even if catalog,
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

Revert is available only while the session is idle. It appends a
`session_reverted` control event; it never truncates the physical event log.
Events through the selected positive sequence remain visible, and subsequent
events form a new branch. Title, status, usage, approvals, transcript, and model
context are derived from that visible branch.

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

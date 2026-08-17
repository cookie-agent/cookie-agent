# Sessions

## Lifecycle and persistence

A new empty session exists only in memory. Its directory, metadata cache, and
event JSONL are created atomically when the first user message is submitted.
Closing an empty session leaves no persisted session.

Persisted sessions use an append-only, versionless JSONL history. New events are
stamped with the writing engine version for diagnostics, but opening a session
does not require an exact version match. The engine loads known records
best-effort, reports skipped unsupported or corrupt records in session metadata,
and tolerates the resulting sequence gaps. The derived `meta.json` file is only a
cache: missing, stale, mismatched, or unreadable cache content is rebuilt from
the event history.

Use `/new` to create a fresh root session and `/sessions` to search and switch
between sessions. Delegated sessions form a tree beneath the root that created
them. Accepted runs retain their frozen model binding even if catalog,
configuration, or provider-store state changes later.

## Titles

Root sessions may generate a title from their opening user messages according to
the `session_title` configuration. A delegated session is titled immediately
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

In the TUI, choose **Fork** from a user message. That message is included in the
copied prefix, the new title receives ` (fork)`, and the new session is selected.

## Compaction

Automatic compaction is enabled by default. It triggers from actual token usage
or a learned pre-send estimate relative to the model context limit. Raw history
is preferred; old bulky tool outputs are replaced with artifact references only
when the raw compaction input would overflow its budget or a normal model request
has already failed for context length. If that is enough to bring an automatic
compaction below its trigger, no summarizer call is made.

Use `/compact` to force compaction for the selected idle session. Add optional
focus text, for example:

```text
/compact preserve the parser decisions and failing test evidence
```

Steering remains available while compaction runs. Admitted inputs stay pending
and are promoted only after the checkpoint, honoring any recalls made during
compaction. Set `context_compaction.auto = false` to disable automatic signals;
manual and context-overflow recovery compaction remain available.

See [Compaction](compaction.md) for the trigger math, the internal agent that
performs it, and rehydration.

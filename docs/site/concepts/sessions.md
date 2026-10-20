# Sessions

A *session* is one conversation: its messages, the runs that answered them,
every tool call with its result, and the settings it ran with. The daemon
keeps each session as an append-only event log, and every client renders the
session from those events.

## Trees and delegation

A session you start is a *root*. When its agent delegates a task with
`delegate_subagent`, the daemon creates a child session for the subagent,
under the root; the subagent may delegate in turn, up to the configured depth.
Together they form a *session tree*. Each subagent has a short handle, such as
`explore_1a2b3c4d`, by which the agents address it: to read its result, send
it a message, or stop its turn. A subagent reports every turn it finishes to
its parent, and runs on whether or not the parent waits. See
[Delegation](../engine/delegation.md), [Agent messaging](../engine/messaging.md),
and the [delegation tools](../reference/tools.md#delegation-and-skills).

Usage, permission modes, and approvals granted "for the session tree" all apply
to the whole tree; `/usage` shows the selected session and its tree.

## Where sessions live

Sessions are stored per directory under `~/.cookie-agent/sessions/`, each tree
in its own folder with its subagents below it and the artifacts its tools
retained. A session exists on disk from its first message. Its directory
decides where it is listed: the TUI shows the sessions of the directory it was
started in; the web page lists every directory's. See
[Lifecycle and persistence](../sessions/index.md#lifecycle-and-persistence).

## Ownership

One process writes a tree at a time. The daemon that opens a tree takes its
`owner.lock`; another cookie process, such as a standalone TUI beside a
running daemon, can read the tree but shows it as a read-only snapshot until
the owner exits. Clients of one daemon share its ownership, so they all write.
See [Tree-scoped session ownership lock](../specs/tree-ownership-lock.md).

## History that lasts

The log is a compatibility surface. Each session records the *history format*
it was written in (currently `1`); a later build reads it tolerantly, skipping
what it does not know, and a build that changes the format converts older logs
when it opens them, keeping the originals as `events.jsonl.v<N>.bak`.
`cookie sessions upgrade` converts a directory's sessions at once. See
[History formats](../sessions/index.md#history-formats) and
[Compatibility](../reference/compatibility.md).

## Changing history

- **Revert** removes a message and everything after it, subagents it started
  included, and puts the message back into the composer. It needs an idle
  session and is refused when the session gained events the client has not
  shown. See [Revert](../sessions/index.md#revert).
- **Fork** copies a session up to a message into a new, independent session.
  See [Fork](../sessions/index.md#fork).
- **Compaction** replaces older context with a summary when the context fills,
  or on `/compact`. See [Compaction](../sessions/compaction.md).

## What a session is bound to

A session has one selection (agent, model, variant, preset) shared by every
client, one permission mode for its tree, and the runtimes it attaches. A run
freezes its model binding and system prompt when it starts, so changes to the
configuration apply from the next run. See [Agents and presets](agents.md) and
[Runtimes](runtimes.md).

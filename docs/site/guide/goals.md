# Goals

Goal mode lets you give a root session a durable objective and a checklist the
agent maintains while it works. It is activated explicitly: the engine never
infers a goal from prompt content, and only root sessions can have one.

The [goal mode design spec](../development/goal-mode.md) describes the runtime,
plugin, and persistence contracts behind this workflow.

## Activation and goal bar

Activate goal mode in the root session with `/goal <objective>`. Bare `/goal`
shows usage requiring an objective, not a status report. `/goal status` is no
longer supported and is rejected.

A persistent one-line goal bar appears above the message composer whenever the
selected session has a goal. Click its objective description to open a detail
modal with the full objective, checklist, and lifecycle status. The checklist
view is read-only; the root agent maintains it through its items-only update tool.

Use the bar's **Pause** or **Resume** button and **Cancel** button to control an
active or paused goal. Completed and cancelled goals keep a read-only bar and
detail view; the bar is hidden when no goal exists. The producer queue strip
is separate and appears above the goal bar and composer.

The buttons are the primary lifecycle controls. These slash shortcuts remain:

| Command | Action |
|---|---|
| `/goal pause` | Pause an active goal |
| `/goal resume` | Resume a paused goal |
| `/goal cancel` | Cancel an active or paused goal |

`/goal <objective>` errors if a goal is already active or paused; cancel or
complete the current goal first. Typing `/goal` in a delegated child session is
rejected with an explanatory error.

A goal moves through four lifecycle states: `active`, `paused`, `completed`,
and `cancelled`. Completed and cancelled are terminal. Only you control the
lifecycle: the agent can read and revise the checklist through its tools, but
it cannot activate, pause, resume, or cancel a goal.

## Checklist and tools

While a goal is active or paused, the root run gains two extra tools, `goal_get` and
`goal_update`. They are **root-only**: delegated child sessions never see them,
and there is no goal inheritance into subagents. They are also not a
configurable capability; they cannot be granted to anything else. The tools
appear or disappear at the next run start after a lifecycle change, never
mid-run.

Tool availability does not bypass permission rules. `goal_get` uses the `read`
action with resource `goal:current`; `goal_update` uses `write` with resource
`goal:current`. Rules for `goal:*` cover this resource. A workspace-only
file pattern such as `${workspace_dir}/*` does not cover goal state; normal
matching, explicit deny/ask rules, and the unmatched default still apply.

Each checklist item contains only a description and a finished flag, with no
item ID. `goal_update { items }` replaces the entire ordered checklist of the
current/latest session goal when the engine actor accepts the update. The engine
serializes updates: the last accepted replacement wins, with no model-supplied
goal ID, revision check, or lost-update protection. An older run's update can
intentionally affect a newly activated active or paused goal. Updates reject if
the current goal is absent, completed, or cancelled. The engine still numbers
durable state revisions for reminders and user lifecycle controls; those controls
retain their goal-ID and expected-revision checks. The root checklist is the
authoritative record of progress: the agent is instructed to verify work
directly or through subagents before marking an item finished, and to trust
its own checklist over any subagent's self-report.

Completion has one automatic rule. When a checklist update leaves the
checklist **non-empty and every item finished**, the goal is recorded as
completed. An empty checklist preserves the current active or paused state and
never completes a goal: activating a goal with no checklist yet still starts
work, and the continuation reminder asks the agent to establish the checklist.

Checklist updates are also preserved while a goal is paused, including an
all-finished update, which records completion without scheduling any further
work.

## Continuations

While a goal is active, the engine can wake an idle session with a goal
continuation reminder that carries the full objective and the entire
checklist. A continuation is scheduled only when **all** of the following
hold:

- the goal is active, and the checklist is empty or has unfinished items;
- the session is idle (no active run);
- no pending user, steer, or queued messages exist;
- no other live producer registration (background delegation or plugin)
  exists;
- no goal reminder is already pending.

The condition is rechecked when the run actually starts, so real work that
arrives in between always wins. Because the reminder re-injects the objective
and checklist, compaction cannot strand the goal.

## Pause, cancel, and interruption

Pausing or cancelling a goal stops future goal continuations. The engine also
durably queues a steering notification before closing the producer registration
if the root has an active run, other live producers (excluding the goal's own
registration), or queued real messages. This includes an idle root waiting for
subagent results or plugin work. At the next safe model API request, the model
is told that the goal was paused or cancelled, to stop autonomous pursuit of
that old goal, and to follow your directions.
These are durable lifecycle-control messages, separate from continuation
reminders, so closing the goal controller does not remove them.

Because the notification uses steer delivery, it can wake a normal root run
when the root is idle but has other producers or queued real messages. That
wake does not resume goal reminders: the goal remains paused or cancelled.
A fully idle root with no other live producers and no queued real messages
saves the lifecycle change without starting an announcement run. The goal's
own registration or pending reminder alone does not trigger one.

This does not cancel an in-flight request, abort active delegations, or undo
actions already emitted. The existing explicit interrupt
(`/cancel`) remains unchanged and separate from goal lifecycle controls.
Checklist updates still target the current session goal at actor acceptance;
the notification does not bind an older run's updates to its former goal.

## Producers: plugins and background work

Background delegation results and plugin-originated messages reach the session
through runtime **producer registrations**. Registrations are runtime-only:
they vanish on restart. Messages that were accepted before a restart are
durable, and accepted-but-unconsumed messages that have not been discarded are delivered after recovery
even if their producer never re-registers.

Each message is sent in one of two modes, chosen per send:

- **Steer** — inserted into the next model API request, including mid-run at a
  tool boundary. It never cancels an in-flight request. Background subagent
  results always steer; a steer to an idle session starts a normal run.
- **Queue** — starts a subsequent normal run. While a run is active it is
  persisted and deferred, then delivered when the session is idle.

Pending and admitted producer messages in either mode appear in the existing
queue strip above the goal bar and composer, which automatically shows and hides with its
contents. Messages reserved for a model request are hidden while claimed; release
after a failed/cancelled attempt returns them to the strip unless already consumed
or discarded. Discarded messages are removed from the strip. Producer messages
cannot be recalled into the user composer; displaying them does not make them
user-authored input.

The producer's owner can discard its own waiting message through the producer
discard API, even after unregistering, because accepted messages are independent
of runtime registrations. Discard is durable and retrying an owned already-discarded
message is idempotent; another owner cannot discard it. Only unclaimed waiting
messages qualify, including admitted messages not yet reserved by the engine.
Reservation happens before provider dispatch, so a claimed message cannot be
discarded even if the provider has not received it yet. Releasing all claims
makes an unconsumed, undiscarded message waiting again. In-flight or consumed input
cannot be retracted, and discard cannot undo actions already taken.

After a restart, producer-capable plugins reconstruct their own pending work
from their own storage or the external services they front; the engine does
not replay plugin state for them. A goal's automatic continuation waits until
all tracked producer-capable plugins report ready, so recovered external work
can register before the agent resumes. A failed or disabled producer is
surfaced as a notification and holds goal continuation; it is not silently
treated as work done. Already accepted real messages can still be delivered.

There are no watchdogs or deadlines for producers. A producer that registers
and then stalls stays visible in the runtime inspection API (a read-only
snapshot of registrations) and blocks automatic goal continuations, but no TUI
command manufactures a wake around it. A plugin that never finishes startup
recovery can likewise keep the readiness barrier waiting indefinitely.
The engine also makes no exactly-once
claim: a producer that does not see an acknowledgment retries with the same
sender-chosen idempotency key, and the engine deduplicates the retry, but external side effects
of a repeated attempt are the producer's own concern.

## See also

- [Sessions](sessions.md) — session persistence, revert, and fork
- [Compaction](compaction.md) — how history checkpoints interact with durable
  goal state
- [Goal mode design spec](../development/goal-mode.md) — the governing
  technical specification

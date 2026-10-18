# Background Tool Calls

Status: **implemented** (decisions of 2026-10-16; all three phases landed 2026-10-16).
Sections 1, 7.1, and the "today" notes describe the design this replaced. This document is the
governing specification for running supported tool calls in the background,
whether the model asks for it or the user moves a running call there.
Section [Decisions](#11-decisions) records what review settled.

## 1. Goal and scope

Two tools can run in the background today, each its own way:

- `bash` (coding runtime) takes a `background` argument and runs a separate
  code path (`execute_background`). It reserves a job through
  `ToolExecutionContext::start_background`, and the job reports a
  `<background_bash>` element whose body (`Command:`/`Status:` lines and an
  output preview) differs from the foreground result.
- `delegate_subagent` (daemon base layer) takes a `background` argument too,
  but reports through the delegation machinery as `<subagent_notification>`,
  whose body is the same text the foreground result carries.

Neither lets the user background a call that is already running, and each tool
owns its own flag, its own notice, and its own completion format.

After this work:

- A tool **declares** that it supports the background. It runs one code path,
  its foreground one; the **host** decides whether and when to stop waiting.
- The host adds the `background` argument to every supporting tool's schema
  and strips it before the tool sees the call.
- The model starts a call in the background with `background: true`; the user
  moves a running foreground call there with one key.
- A background call reports with one element, `<background_result>`, wrapping
  exactly the result the foreground call would have returned.
- The model stops a background call with one base-layer tool,
  `stop_background`.
- Subagents report every finished turn to their parent (section 7).

- Clients list a session's background calls and let the user stop one; the
  model is told the user stopped it.

Out of scope: hotkeys and UI for clients other than the TUI, persistence of
background calls across a daemon restart (unchanged: shutdown kills them),
and any replacement for interactive stdin, which this work removes (section
8).

## 2. Vocabulary

| Term | Meaning |
|---|---|
| **Background-capable tool** | A tool whose spec sets `background: true`. |
| **Detach** | The host stops waiting for an executing call: it commits an immediate result for the call and keeps the call's execution running as a background call. |
| **Background call** | A detached call, from detach until its execution ends or is stopped. Identified by a *job ID*. |
| **Job ID** | The handle the model uses for a background call: `job_<8 hex>` taken from the tool call ID, or the subagent handle for `delegate_subagent`. |
| **Immediate result** | The tool result committed at detach. |
| **Background result** | The producer message a background call sends when its execution ends. |

## 3. Tool declaration

`ToolSpec`, and `ToolDeclaration` in the runtime protocol, gain two fields:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `background` | bool | `false` | The tool supports running in the background. |
| `background_default` | bool | `false` | Whether a call runs in the background when the model omits the argument. Only valid with `background: true`; `true` without it is a declaration error. |

For a background-capable tool the host:

- adds `"background": {"type": "boolean", "default": <background_default>}`
  to the tool's `parameters` before publishing it to the model, with a
  description saying what the background does. A tool whose own schema
  already has a `background` property is a declaration error.
- removes `background` from the arguments before `prepare`. The tool never
  sees it, and it is not part of the prepared operation's identity, so an
  approval covers a call in either mode.

`bash` and `delegate_subagent` declare `background: true,
background_default: false` and drop their own `background` arguments.

## 4. Detach

### 4.1 Triggers

- **Model.** A call whose effective `background` is true detaches as soon as
  its execution starts, after preparation and approval.
- **User.** A client asks the daemon to detach one executing call (section
  9). Only calls of background-capable tools in the execute phase can be
  detached; a call awaiting approval, still preparing, or already finished
  cannot, and the request answers `accepted: false`.

### 4.2 What the host does

The engine already awaits each executing call in one `select!` loop
(`runtime/tool_execution.rs`). Detach is one more branch of that loop.

A background-capable tool marks its **detach point**: once it has started
its work (bash right after spawning its command), it calls
`ToolExecutionContext::detachable(note)` with an optional **detach note**, a
line for the immediate result (bash: the process ID; `delegate_subagent`:
the subagent handle). A remote runtime forwards it as `tool/detachable`
(section 8). The engine detaches only at or after that point: a call the
model started in the background detaches as soon as it reaches it, a user's
request waits for it, and a call that finishes first simply returns its
result in the foreground. Detaching:

1. Reads the detach note. The call's execution is otherwise untouched.
2. It commits the call as completed with the immediate result (4.3).
3. It moves the call's execution future, its output capture, and its
   progress channel into a background call under the existing background-job
   rules (`runtime/background_tasks.rs`): the background result is held until
   the immediate result is durably committed, and the background call is
   killed if that commit fails, a revert removes the call, its session is
   deleted, or the daemon shuts down.
4. It releases the run: the call no longer blocks the turn, and later calls
   of the same batch proceed as if it had returned.

The background call's cancellation is no longer the run's. Each call gets its
own cancellation token, linked to the run's only until detach, so cancelling
the run (Esc) after detach leaves the background call running.

### 4.3 Immediate result

The tool result committed at detach, model-visible as the call's result:

- Model-started: `Started in the background as job <job>. You will get a
  <background_result job="<job>"> with its result when it finishes; do not
  wait for it or run it again. Stop it with stop_background(<job>).`
- User-moved: `User manually moved this call to the background; it is still
  running as job <job>. You will get a <background_result job="<job>"> with
  its result when it finishes; do not wait for it or run it again. Stop it
  with stop_background(<job>).`

The detach note, when present, follows on its own line. The result carries
`background: {"state": "job", "job": "<job>", "by": "model" | "user"}` so
clients can show the call as backgrounded (`state` is `self_reporting`, with no
`job`, for work that reports itself). The compatibility spec replaced the
earlier `metadata.background` key with this typed field.

## 5. Background result

When a background call's execution ends, the host finishes its output capture
and renders the result exactly as it would have for the foreground call:
same output preview, same truncation, same metadata, same failure text. It
sends that, wrapped, as a `Tool`-owned producer message with `steer`
delivery:

```text
<background_result job="job_1a2b3c4d" tool="bash" status="completed">
…the foreground result…
</background_result>
```

`status` is `completed`, `failed` (the execution returned an error; the body
is the failure text and any partial output), or `stopped` (section 6). The
notification row's summary is `<tool> <status>` plus the call's display
argument.

`<background_bash>`, `BackgroundReport`, `ToolExecutionContext::start_background`,
and the `BackgroundJobs`/`BackgroundLauncher` traits are removed: no tool
starts its own job any more.

## 6. Stopping: `stop_background`

A base-layer tool:

| | |
|---|---|
| Name | `stop_background` |
| Arguments | `job` (string, required): a job ID from an immediate result. |
| Permission | None (6.1): it can only reach the session tree's own background calls. |
| Published | Only while the model can see at least one background-capable tool. |

### 6.1 Tools that need no permission

Today a base-layer tool that needs no permission declares the reserved
permission name `unconditional`, which the permission pipeline special-cases:
always visible, always allowed, never named by a rule. This work makes it part
of the tool interface instead of a magic string:

- `ToolSpec.permission` becomes optional. `None` means the tool needs no
  permission: it is published whenever its own publication rule says so, and
  every call is allowed without evaluating rules or asking.
- The reserved `unconditional` name goes, both as a permission name and as
  the reserved `PermissionAction` label; `read_artifact` and `stop_background`
  declare `None`.
- Its prepared operations still carry an approval identity, as
  `unconditional` tools' do today; it is never evaluated.

Runtime tools may declare no permission too: `permission` on a runtime
protocol tool declaration becomes optional (section 8). Such a tool runs
without any rule or approval, and the user cannot restrict it through
permissions, only by detaching its runtime.

A tool's own publication rule stays separate from permissions:
`stop_background` declares no permission, and is published only alongside a
background-capable tool.

It cancels the background call's execution (for a remote runtime, the
existing `tool/cancel`) and returns once the call has ended. The background
result is still sent, with `status="stopped"` and whatever output the call
produced, so the model sees one result per job either way. A job that already
ended, or one outside the session tree, is an error naming the job.

**The user** stops a background call through the client (section 9). It ends
the call exactly as `stop_background` does, and the background result says
who stopped it, first line of the body:

- model: `Stopped with stop_background.`
- user: `User stopped this background call.`

Either way the result wakes the session like any other background result, so
the model learns the call is gone.

`cancel_subagent` is removed; `stop_background` with a subagent handle stops
the subagent's in-flight turn (section 7).

## 7. Subagents

### 7.1 Today

A parent hears from a subagent once per `delegate_subagent` invocation: when
that invocation's child run ends, as the foreground tool result or, in the
background, as one `<subagent_notification>` (owner `Delegation {
invocation_id }`, `steer`). Every later turn of the child is invisible to the
parent: a turn woken by the parent's own `send_message`, by another agent's
mail, or by the child's background `bash` finishing ends without a word, and
the parent can only poll `get_subagent_result`. Cancelling the parent's run
cascades to every child that run started, background ones included.

### 7.2 Every finished turn reports

From now on a subagent reports **every turn it finishes**, whatever started
the turn: the delegation itself, `resume_session_id`, `send_message` mail, its
own background results, or anything else. When a run of a delegated session
ends, its direct parent receives:

```text
<subagent_report handle="explore_1a2b3c4d" status="completed">
…the turn's final assistant message, previewed…
[subagent session explore_1a2b3c4d; completed; 79 lines; full output shown]
</subagent_report>
```

- The body is today's teaser body (`render_delegate_teaser_body`): the final
  assistant message previewed under `[subagent_output]`, then the handle,
  status, line count, and a `get_subagent_result` hint when the preview is
  cut.
- `status` is `completed`, `failed`, `cancelled`, or `interrupted`. A turn
  that fails or is cancelled still reports, with whatever text it produced
  (`(no output)` when none), so a parent never waits on a child that went
  quiet.
- It is a producer message with owner `Subagent { session_id }` (the child),
  `steer` delivery, and the child's run ID as idempotency key, so each run
  reports exactly once and a restart or replay cannot report it twice.
- Only the direct parent hears it; a grandchild reports to its own parent.

### 7.3 The foreground exception

`delegate_subagent` is background-capable (`background_default: false`).

- **Foreground.** The *first* turn's report is the tool result instead of a
  producer message: the tool result carries the same body, as today. Every
  later turn reports as in 7.2.
- **Background, or detached.** The call ends with its immediate result (4.3;
  the detach note is the subagent handle) and the first turn reports as in
  7.2 like every other. There is no separate `<background_result>` for a
  delegation: the subagent report is its background result.

Detaching a foreground delegation just stops the parent waiting; the child
does not notice. From detach on it counts toward `[delegation]
max_concurrency` like a delegation started in the background, and may take
the count over the limit for a while, which delays queued delegations rather
than stopping a running one.

### 7.4 Stopping and cancellation

- `stop_background` with a subagent handle cancels the child's **current**
  run, whichever run that is, and returns once it has ended; its report
  follows with `status="cancelled"`. A queued delegation is dropped and
  reports `cancelled` as today. The child can still be resumed or messaged
  afterwards. `stop_background` on a child that is idle is an error saying
  so.
- `cancel_subagent` is removed: its tool, prompt text, TUI row label, docs,
  and tests.
- Cancelling or interrupting the parent's run **no longer cascades to any
  child**. A foreground `delegate_subagent` the run was waiting on ends as a
  cancelled tool call, but its child keeps running, and the child's first
  turn then reports as in 7.2, as if the call had been detached. A background
  or detached child keeps running, as every other background call does (4.2).
  Children stop only through `stop_background`, the user (section 9), or
  their own session being cancelled.
- `get_subagent_result` stays: it reads a child's full output without
  waiting.

### 7.5 Bookkeeping

Per-invocation completion tracking (`DelegateFinishedV2`, the background
monitors, slots, and the queue) stays for admission and concurrency. What
changes is delivery: the engine raises the per-run report of 7.2 wherever it
appends the end of a run of a delegated session
(`engine/src/runtime/subagent_reports.rs`). One run reports elsewhere, with
the same `<subagent_report>` body: the first run of a delegation started with
`background: true` still reports through its invocation's completion, owned by
`Delegation { invocation_id }`, so its queue, slot, and restart recovery stay
as they were. A run reports through the `Subagent` owner unless that
invocation path or a foreground call's tool result already carries it. A
foreground call the parent stops waiting for, by detach or by its run being
cancelled, hands its child to a background monitor that counts toward
`max_concurrency` as 7.3 says. History replay renders legacy
`<subagent_notification>` messages as they were recorded.

## 8. `bash`, stdin, and the runtime protocol

**`bash`.** One code path: `execute_background` goes, and so do the
`background` and `interactive` arguments. `timeout` is unchanged: it kills
the process tree after this many milliseconds (`0` never), defaults to
120,000 ms (2 minutes), and runs from the start of the call for its whole
life, foreground and background alike. Detaching does not touch it, so a call
the user moves to the background after 90 seconds is still killed 30 seconds
later unless the model set a longer `timeout`. The tool description tells the
model to pass `timeout: 0` for long-lived background commands such as dev
servers. The detach note is the process ID: `Process <pid>`; the
immediate result already says how to stop the call by its job ID.

**stdin is removed** from tools entirely: `ToolExecutionContext::stdin`,
`ToolStdin`, `StdinWrite`, the engine's hard-coded `call.name == "bash"`
interactive check, the client RPC `run.tool_stdin`, the TUI's stdin input,
and the runtime protocol's `tool/stdin` and the `stdin` flag of the execute
request. A later feature may bring interactive input back in another form.

**Runtime protocol** (version bump):

| Change | Message |
|---|---|
| Added | `tool/detachable` (runtime → daemon, notification): `prepared_id`, optional `note`. The call reached its detach point; it keeps executing and still answers its `tool/execute` request when it ends. |
| Added | `background`, `background_default` on tool declarations (section 3). |
| Changed | `permission` on tool declarations becomes optional; absent means the tool needs no permission (6.1). |
| Removed | `tool/stdin`, and `stdin` on `tool/execute`. |
| Removed | `job/started`, `job/progress`, `job/completed`, `job/kill`: the daemon owns background calls, and a detached call's output keeps arriving as `tool/progress` for its `prepared_id`. |

`tool/cancel` stops a detached call as it stops a foreground one.

## 9. Client protocol

| Change | Method |
|---|---|
| Added | `run.tool_background`: run ID and tool call ID; returns `accepted`. Detaches the call as in 4.1. |
| Added | `session.background.list`: session ID; returns the session's background calls (below). |
| Added | `session.background.stop`: session ID and job ID; stops the call as the user (section 6) and returns once it has ended. An unknown or finished job is an error. |
| Added | `background.changed` notification: session ID and its current list, sent whenever a background call of the session starts, ends, or changes state. |
| Removed | `run.tool_stdin`. |

A background call in a list is `{ job, tool, tool_call_id, display, started_at,
detached_at, by }`: its job ID, the tool's published name, the call it came
from, the call's display argument, when the call started and when it was
detached, and whether the model or the user backgrounded it. A subagent with
a turn in flight is listed under its handle with tool `delegate_subagent`, so
the user can stop it the same way. The list lives in the daemon, not in the
session log, so a client never shows a call that a daemon restart killed.

Tool definitions published to clients carry `background` and
`background_default`, so a client knows which running calls it may offer to
move.

**TUI.**

- The title line of a running call of a background-capable tool shows a
  **background** button at its right end; clicking it sends
  `run.tool_background`. The row then shows the call as backgrounded, from
  its immediate-result metadata.
- A **background bar**, like the goal bar, appears while the viewed session
  has background calls, with their count. Opening it shows a panel like the
  goal detail view: one row per call (tool, display argument, elapsed time,
  who backgrounded it) and a **stop** action per row that sends
  `session.background.stop`. It follows `background.changed`.

Hotkeys, and UI for other clients, are left to those clients.

Client protocol 42, runtime protocol 3.

## 10. Phases

Each phase lands as its own commits with every gate green, and is pushed.

1. **Host-owned background for tools.** `background`/`background_default` on
   tool specs and declarations; schema injection and argument stripping; the
   detach branch, per-call cancellation, immediate and background results;
   `stop_background` and optional tool permissions (6.1); `bash` on one code
   path with its detach note; stdin
   removed; `tool/detachable` and the removal of `job/*`; `run.tool_background`,
   `session.background.list`/`stop`, and `background.changed`; the TUI's
   background button, bar, and panel. Runtime protocol 3, client protocol 42.
2. **Subagents.** `delegate_subagent` as a background-capable tool;
   per-turn `<subagent_report>` (7.2) and the foreground exception (7.3);
   `stop_background` for subagents and the end of the cancel cascade (7.4);
   `cancel_subagent` removed; `max_hops` no longer applied between a parent
   and its direct children (section 11).
3. **Documentation.** Tool reference, agents and tools guides, events
   reference, and the runtime and client protocol references; this spec
   marked implemented.

## 11. Decisions

All questions raised during review are decided (2026-10-16):

- Every subagent turn, a background delegation's first included, reports as
  `<subagent_report>` (7.2), not `<background_result>`.
- A parent and its direct children may exchange as much as they like:
  `[messaging] max_hops` does not apply between them. Subagent reports are not
  hops, and `send_message` between a parent and its direct child does not
  count toward a chain's hops either. `max_hops` keeps bounding chains between
  other agents.
- Runtime tools may declare no permission, like the daemon's own (6.1).

There are no open questions.

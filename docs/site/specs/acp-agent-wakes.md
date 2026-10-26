# ACP agents: background tasks and self-wakes

Status: **draft** (2026-10-26); every decision in
[section 11](#11-decisions) made by the user on 2026-10-26 (each
recommendation taken). Not yet implemented. Builds on [ACP agents](acp-agents.md),
[background tool calls](background-tools.md),
[external producers](external-producers.md), and
[held sessions](held-sessions.md).

## 1. Problem

An ACP agent can start work with no prompt from cookie. Claude Code does
it whenever something it started in the background finishes:

- a background shell (`run_in_background`) ends;
- a `Monitor` reports a line;
- a background subagent finishes;
- a scheduled wakeup fires;
- a message comes from another of its sessions.

Claude Code then starts a model turn of its own. Its ACP adapter
(`@agentclientprotocol/claude-agent-acp`, 0.97.0 checked) keeps reading
Claude Code's stream for the session's whole life. It forwards that turn
as ordinary `session/update`s, `agent_message_chunk` and `tool_call`
among them, with no `session/prompt` open. The adapter source says so:
"so background/between-turn output streams live, not just while a prompt
is awaiting".

Cookie loses all of it. `acp_round` (`crates/engine/src/runtime/acp/runner.rs`)
treats what arrived between rounds as metadata. It keeps commands,
options, and modes, and drops the following:

- message and thought text;
- tool calls;
- usage;
- permission requests, which are answered `cancelled`;
- elicitations, which are cancelled;
- calls to cookie's own MCP bridge, which are refused.

Updates for a session with no route wait in a capped buffer (`early`, 256
per session).

Observed 2026-10-26: a Claude Code session ran the web e2e suite in the
background and twice wrote a full report when it finished. The user saw
neither. Asked "still running?", the agent could only repeat itself.

Two more gaps end the agent's background work early:

- **Process idle shutdown.** `[acp] idle_shutdown_ms` (5 minutes) stops
  the agent's process once no cookie run uses it. Every background shell
  the agent still runs dies with it.
- **Session eviction.** Unloading an idle session sends `session/close`
  (`residency.rs`). The agent's background tasks for that session end
  with it.

Cookie also never shows the agent's background tasks. Its own background
calls are listed in the background bar, and the agent's are not, so the
user cannot see or stop them.

## 2. Goal and scope

1. **Self-wakes are shown and recorded.** A turn the agent starts on its
   own becomes a cookie run. It is started by a **producer message from
   the agent itself**, which says why it woke when the agent says so. Its
   text, tool calls, usage, permission requests, questions, and cookie
   tool calls are handled exactly as in a prompted run.
2. **Background tasks are shown and stoppable.** When the agent reports
   its background tasks, they appear beside cookie's own background calls
   (`session.background.list`, the background bar) and can be stopped
   from there.
3. **Background work is not killed early.** Neither process idle shutdown
   nor session eviction ends an agent that has background tasks running
   or a self-woken run open.

Out of scope:

- Claude's native Task subagents as cookie child sessions (ACP bridge
  gaps, item 7).
- Speaking ACP v2. Section 5.3 only makes sure its `state_update` slots
  in later.
- Changing what the agent sees. Nothing in this spec sends the agent new
  input.

## 3. Vocabulary

- **Round:** one `session/prompt` and the updates until its answer, as
  today.
- **Self-wake:** foreground work the agent starts while no round is open.
  The ACP v2 draft calls this foreground work outside a prompt ("a model
  cycle that Claude Code starts on its own" in the adapter).
- **Wake run:** the cookie run that records a self-wake.
- **Agent task:** a background task the agent reports through the task
  lifecycle of section 6.

## 4. The wake producer

A self-wake is recorded as a producer message, so clients, push
notifications, held sessions, and subagent reports treat it like any other
automatic run.

- **Owner:** a new `ProducerOwner` variant,
  `acp_agent { agent: AgentId }`, for the session's ACP agent (`claude`).
  - The engine derives it from the live ACP session; nothing else can
    produce under it.
  - The owner is closed but sits inside the open `EventPayload`. A client
    older than this change reads the event as an unknown payload and
    skips it.
- **The message:** `ProducerMessageAccepted` with:
  - `mode: steer`;
  - the wake's id as the idempotency key: the agent's own task id when it
    gives one (section 6), else a fresh id;
  - `description`: "woke on its own";
  - `body`: the cause, as plain text, when the agent says one. Examples:
    "A background task finished: Run every headless-Chrome scenario
    (completed)", "A scheduled wakeup fired." Without a cause the body is
    "The agent started a turn on its own."
- **Never sent to the agent.** The ACP runner leaves `acp_agent` messages
  out of prompt content: the agent made them. They record why the run
  exists and give clients a row to show.
- **No model input elsewhere.** Only ACP sessions get this owner, so the
  model loop never sees it.

Clients show the message as a producer row, like a scheduled fire. For
example: "◷ Claude woke on its own · A background task finished: Run every
headless-Chrome scenario". The run's output follows under it.

## 5. Wake runs

### 5.1 Listening between rounds

Each live ACP session gets a listener that reads its update channel while
no round holds it (the round takes the same lock it takes today).

- **Metadata** (`available_commands_update`, `config_option_update`,
  `current_mode_update`, `session_info_update`) is applied as today.
- **Output starts a wake**, when the session has no run:
  `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `plan`, or
  a task-lifecycle start (section 6).
  - The listener asks the engine to start a wake run with the update.
  - The engine accepts the wake message (section 4), starts the run at
    once, and hands the update to it.
  - Run admission does not apply: the agent is already working. A wake
    run is the one automatic run that a hold does not stop (D3).
- **`tool_call_update` for a call of an ended run** updates that call in
  place, as late output. It starts nothing.
- **`user_message_chunk` outside a round** never becomes a user message.
  When it comes first, it becomes the wake message's body.
- **Requests** (permission, elicitation, bridge calls) that arrive with no
  run open start a wake run too, and are then answered inside it as in a
  round. They are no longer refused.

### 5.2 Inside a wake run

A wake run is an ACP run with a **listening round** in place of a prompted
one. It sends no `session/prompt` and records updates exactly as a round
does:

- text and thinking;
- tool calls, rendered richly;
- the plan;
- usage, priced or not;
- permission requests through the session's rules and the approval flow;
- elicitations as questions;
- cookie tool calls through the bridge under the session's rules.

`Stop` (`run.cancel`) sends `session/cancel`. The adapter ends the cycle
`cancelled`, and the session is held as after any Stop (held sessions,
section 2).

### 5.3 The end of a wake

ACP v1 has no answer to wait for. In order of precedence, a wake run
ends at:

1. **The task lifecycle's end** of the wake (section 6), when the agent
   reports one: `completed` or `failed` ends the run that way.
2. **`state_update` `idle`** (ACP v2 draft), if an agent sends it.
   `requires_action` and `running` are recorded and change nothing else.
3. **The cycle's usage:** a `usage_update` whose `_meta["_claude/origin"]`
   is set. Claude's adapter sends one at the end of each cycle.
4. **Quiet:** no update for `[acp] wake_quiet_ms` (default 10 s) while
   no tool call of the run is open. An open call keeps the run waiting.

The run completes with its last message as its final text, as a round's
`end_turn` does. A later update starts a new wake.

Ending a wake run only ends cookie's record of it. Cookie sends nothing to
the agent: no `session/cancel`, and no process stop. After a quiet end:

- The process idle timer runs as after any run (section 7).
- If the agent was only pausing, its next update starts a second wake
  run, with its own producer row.

### 5.4 Input during a wake run

A wake run is a run, so the user's input follows the usual steer and
queue rules (D4):

- **Steer** (Send): the input is admitted to the wake run and goes to the
  agent as `session/prompt` at once. The adapter folds a prompt into the
  running cycle ("the turn reports from there"). The run goes on as a
  prompted round and ends with that round's answer.
- **Queue:** the input waits until the wake run ends, then starts a run
  of its own.
- **Producer messages** (cron, subagent reports, mail) follow the same
  rules as user input.

### 5.5 What follows from a wake run

Wake runs inherit everything an ordinary run gets:

- Session status and badges show it running.
- **Push notifications:** its end can settle a root session, so the user
  gets a push when the agent finishes reacting to its background work.
- **Subagent reports:** in a delegated session, its end reports to the
  parent as a `<subagent_report>`, as every finished turn does
  (background tools, section 7.2).
- **Held sessions:** a failed wake run counts toward the failure hold.
  While the session is held, a self-wake is still recorded (D3).

## 6. Agent tasks

Claude's adapter offers its background-task lifecycle only to a client
that opts in. The opt-in is JetBrains' AIR extension: `initialize`
`clientCapabilities._meta.jetbrains.air` = `{ version: 1, capabilities:
["asyncTasks"] }`.

Cookie already relies on adapter extensions (`_session/steering`, the
terminal `_meta`), and this spec adds this one (D2). With it, the
adapter sends three updates:

- `async_task_spawned`, with these fields:
  - `asyncTaskId` and `name`;
  - `taskType` (`local_bash`, `turn`, ...);
  - `description`;
  - `canStop`;
  - `outputFilePath`;
  - the `toolCallId` that started it;
  - `showInTranscript`.
- `async_task_progress`: description, last tool, and usage changes.
- `async_task_state_update`: `running`, `paused`, `completed`, `failed`,
  or `stopped`, with an optional summary.

The task stop method is `_session/async_task/stop { sessionId,
asyncTaskId }`.

Cookie uses them as follows:

- **Background calls.** A task that is not the wake's own `turn` task is
  listed by `session.background.list` and `background.changed`. Its
  entry carries:
  - `job`: the task id;
  - `tool`: the tool name of its call;
  - `tool_call_id`: cookie's id for that ACP call;
  - `display`: the description;
  - `by: model`;
  - a new additive field, `agent_task: true`.

  Its entry leaves the list at a terminal state.
- **Stopping.** `session.background.stop` on an agent task sends
  `_session/async_task/stop`. The `stop_background` tool stays cookie's
  own: the agent stops its tasks itself.
- **The wake's cause.** A `turn` task (`name` "Claude is working") marks
  a self-wake. Its `description` is the cause: the last task
  notification's summary, or "A scheduled wakeup fired." Its end is the
  wake's end (section 5.3, rule 1). It is not listed as a background
  call.
- **Unknown or absent.** The opt-in cannot fail. It is a `_meta` key of
  the client capabilities, which ACP requires an agent to ignore when it
  does not know it. An agent that does not support it (Codex, Gemini CLI,
  an older Claude adapter) sends no `async_task_*` updates, and then:
  - Self-wakes still start wake runs, but the body is the generic "The
    agent started a turn on its own."
  - They end by rules 2 to 4 of section 5.3.
  - The agent's tasks are not listed and cannot be stopped from cookie.
  - Only open wake runs keep the process and the session (section 7).
    Background work cookie cannot see still ends with the idle shutdown
    or eviction, as today.
- **What the opt-in changes in Claude's adapter** (0.97.0):
  - A backgrounded Bash call is marked as a background task in its tool
    call. Cookie renders that call as started, with its task in the
    background bar.
  - With the adapter's per-task stop affordance on, `session/cancel` no
    longer stops its background workflows, since the client stops tasks
    one by one. Stop in cookie stops the run only, as for cookie's own
    background calls (background tools, section 6).

Agent tasks live only in memory: the agent's process, and its tasks, end
with the daemon.

## 7. Keeping background work alive

- **Process idle shutdown.** The idle timer counts as busy while any
  session of the process has an open wake run or a running agent task.
  The cap is the same as for a busy process today: none.
- **Session eviction.** A session with a running agent task or an open
  wake run is pinned, like "work about to happen" in background tools,
  section 13. A session with no task lifecycle keeps today's eviction.
- **Daemon restart.** `daemon.restart` and `cookie daemon restart` wait
  for agent tasks as they wait for background calls (D6). A forced
  restart ends them, and the session records that the agent's tasks were
  stopped.

## 8. Protocols and storage

- **Client protocol 1.37.0, additive:**
  - `ProducerOwner::acp_agent { agent }`;
  - `BackgroundCall.agent_task`;
  - no new methods.
- **Events:** none new. A wake is `ProducerMessageAccepted` and
  `ProducerMessageAdmitted`, then the usual run events.
- **Configuration:** `[acp] wake_quiet_ms`.
- **Plugin protocol:** plugins see the new owner in events (minor bump).
- **Runtime protocol:** unchanged. The relay already carries every ACP
  line.

## 9. Clients

- **Producer row:** "◷ <agent> woke on its own · <body>", in the TUI and
  the web client, the same chip family as scheduled fires.
- **Background bar and sheet:** agent tasks beside cookie's background
  calls, with their description, running time, and Stop. A task whose
  `canStop` is false shows no Stop.
- **Diagnostics filter:** the wake's `turn` task lifecycle is not shown;
  it already appears as the run.

## 10. Tests

Engine tests use the scripted ACP agent, extended to send updates and
requests with no prompt open.

**Wakes:**

- an `agent_message_chunk` between rounds starts a wake run;
- the run records the text and ends on quiet;
- it ends on a `usage_update` with `_claude/origin`;
- it ends on a task-lifecycle end;
- it ends on `state_update` `idle`.

**Inside a wake run:**

- a permission request is answered through the rules and approvals;
- a bridge call goes through;
- an elicitation becomes a question.

**Input:**

- Send during a wake run is prompted at once and the run ends with its
  answer;
- Queue waits for the wake run to end;
- Stop cancels and holds the session;
- a wake while the session is held is recorded.

**Agent tasks:**

- `async_task_*` updates list, update, and remove background calls;
- `session.background.stop` sends `_session/async_task/stop`;
- the `turn` task gives the wake's body.

**Keeping work alive:**

- no idle shutdown while a task runs or a wake run is open;
- no eviction while one runs.

**Subagent:** a delegated ACP session's wake run reports to its parent.

**Browser e2e (`acp` group):** a wake row and its output appear live; an
agent task appears in the background bar and Stop stops it.

**Manual (Claude Code on yunhao-labserver):**

1. Start `sleep 30 && echo done` in the background.
2. End the turn.
3. See the wake row, the agent's report, and a push notification.

## 11. Decisions

The user took every recommendation below on 2026-10-26: D1 a new owner,
D2 opt in, D3 record, D4 steer, D5 keep with no cap, D6 wait, D7 10 s.

- **D1. Owner.** A new owner `acp_agent { agent }` (recommended), or
  `external { source: "acp:<agent>", name: "wake" }`. The second needs no
  protocol change, but it reads as a plugin or runtime, and external
  messages are model input by definition.
- **D2. Opt into the AIR extension** (`asyncTasks` only) to get agent
  tasks and wake causes from Claude's adapter. Recommended yes. Without
  it, self-wakes still show, but without a cause, without precise ends,
  and without tasks.
- **D3. Self-wakes while held.** Record them (recommended), or cancel
  them on sight. The agent is already working, so hiding the work is
  worse than showing it. Stop holds the session again.
- **D4. Send during a wake run** steers into it (recommended), or always
  queues behind it.
- **D5. Keeping work alive.** Running agent tasks and open wake runs keep
  the process and the session (recommended), with no cap. The
  alternative is a cap such as `[acp] max_task_keepalive_ms`.
- **D6. Daemon restart** waits for agent tasks as for background calls
  (recommended), or ignores them.
- **D7. Quiet fallback** of 10 s (`[acp] wake_quiet_ms`), or longer for
  agents that pause mid-cycle.

## 12. Phases

1. **Engine wakes:**
   - the between-round listener, wake runs, the `acp_agent` owner;
   - requests inside wake runs;
   - end rules 2 to 4;
   - the idle and eviction pins for open wake runs;
   - the TUI and web producer row.
2. **Agent tasks:**
   - the AIR opt-in;
   - background calls and stop;
   - the wake cause and end rule 1;
   - task pins and daemon restart.
3. **Docs:** the ACP agents guide (what a self-wake looks like, which
   agents report tasks) and the reference Claude Code agent document.

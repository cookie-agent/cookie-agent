# ACP v2: the port, self-wakes, subagent sessions, and background work

Status: **draft** (2026-10-26). Not yet implemented.

The user decided on 2026-10-26:

- port cookie's ACP client to ACP v2 completely and drop v1;
- accept the agents' subagent sessions.

The earlier decisions of this draft (then "background tasks and
self-wakes", D1 to D7) carry over where v2 leaves them meaningful
([section 13](#13-decisions)).

Builds on [ACP agents](acp-agents.md),
[background tool calls](background-tools.md),
[external producers](external-producers.md), and
[held sessions](held-sessions.md).

## 1. Problem

### 1.1 Agents work without a prompt, and cookie drops it

An ACP agent can start work with no prompt from cookie. Claude Code does
it when something it started in the background finishes:

- a background shell;
- a `Monitor` event;
- a background subagent;
- a scheduled wakeup.

Its adapter streams that work as ordinary session updates.

Cookie's v1 client drops it. `acp_round` treats what arrives between
rounds as metadata:

- text, tool calls, and usage are lost;
- permission requests, questions, and calls to cookie's MCP bridge are
  refused.

On 2026-10-26 a Claude Code session twice reported finished background
work that the user never saw.

ACP v1 has no way to say when such work ends. Covering it on v1 needed
three workarounds: heuristics (Claude's usage `_meta`, a quiet period),
and an IDE vendor's extension for task reports.

ACP v2 makes this part of the protocol. A session's foreground work is
reported as state: `running`, `requires_action`, and `idle`, the last
with a stop reason. The schema says so directly: work "can start with a
prompt or be agent-initiated, for example to pursue an ongoing goal or
follow up on a completed background task … Agent-initiated work does not
require a prompt request or a synthetic user message."

### 1.2 v1 limits elsewhere

- **A turn's end is the prompt's answer.** Steering needs the Claude
  adapter's `_session/steering` extension.
- **Subagents.** Claude's Task subagents show flat, as tool calls (ACP
  bridge gaps, item 7). v2 makes subagent sessions part of a client's
  baseline.
- **Terminal output** relies on an adapter `_meta` extension. v2 has
  agent-owned terminals.

### 1.3 Background work dies early

- The agent's process stops after `[acp] idle_shutdown_ms` (5 minutes)
  with no cookie run.
- An evicted session sends `session/close`.

Both end the agent's background shells.

## 2. Scope

1. **v2 only.** Cookie initializes every ACP agent with protocol version
   2 and speaks only v2. An agent that answers otherwise is refused
   (section 3). All v1 paths go (section 11).
2. **Turns end by state.** A run ends at the agent's `idle`.
   `session/prompt` only says the prompt was taken in.
3. **Steering is v2's own:** a prompt sent while the agent works.
4. **Self-wakes** become runs, started by a producer message from the
   agent itself (sections 6 and 7).
5. **Subagent sessions** become cookie child sessions under the parent
   (section 8).
6. **Background work:**
   - the agent's still-running terminals are shown;
   - background work and open wake runs keep the process and the session
     alive (section 9).
7. **Cookie's own tools show once.** A bridged call is shown as cookie's
   call. The v1 fix of 2026-10-26 already does this; on v2 the match uses
   the tool call's standard `name` (section 10).

Out of scope:

- v1 agents (Gemini CLI unless it adds v2, hand-written v1 agents);
- `providers/*`;
- `auth/logout`.

## 3. Handshake

- **The request:** `initialize` with:
  - `protocolVersion: 2`;
  - `info`: cookie's name and version;
  - `capabilities`: `elicitation` (form and URL) and `_meta`.

  v2 has no client file system or terminals. Notices, compaction, boolean
  options, plans, and subagents are baseline.
- **The response** must carry `protocolVersion: 2` and `info`.
  - Its `capabilities.session` gives what the session supports: `fork`,
    `delete`, `resume`, `additionalDirectories`, prompt content, and MCP
    transports.
  - Its `authMethods` drive `auth/login`.
- **Refusal.** Any other version, or an `initialize` error, stops the
  process. The agent's runs fail with "<agent> speaks ACP v<n>; cookie
  needs ACP v2. Update its adapter, or start it in v2 mode." The agent is
  reported unavailable in the runtime's agent list, as an agent whose
  process failed to start is today.
- **Adapters.** The official ACP organisation's Claude and Codex adapters
  serve v2 (the user, 2026-10-26).
  - Verified for Claude: `@agentclientprotocol/claude-agent-acp` 0.97.0
    serves both versions, and `CLAUDE_AGENT_ACP_PROTOCOL=v2` pins v2.
  - The starter kit and the ACP agents guide name these packages.

## 4. Methods

| v1 | v2 |
|---|---|
| `initialize` (`clientCapabilities`, `clientInfo`) | `initialize` (`capabilities`, `info`) |
| `authenticate` | `auth/login` |
| `session/new`, `session/resume`, `session/close`, `session/delete`, `session/list` | unchanged names, v2 shapes |
| `session/load` (with replay) | gone: `session/resume` only. An agent without resume gets a new session, as when resume fails today |
| `session/fork` with `_meta.jetbrains.air.fork` | `session/fork` (unstable in the schema), with the same `_meta` fork point (section 4.1) |
| `session/prompt`, answered at the turn's end | `session/prompt`, answered at insertion with the user message's `messageId` |
| `session/cancel` | `session/cancel`; then `idle` with `cancelled` |
| `session/set_mode`, `current_mode_update` | gone: the mode is a config option |
| `session/set_config_option` | unchanged |
| `session/request_permission`, `elicitation/create`, `elicitation/complete` | unchanged; requests name their session, which may be a subagent's (section 8) |
| `_session/steering` (Claude's extension) | gone: steering is a prompt during work (section 5.2) |

### 4.1 Forking at a point

v2's `session/fork` forks a whole session. Its request has `sessionId`,
`cwd`, `additionalDirectories`, `mcpServers`, `systemPrompt`, and
`_meta`, and no fork point.

Cookie keeps the fork point it sends today:

```
_meta.jetbrains.air.fork = { version: 1, messageId: <agent message ID> }
```

It works over v2, checked against `@agentclientprotocol/claude-agent-acp`
0.97.0:

- **The `_meta` reaches the fork.** The v2 surface hands `session/fork` to
  the agent's v1 fork (`v1ForkSessionRequest`), which drops only
  `mcpServers` and `systemPrompt`. The fork code reads the point from
  `_meta.jetbrains.air.fork` (`forkPoint` in `fork-session.js`).
- **The message IDs are the same.** v2 requires a `messageId` on every
  message chunk, and the v2 surface passes v1's through unchanged. So the
  agent message IDs cookie records are the IDs the fork point names.
  `:segment:<n>` suffixes resolve as before.
- **The probe still works.** An unknown point is refused with
  `invalidParams` (`-32602`), and JSON-RPC errors pass through the v2
  surface. So cookie's once-per-process probe (`probe_fork_point`)
  carries over unchanged. An agent that ignores the point forks whole and
  is handled as such, as today.
- **The fork is ready.** The forked session is started by the agent and
  can be prompted at once, with its config options and commands in the
  response.

The point stays a vendor extension until v2 names one. Agents without
it fall back to cookie's existing whole-session handling.

## 5. Turns by state

### 5.1 A prompted run

1. Cookie sends `session/prompt`. Its answer is the insertion: the
   prompt's `messageId`, and the claim on its input is released as at a
   commit today.
2. The run records updates (section 5.3) until the session's
   `state_update` says `idle`. A prompt the agent rejects fails the run
   with its error.
3. `idle`'s `stopReason` ends the run:
   - `end_turn` completes it;
   - `cancelled` cancels it;
   - `max_tokens`, `max_turn_requests`, and `refusal` fail it with that
     reason;
   - `error` fails it with the error's message;
   - a missing or unknown reason completes it.
4. `requires_action` is recorded as the session waiting on the user. The
   run stays open; clients show the approval or question as today.
5. `usage_update` gives usage and context as today.

A `running` with no prompt of cookie's open is a self-wake (section 7).
A repeated `idle` (a resume snapshot) ends nothing.

### 5.2 Steering and queueing

- **Steer:** cookie sends `session/prompt` while the run's work is
  `running`.
  - The prompt is steered once its answer (insertion) arrives, as the
    2026-10-25 addendum's "a steer waits until the agent takes it".
  - The same run goes on until `idle`.
  - A cancel before insertion leaves the input pending for the next run.
- **Queue:** cookie waits for `idle`, then sends the prompt as a new
  run's.

### 5.3 Updates

| v2 update | Cookie |
|---|---|
| `user_message`, `user_message_chunk` | the inserted prompt's echo, matched by `messageId` and not recorded twice; outside a prompt of cookie's, a wake's body (section 6) |
| `agent_message`, `agent_message_chunk` | assistant text; a whole `agent_message` replaces its chunks, by `messageId` |
| `agent_thought`, `agent_thought_chunk` | thinking, the same way |
| `tool_call_update` | upsert by `toolCallId`. The first update shows the call; `name` is the tool's own name (in place of Claude's `_meta.claudeCode.toolName`) |
| `tool_call_content_chunk` | appends one content item to the call |
| `terminal_update`, `terminal_output_chunk` | the agent's terminal, shown in the call whose content names it (`type: terminal`). This replaces the `_meta` terminal extension; a still-running terminal is background work (section 9) |
| `plan_update`, `plan_removed` | the checklist block, by plan ID |
| `state_update` | section 5.1 |
| `subagent_update`, `session_message`, `session_message_chunk` | section 8 |
| `usage_update`, `notice`, `compaction_update`, `compaction_summary_chunk`, `available_commands_update`, `config_option_update`, `session_info_update` | as today |
| unknown kinds | ignored, as today |

## 6. The wake producer

A run cookie did not prompt is recorded as started by a producer message
from the agent itself. Clients, push notifications, held sessions, and
subagent reports then treat it like any automatic run.

- **Owner:** a new `ProducerOwner` variant,
  `acp_agent { agent: AgentId }`.
  - It is engine-derived.
  - The owner enum is closed but sits in the open `EventPayload`. An
    older client reads the event as an unknown payload.
- **The message:** `ProducerMessageAccepted` with:
  - `mode: steer`;
  - a fresh id as the idempotency key;
  - `description`: "woke on its own";
  - `body`: the cause when the agent says one, else "The agent started
    work on its own."

  v2 names no cause. A `user_message` the agent inserts first, or a cause
  in the `running` update's `_meta`, becomes the body when present.
- **Never sent to the agent.** The runner leaves `acp_agent` messages out
  of prompts.
- **Clients** show it as a producer row: "◷ Claude woke on its own · …".

## 7. Self-wakes

- **Listening between runs.** A listener reads each live session's update
  channel while no run holds it. Metadata updates apply as today.
- **Start.** `running` or `requires_action` with no run open starts a
  wake run:
  - the engine accepts the wake message and starts the run at once;
  - run admission does not apply, because the agent is already working;
  - the listener hands over the updates that follow.
- **Requests** (permission, elicitation, bridge calls) that arrive with no
  run open start a wake run too. They are no longer refused.
- **Inside.** A wake run records updates and answers requests exactly as
  a prompted run does (section 5).
  - Stop sends `session/cancel`, and the session is held as after any
    Stop.
  - User input steers into it (section 5.2).
- **End.** `idle`, by its stop reason (section 5.1).
- **Ending a run never stops the agent.** After `idle` the process idle
  timer runs as after any run (section 9).
- **What follows** is as for any run:
  - session status;
  - a push notification when a root settles;
  - a `<subagent_report>` to the parent of a delegated session;
  - the failure count of held sessions.

  A self-wake while the session is held is still recorded.

## 8. Subagent sessions

v2 reports an agent's subagents (Claude's Task and Agent subagents) as
child sessions.

- **The parent's stream** carries `subagent_update`:
  - the child's `sessionId`;
  - `title` and `description`;
  - `capabilities` (`cancel`);
  - a mirror of the child's `state`.
- **The child's stream** carries its own updates under the child's
  `sessionId`.

The subagents part of the schema is marked unstable.

### 8.1 Cookie child sessions

- **Creation.** The first `subagent_update` for a new child creates a
  cookie session under the parent's tree. It records origin `delegated`:
  - `root_session_id`, `parent_session_id`, and `parent_run_id` are the
    parent's run;
  - `parent_tool_call_id` is the call that spawned it, when the update's
    `_meta` names one (Claude: the Task call), else none;
  - `invocation_id` and `depth` are as for a delegation.

  `SessionOrigin` is closed, and a new variant would break older clients'
  session lists. So the child reuses `delegated` and carries an additive
  `SessionMeta.acp_subagent: true`.
- **Fields.** The child's agent is the parent's ACP agent. Its title is
  the update's `title`, else its `description`. Its ACP session id is the
  child's `sessionId`, recorded like any ACP session's.
- **Clients** nest it under its parent as they nest delegated children.
  The subagent tree shows it running from its mirrored state.

### 8.2 The child's work

- **Runs.** The child's own `state_update`s make its runs, each a wake
  run of the child (section 7):
  - the first: "◷ <parent title> started this subagent: <description>";
  - later ones: "woke on its own".

  Its updates, permission requests, and questions are recorded and
  answered in the child's session, under the child's rules, which are the
  parent's agent document.
- **Stop.** Allowed only when the child's `capabilities.cancel` is
  present. It sends `session/cancel` with the child's id. Sending the
  child input is not offered: v2 grants no prompt capability to a child.
- **Not cookie's delegation.** The engine's delegation machinery stays
  off for these children:
  - no `<subagent_report>` to the parent, because the parent agent gets
    the child's result itself;
  - no handle and no `send_message`;
  - no cascade.

  The marker `acp_subagent` selects this.
- **Messages.** `session_message` and `session_message_chunk` (between
  parent and child) are recorded as producer rows of the receiving
  session, owner `acp_agent`, described "message from <session title>".
  They are never sent to the agent.
- **Lifetime.**
  - A child lives in the parent's ACP process.
  - Its session stays resident while the parent's does.
  - Ending or deleting the parent ends it.
  - After a restart, its history stays in cookie, but it is not
    resumed: it was the parent agent's.

## 9. Background work and keeping it alive

v2 has no background-task report. Claude's v2 surface refuses the
async-task updates of the IDE extension that v1 could opt into (it
throws on update kinds it does not translate). So this draft's earlier
D2 is dropped.

What v2 does report is the agent's terminals.

- **Agent terminals.** A terminal still running after the tool call that
  started it ended (no `exitStatus`) is background work:
  - It is listed by `session.background.list` and `background.changed`
    as a background call, with a new additive `agent_task: true`. Its
    `display` is the terminal's command, and its tool call is the call
    that shows it.
  - It leaves the list when the terminal exits.
  - It has no Stop: v2 gives clients no terminal kill.

  Phase 1 checks that Claude's adapter keeps a backgrounded command's
  terminal open until it exits. If it does not, background shells stay
  invisible, and only wakes report their end.
- **Process idle shutdown** treats a process as busy while any session of
  it has:
  - an open run, wake runs included;
  - a running agent terminal;
  - work in `running` or `requires_action`.
- **Session eviction** pins a session in the same cases, like "work about
  to happen" (background tools, section 13).
- **Daemon restart** (`daemon.restart`, `cookie daemon restart`) waits for
  them as for background calls. A forced restart ends them, and the
  session records that.

## 10. Cookie's tools through the bridge

The 2026-10-26 fix applies unchanged:

- the agent's report of its call of a cookie tool is held back while
  cookie's call through the bridge stands for it;
- it is shown only when it ends without reaching cookie.

On v2 the tool is read from the call's `name` (`mcp__cookie__<tool>` for
Claude). The title and raw-input forms remain for adapters that leave
`name` out.

## 11. What goes

- **The v1 wire code:**
  - `clientCapabilities` and its v1 markers (notices, compaction, boolean
    options);
  - the v1 update kinds (`tool_call`, `plan`, `current_mode_update`);
  - `session/load` and its replay handling;
  - `authenticate`;
  - mode handling (`session/set_mode`, the modes-to-option shim).
- **Claude-only extensions:** `_session/steering`, the terminal `_meta`
  keys, and `_meta.claudeCode.toolName` as the only name source.
- **Draft rules** made moot by state: the v1 end rules of this draft's
  earlier version (Claude's usage `_meta`, `[acp] wake_quiet_ms`). Their
  config key is never added.
- **The ACP fixture agent** of the engine tests is rewritten to speak v2
  (section 14).

Sessions created under v1 keep their history. Their next run resumes the
same ACP session with v2 `session/resume`; the agent keeps its own
storage.

## 12. Protocols and storage

- **Client protocol, next minor, additive:**
  - `ProducerOwner::acp_agent { agent }`;
  - `BackgroundCall.agent_task`;
  - `SessionMeta.acp_subagent`;
  - no new methods.
- **Events:** none new.
  - Wakes are producer messages and runs.
  - Children are `SessionCreated` with origin `delegated` and their
    runs.
  - A run's stop reason uses the existing outcomes.
- **Plugin protocol:** a minor bump, for the new owner in events.
- **Runtime protocol:** unchanged; the relay carries every ACP line.
- **Configuration:** none new.

## 13. Decisions

Made by the user on 2026-10-26:

- **v2 only, drop v1.** Agents that cannot speak v2 are refused with a
  clear error.
- **Accept subagent sessions** as cookie child sessions (section 8).
- **D1. Owner:** a new `acp_agent { agent }` owner.
- **D2. The IDE task extension:** dropped. It is not usable over v2
  (section 9); agent terminals stand in for it where the adapter keeps
  them open.
- **D3. Self-wakes while held** are recorded.
- **D4. Input during a wake run** steers into it.
- **D5. Background work and open runs** keep the process and the session
  alive, with no cap.
- **D6. Daemon restart** waits for them.
- **D7. The quiet fallback:** dropped. `idle` ends work.

## 14. Tests

The engine's ACP fixture agent (`crates/engine/tests/fixtures/acp_agent.py`)
is rewritten as a v2 agent, keeping its commands. It adds:

- work it starts on its own;
- a subagent;
- a backgrounded terminal;
- a v1-only mode for the refusal test.

**Handshake:**

- v2 is accepted;
- an agent answering version 1 is refused with the message of section 3;
- `auth/login` follows `authMethods`.

**Turns:**

- a prompt's run ends at `idle` with each stop reason;
- `requires_action` keeps it open;
- an `error` stop fails it with its message;
- a resume snapshot's `idle` ends nothing.

**Steering:**

- a prompt during work is steered at insertion;
- a cancel before insertion leaves the input pending;
- Queue waits for `idle`.

**Updates:**

- whole messages replace their chunks;
- `tool_call_update` upserts, and `tool_call_content_chunk` appends;
- terminals show in their call;
- `plan_update` and `plan_removed` work.

**Wakes:**

- `running` with no run open starts a wake run with an `acp_agent`
  producer row, and `idle` ends it;
- a permission request, an elicitation, and a bridge call inside one are
  answered;
- Stop cancels and holds the session;
- a wake while held is recorded.

**Subagents:**

- `subagent_update` creates a nested child session;
- the child's updates and requests land in it;
- its Stop is offered only with `cancel`;
- no `<subagent_report>` goes to the parent;
- `session_message` rows appear.

**Background work:**

- a terminal running past its call is listed, and leaves the list at
  exit;
- no idle shutdown or eviction while one runs or a run is open.

**Bridge:** cookie's tools show once, matched by `name`.

**Browser e2e (`acp` group):** a wake row and its output live; a subagent
nested under its parent; a background terminal in the background bar.

**Manual (Claude Code on yunhao-labserver, v2):**

1. Start `sleep 30 && echo done` in the background and end the turn. See
   the terminal in the background bar, then the wake row, the report, and
   a push.
2. Ask for a Task subagent. See it nested and live.

## 15. Phases

1. **Wire v2** (one phase, since v1 goes with it):
   - the handshake and refusal;
   - the methods and updates of sections 3 to 5;
   - turns ending by state;
   - native steering;
   - the bridge match by `name`;
   - the fixture agent rewritten;
   - the v1 code removed.

   Phase 1 also checks:
   - whether Claude keeps background terminals open;
   - Codex's v2 surface.
2. **Self-wakes:**
   - the between-run listener;
   - wake runs and the `acp_agent` owner;
   - requests inside wake runs;
   - the producer row in the TUI and the web client.
3. **Subagent sessions:** section 8, with the nesting and the child's
   Stop in both clients.
4. **Background work:**
   - agent terminals as background calls;
   - the idle, eviction, and restart rules of section 9.
5. **Docs:**
   - the ACP agents guide (v2 only, which adapters, what wakes and
     subagents look like);
   - the reference Claude Code document;
   - the starter kit's adapter packages.

## 16. Open questions

- **Unstable parts.** Some of v2 is marked unstable and may change:
  - the subagent part (`subagent_update`, `session_message`);
  - `session/fork`, whose fork point is a vendor `_meta` (section 4.1).
  Cookie follows the SDK version the official adapters use, and treats
  unknown fields and kinds as today.
- **Codex.** Its v2 surface, including what it reports of subagents and
  background work, is checked in phase 1.

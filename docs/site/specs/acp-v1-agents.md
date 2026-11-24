# ACP v1 agents through a translator

Status: **implemented** (2026-11-24; no protocol change).

The user decided on 2026-11-24:

- support Antigravity CLI (`agy acp`), which speaks only ACP v1, through
  a translation layer rather than waiting for a v2 release;
- make the translation generic, for any agent that answers v1, and test it
  against Claude Code's and Codex's adapters in their v1 mode as well as
  against `agy`;
- carry the v1 extensions that v2 made standard, where they fit: subagent
  sessions, the JetBrains AIR fork point, and the rest of section 2.5.

Builds on [ACP v2](acp-v2.md), which made cookie's client v2-only and
listed v1 agents as out of scope. Cookie's client stays v2-only: the
translator sits under it and turns a v1 agent's messages into v2 ones and
back.

## 1. Problem

`agy acp` 1.5.16 answers `initialize` with `protocolVersion: 1` even
when offered 2. Cookie refused it: "speaks ACP v1; cookie needs ACP v2".
No v2 release is announced, and v2 itself is still a draft upstream. The
[migration guide](https://agentclientprotocol.com/protocol/v2/migration)
asks clients to keep serving v1 agents. The Rust SDK's polyfill crate
only proxies MCP over ACP, and the community `agy-acp` adapter drives
`agy` through a terminal and its SQLite files, which Google's FAQ calls a
terms-of-service violation.

## 2. Design

`V1Transport` (`crates/engine/src/runtime/acp/v1.rs`) wraps the transport
of every ACP agent process. Until the agent answers `initialize` it
passes frames through. An agent that answers v2 is passed through from
then on, unparsed. One that answers v1 is translated in both directions.
`process.rs` accepts version 1 and 2 at `initialize`, and records the
version the agent answered (`acp_session_started.protocol_version`).

### 2.1 Handshake

Cookie's `initialize` carries both versions' fields: v2's `info` and
`capabilities`, and v1's `clientInfo` and `clientCapabilities`: no file
system and no terminals, elicitation as v2's, and the v1 forms of what v2
gives every client (section 2.5). A v2 agent ignores the v1
fields; Claude Code's and Codex's adapters were checked to answer v2 to
it. A v1 agent reads its own, so `agy` sees that cookie answers questions.
Without them `agy` writes its question into the reply and ends the turn.

A v1 answer is put in v2's names: `agentInfo` becomes `info`;
`sessionCapabilities`, `promptCapabilities` (as `prompt`), and
`mcpCapabilities` (as `mcp`, with `stdio`) become `capabilities.session`,
with `true` markers as objects; `authMethods[].id` becomes `methodId`.

### 2.2 Turns by state

| v2, as cookie speaks it | What the translator does with a v1 agent |
|---|---|
| `session/prompt` answers once the prompt is taken in | Sends the prompt under its own request ID and answers cookie's at once with a made-up `messageId`, a `user_message` echo, and `state_update` `running` if the agent was not working |
| `idle` with a stop reason ends the work | The agent's answer to the prompt. A JSON-RPC error is an `idle` with stop reason `error` and its message. Usage from every prompt answered in the work is summed |
| A prompt during the work steers it | Forwarded. The work is `idle` only once the agent has answered every prompt sent. `agy` answers the steered prompt at once and the steer at the end, and Claude Code's adapter queues it; both end as one run |
| Work the agent starts itself: `running` … `idle` | A work update (message, thought, tool call, plan) or a request (permission, question) with no prompt in flight is reported `running` first. v1 has no end marker, so the work is `idle` (`end_turn`) after 10 s with no update, 120 s while one of its calls runs, and never while a request waits on the user. More updates later start a new run |
| `session/cancel` | Forwarded. Cancelling the agent's own work, with no prompt in flight, is answered `idle` `cancelled` at once |

The end of a call that arrives after its work ended (a call cancelled with
its turn) starts no work.

### 2.3 Shapes

- `tool_call` becomes `tool_call_update`, which creates the call in v2.
- A diff's `path`, `oldText`, and `newText` become `changes` (`add`
  without old text, else `modify`) and a `git_patch` made with `similar`.
- `plan` becomes `plan_update` (`planId` `plan`).
- Message and thought chunks get a `messageId`, one per run of chunks of a
  kind, which cookie uses to split paragraphs.
- A permission request gets v2's `title` (the call's title) and `subject`
  (`tool_call`).
- `session/set_config_option` loses `type: "id"`; a select's value is a
  string in v1.
- Session responses lose `modes` and `models`, and `current_mode_update`
  is dropped: `agy`, Claude Code's adapter, and Codex's all send the same
  as config options, which cookie uses.
- A stdio MCP server loses `type: "stdio"`, which v1 does not have.

### 2.4 Agent extensions

- **Tool names.** `agy` names an MCP tool `mcp_<server>_<tool>`. For a
  server cookie handed the session, the translator renames the call
  `mcp__<server>__<tool>` (longest server name first), as Claude Code
  names it. Cookie's permission mapping now also reads that form from a
  call's `name`, not only its title or raw input, so `agy`'s calls of
  cookie's own tools are matched to their bridged calls and shown once.
- **Appended system prompt.** `agy` offers
  `agentCapabilities._meta.antigravity.systemPrompt.append` and takes the
  text as `_meta.antigravity.systemPrompt.append` on `session/new` and
  `session/resume`; probes showed it ignores v2's `systemPrompt`. The
  translator reports `capabilities.session.systemPrompt` for such an
  agent and moves the text there. An agent that offers v1's
  `sessionCapabilities.systemPrompt` gets `systemPrompt` as it is.

### 2.5 v1 extensions that v2 made standard

`clientCapabilities` declares them, as cookie's v1 client did before
[ACP v2](acp-v2.md):

| Declared | What the agent then sends | What the translator does |
|---|---|---|
| `subagents` (the subagents RFD) | Its subagents as sessions: `subagent_update` announcing each, with its `state`; `session_message` between parent and child; the child's own updates | v2's shapes already. A `subagent_update`'s `state` is also sent as a `state_update` on the child's session, as v2 does, which cookie reads. A child's updates never count as work the agent started itself, and its MCP tool names go by its root session's servers |
| `session.notices`, `session.compaction` | `notice`, `compaction_update`, `compaction_summary_chunk` | Passed on: v2's names |
| `session.configOptions.boolean` | Boolean config options (`type: boolean`) | Passed on; `set_config_option` keeps `type: boolean` |
| `_meta.terminal_output` (Zed's terminal extension) | A command's output as `_meta` keys of its call: `terminal_info`, `terminal_output` or `terminal_output_delta`, `terminal_exit` | v2's agent terminals: the call's content names the terminal (`type: terminal`), output becomes `terminal_output_chunk`, the exit a `terminal_update` with `exitStatus`; the keys leave the call |

**The JetBrains AIR fork point** needs nothing new. Cookie sends
`_meta.jetbrains.air.fork` on `session/fork`, which passes through, and
names the fork point by the agent's message ID. Claude Code's and Codex's
adapters send their own `messageId` on v1 chunks too, which the
translator keeps, so a fork starts at the right message. The translator
makes up IDs only for an agent that sends none, such as `agy`, which
offers no fork either.

Not declared: AIR's own client capabilities. They switch Claude Code's
adapter to AIR's draft forms (`subagent_spawned`, its session index),
which cookie does not read in v2 either.

### 2.6 MCP schemas

`agy` (Go's RE2) refused cookie's whole MCP toolbox over one tool whose
schema has a lookahead `pattern` (the agent ID pattern
`^[a-z0-9](?:[a-z0-9]|-(?=[a-z0-9]))*$`). The bridge's `tools/list` now
drops string `pattern`s that use lookaround from the schemas it lists, for
every agent. Cookie validates a call's arguments itself, so no check is
lost.

## 3. What is left out

- v1 modes and models as such (see 2.3).
- `session/load`: cookie never sends it.
- v1 client file system and terminals: cookie offers neither.
- `agy`'s subagents: it offers no subagent sessions. Their permission
  requests and questions arrive on the parent session and are answered
  there.
- `authenticate`: cookie runs no sign-in flow for ACP agents. Sign `agy`
  in once on its machine (`agy`, or `agy acp --login` without a browser).

## 4. Tests

Run on yunhao-labserver on 2026-11-24 with `agy` 1.5.16,
`claude-agent-acp` 0.111.0 (`CLAUDE_AGENT_ACP_PROTOCOL=v1`), and
`codex-acp` 2.11.0 (its `initialize` rewritten to version 1 by a test
proxy), through `cookie run` and a scratch daemon driven over its
WebSocket:

- shell commands, file writes and edits with diffs, and cookie's
  `list_runtimes` over the bridge, shown once as cookie's call;
- the appended system prompt (`agy`);
- `--resume-session` in a new process (`session/resume`);
- steering on all three: one run, the steer taken in;
- a question (`agy`'s `ask_question`), answered from the client;
- cancelling a running command, then a further prompt;
- Claude Code's follow-up after a background command, as a run of the
  agent's own ended by the quiet period;
- a Claude Code subagent (Task, in the foreground) as a cookie child
  session, its shell call's output shown from the terminal extension, the
  child `completed`;
- a fork of a Claude Code session at its first turn (the AIR fork point):
  the fork remembers the first turn's word, not the second's;
- Claude Code and Codex in v2 mode, given the two-version `initialize`,
  still answer v2.

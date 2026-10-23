# ACP agents

Status: **approved** (2026-10-23), being implemented.

## 1. Goal and scope

Run external coding agents, such as Claude Code, Codex, and Gemini CLI, as
cookie agents: a session or a subagent whose turns an external agent takes,
driven over the [Agent Client Protocol](https://agentclientprotocol.com)
(ACP). Cookie is the ACP **client**; the external agent is the ACP agent.

Naming: the docs call these **ACP agents**, never "the client protocol",
which stays the name of cookie's own protocol.

**Decided 2026-10-23:**

- **Runtimes host ACP agents.** A runtime (the daemon's own `local`, a
  template runtime the daemon starts, or a dial-in runtime, including one a
  TUI or web client offers) declares agents in its configuration, next to its
  MCP servers. The daemon registers each as **`<runtime_id>.<agent_id>`**
  (`local.claude`, `labpc.codex`), available while that runtime is connected.
- **The runtime spawns the agent** on its own machine and relays its ACP
  messages to the daemon over the runtime protocol. **The daemon speaks
  ACP**: sessions, rendering, permissions, cancel, resume.
- **Cookie's tools reach the agent as an MCP server over stdio**, through a
  bridge the runtime spawns: the agent's MCP traffic reaches the daemon over
  the runtime's existing connection, so the agent's host needs no route to
  the daemon. (MCP-over-ACP, which would carry MCP on the ACP connection
  itself, is a draft RFD; not used.)
- **Cookie settings come from a daemon-side agent document with the same id**
  (`~/.cookie-agent/agents/labpc.codex.md`): mode, permissions, body,
  `hide_tools`, starting mode and config options. Without one, the agent gets
  the defaults (section 4).
- **cwd:** the session's workspace on that runtime, else the runtime's home.
- **The agent runs its own file and shell tools.** Cookie does not advertise
  ACP `fs` or `terminal`. `hide_tools` lists the cookie tools it does not get;
  by default cookie's coding runtime tools.
- **Its permission requests are decided by `permissions`**, its ACP tool
  kinds mapped onto cookie's actions (section 8).
- **Mode:** the agent's "auto" mode when it offers one, else its default.
- **Login:** credentials already on the runtime's machine; authentication
  errors reach the session with what the agent said. No login flow in cookie.
- **The agent document's body** is sent with the session's first prompt; an
  empty body sends nothing.
- **Slash commands** the agent offers appear in the TUI and web composers.
- **`deny`** stays the spelling for turning a permission off.

**Out of scope for v1:** ACP `fs` and `terminal`, logins run from cookie,
ACP v2, MCP-over-ACP, the agents' vendor extensions (`_meta`, AIR,
steering, goal).

## 2. ACP in brief

JSON-RPC 2.0 over the agent's stdin and stdout, one message per line; the
agent's stderr is diagnostics. Version 1 (schema 1.26, Oct 2026).

- **Client to agent:** `initialize`, `authenticate`, `session/new`
  (`cwd`, `mcpServers`), `session/prompt` (content blocks, returns a stop
  reason), `session/cancel`, and when the agent supports them `session/load`,
  `session/resume`, `session/close`, `session/set_config_option`.
- **Agent to client:** `session/update` notifications (message and thought
  chunks, tool calls and their updates, plans, available commands, config
  options, usage, session info) and `session/request_permission`.
- Every agent MUST accept stdio MCP servers in `session/new`; HTTP is
  optional.
- Adapters: Claude Code `@agentclientprotocol/claude-agent-acp`, Codex
  `@agentclientprotocol/codex-acp`, Gemini CLI `gemini --acp`.

## 3. Declaring agents on a runtime

```toml
# The daemon's own machine, and any machine that dials in: its `local` table
[runtimes.local.acp.agents.claude]
command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
env = {}
description = "Claude Code"

# A runtime template the daemon starts (for example over ssh)
[runtimes.labpc.acp.agents.codex]
command = ["npx", "-y", "@agentclientprotocol/codex-acp"]
env = { CODEX_HOME = "/home/ada/.codex" }
description = "Codex"
```

- **Where:** exactly where MCP servers can be declared: `[runtimes.local]` on
  the daemon's machine and on a dial-in runtime's machine, and
  `[runtimes.<id>]` templates in the daemon's config.
- **Fields:** `command` (argument vector, run without a shell, on the
  runtime's machine), `env` (merged over the runtime's environment),
  `description`, `enabled`.
- **Agent ids:** the table key, following the agent id grammar (lowercase
  letters, digits, `_`). The daemon's id is `<runtime_id>.<agent_id>`; the
  agent id grammar gains this one dotted form, reserved for runtime-hosted
  agents. A dotted id that would also name a daemon agent is refused.
- **Reporting:** the runtime reports its agents when it connects and when its
  configuration changes; the daemon adds or removes them and announces the
  change like any agent registry change.
- **Offline:** an agent whose runtime is not connected is listed as
  unavailable; starting a session or a delegation with it is refused at once,
  as for offline runtimes.

## 4. The daemon-side agent document

```yaml
# ~/.cookie-agent/agents/labpc.codex.md
---
description: Codex on labpc, for refactors        # overrides the runtime's
mode: all
acp:
  mode: auto                         # optional; section 5
  config: { model: "gpt-5" }         # the agent's config options, by id; labels
  hide_tools: [read, write, edit, bash, webfetch]
permissions:
  read: allow
  write: { "${workspace_dir}/*": allow, "*": ask }
  bash: { "git *": allow, "*": ask }
  webfetch: allow
---
You review changes for this project before they are committed.
```

- **Optional.** Without it, `<runtime_id>.<agent_id>` is an agent of mode
  `all` with the runtime's description, the defaults every agent gets for
  actions it names no key for, `acp: ask`, and the default `hide_tools`.
- **No `models`, no `acp.command`:** the runtime launches the agent and the
  agent chooses its own model (set through `acp.config` when it offers a
  model option).
- **`acp.hide_tools`:** patterns with `*` and `?` over the tool names cookie
  publishes (`read`, `bash`, `memory_*`, `exa_*`). Default: cookie's coding
  runtime tools (`read`, `write`, `edit`, `bash`, `webfetch`), since the agent
  has its own; `[]` offers them too. Independent of `permissions`, which keep
  deciding calls.
- **`acp.config`:** initial values for the agent's config options, matched by
  option id; unknown ids or values are reported in the session and skipped.
- The document lives with the daemon's other agents, so a runtime cannot
  grant itself permissions. The agent appears in the agent pickers, presets,
  `list_agents`, and `delegate_subagent` like any other.

## 5. Modes and config options

- Cookie reads the agent's `configOptions` from `session/new`, and its
  `modes` when it offers no mode option.
- **Starting mode:** `acp.mode` when set; otherwise the mode option or mode
  whose id is `auto`; otherwise the agent's default. Mode ids are labels.
- Then `acp.config` is applied.
- Clients show the session's agent options (model, mode, effort) in the
  session's details; changing one calls `session/set_config_option`.

## 6. Running a turn

- **The runner.** `start_run_direct` keeps everything it does today (run
  start, idempotency, `RunStarted`, the end of the run) and, for an ACP agent,
  calls the ACP runner instead of the model loop.
- **The prompt.** The user's message (or a producer's: a cron fire, a
  subagent report) becomes `session/prompt` content: text, images when the
  agent accepts them, and the document's body before the session's first
  prompt.
- **Rendering**, from `session/update`:
  - `agent_message_chunk` → assistant text, streamed as text deltas;
  - `agent_thought_chunk` → thinking;
  - `tool_call` / `tool_call_update` → a cookie tool call from a new tool
    source, `acp:<agent>`, upserted by its ACP id: title and kind, status as
    the call's state, `diff` content as a diff block, `content` as text or
    media blocks, locations as file links, raw input and output in metadata;
  - `plan` → a checklist block, replaced on each update;
  - `usage_update` → recorded usage, without a cost when cookie cannot price
    the agent's model;
  - `session_info_update` → the session title, unless the user set one;
  - `available_commands_update` → the session's agent commands (section 10);
  - `config_option_update` → the session's agent options (section 5);
  - unknown kinds are ignored.
- **Durability.** Each turn is recorded as model turns and tool calls with a
  synthetic model reference (`acp:<agent>`), so history, reports, and clients
  work unchanged. The agent keeps its own conversation; cookie never replays
  history to it.
- **The end.** `end_turn` completes the run with the last message as its
  final text; `cancelled` cancels it; `max_tokens`, `max_turn_requests`, and
  `refusal` fail it with that reason.
- **Steering** falls back to queueing: ACP has no standard steer.
- **Interrupt** sends `session/cancel`, answers every pending permission
  request `cancelled`, and waits for the `cancelled` stop reason within the
  existing cleanup grace.

## 7. Sessions, processes, and the runtime relay

- **One ACP session per cookie session**, created on its first turn; its ACP
  session id and cwd are recorded in the session log.
- **cwd:** the session's workspace attachment on the agent's runtime; without
  one, the runtime's home directory.
- **After a daemon or runtime restart**, or a process exit, the next turn
  reconnects with `session/resume` when the agent offers it, else
  `session/load` (its replay is ignored), else a new ACP session, which the
  session is told about.
- **Processes:** one per agent per runtime, shared by its sessions, started on
  first use, stopped after an idle period with no live session
  (`[acp] idle_shutdown_ms`), with the runtime, and with the daemon.
  `session/close` when the agent offers it, as a cookie session is deleted or
  unloaded. A process exit fails the runs it served, with its stderr tail.
- **Errors**, including `auth_required`, end the run as failed with the
  agent's message.
- **The relay (runtime protocol, additive):**
  - the runtime's hello and `runtime/agents_changed` carry its agents (id,
    description, enabled);
  - `agent/start` and `agent/stop` (daemon → runtime) start and stop an
    agent's process;
  - `agent/message` carries one ACP JSON-RPC line in either direction,
    unchanged; `agent/exited` reports an exit with its status and stderr
    tail;
  - the daemon's `local` runtime does the same in process.

## 8. The agent's own permission requests

Each `session/request_permission` is decided by the agent's merged rules,
its ACP tool kind mapped onto cookie's actions:

| ACP kind | Action | Resource |
|---|---|---|
| `read`, `search` | `read` | the location's path |
| `edit`, `delete`, `move` | `write` | the location's path |
| `execute` | `bash` | the command (from the call's raw input, else its title) |
| `fetch` | `webfetch` | the URL |
| `think`, `switch_mode`, `other`, unknown | `acp` | the kind, then the title |

- `bash: {"git *": allow}` allows the agent's own `git` commands exactly as it
  allows cookie's `bash` tool. A request with several locations needs every
  one allowed. Without an `acp` key, `acp` defaults to `ask`.
- **Answers:** `allow` picks the agent's allow-once option; `deny` its
  reject-once option; `ask` goes through the approval flow (the reviewer in
  auto modes), and the answer picks allow-once or reject-once. Cookie never
  picks an "always" option: repeats come back to cookie's rules and grants.
- In the agent's auto mode most calls never ask; those that do still meet the
  rules.

## 9. Cookie's tools for the agent (MCP over the runtime)

- On `session/new` cookie passes one stdio MCP server, **`cookie`**, whose
  command is a bridge on the runtime's machine:
  `cookie runtime mcp-bridge --socket <path>`.
- **The bridge** connects to the runtime process over a local socket (a Unix
  socket, a named pipe on Windows) created for that ACP session, readable
  only by the runtime's user, removed with the session. The runtime relays
  its MCP messages to the daemon (`agent/mcp` frames), and the daemon serves
  them.
- **The tools:** the session's tools as cookie composes them for a model
  request (memory, cron, `list_agents`, `delegate_subagent` and the subagent
  tools, MCP servers' tools, runtime tools), minus `acp.hide_tools`.
- **Permissions apply as for a model:** a tool the merged rules hide is not
  listed; a call is decided by the rules, `ask` goes through the approval
  flow.
- Calls through the bridge appear in the session as calls of the ACP agent,
  linked to its turn.
- The bridge needs `cookie` on the runtime's machine, which a runtime always
  has: it is `cookie runtime`.

## 10. Agent commands

- `available_commands_update` gives the session's agent commands (name,
  description, input hint). The TUI and web composers list them with cookie's
  own commands, marked as the agent's.
- Choosing one sends `/<name> <input>` as the prompt text, as ACP specifies.

## 11. Protocols and storage

- **Session log:** a new event records the ACP session (agent, runtime, ACP
  session id, cwd, protocol version, agent info). Tool calls from
  `acp:<agent>` use the existing events with a new tool source.
- **Client protocol minor:** runtime-hosted agents in the agent list (with
  their runtime and availability), the session's agent commands and options,
  an RPC to set an option, the plan block, the `acp:<agent>` tool source.
- **Runtime protocol minor:** agent declarations and the relay (section 7),
  the MCP bridge frames (section 9).
- **Plugin protocol minor:** the new events.
- **Config:** `[runtimes.*.acp.agents.*]`, `[acp] idle_shutdown_ms`, and the
  agent document's `acp` block.

## 12. Tests

A scripted ACP agent fixture (like the MCP fixture) that speaks ACP v1:
declarations on `local`, a template runtime, and a dial-in runtime, with
dotted ids and collisions; offline refusal; handshake and capabilities;
auto-mode selection and `acp.config`; a turn with chunks, thoughts, tool calls
with diffs, a plan and usage; every stop reason; cancel with a pending
permission request; permission requests decided by the mapped rules and by
approvals; resume, load with an ignored replay, and a new session after a lost
one; process exit; the MCP bridge listing and gating cookie's tools with
`hide_tools`; the body sent once; agent commands; delegation to an ACP
subagent; a cron fire into an ACP session; cwd with and without a workspace on
the runtime.

## 13. Phases

1. **Runtime relay:** declarations, agent ids, `agent/*` frames, the local
   in-process runtime, dial-in and template runtimes.
2. **Engine:** the ACP client and runner, rendering, sessions and processes,
   permission mapping, the daemon-side document, protocol additions.
3. **MCP bridge:** `cookie runtime mcp-bridge`, the runtime's socket, the
   daemon's MCP service with `hide_tools`.
4. **Clients:** runtime agents in pickers, agent commands and options, the
   plan block, ACP tool calls.
5. **Docs:** an ACP agents guide with Claude Code, Codex, and Gemini CLI
   examples on `local`, a template runtime, and a dial-in runtime.

## 14. Open questions

None.

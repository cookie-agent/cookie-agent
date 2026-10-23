# ACP agents

Status: **approved** (2026-10-23), being implemented: phases 1 to 3 landed
(2026-10-23; see [Implementation notes](#15-implementation-notes)); phases 4
(clients) and 5 (docs) remain.

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
  MCP servers. The daemon registers each as **`<runtime_id>-<agent_id>`**,
  the runtime ID's `_` spelled `-` (`local-claude`, `labpc-codex`,
  `client-myhost-codex`), available while that runtime is connected. The
  runtime travels next to the ID wherever agents are described; it is never
  parsed from the ID. *(Decided 2026-10-23, replacing the dotted form, which
  the agent ID grammar does not allow and could not gain within a minor.)*
- **The runtime spawns the agent** on its own machine and relays its ACP
  messages to the daemon over the runtime protocol. **The daemon speaks
  ACP**: sessions, rendering, permissions, cancel, resume.
- **Cookie's tools reach the agent as an MCP server over stdio**, through a
  bridge the runtime spawns: the agent's MCP traffic reaches the daemon over
  the runtime's existing connection, so the agent's host needs no route to
  the daemon. (MCP-over-ACP, which would carry MCP on the ACP connection
  itself, is a draft RFD; not used.)
- **Cookie settings come from a daemon-side agent document with the same id**
  (`~/.cookie-agent/agents/labpc-codex.md`): mode, permissions, body,
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
  letters, digits, and single inner hyphens, at most 64 bytes). The daemon's
  id is `<runtime_id>-<agent_id>`, the runtime ID's `_` spelled `-`; the
  agent's runtime is carried explicitly where agents are described
  (`AgentDescriptor.runtime`) and kept in the engine's registry entry, never
  parsed from the id. Refused, with a message naming both sides: a
  combination that is not an agent id (a runtime ID with `__` or a trailing
  `_`), one longer than 64 bytes, and one that names a daemon agent or
  another runtime's agent (`lab_pc` + `codex` and `lab` + `pc-codex`).
- **Reporting:** the runtime reports its agents when it connects and when its
  configuration changes; the daemon adds or removes them and announces the
  change like any agent registry change.
- **Offline:** an agent whose runtime is not connected is listed as
  unavailable; starting a session or a delegation with it is refused at once,
  as for offline runtimes.

## 4. The daemon-side agent document

```yaml
# ~/.cookie-agent/agents/labpc-codex.md
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

- **Optional.** Without it, `<runtime_id>-<agent_id>` is an agent of mode
  `all` with the runtime's description, the defaults every agent gets for
  actions it names no key for, and the default `hide_tools`.
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

Each `session/request_permission` is decided by its ACP tool kind, mapped
onto cookie's actions and the agent's merged rules *(revised 2026-10-23:
no `acp` action)*:

| ACP request | Decided as | Resource |
|---|---|---|
| `read`, `search` | `read` | the location's path |
| `edit`, `delete`, `move` | `write` | the location's path |
| `execute` | `bash` | the command (from the call's raw input, else its title) |
| `fetch` | `webfetch` | the URL |
| a tool of cookie's own MCP server (`mcp__cookie__<tool>`) | allowed at once | none: the call reaches cookie's MCP server, where the rules decide it |
| a tool of an MCP server the agent connects itself (`mcp__<server>__<tool>`) | `mcp` | `<server>_<tool>`, cookie's own name for the tool |
| `think` | allowed | none |
| `switch_mode`, `other`, unknown | the approval flow | the title |

- `bash: {"git *": allow}` allows the agent's own `git` commands exactly as it
  allows cookie's `bash` tool, and `mcp: {"exa_*": allow}` covers cookie's
  and the agent's `exa` tools alike. A request with several locations needs
  every one allowed.
- Cookie's own tools are asked about at most once: the agent's request is
  allowed, and the call that reaches the bridge meets the rules (section 9).
- An MCP tool is recognized only by a title or raw input of one of the
  adapters' whole forms (`mcp__<server>__<tool>`, `Tool: <server>/<tool>`
  with `{server, tool}`, `<tool> (<server> MCP Server)`), and only for kind
  `other` or a kind ACP does not know, so a command or a path that merely
  contains such a name is still decided as a command or a path.
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

## 15. Implementation notes

**Phases 1 to 3** (2026-10-23): client protocol 1.17.0, plugin protocol
1.8.0, runtime protocol 1.7.0; history format unchanged (two new event
types).

Where the spec left a detail open, or where the code differs:

- **Ids.** `<runtime>-<agent>` with the runtime's `_` spelled `-` (user
  decision of 2026-10-23); `cookie_agent_protocol::runtime_agent_id` makes
  it, and `AgentDescriptor.runtime` (`runtime_id`, `agent`, `available`)
  carries the runtime. Collisions among the daemon's own templates and with
  daemon documents fail the configuration load; a dial-in runtime's refused
  declaration is skipped and reported in the daemon's log.
- **Availability.** Agents of `local` and of templates are available
  whenever the daemon runs (it starts their runtime on demand); a dial-in
  runtime's are available while it is connected and listed unavailable, as
  it last reported them, while it is away. A disabled agent is listed
  unavailable. A best-effort selection (an engine wake-up, a client
  continuing) never swaps an ACP agent for another: the run is refused.
- **Declarations.** Template and `[runtimes.local]` agents change with the
  rest of `runtimes`, at the next start. A dial-in runtime reads its
  machine's configuration again every 5 seconds and sends
  `runtime/agents_changed`. The relay messages are information; no
  `agent/configure` was needed.
- **The agent document.** Its `acp` block marks it as an ACP agent's (`acp:
  {}` is enough); such a document is an agent only while a runtime declares
  the id. It names no `models`, `agent_md`, `limits`, or mode `internal`,
  and its body may be empty. `models` and `enabled` became optional for
  every document (`enabled` defaults to `true`). A delegation target
  needs no document *(user decision of 2026-10-23)*: a named target no agent
  has, and a pattern that matches none, load as configuration diagnostics
  (`ConfigStatus.diagnostics`, client protocol 1.18.0, recomputed from the
  published agents, so a diagnostic clears once a runtime reports the
  agent), the name is not a target meanwhile, and delegating to it is
  refused with "no agent `<id>` (its runtime is not connected)". A target
  whose document exists but is disabled or `primary` still fails the load.
- **Synthetic model.** Runs record the model `acp/<agent id>` (a model key
  needs a `/`, so not `acp:<agent>`), adapter `acp`, through a persisted
  binding no build can run as a model; the frozen policy has no model
  suffix. Each prompt round's steps are attempts of that model.
- **Rendering.** Text and thoughts stream as deltas. A new ACP tool call
  commits the turn so far as a model turn holding the call (finish
  `tool_calls`), then starts a cookie tool call of source `acp`; updates are
  its progress (a display replacement), and `completed`/`failed` end it,
  with diffs as diff blocks, content as text blocks, locations as links, and
  raw input and output in `metadata.acp`. Calls left open when the round
  ends end as failed (cancelled when the turn was). The ACP tool call id is
  the model call id, made unique within the run.
- **Events.** `acp_session_started` records the ACP session (agent, runtime,
  the agent's own id, session id, cwd, protocol version, agent info, and
  `how`: new, resumed, or loaded). `acp_session_updated` carries the
  agent's `commands`, `options`, and `plan`, each the whole new value. The
  plan is a `plan` display block there, not inside a tool call. An agent
  that offers modes but no mode option shows them as an option `mode` of
  category `mode`, set with `session/set_mode`.
- **Sessions.** Updates that arrive before a session is routed (the commands
  an agent announces right after `session/new`) are kept for it; turn output
  that arrives before a prompt (a load's replay) is dropped. A replaced
  session is announced with a runless `diagnostic` (kind `acp_agent`). The
  body goes with the first prompt of every new ACP session, so a replaced
  session gets it again. A round's prompt is the input the session took
  since its last committed model turn.
- **Processes.** Started in the runtime's home directory; idle means no run
  in progress for `[acp] idle_shutdown_ms`. `session/close` is sent when a
  session is deleted (not when it unloads). The engine stops every process
  as it shuts down.
- **Permissions.** Requests go through the tool permission pipeline (grants
  included) with the agent's runtime as the scope and the session's cwd as
  `${workspace_dir}`. The `acp` action of phase 2 is gone *(user decision of
  2026-10-23)*: a request for one of cookie's own tools is allowed at once
  (the bridge decides the call), the agent's own MCP tools are `mcp` rules,
  `think` is allowed, and `switch_mode` and the rest go to the approval flow
  as an approval of the kind's label (`switch_mode` or `other`) with no
  tree grant offered. `PermissionAction` is an open label and `acp` was
  never in `BUILT_IN`, so dropping the constant changes no wire. A request whose resources
  cannot be read is rejected; an agent that offers no allow-once or
  reject-once option is answered `cancelled`. Approvals name the cookie
  tool call the agent's call is shown as. The approval reviewer of the auto
  modes runs on the run's model, which an ACP run lacks, so its requests
  escalate to the user.
- **Usage.** The prompt answer's `usage` when the agent sends one, else the
  last `usage_update`'s `used` as input tokens; a cost only from a
  `usage_update` cost in USD. Recorded once per round, on its last turn.
- **Titles.** `session_info_update` sets a fallback title while the user has
  set none; ACP runs generate no title of their own (the title agent needs
  a model).
- **Not run for ACP agents:** `AGENTS.md` loading, the skill listing and tool
  prompt sections, the `agent_before_start` hook, and input media admission
  (images go to an agent whose `promptCapabilities.image` is set; other
  files become a note).
- **The bridge.** The daemon's MCP server is hand-written (`initialize`,
  `ping`, `tools/list`, `tools/call`); rmcp's server features are not used.
  A bridged call runs inside the agent's turn as a cookie tool call of the
  run (the runner commits the turn so far with the call in it), through
  preparation, the rules, approvals, and execution; outside a turn it is
  refused. The agent usually reports the same call as its own tool call
  too. On Windows the pipe has a random name and refuses remote clients but
  keeps the default security descriptor, not one limited to the user.
- **ACP client.** Hand-written JSON-RPC types over the protocol crate's peer
  (the `agent-client-protocol` crate was not needed); `initialize`
  advertises no `fs` and no `terminal`. A runtime that cannot start an
  agent answers the new error `agent_failed`.

**§12 coverage** (`crates/engine/src/runtime/tests/acp.rs` against the
scripted agent `crates/engine/tests/fixtures/acp_agent.py`,
`crates/coding_runtime/src/server/acp_tests.rs`,
`crates/config/tests/acp_agents.rs`, and the `cookie runtime mcp-bridge`
test in `crates/cookie_agent/tests/runtime_command.rs`): declarations on
`local`, a template (configuration), and a dial-in runtime, dotted-free ids
and collisions; offline refusal; handshake and capabilities; auto mode, modes
without a mode option, and `acp.config`; a turn with chunks, thoughts, a
tool call with a diff, a plan, and usage; every stop reason and an agent
error; cancel with a pending permission request; permissions by the mapped
rules and by an approval; resume, load with an ignored replay, and a new
session after a lost one; process exit; the bridge listing and gating
cookie's tools with `hide_tools` and calling one; the body sent once; agent
commands; delegation to an ACP subagent through the bridge; a cron fire
into an ACP session; cwd with and without a workspace on the runtime; an
agent that connects its MCP servers while it answers `session/new`. Not
covered: steering into a running ACP turn, which queues for the next round.

**Checked by hand against Claude Code** (2026-10-23; `claude` 2.1.309,
`@agentclientprotocol/claude-agent-acp` 0.94.0, on `local` of a scratch
daemon): the handshake, `auto` mode chosen from the agent's mode option, a
reply, a `Read` call rendered with its content and location, the cookie
bridge's tools listed by the agent without the hidden ones, `ls` allowed and
`touch` rejected by the mapped `bash` rules, a cancel, and a resume across a
daemon restart (the agent kept the conversation). What the adapter does that
the spec did not anticipate:

- It connects its MCP servers while it answers `session/new`, so the bridge
  is bound to the session before the request goes out (a test covers it).
- It offers `resume` and options `mode`, `model`, `effort`, and `fast`; it
  announces its commands twice right after a session opens, and a title
  (`session_info_update`) between turns.
- A tool call's `rawInput` arrives in a later update, so the call's
  presentation carries none; its result has it.
- Outside its auto mode it asks permission for the calls it makes of
  cookie's own tools, as kind `other` titled `mcp__cookie__<tool>`. These
  are now allowed at once, so the bridge's rules decide such a call alone
  (section 8).

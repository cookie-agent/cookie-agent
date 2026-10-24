# ACP agents

Status: **implemented** (2026-10-23); see
[Implementation notes](#15-implementation-notes). The user guide is
[ACP agents](../extend/acp-agents.md).

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
  the agent ID grammar does not allow and could not gain within a minor.
  Superseded the same day: the ID is the declaration's key, and the
  session's attachments decide which runtime runs it; section 15,
  naming.)*
- **The runtime spawns the agent** on its own machine and relays its ACP
  messages to the daemon over the runtime protocol. **The daemon speaks
  ACP**: sessions, rendering, permissions, cancel, resume.
- **Cookie's tools reach the agent as an MCP server over stdio**, through a
  bridge the runtime spawns: the agent's MCP traffic reaches the daemon over
  the runtime's existing connection, so the agent's host needs no route to
  the daemon. (MCP-over-ACP, which would carry MCP on the ACP connection
  itself, is a draft RFD; not used.)
- **Cookie settings come from a daemon-side agent document with the same id**
  (`~/.cookie-agent/agents/labpc-codex.md`; since the naming of section 15,
  `agents/codex.md`, shared by every runtime that offers `codex`): mode, permissions, body,
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
  empty body sends nothing. *(Extended 2026-10-23: the first prompt also
  carries the tool instructions and the memory snapshot; section 15,
  round 3.)*
- **Slash commands** the agent offers appear in the TUI and web composers.
- **`deny`** stays the spelling for turning a permission off.

**Out of scope for v1:** ACP `fs` and `terminal`, logins run from cookie,
ACP v2, MCP-over-ACP, the agents' vendor extensions (`_meta`, AIR, goal).
*(Revised 2026-10-23: the steering extension is used when advertised, and
`_meta` enriches tool calls where it helps; section 15, round 3.)*

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
- **Steering** falls back to queueing: ACP has no standard steer. *(Revised
  2026-10-23: through the agent's steering extension when it advertises
  one; section 15, round 3.)*
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
  session is deleted or unloads; a deleted session's ACP sessions are also
  deleted (`session/delete`) since the lifecycle round below. The engine stops every process
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
  `usage_update` cost in USD. Recorded once per round, on its last turn,
  whether the round completed, failed, or was cancelled *(since the
  lifecycle round of 2026-10-24; before, only a completed round's)*.
- **Titles.** `session_info_update` sets a fallback title while the user has
  set none. *(Revised 2026-10-24: each new agent title applies until the
  user names the session, and an untitled session gets cookie's generated
  title after a round; see the lifecycle round below.)*
- **Not run for ACP agents:** `AGENTS.md` loading, the skill listing and tool
  prompt sections, the `agent_before_start` hook, and input media admission
  (files go to the agent as its prompt capabilities allow, others become a
  note; see **Input and pickers** below).
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
  advertises no `fs` and no `terminal` (it asks for terminal output, see
  the display round below, and declares `elicitation` `{form, url}`, see
  elicitation below). A runtime that cannot start an
  agent answers the new error `agent_failed`.

**Revisions of 2026-10-23** (user decisions; client protocol 1.18.0):
the `acp` permission action is gone (section 8), and a delegation target
no agent has yet is a configuration diagnostic rather than a load error
(see **The agent document** above). `ConfigStatus.diagnostics` carries the
diagnostics in `config.get`, `config.apply`, and `config.reload`; `cookie
config status`, the TUI's `/config`, and the web's configuration page show
them.

**Phase 4, the clients** (2026-10-23; no wire change beyond 1.17.0):

- **Pickers.** The agent pickers list the root-runnable agents and the ACP
  agents that would be but are unavailable (runtime away, or disabled),
  with their runtime (`[on lab · unavailable]`), dimmed and refused with
  the reason. A draft of an ACP agent runs its `acp/<agent>` model, which
  the clients treat as live though no catalog lists it; the model pickers
  list only the catalog's models, and choosing the model of an ACP draft
  opens its options instead (`/options` in the TUI, **Agent options** from
  the web's model chip, which shows the agent's `model` option).
- **Commands.** The reducer folds `acp_session_updated` into the session's
  `acp` view (commands and options, each the latest whole value). The TUI
  lists the agent's commands in the command palette after cookie's own,
  marked `(agent)`; one with an input hint asks for its input in a text
  step. The web client, whose composer has no command palette, lists them
  above the composer while the draft is `/` and a prefix of a name; one
  that takes input puts `/<name> ` in the composer, one that takes none is
  sent. Both send `/<name> <input>` as an ordinary prompt.
- **Options** are set with `session.agent_option.set`, and its answer
  replaces the session's options at once (the daemon's
  `acp_session_updated` follows).
- **Plan.** A `plan` in `acp_session_updated` is a transcript row per run,
  replaced in place by the run's later plans, after the run's reply: a
  checklist with `[x]`, `[~]`, and `[ ]` (an icon and a screen-reader
  label in the web) and a high or low priority. A `plan` block inside a
  tool's display renders the same way.
- **Tool source.** A call of source `acp` names `<agent> on <runtime>` after
  its title (in the web, a `data-tool-source` span).
- No new pointer target: the TUI's `/options` is a picker modal on the
  existing `PickerRow` targets, and palette rows are `PaletteRow`s.

**Phase 5, the docs** (2026-10-23): the [ACP agents
guide](../extend/acp-agents.md) (Claude Code, Codex, and Gemini CLI on
`local`, an ssh template, and a dial-in runtime; the daemon-side document;
permissions; `hide_tools`; the bridge; the clients; delegation;
troubleshooting), with the TUI, web, agents, runtimes, and `[acp]` pages
pointing to it.

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
agent that connects its MCP servers while it answers `session/new`; the
revised mapping (cookie's bridged tools by each adapter's naming, the
agent's own MCP tools as `mcp`, `think`, and `switch_mode` and unknown kinds
through approvals); a delegation target no runtime has reported, refused and
a diagnostic until a dial-in runtime reports it. Clients:
`crates/client_state/src/acp.rs` and the reducer test,
`crates/tui/src/ui/transcript/tests/acp.rs`, the web plan block test, and
the `acp_agents` browser scenario (`scripts/web-e2e`, the fixture on the
`acp` group daemon's `local`). Not covered: steering into a running ACP
turn, which queues for the next round.

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

**Round 3** (2026-10-23; user decisions of the same day; client protocol
1.20.0, plugin protocol 1.9.0; history format unchanged):

- **First-prompt context.** The first `session/prompt` of an ACP session
  carries, as separate text blocks before the input and each left out when
  empty: the `<tool_instructions provider=...>` sections cookie would
  compose for a model run, for exactly the tools the agent is offered over
  the bridge (after `hide_tools` and the merged rules: a provider gives its
  sections only when one of its tools is offered, and memory's section has
  part A when a memory tool is offered and part B when a write tool is);
  the session's memory snapshot as a model run sees it (the latest
  `memory_loaded`, which ACP runs record at admission like model runs, so
  it is there even when no memory tool is offered); and the agent
  document's body. No cookie-written preamble or guidelines: those belong in
  the agent document, and the [guide](../extend/acp-agents.md#a-reference-document-for-claude-code)
  has a reference document for Claude Code. `AGENTS.md` is not sent. The
  context goes with the first prompt of every new ACP session, so a lost
  session's replacement gets it again; a resumed or loaded one does not.
- **Prompt boundary.** A round's prompt is now the input after the
  `input_through_seq` of the session's last committed turn (was: after that
  turn's own sequence number). The two agree for every round so far; the
  new one also covers input taken to steer that the agent did not take.
- **Steering.** An agent advertises the steering extension with
  `_meta.steering.supported: true` in its `initialize` answer (top-level
  `_meta`, not `agentCapabilities`; Claude Code's adapter 0.95.0 and
  Codex's do). The client declares nothing to enable it. While a round runs,
  input admitted to steer (a user's steer, a producer message of mode
  `steer`: a cron `steer` fire, a subagent report) wakes the runner, which
  commits the turn so far, promotes and claims the input as a round does,
  and sends `_session/steering` `{sessionId, prompt, _meta: {steering:
  {idleBehavior: "promptRequired"}}}`. `injected` (and `startedNewTurn`, an
  agent that does not know `idleBehavior`) means the agent took it: the
  turns after cover the input, and `acp_session_updated.steered` records
  which (`user_inputs`, the `user_input_submitted` sequence numbers, and
  `producer_messages`). `promptRequired` (no turn running), `failed`, or an
  error leave the input for the next round, which the run then takes even
  when nothing else is pending; an error is reported in the session as a
  diagnostic. A steer still waiting when the round ends is awaited for up
  to 10 s. Claude Code's adapter also advertises
  `agentCapabilities._meta.claudeCode.promptQueueing` (a second
  `session/prompt` while one runs is queued by the agent); cookie does not
  use it, since it queues input itself. `acp_session_started.steering`
  records whether the agent steers; clients derive the Steer button from it
  (the web client's reads "Queue (this agent can't be steered)", the TUI's
  pending strip says so) when the session's running agent is that ACP
  agent.
- **Tool calls.** A call's presentation is rebuilt from what the agent
  reported so far (title, the kind's hint, a primary argument from the raw
  input's `command`, `file_path`, `path`, `url`, `pattern`, or `query`, and
  the raw input as arguments) and recorded as
  `acp_session_updated.tool_presentation` whenever it changes, since Claude
  Code's adapter sends the raw input in a later `tool_call_update` (and
  then once more per streamed field). Clients replace the call's title,
  hint, primary argument, arguments, and preview with it. Content renders
  as Markdown (text), media (images, stored as artifacts), links (resource
  links), code (embedded text resources), and diff blocks; locations as
  file links; raw output as code or JSON when the call has no content.
  `_meta` is optional enrichment only: Claude Code's
  `claudeCode.toolName` becomes the call's published name and
  `metadata.acp.tool_name`; its `claudeCode.toolResponse` (the whole tool
  response, including written files) is not kept.
- **Notices and compaction.** `initialize` declares
  `clientCapabilities.session: {notices: {}, compaction: {}}`. A `notice`
  update (`severity`, `title`, `description`) and a completed or failed
  `compaction_update` (its `summary`, else the streamed
  `compaction_summary_chunk`s; its `error`) are recorded as
  `acp_session_updated.notice`; clients show it where it arrived, with no
  level filter. In progress and cancelled compactions show nothing.
- **Usage and session info** were already handled (`usage_update`,
  `session_info_update`); Claude Code's `PromptResponse._meta.quota` is not
  read.
- **Checked by hand against Claude Code** (`claude` 2.1.309, adapter
  0.95.0, scratch daemon): the first prompt's context; a `Read` (its raw
  input arrived after the call started and the header updated; content as
  Markdown and a file link); an `Edit` (presentation updated three times as
  `old_string`, `new_string`, and `replace_all` streamed in; a diff); a
  `Bash` call; a steer during a running `sleep` (`injected`, and Claude
  moved the foreground command to the background so the message could
  reach it); and a cancel. The adapter sent no `notice` or
  `compaction_update` in these short turns; it did send
  `current_mode_update`, three `session_info_update`s, and a background
  task's follow-up text after a turn ended, which cookie drops as output
  between rounds.

**Tests** (round 3): the first prompt's parts and their order, with and
without memory write tools and with every memory tool hidden; a steer
through the fixture's extension (one round, the `steered` record, the
request's `idleBehavior`) and without it (the next round); a cancel after
a steered turn; a call whose raw input arrives later (the recorded
presentation, Markdown, media, and link content, the tool name); notices
and a compaction; the client reducer, the TUI, and the web rendering of
notices, steered messages, and presentations.

**Naming** (2026-10-23, user decision; client protocol 1.21.0, no runtime
or plugin protocol change):

- **IDs come straight from the declaration.** `[runtimes.local.acp.agents.claude]`
  is the agent `claude`; the `<runtime>-<agent>` ID, the `_`-to-`-`
  spelling, and the refusals of combined IDs that are malformed, too long,
  or shared are gone (`cookie_agent_protocol::runtime_agent_id` is
  removed). Every runtime that declares an ID offers the one agent of that
  ID (the engine's `AcpOffers`), sorted by runtime ID.
- **The session's attachments decide where it runs.** A turn of `claude`
  runs on the attached runtime that offers it. With none attached, or with
  two or more, it cannot run in that session: `session.create`, a run's
  admission (before `run_started`), and a delegation (against the child's
  attachments: the parent's inherited ones, or the delegated capabilities;
  a resumed child is checked when its run starts) are refused with
  `` `claude` needs a runtime that offers it attached: labpc, local `` or
  `` `claude` is offered by more than one attached runtime (labpc, local);
  a session runs it on one, so detach the others ``; an offering runtime that is
  away or has the agent disabled refuses with that reason. A session whose
  current agent is an ACP agent cannot attach a second runtime that offers
  it (`runtime.attach` is refused, naming both); sessions of other agents
  attach any runtimes.
- **One document per ID.** `~/.cookie-agent/agents/claude.md` serves every
  runtime that offers `claude`. Without one, the default document takes the
  first offering runtime's description (in runtime ID order).
- **Assumptions** (recorded for the user to adjust):
  - A daemon document *without* an `acp` block whose ID a runtime declares
    wins as the daemon's own agent; the declaration is ignored and
    `ConfigStatus.diagnostics` says so (`ACP agent \`claude\` of runtime
    \`local\` is ignored: ...`). This used to fail the configuration load.
  - When the hosting runtime is detached and another attached runtime
    offers the agent, the next turn starts a new ACP session there, with a
    diagnostic in the transcript: the old ACP session lives on the old
    machine.
  - A delegated child resolves its ACP agent against its own attachments
    with the same one-provider rule.
  - The ACP session record keeps the hosting runtime (`runtime_id`), and
    resume and load require it: the latest record for the agent is resumed
    only when its runtime is the one that runs the agent now.
- **Synthetic binding.** `acp/<agent>`'s binding no longer names a runtime
  (`cookie-agent/acp-binding/v2`, `frozen: {"acp": {"agent"}}`); a run's
  policy names the agent, and the runner places it on a runtime.
- **Protocol.** `AgentDescriptor.runtimes` lists every offering runtime
  (`runtime_id`, `agent`, `available`); `runtime` stays populated when
  exactly one runtime offers the agent, so a client older than 1.21.0
  lists an agent several runtimes offer as a daemon agent.
  `runnable_as_root` is true while some offering runtime could run it; the
  clients compute where it runs from the draft's or session's attachments
  (`cookie_agent_client_state::acp::placement`): the TUI's and web's agent
  pickers show `[on local]`, `[needs labpc, local attached]`,
  `[offered by labpc, local: attach one]`, or `[on lab · unavailable]`, and
  refuse the ones that cannot run with the reason.
- **Model-facing tools.** `list_agents` marks an ACP agent `(<mode>, acp)`,
  drops its synthetic model line, and adds `offered by runtimes: labpc,
  local` and `runs here: yes, on runtime local` (or `no — needs one of:
  labpc, local attached`, `no — two attached runtimes offer it: labpc,
  local`, `no — runtime lab is not connected`, `no — disabled on runtime
  local`), judged against the calling session's attachments (a child
  inherits them). `list_runtimes` gives each runtime an `acp_agents`
  attribute (`acp_agents="claude, codex"`). Both tool descriptions say so.
- **cwd.** With the agent's runtime always attached, the runtime's home is
  the working directory only when that attachment has no workspace.
- **Delegation diagnostics** resolve unknown names against the plain IDs:
  a target clears once any runtime declares it. A refused delegation to a
  name no runtime offers yet says `no agent \`codex\` (no connected runtime
  offers it)`.

**Tests** (naming): two runtimes offering one ID (the descriptor's
`runtimes`, a turn placed on `local`, `list_agents` and `list_runtimes`,
attaching the second refused, a move to `labpc` starting a new ACP
session with the notice, none or both attached refused at creation); a
session and a run without the agent's runtime refused before the run
starts; a daemon document without an `acp` block shadowing a declaration
(the configuration diagnostic); the client placement and picker notes; the
`list_agents` placement lines.

**Input and pickers** (2026-10-24; client protocol 1.26.0, plugin
protocol 1.14.0; history format unchanged):

- **Model and effort as cookie's model and variant.** ACP dropped
  `session/set_model`: an agent's model is a config option of category
  `model`, its reasoning effort one of category `thought_level` (Claude
  Code's adapter: `model` and `effort`; Codex's: `model` and
  `reasoning_effort`). The clients show them as the model and variant
  (`cookie_agent_client_state::acp::model_label`, `Opus • High`; the TUI's
  title `claude • Opus[High]`), the model picker lists the model option's
  values and the variant (the TUI's bracketed variant and ctrl+t, the web
  sheet's effort buttons) the effort option's, and choosing one is
  `session.agent_option.set`. The other options stay in `/options` and the
  web client's **All agent options**. An option is the model by its ID
  `model` when none has the category. Gemini CLI's older `models` API is
  not read.
- **Drafts.** The daemon remembers, per agent and runtime, the options a
  new ACP session answered `session/new` with (the agent's own starting
  values, before cookie sets any), and keeps them in `acp-agents.json` in
  its data folder, a cache it rewrites when they change and ignores when
  unreadable. A resumed or loaded session's options are taken only while
  nothing is known (their values are that session's). They reach clients as
  `AgentRuntime.options`, with the document's `acp.mode` (or `auto`) and
  `acp.config` applied, so a draft shows what its session would start with;
  a change republishes the agents. A draft's choices are kept per agent
  (`DraftAgentOptions`) and sent as `session.create`'s `agent_options`;
  they need an ACP agent, at most 64, each option once. The session records
  them as `acp_session_updated.starting_options`, and every new ACP session
  of that cookie session applies them after `acp.config` (a session
  created but not started yet shows them over the remembered options);
  one the agent does not offer, or a value it does not take, is skipped
  with a diagnostic as `acp.config` is. *Assumption:* they apply to every
  new ACP session of the cookie session, a lost session's replacement
  included, not only the first.
- **Files as prompt input.** An agent's `promptCapabilities` decide how a
  file the user attached reaches it (`acp/prompt.rs`): an image as an
  `image` block (as before), audio as an `audio` block when it advertises
  `audio` (Gemini CLI does), and a text file as a `resource` block
  (`uri` `attachment:///<file name>`, `mimeType`, `text`) when it
  advertises `embeddedContext` (Claude Code's and Codex's adapters do).
  Anything else, PDFs and video included, stays the note `[attached file
  <name> (<type>), which this agent cannot read]`: ACP's blob resources
  would carry them, but Claude Code's adapter drops blob resources, so a
  note says more. Uploads take text files: content that is no media file
  but UTF-8 without NUL bytes is `text/plain` of the new media kind
  `text` (an open label), up to 1 MiB (`MAX_TEXT_ATTACHMENT_BYTES`);
  models list no `text`, so a model run still refuses one at admission.
  Audio was already admitted (`audio/flac`, `mpeg`, `ogg`, `wav`, 20 MiB).
  The clients check attachments against `AgentRuntime.input_media`, which
  the daemon records from the agent's `initialize` answer each time it
  starts (image, audio, and text entries with the upload types and caps,
  20 files of a kind), since the synthetic model is in no catalog; the web
  client's file picker offers every file when the agent takes text.
- **Protocol shape.** `session.create` rather than the first `run.start`:
  the draft becomes a session there in both clients (attachments are
  uploaded into it first), the choices are the session's like its
  permission overlay, and recording them at creation keeps `run.start`
  free of a member that would mean something only once.

**Fixes** (2026-10-23):

- **Processes stop with the daemon.** `agent/stop` and a runtime that
  stops (the end of its connection, `runtime/shutdown`) kill each agent's
  process group and answer only once it has been reaped; before, the kill
  ran after the answer, so a daemon that exited right after stopping its
  agents (its shutdown, SIGTERM, or `daemon.restart`) could leave an agent
  and its children running.
- **A cancel always reaches the agent.** The round's loop polls the run's
  cancellation first (a biased select), so an agent that streams without a
  pause cannot hold `session/cancel` back; a steer being taken when the run
  is cancelled is dropped instead of sent; and a turn the agent ends after
  the cancel was accepted ends the run as cancelled, as a model run's does.
  The manual check of round 3 saw an accepted `run.cancel` that never sent
  `session/cancel` while Claude Code streamed updates; this was the window.

**Display round** (2026-10-24; client protocol 1.23.0, plugin protocol
1.11.0; history format unchanged):

- **Context.** Each `usage_update` whose `used` or `size` changed is
  recorded as `acp_session_updated.context` (`used`, and `size` when it is
  not 0). ACP turns still commit with empty usage, since the turns' usage
  feeds the session's usage totals and the round's usage is recorded once
  (above); the reducer keeps an ACP agent's context across its turns
  (a turn of the `acp` provider leaves it standing) and takes the window
  from `size`, which no catalog lists for `acp/<agent>`. The TUI's status
  line shows `ctx` with the percentage of that window, the web's the tokens
  (the window in its title). All three adapters send `usage_update`.
- **Mode updates.** `current_mode_update` moves the current value of the
  option that shows the agent's modes (id `mode` or category `mode`) when it
  lists the new mode, whether cookie made the option of the agent's modes
  or the agent offers its own mode option beside them, as Claude Code's
  adapter does; before, only the made-up option followed, and Claude Code
  leaving plan mode by itself left the chip on `plan`.
- **Terminal output.** `initialize` declares
  `clientCapabilities._meta.terminal_output: true` (and still `terminal:
  false`: cookie runs no `terminal/*`). Claude Code's and Codex's adapters
  then report an execute call's output in `_meta`: `terminal_info`
  (`terminal_id`) names the call's terminal, `terminal_output` or
  `terminal_output_delta` (`terminal_id`, `data`) carry output, which cookie
  appends in both cases (Codex sends chunks under either key, Claude Code
  the whole output once), and `terminal_exit` (`exit_code`, `signal`) the
  end. The runner (`acp/terminal.rs`) treats them as cookie's `bash` tool
  treats a command: the output goes to an output capture (retained, an
  artifact past the preview, `retained_output` on the result) and the
  call's output hub, and the live display gets a `terminal` block, replaced
  once with the display and then grown by appends to it, up to the display
  budget. A `terminal` content item renders that block, with the exit code,
  where it stands (first when no item names it), plus a line for a signal;
  the old `[terminal <id>]` placeholder is gone. The result's output is the
  capture's preview. A failed call keeps its output as the termination's
  partial result, marked incomplete like every failed call's, and says
  `the command exited with code <n>` when its content says nothing; calls
  left open when the round ends keep theirs the same way.
- **Boolean options.** `initialize` declares
  `clientCapabilities.session.configOptions.boolean: {}`, so an agent sends
  its on/off options as `type: boolean` (Claude Code's `fast`, category
  `model_config`) rather than an `on`/`off` select. `AcpConfigOption.kind`
  (`select`, left out, or `boolean`) says which; a boolean's
  `current_value` is `true` or `false` and its `values` list both (`true`
  named On, `false` Off), so a client older than 1.23.0 still shows a
  two-value select that sets it. `session.agent_option.set` keeps its
  string `value` (`"true"` or `"false"`), and the daemon sends
  `session/set_config_option` with `type: boolean` and a JSON boolean; a
  select's request is unchanged (no `type`). `acp.config` takes `"true"`
  or `"false"` for a boolean. An option of a type cookie does not know is
  left out, as ACP asks. The TUI's `/options` toggles a boolean from its
  row and stays open; the web's Agent options sheet shows a switch
  (`role="switch"`).
- **Images in replies.** An `agent_message_chunk` whose content is an image
  becomes a `file` part of the turn (an artifact of the session, like a
  model's generated image) in place of the `[image]` note, so the clients
  show it as media in the reply (`AssistantChild::MediaFile`); text after
  it starts a new text part. An image that cannot be decoded or stored
  still becomes the note. Images in thoughts stay notes.
- **Subagents stay flat.** Claude Code's Task subagents arrive as flat tool
  calls whose `_meta.claudeCode.parentToolUseId` names the Task call, and
  ACP's `subagent_update` and child sessions (for a client that declares
  `clientCapabilities.subagents`) are unstable. Cookie deliberately shows
  them as the turn's own calls and declares no `subagents` capability:
  neither nesting by `parentToolUseId` nor child sessions is implemented
  until the subagent API is stable.
- **Checked by hand against Claude Code** (`claude` 2.1.309, adapter
  0.95.0, scratch daemon): a `Bash` call of `printf 'one\ntwo\n'; exit 3`
  showed its output in a `terminal` block (replaced into the live display
  once the adapter sent it, with the result) and ended failed with exit
  code 3, `the command exited with code 3`, and the output retained; the
  context was recorded twice (`used` 21205, then 21285, of a 1000000-token
  window); the options came as `mode`, `model`, `effort`, and `fast` as a
  boolean, which `session.agent_option.set` turned on.

**Tests** (display round): the context use recorded once for a repeated
`usage_update`, and the client reducer keeping it across ACP turns and
clearing it at a model's; the TUI status line's `ctx` with the agent's
window; the agent's own mode switch on its own mode option and on one made
of its modes, and an unknown mode ignored; terminal output streamed as
Codex and Claude Code send it (the capabilities, the live replacement and
append, the exit codes, the retained output, a failed call's partial
result); a boolean option recorded with its values and set with a JSON
boolean, and the TUI toggling it; an image in a reply as a file part whose
artifact holds its bytes; the `acp_agents` browser scenario (context chip,
command output, the reply's image, the boolean switch).

**Session lifecycle** (2026-10-24; user decisions of the same day; client
protocol 1.24.0, plugin protocol 1.12.0; history format unchanged):

- **Fork points.** Each committed ACP turn records the ACP `messageId`s of
  the agent messages it holds (`response_metadata.acp_message_ids`), so a
  fork or a revert can name the agent's last message it keeps.
- **Detached sessions.** A fork of a cookie session, and a revert that cuts
  turns of the agent's conversation, record the ACP session the cookie
  session talked in as left behind: `acp_session_updated.detached`
  (`acp_session_id`, `reason` `forked` or `reverted`, and the agent's last
  kept `message_id`). The fork's log keeps the source's
  `acp_session_started` (so the fork knows what to fork); a record
  followed by `detached` is never resumed. A revert keeps no marker of its
  own, so this event is how the log says the agent's session no longer
  matches.
- **Branching.** The agent's next turn in such a session calls
  `session/fork` on the detached session with the fork point as
  `_meta.jetbrains.air.fork = {version: 1, messageId}` and records the new
  session (`how: forked`, `forked_from`). After a revert the old session is
  then closed when live and deleted (`session/delete`), and the session is
  told the agent continues from the revert point; after a fork the source
  keeps its session. With no agent message kept (a fork or a revert before
  the agent's first reply) the turn starts a new session instead, and a
  revert's old session is deleted. A session whose every record a revert
  cuts is deleted at once. A fork the agent refuses at that turn fails the
  run with what it said.
- **The AIR extension is a vendor extension.** Standard ACP
  `session/fork` (unstable, gated by `sessionCapabilities.fork`) forks a
  whole session; the fork point is JetBrains AIR's `_meta`, which Claude
  Code's adapter (`src/fork-session.ts`) and Codex's (`src/SessionFork.ts`)
  read and nothing advertises. An agent that does not know it ignores it
  and forks the whole session. The engine therefore asks once per agent
  process, after the agent's first completed round (Claude Code's adapter
  cannot fork a session it has not written to disk yet, and answers
  `-32603` "Session ... not found" before), off the turn's path:
  `session/fork` of that session at a `messageId` that cannot exist
  (`cookie-fork-probe-<uuid>`), without `messageFingerprint` (Codex
  validates its format). Invalid params (`-32602`, "Fork point message ...
  was not found") means the fork takes a fork point; a fork that succeeds
  means it does not, and that fork is deleted (closed when the agent
  offers no `session/delete`); any other answer counts as no. The answer stays with
  the process and is recorded on every session it serves
  (`acp_session_updated.fork_point`) and on later records
  (`acp_session_started.fork_point`, next to `fork`, whether the agent
  offers `session/fork`), so a revert can be judged while the process is
  down. A record without them is unknown until the next probe.
- **Refusals** (error `acp_agent_unsupported`, its `reason` the message,
  checked before anything changes and before plugin hooks):
  - a revert past the agent's first reply when its fork takes no fork
    point (`` `claude` on `local` can't rewind its own conversation, so the
    session can't be reverted past the agent's first reply ``) or nothing
    was learned yet (`cookie has not yet checked whether ... (it checks
    after the agent's next reply) ...`);
  - a fork after the agent's first reply when it offers no `session/fork`
    (`... can't fork its own conversation, so the session can't be forked
    after the agent's first reply`), and a fork whose point cuts into the
    agent's conversation when its fork takes no fork point (`... can't fork
    its own conversation at an earlier point, ...`). A fork at the end of
    the conversation is a plain whole-session fork, taken as the cookie
    fork is made (see below). Revert or fork before
    the agent's first reply always works. Gemini CLI (no `session/fork`)
    therefore refuses both past its first reply.
  - *Superseded the same day:* a first version started a new session given
    a condensed cookie transcript when the agent could not fork; dropped
    for the refusals above.
  - *Eager whole forks* (2026-10-24, closing a gap the first version
    left): a fork of a cookie session at the end of the agent's
    conversation, when the agent's fork takes no fork point or the engine
    has not asked yet, calls `session/fork` while `session.fork` runs
    (starting the agent's process when needed) and records the fork's own
    session then, so turns the source runs before the fork's first turn
    stay out of it. When the agent cannot fork there, or the session is on
    a runtime that does not run the agent for the fork, the cookie fork is
    deleted and refused with `acp_agent_unsupported` (`` `claude` on
    `local` could not fork its own conversation for the new session: ...
    ``). A fork that takes a fork point still forks at the fork's first
    turn, at the recorded message.
- **Deleting a session** closes its live ACP session and deletes every ACP
  session it owns with `session/delete` when the agent offers it, starting
  the agent's process for it when needed (one a fork left behind stays its
  source's). Paging a session out only closes.
- **Usage** is recorded for failed and cancelled rounds too, from the
  answer's `usage`, else the last `usage_update`.
- **Stop reasons.** `refusal`, `max_tokens`, and `max_turn_requests` fail
  the run with "the agent refused", "the agent hit its output token
  limit", and "the agent hit its turn request limit" (each naming the stop
  reason), and the turn so far commits as finished `refused`, `length`, or
  `max_turn_requests`. `RunOutcome` has no kind that fits; the failure
  stays `failed`.
- **Compaction.** `session.compact` on an ACP session used to fail reading
  the synthetic `acp/<agent>` binding as a model ("the stored binding of
  model `acp/<agent>` cannot be read by this build"). It now runs the
  agent's `compact` command, when its `available_commands_update` lists
  one, as a run of `/compact <focus>` and waits for it; otherwise it is
  refused with `acp_agent_unsupported` (`` `claude` does not offer
  compaction ``). Automatic compaction belongs to the model loop and never
  runs for an ACP run.
- **Titles.** After a completed round, an ACP session with no title runs
  cookie's title generation on the title agent's models minus the
  synthetic `acp/<agent>` it would inherit as `${parent_model}`; with
  `small_model` unset none remain and nothing runs. A later
  `session_info_update` title still replaces it, and each new agent title
  applies, until the user names the session.
- **Checked by hand against Claude Code** (adapter 0.95, outside the
  daemon, over the adapter's stdio): its initialize advertises `fork`,
  `delete`, `close`, `resume`, `list`; the probe on a fresh session answers
  `-32603`, and after one turn `-32602`; a fork at the first of two
  replies' `messageId` (`msg_...`) remembered only the first turn's word;
  `session/delete` of the fork succeeded.

**Tests** (lifecycle; `crates/engine/src/runtime/tests/acp/` against the
scripted agent in its three fork modes: at a fork point, whole sessions
only (`ACP_FIXTURE_FORK_WHOLE`), and no fork): the probe's answer in each
mode, recorded once per process and on later records, and the deleted
probe fork; a fork at its fork point remembering only the turns before
it, the source keeping its own; a whole fork allowed at the end and
refused earlier; a fork refused without `session/fork` and a fork before
the first reply starting fresh; a whole fork taken as the cookie fork is
made, so a turn the source runs before the fork's first turn stays out of
it, and a cookie fork the agent fails to take refused and deleted; a
revert forking at the revert point and
deleting the old session, refused for whole and no-fork agents and while
not asked yet; a revert before the first reply deleting the session and
starting fresh; a revert of a fork into its source's turns leaving the
source alone; deletion closing and deleting, not a source's; paging out
only closing; usage on failed and cancelled rounds; the stop reasons'
messages and finish reasons; `/compact` forwarded and refused; the
generated title, the agent's later one, and no title without
`small_model`.

**Elicitation** (2026-10-24; client protocol 1.25.0, plugin protocol
1.13.0; history format unchanged, two new event types). ACP v1's stable
`elicitation/create` and `elicitation/complete`, which Claude Code's adapter
(0.95.0) uses for its `AskUserQuestion` tool (a form tied to the tool call:
a titled `oneOf` per question, an array of titled `anyOf` for a
multi-select one, and a free-text `question_<n>_custom` beside each) and
for an MCP server's sign-in (a URL), and Codex's adapter for its
elicitations. Before, cookie advertised neither mode and these degraded.

- **Capabilities.** `initialize` declares `clientCapabilities.elicitation:
  {form: {}, url: {}}`.
- **Routing** (`crates/engine/src/runtime/acp/elicitation.rs`). A
  session-scoped `elicitation/create` goes to its session's runner like a
  permission request. During a prompt round it becomes a question of the
  run; between rounds, after the round's answer, after a cancel, for a
  session no turn runs in, and when request-scoped (`requestId`, no
  `sessionId`), it is answered `cancel`. That includes a question asked
  while the agent opens its session (an MCP server of the agent's own that
  wants a sign-in while `session/new` or `session/resume` runs): the
  session is not routed, or its round has not started, yet. A mode other than `form` and `url`,
  or a URL question without a `url`, is refused with `-32602`, as ACP asks.
- **Cookie's model of a question** is a new client protocol concept, not an
  approval: approvals are tool-call operations with fingerprints,
  evaluations, grants, and the reviewer, none of which apply, and a form's
  answer carries values. `elicitation_requested` (run-bound) records the
  question: `elicitation_id` (cookie's own UUID), `agent`, the cookie
  `tool_call_id` the agent's `toolCallId` is shown as (when the agent
  reported that call), `message`, and a `form` or a `url`. The form's
  properties become typed fields: `string` → `text` (`minLength`,
  `maxLength`, `pattern`, `format`, `default`), `string` with `oneOf`, or
  with `enum` (titled by `enumNames` when present) → `choice`, `array` with
  `items.anyOf`/`items.enum` → `multi_choice` (`minItems`, `maxItems`,
  `default`), `number`, `integer`, `boolean`; `required` from the schema;
  any other type is kept as an unknown input (`Open::Unknown`) that no
  client can fill in. **Field order:** the runtime relay carries the agent's
  messages as parsed JSON and the daemon's `serde_json` keeps no member
  order, so the order of the schema's properties is lost before the engine
  sees them; fields are sorted in natural order of their names
  (`question_2` before `question_10`), which keeps Claude Code's forms in
  its order. Keeping the agent's order would need the relay to carry raw
  text (a runtime protocol change).
- **Answers.** `elicitation.respond` (`session_id`, `elicitation_id`,
  `action`, `content`) hands the user's answer to the waiting question; an
  accepted form is checked against its fields first (required, type,
  bounds, choices, formats `email`, `uri`, `date`, `date-time`; `pattern`
  is shown, not checked: the protocol crate has no regex engine), and a
  refused answer (`elicitation_answer_invalid`) leaves it waiting. A
  question no run waits on is `elicitation_not_pending`.
  `elicitation_resolved` records `action`, an accepted form's `content`,
  `answered_by`, and the daemon's `reason`, before the run's end; the agent
  gets `{action, content?}`.
- **No rule or mode answers a question** *(task decision: it is the agent
  asking the user, not an action)*, with these daemon answers, each
  recorded: a session whose tree's permission mode is `auto_approve_n` (a
  cron run's default, and the mode that already answers escalated
  approvals for an absent user) declines at once (`unattended`); a
  headless `cookie run` declines what waits in its tree on its approval
  tick (`unattended`, "a headless `cookie run` has no one to answer"); the
  run's cancel answers `cancel` at once, and the round's end cancels what
  still waits (`run_ended`); a form with a *required* input of an unknown
  type declines (`unsupported`). Questions do not expire: `[approval]
  timeout_ms` (30 s by default) suits a yes/no, not a form. Yolo,
  auto-approve, and auto-approve(Y) sessions still ask, since someone who
  chose not to be asked about permissions may still want the agent's
  questions.
- **URL completion.** Each agent process keeps its URL questions by the
  agent's `elicitationId`; an `elicitation/complete` for one still waiting
  accepts it (`completed`), and an unknown or answered ID is ignored, as ACP
  asks.
- **Clients.** The client state keeps a session's waiting questions
  (`SessionState.questions`) until `elicitation_resolved` or the run's end,
  and shares the form draft, field hints, URL host and warnings, and
  resolution words (`cookie_agent_client_state::elicitation`); a resolved
  question is an info event row. The TUI docks a question panel where the
  approval panel goes (an approval stays on top), keyboard-only, with the
  approval hotkey grace before an answer; the web client shows a card above
  the composer, says *waiting for your answer* in its bottom bar and on a
  subagent's delegate row (`tree::Activity::WaitingForAnswer`, ahead of a
  waiting approval). Both show who asks, the message, a form's fields with their
  hints and the first field that does not fit, or a URL with its host and
  warnings, and Submit/Done, Decline, and Cancel.

**Tests** (elicitation): the fixture's `elicit form|url|url-complete` and
`elicit-wait`; an AskUserQuestion-shaped form answered (fields, the call it
belongs to, a refused answer that leaves it waiting, the accepted values
reaching the agent, `elicitation_not_pending` after, the `initialize`
capability); a declined URL question and one the agent completes (after an
unknown ID); a cancel that answers a waiting question `cancel` before the
run finishes; an `auto_approve_n` session declining at once and a headless
driver declining a waiting one; the schema conversion (natural order,
choices from `oneOf`, `enum`, and `enumNames`, multi-choices, defaults,
unknown types), the ACP answer shape, the protocol's answer checks and
formats, the client state's draft, hints, URL warnings, and reducer; and
the TUI panel (rendering, keyboard, a required field, a URL question, Esc
and the grace); the web client's helpers and the `acp_questions` browser
scenario (a form refused for its required field, then answered; a URL
question's host and link, declined).

**Checked by hand against Claude Code** (elicitation; adapter 0.95.0 on
`local` of a scratch daemon, driven over the client protocol): asked to use
`AskUserQuestion`, the agent announced the call ("Asking for your input"),
then sent a form tied to it, recorded as `question_0` (a choice of Red and
Blue, each with its description, titled "Color") and `question_0_custom`
("Other", text), with the question as the message. Accepting
`{"question_0": "Blue"}` ended the call and the agent replied "You chose
Blue."; declining another made it reply "You skipped.".

# ACP agents

Status: **draft** (2026-10-23), awaiting review and the go-ahead. Section 13
lists what is still open.

## 1. Goal and scope

Run external coding agents, such as Claude Code, Codex, and Gemini CLI, as
cookie agents: a session or a subagent whose turns an external agent takes,
driven over the [Agent Client Protocol](https://agentclientprotocol.com)
(ACP). Cookie is the ACP **client**; the external agent is the ACP agent.

Naming: the docs call these **ACP agents**, never "the client protocol",
which stays the name of cookie's own protocol.

**Decided 2026-10-23:**

- An ACP agent is an agent document with an `acp` block instead of
  `models` (section 3).
- **Where it runs:** the daemon launches the agent's command on the
  daemon's machine. The command may itself reach another machine (for
  example `ssh labpc npx …`), since ACP is just stdin and stdout.
- **Its own tools:** the agent runs its own file and shell tools. Cookie
  does not advertise ACP `fs` or `terminal`. An agent document that should
  not also get cookie's runtime tools denies them (`read: deny`,
  `write: deny`, `bash: deny`).
- **Mode:** each session starts the agent in its "auto" mode when it offers
  one, else in its default mode (which asks); the agent document can name
  another.
- **Login:** credentials already on the machine (an existing `claude` login,
  API keys in `env`). Authentication errors reach the cookie session with
  what the agent said, so the user knows what to do. No login flow in cookie.
- **Cookie's tools reach the agent as MCP**, from v1: the session's tools
  (memory, cron, `list_agents`, delegation, MCP servers' tools, runtime tools
  that are not denied), each following the permission rules (section 7).
- **The agent document's body** is sent as context with the session's first
  prompt; an empty body sends nothing.
- **Slash commands** the agent offers appear in the TUI and web composers.

**Out of scope for v1:** ACP `fs` and `terminal` (cookie's runtime executing
the agent's file and shell operations), logins run from cookie, ACP v2,
the agents' vendor extensions (`_meta`, AIR, steering, goal).

## 2. ACP in brief

JSON-RPC 2.0 over the agent's stdin and stdout, one message per line;
the agent's stderr is diagnostics. Version 1 (schema 1.26, Oct 2026).

- **Client to agent:** `initialize`, `authenticate`, `session/new`
  (`cwd`, `mcpServers`), `session/prompt` (content blocks, returns a stop
  reason), `session/cancel`, and when the agent supports them `session/load`,
  `session/resume`, `session/close`, `session/set_config_option`.
- **Agent to client:** `session/update` notifications (message and thought
  chunks, tool calls and their updates, plans, available commands, config
  options, usage, session info) and `session/request_permission`.
- Adapters: Claude Code `@agentclientprotocol/claude-agent-acp`, Codex
  `@agentclientprotocol/codex-acp`, Gemini CLI `gemini --acp`.

## 3. The agent document

```yaml
---
description: Claude Code, driven over ACP
mode: all
acp:
  command: ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
  env: {}                       # merged over the daemon's environment
  config: { model: "opus" }     # the agent's config options, by id; values are labels
  mode: auto                    # optional; see section 5
permissions:
  read: deny
  write: deny
  bash: deny
  acp: { "read *": allow, "*": ask }
---
You review changes for this project before they are committed.
```

- **`acp.command`:** the argument vector, run without a shell. It may start
  with `ssh host`; cookie does not interpret it.
- **`acp.env`:** extra environment variables.
- **`acp.config`:** initial values for the agent's config options
  (`session/set_config_option`), matched by option id. Unknown ids or values
  are reported in the session and skipped.
- **`acp.mode`:** see section 5.
- **`acp.cwd`:** optional absolute path for `session/new`; see section 6.
- **`models` is not allowed** with `acp`: the agent chooses its own model,
  set through `acp.config` when the agent offers a model option.
- `mode`, `description`, `enabled`, and `permissions` mean what they mean for
  any agent. The agent appears in the agent pickers, presets, `list_agents`,
  and `delegate_subagent` like any other.

## 4. Running a turn

- **The runner.** `start_run_direct` keeps everything it does today (run
  start, idempotency, `RunStarted`, the end of the run) and, for an ACP agent,
  calls the ACP runner instead of the model loop.
- **The prompt.** The user's message (or a producer's: a cron fire, a
  subagent report) becomes `session/prompt` content: text, images when the
  agent accepts them, and the agent document's body before the first prompt
  of the session (section 1).
- **Rendering**, from `session/update`:
  - `agent_message_chunk` → assistant text, streamed as text deltas;
  - `agent_thought_chunk` → thinking;
  - `tool_call` / `tool_call_update` → a cookie tool call from a new tool
    source, `acp:<agent>`, upserted by its ACP id: its title and kind, its
    status as the call's state, `diff` content as a diff block, `content`
    as text or media blocks, locations as file links; raw input and output
    kept in the call's metadata;
  - `plan` → a checklist block, replaced on each update;
  - `usage_update` → recorded usage, without a cost when cookie cannot
    price the agent's model;
  - `session_info_update` → the session title, unless the user set one;
  - `available_commands_update` → the session's agent commands (section 9);
  - `config_option_update` → the session's agent options (section 5);
  - unknown kinds are ignored.
- **Durability.** Each turn is recorded as model turns and tool calls with
  a synthetic model reference (`acp:<agent>`), so history, reports, and
  clients work unchanged. The ACP agent keeps its own conversation; cookie
  never replays history to it.
- **The end.** The `session/prompt` stop reason ends the run: `end_turn`
  completes it with the last message as its final text; `cancelled` cancels
  it; `max_tokens`, `max_turn_requests`, and `refusal` fail it with that
  reason.
- **Steering** falls back to queueing: ACP has no standard steer.
- **Interrupt** sends `session/cancel`, answers every pending permission
  request `cancelled`, and waits for the `cancelled` stop reason (with the
  existing cleanup grace).

## 5. Modes and config options

- Cookie reads the agent's `configOptions` from `session/new`, and its
  `modes` when it offers no mode option.
- **Starting mode:** `acp.mode` when set; otherwise the mode option or mode
  whose id is `auto`; otherwise the agent's default. Mode ids are labels:
  cookie never interprets them beyond looking for `auto`.
- Then `acp.config` is applied.
- Clients show the session's agent options (model, mode, effort) in the
  session's details; changing one calls `session/set_config_option`.

## 6. Sessions and processes

- **One ACP session per cookie session**, created on the session's first
  turn. Its ACP session id and cwd are recorded in the session log.
- **cwd:** `acp.cwd` when set; otherwise the session's first workspace
  attachment's path; otherwise the daemon user's home. With an `ssh`
  command, the path must exist on that host; cookie cannot check it, and the
  agent's error reaches the session.
- **After a daemon restart** or a process exit, the next turn reconnects with
  `session/resume` when the agent offers it, else `session/load` (its replay
  is ignored, since cookie has the transcript), else a new ACP session, which
  the session is told about.
- **Processes:** one per ACP agent document, shared by its sessions; started
  on first use, stopped after an idle period with no live session
  (`[acp] idle_shutdown_ms`), and stopped with the daemon. `session/close`
  when the agent offers it, as a cookie session is deleted or unloaded. A
  process exit fails the runs it was serving with its stderr tail.
- **Errors**, including `auth_required`, end the run as failed with the
  agent's message.

## 7. Cookie's tools for the agent (MCP)

- On `session/new` cookie passes one MCP server, **`cookie`**, that serves the
  session's tools as cookie composes them for a model request: memory, cron,
  `list_agents`, `delegate_subagent` and the subagent tools, the MCP servers'
  tools, and runtime tools the rules do not deny.
- **Permissions apply as for a model:** a tool the merged rules hide is not
  listed; a call is decided by the rules, and `ask` goes through the approval
  flow (the reviewer in auto modes).
- **Transport:**
  - **HTTP** when the agent declares `mcpCapabilities.http`: the daemon serves
    the session's tools at an MCP endpoint with a bearer token that is valid
    only for that ACP session and dies with it.
  - **stdio** otherwise (every agent must accept it): `cookie mcp serve
    --session <id>` with the daemon's URL and a session-scoped token, a small
    bridge that relays MCP to the daemon.
  - With an `ssh` command the agent runs elsewhere, so the endpoint or the
    bridge must reach the daemon from that host; the docs explain how
    (`--url` to a reachable daemon address).
- Tool calls through the bridge appear in the session as calls of the ACP
  agent, linked to its turn.

## 8. The agent's own permission requests

- Each `session/request_permission` is decided by cookie's rules under the
  action **`acp`**, so they never collide with `read`, `write`, and `bash`,
  which govern cookie's own tools. The resource is the ACP tool kind and its
  target: `execute git push`, `edit /home/ada/project/src/lib.rs`,
  `fetch https://example.com`, `read /path`, or the kind alone when there is
  no target.
- **Default:** an agent document with no `acp` key gets `acp: ask`.
- **Answers:** `allow` picks the agent's allow-once option; `deny` its
  reject-once option; `ask` goes through the approval flow (the reviewer in
  auto modes), and the user's answer picks allow-once or reject-once. Cookie
  never picks an "always" option: repeats come back to cookie's rules and
  grants.
- In the agent's auto mode most calls never ask; those that do still meet
  the rules.

## 9. Agent commands

- `available_commands_update` gives the session's agent commands (name,
  description, input hint). The TUI and web composers list them with
  cookie's own commands, marked as the agent's.
- Choosing one sends `/<name> <input>` as the prompt text, as ACP specifies.

## 10. Protocol and storage

- **Session log:** a new event records the ACP session (agent, ACP session
  id, cwd, protocol version, agent info). Tool calls from `acp:<agent>` use
  the existing events with a new tool source.
- **Client protocol minor:** the session's agent commands and agent options
  in the session state, an RPC to set an option, the plan block, the
  `acp:<agent>` tool source. Plugin protocol minor for the events.
- **Config:** `[acp] idle_shutdown_ms`, and the agent document's `acp` block.
- **Runtime protocol:** unchanged in v1.

## 11. Tests

A scripted ACP agent fixture (like the MCP fixture) that speaks ACP v1:
handshake and capabilities, auto-mode selection and `acp.config`, a turn
with chunks, thoughts, tool calls with diffs, a plan and usage, every stop
reason, cancel with a pending permission request, permission requests
decided by `acp` rules and by approvals, resume, load with an ignored
replay, a new session after a lost one, process exit, the MCP bridge
listing and gating cookie's tools (HTTP and stdio), the body sent once,
agent commands, delegation to an ACP subagent, a cron fire into an ACP
session.

## 12. Phases

1. **Engine:** the agent document, the ACP client and runner, rendering,
   sessions and processes, `acp` permissions, protocol additions.
2. **Cookie's tools over MCP:** the HTTP endpoint and the stdio bridge.
3. **Clients:** agent commands and options, the plan block, ACP tool calls.
4. **Docs:** an ACP agents guide with Claude Code, Codex, and Gemini CLI
   examples, including `ssh`.

## 13. Open questions

1. **The `acp` permission action** (section 8) for the agent's own requests,
   so that denying `read`, `write`, and `bash` to hide cookie's runtime tools
   does not also refuse every file and shell request the agent makes. The
   alternative, mapping the requests onto `read`/`write`/`bash`, collides with
   that decision.
2. **"Deny" spelling.** Decided as "`read false`"; the spec uses the
   existing `deny`. Add `false` as an alias, or keep `deny`?
3. **The MCP transports** (section 7): HTTP when the agent accepts it and a
   stdio bridge otherwise, or only one of them in v1?

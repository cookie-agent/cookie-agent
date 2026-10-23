# ACP agents

External coding agents, such as Claude Code, Codex, and Gemini CLI, can take
a session's turns, or a subagent's, as cookie agents. Cookie drives them over
the [Agent Client Protocol](https://agentclientprotocol.com) (ACP): cookie is
the ACP client, the external agent the ACP agent. The agent keeps its own
conversation, model, file tools, and shell; cookie renders its turns, decides
its permission requests with your rules, offers it cookie's own tools (memory,
cron, delegation, MCP servers), and keeps the session's history as for any
other agent.

A *runtime* hosts each agent: the daemon's own `local` runtime, a runtime the
daemon starts over ssh, or a runtime that dials in, such as one on a lab
machine or the one a TUI or web client offers. The runtime starts the agent on
its own machine, with that machine's credentials, and relays its messages to
the daemon over the runtime connection; the agent's host needs no route to the
daemon. The [ACP agents spec](../specs/acp-agents.md) has the design.

## Before you start

On the machine that runs the agent:

- Install the agent and log in to it there, as you would to use it in a
  terminal: `claude` and its `/login`, `codex login`, or `gemini` and its
  sign-in. Cookie runs no login flow; it uses the credentials already on that
  machine.
- Install its ACP adapter, or let `npx` fetch it: Claude Code speaks ACP
  through `@agentclientprotocol/claude-agent-acp`, Codex through
  `@agentclientprotocol/codex-acp`, and Gemini CLI natively with
  `gemini --acp`. `npx` needs Node.js on that machine's `PATH`, as the
  runtime sees it.
- `cookie` must be on that machine: the MCP bridge that offers cookie's tools
  to the agent is `cookie runtime mcp-bridge`. A runtime always has it, since
  it is `cookie runtime`.

## On the daemon's machine

Declare agents under `[runtimes.local.acp.agents.<agent>]` in
`~/.cookie-agent/config.toml`:

```toml
[runtimes.local.acp.agents.claude]
command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
description = "Claude Code"

[runtimes.local.acp.agents.codex]
command = ["npx", "-y", "@agentclientprotocol/codex-acp"]
description = "Codex"

[runtimes.local.acp.agents.gemini]
command = ["gemini", "--acp"]
description = "Gemini CLI"
```

The daemon lists them as `local-claude`, `local-codex`, and `local-gemini`
once it restarts (`[runtimes.local]` changes with the rest of `runtimes`, at
the next start). Pick one in the agent picker of the TUI (`/agent`) or the
web client (the agent chip) for a new session, and send a message: the daemon starts the
agent's process on the first turn, opens an ACP session in the session's
workspace, and shows the agent's reply, thoughts, tool calls, and plan as
they stream.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `command` | array of strings | required | The program and its arguments, run without a shell on the runtime's machine, in its home directory. |
| `env` | table of strings | `{}` | Variables merged over the runtime's environment. |
| `description` | string | `""` | Shown where the agent is listed when its daemon document gives none. |
| `enabled` | boolean | `true` | A disabled agent is listed as unavailable. |

[Runtimes: ACP agents](../runtimes/tools.md#acp-agents) has the rules for
IDs, availability, and processes.

## On a machine the daemon reaches over ssh

A [runtime template](../runtimes/templates.md#example-a-repository-on-another-machine-over-ssh)
declares its agents in the daemon's `config.toml`, next to its command. They
run on the remote machine, with its credentials:

```toml
[runtimes.labpc]
command = ["ssh", "-o", "BatchMode=yes", "labpc", "cookie", "runtime", "serve", "--stdio"]
startup_timeout_ms = 20000

[runtimes.labpc.acp.agents.codex]
command = ["npx", "-y", "@agentclientprotocol/codex-acp"]
env = { CODEX_HOME = "/home/ada/.codex" }
description = "Codex on labpc"
```

The agent is `labpc-codex`. The daemon starts the `labpc` runtime when a
session first uses the agent. A session that attaches a `labpc` workspace
runs the agent there; without one, the agent works in the runtime's home
directory.

## On a machine that dials in

A [runtime that dials in](../runtimes/dial-in.md)
declares its agents in its own machine's `config.toml`, under
`[runtimes.local]`, exactly where its MCP servers go:

```toml
# ~/.cookie-agent/config.toml on the lab machine
[runtimes.local.acp.agents.claude]
command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
description = "Claude Code on the lab machine"
```

```sh
# On the lab machine: dial in as `lab`.
cookie runtime serve --daemon ws://laptop:7419/ws --id lab --token <token>
```

The agent is `lab-claude`, available while the runtime is connected and
listed as unavailable while it is away. The runtime reads its configuration
again every few seconds and reports changed agents, so adding one needs no
restart. To keep it connected, run it as a
[service](../runtimes/service.md). A runtime ID's `_` becomes `-` in
the agent's ID: runtime `lab_pc`'s `claude` is `lab-pc-claude`.

## Cookie settings: the daemon-side document

Cookie settings come from an agent document of the agent's ID in the
daemon's agents directory, never from the runtime, so a runtime cannot grant
itself permissions:

```yaml
# ~/.cookie-agent/agents/labpc-codex.md
---
description: Codex on labpc, for refactors
mode: all
acp:
  mode: auto
  config: { model: "gpt-5" }
  hide_tools: [read, write, edit, bash, webfetch]
permissions:
  read: allow
  write: { "${workspace_dir}/*": allow, "*": ask }
  bash: { "git *": allow, "*": ask }
  webfetch: allow
  mcp: { "exa_*": allow }
  memory: allow
---
You review changes for this project before they are committed.
```

- **Optional.** Without it, the agent has mode `all`, the runtime's
  description, the rules every agent gets for actions it names no key for,
  and the default `hide_tools`.
- **`acp`** marks the document as the agent's; `acp: {}` is enough. `mode`
  picks the mode its sessions start in (else the agent's `auto` mode, else
  its default), and `config` sets its config options by ID, such as its
  `model` or reasoning `effort`; an unknown option or value is reported in
  the session and skipped.
- **No `models`.** The agent chooses its own model; set it through
  `acp.config` when the agent offers a model option.
- **The body** goes with the first prompt of each of the agent's ACP
  sessions (see [What the first prompt carries](#what-the-first-prompt-carries));
  an empty body sends nothing. Cookie adds no preamble of its own: tell the
  agent what it is and how to work with cookie here, as the
  [reference document for Claude Code](#a-reference-document-for-claude-code)
  does.

[ACP agent documents](agents.md#acp-agent-documents) lists every key.

### A reference document for Claude Code

The agent learns from the document's body that it runs inside cookie and
which of cookie's tools to prefer over its harness's own. Adapt this one to
your setup; it names Claude Code as the harness, so change that line for
another agent:

```markdown
---
description: Claude Code
mode: all
acp: {}
permissions:
  read: allow
  write: { "${workspace_dir}/*": allow, "*": ask }
  bash: { "git status*": allow, "git diff*": allow, "*": ask }
  memory: allow
---
You are an agent in cookie agent, running in ACP mode: cookie drives you
over the Agent Client Protocol, and your actual harness is Claude Code. The
user talks to you through cookie (its terminal UI, web client, or CLI,
possibly from another machine), and cookie records this conversation.

Guidelines:

- Cookie's tools come from the MCP server named `cookie`. Your harness's
  own tools stay available, but when a cookie tool covers the same job, use
  the cookie tool: cookie applies the user's permission rules, shows the
  call in its transcript, and other cookie agents can see the result.
- Keep durable notes in cookie memory (memory_search, memory_read,
  memory_write), not in your harness's own memory or notes files.
- Delegate with cookie's delegate_subagent (list_agents shows who), not
  with your harness's own subagents; schedule follow-ups with cookie's cron
  tools.
- Your built-in file and shell tools work in this session's workspace.
- Some calls need the user's approval through cookie; if one is refused,
  do not retry the same call.
- Messages wrapped in <scheduled_job> or <subagent_report> come from
  cookie, not from the user typing.
- When you finish a delegated task, make your last message a
  self-contained summary: cookie returns it as your report.
- Change cookie's own configuration only when the user asks (the
  customize-cookie-agent skill explains how).
```

Save it as `~/.cookie-agent/agents/local-claude.md` for the `claude` agent
of `local`, or under the ID of the agent it is for.

## What the first prompt carries

The first prompt of each of the agent's ACP sessions carries, before your
message and in this order, each part left out when it is empty:

1. the tool instructions cookie would give a model for the tools the agent
   is offered over the [bridge](#cookies-tools-for-the-agent-the-mcp-bridge),
   after `hide_tools` and the rules (`<tool_instructions provider=...>`
   sections, such as memory's: how memory is included, and the rubric for
   writing it when a write tool is offered);
2. the session's memory as a model would see it at the start of a session
   (the `# Memory` reminder);
3. the agent document's body.

`AGENTS.md` is not sent: the agents read it themselves. The prompt carries
these again when cookie has to start a new ACP session because the agent
lost the one it had, and not when it resumes or loads one, since the agent
remembers them there. Later prompts carry only the input.

## Permissions

The agent runs its own file and shell tools, and asks cookie before the
calls its own policy asks about. Cookie decides each request with the
document's rules, by the request's ACP tool kind:

| The agent asks to | Decided as |
|---|---|
| read or search a file | `read`, by the file's path |
| edit, delete, or move a file | `write`, by the file's path |
| run a command | `bash`, by the command |
| fetch a URL | `webfetch`, by the URL |
| call a tool of an MCP server it connects itself (`mcp__exa__search`) | `mcp`, by `exa_search`, the name cookie gives the same tool |
| call one of cookie's own tools (`mcp__cookie__memory_search`) | allowed: the call reaches cookie, whose rules decide it there |
| think | allowed |
| switch its mode, or anything else | asks you |

So `bash: {"git *": allow}` allows the agent's own `git` commands as it
allows cookie's `bash` tool, and `mcp: {"exa_*": allow}` covers an `exa`
server whether cookie or the agent connects it. A request that touches
several files needs each allowed. An `ask` goes through the approval flow
like any other, and cookie answers the agent with its allow-once or
reject-once option, never an "always" one, so a repeat comes back to your
rules. In the agent's auto mode, which cookie picks when it offers one, the
agent asks about few calls.

## Cookie's tools for the agent: the MCP bridge

Each ACP session gets one stdio MCP server, `cookie`, whose command is
`cookie runtime mcp-bridge` on the agent's machine. The bridge reaches the
runtime over a local socket, and the runtime relays its messages to the
daemon over the connection it already has. Through it, the agent sees the
tools cookie would offer a model in that session: memory, cron,
`list_agents`, `delegate_subagent` and the subagent tools, MCP servers'
tools, and runtime tools, each listed only when the rules offer it, and
each call decided by the rules as a model's would be.

`acp.hide_tools` lists cookie's tools the agent does not get, with `*` and
`?`. By default it hides the coding runtime's `read`, `write`, `edit`,
`bash`, and `webfetch`, since the agent has its own; `[]` offers them too.
`hide_tools` only hides: the rules still decide every call.

## In the clients

- **Agent pickers.** The TUI's `/agent` and the web client's agent chip list
  the agents runtimes host with their runtime (`local-claude [on local]`).
  An agent whose runtime is away, or that is disabled, is shown dimmed with
  why (`on lab · unavailable`) and cannot be picked. No model picker lists
  an ACP agent's synthetic model `acp/<agent>`: the agent chooses its own.
- **Agent commands.** Once the agent has announced its slash commands, the
  TUI's command palette (`/` on an empty composer, or ctrl+p) lists them
  after cookie's own, marked `(agent)`; a command that takes input asks for
  it. In the web client, typing `/` in the composer lists them above it.
  Choosing one sends `/<name> <input>` to the agent.
- **Agent options.** The agent's options, such as its model, mode, and
  effort, are the session's agent options: `/options` in the TUI (or the
  title's model, which opens them for an ACP agent), and the model chip in
  the web client, which shows the agent's model. Choosing a value changes
  it on the agent at once. They appear once the agent's session has
  started, with the first message.
- **Plan.** The agent's plan shows in the transcript as a checklist after
  the turn's reply, each step done (`[x]`), in progress (`[~]`), or pending
  (`[ ]`), updated in place as the agent works; each turn's plan stays where
  it was.
- **Tool calls** the agent reports name it after their title
  (`Read notes.txt · local-claude on local`).
- **Tool calls** show what the agent reports as it reports it: the title,
  the kind's icon, and the arguments, updated in place when the agent
  sends them after the call started (Claude Code does); its status; its
  content as text, images, and links; edits as diffs; the files it touched
  as links; and its raw output when it sends no content. With Claude Code,
  the call's tool name (`Read`, `Bash`) is the name it was called by.
- **Notices.** What the agent flags for you (a rate limit, a retry, a
  compaction of its conversation with its summary) shows as a notice in the
  transcript, colored by how much it matters. Cookie asks agents for
  notices and compaction updates (`session.notices` and
  `session.compaction`), so they do not arrive as message text.
- **Steering.** A message you send while the agent works goes into its
  running turn when the agent supports steering (it advertises the
  steering extension, as Claude Code's adapter and Codex's do): the agent
  reads it at its next step, the transcript marks the message *steered into
  the agent's running turn*; a scheduled job set to `steer` and a
  subagent's report arrive the same way. An agent without it takes the message with its next turn: the
  web client's Steer button then reads "Queue (this agent can't be
  steered)", and the TUI's pending strip says the agent can't be steered.
- **Interrupting** a turn cancels it on the agent.

## Delegating to an ACP agent

An ACP agent of mode `all` or `subagent` is a delegation target like any
other: name it in a `delegate` rule, and `delegate_subagent` runs a child
session whose turns the agent takes.

```yaml
permissions:
  delegate: { reviewer: allow, labpc-codex: allow }
```

The name may be one no agent has yet: an agent of a runtime that dials in is
known only once that runtime connects. The configuration still loads, with a
diagnostic: `cookie config status`, the TUI's `/config`, and the web's
configuration page show

```text
Note: agent `lead` delegates to `labpc-codex`, which is not known yet: no agent document has that ID and no connected runtime reports it
```

until the runtime connects and reports the agent; then the note clears and
the agent is a target. Delegating to it before then is refused with
``no agent `labpc-codex` (its runtime is not connected)``. A typo stays a
note.

## Troubleshooting

- **An authentication error ends the turn.** The run fails with what the
  agent said, typically that authentication is required. Log in to the agent on the
  runtime's machine, as the runtime's user (for a service, the account it
  runs as), and send the message again.
- **The agent is dimmed in the picker, or a session with it is refused
  with "runtime `lab` is not connected".** Its runtime is away. Start
  `cookie runtime serve --daemon ...` on that machine; the agent becomes
  available once it connects.
- **The agent is missing from the picker.** Its runtime has not reported
  it: check the declaration on the runtime's machine (a dial-in runtime) or
  in the daemon's `config.toml` (`local` and templates, applied at the
  daemon's next start), and the daemon's log, which says why it skipped a
  declaration whose ID collides with another agent.
- **"The runtime could not start the agent" (`agent_failed`).** The command
  did not run on the runtime's machine: `npx` or the agent is not on the
  `PATH` the runtime sees (a service starts with a minimal environment; add
  it to the unit or to the declaration's `env`).
- **The run fails with the agent's last stderr lines.** The agent's process
  exited; the next turn starts it again and resumes its ACP session when the
  agent can.
- **"which is not known yet" in the configuration status.** A `delegate`
  rule names an agent no runtime has reported; see
  [Delegating to an ACP agent](#delegating-to-an-acp-agent).
- **A steered message arrives after the turn.** The agent took it with its
  next turn: either it cannot be steered, or its turn had already ended
  when the message reached it.
- **The agent does not see cookie's tools.** `cookie runtime mcp-bridge`
  must run on the runtime's machine; the session says when the bridge could
  not be opened.

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

The daemon lists them as `claude`, `codex`, and `gemini`, the keys of their
declarations, once it restarts (`[runtimes.local]` changes with the rest of
`runtimes`, at the next start). Pick one in the agent picker of the TUI (`/agent`) or the
web client (the agent chip) for a new session, and send a message: the daemon starts the
agent's process on the first turn, opens an ACP session in the session's
workspace, and shows the agent's reply, thoughts, tool calls, and plan as
they stream.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `command` | array of strings | required | The program and its arguments, run without a shell on the runtime's machine, in its home directory. On Windows, a bare program name also finds a `.cmd` or `.bat` launcher on `PATH`, such as `npx`'s. |
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

The agent is `codex`, run on `labpc` by a session that attaches `labpc` (a
`labpc` workspace); the daemon starts the runtime when the session first
uses it. See [Which runtime runs it](#which-runtime-runs-it).

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

The agent is `claude`, which a session that attaches `lab` runs on the lab
machine while the runtime is connected. The runtime reads its configuration
again every few seconds and reports changed agents, so adding one needs no
restart. To keep it connected, run it as a
[service](../runtimes/service.md).

## Which runtime runs it

An agent's ID is the key of its declaration, whichever runtime declares it:
`[runtimes.local.acp.agents.claude]` and a lab machine's own
`[runtimes.local.acp.agents.claude]` both offer the one agent `claude`. A
session runs it on the runtime it attaches that offers it:

- A session that attaches `local` (the default for a new session) runs
  `claude` on the daemon's machine; one that attaches only the lab
  machine's runtime runs it there.
- A session with no attached runtime that offers it cannot run it: creating
  the session, starting a run, or delegating to it is refused with, for
  example, ``\`claude\` needs a runtime that offers it attached: lab, local``,
  and the agent pickers show it dimmed with what it needs.
- A session that runs it may not attach two runtimes that offer it:
  attaching the second is refused, naming both. Sessions of other agents
  attach any runtimes.
- Moved to another runtime (detach one, attach the other), the agent starts
  a new ACP session there, and the transcript says so: the old one stays on
  the machine that ran it.
- A subagent runs it on the runtimes it has: its parent's, or the
  capabilities its delegation names, by the same rule.

`list_agents` shows an agent's runtimes and whether it runs in the calling
session, and `list_runtimes` names the ACP agents each runtime offers.

## Cookie settings: the daemon-side document

Cookie settings come from an agent document of the agent's ID in the
daemon's agents directory, never from the runtime, so a runtime cannot grant
itself permissions. Every runtime that offers the agent shares it:

```yaml
# ~/.cookie-agent/agents/codex.md
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

- **Optional.** Without it, the agent has mode `all`, the first offering
  runtime's description, the rules every agent gets for actions it names no
  key for, and the default `hide_tools`.
- **`acp`** marks the document as the agent's; `acp: {}` is enough. `mode`
  picks the mode its sessions start in (else the agent's `auto` mode, else
  its default), and `config` sets its config options by ID, such as its
  `model` or reasoning `effort`; an unknown option or value is reported in
  the session and skipped.
- **No `models`.** The agent chooses its own model; set it through
  `acp.config` when the agent offers a model option.
- **A document without an `acp` block** of the same ID is the daemon's own
  agent: it keeps the ID, the runtimes' declarations are ignored, and the
  configuration status (`cookie config status`, the TUI's `/config`, the
  web's configuration page) says so.
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

Save it as `~/.cookie-agent/agents/claude.md`: every runtime that offers
`claude` shares it.

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
lost the one it had (or a fork or revert before its first reply starts
over), and not when it resumes, loads, or forks one, since the agent
remembers them there. Later prompts carry only the input.

## Forking, reverting, and deleting a session

The agent keeps its own conversation, so cookie keeps it in step when you
change the session's:

- **Fork.** A fork of the session gets its own agent conversation: at its
  first turn cookie asks the agent to fork the source's (`session/fork`),
  and the source keeps its own. An agent without `session/fork` (Gemini
  CLI) cannot be forked after its first reply; the fork is refused with
  `` `gemini` on `local` can't fork its own conversation ``. A fork at an
  earlier point than the end of the agent's conversation also needs its
  fork to start at a given message (below). An agent whose fork cannot
  start at a given message is asked to fork as you fork the session, so
  the fork holds the conversation as it was then; if the agent cannot, the
  fork is refused.
- **Revert.** After a revert the agent's next turn forks its conversation
  at its last reply the session keeps, then deletes the old conversation,
  and the transcript says *the agent continues from the revert point*. The
  agent must fork at a given message: cookie names it with JetBrains AIR's
  fork point, which Claude Code's and Codex's adapters read. Cookie checks
  whether the agent does once per agent process, after its first reply in
  a run; until then, and for an agent that does not (Gemini CLI, or one
  whose fork ignores the fork point), a revert past the agent's first reply
  is refused with `` `claude` on `local` can't rewind its own conversation ``
  (or that cookie has not checked yet). A revert to before the agent's
  first reply always works: the next turn starts a new agent conversation.
- **Delete.** Deleting a session also deletes the agent's own transcripts
  of it (`session/delete`, when the agent offers it), but never those of
  the session it was forked from.

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

## Questions from the agent

An agent can ask you something mid-turn (ACP elicitation): Claude Code's
`AskUserQuestion` arrives as a form, and an MCP server that needs you to
sign in as a page to open. Cookie advertises both kinds, so the agent asks
instead of guessing or failing. The run waits on you meanwhile, and the
clients show the question in the viewed session:

- **A form** lists its fields in order, each with its title, description,
  and what it takes: a choice (pick one), a multiple choice (pick any), yes
  or no, text, or a number, and whether it is required. **Submit** sends
  your answers; cookie checks them against the form first and names a field
  that does not fit. **Decline** says no, and **Cancel** dismisses the
  question without an answer. With Claude Code, each question comes with an
  *Other* field for an answer of your own; leaving everything empty skips
  the question.
- **A page to open** shows the whole URL and its host, and warns when it is
  not HTTPS, has a punycode host, or carries a user name. Open it yourself
  (cookie never opens or fetches it), finish there, then choose **Done**;
  or **Decline** or **Cancel**. When the agent reports that you finished
  there before you choose, cookie answers Done for you.

No permission rule or mode answers a question: it is the agent asking you,
not an action. Where no one is there to answer, cookie declines it at once
with the reason, so the turn goes on: a session in `auto_approve_n` (a cron
run's default mode), and a headless `cookie run`. Interrupting the turn
cancels a question still waiting. The transcript keeps a line for each
answered question (an *info* event).

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
  the agents runtimes offer with where the new session (or the viewed
  one) would run them (`claude [on local]`). One that cannot run there is
  shown dimmed with why and cannot be picked: no attached runtime offers it
  (`needs lab attached`), two do (`offered by lab, local: attach one`), or
  its runtime is away or has it disabled (`on lab · unavailable`). No model picker lists
  an ACP agent's synthetic model `acp/<agent>`: the agent chooses its own,
  and the model picker lists the agent's models instead (below).
- **Agent commands.** Once the agent has announced its slash commands, the
  TUI's command palette (`/` on an empty composer, or ctrl+p) lists them
  after cookie's own, marked `(agent)`; a command that takes input asks for
  it. In the web client, typing `/` in the composer lists them above it.
  Choosing one sends `/<name> <input>` to the agent.
- **Model and effort.** The agent's model option (category `model`) and
  its reasoning effort (category `thought_level`), when it offers them,
  read as the session's model and variant: `claude • Opus[High]` in the
  TUI's title, `Opus • High` on the web client's model chip. The model
  picker (the TUI's `/model` or the title's model, the web client's model
  chip) lists the model option's values, and the variant (the TUI's
  bracketed variant or ctrl+t, the efforts in the web client's model sheet)
  the effort option's. Claude Code offers `model` and `effort`; Codex
  `model` and `reasoning_effort`.
- **A new session's draft** offers the options the agent last started a
  session with on the runtime the draft would run it on, with the
  document's `acp.mode` and `acp.config` applied: what its new session
  would start with. The daemon remembers them per agent and runtime in
  `acp-agents.json` in its data folder, across restarts. Your choices go
  with the new session and apply when its agent session starts, after the
  document's `acp.config`; one the agent no longer takes is skipped with a
  note in the transcript. Before the agent has started a session on that
  runtime, there is nothing to offer yet: send a first message.
- **Agent options.** The agent's other options, such as its mode, are the
  session's agent options: `/options` in the TUI, and **All agent options**
  in the web client's model sheet. An on/off option, such as Claude Code's
  fast mode, is a switch in the web client and toggles from its row in
  `/options`. Choosing a value changes it on the agent at once (on a
  draft, when its session starts), and a mode the agent switches to by
  itself (Claude Code leaving plan mode) shows at once too.
- **Context.** The status line shows how much context the agent's
  conversation uses, as the agent reports it (`ctx` in the TUI, with the
  share of the agent's context window; the tokens in the web client).
- **Plan.** The agent's plan shows in the transcript as a checklist after
  the turn's reply, each step done (`[x]`), in progress (`[~]`), or pending
  (`[ ]`), updated in place as the agent works; each turn's plan stays where
  it was.
- **Tool calls** the agent reports name it after their title
  (`Read notes.txt · claude on local`).
- **Tool calls** show what the agent reports as it reports it: the title,
  the kind's icon, and the arguments, updated in place when the agent
  sends them after the call started (Claude Code does); its status; its
  content as text, images, and links; edits as diffs; the files it touched
  as links; and its raw output when it sends no content. With Claude Code,
  the call's tool name (`Read`, `Bash`) is the name it was called by.
- **Images** the agent puts in its reply show in the reply, as an image a
  model generates does; cookie keeps them with the session.
- **Command output.** A command the agent runs shows its output as cookie's
  own `bash` calls do: in a terminal block that grows while the command
  runs (Codex streams it; Claude Code sends it when the command ends), with
  the exit code once it ends. The output is kept with the call, and what
  does not fit the preview is kept as an artifact. Cookie asks Claude Code's
  and Codex's adapters for it (`clientCapabilities._meta.terminal_output`);
  the agent still runs its commands itself.
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
- **Questions.** A question from the agent (see [Questions from the
  agent](#questions-from-the-agent)) docks a panel above the TUI's status
  bar, titled `<agent> asks · waiting on you`: Tab moves between the fields
  and the buttons, the arrows between a field's choices, Space picks or
  ticks, Enter picks and moves on or presses the focused button, `c` copies
  a page's link, and Esc cancels. The web client shows it as a card above
  the composer, with the same fields and buttons and an *Open* link for a
  page, and its bottom bar says *waiting for your answer*.
- **Interrupting** a turn cancels it on the agent.
- **Compaction.** `/compact` (in the TUI, the web's compaction action)
  runs the agent's own `compact` command when it offers one (Claude Code
  does), with the focus you give; otherwise it is refused with
  `` `gemini` does not offer compaction ``. Cookie never compacts an ACP
  agent's conversation on its own; the agent does, and its compaction
  shows as a notice.
- **Titles.** The agent's own title names the session, and each new one
  replaces the last until you rename the session. An agent that names
  nothing gets cookie's generated title after its first reply, on your
  `small_model`; with no `small_model` the session stays untitled.
- **A run that fails because the agent stopped** says why: "the agent
  refused", "the agent hit its output token limit", or "the agent hit its
  turn request limit". What the agent used up to then still counts in the
  session's usage.

## Delegating to an ACP agent

An ACP agent of mode `all` or `subagent` is a delegation target like any
other: name it in a `delegate` rule, and `delegate_subagent` runs a child
session whose turns the agent takes.

```yaml
permissions:
  delegate: { reviewer: allow, codex: allow }
```

The name may be one no agent has yet: an agent of a runtime that dials in is
known only once that runtime connects. The configuration still loads, with a
diagnostic: `cookie config status`, the TUI's `/config`, and the web's
configuration page show

```text
Note: agent `lead` delegates to `codex`, which is not known yet: no agent document has that ID and no connected runtime reports it
```

until the runtime connects and reports the agent; then the note clears and
the agent is a target. Delegating to it before then is refused with
``no agent `codex` (no connected runtime offers it)``. A typo stays a
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
- **"can't rewind its own conversation" or "can't fork its own
  conversation".** The agent cannot fork its conversation where the
  revert or the fork would cut it (see
  [Forking, reverting, and deleting a session](#forking-reverting-and-deleting-a-session)).
  Revert or fork to before the agent's first reply, or go on from where
  the session is. "cookie has not yet checked" clears after the agent's
  next reply.
- **The agent's question was declined without you seeing it.** The session
  runs unattended: its permission mode is `auto_approve_n` (a cron run's
  default), or it is a headless `cookie run`. The transcript's info line
  says so; switch the mode to answer questions yourself.
- **The agent does not see cookie's tools.** `cookie runtime mcp-bridge`
  must run on the runtime's machine; the session says when the bridge could
  not be opened.

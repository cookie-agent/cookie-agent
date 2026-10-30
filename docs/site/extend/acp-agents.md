# ACP agents

External coding agents, such as Claude Code and Codex, can take
a session's turns, or a subagent's, as cookie agents. Cookie drives them over
the [Agent Client Protocol](https://agentclientprotocol.com) (ACP): cookie is
the ACP client, the external agent the ACP agent. Cookie speaks ACP version
2 only. The agent keeps its own
conversation, model, file tools, and shell; cookie renders its turns, decides
its permission requests with your rules, offers it cookie's own tools (memory,
cron, delegation, MCP servers), and keeps the session's history as for any
other agent.

A *runtime* hosts each agent: the daemon's own `local` runtime, a runtime the
daemon starts over ssh, or a runtime that dials in, such as one on a lab
machine or the one a TUI or web client offers. The runtime starts the agent on
its own machine, with that machine's credentials, and relays its messages to
the daemon over the runtime connection; the agent's host needs no route to the
daemon. The [ACP agents spec](../specs/acp-agents.md) has the design, and
[ACP v2](../specs/acp-v2.md) the port to version 2.

## Before you start

On the machine that runs the agent:

- Install the agent and log in to it there, as you would to use it in a
  terminal: `claude` and its `/login`, or `codex login`. Cookie runs no
  login flow; it uses the credentials already on that machine.
- Install its ACP adapter, or let `npx` fetch it: Claude Code speaks ACP
  through `@agentclientprotocol/claude-agent-acp`, and Codex through
  `@agentclientprotocol/codex-acp`. Both speak ACP v2. `npx` needs Node.js on
  that machine's `PATH`, as the runtime sees it.
- **ACP v2 only.** An agent that answers `initialize` with version 1 is
  refused, and its runs fail with `` <agent> speaks ACP v1; cookie needs ACP
  v2. Update its adapter, or start it in v2 mode ``. Gemini CLI's
  `gemini --acp` may speak only v1; Claude Code's adapter serves both and
  can be pinned to v2 with `CLAUDE_AGENT_ACP_PROTOCOL=v2` in the
  declaration's `env`.
- `cookie` must be on that machine: the MCP bridge that offers cookie's tools
  to the agent is `cookie runtime mcp-bridge`. A runtime always has it, since
  it is `cookie runtime`.
- No model provider is needed: a daemon whose only agents are ACP agents
  runs them, from the TUI, the web client, and `cookie run -a <agent>`.
  Without a provider's `small_model`, a session the agent does not name
  stays untitled, and the setup banner can be dismissed with **Not now**.

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
```

The daemon lists them as `claude` and `codex`, the keys of their
declarations, as soon as the configuration is applied: a changed `acp`
table reaches its runtime at once. Pick one in the agent picker of the TUI (`/agent`) or the
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

A [runtime that dials in](../runtimes/dial-in.md) hosts the agents of its
table in the daemon's `config.toml`: a `[runtimes.<id>]` without `command`,
named after the ID it dials in with, exactly where its MCP servers go. The
command runs on the lab machine:

```toml
# ~/.cookie-agent/config.toml on the daemon's machine
[runtimes.lab.acp.agents.claude]
command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
description = "Claude Code on the lab machine"
```

```sh
# On the lab machine: dial in as `lab`.
cookie runtime serve --daemon ws://laptop:7419/ws --id lab --token <token>
```

The agent is `claude`, which a session that attaches `lab` runs on the lab
machine while the runtime is connected. A change to the table reaches the
connected runtime at once, so adding an agent needs no restart. To keep it
connected, run it as a [service](../runtimes/service.md).

## On every runtime

To offer an agent on every runtime without declaring it on each, declare
it once under `[acp.agents.<agent>]`:

```toml
[acp.agents.claude]
command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
description = "Claude Code"
```

Every runtime but `global` offers it, and runs it on its own machine, so
the command must work there. A runtime's own declaration of the same key
replaces it on that runtime: give it another command there, or leave it off
with `enabled = false`:

```toml
[runtimes.labpc.acp.agents.claude]
command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
enabled = false
```

In the web client, the Machines view lists these under **On every
runtime**, where **ACP agent** adds one; each runtime's card shows the ones
it offers with an `every runtime` badge, and **Turn off here** writes that
runtime's own declaration with `enabled = false`.

## Which runtime runs it

An agent's ID is the key of its declaration, whichever runtime declares it:
`[runtimes.local.acp.agents.claude]` and `[runtimes.lab.acp.agents.claude]`
both offer the one agent `claude`. A
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
- The agent works in the session's first workspace on that runtime (the
  runtime's home without one). The session's other workspaces on the same
  runtime reach an agent that supports it (`additionalDirectories`) as
  extra workspace roots, when its session starts or resumes: attach
  `local` at two folders and the agent may work in both. Workspaces on
  other runtimes are on other machines, and the agent does not get them.

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
models: [{ model: "gpt-5", variant: "high" }]
acp:
  mode: auto
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
  its default), and `config` sets its other config options by ID, such as
  Claude Code's `fast`; an unknown option or value is reported in the
  session and skipped.
- **`models`** picks the agent's model and effort, as a fallback chain
  over the agent's own values (not cookie's model keys): each entry's
  `model` is a value of the agent's model option and its `variant` one of
  its effort option, or `null` for the agent's default effort. For Claude
  Code, `models: [{ model: "opus", variant: "high" }, "sonnet"]`. The
  first entry whose model the agent offers applies when its session
  starts; when none does, the session says so and the agent keeps its own.
  The model and variant pickers show each value's ID, dimmed beside its
  name: the ID to write here. `models` beats `acp.config` for the model and effort
  (the configuration status says so when both set one), and a draft's or
  session's own choice beats `models`. `${default_model}` and the other
  variables name cookie's models, so an entry naming one is skipped with a
  configuration diagnostic.
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

### Several documents over one agent

One declaration can serve several documents, each its own agent with its
own permissions, mode, `hide_tools`, and body. `acp.agent` names the
declaration a document runs; without it, a document runs the declaration
of its own ID:

```yaml
# ~/.cookie-agent/agents/claude-review.md
---
description: Claude Code, read-only reviewer
mode: subagent
acp:
  agent: claude          # a [runtimes.*.acp.agents.claude] declaration
  mode: plan
permissions:
  read: allow
  write: deny
  bash: { "git diff *": allow, "*": ask }
---
You review the change you are given and report findings; you do not edit.
```

- **The document's ID is the agent's.** `claude-review` is listed, picked,
  delegated to, and scheduled by that ID, on the runtimes that declare
  `claude`; the pickers say `claude on local`, and `list_agents` adds
  `acp agent: claude`.
- **The declaration stays an agent** of its own ID while no document has
  that ID; a `claude.md` with `enabled: false` hides it and leaves the
  documents over `claude` running. A `claude.md` whose `acp.agent` names
  another declaration takes the ID, and the configuration status says the
  `claude` declaration is ignored.
- **One process.** The documents over a declaration share its agent process
  on each runtime, each cookie session with its own ACP session, and share
  what the agent offered there (its models and options in the
  pickers). Switching a session to another document starts a new ACP
  session, and so does changing a document's `acp.agent`.
- **`command` and `env` stay with the declaration**, so documents cannot
  change them; a different environment (another `CLAUDE_CONFIG_DIR`) is a
  second declaration.
- A document whose `acp.agent` no runtime declares yet is no agent until
  one does; the configuration status says so.

### A reference document for Claude Code

The agent learns from the document's body that it runs inside cookie and
which of cookie's tools to prefer over its harness's own. Adapt this one to
your setup; it names Claude Code as the harness, so change that line for
another agent:

```markdown
---
description: Claude Code
mode: all
models: [{ model: "opus", variant: "high" }]
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
- Your built-in file and shell tools work in this session's workspace;
  with several attachments, the first prompt says which one, and on which
  machine.
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
2. the skills a model would see (`<available_skills>`: each skill's name,
   description, and file), when the agent is offered cookie's `skill` tool
   over the bridge, with the same rules a model's listing follows (a
   denied skill, or one that opts out of model invocation, is left out);
3. where the session's attachments work, as a model's system prompt says
   it, even for an attachment whose tools `hide_tools` hides: with one
   attachment, its working directory and machine; with several, a
   `<machines>` listing of every attachment by machine and runtime, which
   explains tool names such as `labpc_read`, with the attachment your
   harness's built-in tools run on marked `builtin_tools="true"` and a
   sentence naming that attachment, its runtime and machine, and the
   working directory: the built-in tools act on that machine and workspace
   only, while cookie's tools act on the attachment their description or
   `<attachment>_` prefix names. A session in a
   [project](../concepts/projects.md) also names the project here;
4. the session's memory as a model would see it at the start of a session
   (the `# Memory` reminder);
5. the agent document's body.

`AGENTS.md` is not sent: the agents read it themselves. The prompt carries
these again when cookie has to start a new ACP session because the agent
lost the one it had (or a fork or revert before its first reply starts
over), and not when it resumes, loads, or forks one, since the agent
remembers them there. Later prompts carry only the input.

An agent that takes a system prompt (`sessionCapabilities.systemPrompt`;
Claude Code and Codex do) gets these as one instead, appended to its own
instructions when cookie opens its session, and the first prompt carries
only your message, so an agent that names its session after its first
message (Codex) names it after yours. Its system prompt is not part of its
conversation, so cookie sends the same one again whenever it resumes or
forks the agent's session. The transcript shows what was sent either way.
An agent's title that only repeats the start of what cookie put ahead of
your first message names nothing: the session keeps cookie's title.

## Forking, reverting, and deleting a session

The agent keeps its own conversation, so cookie keeps it in step when you
change the session's:

- **Fork.** A fork of the session gets its own agent conversation: at its
  first turn cookie asks the agent to fork the source's (`session/fork`),
  and the source keeps its own. An agent without `session/fork` cannot be
  forked after its first reply; the fork is refused with
  `` `<agent>` on `local` can't fork its own conversation ``. A fork at an
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
  a run; until then, and for an agent that does not (one without
  `session/fork`, or whose fork ignores the fork point), a revert past the agent's first reply
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
cancels a question still waiting. The transcript shows each question as
a row where the agent asked it, `claude asks: “…”`, which says *waiting
for your answer* until it is answered and then how, with the answers given
(`accepted`, then `Plan: safe`). The row splits the turn's work: in the
reduced transcript, what the agent did before the question folds into
its own `Worked for` row, and what it does after opens a new one.

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

The transcript's tools row, pinned under the system prompt in the TUI and
the web client, lists the tools the bridge offers the agent in its latest
turn (*cookie tools · 12 offered to the agent*); the agent's own tools are
not in it.

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
  Choosing one sends `/<name> <input>` to the agent. The agent's skills,
  which Codex announces as commands named `$<skill>`, are listed only for
  a search that starts with `$` (`$review` in the TUI's palette), and
  choosing one sends the mention `$<skill> <input>`, as Codex takes it.
- **Model and effort.** The agent's model option (category `model`) and
  its reasoning effort (category `thought_level`), when it offers them,
  read as the session's model and variant: `claude • Opus[High]` in the
  TUI's title, `Opus • High` on the web client's model chip. The model
  picker (the TUI's `/model` or the title's model, the web client's model
  chip) lists the model option's values, and the variant (the TUI's
  bracketed variant or ctrl+t, the efforts in the web client's model sheet)
  the effort option's, each value with its ID dimmed beside its name: the
  ID an agent document's `models` names it by. Claude Code offers `model`
  and `effort`; Codex `model` and `reasoning_effort`.
- **A new session's draft** offers the options the agent last started a
  session with on the runtime the draft would run it on, with the
  document's `acp.mode`, `acp.config`, and `models` applied: what its new
  session would start with. The daemon remembers them per agent and runtime in
  `acp-agents.json` in its data folder, across restarts. Your choices go
  with the new session and apply when its agent session starts, after the
  document's `acp.config` and `models`; one the agent no longer takes is skipped with a
  note in the transcript. Before the agent has started a session on that
  runtime, there is nothing to offer yet: send a first message.
- **Attachments.** Files go to the agent as it takes them: images to an
  agent that takes images, audio to one that takes audio, and
  text files, up to 1 MiB, as embedded context to one that takes it
  (Claude Code and Codex do). The composers offer attaching once the
  agent has started on that runtime and check files against what it
  advertised; anything else, such as a PDF, reaches the agent as a note
  that it cannot read the file.
- **Agent options.** The agent's other options, such as its mode, are the
  session's agent options: `/options` in the TUI, and **All agent options**
  in the web client's model sheet. An on/off option, such as Claude Code's
  fast mode, is a switch in the web client and toggles from its row in
  `/options`. Choosing a value changes it on the agent at once (on a
  draft, when its session starts), and a mode the agent switches to by
  itself (Claude Code leaving plan mode) shows at once too. The session
  keeps its options, model and effort included: when cookie resumes or
  forks the agent's session, after its process stopped while idle
  (`[acp] idle_shutdown_ms`) or the daemon restarted, it sets back every
  value the agent came back without (Codex resumes on its own defaults).
- **Context.** The status line shows how much context the agent's
  conversation uses, as the agent reports it (`ctx` in the TUI, with the
  share of the agent's context window; the tokens in the web client).
- **Usage and cost.** Each turn's usage is what the agent reports when
  its work ends, once for the turn however many steps it took: its input
  counts the cached tokens too (ACP's `inputTokens` leaves them out), and
  its cost is the one the agent reports (Claude Code reports one; Codex
  none). The session's usage and estimated cost add these up, as for any
  other agent.
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
  the call's tool name (`Read`, `Bash`) is the name it was called by. A
  command the agent runs through a shell string (Codex reports
  `/bin/bash -lc '<command>'`) shows as the command itself. Codex's
  auto-review of one of its calls (its "Guardian Review") shows as one line
  with its verdict and what it reviewed (`Auto-review: approved, low risk`
  · `cat calc.py`), its reasoning as the line's output.
- **Images** the agent puts in its reply show in the reply, as an image a
  model generates does; cookie keeps them with the session.
- **Command output.** A command the agent runs shows its output as cookie's
  own `bash` calls do: in a terminal block that grows while the command
  runs (the agent's ACP terminal, as it streams its output or sends it
  whole), with the exit code once it ends. The output is kept with the
  call, and what does not fit the preview is kept as an artifact. The agent
  runs its commands itself.
- **Notices.** What the agent flags for you (a rate limit, a retry, a
  compaction of its conversation with its summary) shows as a notice in the
  transcript, colored by how much it matters.
- **Steering.** A message you send while the agent works goes into its
  running work: cookie sends it as a prompt during the work, and the agent
  takes it in at its next step. Until the agent takes it, the message waits in
  the pending strip, where any client can take it back; once taken, it
  shows in the transcript where the turn stood, as a steered message does
  for cookie's own agents. A message the agent declines keeps waiting and
  goes with its next turn. A scheduled job set to `steer` and a subagent's
  report arrive the same way.
- **Work the agent starts on its own.** An agent can start work with no
  message of yours, such as Claude Code's follow-up when a command it ran
  in the background ends. Cookie shows it as a run of its own, under a row
  saying the agent woke on its own (`<agent>, on its own`), with everything
  it does as in any turn: its reply, its tool calls, its permission requests
  and questions, and cookie's tools. Stop stops that work too; a message you
  send meanwhile goes into it. It shows even after you stopped a run, since
  the agent is already working.
- **Commands left running.** A command the agent leaves running when its
  call ends (Claude Code's background commands), or when its turn ends or
  is stopped with the call still open (Codex keeps a command running after
  a stop), shows in the session's
  background calls, marked "run by the agent", until it exits. Cookie keeps
  the agent's process and the session alive meanwhile, and an idle daemon
  restart waits for it. Only the agent can stop it: ask it to. When the
  command ends, the agent usually starts work on its own to report it.
- **The agent's own subagents.** A subagent the agent starts itself (Claude
  Code's Task and Agent subagents) is a session of its own, nested under
  the session that started it, as cookie's own subagents are. Its title is
  the agent's name for it; its runs show what it does, starting with the
  task it got; its permission requests and questions are asked in it. It
  takes no messages: it works on what its parent gives it, and the parent
  agent gets its result. While it works it is in its parent's background
  calls as a `subagent`, beside cookie's own subagents (`delegate_subagent`);
  Stop there or in its own session stops its work. It lives in the agent's
  process: once that process ends, it takes no more turns.
  Its session shows what its agent gives it, never what cookie composes
  for its parent: no system prompt row and no memory row, and its tools
  row lists its parent's cookie tools, since it reaches cookie over its
  parent's bridge. Codex hands the context cookie sent as its parent's
  system prompt on to its subagents, which show it as context from the
  parent; Claude Code gives its subagents a prompt of their own and none of
  that context. An agent cookie does not know is taken to hand its system
  prompt on. A call of cookie's tools the subagent makes shows once, in
  the subagent's session, decided by its rules; one its rules do not offer
  runs in the parent, and shows there.
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
  `` `<agent>` does not offer compaction ``. Cookie never compacts an ACP
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

- **"The agent document's `models` was skipped there".** The agent offers
  none of the entries' models, or not the entry's variant, on that
  runtime: the session runs on the agent's own choice. The model and
  variant pickers show the IDs the agent takes, dimmed beside each name.

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
  next turn: it declined to take it into its running work, or its work had
  already ended when the message reached it.
- **"speaks ACP v1; cookie needs ACP v2".** The agent's adapter answered
  `initialize` with version 1. Update the adapter, or start it in its v2
  mode (Claude Code's: `CLAUDE_AGENT_ACP_PROTOCOL=v2`).
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

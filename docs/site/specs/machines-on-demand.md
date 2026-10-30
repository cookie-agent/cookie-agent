# Machines on demand: official agents, model tiers, attachments from tools, routed tools, and transfers

Implemented, 2026-10-30 (client protocol 1.56.0, runtime protocol 1.13.0).
Every decision is made; the [implementation notes](#implementation-notes)
list where the code departs from the text below.

**Decided by the user, 2026-10-29:**

- Today's `small_model` is renamed `utility_model`. A new `small_model` is
  the model of subagents that carry out work, such as implementation,
  like the user's `sub-coder` (section 2.2).
- The daemon carries no default agent. The kit's agents and the internal
  agents move to an official `agents` repository that the daemon pulls into
  `<data_root>/official_agents/`, as it does official skills. A document in
  `agents/` with the same ID overrides the managed one (section 2.1).
- `transfer` never stores what it moves: no artifact and nothing in the
  session log beyond what moved where. The model cannot write to the
  artifact store. Artifacts can be copied out to a machine, never in
  (section 6).
- The official agents live in `cookie-agent/agents`, served at
  `https://cookie-agent.github.io/agents/`, beside `cookie-agent/skills`.
- The official `explore` runs on `${small_model}`, like `worker`.
- Compaction alone keeps a compiled-in prompt, used only when neither an
  authored nor an official `compaction` document exists (section 2.1).
- `primary` becomes the general assistant and the coding prompt moves to
  `coder`. A new `[agents] default` names the default agent (section 2.1).
- No prefixed tool names at all: every runtime tool is routed by
  `machine` (section 5). So one machine may not have two attachments that
  offer a tool of the same name, which includes two attachments of one
  capability (section 4).
- The TUI keeps attaching `local` at its directory (section 3.4).
- `machine` is optional, and each tool states its default machine
  explicitly; the machines block says which machines the session is
  attached to and points at `list_runtimes` (sections 5.1 and 5.4). This
  replaces the earlier "required when several machines offer the tool".
- The tools are `attach_machine` and `detach_machine` (section 3.1).
- Any run with an `attach` rule may attach; by default root runs are
  allowed and subagents denied (section 3.3).
- Uploads that no model takes are accepted as `kind: file`, and every file
  part carries a label naming its artifact, which is also the fallback for
  media the model cannot take (section 6.4).
- Official agents' refreshes apply as they arrive, frontmatter and
  permissions included, as official skills' do (section 2.1).
- No official agent uses `{{default_prompt}}`; each holds its whole prompt.
  In a document of the user's, `{{default_prompt}}` stands for the official
  agent's prompt of the same ID, else for the prompt the binary carries,
  which only compaction has (decided 2026-10-30).

## Implementation notes

What the implementation does differently from the sections below, or adds:

- **`transfer` is offered only where it can run** (section 6.1): when the
  session has a machine attached whose runtime reports `files`, besides the
  `write` rule its permission name needs. A session on `global` alone gets
  it once `attach_machine` attaches a machine.
- **The relay pipeline** (section 6.3, step 3) reads one chunk while the one
  before it is written, rather than keeping up to 4 in flight: two chunks,
  at most 2 MiB, per transfer.
- **`fs/copy` takes two prepared routes** (`from`, `to`) instead of paths:
  both ends are prepared for the permission check anyway. Closing its `to`
  route stops it. A folder copied within one runtime while `[transfer]
  max_bytes` is set is relayed through the daemon instead, which counts it.
- **`fs/prepare` answers more** than a route: the lexical absolute `path`
  results show, the permission `label`, and a `binding` of the resolved
  location for the approval identity. The destination's existence is
  checked there too, so an existing destination without `overwrite` is
  refused before anyone is asked to approve.
- **Links that may lead outside a folder** (section 6.3) are left out at
  the destination and counted in `skipped`, as devices are at the source,
  rather than failing the transfer: a virtual environment's absolute links
  would otherwise make it impossible to copy. A link counts as inside when
  it is relative and climbs (`..`) only before its first name and no higher
  than the folder; names that climb and entries below a link the stream
  made still fail it.
- **Permissions per resource**: a prepared call can check each resource in
  its own scope (`ResourceScope`: runtime, workspace, home), so the source is
  checked as `read@<source runtime>` and the destination as
  `write@<destination runtime>` only, not each under both runtimes.
- **`kind: file` uploads** are stored as `application/octet-stream`, the
  type no model is sent as bytes; the label carries the name and size. The
  clients upload as a file whatever the selected model does not take
  inline, rather than refusing it, and offer attaching for any selected
  model; only the 25 MiB upload limit refuses a file.
- **Labels in the clients**: the label is model input the engine adds
  while it builds the history; the clients show a file as before, by its
  chip, not by the label.
- **Golden history**: the frozen golden logs predate labels, so their
  comparison ignores label text parts in the history views and nothing
  else.
- **An internal agent without a document** (`approval` or `title` before
  the official agents were first fetched, or with `[agents] official =
  false` and no document of the user's) is frozen disabled: its runs end
  unavailable, so a review passes the call to the user and a title falls
  back, instead of every run failing to start.
- **The web client's runtime cards** take one workspace each, and the note
  about prefixed tool names is gone; a second coding runtime on the same
  machine is refused on its card with the daemon's reason.
- **ACP sessions** keep the earlier runtimes block and are exempt from
  section 4's one-name-per-machine rule: an ACP agent takes no routed
  tools, and its extra directories are attachments of one capability.
- **`[agents] default`** is diagnosed only when it is set and names no
  runnable agent; without it the first runnable agent is the fallback.
- **Partial-copy cleanup after a crash** uses a journal: each partial copy
  is recorded under `~/.cookie-agent/transfers/` while it exists, and a
  start of `cookie runtime serve` (or the daemon's own `local` runtime)
  removes the ones recorded more than a day ago.

**Not done yet** (section 5.4, items 4 and 5): `list_runtimes` does not show
each runtime's tool groups, and the clients show a routed call's machine
only among its arguments, as the model sent `machine`, not as a badge in the
tool header.

## 1. Goal and scope

Cookie is meant to be an always-on agent platform that works across devices:
every integration and context in one place, kept private, and still minimal
in what each request carries. Today the defaults assume one coding agent
working in one directory:

- **The kit's `primary` is a coding agent** ("You help the user understand,
  change, and verify software in the session's workspace"). A session in
  **No project** starts with that prompt even when it has no workspace.
- **A session cannot reach a machine by itself.** Attachments change only
  from a client, and only between runs (`session.runtime.attach` is refused
  while a run is active). A global-only root reaches a machine only by
  delegating with `capabilities` (runtime delegation spec, section 1 lists
  "attaching runtimes to a running session from a tool" as out of scope).
- **Two attachments rename each other's tools.** A second coding attachment
  turns `bash` into `local_web_bash` and `labpc_api_bash`, and so on for
  `read`, `write`, and `edit`: four more tools per attachment, and on
  models that defer, the working tools leave mid-round to come back renamed
  (stable prefix spec, section 1, item 2).
- **Nothing moves files between machines.** An agent shells out to `scp`
  or `rsync`, which works only when the machines can reach each other. The
  daemon is the one place that reaches every runtime, and it never relays
  bytes. An artifact (a user's upload, retained tool output) can only be
  read as text, never saved to a machine.

This spec lets a session start with what its client gives it (`global`
alone on the web) and grow into machines as it needs them. It covers:

1. official agents, model tiers, and a prompt sweep (section 2);
2. `attach_machine` and `detach_machine` (section 3);
3. no overlapping tools on one machine (section 4);
4. routed tools: one `bash`, `read`, `write`, `edit` with a `machine`
   argument, in place of prefixed copies (section 5);
5. `transfer`: files and folders between machines, and from the artifact
   store to a machine, relayed and never stored (section 6).

Out of scope: starting a runtime on a machine the daemon has never seen
(the dial-in pattern of runtime delegation section 7 stays the way);
choosing a model per machine; ACP agents' own harness tools, which stay on
the one runtime that runs the agent.

## 2. Agents, models, and prompts

### 2.1 Official agents

Agent documents are cookie's own instructions as much as skills are, and a
kit installed once never sees a later release's prompts. They move to an
official source, kept current like official skills (default models and
onboarding spec, section 13).

- **The source:** a repository `cookie-agent/agents`, served at
  `https://cookie-agent.github.io/agents/`, with the skills repository's
  shape: `index.json` (`{ "version": 1, "agents": [ { "id", "version",
  "requires_cookie", "sha256" } ] }`) and one `<id>.md` per agent. Its CI
  runs the latest release's `cookie agents check`: every document parses
  with the release's strict agent parser, and every `${…}` variable,
  permission action, and tool it names exists.
- **What it holds:** the kit's agents and the internal ones:

  | ID | Mode | Models | Role |
  |---|---|---|---|
  | `primary` | `primary` | `${default_model}` | The general assistant (section 2.6) |
  | `coder` | `all` | `${default_model}` | Today's coding `primary`, for projects' defaults |
  | `explore` | `subagent` | `${small_model}`, `${default_model}` | Reads and searches, never writes |
  | `worker` | `subagent` | `${small_model}`, `${default_model}` | Carries out a delegated task, implementation included |
  | `approval` | `internal` | `${utility_model}`, `${parent_model}` | The approval reviewer |
  | `title` | `internal` | `${utility_model}`, `${parent_model}` | Session titles |
  | `compaction` | `internal` | `${parent_model}` | Checkpoint summaries |

- **The daemon:** `<data_root>/official_agents/`, written only by the
  daemon, refreshed at start and every 24 hours with hash checks, entries
  the index drops or whose `requires_cookie` this version fails removed,
  network failures keeping the cached copies with a configuration
  diagnostic. The refresh code is shared with official skills.
- **Precedence:** a document in `~/.cookie-agent/agents/` with the same ID
  replaces the official one whole, as today's authored documents replace
  the built-in internal ones. No merging of frontmatter or body.
- **Configuration:** `[agents] official = true` (default) or `false`,
  beside `[skills] official`.
- **The default agent** is a key in the existing `config.toml`
  (`<data_root>/config.toml`, beside `agents/`), since the daemon no longer
  synthesizes one:

  ```toml
  [agents]
  default = "primary"
  ```

  - It names the agent a new root session starts with when neither the
    client nor the session's project chooses one, and the agent a later
    turn falls back to when its own is gone (today hard-wired to
    `primary`, then the first root-runnable agent).
  - Precedence: the client's explicit choice, then the project's default
    agent, then `[agents] default`.
  - The value must be an agent ID. An agent of that ID that does not exist
    or cannot run as a root is a configuration diagnostic, not a load
    error, because official agents arrive after the configuration loads;
    a session created meanwhile fails with that diagnostic.
  - Clients read it from the agent listing (client protocol minor: the
    listing carries `default`) and preselect it, in place of looking for
    `primary` themselves. `/config` and the web Settings edit it.
  - Unset, it is `primary`, the official assistant.
- **Commands:** `cookie agents update`, `cookie agents list` (origin:
  user or official, and the official version), `cookie agents check <dir>`.
  Clients mark official agents in their pickers; the TUI and web editors
  open an official agent as a copy to save into `agents/`.
- **Removed from the binary:**
  - the synthesized `default` agent and its reserved ID;
  - the compiled-in approval and title prompts (`DEFAULT_APPROVAL_PROMPT`,
    `DEFAULT_TITLE_PROMPT`), which become the official documents' bodies.
    The compaction prompt stays compiled in as a last-resort fallback
    (decided 2026-10-29): the official `compaction` document carries the
    same text and wins over it, an authored one wins over both, and the
    fallback runs only on a daemon that has neither, so a long session never
    fails at its context limit for want of a summary. Both still say
    "coding agent" today (`runtime/approval_flow.rs`, `runtime/titles.rs`);
    the official versions do not.
- **Without a root agent** (official agents off and none authored, or the
  first refresh not done yet), `session.create` fails with an error naming
  `cookie agents update`, `agents/`, and `[agents] default`.
- **Without an internal agent,** the existing safe failures apply: approval
  escalates to the user and the title falls back to an excerpt. Compaction
  uses the compiled-in fallback, on `${parent_model}`. `cookie agents
  check`, in the repository's CI, keeps the repository's `compaction.md`
  body identical to the release's fallback.
- **Refreshes apply as they arrive** (decided 2026-10-29), frontmatter
  and permissions included, as official skills' do. The trust is in the
  repository: whoever can push to it, or serve its Pages site, decides what
  the official agents may do on every daemon that uses them. A user who
  wants to fix an agent's permissions copies it into `agents/`, where it
  overrides the official one and no refresh touches it; the docs say so.
- **A refresh mid-session** changes the next run's prompt, as editing an
  agent document does today: an admitted run keeps the snapshot frozen in
  `run_started`.
- **The starter kit** keeps `config.toml` only, and onboarding waits for the
  first agents refresh. Agent files an earlier kit installed into `agents/`
  stay the user's and keep overriding; onboarding lists the ones that are
  byte-identical to an old kit version and offers to delete them.

### 2.2 Model tiers

| Field | Variable | Used by |
|---|---|---|
| `default_model` | `${default_model}` | Root agents: `primary`, `coder` |
| `small_model` (new meaning) | `${small_model}` | Subagents that do the work: `worker`, `explore` |
| `utility_model` (today's `small_model`) | `${utility_model}` | Internal agents: `approval`, `title`; an ACP session's reviewer |

- **The rename is a hard break in meaning, not in shape:** a `[small_model]`
  table written today parses, and would quietly become the workers' model.
  Configuration is strict and never migrated, so the release notes, the
  configuration reference, and the onboarding check say so, and the
  user's own `config.toml` is updated in the same change.
- **Onboarding** asks for all three; `small_model` and `utility_model` may
  stay unset, and each entry list above falls through to the next model.
- **Client protocol minor:** onboarding status and input gain
  `utility_model`; `small_model` keeps its field and its new meaning.
  `AgentInfo`'s list of configuration fields an agent uses gains
  `utility_model`.
- **ACP options** that use `${small_model}` today
  (`runtime/acp/options.rs`) move to `${utility_model}`.

### 2.3 Agent prompts

- **`primary` becomes a general assistant.** Its body describes the
  platform, not one workspace (draft text in section 2.6). It is the agent
  a No-project session starts with.
- **`coder`** has today's `primary` body and permissions. A project's
  defaults name it, so a session in a repository project still gets a
  coding agent.
- **`explore` and `worker`** say "the machines and directories you were
  given" instead of "the workspace", since a global-only root hands them
  `capabilities`.
- **`approval` and `title`** describe "an AI agent" and "sessions", not a
  coding agent.

### 2.4 The runtimes block for a global-only session

Today a session with only `global` gets nothing in its runtimes block (a
`Project:` line at most), and the "no workspace" paragraph lives in the
delegate tool's section, so an agent that cannot delegate is never told.
The paragraph moves to the runtimes block, which already is a frozen and
announced prompt block (stable prefix spec, section 6):

```text
No machine is attached: you have no files or shell of your own yet.
list_runtimes shows the user's machines. To work on one yourself, call
attach_machine; to hand a task to a subagent there, delegate with
`capabilities`.
Project: none
```

Lines appear only for the tools the run is offered. The delegate section
drops its own paragraph.

### 2.5 Tool description sweep

- **`list_runtimes`** names `attach_machine` beside
  `delegate_subagent` as the consumer of what it lists.
- **The paging boilerplate** (`offset`, `limit`, truncated lines, the
  continue note) is repeated in `read`, `read_artifact`,
  `get_subagent_result`, and `memory_read`, about 450 characters each. It
  moves to `read`'s description; the others say "paged as `read` pages a
  file" and keep only what differs. Same for `edit` and `memory_edit`.
- **`list_agents`** and **`list_runtimes`** keep the ACP sentence to one
  clause each.
- **`delegate_subagent`'s `capabilities`** drops "The same runtime may be
  listed twice with different workspaces", which section 4 now refuses on
  one machine, and says that a child's tools take `machine` like the
  root's.
- **The several-attachments block** is rewritten for routed tools
  (section 5.4).
- Every changed text is regenerated into `docs/site/reference/tools.md` by
  the `tool_reference` test, as today.

### 2.6 Draft `primary` body

```markdown
You are Cookie Agent, the user's assistant across their devices. The user
may be on a phone, a laptop, or a terminal, and the work may need any of
their machines, or none.

- Answer directly when you can. Reach for a machine only when the task
  needs files, commands, or software that lives there.
- list_runtimes shows the user's machines. Attach the one the work belongs
  on with attach_machine, or delegate a self-contained task to a
  subagent there. Remove an attachment you no longer need.
- Keep what you learn about the user, their machines, and their projects in
  memory, so the next session does not have to rediscover it.
- Move files between machines with transfer, never by guessing that one
  machine can reach another.
- Ask before doing anything destructive or hard to undo, and never work
  around a denied permission.
- Finish with what you did, where (which machine), and what is left.
```

## 3. Attachments from tools

### 3.1 The tools

Two `global` tools in a new built-in group `machines`, behind a new
permission action `attach`:

```text
attach_machine(capability: string, workspace?: string)
detach_machine(capability: string)
```

- **`capability`** is written as in `delegate_subagent`'s `capabilities`:
  `<runtime_id>/<capability_id>` from `list_runtimes`, such as
  `labpc/coding`. `workspace` follows the same rule: required when the
  capability takes one, passed to the runtime as given.
- **The result** is what the model needs at once, since the runtimes block
  is frozen for the run: the attachment's name, its machine and platform,
  the runtime's home, the canonical workspace, and the workspace's
  `AGENTS.md` (the same lookup `runtime/context` does, bounded by
  `[agent_md] max_bytes`). Later runs get `AGENTS.md` from the context turn
  as usual.
- **Errors** are self-repairing: an unknown or offline runtime lists the
  connected ones; a workspace the runtime refuses carries its reason.
- **`detach_machine`** takes the same `<runtime_id>/<capability_id>`,
  which names one attachment of the session (section 4). It is refused
  while a call on that attachment runs, in the foreground or the
  background, a `transfer` to or from it included, naming the calls.

### 3.2 Attaching during a run

The tools call the existing attach and detach paths with a new
`from_own_run` flag that skips the `session running` refusal for the
calling session only. The attachment is recorded with the usual
`runtime_attached` and `runtime_detached` events, so clients' badges follow
and replay is unchanged.

- **Tools that join** follow tool rounds: on a deferring model they are
  deferred and announced, and `attach_machine`'s result loads them,
  as `tool_search` would; elsewhere they are sent up front. With routed
  tools (section 5) only the first attachment that offers them changes the
  tool list at all; every later one changes only the `machine` values the
  call accepts.
- **The runtimes block** changes at the next run, or is announced on a
  deferring model, as any attachment change is.

### 3.3 Who may attach

- **Any run with an `attach` rule** that allows or asks, root or subagent
  (decided 2026-10-29). A run attaches to its own session only: a
  subagent's attachments are its own, and its parent's are unchanged. The
  tools are `global` tools, so a run has them only while its session has
  `global`.
- **Defaults** (the daemon's, as for `memory_write` and `cron`): root runs
  `attach: allow`; delegated runs `attach: deny`, so a subagent's tools are
  hidden unless its document grants them. An agent document's own `attach`
  key replaces the default, as for every action.
- **`attach@<runtime>`** narrows where, with the existing `action@runtime`
  selector, and the resource is the workspace path, so
  `attach@labpc: { "~/src/*": allow, "*": ask }` works. `detach_machine`
  is checked as `attach` on the attachment's runtime and workspace.
- **`delegate_subagent`'s `capabilities`** stays at the root only. A
  subagent that may attach reaches another machine itself.

### 3.4 Where sessions start

Unchanged (decided 2026-10-29): the web client's New session starts with
`global` alone, the TUI attaches `global` and `local` at its directory, and
a project's defaults apply in both. A TUI session that attaches a second
machine has two attachments, and its routed tools take `machine`.

- **ACP agents need a machine.** An ACP agent runs on the one attached
  runtime that offers it, and `global` does not offer the globally
  configured ones (that duplicate on the daemon's machine is being removed
  separately, 2026-10-29: `local` keeps them). So choosing an ACP agent for
  a global-only (web) session attaches the runtime that offers it, the one on
  the client's machine first, with the usual `attach` check. A session
  already running an ACP agent cannot remove that attachment.

## 4. No overlapping tools on one machine

This is a rule about **a session's attachments**, not about runtimes:
every runtime still connects, and `local` and `client_laptop` both stay
connected on the laptop. A session may use either; it may not attach both.

**An attach is refused when one of its tools has the name of a tool that
another of the session's attachments on the same machine offers**
(decided 2026-10-29). Routing (section 5) picks an attachment by machine,
so each tool name may have only one attachment per machine.

- **Two attachments of one capability on one machine always overlap:**
  `local/coding` at `~/web` and `local/coding` at `~/api`, or `local/coding`
  and `client_laptop/coding`, both offer `bash`. The second is refused.
- **Different capabilities with disjoint tools** may share a machine, such
  as `coding` and a runtime's `browser` capability.
- **Runtime-scoped MCP servers count as their runtime's tools:** a
  `playwright` server on the laptop's runtime and on `yunhao_sff` publish
  one `playwright_browser_click` with a `machine`.
- **The rule compares tool names only** and never branches on a kind or a
  capability name.
- **The error** names the attachment that already offers the tool and says
  to use absolute paths for other directories, or to detach it first.
- **Every attached runtime has a machine** (section 4.1), so the rule and
  routing always have one to go by.
- **A container or WSL runtime grouped with its host** (machines, section
  "Regrouping") is on the host's machine, so its `coding` overlaps the
  host's although their files differ. The error says so and points at
  `cookie machine unassign` (or the Machines view), which gives the runtime
  a machine of its own; the daemon does not guess from differing
  platforms.
- **`session.create`, `session.runtime.attach`, project defaults,
  `capabilities`, and `attach_machine`** all apply it. A delegated child
  gets the rule on its own attachments.
- **A tool that arrives later** (a runtime's MCP server reconnects with new
  tools, `capability/tools_changed`) and overlaps on its machine is left
  out, with a diagnostic naming both attachments.
- **Sessions written before this change** that hold two overlapping
  attachments on one machine detach the later one at their next run, with
  a `runtime_detached` event and a transcript row saying why.

What is lost: a second workspace on one machine no longer brings its own
`AGENTS.md` or `${workspace_dir}` rules. The agent reads the other
directory's `AGENTS.md` like any file.

### 4.1 Every connected runtime has a machine

Today a runtime can be without one in two cases:

- **It has never connected,** such as a template that starts on use: the
  daemon learns its machine ID only when it first connects.
  `list_runtimes` lists it under `<unplaced>`.
- **Placing it failed:** `Machines::resolve` writes a new machine to
  `machines.json` before it answers, and a failed write leaves the
  runtime with none (`runtime/machine_api.rs` logs "has no machine" and
  returns `None`). The machines block then shows a `<machine>` without a
  name.

**Both are fixed:**

- **Placement cannot fail.** `resolve` assigns the machine in memory first
  and answers with it; writing `machines.json` is retried with the next
  registry change, and a failed write is a configuration diagnostic. So a
  connected runtime, and therefore every attachment, always has a machine.
  The nameless `<machine>` of the machines block and of an ACP agent's
  first prompt goes away.
- **A runtime that has never connected** is not on a machine yet, and
  says so instead of looking broken: `list_runtimes` lists it under
  `<not_connected_yet>`, with a comment that its machine is known once it
  first connects, and `attach_machine` (or delegating to it) starts it,
  places it, and names the machine in its result. Nothing can run on it
  before that, so nothing needs its machine sooner.

## 5. Routed tools

### 5.1 One tool, a `machine` argument

**Every runtime tool is routed; nothing is prefixed** (decided
2026-10-29). Composition publishes **one** tool per name across all of the
session's attachments, and adds one host-owned argument, as it adds
`background` today:

```text
bash(command: string, timeout?: integer, machine?: string, background?: boolean)
```

- **`machine`** is a plain string: the machine's name as `list_runtimes`
  and the runtimes block show it. It is not an enum, so attaching or
  removing a machine never changes the tool's schema, its description, or
  the cached prefix.
- **Omitted** means the tool's **default machine**: the first machine,
  in attach order, that offers the tool. For a session the TUI started that
  is `local`'s machine; a tool only one machine offers defaults to it.
  The default is never implicit: the tool's own definition names it
  (below), and so does the machines block (section 5.4).
- **The default is fixed per round,** with the definition that names it.
  Attaching another machine never moves it. Detaching the default machine
  mid-round does not move it either: a call without `machine` then fails
  before approval and lists the machines that offer the tool, rather than
  running somewhere the definition does not say. The next round names a
  new default.
- **Unknown or removed** machines fail the same way.
- **A machine renamed or merged mid-round** keeps answering to the name the
  round's definitions and machines block use: dispatch resolves names
  through the machine IDs the round recorded, and the next round uses the
  new name. The call's display shows the current name.
- **Dispatch** strips `machine`, resolves it to the one attachment on that
  machine that offers the tool (section 4), and prepares the call there. Permissions see the
  attachment's runtime, so `bash@labpc` rules apply unchanged.
- **The examples are not special:** `read`, `write`, `edit`, `bash`, a
  runtime's MCP tools, and any tool a third-party runtime offers route
  the same way.

#### Which tools get `machine`

Nothing declares it; composition decides from two facts it already has.

1. **The tool's source.** A tool gets `machine` when it comes from a
   runtime attachment (its `SourcedProvider` has an attachment other than
   `global`), since those are the tools that run on a machine. The base
   layer and the `global` runtime run inside the daemon and never get it:
   delegation, memory, cron, `list_runtimes`, `attach_machine`,
   `transfer`, top-level MCP servers, and plugins.
2. **The session.** It gets `machine` when the session has `global`
   attached (so it can attach a machine, or will once a rule allows it) or
   has attachments on two or more machines. A session created with
   `global: false` and one workspace, the only other kind, publishes its
   tools as their runtime declares them.

- **Fixed per round.** Both facts are read when a tool round starts, and
  the round's definitions keep the answer until the next compaction.
  Attaching or detaching a machine never adds or removes the argument
  mid-round. The one case that changes it, a second machine attached from a
  client to a session without `global`, adds `machine` to the round's
  definitions once, the way the loader tools may join a round once.
- **The argument's description names the default,** and nothing else
  varies: ``Machine to run on, by its name in the machines block. Default:
  `laptop`. Other machines that offer this tool are listed there.`` With
  no default (the round started before any machine offered the tool, and
  the tool arrived later), it arrives with the default it has then, since
  a tool that joins mid-round brings its definition with it.
- **Why this keeps the cache:** the description changes only when the
  default does, which happens only at a round's start, where the tool
  definitions are written anyway. Attaching a second or third machine
  changes no definition.
- **Reserved,** as `background` is: a runtime tool whose own parameters
  declare `machine` is left out with a diagnostic, as composition already
  does for a tool that declares `background`
  (`runtimes/composition.rs`).
- **Through the ACP bridge,** cookie's routed tools carry the same
  argument.

### 5.2 Why a parameter and not names or paths

- **Prefixed names** (today, removed) cost four tools per machine, rename working
  tools when a second machine arrives, and make the model choose among
  `local_web_bash`-style names it has to decode.
- **Path prefixes** (`read("labpc:/srv/x")`, as `scp` writes them) fit the
  file tools but not `bash`, and they put routing inside a value
  permissions match as a path.
- **A parameter** keeps the tool list fixed after the first machine, reads
  the same as the user's own words ("run it on labpc"), and keeps paths
  paths.

### 5.3 Names that still meet

With prefixes gone, `ToolCollision` keeps one meaning besides routing:

- **`fallback`** stays: `global`'s `webfetch` is hidden whenever an
  attachment offers one, as today.
- **`prefix` and `override`** go. A runtime that sends them (an older
  `cookie runtime serve`) is read as routing; the runtime protocol's open
  enums already let the daemon do that.
- **A runtime tool with a base-layer or `global` name** (`delegate_subagent`,
  `memory_read`, a top-level MCP server's tool) is left out with a
  diagnostic, where today it is prefixed. Base names were always reserved
  for the base layer.
- **Two plugins** declaring one name: the second is left out with a
  diagnostic.
- **One name, different schemas on two machines** (two versions of
  `cookie runtime serve`): the tool is published with the round's
  definition, the first attachment's at the round's start, and the target
  runtime checks the arguments against its own schema as it does today. An
  argument it does not know is a tool error naming the runtime and its
  version, so the model can retry without it.
- **Pins** (stable prefix spec, section 5) simplify: a published name is
  always the tool's own name, so there is nothing to pin. Rounds still keep
  definitions until compaction.
- **Old sessions** replay their prefixed calls as recorded; their next
  round publishes bare names.

### 5.4 Telling the model which machine offers what

The tools themselves say nothing about machines: their descriptions are
constant, and the per-tool suffix ``Belongs to attachment `…` `` goes away
(it is what made descriptions change with every attachment). Instead, one
place maps machines to tools, and everything else points at it.

**1. The machines block** (the runtimes prompt block) says plainly which
machines the session is attached to, what each offers, and which is each
tool's default:

```text
This session is attached to the following machines. Tools that take a
`machine` argument run on the machine you name in it; when you leave it
out, they run on the tool's default machine, which each tool's definition
names and which is marked default below. Relative paths resolve against
the machine's directory, and a path on one machine means nothing on
another: move files between machines with transfer. To see every machine
and runtime you could attach, call list_runtimes; attach one with
attach_machine.
<machines project="web">
<machine name="laptop" platform="linux" home="/home/me" directory="~/web" default="true">
coding: read, write, edit, bash
playwright: playwright_* (21 tools)
</machine>
<machine name="labpc" platform="linux" home="/home/lab" directory="~/api">
coding: read, write, edit, bash
</machine>
</machines>
```

- **`default="true"`** marks the machine that is the default for the tools
  it offers: the first attached. A tool whose default is another machine
  (only a later machine offers it) is listed under that machine with
  ` (default)` after its group, `browser (default): …`.
- **The sentences name only the tools the run is offered:** without
  `attach_machine` the last sentence ends at list_runtimes, and without
  `list_runtimes` it is left out.
- **Groups are the deferred-tools groups** (deferred tools in cookie,
  section 7.1): the provider's group for a runtime's own tools, the server
  name for a runtime-scoped MCP server. A group of at most 8 tools lists
  them; a bigger one is written `<prefix>* (N tools)` when its published
  names share a prefix, as an MCP server's do, and is listed otherwise.
- **Order:** machines in the order the session first attached them, groups
  in each machine by name. With nothing but `global`, the block is the
  "no machine attached" text of section 2.4.
- **Every model gets it,** not only sessions with several attachments,
  since the `machine` argument is there from the first attachment
  (section 5.1).

**2. Changes are announced where the stable prefix announces them.** The
block is a prompt block, frozen per round on deferring models with
`<instructions_changed>` announcements, and composed at each run
elsewhere (stable prefix spec, section 6). So:

- A machine attached or detached by a client between runs: the next run's
  block, or its announcement.
- **A machine attached by the model** (`attach_machine`) mid-run: its result
  carries that machine's `<machine>` element as the block will show it, so
  the model knows at once on every model. `detach_machine`'s result names
  what left.
- **A routed tool that joins or leaves** follows tool rounds as any tool
  does; a `<tools_added>` or `<tools_removed>` group of routed tools also
  names its machines, `<group name="playwright" source="mcp"
  machines="laptop">`, so a tool already published from another machine
  says where it is now also offered.

**3. Calls repair themselves.** A call naming a machine that does not offer
the tool fails before approval with where it is offered:
``playwright_browser_click is not on labpc; it is on: laptop.`` A call
without `machine`, or naming no attached machine, lists the attached
machines that offer the tool. These are tool errors, so the model retries
without the user.

**4. `list_runtimes`** shows each runtime's groups the same way, so the
model can see what attaching a machine would bring before it attaches.

**5. The clients** show the machines block as they show the system prompt
today, its announcements as model-input rows, and each routed call's
machine as a badge in its tool header.

## 6. Transfers

### 6.1 The tool

One `global` tool in the `machines` group:

```text
transfer(from: {machine, path} | {artifact, stream?}, to: {machine, path}, overwrite?: boolean, background?: boolean)
```

- **Machine to machine:** a file, or a folder copied recursively. The
  daemon relays the bytes, so the machines need not reach each other. A
  copy within one machine is a local copy on its runtime.
- **Artifact to machine:** a user's upload, retained tool output, or a
  stream of it, saved as a file.
- **Never into the artifact store** (decided 2026-10-29): the
  destination is always a machine. The model has no way to write an
  artifact.
- **Nothing is stored on the way.** The daemon relays chunks from one
  runtime to the other and keeps none of them: no artifact, no temporary
  file in the session tree, no output capture. The tool's output and
  display are only the summary below, so a 10 GiB copy adds a few hundred
  bytes to the session log.
- **The result** gives bytes and files moved, each file's SHA-256 for a
  single file, and the destination's path.
- **`overwrite`** is `false` by default: an existing destination is an
  error. With `true` a file is replaced atomically and a folder is merged.
- **Long transfers** use the host-owned `background` argument like `bash`,
  and report progress through `tool/progress`.

### 6.2 Permissions

No new action. The source is checked as `read@<source runtime>` on its
path and the destination as `write@<destination runtime>` on its path, both
before the call runs; a call that needs approval asks once, naming both.
An artifact source needs no rule (it is the tree's own). So the rules the
user has already written for `read` and `write` decide what may be copied
where.

### 6.3 How a machine-to-machine transfer runs

The daemon is the only party that reaches both runtimes, so it relays. It
stores nothing: bytes pass through its memory a chunk at a time, never its
disk or the session tree.

**Runtime protocol 1.13.0 (minor).** A runtime that can take part says
`files: true` in its `runtime/initialize` result. Six requests, all daemon
→ runtime, on an instance of the session's attachment:

| Method | Params | Result |
|---|---|---|
| `fs/read_open` | `instance`, the prepared route | `handle`, `kind` (`file` or `folder`), `size` for a file |
| `fs/read_chunk` | `handle`, `max_bytes` (at most 1 MiB) | `data` (base64), `eof`; at `eof` a file's `sha256`, or a folder's file count and bytes |
| `fs/write_open` | `instance`, the prepared route, `kind`, `overwrite`, `size` when known | `handle` |
| `fs/write_chunk` | `handle`, `offset`, `data` | `{}` |
| `fs/write_commit` | `handle`, the source's `sha256` for a file | `path`, `bytes`, `files` |
| `fs/close` | `handle` | `{}` |

**Paths follow the file tools' permission contract**
(`docs/site/extend/agents.md`, path resources): the label a rule matches is
the path as written, normalized lexically against the workspace, and never
resolved through symbolic links. A seventh request, `fs/prepare
{ instance, path, direction }`, does what `tool/prepare` does for `write`:
it normalizes the label and resolves the route once, returning a route
handle, the `kind`, and the `size`. Opening uses that route, so a link
retargeted after approval does not redirect the transfer, and no second
check is made against where a link leads, as for `read` and `write`.

**The daemon's loop:**

1. `fs/prepare` both ends, and check permissions (section 6.2) on the two
   labels, and both runtimes' `files`.
2. `fs/read_open` on the source's route and `fs/write_open` on the
   destination's. A destination that exists without `overwrite` fails
   here, before a byte moves.
3. Read a chunk from the source, write it to the destination, and repeat.
   Up to 4 chunks are in flight, so a slow link is not idle while the
   other answers; the daemon holds at most those 4 MiB per transfer.
4. At the source's `eof`, `fs/write_commit` with the source's digest. The
   destination has hashed what it wrote; a mismatch fails the commit and
   removes the partial copy.
5. `fs/close` both handles. The result is the summary of section 6.1.

**On the source,** a file is read as it is. A folder is packed on the fly
as a tar stream: regular files, folders, and symlinks as links, with mode
bits and modification times, no owners; devices, sockets, and FIFOs are
skipped and counted in the result. Nothing is staged on the source's disk.

**On the destination,** a file is written to a hidden sibling
(`.<name>.cookie-partial-<id>`) and renamed into place at commit, so a
reader never sees half a file. A folder is unpacked into a hidden sibling
folder and renamed at commit; with `overwrite` it is merged into the
existing folder at commit instead. Unpacking refuses absolute names, `..`,
and links that point outside the destination folder.


**Within one machine** (`from` and `to` name the same machine), nothing
crosses the daemon: an eighth request, `fs/copy { instance, from, to,
overwrite }`, copies locally on that runtime with the same partial-and-
rename rule.

**Failure and cancellation:**

- Stopping the call (`stop_background`, or the run's cancel) or any error
  `fs/close`s both handles; an uncommitted destination is removed.
- A runtime whose connection drops drops its handles with it, and removes
  their partial files. At start, `cookie runtime serve` removes
  `.cookie-partial-*` files older than a day that a crash left behind.
- The result says how far it got: bytes and files moved before the failure.

**Progress** goes through `tool/progress` as display only (`12.3 MiB of
80 MiB`), never as output, so the session log keeps the call's arguments
and the final summary and nothing of the content.

**The local runtime** (inside the daemon) implements the same operations
directly, without the JSON-RPC hop.

**Limits:**

- `[transfer] max_bytes`, an integer, where 0 or less means no limit;
  checked against a file's `size` at open and against the running count
  for folders.
- The 48 MiB frame limit is far above a 1 MiB chunk (about 1.4 MiB as
  base64).
- Throughput is bounded by the slower of the two links to the daemon, plus
  base64's third. Machines that can reach each other directly can still
  use `scp` or `rsync` through `bash`.

**An artifact source** is read from the session tree's store in the same
chunks and written with `fs/write_*`; it is the only direction the store
takes part in.

A runtime without `files` fails the call naming the runtime and its
version.

### 6.4 Files the model can name

A user's upload reaches the model as file bytes only, with no reference the
model can pass on. Each user file part gains a model-text label beside it,
shown in the clients as model input (it is something the engine
injects), so the model can name it as `transfer`'s `from`:

```text
[file report.zip, 3.2 MB, artifact 5f2c81d0]
```

The label shows the digest's first 8 hex digits; `transfer` takes that, a
longer prefix, or the full digest, resolved within the session tree's store
as handles are within a tree. A prefix two artifacts share fails with both
listed.

Uploads that no model takes inline (archives, data files, text over 1 MiB)
become accepted as `kind: file`: stored, labelled, never sent as bytes
(client protocol minor, decided 2026-10-29). The 25 MiB upload cap stays.
That makes "save this to labpc" from the phone a single `transfer`.

**The label is also the fallback for media the model cannot take** (decided
2026-10-29). Today a file part the attempt's model cannot accept becomes a
placeholder such as `[image omitted: acme/text-model does not accept
images (screenshot.png, 471 KB)]`, which names no artifact. The placeholder
becomes the label with its reason, `[file screenshot.png, 471 KB, artifact
5f2c81d0; not shown: acme/text-model does not accept images]`, so the
model can still pass the file on. Admission no longer refuses a file part
the model cannot take; it sends the label. The placeholder depends only on
the history and the model, as today, so the prompt cache is unaffected.

These are the user's uploads, not the model's: only a client writes the
store.

## 7. Protocol and compatibility

- **Runtime protocol minor:** `files` and the `fs/*` methods; `prefix` and
  `override` read as routing. A runtime without `files` has no
  transfers.
- **Client protocol minor:** upload `kind: file`, the file label as
  model text, `utility_model` in onboarding and `AgentInfo`, and an
  agent's origin (`user` or `official`, with the official version) in the
  agent listings. Dropping the `default` agent changes no wire type: it
  was an ordinary agent ID on the wire.
- **Tool schemas** are model-facing and need no version; the reference is
  regenerated.
- **History:** routed calls record the published name and the
  arguments as sent, `machine` included. Old logs replay with their
  prefixed names.

## 8. Phases

1. **Official agents and model tiers:** the `agents` repository and its
   CI, `<data_root>/official_agents/` with refresh and precedence,
   `[agents] official`, the `cookie agents` commands, removing the
   synthesized `default` and the compiled-in internal prompts,
   `utility_model` and the new `small_model`, onboarding, the kit reduced to
   `config.toml`, and the user's own `config.toml` renamed in step.
2. **Prompts:** `primary`, `coder`, `explore`, `worker`, `approval`,
   `title` as published in the repository, the global-only runtimes block,
   the description sweep.
3. **Attachments from tools:** `attach_machine` and `detach_machine`,
   attaching during a run, the `attach` action and its defaults, section 4's
   rule, placement that cannot fail (section 4.1), ACP agents attaching
   their runtime.
4. **Routed tools:** composition without prefixes, the `machine`
   argument, dispatch, the overlap diagnostics, the prompt block, client
   badges.
5. **Transfers:** the runtime protocol methods, `transfer` relaying without
   storing, file labels, and `kind: file` uploads.
6. **Docs:** concepts (machines, runtimes), the tools reference, the
   runtime protocol reference, the web and TUI guides.

Each phase is usable on its own; phase 3 without phase 4 still works,
with prefixed names until phase 4 lands.

## 9. Alternatives considered

- **An `attachment` argument instead of `machine`.** Attachment names
  (`labpc_api`) are cookie's bookkeeping, not the user's vocabulary, and
  after section 4 a machine picks one attachment per tool anyway.
- **Keeping several workspaces per machine** and routing by attachment.
  It keeps per-directory `AGENTS.md`, at the cost of the model holding two
  names for one computer. Rejected by section 4.
- **Transfers through `bash`** (`scp`, `rsync`). It needs a network path
  between the machines and credentials on them. It stays possible.
- **Staging transfers in the artifact store,** which would give the user a
  copy to download. Rejected by the user on 2026-10-29: it would grow the
  session tree by every byte moved.
- **Holding official agents' permission changes for the user's
  acceptance.** Rejected by the user on 2026-10-29: refreshes apply as
  skills' do, and `agents/` is the way to pin an agent.
- **A compiled-in seed of all the official agents,** used until the first
  refresh. Only compaction needs one, since the others fail harmlessly, so
  only compaction keeps one.

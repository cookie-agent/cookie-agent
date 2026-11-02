# ACP agents across several machines

Status: **implemented** (2026-11-01; client protocol 1.70.0, plugin
protocol 1.37.0). The user made decisions D1 to D6 on 2026-11-01
(section 8). It builds on [ACP agent permissions](acp-permissions.md).
Where the code departs from the text is in the
[implementation notes](#implementation-notes).

## 1. Problem

An ACP agent (Claude Code, Codex) runs as one process on one runtime, and
its harness's own file and shell tools act on that runtime's machine. Cookie
gives it cookie's tools over the `cookie` MCP bridge. Three things make a
second machine unusable today:

1. **A second runtime that offers the agent cannot be attached.**
   `refuse_second_acp_runtime` (`runtime/machines/attach.rs`, called from
   `attach_runtime` and `attach_from_run`) and `AcpOffers::place`
   (`runtime_snapshot/agents.rs`) refuse a session where two attached
   runtimes offer the agent, and the clients mirror it
   (`AcpPlacement::Ambiguous`, `client_state/src/acp.rs`). Since shared ACP
   agents (`[acp.agents.<id>]` on every runtime) put `claude` on every
   runtime, this refuses nearly every second machine.
2. **`hide_tools` hides tools on every machine.** Its default,
   `[read, write, edit, bash, webfetch]`, is there because the harness has
   its own on its machine. But it is matched by tool name, and with routed
   tools (machines on demand, section 5) one `bash` serves every machine.
   So an agent that attaches `labpc` gets no way to read, write, or run
   anything there.
3. **The agent is told about tools that no longer exist.** Its first
   prompt still gets the pre-routing block (`runtimes_prompt` with the
   `SEVERAL_ATTACHMENTS` text: "a tool several attachments offer is named
   `<attachment>_<tool>`", "each tool's description names the attachment
   it belongs to"). The bridge publishes routed tools with a `machine`
   argument and no prefixes or notes. The comment in
   `runtimes_block_with` ("an ACP agent's bridge offers cookie's tools
   under their attachments' names") is stale.

This spec lets one ACP session attach any number of machines, including
others that offer the same agent. The agent's process stays on one of them
(**the host**). Cookie's routed tools reach every attached machine, the
host's included. The agent is told to prefer its own tools on the host
and to use cookie's, naming the machine, everywhere else.

## 2. Terms

- **Host runtime:** the attached runtime that runs the agent's process for
  the session.
- **Host machine:** the machine of the host runtime. Attachments of other
  runtimes on the same machine share its files (machines on demand,
  section 4.1), so "where the built-in tools reach" is a machine, not a
  runtime.

## 3. Several runtimes that offer the agent

### 3.1 Which runtime hosts the agent

1. **Once the session has an ACP session:** the runtime of its latest
   `acp_session_started` record (`AcpSessionRecord.runtime_id`), while
   that runtime is attached. This is already recorded, so nothing new is
   stored. If that runtime no longer offers the agent, or has it disabled
   or is away, the turn is refused (section 3.2); placement never falls
   through to rule 2 while the recorded runtime is attached.
2. **Before that, the runtime the user chose** (section 3.7): the
   selection's `acp_runtime`. If that runtime is not attached, or is away,
   or does not offer the agent, the turn is refused; it never falls
   through to rule 3.
3. **With no choice made** (a cron job's or a subagent's selection that
   names none): the first attachment, in attach order, whose runtime
   offers the agent and has it enabled and connected.

`AcpOffers::place` takes the session's log and selection as well as its
attached runtime IDs. Its "more than one" refusal goes. The clients'
`placed_runtime` and `AcpPlacement` apply the same rule (the clients
already fold `acp_session_started` and the selection): `Ambiguous` is
removed.

### 3.2 A recorded host that is away or disabled

The session's turn is refused, as today ("ACP agent `claude` is
unavailable: runtime `laptop` is not connected"). The daemon never
moves the agent to another runtime: that would end the agent's
conversation (its sessions stay on the machine that ran them) and change
where its built-in tools act. To run it elsewhere, start a new session
(section 3.3).

### 3.3 The host stays (decided 2026-11-01)

- **Detaching the host runtime is refused** once the agent has started
  there (an `acp_session_started` record names it, which also happens
  when an option is set before the first message) and while the session's
  agent is still that ACP agent: "runtime `laptop` runs `claude` for
  this session; it cannot be detached. Start a new session to run
  `claude` elsewhere." Today the engine does not refuse it, and the next
  turn fails or lands elsewhere; the check goes into `detach_runtime` and
  `detach_from_run`, on the session's actor like the attach checks.
- **No host move of any kind,** neither by detach nor by an action of its
  own. An agent stays on the runtime it started on for the session's
  life. Changing the session's agent to another one lifts the refusal for
  the runtime it no longer needs.
- The runner's existing notice for a record on another runtime ("this
  turn starts a new session on runtime `labpc`, which does not remember
  the conversation so far") stays for the one case left: a session that
  switched agents and back after the first host was detached.
- **Before the agent starts, the choice is free.** The runtime the
  selection names can be changed (section 3.7) or detached; detaching it
  clears the selection's `acp_runtime`, and the clients show the new
  default. There is no conversation to lose yet.

### 3.4 The agent's working directory and extra directories

- **`cwd`** stays `acp_cwd`: the first workspace attachment on the host
  runtime, else that runtime's home.
- **`additionalDirectories`** become the workspaces of every attachment
  on the **host machine** (today: the host runtime only). A second runtime
  on the same machine sees the same files.
- Workspaces on other machines are never passed to the agent. They are
  reached through cookie's tools.

### 3.5 One name per machine

ACP sessions are exempt from machines-on-demand section 4 (no two
attachments on one machine that offer a tool of the same name), so that
extra directories can be attachments of one capability. The bridge now
routes every runtime tool by `machine`, on the host machine too
(section 4), so the exemption needs a rule for routing:

- **On the host machine,** a routed tool goes to the attachment that
  serves the agent's `cwd` (section 3.4), else the first on that machine
  that offers it. The other attachments there only add
  `additionalDirectories`; their copies of the tool are not routed.
  Cookie's tools reach those directories by absolute path, as the
  section 4 refusal already advises.
- **Every other machine** follows section 4 as a model session does.

### 3.6 Subagents

An ACP agent delegated to with `capabilities` is placed by the same rule
over the child's attachments. The agent's own subagent sessions
(`how: subagent`) run in the parent's process and keep the parent's host.

### 3.7 Choosing where the agent runs

Attach order is not a way to pick where the agent runs. When the user
selects an ACP agent that more than one runtime offers, the clients let
them say where it runs, and the choice is stored with the selection.

- **In the selection.** `RunSelection` gains an optional `acp_runtime`:
  the runtime that runs the selected ACP agent. It is set wherever an
  agent is chosen: a new session, the agent picker of a session that
  changes to an ACP agent, a cron job's template. It is absent for a
  native model and when only one runtime offers the agent. Selecting
  another agent drops it.
- **The default** the clients preselect: the runtime on the client's own
  machine, else the first attached runtime that offers the agent, else
  the first that offers it at all, in the order runtimes are listed.
- **Choosing a runtime that is not attached** attaches it, with the usual
  attach checks, at the project's default workspace for that runtime,
  else its home. This is what a global-only web session already does when
  an ACP agent is chosen (machines on demand, section 3.4); it now
  attaches the chosen runtime instead of the first.
- **After the agent has started** the choice is fixed (section 3.3).
  A `session.select_model` that names another `acp_runtime` for the same
  agent is refused with the section 3.3 reason.
- **Subagents.** `delegate_subagent` takes no new argument. An ACP child
  runs on the first of its `capabilities` that offers the agent, so the
  delegating model chooses by order.

## 4. Cookie's tools on every machine, the host's included

### 4.1 Nothing is hidden for being native

The bridge publishes cookie's runtime tools to an ACP agent as it does to a
model: one routed tool per name, with `machine`, every attached machine
included. On the host machine that duplicates the harness's own tools,
and a call there is allowed. It runs like any bridged call: cookie's
rules decide it and it shows as a cookie tool call of the agent's turn.
The agent is told to prefer its own tools there (section 5), but nothing
refuses it.

This follows from D5: once the tools are always in the agent's list, so
that Codex has them before a second machine arrives, hiding or refusing
them on the host buys nothing. So:

- **`acp.native_tools` is not added** (it was D1's split; section 8).
- **`acp.hide_tools`** keeps its meaning, the tools the agent does not get
  anywhere, and its default becomes `[webfetch]` (D4). A document that sets
  it explicitly is unchanged. Configuration stays strict: the default
  changes and nothing else, documented in the configuration reference.
  As before, no test pins the default.

### 4.2 What the bridge publishes

- **Always routed.** For an ACP session, `routing_machines` routes every
  runtime tool, even with no `global` and one machine. Otherwise a session
  started on the host alone publishes `bash` without `machine`, and a
  second machine attached later changes its schema, which Codex never
  fetches again. With `machine` there from the start, a definition fetched
  once stays true.
- **The default machine** is a model's (machines on demand, section 5.1):
  the first machine, in attach order, that offers the tool, usually the
  host. Leaving `machine` out runs on the host, which is allowed. The
  default changes only when its machine is detached, never when one is
  attached. A definition Codex fetched earlier stays true until then, and
  after it a call without `machine` fails with the machines that offer
  the tool, as for a model.
- **Routing errors** are a model's (machines on demand, section 5.4,
  item 3). There is no ACP-specific refusal.

### 4.3 Permissions

[ACP agent permissions](acp-permissions.md) applies (D6):

- a call of cookie's tools that reaches the bridge is refused if cookie's
  rules point to `deny`, and otherwise runs;
- a harness permission request is rejected on `deny`, allowed on
  `allow`, and otherwise shown to the user with the harness's options;
- the agent's mode is the session's permission mode.

For several machines, what matters is scope. A bridged call's rules, and
those of a harness request for a `cookie` tool, are the rules of the
attachment `machine` routes the call to: `bash@labpc`, with
`${workspace_dir}` as labpc's workspace. A request for the harness's own
tools is matched in the host runtime's scope, with the agent's `cwd` as
the workspace. Section 3.5 routes the host machine's calls to the `cwd`
attachment, so one `deny` on the host covers both paths there. Each attached
machine widens what an undenied call can touch, so the ACP agents guide's
suggested `deny` rules are per machine where it matters, with
`action@runtime` (`bash@prod: deny`).

## 5. Telling the agent

The agent's context gets the **routed machines block**, the one a model run
gets (`routed_runtimes_prompt`, machines on demand, section 5.4). It no
longer gets the pre-routing `runtimes_prompt`. Two changes for an ACP
agent; the listing and the default markers are a model's:

1. **The host machine is marked** `builtin_tools="true"`.
2. **A paragraph after the block** says which tools to use where.

With `laptop` hosting Claude Code and `labpc` attached:

```text
This session is attached to the following machines. Tools that take a
`machine` argument run on the machine you name in it; ... (the routed
block's usual text)
<machines project="web">
<machine name="laptop" platform="linux" home="/home/me" directory="~/web" default="true" builtin_tools="true">
coding: read, write, edit, bash
playwright: playwright_* (21 tools)
</machine>
<machine name="labpc" platform="linux" home="/home/lab" directory="~/api">
coding: read, write, edit, bash
</machine>
</machines>
Your harness's built-in tools (its file, edit, and shell tools) run on
machine `laptop`, marked `builtin_tools` above, in working directory
/home/me/web, and reach no other machine. Prefer them for work on
`laptop`. To work on another machine, use the tools of the `cookie` MCP
server and name the machine in their `machine` argument, for example
`bash` with `machine: "labpc"`; without `machine` they run on their
default machine. A path on one machine means nothing on another: copy
files between machines with `transfer`.
```

- **The paragraph names no cookie tools as duplicates.** It says only
  that the harness's own tools reach the host machine. The `bash` example
  uses a routed tool the bridge offers (`bash`, else the first routed
  tool), and the `transfer` clause appears only when `transfer` is
  offered.
- **Only the host machine attached** (with or without `global`): the
  block is a model run's, the host is marked, and the paragraph shrinks to
  its first sentence. When `attach_machine` is offered, it adds: "A machine
  you attach is reached through the `cookie` MCP server's tools, with its
  `machine` argument." This replaces the 2026-10-25 decision that one
  attachment gets no sentence: the agent can now attach machines from a
  run, so it needs to know before it does.
- **`attach_machine`'s result** already carries the new machine's
  `<machine>` element and its tools (`AttachedMachine.tools`, after
  `hide_tools`). For an ACP agent on a machine other than the
  host, it adds the same "use cookie's tools with `machine: "<name>"`"
  line.
- **Machines attached or detached between turns** by a client or a cron
  target are announced ahead of the next prompt with a
  `<machines_changed>` note (D3, section 8). The routed tools, with
  `machine`, are already in every agent's list (section 4.2), so the note
  works for Codex too, which ignores `tools/list_changed`.
- **Transparency:** the block and paragraph are part of the context the
  system prompt row shows (`appended system prompt` / `system prompt
  (sent as user message)`). An announcement under D3 is a model-input row,
  like a model's `<instructions_changed>`.

## 6. Clients

- **Choosing where it runs (web).** In the composer, when the selected
  agent is an ACP agent that more than one runtime offers, a **Runs on**
  chip appears next to the agent chip (`claude · on labpc`). It opens a
  list of the runtimes that offer the agent, by machine and runtime name:
  the current one checked, and the ones that are away or have the agent
  disabled greyed, with the reason. Picking one sets the draft's
  `acp_runtime` and attaches it if needed (section 3.7). On a phone the
  list is a sheet, like the agent picker's.
- **Choosing where it runs (TUI).** The agent picker gets a step after the
  agent, like `/model`'s variant step: for an ACP agent several runtimes
  offer, a **Runs on** list with the same rows, the default first. The
  status line names the place with the agent (`claude@labpc`), and a click
  there reopens the step. Each row and the status-line target are pointer
  targets with a visible hover (`PointerTarget`, `apply_hover`).
- **After the agent starts** the chip and the status-line target show the
  host and no longer open the list; their hover says why ("claude runs on
  labpc for this session; start a new session to run it elsewhere").
- **Where the agent runs, elsewhere.** The machines panel (web) and the
  attachments list (TUI) mark the host attachment with a "runs claude"
  chip.
- **Attach.** Attaching another runtime that offers the agent is no longer
  refused. The Machines panel's ACP top card (shared ACP agents) keeps
  saying where the agent is offered.
- **Detach.** The host attachment's Detach is disabled, with the
  daemon's reason as its hover text (section 3.3); the daemon refuses it
  regardless.

## 7. Protocol, history, compatibility

- **Client protocol:** a minor bump. `RunSelection.acp_runtime` is
  optional (section 3.7). D3 adds one optional field for the
  `<machines_changed>` row. A session's host after its agent has started
  is still derived from `acp_session_started`.
- **Runtime and plugin protocols:** unchanged.
- **Session history:** no format bump. `acp_runtime` is optional in the
  selections the log records (`session_created`'s `creation_selection`,
  selection changes) and left out when absent, so old logs and the golden
  fixtures read unchanged. Old sessions' records name their host.
- **Configuration:** only the `hide_tools` default changes, to
  `[webfetch]` (section 4.1). A document that set `hide_tools` explicitly
  keeps its meaning: hidden everywhere.

## 8. Decisions for the user

**Decided by the user, 2026-11-01:**

- **D1, revised the same day: no `native_tools`.** The user first chose
  a `native_tools` key beside `hide_tools`, to hide the harness's
  duplicates on the host machine only. Once D5 put those tools in every
  agent's list, the user pointed out that refusing them on the host buys
  nothing, so they are allowed there and the key is not added
  (section 4.1).
- **D2. The host never moves** (section 3.3). Detaching the host runtime
  is refused while the session runs that ACP agent; to run the agent
  elsewhere, start a new session. Rejected: moving it by detach into a
  fresh ACP session, and seeding a new session with the cookie
  transcript. Before the agent starts, the user chooses where it runs
  (section 3.7).
- **D3. Machine changes are announced to a live ACP session.** Ahead of
  the next prompt after a client or a cron target attaches or detaches a
  machine, the agent gets a short `<machines_changed>` note: the new
  `<machine>` element with the section 5 line, or the name that left. It
  shows as a model-input row. This narrows the 2026-10-31 decision that a
  live ACP session is told nothing mid-session; skills, tool
  instructions, and memory stay as decided then.
- **D4. `hide_tools` defaults to `[webfetch]`:** a fetch reads the same
  page from nearly any machine, and the harness has its own.
- **D5. The routed tools are in every agent's list from the start**
  (section 4.2). Whether an agent picks up
  `notifications/tools/list_changed` (Claude Code does, Codex does not)
  is declared nowhere in MCP, so the bridge does not depend on it: cookie's
  runtime tools are published with `machine` from the first machine on,
  the host's included, so a definition fetched once stays true as
  machines come. A tool only a later machine brings (its MCP servers)
  still reaches Codex only in its next new ACP session. Rejected:
  learning per agent version which agents ignore the notification (the
  first session of each version misses the tools), and a document key
  such as `acp.list_changed` (every user must know to set it).
- **D6. Permissions are those of [ACP agent permissions](acp-permissions.md)**,
  a separate spec that ships first. A call of cookie's tools is refused on
  `deny` and otherwise runs. A harness request is decided by `deny` and
  `allow`, and otherwise shown to the user with its options. The agent's mode is the session's permission mode.

## 9. Tests

- Placement: two attached runtimes offer the agent; the selection's
  `acp_runtime` hosts it, and without one the first in attach order does.
  A chosen runtime that is away, detached, or no longer offers the agent
  refuses the turn. A recorded host wins over the selection. A recorded host
  that is away, or no longer offers the agent, refuses the turn and never
  places it on another attached runtime. Detaching the host is refused,
  from a client and from `detach_machine`; detaching another runtime that
  offers the agent is not. Before the agent starts, its chosen runtime can
  be changed or detached, and detaching clears `acp_runtime`; once it has
  started, a selection naming another runtime is refused. After an agent
  change, the old host can be detached.
- Choosing: choosing a runtime that is not attached attaches it at the
  project's default workspace, else its home; a global-only session gets
  the chosen runtime, not the first. A cron template's `acp_runtime`
  places its runs. `acp_runtime` round-trips through `session_created`
  and is absent from logs without it.
- Attach: `attach_runtime` and `attach_machine` accept a second runtime
  that offers the agent. Section 4's overlap rule applies off the host
  machine and not on it.
- Bridge: an ACP session with only the host and no `global` publishes
  `bash` with `machine`, defaulting to the host, and a call there runs.
  Attaching `labpc` changes no definition; `machine: "labpc"` runs there,
  decided as `bash@labpc`. Detaching the default machine makes a call
  without `machine` fail with the machines that offer it. `hide_tools`
  hides on every machine. Two coding attachments on the host machine
  route to the one serving the agent's `cwd`.
- Context: the exact block and paragraph for host plus one machine, and
  for host only with and without `attach_machine`. No
  `<attachment>_<tool>` text reaches an ACP agent.
  `attach_machine`'s result carries the line off the host.
- `additionalDirectories`: workspaces of two runtimes on the host machine,
  and none from another machine.
- Clients (`client_state`): placement without `Ambiguous`, the host
  derived from the latest record, else the selection; the default choice
  (the client's own machine first); the Runs on list's rows and their
  greyed reasons. TUI: the Runs on step's rows and the status-line target
  resolve through `pointer_target_at` and hover visibly (the pointer
  sweep). Web: a scenario in `scripts/web-e2e` that picks labpc for
  `claude` in a new session and sees its first turn start there.
- Permissions: `bash@labpc: deny` refuses a bridged call routed to labpc
  and not one routed to the host.
- Configuration: an explicit `hide_tools` keeping its meaning. No test
  pins the default.

## 10. Phases

1. Placement, attach, and `acp_runtime` (section 3) in the protocol, the
   engine, and `client_state`, with section 3.5's narrowed exemption.
2. The bridge (section 4): routing always on for ACP sessions, routing on
   the host machine (section 3.5), and the `hide_tools` default.
3. The context: the routed block for ACP agents, the marker, the
   paragraph, and `attach_machine`'s line (section 5). This retires the
   pre-routing `runtimes_prompt`'s several-attachments branch and
   `SEVERAL_ATTACHMENTS` if no other caller is left.
4. Clients (section 6: the Runs on chip and step, the host marks) and the
   `<machines_changed>` note (D3).
5. Docs: the ACP agents guide (`extend/acp-agents.md`: "What the first
   prompt carries", `hide_tools`), the configuration reference, and an
   addendum to the ACP agents spec. Machines on demand's implementation
   note on ACP sessions points here.

## 11. Alternatives considered

- **One ACP process per machine,** each agent working locally and cookie
  merging them: several conversations, several contexts, and nothing in
  ACP to tie them together. Delegating to the agent on the other machine
  (`delegate_subagent` with its `capabilities`) already does this
  explicitly.
- **ACP `fs` and `terminal` from cookie,** so the harness's own tools act
  wherever cookie routes them: ACP's file and terminal methods carry no
  machine, so the harness could not choose one. Claude Code also keeps
  using its built-in tools for most work regardless.
- **Prefix the remote tools' names** (`labpc_bash`): machines on demand
  removed prefixes for every model (section 5.2). One routed tool and one
  explanation keeps the ACP agent's tools the same as a model's.

## Implementation notes

What the implementation does differently from the sections above, or adds:

- **Routing on the agent's machine** (section 3.5): the first attachment,
  in attach order, on the agent's machine routes a tool there. That is the
  attachment of the agent's working directory unless another runtime on
  the same machine was attached first; both reach the same files.
- **The default runtime** (section 3.7): the clients preselect the first
  attached runtime that offers the agent and can run it, else the first
  runtime that offers it and can run it at all. They do not prefer the
  client's own machine, which they cannot tell reliably.
- **A run that names no runtime** keeps the session's choice for the same
  agent, so a client older than client protocol 1.70.0 does not reset it.
- **Cron templates** gain `acp_runtime` too, for a job's runs.
- **A chosen runtime not attached** is attached at its home, both when a
  session is created and on `session.select_model`; a project's default
  workspace for it is already attached by the project's defaults.
- **No separate Runs on control** (revised 2026-11-01 at the user's
  request): the agent picker lists an ACP agent several runtimes offer once
  per runtime (the web's agent sheet as options under the agent, the TUI's
  picker as a row each), and the agent chip and the TUI's composer title
  read `claude · on local` for any ACP agent. Once the agent has started,
  the other runtimes' options are dimmed (web) or refused with the reason
  (TUI). The permission mode shows the agent's mode by its plain name.
- **`<machines_changed>`** (D3) carries the whole machines block as it
  stands, with the paragraph, rather than only the new machine's element or
  the name that left. It is recorded as one of the agent's session notices
  (a diagnostic the transcript shows), so it needs no protocol field.
- **`attach_machine`'s result** for a machine other than the agent's adds
  a line saying the agent's own tools do not reach it.
- **The pre-routing runtimes block** no longer reaches ACP agents; its
  `builtin_tools` marker and sentence for attachments are gone, and the
  routed block's `<machine>` carries the marker instead.

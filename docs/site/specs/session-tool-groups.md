# Session tool groups and skills

Status: **approved** (2026-11-03), with the user's decisions in
[section 3](#3-decisions); phases 1 to 3 implemented (client protocol
1.83.0, plugin protocol 1.43.0, runtime protocol 1.16.0), see
[implementation notes](#17-implementation-notes). Builds on
[permission consolidation](permission-consolidation.md) (session overlays),
[MCP tool filters](mcp-tool-filters.md),
[deferred tools in cookie](deferred-tools-cookie.md) (tool groups and
announcements), [frozen context](frozen-context.md) (refresh points),
[machines on demand](machines-on-demand.md) (routed tools), and
[projects](projects.md).

## 1. Goal and scope

Let each session choose:

1. **which tool groups it is offered**: built-in groups such as `coding`,
   `memory`, or `cron`, each MCP server, each plugin, and a runtime's own
   groups, separately for each machine (`coding` off on `labpc`, on on
   `local`), and whether each is sent up front or deferred;
2. **which skills it loads**: listed in `<available_skills>` and callable
   through `skill`.

The aim is a smaller, focused tool list and skill listing per session.
That means less context, fewer wrong tool picks, and no 40-tool MCP server
in a session that never uses it. The same choices can be made once in an
agent document, or as a project's default for its new sessions.

**Out of scope:** the harness's own tools and skills of an ACP agent
(Claude Code's `Bash`, `~/.claude/skills`); selecting single tools inside a
group (an MCP server's `include_tools`/`exclude_tools` and permissions
cover that); deciding what a call may do (permissions stay the only call
gate for tools).

## 2. Today

- **Tool visibility comes from permissions.** A tool is offered when the
  merged rules (defaults, agent document, session overlay) allow or ask its
  action (`MergedRules::offers`; `compose_for_policy_with` in
  `crates/engine/src/runtime/tool_execution.rs`). MCP tools are checked by
  name (`tool_offered_by`). Writing groups as permission rules was
  considered and rejected (D1): a more specific agent rule beats a session
  deny, `exa_*` also catches a server named `exa_pro`, one machine's
  built-in tools cannot be hidden while a plain rule allows them, and the
  edits replace the user's own session rules and forget remembered
  approvals for the whole action.
- **Groups already exist** for deferred tools (`deferred_tools::group_of`):
  a built-in provider's `ToolGroup` (`agents`, `artifacts`, `assets`,
  `background`, `coding`, `cron`, `delegation`, `goal`, `machines`,
  `memory`, `messaging`, `runtimes`, `skills`), the MCP server, or the
  plugin. A remote runtime's own tools have no group and fall back to the
  attachment's name, so a dial-in coding runtime's `read`/`bash` are
  grouped as `labpc`, not `coding`. `[deferred_tools] always_load` and the
  MCP and plugin `always_load` keys choose eager or deferred, globally.
- **Other filters** are global or per agent: an MCP server's filters apply
  to every session, and an ACP agent document's `acp.hide_tools` (default
  `[webfetch]`) to every session of the agent.
- **Skills** come from the attached runtimes. The `skill` permission hides
  a denied skill from the listing and refuses it, gates loading with
  `ask`, and is the action `Skill(...)` entries in `allowed-tools` grant.
  No agent document declares a `skill` rule; every agent runs on the
  default `skill: allow`. A user invocation (`prepare_user_skill_invocation`)
  checks only `user-invocable`, so the user can already run a denied skill.
- **Eager tools that leave mid-round** stay declared until the next refresh
  point, without a notice; a call is refused with "`<tool>` is no longer
  available in this session" (`crates/engine/src/runtime/model_loop.rs`).
  They leave when an exposure change turns them off (new), and today when
  a runtime sends a new tool list (`capability/tools_changed`,
  `runtime/tools_changed`), including the list that drops a `failed` MCP
  server's tools.
- **MCP servers** connect eagerly, in every runtime, whether or not any
  session uses them.

## 3. Decisions

The user decided these on 2026-11-02 and 2026-11-03.

- **D1, a separate layer.** A **tool exposure** layer next to the
  permission overlay. It only takes away; it never grants a call.
- **D2, three states:** `on`, `deferred`, `off`.
- **D3, machines by runtime ID:** `coding@labpc`, as permission selectors
  write it. Two attachments of one runtime in a session are not told apart.
- **D4, layers:** defaults < agent document < session overlay, and a
  **project default** copied into a new session's overlay, so it beats the
  agent document.
- **D5, no inheritance:** a subagent starts from its own agent document.
- **D6, mid-session changes keep the cache.** No refresh is forced. Every
  eager tool that leaves mid-round, whatever the reason, is announced once
  in a `<system-reminder>` (section 8).
- **D7, the user can still run an off skill** from `/skills`.
- **D8, fixed groups:** `artifacts`, `background`, `tool_search`, and the
  `skills` group (the `skill` tool) cannot be switched.
- **D9, runtime-declared groups:** groups are declared once per tool list,
  and each tool names its group; a group may declare a `default` of `on`
  or `deferred`.
- **D10, `acp.hide_tools` is removed:** a document that declares it fails.
  `webfetch` moves to a group of its own, `web`, and ACP agents default to
  `web: off`.
- **D11, MCP servers connect on demand:** when a client opens a session
  that uses them, and stop `[mcp] idle_stop_after` (default `10m`) after
  the last such session.
- **Skills leave permissions.** The `skills` key of the layer is the only
  place that decides which skills an agent and a session get.
  - **S1:** a skill that is on loads without asking, as with today's
    default `skill: allow`; a skill whose grants you distrust is turned off.
  - **S2:** `Skill(...)` in `allowed-tools` gets no special handling: it
    parses as an action nothing uses, so it grants nothing.
  - **S3:** an agent document that declares `permissions.skill` fails, with
    no other handling.
  - **S4:** `skill` rules in old sessions' permission overlays stay in the
    history and govern nothing.
- **D12, layers before specificity:** any rule of a higher layer beats
  every rule of a lower one; specificity only decides within one layer.
  Why: finer control is already available on the permission side, so the
  layer needs no precedence across layers.
- **D13, no rule editor:** the clients change one group or one skill at
  a time; tool groups sit on the runtime panels and cards, skills in a
  tab and section of their own; no composer chip. A session's choices and
  a project's defaults hold exact selectors only; patterns stay in agent
  documents.
- **D14, groups remembered:** the daemon caches the tool groups and
  project skills each runtime offered the last time it ran, so a runtime
  that is not running shows them; only a runtime that never ran shows
  *known once it starts*.

## 4. Groups

### 4.1 What a group is

A group is identified by its kind, its name, and the runtime that offers
it:

| Kind | Name | Runtime | Selector |
|---|---|---|---|
| built-in | the provider's group (`coding`, `memory`, ...) or a runtime-declared group (section 11.1) | the attachment's runtime; none for the base layer and `global` | `coding@labpc`, `memory` |
| MCP server | the server's configured name | the runtime that hosts it; none for top-level servers | `mcp:exa`, `mcp:exa@labpc` |
| plugin | the plugin's name | none | `plugin:browser` |

- **An MCP tool's group is its server,** from `tool_source`, never a name
  prefix: `mcp:exa` never matches `exa_pro`'s tools.
- **A runtime's own tools** take the group they declare (section 11.1).
  A tool without one belongs to its capability's group, named after the
  capability ID with the capability's description as summary
  (`computer_use@devbox`). A runtime-wide tool outside any instance belongs
  to `runtime`.
- **`web` is a new built-in group:** `webfetch` leaves `coding` for it
  (summary: "Fetching web pages."). `coding` keeps `read`, `write`,
  `edit`, and `bash`.

### 4.2 Selectors

- `<name>[@<runtime>]` for built-in and runtime groups, `mcp:<server>[@<runtime>]`,
  and `plugin:<name>`. The runtime ID follows `[a-z][a-z0-9_]{0,31}`.
- **Without `@runtime`** a selector matches the group on every runtime and
  in the base layer, as a plain permission action does.
- **Patterns** (agent documents only, D13): names take `*` and `?`; `*`
  alone is every group, `mcp:*` every MCP server. A session's choices and
  a project's defaults name groups exactly, as the clients write them.
- **Precedence:** layers first (section 5.2): the highest layer with a
  rule matching the group decides, however general that rule is. Within
  that layer, more literal characters win, then fewer wildcards, then a
  rule with a runtime over one without, then the later declaration.
- **A selector that matches nothing** is kept, and applies once its group
  appears: a server that connects later, a runtime attached again.

### 4.3 Fixed groups

`artifacts` (`read_artifact` reads truncated output back), `background`
(`stop_background` follows the background tools), the loader
(`tool_search` follows what is deferred), and `skills` (the `skill` tool is
offered while at least one skill is on) are never switched. A rule whose
selector names one exactly fails the document, or is refused by
`session.tools.set`; a pattern such as `*` in an agent document passes
over them. The clients do not show them.

### 4.4 Groups a runtime offered before (D14)

A runtime that is not running still shows what it brings, as of the last
time it ran:

- **What the daemon keeps,** in the runtime's record in `machines.json`
  (the record that already keeps its machine, platform, home, and when it
  was last seen; [machines](../concepts/machines.md#offline-runtimes)):
  - each capability's own groups, as its instance last declared them;
  - its runtime-scoped MCP servers;
  - per workspace, the workspace-scoped MCP servers and the project
    skills, for the 16 workspaces it opened most recently (older ones are
    dropped);
  - for each group its kind, name, summary, tool names, token estimate,
    and declared `default`; for each skill its name, description, and
    source; and when it was seen.
- **When it is updated:** each time an instance opens, sends
  `capability/tools_changed`, or reports `mcp/changed`, and when the
  runtime sends `runtime/tools_changed`. A server that fails keeps its last
  good entry. Nothing is kept for `global` and the daemon's own groups,
  which are always live.
- **When it goes away:** with the record, when the runtime is forgotten,
  its machine is forgotten, or a template leaves `config.toml`.
- **What the clients show:** live groups when the runtime runs; otherwise
  the cached ones, dotted, with when they were seen ("as of its last
  start, 2 days ago"). Choices on a cached group are stored like any
  other, by exact selector. A runtime that has never run, or a workspace
  it never opened, shows *known once it starts* for its tools and its
  project skills; the figures do not count it, and say so.
- **The model is not told:** `list_runtimes` and the runtimes block
  report live runtimes only, as today.

## 5. States and layers

### 5.1 States

- **`on`:** offered, eager or deferred as the defaults decide.
- **`deferred`:** offered deferred, loaded with `tool_search`. On a model
  that does not defer, and for an ACP agent, it is `on`.
- **`off`:** not offered.

`on` and `deferred` both override the deferral defaults: `on` sends the
group up front even where `always_load` would not, and `deferred` defers a
built-in group that is eager by default.

### 5.2 Layers

From lowest to highest:

1. **Defaults:** every group `on`, with deferral as today:
   `[deferred_tools] always_load`, the MCP and plugin `always_load` keys,
   and a runtime group's declared `default`. An ACP agent's defaults add
   `web: off`, which replaces the former `hide_tools: [webfetch]` default.
2. **Agent document:** `tool_groups` and `skills` (section 5.3).
3. **Session overlay:** section 5.5; a project's default lands here
   (section 5.4).

**Any rule of a higher layer beats every rule of a lower one** (D12): a
session's `coding: off` (from the CLI, say) turns `coding` off on `labpc`
too, even when the agent document says `coding@labpc: on`; and an agent
document's `"mcp:*": off` beats the defaults' `always_load` of any server.
A layer with no rule matching a group leaves the group to the next layer
down. Finer control of single tools belongs to
permissions, so the layer needs no precedence across layers.

### 5.3 Agent documents

```yaml
---
description: Reviews changes for correctness
mode: subagent
tool_groups:
  cron: off
  machines: off
  "mcp:*": deferred
  "mcp:github": on
skills:
  "*": off
  release-check: on
---
```

- **`tool_groups`** maps selectors to `on`, `deferred`, or `off`.
- **`skills`** maps patterns over published skill names to `on` or `off`.
  A colliding project skill is matched by its published name
  (`local_deploy`), so skills take no `@runtime`.
- Both default to empty (everything as the defaults say). At most 256
  rules each. Unknown states, malformed selectors, and fixed groups fail
  the document. Internal agents reject both keys.
- **Removed keys fail the document** with an error naming the
  replacement: `acp.hide_tools` ("use `tool_groups`") and a `skill` key in
  `permissions` ("use `skills`").

### 5.4 Project defaults

- A project's `defaults` gain `tool_groups` and `skills`: exact
  selectors and skill names, no patterns (D13).
- **When `session.create` names a project and carries no exposure of its
  own,** the daemon copies the project's into the new session's overlay,
  recorded in its first `session_tool_exposure_set`. A creation that
  carries its own uses it as given.
- **A copy, not a link:** editing the project later changes no existing
  session, and moving a session to another project keeps its overlay.
- It applies to every new root session in the project: from a client,
  `cookie --project`, and a cron job that creates a session. Subagents
  never get it (D5).
- Because the copy is session overlay, it beats the agent document.

### 5.5 The session overlay

- **State:** `tool_groups` and `skills` rule maps, next to the permission
  overlay. Every change appends a complete
  `session_tool_exposure_set { tool_groups, skills }` event. A log without
  one has an empty overlay.
- **Revert and fork:** a setting, like the permission mode. A revert at a
  reply's end keeps a change made after the reply; a revert to before a
  change drops it; a fork copies the overlay at its cut.
- **Drafts:** before the first prompt, the panels edit the draft, and the
  rules go with `session.create`, so the first run already uses them. A
  draft in a project starts from the project's defaults.
- **Subagents:** a delegated session starts with an empty overlay. Its
  own overlay can be changed like any session's.
- **What it leaves alone:** permission rules, remembered approvals, the
  approval reviewer, the permission mode.

## 6. Composition

- **Where the layer applies:** in `compose_for_policy_with`, after the
  permission filter and the existing gates (delegation targets, goal,
  memory), before deferral is planned. A group that is `off` is dropped;
  `on` and `deferred` feed the deferral plan ahead of `always_load`.
- **On cannot bring back** a tool that permissions hide. The panels show
  such a group as *hidden by permissions*.
- **Routed tools:** a routed tool is offered while at least one machine
  offers it. `coding@labpc: off` takes `labpc` out of the routed tools'
  machines and out of the runtimes block's tool list for `labpc`; the
  `machine` argument is a plain string, so no definition changes.
- **`tool_search`** never finds or loads a tool that is off.
- **A call to a tool that is off** is refused before permissions, with the
  notice's words (section 8.2). A call naming a machine where its group is
  off fails with where it is still offered, as a routed call to the wrong
  machine does today.
- **The runtimes block and `list_runtimes`** list each machine's groups as
  the layer leaves them.

## 7. Skills

### 7.1 Out of permissions

- **Removed:** the `skill` permission action, its default (`skill:
  allow`), its row in `/permissions`, and `cookie run --allowed-tools
  skill:<name>`.
- **The layer decides:** a skill that is off is left out of
  `<available_skills>`, and the `skill` tool refuses it:
  `` skill `release-check` is off in this session ``. With no skill on, the
  `skill` tool goes away (D8).
- **Loading never asks** (S1). The skill's own frontmatter still applies:
  `disable-model-invocation` keeps it out of the listing, `user-invocable:
  false` keeps it from the user, and `context: fork` delegates as today.
- **`allowed-tools`** grants as today: turn-scoped grants that turn asks
  into allows and never override a deny. `Skill(...)` is an action nothing
  uses, so it grants nothing (S2). The code that maps `Skill` grants onto
  the base layer's `skill` tool goes.
- **Shadowing is unchanged:** turning off a project skill does not bring
  back the user skill it shadows.
- **The user** can still run a skill that is off from `/skills` or
  `cookie run --skill` (D7); the picker marks it *off for the agent*.
- **ACP agents:** cookie's skill listing in the agent's context follows the
  layer, as does the `skill` tool through the bridge.

### 7.2 Migration

- Agent documents with `permissions.skill` fail (S3). None of the official
  agents or the user's documents declare one.
- `skill` rules in old sessions' `session_permission_overlay_set` events
  stay in the history and govern nothing (S4); `/permissions` no longer
  lists them.
- `permissions.md`, `agents.md`, and `skills.md` drop the `skill` action;
  the official agents' permission comments drop "use skills".

## 8. Mid-session changes

### 8.1 What changes when

The frozen-context rules apply unchanged (D6):

- **A deferred tool that leaves** leaves at once, and is announced with
  today's wording.
- **An eager tool that leaves** stays declared until the next refresh
  point, and calls to it are refused. New: it is announced (section 8.2).
- **A group that is turned on** joins as any new tool: in an announcement
  on a model that defers, else at a refresh point.
- **Skills:** a change is announced through the `skills` prompt block, as
  today. A skill already loaded stays in the context until the next refresh
  point or compaction.

### 8.2 The notice

Before the next request after an eager tool leaves, for any reason (an
exposure change, a runtime's new tool list, a failed MCP server), the
deferred-tools announcement gains a part for it, in a `<system-reminder>`.
The notice is recorded and rendered where the announcement already is, so
it never moves in the cached prefix and the clients show it as the
announcement's model-input row.

**Gone from some machines** (here `coding@labpc: off` while `local` and
`devbox` offer it), one part per machine:

```text
<system-reminder>
The following tool groups are no longer available on the machine "labpc" in this session. Calls to them with machine="labpc" will fail; they are still available on the machines each group lists:
<group name="coding" source="builtin" available_on="local, devbox">
read
write
edit
bash
</group>
</system-reminder>
```

**Gone from every machine** (`mcp:exa: off`, or a base-layer group such as
`cron`):

```text
<system-reminder>
The following tool groups are no longer available in this session. Their tools stay listed until the context is next refreshed, but calls to them will fail:
<group name="exa" source="mcp">
exa_web_search_exa
</group>
</system-reminder>
```

- **One reminder** holds every part due, with the deferred parts of the
  same announcement.
- **`available_on` is per group:** each group lists its own machines.
- **Back before the next refresh point:** a group whose tools are still
  declared returns with "The following tool groups are available again in
  this session:" (or "...available again on the machine "labpc" in this
  session:").
- **Refusals use the same words:** `` `bash` is no longer available on
  labpc in this session; it is on: local, devbox. `` and
  `` `exa_web_search_exa` is no longer available in this session. ``
- **History:** `deferred_tools_announced` gains optional `unavailable` and
  `available_again` parts, each group with `machine` (absent: every
  machine) and `available_on`. Older events have neither.

## 9. ACP agents

- **The bridge's `tools/list`** is the session's composition, so groups
  that are off are gone, and the bridge sends `tools/list_changed` at its
  check points. Codex ignores it; until its next session start, a call to a
  tool that is off is refused (section 6).
- **`acp.hide_tools` is removed** (D10). The `_acp.md` template drops it;
  the ACP defaults' `web: off` replaces its default. The user's
  `claude.md` and `codex.md` (`hide_tools: [read, write, edit, bash,
  webfetch]`) become `tool_groups: {coding: off, web: off}`, the same
  effect on every machine, since routed tools share one name.
- **`deferred` is `on`:** the harness chooses its own deferral.
- **The notice** (section 8.2) reaches the agent ahead of its next prompt,
  as `<machines_changed>` does.

## 10. MCP servers on demand

- **Wanted:** a server is wanted while at least one loaded session that can
  reach it leaves its group `on` or `deferred`. A session can reach a
  workspace-scoped server through an attachment to its instance, a
  runtime-scoped one through an attachment to its runtime, and a
  top-level one always. Paged-out sessions do not count; ACP sessions do.
- **Start:** when a client opens a session that wants it (subscribes to
  the session's events), so it usually connects while the user types; and
  when a run starts, a cron job fires, or an exposure change turns it back
  on. A run that needs a server still connecting waits for the attempt, as
  runs already wait for `mcp/reconnect`, bounded by its `timeout_ms`.
- **Stop:** `[mcp] idle_stop_after` after the last session that wants it
  stops wanting it (default `"10m"`; `0` or less keeps servers connected
  once started). `/mcp` shows it as `stopped: no session uses it`; `r`
  starts it, and it stops again after the same delay.
- **Startup:** nothing connects until a session wants it.
- **What the panels show** for a stopped server: its tools and token
  estimate from its last connection, marked *not connected*; a server that
  never connected shows *not started*.
- **Loaded deferred tools of a stopped server:** a session that wants the
  server starts it, so a loaded tool's server is running whenever its
  session is loaded; the reconnect rule of the deferred tools spec
  (section 11) covers a start that fails.
- **Config:** `[mcp]` gains `idle_stop_after`, beside `servers`. The
  runtime-side tables (`[runtimes.<id>.mcp]`, `[runtimes.local.mcp]`) take
  no copy: the daemon decides what is wanted.
- **Runtimes:** the daemon sends the wanted set (section 11.2). A runtime
  that predates it keeps connecting every server.

## 11. Runtime protocol (minor)

### 11.1 Tool groups

The `tools` object of `capability/open`, `capability/tools_changed`, and
`runtime/tools_changed` gains `groups`, and each spec an optional `group`:

```json
{
  "groups": [
    { "name": "coding", "summary": "The workspace: reading, writing, and editing files, and running shell commands." },
    { "name": "browser", "summary": "Drive a headless browser.", "default": "deferred" }
  ],
  "specs": [
    { "name": "bash", "group": "coding" },
    { "name": "browser_click", "group": "browser" }
  ],
  "prompt_sections": []
}
```

- **`name`** follows `[a-z][a-z0-9_]{0,31}`. A built-in name such as
  `coding` may be reused; cookie's own coding runtime declares `coding`
  and `web`. `mcp`, `plugin`, `runtime`, and the fixed groups are
  reserved.
- **`summary`** is what announcements and the panels show, at most 512
  bytes.
- **`default`** (optional, `on` or `deferred`; absent is `on`) is the
  group's state in the defaults layer.
- **A tool naming an undeclared group** is left out with a diagnostic, as
  composition does for a tool that declares `machine`. A declared group no
  tool names is ignored. Names are unique within one object.
- **Without `group`** a tool takes the fallback of section 4.1. An older
  runtime's tools all do.

### 11.2 The wanted MCP servers

`mcp/wanted`, daemon → runtime notification:
`{ instance?, servers: [name] }`, the whole wanted set of an instance, or
of the runtime's runtime-scoped servers without `instance`. The runtime
connects the servers it names that are not connected, and disconnects the
others, reporting each as `stopped` in `mcp/changed`
(`runtime/mcp_changed`). The daemon sends it at `capability/open`, and
whenever the set changes. A runtime declares support in
`runtime/initialize`'s result (`mcp_on_demand: true`); without it the
daemon never sends it.

`stopped` joins the MCP server states: no tools, no retry, not a failure.

## 12. Clients

### 12.1 No editor: one group or one skill at a time (D13)

The clients change one group or one skill at a time, to On, Deferred, or
Off (skills: on or off). There is no rule editor and no separate tools
panel:

- **Tool groups sit on the runtime that brings them:** an attachment in
  the session's runtimes panel, a card on the New session page, a card in
  the project editor. The daemon's groups and the top-level MCP servers
  sit on `global`.
- **Skills have their own place:** a Skills tab in the session panel, and
  a Skills section on the New session page and in the project editor.
- **A choice writes the group's exact selector:** `coding@labpc`,
  `mcp:playwright@local` for a server a runtime hosts, `mcp:exa` for a
  top-level server, `memory` for a daemon group. Two attachments of one
  runtime share the selector (D3), so changing one changes both.
- **Picking the state the layers below already give** removes the
  session's (or project's) choice instead of storing a copy. **Reset**
  removes it and names the state it falls back to.
- **Choices act at once,** like permission rules, and reach every client
  with the session open.

### 12.2 Web

The [preview](session-tool-groups/preview.html) shows each part below in
both themes and on a phone.

- **Session panel › Runtimes:** each attachment keeps its name, status,
  description, Retry, and Detach, and gains a **Tools** row of chips, one
  per group:
  - on (plain), deferred (accent), off (dashed, struck through); a dot
    says who set it: this session (accent), the project's defaults
    (dark), the agent (grey); no dot for the defaults;
  - groups that are on and untouched fold into one **+N** chip; the fixed
    groups are not shown;
  - a group permissions hide is dimmed and not clickable, its tooltip
    pointing at the Permissions tab; a stopped MCP server is dotted and
    shown from its last connection;
  - a click opens a menu: the group's name, tool count, and token
    estimate; On, Deferred, Off; **Reset** (to the layer below, named);
    and the group's tools;
  - an eager group turned off adds a line under the row: it stays listed
    to the model until the context refreshes, and calls are refused.
  - Above the attachments, three figures: tools up front and their
    tokens, tools deferred and their tokens, groups off and their tools.
- **Session panel › Skills,** a new tab after Runtimes, counting the
  skills that are off: a filter, then the skills grouped by where they
  come from (each attachment's project skills, user, official). Each has
  its description, the same who-set-it dot, ↺ for this session's choice,
  **Run** (invokes it as the Skills sheet does, on or off, D7), and a
  switch. Shadowed skills are struck through without a switch.
- **The New session page** scrolls above the composer:
  - the runtime cards, each selected card with the same **Tools** row
    under its workspace field; an unselected card says what it would bring
    ("Brings coding, web"); a runtime that is not running shows the groups it
    offered the last time it ran, dotted, with when (section 4.4); one
    that never ran shows the choices that name it, then "known once it
    starts";
  - under the cards, below the line that says where the session will
    work: the up-front, deferred, and off figures, and in a project
    "From project greeter: ..." with **reset to the project's**;
  - then **Skills**, as the Skills tab lists them, following the selected
    cards (a selected coding card adds its workspace's project skills, cached
    for a runtime that is not running, "known once it starts" for one that
    never ran);
  - **Runtimes · Skills** links beside the project picker jump between the
    two and stay in view; the Skills link counts the skills that are off.
- **The project editor** puts the same Tools rows on its **New sessions
  attach** cards and a **New sessions' skills** list under them. The
  accent dot means set by the project there. They are saved with the
  cards on **Save** as `defaults.tool_groups` and `defaults.skills`.
- **No composer chip:** the Runtimes tab is where tool groups live.
- **Phones:** the same chips; a tap opens the states as a bottom menu.

### 12.3 TUI

- **`/runtime`** lists each attachment's groups under it, with state,
  who set it, and the token estimate; on a group, left/right steps On,
  Deferred, Off, and `d` resets it. The details pane lists the group's
  tools. On a draft it edits the draft, as it does for runtimes today.
- **`/skills`** gains a state column: space switches the selected skill
  on or off for the agent, and Enter still runs it.
- **`/project`'s editor** edits the project's defaults the same way.

### 12.4 Transcript

The tools row counts the groups that are off ("3 groups off"). A change
shows as a quiet row naming who changed what, and the notice (section
8.2) as the announcement row.

### 12.5 CLI

- `cookie run --tool-group 'mcp:exa=off' --tool-group 'coding@labpc=deferred'
  --skill-state 'deploy=off'`, each repeatable and exact (no patterns):
  the new session's choices, or the resumed session's with
  `--resume-session`, on top of a project's defaults. (`--skill-state`,
  since `--skill <name>` already runs a skill.)
- `cookie sessions tools <id>` prints the session's groups and skills with
  their states and who set them.

## 13. Client protocol and history

- **Client protocol (minor):**
  - `session.tools.get { session }` returns the groups (selector, kind,
    runtime, attachment, state, layer, fixed, hidden by permissions,
    connected, tool names, token estimate) and the skills (name, source,
    runtime, state, layer).
  - `session.tools.set { session, tool_groups, skills }` replaces the
    overlay; it refuses patterns, fixed groups, and malformed selectors.
    The clients send the whole overlay with the one change applied.
  - `session.create` takes `tool_groups` and `skills`.
  - The session snapshot carries the overlay; every client with the
    session open is told of a change.
  - A project's `defaults` gain `tool_groups` and `skills`.
  - `runtimes.list` gains each capability's groups and its runtime-scoped
    MCP servers, live or cached with `as_of`; `runtimes.groups { runtime_id,
    capability, workspace }` returns a workspace's MCP servers and project
    skills the same way, or `known: false`. The New session page and the
    project editor read them for the cards they show.
- **History:** `session_tool_exposure_set`; the announcement parts of
  section 8.2; run records' agent snapshots gain the document's
  `tool_groups` and `skills`, defaulted.
- **Removed:** the `skill` permission action from the protocol's action
  set and from `--allowed-tools`.

## 14. Tests

- **Composition:** `coding@labpc: off` with `coding` on elsewhere; `mcp:*`
  off with `mcp:exa` on in one layer; a session's `"mcp:*": off` beating
  an agent's `mcp:github: on`; a layer with no matching rule passing the
  group down; fixed groups
  refused by name and passed over by patterns; on never revealing what
  permissions hide; `web` split out of `coding`.
- **Deferral:** `deferred` defers an eager built-in group, `on` sends an
  MCP server up front, a runtime group's `default`, `deferred` as `on` on
  a model that does not defer and for an ACP agent.
- **Layers:** agent document keys, ACP default `web: off` and its override,
  a project's defaults copied at creation (and not on a later project edit
  or move), drafts, no inheritance by subagents.
- **History:** the overlay survives restart; revert at a reply's end keeps
  a later change, revert before it drops it, fork copies it.
- **Notice:** both forms; several machines and groups in one reminder;
  available again; a runtime's new tool list and a failed MCP server
  trigger it; refusals' words; nothing announced for a deferred tool
  beyond today's part.
- **Skills:** off out of the listing and refused by `skill`, user
  invocation still allowed, `Skill(...)` inert, `permissions.skill` and
  `acp.hide_tools` fail documents, old `skill` overlay rules govern
  nothing.
- **MCP on demand:** start on session open, run waits for a connecting
  server, stop after `idle_stop_after`, `0` keeps it, a stopped server's
  cached tool list, a runtime without `mcp_on_demand` keeps connecting.
- **Runtime protocol:** declared groups, undeclared group diagnostics,
  reserved names, the fallback group.
- **ACP:** the bridge drops off groups and sends `list_changed`.
- **The cache:** filled at instance open and on each tool or MCP report;
  a failed server keeps its last good entry; the 16-workspace bound; it
  survives a daemon restart and goes with forget and a removed template;
  a never-run runtime reports `known: false`.

## 15. Phases

1. **Engine and protocol:** groups and selectors, the `web` split, the
   layer and its event, agent document keys and the removed keys, project
   defaults, composition and refusals, `session.tools.*`.
2. **Skills out of permissions:** the action's removal, the layer for
   skills, `allowed-tools`, docs.
3. **The notice** for eager tools that leave, in the announcement and its
   event.
4. **Runtime protocol:** declared groups and `mcp/wanted`; MCP servers on
   demand and `[mcp] idle_stop_after`; the cache of groups a runtime
   offered, `runtimes.list`'s groups, and `runtimes.groups`.
5. **Clients:** the TUI's `/runtime`, `/skills`, and project editor; the
   web Runtimes and Skills tabs, the New session page, and the project
   editor; the transcript, the CLI; the user's `claude.md` and `codex.md` and the
   `_acp.md` template.

## 16. Documents to update on landing

`extend/agents.md` (frontmatter keys, permissions, ACP documents),
`extend/acp-agents.md`, `extend/skills.md`, `extend/mcp.md` (connection
lifecycle, `[mcp] idle_stop_after`), `concepts/permissions.md`, `concepts/machines.md` (the cache),
`concepts/projects.md`, `reference/tools.md` (groups table, `web`),
`reference/system-prompt.md` (the notice), `reference/protocol.md`,
`reference/runtime-protocol.md`, `reference/events.md`,
`engine/deferred_tools.md`, `clients/tui.md`, `clients/web.md`,
`clients/cli.md`.

## 17. Implementation notes

### 17.1 Phases 1 and 2 (2026-11-03)

- **Phases 1 and 2 landed together,** with the runtime-declared groups of
  phase 4 (section 11.1): the daemon reaches its own `local` coding
  runtime over the runtime protocol, as every other runtime, so without
  declared groups its tools had no `coding` or `web` group and the ACP
  default `web: off` matched nothing.
- **Wire names.** `session.create`, `session.tools.set`, and a project's
  `defaults` carry one `exposure` (`ToolExposure { tool_groups, skills }`)
  rather than separate `tool_groups` and `skills` members. A rule copied
  from a project carries `from_project`, which `session.tools.get` reports
  as the `project` layer.
- **A runtime tool naming an undeclared or invalid group** belongs to its
  capability's group, without a diagnostic (section 11.1 said it is left
  out).
- **Agent document edits** reach a session at its next run: a run's agent
  rules are frozen into its snapshot when it starts. A session's choices
  apply from its next request, also during a run.
- **The ACP bridge** refuses a call to a tool the session no longer
  offers with "`<tool>` is no longer available in this session".
- **Not yet:** MCP servers on demand (section 10) and `mcp/wanted`, the
  cache of a runtime's groups (section 4.4), and the clients (section 12).

### 17.2 Phase 3 (2026-11-03)

- **The notice is an event of its own,** `tools_availability_announced`
  (`unavailable`, `available_again`), rendered as its own
  `<system-reminder>` turn, rather than parts of
  `deferred_tools_announced`: what a request announces about deferred tools
  and about tools that left are then two turns in a row, not one.
- **Each round tool records its group and its machines** (`group`,
  `machines` of `RoundToolDefinition`), which the notice compares with what
  the request publishes. A round recorded before them only learns of tools
  that left every machine, named by their own name as the group.
- **The clients show it as a transcript row** (a model-input row,
  `tools no longer available · coding on labpc`), its text the notice.
- **Refusals keep today's words:** "`<tool>` is no longer available in
  this session" for a tool gone everywhere, and the routed tools' "is not
  on `<machine>`; it is on: ..." for one machine.
- **ACP agents** get no notice: the bridge's list changes and says so
  (`tools/list_changed`), and a call to a tool no longer offered is refused.

### 17.3 The groups a runtime offered (phase 4, 2026-11-03)

- **Kept in `runtime_groups.json`,** a file of its own in the data root,
  not in `machines.json`: the built-in `local` runtime has no record
  there, yet a workspace it has not opened needs its groups too. Entries
  of a runtime the daemon no longer knows are dropped when one is looked
  up.
- **`runtimes.groups { runtime_id, capability?, workspace? }`** answers
  for one card, rather than `runtimes.list` carrying every capability's
  groups: live from a running instance, else the exact workspace's entry,
  else the latest workspace's (`seen_at`, without its project skills).
- **Refreshed** whenever a session gathers its tool sources, at most once a
  minute per instance.


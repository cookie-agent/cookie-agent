# Onboarding with ACP agents, the agents panel, and unavailable agents

Status: **approved** (2026-11-01), not implemented yet. The user took
every recommendation of [section 11](#11-decisions) on 2026-11-01.

## 1. Goal and scope

Today a daemon is set up only through model providers, and an ACP agent is
something you configure by hand:

- **Onboarding requires a provider.** `onboarding.get` reports `provider`
  and `default_model` as missing until a provider is connected, so a user
  who only wants Claude Code or Codex sees a setup banner they can only
  dismiss with **Not now** (`extend/acp-agents.md`, "Before you start").
- **Declaring an ACP agent means copying TOML** from the guide: the adapter
  package, `npx -y`, and which table (`[runtimes.local.acp.agents.*]` or
  `[acp.agents.*]`) gives which reach.
- **A prompt means copying a document.** A declaration without a daemon-side
  document runs with no body and only the default rules. The guide's
  "reference document for Claude Code" must be copied into
  `agents/claude.md` and edited for any other harness.
- **Official agents cannot be seen or managed.** The web's Agents view lists
  only files under `agents/`, and the TUI has only `/config`'s editor.
  Disabling or adjusting an official agent means writing an `inherit:`
  document by hand.
- **Agents with no usable model disappear.** Pickers keep only
  `runnable_as_root` agents. An agent whose fallback list resolves to
  nothing (field unset, provider disconnected, model gone) drops out with
  no reason. `unset_models` explains only one of those causes, and no client
  shows it in a picker.

This spec covers:

1. **Two onboarding paths:** connect model providers (today's steps), link an
   ACP agent, or both (sections 2 to 5).
2. **ACP presets:** Claude Code and Codex fill in their launch command, and
   **Other** takes any ACP agent. Each can run on every machine or on the
   daemon's machine only (sections 3 and 4).
3. **A universal ACP document:** an official template that every ACP
   agent follows unless the user writes their own. Inheritance customizes it
   with a few lines (section 6).
4. **The agents panel:** official agents, the user's agents, and ACP
   agents, with Disable, Customize (inherit and override), Replace, and Reset
   (section 7).
5. **Unavailable agents listed with a reason** in every client's agent
   selection and in `list_agents` (section 8).

**Out of scope:**

- Installing Node.js or an adapter, or logging the user in to the harness.
  Cookie still runs no login flow; it states what each machine needs.
- ACP v1 agents.
- Presets beyond Claude Code and Codex; **Other** covers the rest.
- Choosing a subset of machines in onboarding. The Machines view's
  per-runtime **Turn off here** already does that.

## 2. Readiness

The daemon is **ready** when both of these hold:

- **Some agent can run as a root and the default agent resolves:** `[agents]
  default`, or the first runnable root agent while that key is unset. This is
  today's `default_agent` requirement.
- **At least one path is complete:**
  - **Models path:** a provider is connected and `default_model` names an
    available model (unchanged).
  - **ACP path:** at least one enabled ACP declaration is offered by a
    runtime that can run it now (connected, or started on demand). This is
    the `AgentRuntime.available` the agent listing already computes.

ACP readiness comes from the configuration, not from running the agent: a
declaration whose program is missing still counts. The check in section 5
reports that instead, and the agent's first run fails with the same
reason.

**Protocol (client minor, additive):**

- `OnboardingRequirement` gains **`agent`**: no path is complete. While the ACP
  path is complete, `provider` and `default_model` no longer appear in
  `missing`; they move to the models path below.
- `OnboardingStatus` gains:
  - **`paths`**:
    - `models`: `{ complete, missing: [provider | default_model] }`;
    - `acp`: `{ complete, agents: [OnboardingAcpAgent] }`. Each agent has
      `id`, `where` (`every_runtime` or `runtime(<id>)`), `command`, `env`
      keys (never values), `description`, `enabled`, `preset` (the preset ID
      when the command equals one), and `runtimes` (each with `available`).
  - **`acp_presets`**: the presets of section 3.
  - **`default_agent`**: the agent new sessions start with, and whether it
    comes from `[agents] default` or is the first runnable root.

**ACP only, no provider:** sessions run, but the `title` and `approval`
agents have no model. The agent may still name the session, and `ask`
requests go to the user. Onboarding's summary says so and offers the models
path for `utility_model` alone. Readiness does not require it.

## 3. ACP presets

| Preset | ID | `command` | Needs on each machine |
|---|---|---|---|
| Claude Code | `claude` | `["npx", "-y", "@agentclientprotocol/claude-agent-acp"]` | Node.js (`npx` on the runtime's `PATH`) and Claude Code logged in (`claude`, then `/login`) |
| Codex | `codex` | `["npx", "-y", "@agentclientprotocol/codex-acp"]` | Node.js, and `codex login` |
| Other | (typed) | (typed) | (whatever the agent needs) |

- **Where presets come from:** the official agents repository
  (`cookie-agent/agents`). Its `index.json` gains `acp_presets: [{ id, name,
  command, env, description, needs, docs_url, requires_cookie }]`. The daemon
  refreshes them with the official agents, so when an adapter package is
  renamed (it has been once already, from `@zed-industries/...`), daemons
  learn of it without a release. `cookie agents check` validates the
  presets. Older daemons ignore the new field: the index is not parsed
  strictly.
- **What onboarding writes is the literal command and environment**, which
  stay editable. Later preset changes never rewrite a declaration. Instead,
  the agents panel shows **"The Claude Code preset now launches `<new
  command>`"** with **Use it**, when a declaration's `preset` origin no
  longer matches.
- **The ID** is prefilled (`claude`, `codex`) and editable. One that is
  already taken (by a declaration, a document, or an official agent) is
  refused in place, with the reason.

## 4. Where it runs

Onboarding offers two choices, each mapped to a table that exists today:

| Choice | Writes | Effect |
|---|---|---|
| **On every machine** | `[acp.agents.<id>]` | Every runtime but `global` offers it and runs it on its own machine: the daemon's own, ssh templates, dial-ins, and a TUI's or web page's offered runtime. The agent picker lists it once per runtime (`claude · on lab`). |
| **On this daemon's machine only** | `[runtimes.local.acp.agents.<id>]` | Only `local` offers it. |

- The step lists the runtimes the daemon knows now, each with whether it
  would offer the agent, so "every machine" is concrete.
- "Every machine" means each machine needs the harness and its login. The
  check (section 5) runs per runtime and shows which machines are not ready
  yet. A machine without the harness keeps offering the agent, and its runs
  fail with the reason. **Turn off here** in the Machines view removes it
  from that machine.

## 5. Checking an agent

The check is a new RPC, `acp.check { runtime_id, declaration }`.
`declaration` is either the ID of an existing declaration or an inline
`{ command, env }`, so a check can run before anything is written.

1. The runtime starts the process in its home directory.
2. `initialize`: the ACP version (v1 is refused, with today's message), the
   agent's name and version, and its auth methods.
3. `session/new` in the home directory. `auth_required` fails here, and the
   result names the agent's login command where the preset knows it
   (`needs`).
4. The result records the models, modes, and options the agent offered.
   They also fill `AgentRuntime.options`, so the onboarding model choice and
   a new draft's pickers have values before the first real session.
5. The session is closed and the process ends (unless an existing
   declaration's process was already running, which is reused and kept).

- **Stages and outcome:** the result is `{ stage_reached, ok, error?, agent_name?,
  agent_version?, options }`. The timeout is 120 s, since a first `npx` run
  downloads the adapter. Progress is shown as stages. Hard-coded output to
  the user stays in client_state.
- **Optional:** Finish never waits for a check, and a failed check does not
  block it.
- **Also used** by the agents panel's **Check** on any ACP agent, and by
  `cookie agents check-acp <id> [--runtime <id>]`.

## 6. The universal ACP document

### 6.1 Template documents

- **A template** is an agent document whose file name starts with `_`
  (`_acp.md`). It is parsed and checked like any document and may be
  inherited, but it is never an agent: not listed, never run, and its name is
  not an agent ID. Today such a name is a load error (`invalid agent
  filename`). Templates are allowed in `official_agents/`, `agents/`, and
  preset folders.
- **Inheriting** is unchanged: `inherit: ../official_agents/_acp.md` or
  `inherit: _acp.md`. A template may inherit another template, or an agent.
- **The official agents index** lists templates as entries with `kind:
  "template"`, and `requires_cookie` at the release that knows templates, so
  older daemons skip them.

### 6.2 The official `_acp` template

```markdown
---
description: An ACP agent driven by cookie
mode: all
acp: {}
permissions:
  read: allow
  write: { "${workspace_dir}/*": allow, "*": ask }
  bash: { "git status*": allow, "git diff*": allow, "git log*": allow, "*": ask }
  memory: allow
  memory_write: { "user://*": allow, "agent://${agent_id}": allow,
                  "machine://${session_machine}/*": allow,
                  "project://${session_project}/*": allow }
---
You are an agent in cookie agent, running in ACP mode: cookie drives you
over the Agent Client Protocol, and your actual harness is the program
cookie launched (such as Claude Code or Codex). The user talks to you
through cookie (its terminal UI, web client, or CLI, possibly from another
machine), and cookie records this conversation.

Guidelines:
… (the guide's reference document today, minus the harness name)
```

- **No `models`:** an ACP agent's models are the harness's own values. A
  user's document sets them (section 6.3).
- **Permissions** are the guide's reference document's, plus the memory
  write rules the official `primary` uses. Each line carries a comment, as in
  the other official agents.
- **The body** is today's reference body with "Claude Code" replaced by a
  neutral phrase (question Q3).

### 6.3 Which document an ACP agent runs

For each declaration ID, the first rule that applies:

1. **A document of that ID in `agents/` (or the preset) exists:** it is used
   as today. With `inherit:` it follows its target; without it, it replaces
   whole.
2. **Otherwise the agent runs the effective `_acp` template:** the user's
   `agents/_acp.md` (or the preset's) if it exists, else the official one.
   The ID is the declaration's, and the description is the declaration's when
   it has one.
3. **No template at all** (official agents off and none of the user's):
   today's behaviour, with no body and the default rules.

So:

| You want | You write |
|---|---|
| Nothing; the defaults are fine | nothing |
| Change every ACP agent at once (a line in the prompt, a permission) | `agents/_acp.md` with `inherit: ../official_agents/_acp.md` and the changed keys |
| Change one agent (its model, mode, a rule) | `agents/claude.md` with `inherit: _acp.md`, or `inherit: ../official_agents/_acp.md` when no `agents/_acp.md` exists |
| Your own prompt for one agent | `agents/claude.md` without `inherit` (as today) |

The agents panel writes these files for you (section 7). A one-agent
customization looks like:

```markdown
---
inherit: ../official_agents/_acp.md
models: [{ model: "opus", variant: "high" }, "sonnet"]
acp: { mode: plan }
---
{{default_prompt}}

Prefer small, reviewable commits.
```

**Behaviour change:** a declaration without a document today runs on the
default rules, which ask for most actions. With rule 2 it runs on the
template's rules, which allow reads, memory, and writes inside the
workspace. The release notes and the guide say so.

## 7. The agents panel

One panel in both clients: the web's Agents view, reworked, and a new TUI
`/agents` built on the same `client_state` model. `/config` keeps its
editor for raw edits.

### 7.1 What it lists

Groups, each row showing the ID, description, an origin badge, and
availability (section 8):

- **Official:** `primary`, `coder`, `explore`, `worker`, and the internal
  `approval`, `title`, and `compaction`, with the official version. The
  badge reads **official**, **customized** (a user document inherits it),
  **replaced** (a user document of the ID without `inherit`), or
  **disabled**.
- **Yours:** the user's own agents with no official counterpart, shared and
  per preset.
- **ACP agents:** one row per agent ID, with where it is offered (every
  machine, or the runtimes), and its document state: **ACP template**,
  **customized**, or **own document**.
- **Templates:** `_acp` (official, customized, or replaced), and any
  templates of the user's.

### 7.2 Actions

| Action | On | Writes |
|---|---|---|
| **Disable / Enable** | official agent | `agents/<id>.md` = `inherit: ../official_agents/<id>.md` + `enabled: false`. Enable removes the key, and the file when nothing else is left. |
| **Disable / Enable** | ACP agent | `enabled = false` on its declaration; the agent stays listed as unavailable (section 8) |
| **Customize** | official agent, ACP agent, template | A form over the keys worth overriding: `description`, `mode`, `models` (the fallback list editor, with `${default_model}`-style entries for cookie's agents and the check's offered values for ACP agents), permissions per action, `acp.mode` and `acp.config`, limits, and **Add to the prompt** (`{{default_prompt}}` then the text). Only changed keys are written, under `inherit:`. A key back at the inherited value is removed. |
| **Replace** | official agent, template | Copies the whole document into `agents/` (today's "open as a copy"), after a warning that it no longer follows refreshes |
| **Reset** | customized, replaced, or disabled | Deletes the user's document, with a confirmation that shows it |
| **View** | any | The effective document, read-only, with each key marked inherited or set here |
| **Check** | ACP agent | Section 5, per runtime |
| **Use it** | ACP agent from an outdated preset | Updates the command (section 3) |
| **Add ACP agent** | — | The onboarding ACP step (section 9) |
| **Remove** | ACP agent | Deletes its declaration, and its document after asking |

- **Daemon RPCs** (client minor) do the writes, so the TUI, the web, `cookie
  agents`, and third-party clients behave alike:
  - `agents.override { id, preset?, set, unset }`: creates or edits the
    user's inheriting document. It keeps the keys and body it does not
    touch, and validates like `config.apply`.
  - `agents.reset { id, preset? }`;
  - `agents.replace { id, preset? }`;
  - `agents.document { id, preset? }`: the effective document with
    per-key origin.

  ACP declarations keep `runtimes.agents.set` and `remove`, plus the
  every-runtime equivalents.
- **CLI:** `cookie agents disable|enable|reset|replace <id>`, `cookie agents
  set <id> <key> <value>`, and `cookie agents show <id>`.
- **Presets:** the panel acts on the preset it is showing. In a preset, the
  inherit target is that preset's own document of the ID where one exists,
  as `{{default_prompt}}` already resolves.
- After onboarding, the summary links to the panel (**Review agents**).

## 8. Unavailable agents

### 8.1 What the daemon reports

`AgentDescriptor` gains `unavailable: Option<AgentUnavailable>`, computed
when the registry compiles:

- `reason` (an open enum):
  - `disabled`: the document's `enabled: false`;
  - `no_model`: the fallback list resolved to nothing, with `entries`;
  - `inherit_missing { target }`;
  - `acp_not_offered`: no runtime declares it;
  - `acp_declaration_disabled`;
  - `acp_runtimes_unavailable`: declared, but every offering runtime is
    away;
  - `official_pending`: an official agent before the first refresh.
- `entries`, for `no_model`: each entry of the list as written
  (`${default_model}`, `openai/gpt-5`, a field's expanded models), with
  why it gave no model:
  - `field_unset { field }`;
  - `provider_not_connected { provider }`;
  - `model_not_offered`;
  - `variant_not_offered { variant }`;
  - `parent_model_only`, for internal agents, which is not a failure.

  At most 32 entries.
- Subagent-only agents are not unavailable for lacking a root mode. That is
  `runnable_as_root`, unchanged.
- `unset_models` stays for older clients.

### 8.2 Where it shows

- **Agent selection** (the TUI's `/agent` picker, the web's agent sheet and
  chip): unavailable root agents, except those the user disabled (D6: reasons
  `disabled` and `acp_declaration_disabled`), are
  listed below the available ones, under a divider (D7),
  dimmed and not selectable, each with a one-line reason formatted by
  `client_state`, for example:
  - "No model: `default_model` is not set";
  - "No model: provider `openai` is not connected";
  - "It inherits `../official_agents/x.md`, which does not exist";
  - "No connected machine offers it".

  Hovering or opening the row shows every entry. The TUI's Tab cycling skips
  them. Each row hovers visibly (AGENTS.md, TUI).
- **The agents panel:** the same line, with every entry.
- **`list_agents`** (the tool) lists unavailable agents with the reason, so a
  model does not delegate blindly. `delegate_subagent` refuses one with that
  reason.
- **`cookie agents list`** shows an `unavailable: <reason>` column. `cookie
  run -a <id>` fails with the reason.
- **A session whose agent becomes unavailable:** the composer shows the
  reason, and Send is refused with it. The selection falls back only when
  the agent no longer exists, as today.

## 9. Onboarding flows

All clients walk the same steps, built from `onboarding.get` and the shared
rules in `client_state::onboarding`.

1. **How agents run** (new first step): two cards, either or both:
   - **Model providers:** cookie's own agents run on models you connect.
   - **An ACP agent:** Claude Code, Codex, or another ACP agent runs with its
     own login.

   Each card is preselected when its path is complete.
2. **Models path** (when chosen): Provider, Default model, Small model,
   Utility model, as today.
3. **ACP path** (when chosen): **ACP agent**:
   - pick Claude Code, Codex, or Other (several may be added, listed with
     a remove button);
   - **Where:** on every machine, or on this daemon's machine only, with the
     runtime list;
   - **ID, command, environment, and description:** prefilled from the
     preset, editable, and empty for Other;
   - **What each machine needs** (the preset's `needs`);
   - **Check**, per runtime (section 5);
   - after a successful check, an optional **Model** among the values the
     agent offered. This writes an inheriting `agents/<id>.md` with `models`.
4. **Tools** (optional, added 2026-11-01, section 9.1): MCP servers that
   need no login, such as web search and a browser.
5. **Finish:**
   - **New sessions start with:** the root agents runnable after applying,
     preselecting what resolves now, and written to `[agents] default` only
     when it differs from that;
   - the summary, with the ACP-only note of section 2;
   - **Review agents** and **Start your first session**.

**Protocol:** `onboarding.run` gains:

- `acp_agents: [{ id, where, command, env, description?, preset?, models?
  }]`;
- `mcp_servers` (section 9.1);
- `default_agent?`.

It applies everything in one candidate configuration, as today, and refuses
with `onboarding_incomplete` when no path would be complete afterwards.

**Per client:**

- **`cookie onboard`:** the first question is "Run agents on: [1] model
  providers [2] an ACP agent [3] both". Flags:
  - `--acp claude|codex|other`;
  - `--acp-where every|local`;
  - `--acp-id`;
  - `--acp-command` (for `other`, or to override);
  - `--acp-env KEY=VALUE` (repeatable);
  - `--acp-check`;
  - `--default-agent`.

  `--yes` without a path refuses.
- **TUI:** the onboarding panel gains a **Start** tab and an **ACP agent**
  tab. The tabs shown follow the chosen paths. The notice reads "No agent can
  run yet: connect a provider or link an ACP agent". There are new pointer
  targets for the cards, presets, where-choices, and Check, each with hover.
- **Web:** the stepper's first step is the two cards. Its steps follow the
  choice. The ACP step uses the Machines view's ACP agent form. Full screen
  on a phone, as today.

### 9.1 The Tools step: MCP servers without a login

Added at the user's request on 2026-11-01 (D9). The last step before
Finish offers a few MCP servers that work without signing in to anything,
so a fresh daemon's agents can search the web, read library documentation,
and use a browser. It is optional, and nothing is selected by default.

- **Presets** come from the official agents repository with the ACP
  presets: `index.json` gains `mcp_presets: [{ id, name, category,
  description, server, where, needs, docs_url, requires_cookie }]`. `server`
  is the `[mcp.servers.<name>]` table as `config.toml` takes it (`url`, or
  `command` and `args`). `where` is `global` for a hosted server, or
  `machine` for one that runs a program on a machine. The first presets:

  | Preset | Category | Server | Where |
  |---|---|---|---|
  | Exa | Web search | `url = "https://mcp.exa.ai/mcp"` | `global` |
  | Context7 | Library documentation | `url = "https://mcp.context7.com/mcp"` | `global` |
  | DeepWiki | Documentation of public GitHub repositories | `url = "https://mcp.deepwiki.com/mcp"` | `global` |
  | Playwright | Browser | `command = "npx"`, `args = ["-y", "@playwright/mcp@latest", "--headless"]` | `machine` |

  The repository's CI checks that each hosted preset answers MCP
  `initialize` without credentials. A preset that stops working is fixed
  or removed there, with no release.
- **Where a server goes:**
  - A `global` preset is written to `[mcp.servers.<id>]`. It runs where the
    daemon runs, and every session that attaches `global` gets it.
  - A `machine` preset asks which machine: the daemon's own (`local`, the
    default) or another runtime the daemon knows. It is written to
    `[runtimes.<runtime>.mcp.servers.<id>]` with `scope = "runtime"`.
    Playwright also offers **Show the browser window**, which drops
    `--headless`. Its `needs` names Node.js and a browser Playwright can
    start, and links [Giving agents a browser](../extend/browser.md).
- **Already set up:** a preset whose name already has a server at that
  place shows as configured and cannot be added twice.
- **Permissions are unchanged:** the official agents' `mcp` rules still
  decide each tool call. The step says that calls ask until allowed, and the
  summary links to the agents panel.
- **Protocol:**
  - `OnboardingStatus` gains `mcp_presets`, and `mcp_servers`: the
    configured servers' names and places.
  - `onboarding.run` gains `mcp_servers: [{ name, preset?, where: global |
    runtime(<id>), server }]`, applied in the same candidate configuration
    and checked like `config.apply`. A name already taken at that place is
    refused.
  - The Tools step is not part of readiness.
- **After Finish,** the summary shows each new server's connection state as
  `mcp.changed` reports it.
- **`cookie onboard`:** `--mcp exa,context7,deepwiki,playwright`, and
  `--mcp-runtime <id>` for the machine presets (default `local`).

## 10. Protocol, configuration, and phases

- **Client protocol minor:**
  - the readiness fields;
  - `acp_presets`;
  - `acp.check`;
  - `onboarding.run`'s ACP fields;
  - `AgentDescriptor.unavailable`;
  - the `agents.*` RPCs.

  All enums are open. No runtime or plugin protocol change is needed, apart
  from a runtime minor if `acp.check` needs a new runtime request rather than
  today's start-the-agent path.
- **Configuration:** `_`-prefixed agent files become templates. The reference
  and its tests cover a template that is listed nowhere, inherits, and is
  inherited, plus the strict errors (a template named as `[agents] default`,
  a delegation target, or `acp.agent`).
- **The official agents repository:** `_acp.md`, `acp_presets` in
  `index.json`, and `cookie agents check` for both.
- **History:** unchanged.

**Phases**, each committed and pushed:

1. Engine: templates, the effective `_acp` rule, `AgentDescriptor.unavailable`
   with its reasons, and `list_agents`/`delegate_subagent` reasons.
2. The official repository: `_acp.md`, the ACP and MCP presets, and the
   checks.
3. Onboarding: readiness paths, `acp.check`, `onboarding.run`'s ACP and MCP
   fields, and `cookie onboard`.
4. Clients: the onboarding paths and the Tools step in the TUI and web, and
   unavailable agents in the pickers.
5. The agents panel: the `agents.*` RPCs, the web view, TUI `/agents`, and
   the `cookie agents` commands.
6. Docs: the ACP guide (onboarding first, the reference document replaced by
   the template), the agents page, onboarding in the quickstart, and the
   configuration reference.

## 11. Decisions

Decided by the user on 2026-11-01; each is the recommendation the draft
made.

- **D1. Templates are `_`-prefixed files** (section 6.1), not an official
  agent with a `mode: template`, which every inheritor would inherit and
  have to undo.
- **D2. Undocumented ACP declarations follow the effective `_acp` template**
  (section 6.3, rule 2), with the permission change for existing setups
  stated in the release notes and the guide.
- **D3. The template's body names no harness:** "the program cookie
  launched (such as Claude Code or Codex)". No `{{acp_agent}}` placeholder.
- **D4. ACP presets live in the official agents repository** and refresh
  with it (section 3), not in the binary.
- **D5. `acp.check` is in scope** (section 5), optional, and never blocks
  Finish.
- **D6. An agent the user disabled (`disabled`, or
  `acp_declaration_disabled` on every runtime) is hidden from agent selection** and
  shown only in the agents panel (and `cookie agents list`); agents
  unavailable for any other reason are listed with the reason (section 8.2).
- **D7. Unavailable root agents are listed below a divider** in the pickers,
  `primary` and `coder` included on an ACP-only daemon; not collapsed, not
  hidden.
- **D8. The ACP path is complete from the configuration** (section 2): a
  declared, enabled agent that a reachable runtime offers. No successful
  check is required.
- **D9. Onboarding ends with an optional Tools step** (section 9.1), which
  offers MCP servers that need no login, from presets kept in the official
  agents repository. The user asked for it on 2026-11-01; the preset list
  is mine.

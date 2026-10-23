# Agent

Agents are Markdown documents with YAML frontmatter. The filename is the agent
ID. An agent defines the system prompt, permission-controlled tool access, its
model fallback chain, and its runtime limits. The harness supplies four agents:
three internal agents plus the synthesized `default` coding agent when no
authored root agent is runnable.

Author agents under `~/.cookie-agent/agents/`. That user directory is the only
place agent documents are read from: a `<cwd>/.cookie-agent/agents/` directory
is never opened for agent documents or presets, and leftover files there are
ignored without a warning. (`AGENTS.md` files there are still read as
[context](#agentsmd-context).)

## Agent document structure

Each file has strict YAML frontmatter and a nonempty Markdown body:

The filename is the agent ID: lowercase letters and digits separated by single
hyphens, starting with a letter or digit, at most 64 bytes. Files are limited to
256 KiB; frontmatter and body are each limited to 128 KiB. Nested maps and lists
have at most 256 entries and depth 16. YAML anchors, aliases, tags, merge keys,
and `${env:` interpolation are rejected. The examples below are complete agent
documents; replace their model IDs with models available in your runtime.

Agent documents do not declare a schema or version. A leftover `schema` field is
a hard error directing the author to remove it; every other unknown field,
wrong type, or malformed YAML construct is also rejected.

```markdown
---
description: Reviews changes for correctness
mode: subagent
enabled: true
models:
  - { model: "openai/gpt-5", variant: null }
limits:
  max_output_tokens: 2048
permissions:
  read: allow
  write: deny
  bash:
    "git diff*": allow
    "*": ask
  delegate: deny
---
Review the requested change and report concrete findings.
```

### Frontmatter keys

| Key | Type | Default | Description |
|---|---|---|---|
| `description` | string | *(required)* | 1–512 bytes, no control characters. Shown in the TUI and snapshots. |
| `mode` | string | *(required)* | `primary`, `subagent`, `all`, or `internal`. |
| `enabled` | boolean | `true` | Disabled agents are never runnable as roots, delegation targets, or internal backends. |
| `models` | array | `[]` | Ordered model chain; must be nonempty for `primary` (unless the document has an `acp` block). Other modes may omit it. |
| `limits` | table | defaults below | Timeouts and token bounds. |
| `permissions` | table | `{}` | Ordered action permission map; see [Permissions](#permissions). At most 256 rules. |
| `agent_md` | boolean | *(unset)* | Load [AGENTS.md context](#agentsmd-context) for this agent's root runs. Unset follows `[agent_md] enabled`; a value overrides it. Rejected on `subagent` and `internal` agents, which never load AGENTS.md. |
| `acp` | table | *(unset)* | Makes the document an ACP agent's; see [ACP agent documents](#acp-agent-documents). |

`max_output_tokens` applies in every mode and defaults to `0` in all of them. A
nonzero value caps each request at the smaller of the document value and the
model's own output limit. For the non-internal `primary`, `subagent`, and `all`
modes, `0` means no document cap: the request sends the model's
`generation_options.max_output_tokens` when one is configured, otherwise the
model's full output limit.

Internal agents run on `${parent_model}` by default, so `0` there means they
inherit the owner run's output cap the same way they inherit its model. The
effective cap for an internal request is the smaller of the model's own output
limit and the first nonzero value of the internal document's
`max_output_tokens` and the owner run's `max_output_tokens`; when both are `0`
the model's default applies as above, and a request is left uncapped only when
the model has neither a default nor a known output limit. An explicit nonzero value in the internal document therefore still
wins over inheritance, subject to that model minimum. All three built-in
internal documents declare `0` and inherit.
`timeout_ms` applies only to internal agents. For other modes, a nonzero value is
a hard error. Internal agents use the configured timeout exactly; zero or an
omitted value uses the 30-second default. The built-in `compaction` agent has a
3-minute default, and an authored `compaction.md` can set its own exact timeout.

Tool visibility is derived only from `permissions`: with no `permissions`
field, no tools are visible. An action's tools become visible when the run's
merged rules (the defaults, the agent document, and the session overlay; see
[Session permission overlays](#session-permission-overlays)) hold any `allow`
or `ask` rule for it — any resource pattern counts, even if a competing deny
wins at execution time — and unmatched
resources deny by default, so approval requires an explicit `ask` rule. `edit`
uses the `write` action. Delegation tools additionally require a `delegate` map
naming at least one eligible target. See
[Permissions](#tool-availability-and-delegation).

The former `tools` field is removed. Documents that still declare it fail
with an error naming `tools` and directing the author to `permissions`; remove
the field and express tool visibility and call policy in the permission map.

The former `model_fallback` field is also removed. Documents that still declare
it fail with an error directing the author to `models`.

The former `limits.max_input_tokens` field is removed. Internal-agent input
budgets now come from each resolved model's input limit, or its context limit
minus its effective output reserve (at most 32,000 tokens or half the context); candidates that cannot fit an invocation are skipped. A model
whose context limit is unknown uses a 16,384-token input budget.

The durable protocol event for advancing through a model chain remains named
`model_fallback` for wire-schema compatibility. This event name is independent
of the agent-frontmatter `models` key.

`models` entries contain `model`, optional `variant`, and optional `cache`.
The `variant` field is optional: omitted (`null`) selects the model's configured
default variant, the string `"base"` selects the base variant explicitly, and any
other string selects that named variant. A primary agent must
declare at least one fallback. The chain may contain up to 256 entries with no
duplicate model keys. Only internal agents may use the `${parent_model}` model
expression, and only without a variant.

`cache` uses the provider-specific shapes documented under
[`[providers.<id>.cache]`](providers.md#prompt-caching). The
resolution order is family default, provider `cache`, then entry-level `cache`
for that binding. This is resolved independently for every fallback, including
mixed-provider chains. OpenAI-compatible cache keys are provider-only.

The entry-level table contains one `anthropic`, `bedrock`, or `openai` shape
matching the binding. It replaces the provider policy rather than merging it.
`cache: {}` explicitly selects no structural strategy; first-party OpenAI and
Azure still send the unconditional session-ID cache key.

### Modes

- `primary` — runnable as a root session agent. Must declare at least one model
  fallback.
- `subagent` — runnable only as a delegation target. Must be enabled to appear in
  another agent's `delegate` map.
- `all` — runnable both as a root and as a delegation target.
- `internal` — engine-only (see below). Cannot be selected as a root or a
  delegation target.

See [System Prompt Composition](../reference/system-prompt.md) for the exact
prompt, skill-listing, plugin, cache, and history assembly order.

### ACP agent documents

An ACP agent a runtime hosts (see [ACP agents](../runtimes/tools.md#acp-agents))
is listed as `<runtime>-<agent>`, and takes its cookie settings from the
document of that name. Its `acp` block, even `acp: {}`, marks the document as
the agent's: it is an agent only while a runtime declares it, and a document
of that name without the block names a daemon agent, which refuses the
declaration.

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
  bash: { "git *": allow, "*": ask }
---
You review changes for this project before they are committed.
```

| Key | Type | Default | Description |
|---|---|---|---|
| `acp.mode` | string | the agent's `auto`, else its default | The mode the agent starts its sessions in, by mode ID. |
| `acp.config` | table of strings | `{}` | Initial values of the agent's config options, by option ID; an unknown option or value is reported in the session and skipped. |
| `acp.hide_tools` | array of patterns | `[read, write, edit, bash, webfetch]` | Cookie's tools the agent does not get through the MCP bridge, with `*` and `?`; `[]` offers every tool the rules offer. |

- The agent chooses its own model, so the document names no `models`; nor
  `agent_md` (the agent reads its own context files), nor `limits`, nor mode
  `internal`. Its body may be empty; a nonempty body is sent with the
  session's first prompt to the agent, after the tool instructions and the
  memory snapshot ([What the first prompt carries](acp-agents.md#what-the-first-prompt-carries)).
- `permissions` decide the calls the agent makes of cookie's tools, as for a
  model, and the agent's own permission requests, whose ACP tool kind maps
  to an action:

  | ACP request | Decided as |
  |---|---|
  | `read`, `search` | `read`, by the location's path |
  | `edit`, `delete`, `move` | `write`, by the location's path |
  | `execute` | `bash`, by the command |
  | `fetch` | `webfetch`, by the URL |
  | a tool of an MCP server the agent connects itself (`mcp__<server>__<tool>`) | `mcp`, by `<server>_<tool>`, the name cookie gives the same tool |
  | a tool of cookie's own MCP server (`mcp__cookie__<tool>`) | allowed at once: the call reaches cookie, whose rules decide it |
  | `think` | allowed |
  | `switch_mode`, any other kind | asks, through the approval flow |

  A request with several locations needs each allowed, so
  `bash: {"git *": allow}` allows the agent's own `git` as it allows
  cookie's `bash` tool, and `mcp: {"exa_*": allow}` covers both cookie's
  and the agent's `exa` tools. Cookie answers with the agent's allow-once
  or reject-once option, never an "always" one.

## AGENTS.md context

Root sessions automatically load AGENTS.md context at the start of every run
unless it is turned off. The run agent's `agent_md` frontmatter decides when it is
set; otherwise the global [`[agent_md]`](../engine/agent_md.md) `enabled` switch
does. The files are read fresh, so edits apply to the next run:

A session in a [project](../concepts/projects.md) first gets the project's
`AGENTS.md`, which the daemon keeps in `~/.cookie-agent/projects/<id>/` and
which needs no runtime, not even `global`. It comes before every workspace's
files, general before specific, so a workspace's `AGENTS.md` still has the
last word, and follows the same switches and size cap.

Every coding runtime the session is attached to then applies the same lookup
to its own workspace (for the built-in `local` runtime, the daemon's working
directory):

1. `.cookie-agent/agents/AGENTS.md` is the default project file. When the run
   uses a preset and `.cookie-agent/agents/<preset>/AGENTS.md` exists, that file
   replaces the default project file.
2. `<workspace>/AGENTS.md` is loaded in addition when present.

Presets themselves are still defined only by the daemon's user configuration.

Missing files add no event or model tokens. Files larger than 2 MiB are skipped
and surfaced as `AgentMdSkipped` warning events. Delegated sessions do not
discover these files for their own runs; explicitly inherited parent text and
forked event prefixes retain their existing behavior. Internal agents never
discover them. Loaded entries are persisted with provenance in one
`agent_md_loaded` event per attachment that returned a file, and one for the
project's file, and replayed
together as one user context turn wrapped in a `<system-reminder>` block, not
as system-prompt text.

Repository-controlled `AGENTS.md` content enters model context automatically.
Treat it as untrusted instructions when opening unfamiliar workspaces and review
the [security guidance](../daemon/security.md#agentsmd-context-files). Configure limits in
[`[agent_md]`](../engine/agent_md.md).

## Subagent tools

`delegate_subagent` requires a short `description`, a self-contained `prompt`,
an allowed `agent_type`, and accepts optional `background`, `resume_session_id`,
and `inherit_context` arguments. `delegate_subagent` is a
[background-capable](../reference/tools.md#background-calls) tool that runs in the
foreground by default: a foreground call blocks and returns the child's first
turn as a concise result teaser. A background call, or a foreground call the
user moves to the background, returns immediately with the child's handle;
admission still waits for any required permission approval. A child is
addressed everywhere by its short tree-unique handle
(`<agent_type>_<8 hex>`, e.g. `explore_1a2b3c4d`) or a full session UUID; a
partial, foreign-tree, or fabricated reference is rejected with a
self-repairing error listing the live candidates. For a
new child, the description becomes the delegated session title. It is truncated
to `session_title.max_chars` using the same Unicode-character limit as generated
titles; invalid title text rejects the delegation.

`resume_session_id` accepts the handle or full UUID of an existing direct child
that was previously created by this same parent session. Top-level, unknown,
foreign, self, and ancestor sessions are rejected. A terminal child starts a new run with the new
prompt; an active child receives the prompt through its pending-input FIFO. The
current delegation link is refreshed, so result, messaging, stop, queue, slot,
and report behavior applies to the resumed work. The existing
session title is never replaced by the new description; the description remains
only the delegation call's display argument. A child that already has a queued
or starting delegation cannot be resumed again until that invocation starts or
terminates; the second resume is rejected without replacing the first.

`inherit_context = true` seeds a newly created child's initial model history
from the parent's assembled history at delegation time. Only user and assistant
text is copied: system content, files, tool calls, and tool outputs/results are
dropped. The retained text is capped at 64 KiB by truncating the oldest content
first. This is a capability and privacy boundary: retained parent text crosses
into the child agent's model context and must be appropriate for that child.
`inherit_context` and `resume_session_id` cannot both be set.

A new child is attached to the same [runtimes](../specs/runtimes.md) as the
root session of its tree, under the same attachment names, and records those
attachments in its own log. A resumed child keeps its own attachments. The child composes its
tools from its own attachments the same way the parent does.

Background sessions move through `queued`, `running`, and a terminal
`completed`, `failed`, `interrupted`, or `cancelled` state.

A subagent reports **every turn it finishes** to its direct parent, whatever
started the turn: the delegation, a resume, `send_message` mail, or its own
background results. The report is a `<subagent_report handle="…"
status="…">` message steered into the parent, carrying a preview of the turn's
final assistant message (see
[`[subagent_output]`](../engine/subagent_output.md)), the status, and the total
line count; it wakes an idle parent. The one exception is the first turn of a
foreground call, whose report is the tool result instead. Each turn reports
exactly once, across retries and restarts; a grandchild reports to its own
parent only.

Use `get_subagent_result` with the child's handle or UUID and zero-based
`offset`/`limit` to read the last assistant message of the child's most recent
turn in pages; a running child (including one just woken by `send_message`)
reports as running and returns no text. It never waits. If a background child
is still running, tell the user that you are waiting and end the turn; its
report will wake this parent session.

A child's running turn appears among the parent's background calls under its
handle. `stop_background` with the handle cancels that turn, whichever turn it
is, and returns once it has ended; its report follows with
`status="cancelled"`. A queued delegation is dropped instead. Stopping an idle
child is an error. The child can still be resumed or messaged afterwards.
Cancelling or interrupting the parent's run does **not** stop its children: a
foreground `delegate_subagent` call it was waiting on ends cancelled, with a
result that says who interrupted the run and that the subagent keeps running
(`The user interrupted this run. ...` for the user), and the child
keeps running and reports its turn as if the call had been moved to the
background; the report still wakes the parent. Result and stop operations reject sessions that are not direct
children of the caller.

The former `steer_subagent` tool is removed. Agents reach children and other
same-tree peers with `send_message`, which is governed by the dedicated
`message` permission action (deny by default) and can also wake finished
sessions. See the [tool reference](../reference/tools.md#agent-messaging) and
the [messaging migration notes](../specs/agent-messaging.md#migration-notes).

Terminal delegated sessions are paged out of memory when their session tree's
resident child count exceeds `delegation.max_resident_subagents` and their last
run has been idle for longer than `delegation.idle_eviction_after`. Each tree
is capped on its own, so one tree's activity never pages out another tree's
children. This is a soft trigger: running,
queued, recently active, pending-input, pending-approval, and not-yet-notified
children remain resident even above the configured count. Eligible children
are selected oldest-idle first. A root session's whole tree pages out once
nobody has used or watched it for 30 minutes. Waiting on a producer alone
(a background call, a subagent, an external producer, or a goal) keeps no
session in memory: when the producer sends, the session reloads and the send
is accepted as if it had stayed. A scheduled wake or a message not yet
delivered still keeps it.

Paging happens only after event appends have been synced and, for background
work, after the parent completion teaser is durable. Session listings retain
lightweight metadata for paged children. Opening one in the TUI, reading its
result, sending it a message, or using `resume_session_id` transparently
reopens its event log and rebuilds the in-memory projection and actor.

On attach, the TUI replays the selected session and the tree's live children
(running or idle). A finished child's history loads when you open it in the
Agents panel, so resuming a tree with many long finished children stays fast.
A finished child that is woken again is picked up on the next tree refresh.

## Layering and replacement

User agent documents and the built-in documents form one registry by agent ID.
The IDs `approval`, `compaction`, and `title` are reserved for internal agents;
an authored document with one of those IDs must use `mode: internal` and
replaces the built-in document completely. The ID `default` is reserved for
the engine-supplied fallback agent and cannot be authored at all.

If no authored agent is runnable as a root, the engine synthesizes the built-in
`default` coding agent bound to the first available model selection. Its prompt
and explicit permission map are fixed by the engine: read is allowed with
additional asks and secret-file denies, while write, bash, and delegate ask by
default. `webfetch`, every runtime's own actions, and every other
undeclared action stay hidden. User
agents do not inherit this list and must declare their own tool permissions.
Two actions are the exception: an agent document, the built-in `default`
included, that names no key for `list_runtimes` or `mcp` gets that action's
rules from its mode (see [Listing runtimes](#listing-runtimes) and
[MCP default](#mcp-default)), so every agent that is not internal is offered
MCP tools and asks before each call. Memory has a default too, by whether
the run is delegated rather than by mode, so the built-in `default` agent
reads every memory and writes the memory related to its session (see
[Memory permissions](#memory-permissions)). `cron` defaults the same way:
a root run may manage cron jobs and a delegated one may not (see
[Cron permissions](#cron-permissions)).

## Agent presets

Agent presets provide named, complete agent sets without duplicating every
shared document. Markdown files directly under `agents/` are shared and are
available when no preset is selected. A directory exactly one level below
`agents/` defines a preset:

```text
~/.cookie-agent/agents/
├── primary.md
├── reviewer.md
├── python/
│   ├── primary.md
│   └── test-writer.md
└── rust/
    ├── primary.md
    └── unsafe-reviewer.md
```

Selecting `python` produces an effective set containing every shared agent,
then fully replaces shared documents whose IDs also exist in `agents/python/`,
and finally adds preset-only IDs such as `test-writer`. Selecting `rust` applies
the same rule independently. Replacement is whole-document replacement: fields,
permissions, model chains, and prompt bodies never merge between same-ID files.

Preset names use the same lowercase alphanumeric and hyphen grammar as agent
IDs, with at most 64 bytes. Presets may add new IDs. The `default` ID remains
non-authorable, and authored `approval`, `compaction`, and `title` documents must
remain internal. Every shared and effective preset set is validated separately,
including delegation targets. Internal built-ins and the synthesized `default`
agent are resolved independently for each effective set.

Only one directory level is supported. Nested directories, non-Markdown entries,
invalid names, malformed documents in unselected presets, and files over 256
KiB are hard configuration errors.

No preset is selected by default. In the TUI, run `/preset` and choose either
`default (shared agents)`, which selects none, or a discovered preset. The choice updates the active root
session's draft for its next run and is also used by `/new` when creating future
root sessions. If the current draft agent is not root-runnable in the new
effective set, the TUI selects that preset's `primary` or first runnable agent.
The choice is in memory only: it is not written to configuration and resets to
`default` when the TUI restarts.

For headless runs, pass the preset explicitly:

```bash
cookie run --preset python --agent primary "Implement the data pipeline"
cookie run --preset rust --agent unsafe-reviewer "Review the FFI boundary"
```

The selected preset is stored in the session's creation selection and is the
default when the session is resumed. Root sessions may select another preset for
any later run; `cookie run --resume-session <id> --preset rust ...` applies
`rust` to that run without rewriting the creation selection. Each run records
its exact preset, agent snapshot, and model bindings in its events, so history
always shows what ran.

A later turn resolves against the live configuration. When the selection it
continues from names a preset, agent, model, or variant that no longer exists,
the turn falls back instead of failing: an unknown preset to the shared agents,
a missing agent to `primary` (or the first root-runnable agent), and a missing
model to the agent's first available model. An explicitly requested agent or
model that does not exist is still an error.

Delegated sessions inherit the preset of the parent run that created them,
including when the parent switched presets after session creation, and later
runs stay on that preset. Each later run looks the delegated agent up by ID in
the live configuration, falling back to the default agent when it is gone.

## Internal agents

The harness runs three internal agents. They are stateless, tool-less model
calls with a strict text-only output contract, and they emit their own event
family (`internal_agent_started`, `internal_agent_finished`, ...).

| ID | Role | Default model | Default limits |
|---|---|---|---|
| `approval` | Stateless approval classifier for `auto_approve` mode | `${parent_model}` | 30 s timeout; model-derived input budget; max output tokens inherit the parent run's cap |
| `compaction` | Summarizes context into a checkpoint | `${parent_model}` | 3 min timeout; model-derived input budget; max output tokens inherit the parent run's cap |
| `title` | Generates a concise session title from the opening user messages (the first `session_title.max_input_messages`, default 4) | `${parent_model}` | 30 s timeout; model-derived input budget; max output tokens inherit the parent run's cap |

All three default to `${parent_model}`, so they run on the model the parent run
is currently using — including its position in the fallback chain: if the run
has fallen back to its second model, internal agents resolve to that second
model too. An internal agent's input budget is derived from each
resolved model's input limit when it declares one, otherwise from its context
limit after reserving its effective maximum output (at most 32,000 tokens or half
the context), with a minimum of one token. A model with an unknown context limit uses a
16,384-token input budget. Agent documents cannot set an input-token cap.

The title agent runs only for root sessions that still need an automatic title.
Delegated sessions already have the `delegate_subagent` description as their
title, so they never invoke the title agent.

The built-in documents are replaced by authored documents with the same ID,
`mode: internal`, and an explicit `models` list. `${parent_model}` is
allowed only in internal agents. When an internal agent document is disabled, or
its fallback chain yields no available model, the internal call fails safely
(approval degrades to asking, compaction is skipped, and title falls back to an
input excerpt) and a failed `internal_agent_finished` event is recorded.

Example replacement — run compaction on a cheaper model while the primary run
keeps its own selection:

```markdown
---
description: Context compaction on a fast model
mode: internal
enabled: true
models:
  - { model: "openai/gpt-5-mini", variant: null }
limits:
  timeout_ms: 30000
  max_output_tokens: 4096
permissions: {}
---
{{default_prompt}}
```

### Internal agent prompt placeholders

The bodies of `approval.md`, `compaction.md`, and `title.md` are templates. Each
built-in body is exactly `{{default_prompt}}`, which expands to that agent's
built-in prompt:

| Agent | `{{default_prompt}}` expands to | Where it is sent | Other placeholders |
|---|---|---|---|
| `approval` | The approval reviewer prompt: when to choose `allow`, `ask`, or `deny`, which kinds of calls need the user, and how to weigh prior decisions. | System prompt | none |
| `compaction` | The structured checkpoint prompt. When the span being compacted starts with an earlier summary, this is the variant that merges it into one updated checkpoint. | Final user message after the history being summarized | `{{focus}}` |
| `title` | The session title prompt: what makes a good title, its length, language, and casing. | System prompt | none |

The output format each agent must follow is not part of `{{default_prompt}}`.
It stays in the request the engine builds, so a custom body cannot break
parsing: approval still asks for `{"decision":"allow"|"deny"|"ask"}` and title
still asks for plain text only.

To keep a default prompt and add your own rules, wrap it:

```markdown
{{default_prompt}}

Network access to github.com for this project is routine; allow read-only requests to it.
```

A body without `{{default_prompt}}` replaces the default prompt entirely.

#### Compaction focus

`{{focus}}` is available only in `compaction.md`.

**How the focus reaches the summarizer.** The focus is the text typed after
`/compact`, plus any instructions added by `session_before_compact` plugins.
Automatic compactions and `/compact` with an empty prompt have no focus.

- If the body contains `{{focus}}`, the focus is inserted there, and only there.
  With no focus, `{{focus}}` becomes empty.
- If the body does not contain `{{focus}}`, the focus is appended after the
  whole expanded body as `User-requested focus: …`. With no focus, nothing is
  appended.

`{{default_prompt}}` itself does not contain `{{focus}}`, and expansion is a
single pass: text inserted by a placeholder is never scanned for more
placeholders. So the built-in body, and any body that uses only
`{{default_prompt}}`, gets the focus appended at the end.

For example, with this body:

```markdown
{{default_prompt}}

Also record the exact name of every failing test and the command that ran it.
```

after `/compact preserve the parser decisions`, the summarizer receives:

```text
<default prompt>

Also record the exact name of every failing test and the command that ran it.

User-requested focus: preserve the parser decisions
```

To choose where the focus goes, place it yourself:

```markdown
{{default_prompt}}

Give extra detail to: {{focus}}
```

#### Syntax and scope

Spaces inside the braces are allowed (`{{ default_prompt }}`). A placeholder
the agent does not accept, such as `{{defaults}}` anywhere or `{{focus}}` in
`approval.md`, is a hard configuration error. Any other `{{`, such as
`{{a b}}`, is literal text. Placeholders are expanded only in the three internal
agent documents. In every other agent document, `{{default_prompt}}` and
`{{focus}}` are neither expanded nor rejected; the model receives them as
literal text.

A session keeps the internal agent prompts frozen when each of its runs
started. A manual `/compact` on a session whose last run started under an
older version uses that run's frozen compaction body as written; the next run
picks up the current documents.

## Selecting an agent

In the TUI, `/new` creates a root session after choosing from agents in the
selected effective set that are runnable as a root (`primary` or `all`, enabled,
with at least one available model). If none are runnable, that effective set's
built-in `default` agent is used.

## Permissions

Agent documents define an ordered permission map keyed by permission action.
The tools Cookie Agent ships use `read`, `write`, `bash`, `delegate`,
`message`, `mcp`, `skill`, `webfetch`, `memory`, and `memory_write`; a runtime's or plugin's tools may
declare actions of their own, such as `browse`, which rules name the same way.
An action is a lowercase ASCII letter followed by up to 31 lowercase letters,
digits, or underscores. A key that
names no tool's action is accepted and simply governs nothing, and `/permissions`
lists it as its own row. Each action is either one bare effect or a
resource-pattern map:

```yaml
permissions:
  read:
    "*": allow
    ".env*": deny
  write: ask
  bash:
    "git status": allow
    "*": ask
  delegate:
    "reviewer": allow
    "*": deny
  mcp:
    "github_*": allow
    github_delete_repo: deny
```

Effects are `allow`, `ask`, and `deny`. A bare effect is equivalent to mapping
`"*"` to that effect. For matching patterns, more literal characters win, then
fewer wildcards, then a rule with a runtime selector over one without, then the
later declaration on an exact tie. Unmatched resources are denied by default.
`ask` remains an explicitly writable effect and routes matching calls through
the existing approval flow; it is never an implicit fallback for an
unmatched resource. The only rules the daemon writes for an agent are the
mode defaults of `list_runtimes` and `mcp` and the run defaults of `memory`,
`memory_write`, and `cron`, each only while the document names no key for
its action.

### Runtime selectors

A key may name a runtime: `action@runtime_id` applies only to tools of
[runtimes](../specs/runtimes.md) with that runtime ID, whatever workspace or
attachment name they have in a session. A plain action applies to every
runtime and to the daemon's base layer, so documents without selectors keep
working unchanged.

```yaml
permissions:
  bash: ask
  bash@labpc:
    "git *": allow
  read@local:
    "${workspace_dir}/*": allow
```

Here `git status` on the `labpc` runtime is allowed while every other command
asks, and reads inside the local runtime's workspace are allowed. A qualified
rule only outranks an unqualified one at the same resource specificity: with
`bash: {"git push *": deny}` and `bash@labpc: {"*": allow}`, `git push` on
`labpc` is still denied. The runtime ID follows `[a-z][a-z0-9_]{0,31}`.
`message` and `skill` belong to the base layer and take no selector.
`delegate@<runtime>` checks a root's `delegate_subagent` call that names that
runtime in `capabilities`, under each runtime it names; a call without
`capabilities` sees only plain `delegate` rules (see
[Delegating to runtimes](../sessions/delegation.md#limiting-where-the-root-delegates)). A selector also limits visibility: with only `bash@labpc` rules,
`bash` tools of other runtimes are not published.

### Resource labels

Tool providers publish a static permission name and an optional resource label:

| Tool | Permission name | Resource label |
|---|---|---|
| `read` | `read` | Filesystem path |
| `write`, `edit` | `write` | Workspace-relative path inside the workspace; absolute path outside it |
| `bash` | `bash` | Complete command string |
| `delegate_subagent` | `delegate` | Target `agent_type` |
| `get_subagent_result` | `delegate` | Owned subagent's `agent_type` |
| `stop_background` | none | Needs no permission |
| `send_message` | `message` | Recipient's relationship to the sender: `parent`, `child`, `sibling`, or `*` |
| `<server>_<tool>` | `mcp` | The complete generated MCP tool name |
| `skill` | `skill` | Skill name |
| `goal_get`, `goal_update` | none | None: published only while the session has an active or paused goal, and always allowed |
| `read_artifact` | none | None: always sent and always allowed, because it reads only the session tree's own retained output |
| `webfetch` | `webfetch` | Initial URL as given, including its query string |
| `memory_search` | none | None: published with `memory_read`; each result is checked against the `memory` rules instead (see [Memory permissions](#memory-permissions)) |
| `memory_read` | `memory` | Memory URL, such as `project://cookie-agent/build` |
| `memory_write`, `memory_edit`, `memory_delete` | `memory_write` | Memory URL |
| `cron_create`, `cron_update`, `cron_delete`, `cron_run` | `cron` | The target kind of each job the call touches: `session`, `subagent`, or `new`; a `cron_update` that changes the kind is checked for both, the strictest deciding |
| `cron_list` | none | None: published only beside another cron tool; it lists only the jobs whose kind the `cron` rules allow or ask for |
| `list_agents` | none | None: published exactly when the run is offered `delegate_subagent` or a cron tool other than `cron_list` |

The `edit` tool uses the `write` permission action. Bash is not parsed into file
operations: `cat .env` is controlled by `bash`, not `read`, and a pattern such
as `git *` also matches a longer command beginning with `git`. See
[Security guarantees](../daemon/security.md) for the platform-specific filesystem and
process boundaries behind these tools.

Path resource labels are normalized lexically from the single path requested by
the caller. They are not canonicalized through symbolic links before permission
matching. That one label authorizes the whole prepared `read`, `write`, or
`edit` operation: if an allowed alias resolves outside the workspace, cookie
agent does not perform a second permission check or require a rule for the
resolved destination. Filesystem preparation resolves the route once and
execution stays on it: changing a link target after preparation does not
redirect the operation, and policy is not reevaluated against the new
destination.

Bash remains independent of this path-resource contract. An allowed command has
only its complete command-string resource and does not gain hidden `read` or
`write` resource checks when the command traverses a link.

Permission policy does not validate Cookie Agent's pre-existing private-state
paths. New state is created with owner-only permissions, but existing loose,
foreign-owned, hard-linked, or symlinked state is read and written as-is. This is
part of the local storage threat model, not an `allow`/`ask`/`deny` decision.

MCP checks are always scoped. A rule such as `"github_*": allow` covers every
tool from that generated server prefix, while a more-specific deny can override
one tool. An unmatched MCP tool is denied and not offered; with no `mcp` key
at all, the [MCP default](#mcp-default) `"*": ask` matches every MCP tool.

When a tool has no resource label, only the permission's bare effect or `"*"`
rule applies. Specific patterns are inapplicable rather than matching or
denying. If neither a bare effect nor `"*"` exists, the normal unmatched result
is `deny`.

`${workspace_dir}` is allowed only in `read` and `write` patterns and expands
against the workspace of the runtime that prepared the call; relative path
labels resolve against it too. Base-layer and global-runtime tools have no
workspace, so `${workspace_dir}` and relative-path patterns never match them. Ordinary absolute patterns
such as `/etc/*` control outside-workspace paths. Permission patterns do not
expand environment variables; only a leading `~` expands (see
[Paths under the home directory](#paths-under-the-home-directory)). A `*` matches across directory separators, so
`"${workspace_dir}/*"` already covers nested paths such as
`${workspace_dir}/crates/engine/src/lib.rs`; no `**` pattern is needed.

Permission evaluation has no implicit filename or resource-name exceptions.
Credential files, `.env` and `.env.*`, and non-file resource labels all use the
configured rules for their action, including in session overlays. Broad file
allows such as `"*": allow` or
`"${workspace_dir}/*": allow` apply without a protected-file override. Explicit
`deny` and `ask` rules follow the same specificity as any other file, across
the agent document and the session overlay alike; unmatched resources are
denied.

The synthesized `default` agent still declares explicit dotenv read denies and
`.env.example` allows in its permission map. These are ordinary policy rules,
not an implicit guard applied to authored agents or session overlays. The same
applies to its explicit credential-file denies (such as `store-v3.json`,
`id_*`, `.netrc`, and `application_default_credentials.json`).

This contract governs tool permission evaluation. It does not replace filesystem
integrity and resource-binding checks, or govern engine configuration and prompt
loading (such as workspace `AGENTS.md` admission). Generic approval modes and
turn-scoped skill grants retain their documented behavior; they are not
resource-name exceptions.

### Paths under the home directory

A pattern that is `~` or starts with `~/` names the home directory of the
runtime the tool runs on, as the runtime reported it (see
[Home](../reference/runtime-protocol.md#home)): on a runtime whose home is
`/home/ada`, `~/notes/*` is `/home/ada/notes/*` and `~` is `/home/ada`.
Each runtime expands it against its own home, so one rule covers the home on
every machine, and a [runtime selector](#runtime-selectors) still limits it
to one runtime:

```yaml
permissions:
  read:
    "~/notes/*": allow
  read@labpc:
    "~/.ssh/*": deny
```

The expanded pattern is an ordinary absolute pattern: it governs paths
outside the workspace, whose labels are absolute, and ranks by its expanded
length. A path argument's own leading `~` expands against the same home, so
`read("~/notes/todo.md")` gets the label `/home/ada/notes/todo.md`. On a
runtime that reports no home (the base layer included) the pattern stays
literal and matches no absolute path; `~user/…` never expands. Windows homes
are spelled with `/`, as path labels are: `C:\Users\ada` makes `~/x`
`C:/Users/ada/x`.

### Tool availability and delegation

Tools are opt-in. A permission action must have at least one `allow`
or `ask` rule in the run's merged rules (defaults, agent document, and session
overlay) before that action's tools are visible. Any resource pattern counts,
not just `"*"`. Visibility does not resolve rule precedence: an overlay `deny`
hides a tool only when it replaces every `allow` or `ask` rule for that action,
so `bash: {"rm *": deny}` in the overlay leaves `bash` offered while the agent
allows `"*"`. An action with no rule at any level, or with only `deny` rules,
advertises no tools for any provider. The most specific matching rule decides
individual calls once tools are visible.

For example, this agent exposes read, write/edit, and bash with granular write
and command policies while leaving delegation hidden; it names no `mcp` key,
so it also gets the [MCP default](#mcp-default) and asks before every MCP
call:

```markdown
---
description: Workspace implementation agent
mode: primary
enabled: true
models:
  - { model: "openai/gpt-5", variant: null }
permissions:
  read: allow
  write:
    "src/*": ask
  bash:
    "cargo test*": allow
    "cargo fmt*": allow
---
Implement and verify requested workspace changes.
```

MCP tools follow the same rule per tool: an MCP tool is offered only when
the merged rules do not deny `mcp` for its name, which the
[MCP default](#mcp-default) `"*": ask` covers when the document names no
`mcp` key; `mcp: deny` hides them all, and `"github_*": allow` offers only
`github`'s tools (see [MCP tool names](mcp.md#tool-names-and-permissions)). Delegation tools additionally require the `delegate` map to name at least one
eligible target with `allow` or `ask`. There is no separate tool allowlist.

There is also no separate MCP server approval prompt or trust store. Configured,
enabled servers connect eagerly, while
the agent's `mcp` map remains the sole visibility and call gate. Project MCP
configuration and project agent documents are version-controlled content
equivalent to code: a repository can ship both a server definition and an agent
that permits it. Review them and work only in repositories you trust.

Delegation targets come from the keys in the `delegate` permission map and must
resolve to enabled `subagent` or `all` agents. A named target no agent has
yet, and a pattern that matches none, do not fail the configuration: the
name may be an [ACP agent](acp-agents.md) whose runtime has not
reported it, such as a dial-in runtime that has not connected since the
daemon started. Until one does, the configuration status carries a
diagnostic (`cookie config status`, `/config`, and the web's configuration
page show it), the agent is not among the targets, and delegating to it is
refused with "no agent `<id>` (its runtime is not connected)". A typo stays
a diagnostic. A target whose document exists but is disabled or of mode
`primary` still fails the configuration. This action controls
`delegate_subagent` and `get_subagent_result`. Both tools use the target
subagent's agent type as their permission resource.
Result retains its existing ownership and argument
validation, and permission evaluation uses that agent type. Their approval
display still shows the owned `session_id`; display text is independent of the
permission resource.

This changes existing mapped delegation policies. For example,
`delegate: {reviewer: allow, "*": deny}` allows `delegate_subagent` targeting
`reviewer`, but denies `get_subagent_result` for other agent types. Existing tree grants issued for their
former identities do not carry over.
Runtime `delegation.max_depth` defaults to 3 and `max_concurrency` defaults to 4.

`delegate_subagent` replaces the former `delegate` tool name without an alias.
Old tool calls and prepared-operation grants therefore fail closed. The
`delegate` spelling above remains the permission action, not a tool alias.

Agent-to-agent messaging is separate from delegation. The removed
`steer_subagent` tool is replaced by `send_message`, which uses the `message`
action with relationship labels (`parent`, `child`, `sibling`, `*`) rather than
agent types, so a `delegate` grant no longer authorizes steering. The tool is
hidden until a `message` rule allows or asks; agents that previously steered
children need an explicit rule such as `message: { "child": allow }`. The
recipient argument is `to`; the earlier `recipient_session_id` spelling has no
alias, and prepared-operation grants issued under the old argument identity
fail closed. See the
[messaging migration notes](../specs/agent-messaging.md#migration-notes).

### Memory permissions

Every agent that is not internal has a memory file, `agent://<agent>`, which
the daemon makes and whose description and keywords it keeps in line with the
agent document: the description names the agent and repeats the document's
`description`, and the keywords are the agent ID and its mode. See
[Agent memory](../sessions/memory.md#agent-memory).

The memory tools of the `global` runtime use two actions, `memory` for
searching and reading and `memory_write` for writing, editing, and deleting,
both with the memory URL as resource (`user://<category>`,
`agent://<agent>`, `machine://<machine>/<name>`,
`project://<project>/<name>`). `memory: allow` lets an agent read every
memory, per-URL rules narrow it, and `memory_write` rules let it write. A
session overlay can grant or deny either for one session tree, as for any
action: `memory: deny` in `/permissions` turns memory off for that session.

**Root runs.** A root run of an agent that is not internal, the built-in
`default` included, gets these rules for each of `memory` and
`memory_write` its document names no key for:

```yaml
memory: allow
memory_write:
  "user://*": allow
  "agent://${agent_id}": allow
  "machine://${session_machine}/*": allow
  "project://${session_project}/*": allow
```

It reads every memory, with the memory related to the session in its
snapshot, and writes that related memory: the user's, its own agent file,
and its session's machines and project. Another project's or machine's
memory it can find with `memory_search` and read, but not change. A declared
key replaces the default for its action only, so `memory_write: deny` leaves
a run that reads, and `memory: deny` turns memory off. Internal agents get
no default.

**Variables.** A `memory` or `memory_write` pattern may name the run's agent,
machines, and project, so one document works wherever it runs:

| Variable | Stands for | Example |
|---|---|---|
| `${agent_id}` | the run's agent | `agent://${agent_id}` |
| `${session_machine}` | each machine the session's attachments run on | `machine://${session_machine}/*` |
| `${session_project}` | the session's project | `project://${session_project}/*` |

A variable must be a whole URL segment (`project://${session_project}-old/*`
is refused), it is valid only in `memory` and `memory_write` patterns of an
agent document, and an unknown variable fails the document to load. Session
overlay rules take no variables. Variables are expanded when a run starts and
its policy is frozen, so a session that gains an attachment or moves to a
project is covered from its next run. `${session_machine}` makes one rule per
machine, and a variable that stands for nothing (`${session_project}` in a
session without a project, or a session with no machine) drops its rule, so
it matches no memory. `agent://self`, the old spelling for the run's own
agent, is gone: it is now an ordinary pattern, and `${agent_id}` replaces it.

**Delegated runs.** A delegated run (a subagent started by
`delegate_subagent`) whose agent document has no key for `memory`, or none for
`memory_write`, gets `agent://${agent_id}: allow` for that action: it sees and
maintains its own agent file and nothing else. The default depends on whether
the run is delegated, not on the agent's mode, so an `all` agent has the root
default as a root and this one when delegated. A document that declares either action gets
exactly its own rules for it, delegated or not; there is no hard limit
behind the default, so a subagent granted `user://*` may write it.

**The write tools.** `memory_write`, `memory_edit`, and `memory_delete`, and
the rubric that tells the agent how to keep memory, are offered only when some
memory URL is both readable (an `allow` or `ask` `memory` rule matches it) and
writable (an `allow` or `ask` `memory_write` rule matches it). `deny` rules
are not considered in this check. A run that only reads gets `memory_search`
and `memory_read`. Beyond the rules, nothing limits what an agent may write,
edit, or delete; the rubric says what it should, and `ask` rules put a
reviewer or you in front of the rest.

A worked agent document: a reviewer that keeps its own file, reads the
memory of its session's project and machines, and writes to those two only
after asking, and that never sees the user's memory.

```markdown
---
description: Reviews a diff for bugs and missing tests
mode: all
models:
  - { model: "openai/gpt-5", variant: null }
permissions:
  memory:
    "user://*": deny
    "agent://${agent_id}": allow
    "project://${session_project}/*": allow
    "machine://${session_machine}/*": allow
  memory_write:
    "agent://${agent_id}": allow
    "project://${session_project}/*": ask
    "machine://${session_machine}/*": ask
---
You review a diff for bugs and missing tests.
```

Run as a root, this agent's snapshot shows its own file and the session's
project and machine memories, it can write its own file freely, and the
others after approval. Root or delegated, it has these same rules rather
than a default, since it declares both keys; without them it would read and
write the user's memory as a root. A subagent that should only keep its own
file needs no rules at all: the delegated default covers it.

Memory needs both the `global` runtime and a visible `memory` action: an
agent without one gets no memory tools at all, even with `memory_write`
rules.

The `memory` rules also decide what the memory snapshot shows of each
memory: one they allow is shown, one they ask for is listed by URL and
description only (reading it with `memory_read` then asks), and one they
deny or do not match is left out, not even counted. When nothing is left to
show, the run gets no memory turn at all, while the tools stay offered. See
[What the model sees](../sessions/memory.md#what-the-model-sees) and the
[memory spec](../specs/memory.md#9-permissions).

### Cron permissions

The cron tools of the `global` runtime (`cron_create`, `cron_list`,
`cron_update`, `cron_delete`, and `cron_run`; see
[Scheduling prompts](../sessions/scheduling.md)) use one action, `cron`,
whose resource is the target kind of the job a call touches:

| Resource | A job that fires into |
|---|---|
| `session` | a root session (`"self"` from a root) |
| `subagent` | a delegated session (`"self"` from a subagent, or a subagent's handle) |
| `new` | a new root session per fire, from a template |

One action covers listing, creating, editing, enabling and disabling,
deleting, and running a job now. Creating checks the new job's kind;
updating checks the job's current kind and, when it changes, the new one;
deleting and running check the job's kind. `cron_list` has no action of its
own and shows only the jobs whose kind the rules allow or ask for. An agent
manages every job its rules allow, whoever created it.

**Defaults.** When the agent document has no `cron` key:

| Run | Default |
|---|---|
| Root, any agent but an internal one | `cron: allow` |
| Delegated | none: the tools are hidden |
| Internal agent | none |

As with memory, the default follows whether the run is delegated, not the
agent's mode. A declared key replaces the default, so `cron: deny` hides the
tools from a root and this grants a subagent jobs into its own session and
the sessions below it:

```yaml
permissions:
  cron:
    subagent: allow
```

A session overlay can grant or deny `cron` for one session tree, as for any
action: `cron: deny` in `/permissions` hides the cron tools for that session.

**Hard limits.** Beyond the rules:

- an agent's job may target only its own session or a session below it in
  its tree; any other session is refused in the tool result, whatever the
  rules say (you can target any session from a client);
- the tools have no permission-mode or model parameter: a new-session job an
  agent creates runs in `auto-approve-n` with its agent's own models, and
  only a client can set another mode or pin a model (an agent's update keeps
  what you set);
- a new-session job's agent must be one that can run as a root (`primary` or
  `all`).

A new-session job's template may add any permission rules to its run
sessions, so `cron` on `new` lets an agent start sessions of any agent with
more rights than its own. Grant it to a subagent deliberately.

**`list_agents`.** Whenever a run is offered `delegate_subagent` or a cron
tool other than `cron_list`, the `global` runtime also offers `list_agents`,
which has no action of its own: for `delegate` it lists the run's delegation
targets, for `cron` every enabled `primary` or `all` agent (the ones a
new-session job can run), each with its mode, description, presets, models,
and the permission keys its document declares; the actions left to the
daemon's defaults share one `defaults:` line, and the defaults are listed
once at the end. See
[`list_agents`](../reference/tools.md#list_agents).

### Listing runtimes

`list_runtimes`, a tool of the `global` runtime, lists the daemon's machines
and runtimes for a root that delegates to them (see
[Delegating to runtimes](../sessions/delegation.md#list_runtimes)). Its
action, `list_runtimes`, has a default by mode:
`allow` for `primary` and `all` agents, and none for `subagent` and
`internal` ones. A declared `list_runtimes` key, with or without a runtime
selector, replaces the default: `list_runtimes: allow` lets a subagent see
the list, and `list_runtimes: deny` hides it from a primary agent.

### MCP default

MCP tools use the `mcp` action, with the complete generated tool name as
resource (see [MCP tool names](mcp.md#tool-names-and-permissions)). When an
agent document, the built-in `default` included, has no `mcp` key (in any
form, `mcp@<runtime>` included), the daemon supplies `mcp: {"*": ask}` for
`primary`, `all`, and `subagent` agents, so the connected servers' tools are
offered and every call asks; `internal` agents get none. A declared `mcp`
key replaces the default entirely:

```yaml
permissions:
  mcp:
    "my_chrome_*": ask
```

offers only the `my_chrome` server's tools, `mcp: allow` runs every MCP tool
without asking, and `mcp: deny` hides them all. A key with only a runtime
selector, such as `mcp@labpc: allow`, declares the action too, so the
other runtimes' MCP tools stay hidden. `/permissions` shows the default with
the source `default`. Session overlays apply on top as for any action, so
`mcp: deny` in `/permissions` hides the MCP tools for one session.

### Web fetching

Web access uses the `webfetch` action, matched against the exact initial URL as
requested, including its query string:

```yaml
permissions:
  webfetch:
    "*https://*.quantumcookie.xyz/*": allow
    "https://review.example.org/*": ask
```

Paging a long response back with `read_artifact` needs no rule.
Permission is checked once, against the initial URL; redirect destinations are
not checked, and there is no SSRF protection, host/IP blocklist, or DNS
pinning, so scope patterns accordingly, including access to local network
services. Requests inherit the cookie-agent process's proxy environment and
have a 30-second timeout. See the [tool reference](../reference/tools.md#webfetch)
for the output format, HTML rendering, the download cap, and result paging.

### Live permission modes

Each session tree starts in `auto_approve` unless changed. The mode is runtime
only, is keyed by the tree root, and can be changed through any session in the
tree. Every client connected to the daemon is told when it changes; the TUI
updates its bottom bar and names a change made by another client:

- `auto_approve` runs the stateless approval classifier and asks the user when
  it escalates or fails safely.
- `auto_approve_n` runs the same classifier but rejects an escalation on the
  user's behalf without showing a prompt.
- `auto_approve_y` runs the same classifier but approves an escalation on the
  user's behalf without showing a prompt. This is an approve-once decision and
  does not create a lasting session-tree grant.
- `ask` skips the classifier and routes policy asks and model-requested
  approvals to the user.
- `yolo` approves asks immediately.

Hard denies, the doom-loop guard, and existing tree grants are evaluated before
the mode shortcut. Changing a mode does not alter an already pending approval
but does apply to subsequent approvals in every delegated descendant. As a
result, root trees in `ask` or `yolo` now gate descendant tool calls with that
same mode.

When one model turn contains multiple tool calls, the engine computes every
permission decision before executing any call. Policy asks and model-requested
approvals are then resolved one at a time in model order. Auto-allowed calls wait
for every ask in the batch to resolve, so no tool side effect begins while a
sibling approval is pending. Denied or rejected calls fail independently; after
the approval pass, surviving parallel-eligible calls are dispatched together.

Mode-decided escalations use the `permission_mode` decision source in the audit
trail. Their final reason codes are `auto_approve_n_rejected` and
`auto_approve_y_approved`; the classifier's preceding evaluation remains an
internal-agent escalation.

### Session permission overlays

Run `/permissions` to edit the selected session's permission overlay. The
panel lists every action with its effective `allow`, `ask`, or `deny` effect,
and each action's resource patterns hang beneath it. The source column marks
where a rule comes from: `session` (an overlay rule), `agent` (the agent
document), or `default` (the daemon's default for the action, or no rule
covers it). The table is the run's merged rule list (see below). The selected rule is
explained in plain words under the table, and the header shows the session
tree's permission mode; `m` cycles it, like clicking the mode in the bottom bar.

Up/down (or the mouse) selects a rule. Left/right steps its effect through
allow, ask, and deny, or click a segment of the selected rule's effect; changing
an agent or default rule adds a session rule with the same pattern. `n` (or the
`new session rule` row) opens a form for the action, pattern, and effect: Tab
moves between fields, left/right changes the action or effect, and Enter adds
the rule once the pattern validates as a wildcard pattern. Enter on a session
rule edits it in the same form, and `d` or Delete removes it. Agent and default
rules cannot be removed, only overridden. A rule limited to one runtime
(`bash@labpc`) shows `@labpc` before its pattern, and editing it keeps the
runtime; the TUI form does not set a runtime on a new rule, which the web
client's form and the agent document can. The editor does not accept
freeform YAML.

Before the first prompt of a new session (the draft `cookie` starts on, or one
opened with `/new`), `/permissions` edits the draft instead: the table previews
the chosen agent's rules with the draft's rules applied, and the title reads
`Permissions · new session`. The draft's rules and permission mode are sent with
the session's creation, so its first run already uses them; a session that was
selected before `/new` is never changed.

Overlay rules accept the same runtime selector as agent documents
(`session.permission.set` and `session.permission.clear` take an optional
`runtime`), and a rule's selector is part of its identity: `bash` and
`bash@labpc` with the same pattern are two rules.

The overlay merges with the run's frozen rules (the agent document's, and the
defaults of each action it names no key for) into one list. An overlay rule
with the same action, runtime selector, and pattern as a frozen rule replaces
it (patterns compare with the memory variables expanded and a leading `~`
expanded against the runtime's home); every other overlay rule is added after
the frozen rules. That list decides which tools are offered, every call (the
most specific matching rule wins, and at equal specificity the overlay rule,
which comes later), permission-name-only checks (a `*` rule, one with a
runtime selector before a bare one), the memory tools and snapshot, and what
`/permissions` shows. A resource no rule matches is denied, and an action with
no rule at any level has no visible tools. For example, with the agent's
`bash: {"*": ask, "git push *": deny}`:

- an overlay `bash: {"*": allow}` replaces the `*` row: commands run without
  asking, but `git push` stays denied, because the document's pattern is more
  specific;
- an overlay `bash: {"git push *": allow}` replaces the document's deny:
  `git push` runs and other commands still ask;
- an overlay `cron: deny` or `mcp: deny` replaces that action's default and
  hides its tools.

A session `*` rule replaces only the `*` row in the table; the document's
other patterns stay listed with their source, because they still apply.
Before 2026-10-23 any matching overlay rule beat every agent rule, whatever
the patterns, and an overlay `deny` never hid a tool the agent offered.

Changes affect
subsequent visibility and permission evaluations only. They do not rewrite an
active run's frozen agent/model
identity or retroactively cancel an operation already executing. A pending
tree-approval response is rejected as changed if the session overlay changed
after that approval was requested, so it cannot commit a stale durable grant.

Every change appends a complete `session_permission_overlay_set` event to the
session log. Overlay state therefore survives daemon restart and follows normal
revert and fork branch semantics. Tightening a rule durably invalidates existing
tree grants for that action under the session root before the overlay event is
committed. A change counts as tightening when it makes the rule stricter for
the base layer or any runtime the session is attached to, each evaluated
against that runtime's workspace. Invalidation is action-wide because tree-grant
records retain opaque prepared identities rather than normalized resource
labels; a qualified rule only invalidates grants of its runtime ID.

Tree grants record the runtime ID of the tool they were approved for and only
apply to calls on that runtime ID; base-layer grants record none. Grants and
approval requests written before runtimes existed carry no runtime ID and are
read as `local`.

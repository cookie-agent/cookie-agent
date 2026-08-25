# Agents

Agents are Markdown documents with YAML frontmatter. The filename is the agent
ID. An agent defines the system prompt, permission-controlled tool access, its
model fallback chain, and its runtime limits. The harness supplies four agents:
three internal agents plus the synthesized `default` coding agent when no
authored root agent is runnable.

## Agent document structure

Each file has strict YAML frontmatter and a nonempty Markdown body:

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
| `description` | string | *(required)* | 1–512 characters, no control characters. Shown in the TUI and snapshots. |
| `mode` | string | *(required)* | `primary`, `subagent`, `all`, or `internal`. |
| `enabled` | boolean | *(required)* | Disabled agents are never runnable as roots, delegation targets, or internal backends. |
| `models` | array | *(required for `primary`)* | Ordered model chain; see below. |
| `limits` | table | defaults below | Timeouts and token bounds. |
| `permissions` | table | `{}` | Ordered action permission map; see [Permissions](permissions.md). At most 256 rules. |

`max_output_tokens` applies in every mode. A nonzero value caps each request at
the smaller of the document value and the model's own output limit. It defaults
to no document cap for the non-internal `primary`, `subagent`, and `all` modes.
Internal agents retain a 2,048-token default; setting it explicitly to zero
removes that document cap.
`timeout_ms` applies only to internal agents. For other modes, a nonzero value is
a hard error. Internal agents use the 30-second invocation timeout when
`timeout_ms` is zero or omitted.

Tool visibility is derived only from `permissions`, and user agents must opt in
to each action they need. With no `permissions` field, no tools are visible. An
action becomes visible when it has at least one effective `allow` or `ask` rule;
`edit` uses the `write` action. Delegation tools additionally require a
`delegate` map naming at least one eligible target. A bare action deny hides
that action's tools. A mapped action with `"*": deny` also hides them unless it
contains a more specific `allow` or `ask` exception.

The former `tools` field is removed. Documents that still declare it fail
with an error naming `tools` and directing the author to `permissions`; remove
the field and express tool visibility and call policy in the permission map.

The former `model_fallback` field is also removed. Documents that still declare
it fail with an error directing the author to `models`.

The former `limits.max_input_tokens` field is removed. Internal-agent input
budgets now come from each resolved model's context limit minus its effective
output reserve; candidates that cannot fit an invocation are skipped. A model
whose context limit is unknown uses a 16,384-token input budget.

The durable protocol event for advancing through a model chain remains named
`model_fallback` for wire-schema compatibility. This event name is independent
of the agent-frontmatter `models` key.

`models` entries are `{ model = "<provider>/<model-id>", variant = <name|null|"base"> }`.
The `variant` field is optional: omitted (`null`) selects the model's configured
default variant, the string `"base"` selects the base variant explicitly, and any
other string selects that named variant. A primary agent must
declare at least one fallback. The chain may contain up to 256 entries with no
duplicate model keys. Only internal agents may use the `${parent_model}` model
expression, and only without a variant.

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

## AGENTS.md context

Root sessions automatically load AGENTS.md context at the start of every run. The
files are read fresh, so edits apply to the next run:

1. `.cookie-agent/agents/AGENTS.md` is the default project file. When the run
   uses a preset and `.cookie-agent/agents/<preset>/AGENTS.md` exists, that file
   replaces the default project file.
2. `<cwd>/AGENTS.md` is loaded in addition when present.

Missing files add no event or model tokens. Delegated sessions do not discover
these files for their own runs; explicitly inherited parent text and forked event
prefixes retain their existing behavior. Internal agents never discover them.
Loaded entries are persisted with provenance in `agent_md_loaded` and
replayed as one user context turn, not as system-prompt text.

Repository-controlled `AGENTS.md` content enters model context automatically.
Treat it as untrusted instructions when opening unfamiliar workspaces and review
the [security guidance](security.md#agentsmd-context-files). Configure limits in
[`[agent_md]`](../reference/configuration.md#agent_md).

## Subagent tools

`delegate_subagent` requires a short `description`, a self-contained `prompt`,
an allowed `agent_type`, and accepts optional `background`, `resume_session_id`,
and `inherit_context` arguments. Foreground calls block and return a concise
result teaser. Background calls return immediately with only the child
`session_id`; admission still waits for any required permission approval. For a
new child, the description becomes the delegated session title. It is truncated
to `session_title.max_chars` using the same Unicode-character limit as generated
titles; invalid title text rejects the delegation.

`resume_session_id` attaches an existing direct child that was previously
created by this same parent session. Top-level, unknown, foreign, self, and
ancestor sessions are rejected. A terminal child starts a new run with the new
prompt; an active child receives the prompt through its pending-input FIFO. The
current delegation link is refreshed, so result, steer, cancel, queue, slot, and
completion-notification behavior applies to the resumed work. The existing
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

Background sessions move through `queued`, `running`, and a terminal
`completed`, `failed`, `interrupted`, or `cancelled` state. Completion appends a
parent event containing the session ID, status, first 20 result lines (at most 2
KiB), and total line count. Use `get_subagent_result` with `session_id`, optional
`wait`, and zero-based `offset`/`limit` to retrieve the full result in pages. Use
`steer_subagent` with the owned `session_id` and a non-empty `message` to add a
user turn to a running or queued child. Running children promote steer messages
FIFO after the current tool batch or at the next completion boundary. A queued
child persists the message before it has a run and promotes it after its initial
model response when the queue starts it. Use `cancel_subagent` with the owned
`session_id` and optional `reason` to cancel it. Result, steer, and cancellation
operations reject sessions that are not direct children of the caller, and
steering rejects terminal children.

Terminal delegated sessions are paged out of memory when resident child count
exceeds `delegation.max_resident_subagents` and their last run has been idle for
longer than `delegation.idle_eviction_after`. This is a soft trigger: running,
queued, recently active, pending-input, pending-approval, and not-yet-notified
children remain resident even above the configured count. Root sessions are
never evicted. Eligible children are selected oldest-idle first.

Paging happens only after event appends have been synced and, for background
work, after the parent completion teaser is durable. Session listings retain
lightweight metadata for paged children. Opening one in the TUI, reading its
result, steering it, or using `resume_session_id` transparently reopens its event
log and rebuilds the in-memory projection and actor.

## Layering and replacement

User-layer and workspace-layer agent directories merge into one registry by
agent ID. A same-ID workspace agent replaces the user agent completely. The IDs
`approval`, `compaction`, and `title` are reserved for internal agents; an
authored document with one of those IDs must use `mode: internal` and replaces
the built-in document through normal layering. The ID `default` is reserved for
the engine-supplied fallback agent and cannot be authored at all.

If no authored agent is runnable as a root, the engine synthesizes the built-in
`default` coding agent bound to the first available model selection. Its prompt
and explicit permission map are fixed by the engine: read is allowed with
additional asks and secret-file denies, while write, bash, and delegate ask by
default. MCP remains omitted. User agents do not inherit this list and must
declare their own tool permissions.

## Agent presets

Agent presets provide named, complete agent sets without duplicating every
shared document. Markdown files directly under `agents/` are shared and are
available when no preset is selected. A directory exactly one level below
`agents/` defines a preset:

```text
.cookie-agent/agents/
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
`None (shared)` or a discovered preset. The choice updates the active root
session's draft for its next run and is also used by `/new` when creating future
root sessions. If the current draft agent is not root-runnable in the new
effective set, the TUI selects that preset's `primary` or first runnable agent.
The choice is in memory only: it is not written to configuration and resets to
shared when the TUI restarts.

For headless runs, pass the preset explicitly:

```bash
cookie run --preset python --agent primary "Implement the data pipeline"
cookie run --preset rust --agent unsafe-reviewer "Review the FFI boundary"
```

The selected preset is stored in the session's creation selection and is the
default when the session is resumed. Root sessions may select another preset for
any later run; `cookie run --resume-session <id> --preset rust ...` applies
`rust` to that run without rewriting the creation selection. Each run persists
its exact preset, agent snapshot, and model bindings, so replay does not consult
the live preset registry.

Delegated sessions are different: they inherit the preset from the parent run
that created them, including when the parent switched presets after session
creation. Their agent is resolved and frozen from that effective set, and later
runs of the delegated session remain pinned to the inherited preset.

## Internal agents

The harness runs three internal agents. They are stateless, tool-less model
calls with a strict text-only output contract, and they emit their own event
family (`internal_agent_started`, `internal_agent_completed`, ...).

| ID | Role | Default model | Default limits |
|---|---|---|---|
| `approval` | Stateless approval classifier for `auto_approve` mode | `${parent_model}` | 30 s timeout; model-derived input budget; 2,048 max output tokens |
| `compaction` | Summarizes context into a checkpoint | `${parent_model}` | 30 s timeout; model-derived input budget; 2,048 max output tokens |
| `title` | Generates a concise session title from the opening user messages (the first `session_title.max_input_messages`, default 4) | `${parent_model}` | 10 s timeout; model-derived input budget; 128 max output tokens |

All three default to `${parent_model}`, so they run on the model the parent run
is currently using. An internal agent's input budget is derived from each
resolved model's context limit after reserving its effective maximum output,
with a minimum of one token. A model with an unknown context limit uses a
16,384-token input budget. Agent documents cannot set an input-token cap.

The title agent runs only for root sessions that still need an automatic title.
Delegated sessions already have the `delegate_subagent` description as their
title, so they never invoke the title agent.

The built-in documents are replaced by authored documents with the same ID,
`mode: internal`, and an explicit `models` list. `${parent_model}` is
allowed only in internal agents. When an internal agent document is disabled, or
its fallback chain yields no available model, the internal call fails safely
(approval degrades to asking, compaction is skipped, and title falls back to an
input excerpt) and an `internal_agent_failed` event is recorded.

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
  max_output_tokens: 2048
permissions: {}
---
Summarize conversation context faithfully within the supplied bounds. Return summary text only.
```

## Selecting an agent

In the TUI, `/new` creates a root session after choosing from agents in the
selected effective set that are runnable as a root (`primary` or `all`, enabled,
with at least one available model). If none are runnable, that effective set's
built-in `default` agent is used.

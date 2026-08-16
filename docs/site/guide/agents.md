# Agents

Agents are Markdown documents with YAML frontmatter. The filename is the agent
ID. An agent defines the system prompt, permission-controlled tool access, its
model fallback chain, and its runtime limits. The engine also runs
four built-in agents that are part of the harness itself.

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
model_fallback:
  - { model: "openai/gpt-5", variant: null }
limits:
  timeout_ms: 30000
  max_input_tokens: 16384
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
| `model_fallback` | array | *(required for `primary`)* | Ordered model chain; see below. |
| `limits` | table | defaults below | Timeouts and token bounds. |
| `permissions` | table | `{}` | Ordered action permission map; see [Permissions](permissions.md). At most 256 rules. |

`limits` defaults to `timeout_ms = 30000`, `max_input_tokens = 16384`,
`max_output_tokens = 2048`. Every limit must be greater than zero.

Tool visibility is derived only from `permissions`. With no `permissions` field,
the `read`, `write`, `edit`, and `bash` tools are visible and unmatched calls ask
by default. `edit` uses the `write` action. Delegation tools require a `delegate`
map naming at least one eligible target, and MCP tools require a non-fully-denied
`mcp` entry. A bare action deny hides that action's tools. A mapped action with
`"*": deny` also hides them unless it contains a more specific `allow` or `ask`
exception.

The former `tools` field is removed. Documents that still declare it fail
with an error naming `tools` and directing the author to `permissions`; remove
the field and express tool visibility and call policy in the permission map.

`model_fallback` entries are `{ model = "<provider>/<model-id>", variant = <name|null|"base"> }`.
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

## Layering and replacement

User-layer and workspace-layer agent directories merge into one registry by
agent ID. A same-ID workspace agent replaces the user agent completely. The IDs
`approval`, `compaction`, and `title` are reserved for internal agents; an
authored document with one of those IDs must use `mode: internal` and replaces
the built-in document through normal layering. The ID `default` is reserved for
the engine-supplied fallback agent and cannot be authored at all.

If no authored agent is runnable as a root, the engine synthesizes the built-in
`default` coding agent bound to the first available model selection. Its prompt
and standard read/write/bash permission map are fixed by the engine.

## Internal agents

The harness runs three internal agents. They are stateless, tool-less model
calls with a strict text-only output contract, and they emit their own event
family (`internal_agent_started`, `internal_agent_completed`, ...).

| ID | Role | Default model | Default limits |
|---|---|---|---|
| `approval` | Stateless approval classifier for `auto_approve` mode | `${parent_model}` | 30 s timeout; 16,384 max input tokens; 2,048 max output tokens |
| `compaction` | Summarizes context into a checkpoint | `${parent_model}` | 30 s timeout; 16,384 max input tokens; 2,048 max output tokens |
| `title` | Generates a concise session title from the opening user messages (the first `session_title.max_input_messages`, default 4) | `${parent_model}` | 10 s timeout; 4,096 max input tokens; 128 max output tokens |

All three default to `${parent_model}`, so they run on the model the parent run
is currently using. The compaction agent's input limit additionally scales to
the largest context window among its resolved models, so it can read the full
conversation it is asked to summarize.

The title agent runs only for root sessions that still need an automatic title.
Delegated sessions already have the `delegate_subagent` description as their
title, so they never invoke the title agent.

The built-in documents are replaced by authored documents with the same ID,
`mode: internal`, and an explicit `model_fallback`. `${parent_model}` is
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
model_fallback:
  - { model: "openai/gpt-5-mini", variant: null }
limits:
  timeout_ms: 30000
  max_input_tokens: 16384
  max_output_tokens: 2048
permissions: {}
---
Summarize conversation context faithfully within the supplied bounds. Return summary text only.
```

## Selecting an agent

In the TUI, `/new` chooses the next root-run agent from the agents that are
runnable as a root (`primary` or `all`, enabled, with at least one available
model). If none are runnable, the built-in `default` agent is used.

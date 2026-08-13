# Agents

Agents are Markdown documents with YAML frontmatter. The filename is the agent
ID. An agent defines the system prompt, the tools it may use, its permission
rules, its model fallback chain, and its runtime limits. The engine also runs
four built-in agents that are part of the harness itself.

## Agent document structure

Each file has schema-4 YAML frontmatter and a nonempty Markdown body:

```markdown
---
schema: 4
description: Reviews changes for correctness
mode: subagent
enabled: true
model_fallback:
  - { model: "openai/gpt-5", variant: null }
limits:
  timeout_ms: 30000
  max_input_tokens: 16384
  max_output_tokens: 2048
tools: [read, bash]
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
| `schema` | integer | *(required)* | Must be exactly `4`. |
| `description` | string | *(required)* | 1–512 characters, no control characters. Shown in the TUI and snapshots. |
| `mode` | string | *(required)* | `primary`, `subagent`, `all`, or `internal`. |
| `enabled` | boolean | *(required)* | Disabled agents are never runnable as roots, delegation targets, or internal backends. |
| `model_fallback` | array | *(required for `primary`)* | Ordered model chain; see below. |
| `limits` | table | defaults below | Timeouts and token bounds. |
| `tools` | array of strings | *(required)* | Tool allowlist: `read`, `write`, `edit`, `bash` (plus the implicit `delegate` tool controlled by the `delegate` permission). At most 256 entries, no duplicates. |
| `permissions` | table | *(required)* | Ordered action permission map; see [Permissions](permissions.md). At most 256 rules. |

`limits` defaults to `timeout_ms = 30000`, `max_input_tokens = 16384`,
`max_output_tokens = 2048`. Every limit must be greater than zero.

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

## Layering and replacement

User-layer and workspace-layer agent directories merge into one registry by
agent ID. A same-ID workspace agent replaces the user agent completely. The IDs
`approval`, `compaction`, and `title` are reserved for internal agents; an
authored document with one of those IDs must use `mode: internal` and replaces
the built-in document through normal layering. The ID `default` is reserved for
the engine-supplied fallback agent and cannot be authored at all.

If no authored agent is runnable as a root, the engine synthesizes the built-in
`default` coding agent bound to the first available model selection. Its prompt,
tools (`read`, `write`, `edit`, `bash`), and the standard read/write/bash
permission map are fixed by the engine.

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

The built-in documents are replaced by authored schema-4 documents with the same
ID, `mode: internal`, and an explicit `model_fallback`. `${parent_model}` is
allowed only in internal agents. When an internal agent document is disabled, or
its fallback chain yields no available model, the internal call fails safely
(approval degrades to asking, compaction is skipped, and title falls back to an
input excerpt) and an `internal_agent_failed` event is recorded.

Example replacement — run compaction on a cheaper model while the primary run
keeps its own selection:

```markdown
---
schema: 4
description: Context compaction on a fast model
mode: internal
enabled: true
model_fallback:
  - { model: "openai/gpt-5-mini", variant: null }
limits:
  timeout_ms: 30000
  max_input_tokens: 16384
  max_output_tokens: 2048
tools: []
permissions: {}
---
Summarize conversation context faithfully within the supplied bounds. Return summary text only.
```

## Selecting an agent

In the TUI, `/new` chooses the next root-run agent from the agents that are
runnable as a root (`primary` or `all`, enabled, with at least one available
model). If none are runnable, the built-in `default` agent is used.

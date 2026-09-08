# Agent

Agents are Markdown documents with YAML frontmatter. The filename is the agent
ID. An agent defines the system prompt, permission-controlled tool access, its
model fallback chain, and its runtime limits. The harness supplies four agents:
three internal agents plus the synthesized `default` coding agent when no
authored root agent is runnable.

Author common agents under `~/.cookie-agent/agents/`. The exact workspace may
use the same document format under `<cwd>/.cookie-agent/agents/`; a same-ID
workspace document takes precedence and replaces the complete user document.
Frontmatter fields and nested permission maps never merge across layers.

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
| `enabled` | boolean | *(required)* | Disabled agents are never runnable as roots, delegation targets, or internal backends. |
| `models` | array | *(required)* | Ordered model chain; must be nonempty for `primary`. Other modes may declare `[]`. |
| `limits` | table | defaults below | Timeouts and token bounds. |
| `permissions` | table | `{}` | Ordered action permission map; see [Permissions](agents.md#permissions). At most 256 rules. |

`max_output_tokens` applies in every mode. A nonzero value caps each request at
the smaller of the document value and the model's own output limit. It defaults
to no document cap for the non-internal `primary`, `subagent`, and `all` modes.
Authored internal agents retain a 2,048-token default; setting it explicitly to zero
removes that document cap.
`timeout_ms` applies only to internal agents. For other modes, a nonzero value is
a hard error. Internal agents use the 30-second invocation timeout when
`timeout_ms` is zero or omitted.

Tool visibility is derived only from `permissions`: with no `permissions`
field, no tools are visible. An action's tools become visible when the agent or
session overlay has any `allow` or `ask` rule for it — any resource pattern
counts, even if a competing deny wins at execution time — and unmatched
resources deny by default, so approval requires an explicit `ask` rule. `edit`
uses the `write` action. Delegation tools additionally require a `delegate` map
naming at least one eligible target. See
[Permissions](agents.md#tool-availability-and-delegation).

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
[`[agent_md]`](../engine/agent_md.md).

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
default. MCP, `webfetch`, and every other undeclared action stay hidden. User
agents do not inherit this list and must declare their own tool permissions.

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
| `compaction` | Summarizes context into a checkpoint | `${parent_model}` | 30 s timeout; model-derived input budget; 4,096 max output tokens |
| `title` | Generates a concise session title from the opening user messages (the first `session_title.max_input_messages`, default 4) | `${parent_model}` | 10 s timeout; model-derived input budget; 128 max output tokens |

All three default to `${parent_model}`, so they run on the model the parent run
is currently using — including its position in the fallback chain: if the run
has fallen back to its second model, internal agents resolve to that second
model too. An internal agent's input budget is derived from each
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
  max_output_tokens: 4096
permissions: {}
---
Summarize conversation context faithfully within the supplied bounds. Return summary text only.
```

## Selecting an agent

In the TUI, `/new` creates a root session after choosing from agents in the
selected effective set that are runnable as a root (`primary` or `all`, enabled,
with at least one available model). If none are runnable, that effective set's
built-in `default` agent is used.

## Permissions

Agent documents define an ordered permission map for `read`, `write`, `bash`,
`delegate`, `mcp`, `plugin`, `skill`, and `webfetch`. Each action is either one bare effect or a resource-pattern map:

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
fewer wildcards, then the later declaration on an exact tie. Unmatched resources
are denied by default. `ask` remains an explicitly writable effect and routes
matching calls through the existing approval flow; it is never an implicit fallback.

### Resource labels

Tool providers publish a static permission name and an optional resource label:

| Tool | Permission name | Resource label |
|---|---|---|
| `read` | `read` | Workspace-relative path inside the workspace; absolute path outside it |
| `write`, `edit` | `write` | Workspace-relative path inside the workspace; absolute path outside it |
| `bash` | `bash` | Complete command string |
| `delegate_subagent` | `delegate` | Target `agent_type` |
| `get_subagent_result`, `steer_subagent`, `cancel_subagent` | `delegate` | None (permission-name-only check) |
| `<server>_<tool>` | `mcp` | The complete generated MCP tool name |
| `skill` | `skill` | Skill name |
| `goal_get`, `goal_update` | `read`, `write` respectively | `goal:current` |
| `read_tool_result` | `read` | Tool-result resource |
| `webfetch` | `webfetch` | Initial URL as given, including its query string |

The `edit` tool uses the `write` permission action. Bash is not parsed into file
operations: `cat .env` is controlled by `bash`, not `read`, and a pattern such
as `git *` also matches a longer command beginning with `git`. See
[Security guarantees](security.md) for the platform-specific filesystem and
process boundaries behind these tools.

Path resource labels are normalized lexically from the single path requested by
the caller. They are not canonicalized through symbolic links before permission
matching. That one label authorizes the whole prepared `read`, `write`, or
`edit` operation: if an allowed alias resolves outside the workspace, cookie
agent does not perform a second permission check or require a rule for the
resolved destination. Filesystem preparation still binds the traversed route
and destination and can reject the operation independently. In particular,
changing a link target after preparation fails with `operation_changed`; it
does not cause policy to be reevaluated against the new destination.

Bash remains independent of this path-resource contract. An allowed command has
only its complete command-string resource and does not gain hidden `read` or
`write` resource checks when the command traverses a link.

Permission policy does not validate cookie agent's pre-existing private-state
paths. New state is created with owner-only permissions, but existing loose,
foreign-owned, hard-linked, or symlinked state is read and written as-is. This is
part of the local storage threat model, not an `allow`/`ask`/`deny` decision.

MCP checks are always scoped. A rule such as `"github_*": allow` covers every
tool from that generated server prefix, while a more-specific deny can override
one tool. An unmatched MCP tool is denied.

When a tool has no resource label, only the permission's bare effect or `"*"`
rule applies. Specific patterns are inapplicable rather than matching or
denying. If neither a bare effect nor `"*"` exists, the normal unmatched result
is `deny`.

`${workspace_dir}` is allowed only in `read` and `write` patterns and expands
against the engine workspace root during evaluation. Ordinary absolute patterns
such as `/etc/*` control outside-workspace paths. Permission patterns do not
expand environment variables.

Permission evaluation has no implicit filename or resource-name exceptions.
Credential files, `.env` and `.env.*`, and non-file resource labels all use the
configured rules for their action, including in session overlays. Broad file
allows such as `"*": allow` or
`"${workspace_dir}/*": allow` apply without a protected-file override. Explicit
`deny` and `ask` rules follow the same specificity and overlay precedence as any
other file; unmatched resources are denied.

The synthesized `default` agent still declares explicit dotenv read denies and
`.env.example` allows in its permission map. These are ordinary policy rules,
not an implicit guard applied to authored agents or session overlays. The same
applies to its explicit credential-file denies (such as `store-v3.json`,
`token-v1`, `id_*`, `.netrc`, and `application_default_credentials.json`).

This contract governs tool permission evaluation. It does not replace filesystem
integrity and resource-binding checks, or govern engine configuration and prompt
loading (such as workspace `AGENTS.md` admission). Generic approval modes and
turn-scoped skill grants retain their documented behavior; they are not
resource-name exceptions.

### Tool availability and delegation

Tools are opt-in. A permission action must have at least one `allow`
or `ask` rule in the agent document or session overlay before that action's
tools are visible. Any resource pattern counts, not just `"*"`. Visibility does
not resolve rule precedence: an overlay `deny` does not hide a tool if the agent
still declares an `allow` or `ask` rule for that action. An action omitted from
both layers, or with only `deny` rules, advertises no tools for any provider.
Resource patterns and overlay precedence still decide individual calls once
tools are visible.

For example, this agent exposes read, write/edit, and bash with granular write
and command policies while leaving delegation and MCP tools hidden:

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

MCP tools follow the same rule: the `mcp` action must contain a non-deny rule.
Delegation tools additionally require the `delegate` map to name at least one
eligible target with `allow` or `ask`. There is no separate tool allowlist.

There is also no separate MCP server approval prompt or trust store. Configured,
enabled servers follow their normal lazy or eager connection lifecycle, while
the agent's `mcp` map remains the sole visibility and call gate. Project MCP
configuration and project agent documents are version-controlled content
equivalent to code: a repository can ship both a server definition and an agent
that permits it. Review them and work only in repositories you trust.

Delegation targets come from the keys in the `delegate` permission map and must
resolve to enabled `subagent` or `all` agents. This action controls
`delegate_subagent`, `get_subagent_result`, `steer_subagent`, and
`cancel_subagent`. Only `delegate_subagent` matches agent-specific patterns.
Result, steer, and cancel retain their existing ownership and argument
validation, but permission evaluation for them uses only the bare effect or
`"*"`. Their approval display still shows the owned `session_id`; display text is
independent of the permission resource.

This changes existing mapped delegation policies. For example,
`delegate: {reviewer: allow, "*": deny}` allows `delegate_subagent` targeting
`reviewer`, but denies `get_subagent_result`, `steer_subagent`, and
`cancel_subagent` because their permission-name-only checks use the `"*": deny`
fallback and ignore the `reviewer` pattern.
The prepared resource identity for these three tools also changed. Existing
tree grants issued for their former agent- or session-scoped identities do not
carry over: old grants can no longer auto-approve these operations, so any call
whose new resource-less policy evaluates to `ask` requires approval again.
Runtime `delegation.max_depth` defaults to 3 and `max_concurrency` defaults to 4.

`delegate_subagent` replaces the former `delegate` tool name without an alias.
Old tool calls and prepared-operation grants therefore fail closed. The
`delegate` spelling above remains the permission action, not a tool alias.

### Web fetching

Web access uses the `webfetch` action, matched against the exact initial URL as
requested, including its query string:

```yaml
permissions:
  webfetch:
    "*https://*.quantumcookie.xyz/*": allow
    "https://review.example.org/*": ask
  read:
    "tool_result:*": allow
```

The `read` rule exposes `read_tool_result` so the model can page long responses.
Permission is checked once, against the initial URL; redirect destinations are
not checked, and there is no SSRF protection, host/IP blocklist, or DNS
pinning, so scope patterns accordingly, including access to local network
services. Requests inherit the cookie-agent process's proxy environment and
have a 30-second timeout. See the [tool reference](../reference/tools.md#webfetch)
for the output format, HTML rendering, the download cap, and result paging.

### Live permission modes

Each session tree starts in `auto_approve` unless changed. The mode is runtime
only, is keyed by the tree root, and can be changed through any session in the
tree:

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

Run `/permissions` to edit the selected session's permission overlay. Each
action exposes its effective `allow`, `ask`, or `deny` effect and its individual
resource patterns. Source labels distinguish `session_overlay`,
`agent_document`, and `default`. Left/right changes an effect, `n` adds a
validated wildcard pattern, and `d` removes a selected session-overlay rule.
The editor does not accept freeform YAML.

An overlay rule is evaluated before matching rules from the frozen agent
snapshot. If no overlay rule matches, evaluation falls back to the agent
document and then the normal default (`deny`). This default affects evaluation
only; an action omitted from both layers has no visible tools. Changes affect
subsequent visibility and permission evaluations only. They do not rewrite an
active run's frozen agent/model
identity or retroactively cancel an operation already executing. A pending
tree-approval response is rejected as changed if the session overlay changed
after that approval was requested, so it cannot commit a stale durable grant.

Every change appends a complete `session_permission_overlay_set` event to the
session log. Overlay state therefore survives daemon restart and follows normal
revert and fork branch semantics. Tightening a rule durably invalidates existing
tree grants for that action under the session root before the overlay event is
committed. Invalidation is action-wide because tree-grant records retain opaque
prepared identities rather than normalized resource labels.

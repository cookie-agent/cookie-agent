# Permissions

Agent documents define an ordered permission map for `read`, `write`, `bash`,
`delegate`, and `mcp`. Each action is either one bare effect or a resource-pattern map:

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
ask.

## Resource labels

Tool providers publish a static permission name and an optional resource label:

| Tool | Permission name | Resource label |
|---|---|---|
| `read` | `read` | Workspace-relative path inside the workspace; absolute path outside it |
| `write`, `edit` | `write` | Workspace-relative path inside the workspace; absolute path outside it |
| `bash` | `bash` | Complete command string |
| `delegate_subagent` | `delegate` | Target `agent_type` |
| `get_subagent_result`, `steer_subagent`, `cancel_subagent` | `delegate` | None (permission-name-only check) |
| `<server>_<tool>` | `mcp` | The complete generated MCP tool name |

The `edit` tool uses the `write` permission action. Bash is not parsed into file
operations: `cat .env` is controlled by `bash`, not `read`, and a pattern such
as `git *` also matches a longer command beginning with `git`.

MCP checks are always scoped. A rule such as `"github_*": allow` covers every
tool from that generated server prefix, while a more-specific deny can override
one tool. An unmatched MCP tool asks.

When a tool has no resource label, only the permission's bare effect or `"*"`
rule applies. Specific patterns are inapplicable rather than matching or
denying. If neither a bare effect nor `"*"` exists, the normal unmatched result
is `ask`.

`${workspace_dir}` is allowed only in `read` and `write` patterns and expands
against the engine workspace root during evaluation. Ordinary absolute patterns
such as `/etc/*` control outside-workspace paths. Permission patterns do not
expand environment variables.

Generic read allows do not override the built-in ask behavior for `.env` and
`.env.*`; an exact or more-specific authored rule decides.

## Tool availability and delegation

Tools are opt-in. A permission action must have at least one effective `allow`
or `ask` rule in the agent document or session overlay before that action's
tools are visible. Omitting an action hides its tools. A bare deny, or `"*":
deny` with no named non-deny exception, also hides them. Resource patterns still
decide individual calls once tools are visible.

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

## Live permission modes

Each session starts in `auto_approve` unless changed:

- `auto_approve` runs the stateless approval classifier and asks the user when
  it escalates or fails safely.
- `ask` skips the classifier and routes policy asks and model-requested
  approvals to the user.
- `yolo` approves asks immediately.

Hard denies, the doom-loop guard, and existing tree grants are evaluated before
the mode shortcut. Changing a mode does not alter an already pending approval
and does not cascade to delegated sessions.

## Session permission overlays

Run `/permissions` to edit the selected session's permission overlay. Each
action exposes its effective `allow`, `ask`, or `deny` effect and its individual
resource patterns. Source labels distinguish `session_overlay`,
`agent_document`, and `default`. Left/right changes an effect, `n` adds a
validated wildcard pattern, and `d` removes a selected session-overlay rule.
The editor does not accept freeform YAML.

An overlay rule is evaluated before matching rules from the frozen agent
snapshot. If no overlay rule matches, evaluation falls back to the agent
document and then the normal default (`ask`). This default affects evaluation
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

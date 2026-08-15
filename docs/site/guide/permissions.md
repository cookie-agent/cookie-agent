# Permissions

Agent documents define an ordered permission map for `read`, `write`, `bash`,
and `delegate`. Each action is either one bare effect or a resource-pattern map:

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

The `edit` tool uses the `write` permission action. Bash is not parsed into file
operations: `cat .env` is controlled by `bash`, not `read`, and a pattern such
as `git *` also matches a longer command beginning with `git`.

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

## Tools and delegation

The agent's `tools` list is a separate allowlist, and both the tool allowlist
and permission rules must pass. A bare deny hides a tool. A mapped action hides
it only when `"*": deny` exists with no non-deny exceptions.

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

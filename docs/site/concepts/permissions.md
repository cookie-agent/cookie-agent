# Permissions

Every tool call a model makes passes a permission check before it runs. The
check asks one question: may this *action* touch this *resource*? The answer
comes from rules, and an `ask` answer comes from you, or from a classifier on
your behalf, depending on the session's *mode*.

## Actions and resources

Each tool declares a permission *action* and names the *resource* of each call:

- `read` and `write` (`write` also covers `edit`): the file path;
- `bash`: the whole command string;
- `webfetch`: the URL;
- `delegate`: the subagent's agent ID; `message`: the recipient's relation,
  `parent`, `child`, or `sibling`;
- `mcp`: the MCP tool's name; `skill`: the skill name;
- `memory` and `memory_write`: the memory URL.

Actions are free-form labels, a lowercase letter then up to 31 lowercase
letters, digits, or underscores, so a runtime's or a plugin's tools can
declare actions of their own. A few tools declare none and are always
allowed, such as `read_artifact`, which reads only the tree's own retained
output. See [Resource labels](../extend/agents.md#resource-labels).

## Rules

An agent document maps each action to an effect, `allow`, `ask`, or `deny`,
either outright or per resource pattern:

```yaml
permissions:
  read: allow
  bash:
    "git status": allow
    "*": ask
  write:
    ".env*": deny
    "*": ask
```

The most specific pattern wins, and a resource no rule matches is denied. A
tool whose action has no `allow` or `ask` rule is not even offered to the
model. See [Permissions](../extend/agents.md#permissions).

Rules can also be added to one session tree without touching the agent, in
`/permissions` or with `cookie run --allowed-tools`; these *overlay* rules are
checked before the agent's. See
[Session permission overlays](../extend/agents.md#session-permission-overlays).

## Rules for one runtime

A rule keyed `action@runtime_id` applies only to that runtime's tools,
whatever workspace they work in, so `bash: ask` with `bash@labpc: {"git *":
allow}` lets git run freely on `labpc` and asks for everything else. Actions of
the daemon's own tools, `delegate`, `message`, and `skill`, take no selector.
See [Runtime selectors](../extend/agents.md#runtime-selectors).

## Modes and approvals

A rule that says `ask` is resolved by the session tree's *permission mode*,
shown in the TUI's bottom bar and shared by every client:

| Mode | An ask is |
|---|---|
| `auto-approve` (the default) | decided by the `approval` classifier agent, which passes to you what it will not approve |
| `auto-approve-n` | decided by the classifier; what it would pass to you is rejected |
| `auto-approve-y` | decided by the classifier; what it would pass to you is approved once |
| `ask` | always yours |
| `yolo` | approved at once |

A `deny` rule and the doom-loop guard hold in every mode. An approval shows the call as it will run, such as the command or
the diff of an edit, and you allow it once, allow it for the whole session
tree, reject it, or reject it with a note for the agent. See
[Live permission modes](../extend/agents.md#live-permission-modes) and
[Approvals](../clients/tui.md#approvals).

## What permissions do not do

Rules decide which calls run; they are not a sandbox. An allowed `bash`
command runs with your user's rights, and `cat .env` is a `bash` call, not a
`read`. See [Security](../daemon/security.md).

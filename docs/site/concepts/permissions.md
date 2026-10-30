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
- `memory` and `memory_write`: the memory URL;
- `cron`: the target kind of the cron job a call touches, `session`,
  `subagent`, or `new`;
- `list_runtimes`: nothing to name; any rule pattern matches;
- `attach` (`attach_machine`, `detach_machine`): the workspace, checked on
  the target runtime (`attach@labpc`).

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

The most specific pattern wins, and whatever no rule matches asks: a
resource your key's patterns leave out, and every action you name no key
for, plugins' and runtimes' own included. Only a `deny` you write denies, and
a tool is not even offered when a `"*": deny` covers its action with no
`allow` or `ask` beside it. See [Permissions](../extend/agents.md#permissions).

Three actions have a default by the agent's mode, which applies only while
the document names no key for that action (a key with a runtime selector
counts):

| Mode | `list_runtimes` | `skill` | `mcp` |
|---|---|---|---|
| `primary`, `all` | `allow` | `allow` | `"*": ask` |
| `subagent` | none (asks) | `allow` | `"*": ask` |
| `internal` | none | none | none |

So every agent that is not internal is offered the connected MCP servers'
tools and asks before each call. A declared key replaces its action's
default entirely: `mcp: {"my_chrome_*": ask}` offers only that server's
tools, and `mcp: deny` hides them all. `/permissions` shows a default with
the source `default`. See [MCP default](../extend/agents.md#mcp-default).

The memory actions have a default by whether the run is delegated, with the
same rule that a declared key replaces it (patterns may use `${agent_id}`,
`${session_machine}`, and `${session_project}`):

| Run | `memory` | `memory_write` |
|---|---|---|
| root, agent not internal | `allow` | `user://*`, `agent://${agent_id}`, `machine://${session_machine}/*`, `project://${session_project}/*`: `allow` |
| delegated, agent not internal | `"agent://${agent_id}": allow` | `"agent://${agent_id}": allow` |
| internal agent | none | none |

So a root agent reads every memory and writes the memory related to its
session, and `memory: deny` turns memory off. See
[Memory permissions](../extend/agents.md#memory-permissions).

The `cron` action, which the [cron tools](../sessions/scheduling.md#agents-and-the-cron-tools)
use, has a default the same way: `cron: allow` for a root run of any agent
but an internal one, and none for a delegated run or an internal agent, so a
subagent sees the cron tools only when its document grants `cron`. Whatever
the rules say, an agent can target only its own session or one below it, and
only you, from a client, can set a new-session job's permission mode or pin
its model. See [Cron permissions](../extend/agents.md#cron-permissions).

The `attach` action defaults the same way: `attach: allow` for a root run of
any agent but an internal one, and none for a delegated run, so a subagent
attaches machines only when its document grants `attach`. A run attaches to
its own session only. See
[Attaching from a session](runtimes.md#attaching-from-a-session).

## How the levels combine

A run's rules come from three levels: the daemon's defaults, the agent
document, and the session's own rules, which you add in `/permissions`, the
web client's permissions panel, or with `cookie run --allowed-tools` without
touching the agent. They merge into **one rule list**:

1. the defaults, for each action the document names no key for;
2. the document's rules;
3. the session's rules. A session rule with the same action, runtime selector,
   and pattern as a rule above replaces it; any other session rule is added.

That one list decides everything: a tool is offered unless a `"*"` deny
covers its action with no rule that allows or asks, and a call follows the
most specific matching rule, with a session rule winning a tie, or asks when
none matches. `/permissions` shows the same list, each
rule with its source (`session`, `agent`, or `default`).

| Session rule | Document and defaults | A call |
|---|---|---|
| `cron: deny` | default `cron: allow` | none: the cron tools are hidden |
| `memory: {"user://*": deny}` | default `memory: allow` | `user://` denied, every other memory readable |
| `bash: {"cargo test *": allow}` | `bash: {"*": ask}` | `cargo test` runs, the rest asks |
| `bash: {"*": allow}` | `bash: {"*": ask, "git push *": deny}` | `git push` denied, the rest runs |
| `bash: {"git push *": allow}` | `bash: {"*": ask, "git push *": deny}` | `git push` runs, the rest asks |
| `mcp: deny` | default `mcp: {"*": ask}` | none: the MCP tools are hidden |

So a session can turn off a tool a default gives (memory, cron, MCP,
`list_runtimes`, skills, attaching), and a broad session rule does not override a narrower rule
of the document: to change that rule for one session, override that exact
pattern, which is what editing a row in `/permissions` does. See
[Session permission overlays](../extend/agents.md#session-permission-overlays).

## Rules for one runtime

A rule keyed `action@runtime_id` applies only to that runtime's tools,
whatever workspace they work in, so `bash: ask` with `bash@labpc: {"git *":
allow}` lets git run freely on `labpc` and asks for everything else. Of the
daemon's own tools' actions, `message` and `skill` take no selector;
`delegate@labpc` checks a root's delegation that gives its subagent the
`labpc` runtime (see [Delegating to runtimes](../sessions/delegation.md#limiting-where-the-root-delegates)).
See [Runtime selectors](../extend/agents.md#runtime-selectors).

A path pattern starting with `~/` means the home directory of the runtime the
tool runs on, so `read: {"~/notes/*": allow}` allows the notes under each
runtime's own home. On a runtime that does not report its home the pattern
matches nothing. See
[Paths under the home directory](../extend/agents.md#paths-under-the-home-directory).

## Modes and approvals

A rule that says `ask` is resolved by the session tree's *permission mode*,
shown in the TUI's bottom bar and shared by every client:

| Mode | An ask is |
|---|---|
| `auto-approve` (the default) | decided by the `approval` reviewer agent, which passes to you what it will not approve |
| `auto-approve-n` | decided by the reviewer; what it would pass to you is rejected |
| `auto-approve-y` | decided by the reviewer; what it would pass to you is approved once, unless its review failed |
| `ask` | always yours |
| `yolo` | approved at once |

The reviewer judges each call against the whole session: your messages, in
order, with what the agent has done so far (its tool calls and their
outcomes, never their output) and the project's `AGENTS.md`. Only your own
messages, your scheduled prompts, and `AGENTS.md` authorise a call; a later
aside adds to the task rather than replacing it. Each decision comes with a
one-sentence reason, shown to you and, on a denial, to the agent. A review
that fails is passed to you in `auto-approve` and `auto-approve-y` and
rejected in `auto-approve-n`. The mode is kept in the tree's log, so it
survives a daemon restart. See
[The approval reviewer](../extend/agents.md#the-approval-reviewer).

A `deny` rule and the doom-loop guard hold in every mode. An approval shows the call as it will run, such as the command or
the diff of an edit, and you allow it once, allow it for the whole session
tree, reject it, or reject it with a note for the agent. See
[Live permission modes](../extend/agents.md#live-permission-modes) and
[Approvals](../clients/tui.md#approvals).

## What permissions do not do

Rules decide which calls run; they are not a sandbox. An allowed `bash`
command runs with your user's rights, and `cat .env` is a `bash` call, not a
`read`. See [Security](../daemon/security.md).

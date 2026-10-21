# Delegating to runtimes

Status: **approved** (2026-10-21); queued after the projects work. Builds on the [runtimes spec](runtimes.md), the
[runtime capabilities spec](runtime-capabilities.md), and the
[projects spec](projects.md).

## 1. Goal and scope

Today every delegated child inherits the root session's attachments, and
the model cannot see which runtimes the daemon has. An optional `runtimes`
argument to `delegate_subagent` was removed on 2026-10-17
([runtimes spec section 5.2](runtimes.md)). This spec brings runtime choice
back, at the root only, so that a root session with only `global` can
orchestrate work across machines. It also gives the model a live view of
the daemon's runtimes.

**Decided 2026-10-21:**

- At depth 0, `delegate_subagent` gains a `capabilities` parameter: the
  runtime capabilities the child works through. Each is written
  `<runtime_id>/<capability_id>`, with a workspace when the capability takes
  one. The name went from `runtimes` to `attachments` to `capabilities`,
  all on 2026-10-21.
  - It is **absent** when the root session does not have `global`
    attached.
  - It is **required** when the root session has nothing attached besides
    `global`.
  - It is **optional** otherwise. Leaving it out keeps today's behaviour:
    the child inherits the root's attachments.
- At depth 1 and below, the parameter does not exist. Every child inherits
  its **parent's** attachments.
- Whether children have `global` is decided by the root session's
  attachments, never by the model.
- Users can limit where the root delegates with `delegate@<runtime>` rules,
  using the existing `action@runtime` selector.
- The `global` runtime offers a new tool, `list_runtimes`, behind its
  own `list_runtimes` permission. It prints every machine and runtime on
  the daemon, plus this session's own attachments (never other sessions'
  instances), and is exempt from output truncation. It reads
  the daemon's state at every call, so a runtime that dials in mid-session
  shows up on the next call.
- **Web New session** opens straight on the composer with only `global`.
  The runtime cards are still there, behind a "Runtimes…" chip in the
  bottom bar, and a project's defaults still apply.
- **TUI** keeps its default of `global` plus `local` at the current
  directory.

Out of scope: attaching runtimes to a running session from a tool;
choosing runtimes below depth 0; and a tool that starts runtimes on other
machines (section 7 shows how a subagent can do it with existing tools).

## 2. `delegate_subagent` at the root

### 2.1 Schema

At depth 0 the schema gains:

```json
"attachments": {
  "type": "array",
  "minItems": 1,
  "items": {
    "type": "object",
    "additionalProperties": false,
    "required": ["capability"],
    "properties": {
      "capability": { "type": "string", "description": "`<runtime_id>/<capability_id>`, from list_runtimes, e.g. `labpc/coding`" },
      "workspace": { "type": "string", "description": "Directory on that runtime's machine; required when the capability takes a workspace" }
    }
  }
}
```

- **Present, required, or absent.** The tool is already composed per
  session and per turn, so its schema follows the root's attachments:
  - **Absent** when the root session does not have `global` attached.
    `capabilities` belongs with `list_runtimes`, which is part of `global`, so
    a session without `global` keeps today's tool, and its children
    inherit the root's attachments.
  - **Required** when the root has `global` and nothing else.
  - **Optional** when the root has `global` and other attachments.
- **The description** covers:
  - **What each item does:** it gives the child that runtime capability,
    at the workspace when the capability takes one. These are runtime
    capabilities, not the subagent's tools or permissions.
  - **How to write one:** always the full `<runtime_id>/<capability_id>`, from
    a `<runtime id=…>` and one of its `<capability name=…>` in
    `list_runtimes`. `list_runtimes` prints no separate "delegate as"
    string (decided 2026-10-21).
  - **When it is needed:** what the parameter does in each case. With only
    `global` attached, the description also says that the root has no
    workspace and must name where the work happens, and it points to
    `list_runtimes`.
- **Unknown names.** A runtime or capability the daemon does not list
  refuses the call, with a message that names the runtime's capabilities
  when the runtime exists.
- **`resume_session_id`** cannot be combined with `capabilities`, because a
  resumed child keeps its own attachments. `inherit_context` can.

### 2.2 What the child gets

- **`global` follows the root, never the model** (decided 2026-10-21).
  Every child in a tree has `global` exactly when the root session does,
  at every depth. `capabilities` cannot name `global/global`; a call that does is
  refused. Attaching or detaching `global` on the root changes only
  children created afterwards.
- **With `capabilities`:** the child gets the root's `global` by the rule
  above, so memory, MCP, and plugin tools stay. It also gets each listed
  runtime, in the order given, attached with the usual names
  (`<runtime>_<workspace basename>`, with `_2` added for duplicates).
  - Two or more workspaces get tool prefixes, exactly as for any session.
  - The same runtime may be listed twice with different workspaces.
- **Without `capabilities`:** the child inherits the root's attachments, as
  today.
- **Starting the runtimes.** The daemon starts each runtime before it
  creates the child, as `session.create` does: a template runtime is
  started, and a dial-in runtime must be connected. If any runtime fails
  to start or is refused, the call fails, nothing is created, and the
  error names the runtime and the reason.
- **The prompt.** The child's own system prompt describes its
  attachments: one workspace, or the machines block when there are
  several. This is unchanged.

### 2.3 Deeper children

- At depth 1 and below, `delegate_subagent` has no `capabilities` parameter.
  A call that passes one anyway is refused as an unknown argument
  (`deny_unknown_fields`).
- A child at depth n + 1 inherits the attachments of its **parent** at
  depth n. Today it inherits the root's. The two are the same until a
  depth-1 child is given its own runtimes, which is what this spec adds.
- A runtime attached or detached on the root later does not reach
  existing children. New children of the root see it.

## 3. Permissions

- **Rules.** The existing `action@runtime` selector applies to `delegate`:
  - `delegate@labpc: deny` keeps the root from delegating to `labpc`;
  - `delegate@labpc: ask` asks first;
  - a plain `delegate` rule covers every runtime.

  As with other selectors, the most specific rule wins.
- **Checking a call.** Each runtime listed in `capabilities` is checked as
  `delegate@<runtime id>`. A denied runtime refuses the whole call. An
  `ask` runtime puts the call through the usual approval, which shows the
  runtimes and workspaces.
- **Inheriting.** A call without `capabilities` is checked as today: plain
  `delegate`, with no runtime selector.
- **The child's own rules** still decide what it can do on that runtime,
  including any `bash@labpc`-style selectors.

## 4. `list_runtimes`

A tool of the `global` runtime that takes no arguments. It is offered
only where `global` is attached, and only when the run's agent may use it.

- **Permission.** The new action `list_runtimes` controls it. An agent
  document with no `list_runtimes` key gets a default rule by its mode,
  as the memory actions do (memory spec section 9.2):

  | Mode | `list_runtimes` |
  |---|---|
  | `primary`, `all` | `allow` |
  | `subagent`, `internal` | none |

  Subagents cannot choose runtimes, so by default they do not see the
  list. An agent document can grant it with `list_runtimes: allow`, or
  turn it off with `deny`. Session overlays apply on top, as for any
  action.

- **Live.** Every call reads the daemon's current state, from the same
  sources as `runtimes.list` and `machines.list`.
- **Untruncated.** It opts out of `[tool_output]` truncation. Its single
  output is bounded only by the event size limit, so a daemon with
  thousands of instances fails with a resource-limit error rather than
  being cut silently.
- **Output.** Tags, grouped by machine and then by runtime:

  ```text
  <runtimes>
    <machine name="yunhao-labserver" platform="linux">
      <runtime id="global" lifetime="builtin" connected="true">
        <capability name="global" takes_workspace="false"/>
        <attachment name="global" capability="global" status="ready"/>
      </runtime>
      <runtime id="local" lifetime="builtin" connected="true" home="/home/u">
        <capability name="coding" takes_workspace="true"/>
        <attachment name="local_cookie_agent" capability="coding" workspace="/home/u/src/cookie-agent" status="ready"/>
      </runtime>
    </machine>
    <machine name="labpc" platform="linux">
      <runtime id="labpc" lifetime="spawned" connected="false" message="starts when a session needs it">
        <capability name="coding" takes_workspace="true"/>
      </runtime>
    </machine>
    <unplaced>
      <runtime id="roamer" lifetime="dial_in" connected="false" message="…"/>
    </unplaced>
  </runtimes>
  ```

  - `<attachment>` elements are the calling session's own attachments,
    and nothing else (decided 2026-10-21). Instances that other sessions
    have open are left out: they say nothing about where this session can
    work, and they would show other sessions' workspaces. Any runtime can
    still be named in `delegate_subagent`'s `capabilities`, opened or not.
  - Attributes with no value (`home`, `platform`, `message`) are left out.
    Values are XML-escaped.
  - A runtime with no known machine goes under `<unplaced>`.
- **Timing.** Its result describes the daemon at call time. A runtime that
  dials in or drops out later shows from the next call. `delegate_subagent`
  checks its `capabilities` against the same live state when it is called.

## 5. Clients

### 5.1 Web

- **New session** opens on the composer with only `global` attached. The
  first message creates the session with `runtimes: []` and
  `global: true`.
- **A "Runtimes…" chip** in the bottom bar, shown on drafts, opens the
  runtime cards as a sheet. A selection there attaches those runtimes at
  creation, as the cards do today. The chip shows the selected runtimes as
  badges, or "global only".
- **Projects.** A project with default attachments prefills the draft's
  selection, and the chip shows them. Choosing a project still never
  changes a running session.
- **The browser no longer remembers a selection** across New session.
  Each new draft starts global-only, or from its project's defaults.

### 5.2 TUI

- No change. Plain `cookie` attaches `global` and `local` at the current
  directory, `/runtime` edits a draft's attachments, and project defaults
  apply as in the projects spec.

## 6. Prompt

- **A root with only `global`.** Its system prompt has no working
  directory and no machine line, as today. Instead, the delegate tool's
  prompt section adds a short paragraph: this session has no workspace;
  work that needs files or commands goes to a subagent with `capabilities`;
  and `list_runtimes` shows what exists.
- **Every other prompt** is unchanged.

## 7. Bringing a runtime in from a subagent

No new mechanism is needed for the pattern of a root that has a daemon
coding subagent bring a remote machine in temporarily:

1. The root delegates to a coding subagent on `local`.
2. That subagent runs a background `bash` call that dials the remote
   runtime in:
   `ssh <host> cookie runtime serve --daemon <url> --id <id> --token <token>`.
   Alternatively, it adds a `[runtimes.<id>]` template to `config.toml`.
3. The root calls `list_runtimes`, sees the new runtime, and delegates to
   it with `capabilities`.
4. Stopping the background call (`stop_background`) ends the dial-in
   connection, and the runtime shows as disconnected.

**Security.** The subagent needs the daemon's token, which is
`[server] token` in `config.toml`. Any agent that may `read` or `bash` on
the daemon's machine can already read it. This spec gives no new access,
but the docs say so plainly. Users who want to prevent it deny reads of
`~/.cookie-agent/config.toml` in that agent's rules. A config edit takes
effect only after `config.reload` or a daemon restart, which a subagent
cannot trigger through a tool. The dial-in path is therefore the one the
docs show.

## 8. Protocol and compatibility

- **No client, runtime, or plugin protocol change.** The tool schemas are
  model-facing. Children record their attachments with the existing
  `runtime_attached` events.
- **Old logs.** Children that inherited the root's attachments keep them.
  Replay reads attachments from each child's own events.
- `docs/site/reference/tools.md` is regenerated for `list_runtimes` and
  the new parameter.

## 9. Phases

1. **Engine:**
   - the `capabilities` parameter and its per-session schema;
   - starting runtimes for a child;
   - parent inheritance;
   - `delegate@runtime` checks;
   - `list_runtimes`;
   - the prompt paragraph;
   - tests;
   - the tools reference.
2. **Web:** a global-only New session, the "Runtimes…" chip and sheet,
   and project prefill through the chip.
3. **Docs:**
   - the runtimes concept page and delegation guide;
   - the pattern in section 7, with its security note;
   - the web guide.

## 10. Open questions

None. The user approved the spec on 2026-10-21, including:
- the `list_runtimes` permission defaults;
- `resume_session_id` excluding `capabilities`;
- the web client forgetting the selection;
- the dial-in pattern in section 7.

A delegated child keeps copying its parent's `cwd_identity`.

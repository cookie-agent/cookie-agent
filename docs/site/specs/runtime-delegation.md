# Delegating to runtimes

Status: **implemented** (2026-10-21); approved the same day. How it
settled, with its deviations, is in
[section 11](#11-implementation-notes). Builds on the [runtimes spec](runtimes.md), the
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
"capabilities": {
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
- **Output.** Tags, grouped by machine and then by runtime (format
  settled 2026-10-21):

  ```text
  <runtimes>
    <!-- global is attached to this session's root, so every subagent in this tree has it, whether or not its capabilities list it. -->
    <global/>
    <machine name="yunhao-labserver" platform="linux">
      <runtime id="local" connected="true" home="/home/u">
        <capability name="coding" takes_workspace="true" description="Read, write, edit, and run commands in a workspace"/>
        <attachment name="local_cookie_agent" capability="coding" workspace="/home/u/src/cookie-agent" status="ready"/>
      </runtime>
    </machine>
    <machine name="labpc" platform="linux">
      <runtime id="labpc" connected="false" starts_on_use="true" home="/home/u">
        <capability name="coding" takes_workspace="true" description="Read, write, edit, and run commands in a workspace"/>
      </runtime>
    </machine>
    <unplaced>
      <runtime id="roamer" connected="false" message="runtime `roamer` disconnected: the other end closed the connection"/>
    </unplaced>
  </runtimes>
  ```

  - **`global`** is never listed as a runtime, because it cannot be named
    in `capabilities`. A single `<global/>` element, with the comment
    above it, appears exactly when the root session of the caller's tree
    has `global` attached. It is left out otherwise.
  - **`<capability>`** carries the capability's own `description`, from
    the `description` the runtime reports for each capability (as in
    `runtimes.list`).
  - **`starts_on_use="true"`** marks a template runtime that is not
    running and that the daemon starts when a session needs it. It
    replaces a `lifetime` attribute: whether a runtime is built in,
    spawned, or dials in tells the model nothing more it can act on.
  - **`<attachment>`** elements are the calling session's own attachments,
    and nothing else (decided 2026-10-21). Instances that other sessions
    have open are left out: they say nothing about where this session can
    work, and they would show other sessions' workspaces. Any runtime can
    still be named in `delegate_subagent`'s `capabilities`, opened or not.
  - **Paths are absolute.** `home` is printed so the model can relate a
    path to the machine's home directory.
  - Attributes with no value (`home`, `platform`, `message`,
    `starts_on_use`) are left out. Values are XML-escaped.
  - A runtime with no known machine goes under `<unplaced>`.
- **Timing.** Its result describes the daemon at call time. A runtime that
  dials in or drops out later shows from the next call. `delegate_subagent`
  checks its `capabilities` against the same live state when it is called.

## 5. Clients

### 5.1 Web

Superseded on 2026-10-23: the draft shows its cards above the composer
and has no chip (see the [addendum](#addendum-2026-10-23-the-web-draft-shows-its-cards-and-the-first-message-creates-the-session)).

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

### 5.3 Where a session works: badges from its subagents

Added 2026-10-21. A root with only `global` shows no badges, yet its
subagents work on `labpc` and in `~/api`. The session's badges should say
so.

- **The union.** A root session's badges are built from its own
  attachments together with every direct child's attachments, with
  duplicates (same runtime, capability, and workspace) removed and
  `global` ignored as before. The usual badge rules then apply (projects
  spec section 9): one machine shows `[machine]` and its workspaces, and
  several machines show their machine badges.
- **Depth 1 is enough.** Only depth-1 children can be given capabilities
  (section 2), and deeper children inherit from their parent
  (section 2.3). So the direct children's attachments already cover the
  whole tree, and nothing deeper is read.
- **Kept with the root, not computed per listing.** `session.list` stays
  metadata-only (tree-local sessions spec), so the union cannot be built
  by reading the children at listing time. When a child is created, the
  daemon adds any of its attachments the root does not yet record to a new
  `SessionMeta.delegated_attachments`, which is folded from a durable
  event on the root's log. A cache rebuild keeps it, and the change is
  announced with `sessions.changed` `upserted`. The cost is one small
  event per child that brings new attachments, and none when a child only
  inherits.
- **Order.** The root's own attachments come first, then delegated ones
  in the order children brought them. Machine badges follow that order.
- **It only grows.** A child that finishes or is evicted leaves its
  attachments in the union: the badges say where the session has worked.
  A detach on the root removes only the root's own attachment.
- **A child's own row** in the subagent tree shows badges from its own
  attachments only.
- **The TUI** uses the same union for the project and workspace label in
  its session picker rows.

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

- **Tools.** The tool schemas are model-facing, so the delegation changes
  need no protocol version. Children record their attachments with the
  existing `runtime_attached` events.
- **Client protocol minor** (additive), for section 5.3:
  `SessionMeta.delegated_attachments` and the root event it is folded
  from. The event is also a plugin protocol minor, since plugins see
  events. Older daemons send nothing, so clients show the root's own
  attachments, as today.
- **Old logs.** Children that inherited the root's attachments keep them.
  Replay reads attachments from each child's own events.
- `docs/site/reference/tools.md` is regenerated for `list_runtimes` and
  the new parameter.

## 9. Phases

1. **Engine:**
   - first, the registry remembering where runtimes ran (memory spec
     section 16.8), so offline runtimes keep their machine in
     `list_runtimes`, badges, and the Machines view;
   - the `capabilities` parameter and its per-session schema;
   - starting runtimes for a child;
   - parent inheritance;
   - `delegate@runtime` checks;
   - `list_runtimes`;
   - the prompt paragraph;
   - tests;
   - the tools reference.
2. **Web:** a global-only New session, the "Runtimes…" chip and sheet,
   project prefill through the chip, and badges from the union of the
   root's and its subagents' attachments (section 5.3). The daemon's
   `delegated_attachments` belongs to phase 1.
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

## 11. Implementation notes

Implemented 2026-10-21 in three phases: the engine, daemon, and protocol
(client protocol 1.10.0, plugin protocol 1.4.0), the clients, and the docs
([Delegating to runtimes](../sessions/delegation.md)). The registry
remembering where runtimes ran ([memory spec section 16.8](memory.md#168-addendum-2026-10-21-the-registry-remembers-where-runtimes-ran))
landed first.

Where things are:

- **The parameter and its checks:** `crates/engine/src/base_tools/delegate.rs`
  (the per-session schema, `check_capabilities`, the prompt paragraph) and
  `crates/engine/src/runtime/runtime_delegation.rs` (`CapabilitiesParam`,
  `parse_capability`, `check_delegated_capabilities`,
  `open_delegated_capabilities`, `inherited_attachments`,
  `record_delegated_attachments`, and `list_runtimes_text`).
- **`list_runtimes`:** `crates/engine/src/runtimes_tool.rs`, a provider of the
  `global` runtime bound to the engine like the memory tools.
- **Per-runtime permission checks:** `PreparedTool::with_permission_runtimes`
  (`crates/runtime_api`) and `strictest_decision` in
  `crates/engine/src/runtime/tool_execution.rs`.

Deviations and readings of the spec:

- **The parameter is `capabilities`**, as decided; the schema block in
  section 2.1 still shows the earlier name.
- **Absent means refused as unknown.** `delegate_subagent` keeps one
  argument type, so a call that passes `capabilities` where the schema has
  none is refused by the provider with serde's unknown-field wording.
- **Checks against the live listing** cover what the daemon knows: a
  runtime that has never connected lists no capabilities yet, so its
  capability and workspace are checked only by starting it. A missing
  workspace for a capability that takes one, and a workspace for one that
  takes none, are refused before anything starts.
- **Runtimes start before the admission lock and the reservation.** A slow
  template does not hold up other delegations, and a runtime that does not
  start leaves no reservation. The capabilities are not part of the durable
  delegation reservation: a redelivered call computes them again from its
  arguments, and a child that already exists keeps the attachments it
  recorded.
- **Deeper children** inherit their parent's attachments other than
  `global`, plus the root's `global` attachment when the root has one now,
  so that `global` follows the root at every depth.
- **Permission evaluation.** Each runtime a call names is evaluated as the
  agent's `delegate` rules with that runtime as the scope, and each resource
  takes the strictest of the runtimes' evaluations, so the approval request
  keeps one evaluation per resource. The request names no runtime; the
  runtimes and workspaces show in the call's presentation, as a `runtimes`
  entry. `delegate@<runtime>` became an accepted permission key; `message`
  and `skill` still take no selector.
- **`list_runtimes` lists `runtimes.list`'s runtimes:** the built-in and
  configured ones and the dial-in runtimes that connected since the daemon
  started. A dial-in runtime remembered from before a restart is in
  `machines.list` but not in `list_runtimes` until it connects, since it
  cannot be delegated to before then. Machines are ordered with the
  daemon's own first and the rest by name, runtimes by ID within a machine;
  a machine whose runtimes report different platforms puts the platform on
  each runtime. An attachment whose instance is not open reports
  `status="stopped"`. The output is also shown to clients as a Markdown
  code block.
- **Delegated attachments.** The root event is
  `delegated_attachments_added { child_session_id, attachments }`, a
  runless state event; `attachments` are `RuntimeAttachment`s as the child
  named them, at least one. The fold keeps one entry per runtime,
  capability, and workspace. A fork leaves the event behind with its
  source's subagents, as `record_omitted`. A child is checked again on a
  redelivery, so a crash between creating the child and recording its
  attachments is repaired by the redelivery.
- **The TUI** uses the union through `RuntimeHomes::session_place` in its
  session picker, which shows the delegated workspaces only for a root with
  no workspace of its own; its machines view lists remembered runtimes, as
  `machine_entries` now does for both clients.
- **The web client** names the sheet's button **Use these runtimes**
  rather than Start. A draft without runtimes of its own in a project whose
  defaults attach runtimes is created with `runtimes` omitted, so the
  daemon attaches the project's defaults; the chip and the draft line show
  them. A draft whose sheet deselected `global` shows **no runtimes**.


### Addendum 2026-10-22: offline runtimes are refused, connections are checked

An incident showed two gaps in "a dial-in runtime must be connected"
(section 2.2). A root delegated to a dial-in runtime on a PC that had gone
away without closing its WebSocket. The daemon still counted the runtime
as connected, so the first call waited 30 s for `capability/open`, and the
second reused an instance opened earlier over the same dead connection:
its child started, prepared a call with `tool/prepare`, and waited for the
answer until the daemon restarted 17 minutes later. Decided and
implemented:

- **Never create, attach, or delegate with an offline runtime** (user
  decision). A dial-in runtime that is not connected is refused at once,
  for `capabilities` as for `session.create`, `session.runtime.attach`, and
  a project's default runtimes, with
  ``runtime `<id>` is not connected (last seen <time>)``; a runtime the
  daemon remembers from before a restart is refused the same way rather
  than as unknown. A template runtime is started and must be online,
  initialized and its capability opened, before the child exists; a start
  that fails or times out fails the call and creates nothing.
- **An instance is handed out only over a live connection.** Reusing an
  open instance checks its connection first: one heard from in the last
  15 s passes, another must answer `runtime/ping` within 10 s or it is
  ended. A dial-in runtime is then refused as not connected; the daemon
  waits up to 2 minutes for a template runtime it restarts.
- **Liveness and deadlines** (runtime protocol 1.4.0, see
  [Liveness](../reference/runtime-protocol.md#liveness)): the daemon pings
  a connection quiet for 15 s and ends one silent for 45 s; `tool/prepare`,
  `tool/revalidate`, and `runtime/context` fail as `runtime_unavailable`
  after 30 s and mark the connection suspect (checked within 10 s);
  `tool/execute` is bounded by liveness. A dial-in runtime pings its end
  over WebSocket and dials in again after 45 s of silence.

### Addendum 2026-10-23: the web draft shows its cards, and the first message creates the session

Requested by the user and implemented the same day; it replaces the chip
and sheet of section 5.1 and the **Use these runtimes** button of
section 11.

- **No Start button.** A new session shows its runtime cards where its
  transcript will be, with the composer under them, so the first message
  is drafted right away with everything the composer offers (files,
  agent, model, preset, permission mode, and rules). The "Runtimes…" chip
  and the cards' sheet are gone, and **Runtimes** in the ⋮ menu points at
  the cards while a session is drafted.
- **The first message creates the session.** It calls `session.create`
  with the selected cards, at the workspaces as typed, plus `global` when
  selected; the daemon starts every runtime that is not running (templates
  included) and creates nothing if one fails. A draft still on its
  project's defaults omits `runtimes`, so the daemon attaches them. The
  message is submitted to the new session; files attached to the draft
  wait in the page and are uploaded into the session first.
- **A refusal keeps the draft.** No session is made, the cards stay, and
  the message and its files go back into the composer. The error shows on
  the card of the runtime it quotes (`` `id` ``, as the daemon names
  runtimes), with a line above the composer; an error that names no
  runtime shows in that line. A message the created session refuses comes
  back to that session's composer, as any refused submit does.

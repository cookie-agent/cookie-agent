# ACP agent permissions: cookie decides what its rules decide, the agent asks

Status: **approved** (2026-11-01), being implemented. The user decided it on
2026-11-01 (section 7). It ships before
[ACP agents across several machines](acp-several-runtimes.md), which
relies on it. It replaces section 8 of the [ACP agents](acp-agents.md)
spec, the agent's own permission requests.

## 1. Problem

An ACP agent's calls meet two gates. Its harness asks in its own way: in
Claude Code's and Codex's auto modes, a classifier or a sandbox decides
most calls. Then cookie maps the same calls onto its own rules, and a rule
that says `ask` runs cookie's `approval` reviewer (a utility-model call)
in cookie's default `auto-approve` mode. When the harness has already
passed a call, that second review costs a model call and a wait and adds
nothing.

## 2. Two checks

Cookie's rules decide what they can, and only what they leave open reaches
the user.

1. **A call of cookie's tools that reaches the bridge** has already passed
   the harness's own check. Before it runs, cookie checks it against its
   rules. If they point to `deny`, the call is refused, and the agent gets a
   denied tool result naming the rule, as a model would. Otherwise it runs.
   Nobody is asked, and an `ask` rule or a default `ask` lets it run.
2. **A permission request from the harness** (`session/request_permission`)
   is matched against cookie's rules first. `deny` rejects it and `allow`
   allows it once, at once. Only a request that an `ask` rule matches, or
   that no rule matches, is shown to the user as an approval, with the
   harness's own options as buttons in its order. The user's choice goes
   back as that option.

That is all. Cookie's permission modes (`auto-approve`, `auto-approve-n`,
`auto-approve-y`, `ask`, `yolo`) and the `approval` reviewer do not apply
to ACP sessions. The tree's mode is unchanged and still governs the
native-model sessions in the tree.

## 3. Check 1: cookie's tools

- **Hidden as today.** A tool the merged rules deny everywhere is not listed
  over the bridge.
- **The rules of the call's own scope.** A bridged call is prepared where it
  runs, so its rules are those of that attachment: `bash@labpc`, with
  `${workspace_dir}` as labpc's workspace and `~/` as labpc's home.
- **Before it runs.** The check is the same preparation and decision a
  model's call gets, stopping at `deny`. What would have been an approval
  runs instead. The call shows as a cookie tool call of the agent's turn,
  with its outcome.
- **Doom-loop guard.** Unchanged: it holds for bridged calls as for a
  model's.

## 4. Check 2: the harness's requests

- **Matched first.** A request is mapped by its ACP tool kind onto
  cookie's actions, as section 8 of the ACP agents spec maps it today:
  - `read` and `search` become `read`, on the location's path;
  - `edit`, `delete`, and `move` become `write`, on the path;
  - `execute` becomes `bash`, on the command;
  - `fetch` becomes `webfetch`, on the URL;
  - a tool of an MCP server the agent connected itself becomes `mcp`, on
    `<server>_<tool>`.

  It is matched against the agent's merged rules (defaults included) in
  the host runtime's scope, with the agent's `cwd` as the workspace:
  - **`deny`** rejects it at once with the harness's reject-once option,
    and the transcript names the rule;
  - **`allow`** allows it once at once, so `bash: {"git *": allow}`
    keeps `git` from interrupting the user, and the default
    `write: {"${workspace_dir}/*": allow}` lets edits in the workspace
    through. A tree grant ("allow for the session tree", from a
    native-model session's approval) counts as `allow`, as
    `decide_operation_with_grants` already does for ACP requests;
  - **an `ask` rule, or no rule at all,** shows the request to the user.
    So do a kind that maps onto no action (`switch_mode`, `other`, an
    unknown kind) and a file request that names no path. `think` is
    allowed at once, as today.

  A request with several resources is allowed only when every one is,
  and rejected when any one is denied.
- **The harness's options.** Every option the harness offers is shown,
  "always allow" included, and the harness remembers it. This replaces the
  2026-10-23 rule that cookie answers only with once options, and it holds
  for requests about cookie's tools too (decided 2026-11-01). The guide
  warns that the harness remembers an "always allow" per tool rather than
  per argument: one for cookie's `bash` stops the harness asking about
  cookie's `bash` on every machine, and only check 1's `deny` rules still
  apply.
- **Reject with a note.** The approval keeps cookie's "reject with a note"
  button (decided 2026-11-01). It answers with the harness's reject-once
  option, and the note goes into the agent's running turn as a steered
  message, the way a message typed mid-turn does, so the agent reads why.
- **Names as the user knows them.** A request for one of cookie's tools
  shows the tool by cookie's name (`bash`, not `mcp__cookie__bash`), with a
  `cookie` badge saying it is one of cookie's tools and its machine when it
  has one. A request for a tool of an MCP server the agent connected itself
  shows `<server>_<tool>`, cookie's name for it, with the server as its
  badge. The harness's own tools show the request's title, as today.
- **A request for one of cookie's tools** (`mcp__cookie__<tool>`) is
  matched by the rules of the call it names, in that call's scope
  (`bash@labpc`), rather than by the kind mapping. It is no longer allowed
  at once. When it is shown, it carries cookie's preview of the call (an
  edit's diff, or the command and its machine), prepared as the bridged
  call will be. The bridged call that follows meets check 1 as usual, so
  the user is asked at most once.
- **Waiting.** A request waits for an answer like any approval, with push
  notifications, and `[approval] timeout_ms` applies (a request that expires
  is rejected once). This holds in every ACP session, a cron run's or a
  delegated agent's included. An automated run that should not stop picks
  the agent's mode for it (`acp.mode` in its document, or the cron
  template's agent options), or the harness's own allow rules.
- **The agent's mode is the session's permission mode.** The
  permission-mode control of an ACP session shows and sets the agent's mode:
  Claude Code's `default`, `acceptEdits`, `auto`, and `bypassPermissions`;
  Codex's `read-only`, `auto`, and `full-access`. It is the same option the
  agent options already set. The TUI's bottom bar and the web's mode picker
  show the agent's modes in place of cookie's, with a hover saying that the
  agent's harness decides what it asks.

## 5. What changes for rules

- **`deny` and `allow` work on both paths.** A cookie
  `bash: {"rm -rf *": deny}` stops cookie's `bash` (check 1) and the
  harness's Bash when the harness asks about it (check 2). It does not stop
  a call the harness runs without asking, in its auto or bypass modes,
  which never reaches cookie. Limits that must hold there go in the
  harness's own settings (Claude Code's permission rules, Codex's sandbox).
- **`allow` matters for the harness's requests only.** A bridged call runs
  whenever it is not denied, so an `allow` there changes nothing.
- **`ask` means "show it to the user"** for a harness request, the same as
  no rule. For a bridged call it means nothing.
- **What this trusts.** Cookie's tools reach every attached machine, and
  beyond them through `attach_machine`, `transfer`, cron, and memory.
  Between the agent and those calls there is only cookie's `deny`, plus
  whatever the harness asks about MCP calls in its current mode. Root runs'
  defaults include `attach: allow`. `delegate_subagent` stays denied by
  default, so it stays hidden. Claude Code asks about MCP tools in its
  default mode. Whether Codex reviews or asks about MCP calls in `auto` is
  unverified, since its sandbox guards its own shell and the bridge runs
  outside it. The probes (section 6) find out, and the ACP agents guide
  lists the results and suggests `deny` rules for documents that want
  cookie-only powers off (`attach: deny`, `cron: deny`, `transfer: deny`,
  `bash@prod: deny`).
- **Gone:** the approval flow and reviewer for harness requests, and ACP
  sessions making grants (their approvals are the harness's). The kind
  mapping (`MappedRequest`, `acp/permissions.rs`) stays. Its `Ask` arm now
  shows the request to the user directly.

## 6. The probes

Before implementation, one live run per agent and mode, on a cheap model
(Claude Code on a Haiku model, Codex on `gpt-6-luna`). The agent makes a
risky call of cookie's `bash` and of its own shell, and the run records
which calls reach cookie as permission requests. The results go in the
ACP agents guide's per-agent table. They change nothing in cookie's
behaviour, which is the same for every agent.

## 7. Decisions

**Decided by the user, 2026-11-01:**

- **Two checks:**
  - **Check 1:** a call of cookie's tools that reaches cookie is checked
    against its rules. It is refused on `deny` and otherwise runs.
  - **Check 2:** a harness permission request is matched against
    cookie's rules. `deny` rejects it and `allow` allows it at once. Only
    an `ask` match, or no match, is shown to the user, with the harness's
    options.
- Cookie's modes and the reviewer do not apply to ACP sessions. The
  agent's mode is the session's permission mode.
- No cookie action keeps asking. The user accepted that `attach_machine`,
  `transfer`, cron, and memory run unless denied (section 5).

Rejected along the way:

- skipping the reviewer only in "vouching" auto modes;
- allowing undecided requests in unattended sessions;
- allowing them when the agent's mode name contains `auto`, `bypass`, or
  `yolo`;
- showing every harness request with no rules applied.

- Every harness option is shown, "always allow" included, for cookie's
  tools as for the harness's own (section 4).
- "Reject with a note" stays: reject once, then the note steered into the
  turn (section 4).
- Approvals for cookie's tools show cookie's name for the tool with a
  `cookie` badge, never the `mcp__cookie__` form (section 4).

My own calls, not the user's:

- tree grants counting as `allow` (section 4);
- a request for a cookie tool matched by that call's own rules (section 4).

## 8. Internal agents in ACP sessions

With this spec, the only internal agent that runs for an ACP session is
`title`:

- **`approval`** no longer runs.
- **`context_compaction`** never ran. `session.compact` sends the agent its
  own `compact` command, or is refused (`acp/compaction.rs`), and automatic
  compaction belongs to the model loop.
- **`title`** runs once, at the end of a root session's first turn, when the
  session still has no title: the agent sent none in `session_info_update`,
  and the user set none. Delegated sessions never run it.

## 9. Protocol, history, configuration

- **Client protocol:** a minor bump. An ACP approval request gains the
  harness's options (id, name, kind) and, for a tool call, the tool's
  display name and source (cookie, or an MCP server), and its answer may
  name one. Clients
  that do not know them see the approve and reject they have today, mapped
  onto the once options. The permission mode of an ACP session is the
  agent's mode option, which clients already have. A
  `session.set_permission_mode` naming an ACP session changes the tree's
  mode only, for its native-model sessions.
- **History:** no format bump. An answer naming an "always" option is
  recorded as the option the agent got, in the approval events' open
  fields. Old sessions replay their recorded decisions as they are.
- **Configuration:** no new keys and no new diagnostics.

## 10. Tests

- Check 1:
  - a bridged call a `deny` matches gets a denied tool result naming the
    rule, without an approval;
  - one an `ask` rule or a default matches runs without one;
  - a deny-everywhere tool is not listed;
  - `bash@labpc: deny` refuses a call routed to labpc and not one routed
    to the host.
- Check 2:
  - a harness request a `deny` matches is rejected, and one an `allow` or
    a tree grant covers is allowed once, both without an approval;
  - one an `ask` rule matches, one no rule matches, and one of an unmapped
    kind are shown with the harness's options, and the chosen option,
    "always" included, is what the agent gets back;
  - a request with one denied resource is rejected, and one with an
    unallowed resource is shown;
  - a request for a `cookie` tool is matched in its call's scope
    (`bash@labpc: deny` rejects it), shows cookie's preview when shown, and
    its bridged call is not asked about again;
  - a request in a cron run's new session waits and expires under
    `[approval] timeout_ms`.
- No `approval` run starts for an ACP session in any tree mode, and the
  tree's mode still governs a native-model session in the same tree.
- The `title` agent runs only when the agent sent no title.

## 11. Phases

1. The probes (section 6).
2. Engine: check 1 stopping at `deny`, check 2 deciding `deny` and
   `allow` and showing the rest with its options, the reviewer and modes off
   for ACP sessions.
3. Protocol and clients: the harness's options on approvals, cookie's
   preview for `cookie` tools, the agent's mode as the permission mode in
   both clients.
4. Docs: the ACP agents guide (permissions, the per-agent probe table,
   suggested `deny` rules), the permissions concept page, and an addendum
   to the ACP agents spec's section 8 pointing here.

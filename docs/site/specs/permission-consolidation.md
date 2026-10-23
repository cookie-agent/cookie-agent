# Permission consolidation

Status: **draft** (2026-10-23), awaiting the go-ahead to implement.

## 1. Goal and scope

A run's permissions come from three levels: the defaults the daemon
supplies, the agent document, and the session's overrides. Today the engine
combines them in three different ways, depending on what it is deciding:

- **which tools are offered:** any rule at any level that allows or asks;
- **whether a call runs:** a matching session rule beats every document
  rule, whatever their patterns;
- **what `/permissions` shows:** a merged table, where a session `*` rule
  hides the document's rules for that action.

So the table can show `cron * deny (session)` while the cron tools are
still offered and every call is refused. A session override cannot turn off
a tool that a default allows (memory, cron, `list_runtimes`, `mcp`).

This spec merges the three levels into **one rule list per run**, and
makes every decision read that list.

**Decided 2026-10-23:**

- **Session overrides the agent document, which overrides the defaults.**
- **Document over defaults** stays per action: a document that names an
  action replaces that action's defaults entirely (unchanged).
- **Session over document** is per rule (option B): a session rule replaces
  the rule with the same action, runtime selector, and pattern, and every
  other rule is kept.
- The merged list decides the tools offered, every call, the memory
  snapshot, and what `/permissions` shows.

**Out of scope:** new permission actions, changing defaults, approval
grants ("allow for this session"), which keep their current role (section
4.4).

## 2. The levels today

- **Defaults:** `mode_default_rules`, `memory_default_rules`, and
  `cron_default_rules` in `crates/engine/src/permissions.rs`, chosen by the
  agent's mode and whether the run is delegated. Each applies only to an
  action the document names no key for.
- **Agent document:** its `permissions` frontmatter. The document's rules
  plus the defaults are frozen into the run's policy
  (`agent_permission_rules`).
- **Session overrides:** `SessionPermissionOverlay`, a list of single
  rules. `/permissions` and the web panel add or remove one rule at a time
  (`set_session_permission`, `clear_session_permission`), identified by
  action, runtime selector, and pattern. It starts empty and holds only
  what was changed.

## 3. The merged list

### 3.1 Building it

For a run, the merged list is built in three steps:

1. **Defaults** for each action the document names no key for.
2. **The document's rules.** Steps 1 and 2 are today's frozen policy.
3. **The session's rules.** A session rule whose **identity** (action,
   runtime selector, pattern) equals a rule from steps 1–2 replaces it. Any
   other session rule is added.

Each rule keeps its **source**: `default`, `agent_document`, or
`session_overlay`. The list is ordered defaults, document, session, which
matters only for ties (section 3.2).

Identity compares patterns as written, after the memory variables are
expanded (the policy already holds them expanded) and `~/` paths are
resolved the way matching resolves them. `bash@labpc` and `bash` are
different identities.

### 3.2 Deciding with it

- **Is a tool offered?** It is offered when some rule in the merged list
  for its action, applying to its runtime, allows or asks. Nothing else
  counts.
- **Is a call allowed?** The most specific matching rule wins: more
  literal characters, then fewer wildcards, then a runtime selector, then
  the later rule in the list. This is today's rule within a document,
  applied to the merged list. A session rule wins a tie because it comes
  last.
- **Checks without a resource** (an action checked by name only) use the
  merged list's `*` rules, a runtime-selected one over a bare one.
- **Memory:** the write tools' readable-and-writable test (memory spec
  section 16.9.1) and the snapshot's read rules use the merged list.
- **`/permissions`** shows the merged list: per action, its `*` row and
  its patterns, each with its source. A session `*` rule replaces only the
  `*` row; the document's other patterns stay listed.

### 3.3 Examples

| Session override | Document and defaults | Merged | Tool | A call |
|---|---|---|---|---|
| `cron: deny` | default `cron: "*": allow` | `"*": deny` (session) | hidden | — |
| `memory: {"user://*": deny}` | default `memory: "*": allow` | `"*": allow` (default), `"user://*": deny` (session) | offered | `user://` denied, the rest allowed |
| `bash: {"cargo test *": allow}` | `bash: {"*": ask}` | both | offered | `cargo test` runs, the rest asks |
| `bash: {"*": allow}` | `bash: {"*": ask, "git push *": deny}` | `"*": allow` (session), `"git push *": deny` (document) | offered | `git push` denied, the rest runs |
| `bash: {"git push *": allow}` | as above | `"*": ask`, `"git push *": allow` (session) | offered | `git push` runs |
| `mcp: deny` | default `mcp: "*": ask` | `"*": deny` | hidden | — |

## 4. What changes

### 4.1 Session rules no longer win by level alone

Today any matching session rule beats the document. With the merged list,
specificity decides, so a broad session rule no longer overrides a narrower
document rule: the fourth row of section 3.3 denies `git push`, where today
it runs. To change a document's rule for a session, the session overrides
that exact rule, which is what the per-row editing in `/permissions` and
the web panel already does.

### 4.2 A session can turn tools off

A session `deny` that replaces every allowing rule hides the tool, as the
first and last rows of section 3.3 show. The docs' note that turning memory
or cron off "has to happen in the agent document" is removed.

### 4.3 The table matches the engine

A session `*` rule no longer hides the document's patterns in
`/permissions`; they stay listed with their source, because they still
apply.

### 4.4 Grants are unchanged

An approval answered "allow for this session" records a grant, which still
turns an `ask` into `allow` for what it covers and can still make a tool
offered, but never overrides a `deny` of the merged list
(`tool_visible_with_grants`). Grants are invalidated when a session change
makes an action stricter, as today, with "stricter" judged on the merged
list.

### 4.5 Running and frozen runs

A session override already applies to the next decision without
re-freezing the run. The merged list is computed from the frozen policy and
the current overlay at each decision, so that stays true. Tools offered are
recomputed at every model request, as today.

## 5. Compatibility

- **No wire change.** `EffectivePermissionAction` and its rule sources
  already describe a merged table; only which rules it lists changes
  (section 4.3).
- **Behaviour change** (section 4.1) noted in the protocol changelog and
  the permissions docs, with no version bump: no message or field changes.
- **History:** recorded approvals and decisions replay as they were
  written.

## 6. Tests

- Building: replacement by identity across levels; runtime selectors as
  distinct identities; `~/` patterns; expanded memory variables; sources.
- Visibility: a session `deny` hides a default-allowed tool (memory, cron,
  `mcp`, `list_runtimes`); a narrower session `deny` leaves it offered;
  a session `allow` offers a document-denied tool.
- Decisions: every row of section 3.3, including specificity across
  levels and a tie won by the session.
- Name-only checks with session `*` rules, with and without selectors.
- Memory: the write tools' test and the snapshot follow the merged list.
- `/permissions`: a session `*` rule keeps the document's patterns listed.
- Grants: a grant never overrides a merged `deny`; invalidation on a
  stricter session change.

## 7. Docs

- `docs/site/concepts/permissions.md` and `docs/site/extend/agents.md`:
  the three levels, how they merge, and the behaviour change, with the
  examples of section 3.3.
- The memory and scheduling guides: drop "turning it off has to happen in
  the agent document".
- The protocol changelog: the behaviour change.

## 8. Phases

One phase: the merged list in the engine and every caller (permissions,
tool composition, skills, messaging, memory, cron, `list_agents`, the
view), tests, then docs.

## 9. Open questions

None.

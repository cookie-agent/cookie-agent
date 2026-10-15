# Runtimes: Daemon, Global Runtime, and Coding Runtimes

Status: **draft; product decisions approved 2026-10-15**. This document is the
governing specification for splitting tool execution and workspace context out
of the engine into attachable *runtimes*. Section
[Open questions](#14-open-questions) lists anything still undecided.

## 1. Goal and scope

cookie agent aims to cover both coding-agent use (opencode) and personal
assistant use (openclaw). Both need the agent's tools to live somewhere other
than the process that runs the model loop: a session may work in a repository
on another machine, in several workspaces at once, or with no workspace at all.

Today the daemon *is* a workspace. `EngineOptions.cwd` locates the session
store, roots permission paths, resolves relative tool paths, feeds the
`<working_directory>` prompt block, finds `AGENTS.md` and project skills, and
selects the workspace configuration layer. The built-in tools are constructed
once for that directory.

After this work the processes split three ways:

```text
clients (TUI, CLI, later others)
   │  client protocol (WebSocket / in-memory)
daemon = gateway
   │  config, agents, sessions, model loop, permissions, approvals,
   │  delegation, goals, messaging, compaction, artifacts
   │  base layer tools · global runtime (in-process)
   │  runtime protocol (stdio / in-memory)
coding runtimes, one per workspace (local in-process, remote via a command)
```

**In scope (the milestone):** the runtime seam, session attachments, the
runtime protocol, and remote coding runtimes launched by command. The
milestone ends when a session on the local daemon works in a repository on
another machine (labpc over ssh) end to end.

**Out of scope:** a persistent or discoverable daemon, runtimes that dial in,
channel adapters (chat apps), schedulers and other openclaw-style triggers,
web clients, and runtime kinds other than `coding`. Producer messages are the
only notification path, and they stay a daemon-side plugin feature.

## 2. Vocabulary

| Term | Meaning |
|---|---|
| **Daemon** | Today's daemon, unchanged in process model: plain `cookie` runs it in-process, `cookie daemon` serves it over WebSocket. It is the gateway. Its working directory now only locates the session store and stamps `cwd_identity`. |
| **Base layer** | Tools the daemon implements itself for its own orchestration. Always present, never detachable. |
| **Runtime** | A source of tools, workspace context, and producer messages. |
| **Runtime kind** | `global` or `coding` in this milestone. A runtime reports its own kind in `runtime/initialize`; templates do not declare it. |
| **Runtime template** | A configured way to start runtimes: `[runtimes.<id>]`. The `<id>` is the *runtime ID*. |
| **Runtime instance** | A running runtime: one per (runtime ID, canonical workspace). Shared by every session attached to it. |
| **Attachment** | A durable session fact binding a session to a runtime instance under a session-unique *attachment name*. |
| **Attachment name** | The name the runtime instance reports for itself in `runtime/initialize`; used as the tool prefix on collisions. |

## 3. Layers

| Layer | Runs in | Tools | Context |
|---|---|---|---|
| Base layer | daemon | `delegate_subagent`, `get_subagent_result`, `cancel_subagent`, `send_message`, `skill`, `goal_get`, `goal_update`, `read_artifact` (new) | none |
| Global runtime (`global`) | daemon process, in-process trait calls | top-level `[mcp]` servers and `[plugins]`, `webfetch` (declared `Fallback`) | user skills (`~/.agents/skills`, `~/.cookie-agent/skills`) |
| Coding runtime (`coding`) | built-in `local` instance in the daemon process; others in child processes | `read`, `write`, `edit`, `bash`, `webfetch`, plus MCP servers from its template | workspace root, platform, `AGENTS.md`, project skills |

Plugins exist only on the daemon, hosted by the global runtime. MCP hosting
is a feature of the coding kind, not of the runtime contract: coding runtimes
host MCP servers, and future runtime kinds need not. No runtime hosts plugins.

Rules:

- The global runtime never offers `read`, `write`, `edit`, or `bash`. Those
  exist only where a coding runtime is attached.
- Every daemon always has the `local` coding runtime template for its own
  working directory. There is no flag to disable it.
- New sessions attach `global` and `local` (the daemon's working directory) by
  default. A session may detach either. A session with no coding runtime is
  valid: it has the base layer and whatever the global runtime offers.
- Artifact reads move from `read` to the base layer's
  `read_artifact { artifact, stream?, offset, limit }`, so a session with no
  coding runtime can still page through retained output. `artifact` is the
  64-hex digest and `stream` an optional stream name; the model-facing
  `artifact://<digest>[/<stream>]` URI form is removed. Truncation hints read
  `read_artifact(artifact="<digest>", stream="stdout", offset=…)`. `read`
  becomes filesystem-only. The persisted `artifact://sha256/<digest>` form in
  `ArtifactReference` is internal and unchanged.
- `read_artifact` needs no permission rule: it can only reach the session
  tree's own artifacts. The `read: {"artifact://*": allow}` pattern and the
  `artifact://` pass-through in permission resource resolution go away.

## 4. Configuration

### 4.1 Templates

```toml
[runtimes.labpc]
command = ["ssh", "labpc", "cookie", "runtime", "coding", "--stdio", "--workspace", "{workspace}"]
env = { }                     # additions to the inherited environment
startup_timeout_ms = 10000
shutdown_grace_ms = 3000

[runtimes.labpc.mcp.servers.rust_analyzer]
command = "rust-analyzer-mcp"
```

- Runtime IDs match `[a-z][a-z0-9_]{0,31}` because they become tool-name
  prefixes. `global` and `local` are reserved.
- `[runtimes.local]` may exist without `command`; it only carries the `mcp`
  table for the built-in local coding runtime. Any other template requires a
  nonempty `command`. Templates accept no `plugins` table.
- Templates do not declare a kind. The command decides what runs, and the
  runtime reports its kind in `runtime/initialize`, so there is one source of
  truth. The `mcp` table is still parsed strictly when the configuration
  loads; a runtime whose kind hosts no MCP servers rejects a nonempty one at
  initialize.
- A template whose command contains `{workspace}` needs a workspace: attach
  and session creation require one. A template without the placeholder takes
  none, and supplying one is an error.
- `{workspace}` is substituted as one whole argv element, with no shell
  interpretation by cookie. A command that reaches a remote shell (ssh) is
  responsible for quoting; the reference docs must say so.
- The child inherits the daemon's environment plus `env`, because `ssh` needs
  `SSH_AUTH_SOCK` and similar variables.
- Template MCP servers are sent to the runtime in `runtime/initialize` and run
  on the runtime's machine. MCP OAuth stays global-runtime-only (its
  credential store is the daemon's `mcp-oauth.json`); a template server with
  `oauth` enabled fails validation. Static-header HTTP servers are allowed.
- Top-level `[mcp]` and `[plugins]` keep their current meaning and belong to the
  global runtime.
- **Plugin environment changes:** plugins, like MCP servers and runtimes, now
  inherit the daemon's environment, and `env` adds to it instead of replacing
  it. This drops today's `env_clear()`, so a plugin also sees provider API keys
  that the daemon reads from its environment.

### 4.2 Removed

- The workspace configuration layer: `<cwd>/.cookie-agent/config.toml` and
  `<cwd>/.cookie-agent/agents/` are no longer read. Configuration is the user
  layer only. `config_paths.workspace`, `workspace_mcp_servers`,
  `McpConfigTarget::WorkspaceFile`, the TUI's "project config.toml" MCP target,
  and `AgentDocumentSource::Workspace` go away.
- `BuiltinTools::new(workspace)`: coding tools take their workspace from the
  runtime instance.
- The `lazy` MCP server setting and the `lazy-not-connected` status. Every
  enabled server connects eagerly (section 6.4); an authored `lazy` key is an
  unknown field.

No migration or compatibility decoder is added (see `AGENTS.md`). A leftover
workspace file is ignored silently because it is simply never opened; the
docs say so. `AGENTS.md` files under `.cookie-agent/agents/` are context, not
configuration, and keep working (section 8).

## 5. Session attachments

### 5.1 Events (additive, versionless)

- `RuntimeAttached { name, runtime_id, kind, workspace }` where `workspace` is
  the canonical path the runtime reported (absent for `global`).
- `RuntimeDetached { name }`.

A session's attachment set is the fold of these events. `SessionCreated`
stays as it is; creation writes one `RuntimeAttached` per initial attachment
in the same append.

### 5.2 Operations

- Attach and detach are accepted only while the session has no active run
  (`SessionRunning` otherwise), so the set is fixed for every run.
- Creating a session or attaching a runtime connects it eagerly (section
  6.4). Attach waits for the instance to start and for `runtime/initialize`;
  failure rejects the attach and records nothing.
- The runtime chooses its attachment name and returns it from
  `runtime/initialize`; clients do not supply one. The global runtime returns
  `global`. A coding runtime returns `<runtime ID>_<workspace basename>`, with
  the basename lowercased, every character outside `[a-z0-9_]` replaced by
  `_`, repeats collapsed, and the result capped at 32 bytes: labpc in
  `~/cookie-agent` is `labpc_cookie_agent`. Because the name is fixed per
  instance, it is the same in every session that attaches that instance.
- If a session already has an attachment with the returned name (two
  workspaces with the same basename), the daemon appends `_2`, `_3`, and so on
  for that session.
- Delegated children inherit the parent's attachments. `delegate_subagent`
  accepts an optional `runtimes` list of the parent's attachment names that
  narrows the child's set. The child's log records its own `RuntimeAttached`
  events so the child is self-describing.
- Reopening a session in a daemon that does not have its instances running
  (after a restart, or a session created by another daemon) connects them
  when the session is opened, with the same rules as creation.

## 6. Tool composition

The model's tool list should change as rarely as possible: every change costs
the provider's prompt cache and can confuse a model that has already called a
tool by name.

### 6.1 Minimal change, not frozen

The tool set is not frozen. As today, `published_tool_definitions` recomputes
it on every model turn. Instead, the sources of change are reduced so that the
list only changes when something really appeared or went away:

- no lazy connections: everything a session can use is connected when the
  session is created or opened (section 6.4);
- short disconnects are bridged by reconnecting before any tool is dropped
  (section 6.4);
- only real collisions rename tools (section 6.3).

What remains is the user attaching or detaching a runtime, an MCP server or
runtime that cannot be reached at all, and an MCP server changing its own
tool list.

### 6.2 Collision enum

Every runtime tool declares `collision: prefix | fallback` (default `prefix`).

- `prefix`: on a collision the tool is published as `<attachment>_<name>`.
- `fallback`: on a collision the tool is hidden. The global runtime's
  `webfetch` uses this.

Base layer names are reserved: a runtime tool with a base name is always
treated as colliding.

### 6.3 Collisions

Names are computed from the current tool set every time it is composed:

- A name offered by exactly one source is published bare.
- When several `prefix` tools offer one name, the bare name is removed and
  each of them is published as `<attachment>_<name>`. Attaching labpc at
  `~/api` to a session whose `local` runtime is at `~/web` therefore turns
  `bash` into `local_web_bash` and `labpc_api_bash`; detaching labpc turns
  `local_web_bash` back into `bash`.
- A runtime tool that collides with a base layer name is published as
  `<attachment>_<name>`; the base tool keeps its name.
- `fallback` tools are hidden whenever a non-fallback tool offers the same
  name. When both declare the same parameters schema, as both `webfetch`
  tools do, the model sees no change.
- A published name longer than 64 bytes is dropped with a diagnostic.
- Delegated children compose the same way from their own attachments.

Inside one runtime, names are unique; that runtime enforces it with today's
MCP claim rules. MCP tools inside a runtime keep the `<server>_<tool>` form, so
a prefixed one reads `<attachment>_<server>_<tool>`.

### 6.4 Connection policy

The same rules apply to MCP servers (in the global runtime and in coding
runtimes) and to runtime instances.

- **Eager connect.** When a session is created or opened, the daemon makes
  sure every attached runtime instance is started and every enabled MCP server
  in it has finished its connection attempt before the session's first run.
  Instances and servers already up are reused; the global and `local`
  instances start with the daemon.
- **Failure at connect.** A server or runtime that fails to connect publishes
  no tools, and every client viewing the session gets an error message naming
  it and the reason. It is retried the next time a session that uses it is
  created or opened, and on demand from the `/mcp` or `/runtime` panel.
- **Disconnect.** When a connected server or runtime drops, the daemon
  reconnects (restarting the process for stdio servers and runtimes) up to 3
  times with 1 s, 2 s, and 4 s backoff. While it reconnects, its tools stay
  published and calls fail fast with `runtime_unavailable`, so a brief drop
  never changes the tool list. If every attempt fails, its tools are dropped
  and clients get an error message.

### 6.5 Routing

The name map resolves a published name to (attachment, runtime tool name).
Preparation, permission decision, and execution keep today's sequence;
`Arc::ptr_eq` publication checks become (attachment, instance generation, spec)
equality, so a runtime restart between publication and preparation fails with
the existing `OperationChanged` path.

## 7. Permissions and approvals

- Rules gain an optional runtime selector written as an action suffix in agent
  documents and session overlays: `bash@labpc`, `read@local`. A plain
  `action` with no selector applies on every runtime and to the base layer,
  so existing agent documents keep working unchanged. A qualified rule is more
  specific than an unqualified one at the same resource specificity.
- The selector matches the runtime ID, not the attachment name, so it is
  stable across sessions.
- Permission actions stay the fixed `PermissionAction` enum. Runtime tools
  declare one of them; the runtime protocol carries the enum.
- `${workspace_dir}` and relative resources resolve against the workspace of
  the runtime that prepared the call. Base layer and global tools have no
  workspace; path-relative patterns never match them.
- Approval grants and tree grants record the runtime ID. A grant only applies
  to calls on the same runtime ID. Grant events written before this change have
  no runtime ID and are read as `local`.
- The approval classifier's prompt names the runtime and its workspace instead
  of "the workspace".

## 8. Workspace context

At admission, for each attached coding runtime, the daemon asks the runtime
for its context (`runtime/context`):

- **Workspace block.** `<working_directory>` is replaced by one `<runtimes>`
  block listing each attachment's name, kind, workspace, platform, and whether
  its tools are prefixed. It is frozen like today's block.
- **AGENTS.md.** The admission request carries the run's preset. Each coding
  runtime applies today's lookup to its own workspace: the preset's
  `.cookie-agent/agents/<preset>/AGENTS.md`, else
  `.cookie-agent/agents/AGENTS.md`, then `<workspace>/AGENTS.md`, under the
  existing size rule. Presets themselves are still defined only by the
  daemon's user layer. `AgentMdLoaded` gains an `attachment` field; one event
  per runtime that returned a file. Replay and compaction pinning treat the
  latest event *per attachment* as current.
- **Skills.** The global runtime contributes user skills; each coding runtime
  contributes project skills found from its repository root down to its
  workspace (today's discovery, run on the runtime's machine). Bodies and the
  supporting-file sample travel in the context reply; `${COOKIE_SKILL_DIR}`
  expands to the absolute path on the runtime's machine. Project skills shadow
  user skills as today. Same-named project skills from two attachments follow
  the `prefix` rule. A skill's `allowed-tools` grants apply to the tools of the
  attachment that contributed it; user skills' grants are unqualified.

Skills are read once when an instance initializes, matching today's "no hot
reload". `AGENTS.md` is read at every root admission, as today.

## 9. Runtime protocol

JSON-RPC 2.0, one message per line over stdio, or frames over an
`InProcessStream`. The protocol version is an integer starting at 1 and must
match exactly; a mismatch fails initialize with both versions in the
diagnostic. The local coding runtime speaks this protocol over the in-memory
transport so the local and remote paths are one code path. The global runtime
does not; it is called in-process through the same Rust trait.

| Method | Direction | Purpose |
|---|---|---|
| `runtime/initialize` | daemon → runtime | Version, runtime ID, requested workspace (when the template takes one), and settings: `tool_output` and `agent_md` limits and the template's MCP servers. A runtime rejects settings its kind does not support. Reply: version, kind, attachment name, canonical workspace, platform, shell, tool declarations, skills. |
| `runtime/tools_changed` | runtime → daemon | Tool list changed; the daemon re-lists before the next model turn (section 6.1). |
| `runtime/context` | daemon → runtime | Per-admission context for the run's preset: `AGENTS.md`. |
| `tool/prepare` | daemon → runtime | Name, arguments, session, run, turn context. Reply: prepared ID, operation identity, permission resource, policy labels, presentation, display argument, serialization key, concurrency. |
| `tool/revalidate`, `tool/execute`, `tool/release` | daemon → runtime | Execute or discard a prepared call. `execute` replies with the terminal result and inline media (bounded by today's attachment budget). |
| `tool/progress` | runtime → daemon | Display deltas and output chunks under today's 64 KiB / 8-stream bounds. The daemon owns capture and artifact retention. |
| `tool/stdin`, `tool/cancel` | daemon → runtime | As today's `ToolStdin` and cancellation token. |
| `job/started`, `job/progress`, `job/completed` | runtime → daemon | Background jobs (bash `background`). The daemon commits completions as today. |
| `job/kill` | daemon → runtime | Revert or delete killed the owning call. |
| `runtime/shutdown` | daemon → runtime | Graceful stop, then termination after `shutdown_grace_ms`. |

`ToolPreparationContext.cwd` and `workspace_root` disappear from the daemon
side; the runtime fills them from its own workspace.

Producer messaging stays a daemon-side plugin feature. Runtimes report back
to sessions only through background-job completions; a later runtime kind
that needs general producer messages adds them to this protocol then.

## 10. Lifecycle and failure

- The `global` and `local` instances start with the daemon. Other instances
  start when a session that attaches them is created or opened, or when they
  are attached, and stay up until the daemon exits.
- Connection, reconnection, and failure follow section 6.4.
- Runtime stderr is captured into bounded diagnostics, as plugin stderr is.
- When a runtime drops, calls in flight fail with a new `runtime_unavailable`
  failure code and running background jobs complete as failed ("runtime
  exited"). Tool calls are never retried transparently.
- Instance status (`starting`, `ready`, `reconnecting`, `failed`, `stopped`)
  is visible to clients (section 11).

## 11. Client protocol

Protocol 38 → 39 (one bump per phase that changes the wire).

- `session.create` gains `runtimes: [{ runtime_id, workspace? }]` and
  `global: bool`. Omitted means `local` at the daemon's directory plus
  `global`.
- New `session.runtime.attach { session_id, runtime_id, workspace }` and
  `session.runtime.detach { session_id, name }`.
- New `runtimes.list`: templates and live instances with status. New
  `runtimes.changed` notification on instance status changes. `mcp.changed`
  covers servers in every runtime.
- Connection failures and drops from section 6.4 reach clients as these
  notifications with the failure reason; the TUI shows them as an error
  notice in each affected session.
- The TUI gains a `/runtime` panel (attached runtimes with status, attach by
  template and workspace path, detach) and shows attachment names in the
  session status line. The MCP panel shows servers from every runtime and
  edits global-runtime servers only.

## 12. Crate changes

This lands open review item 9 (extract `tool_api` into a leaf crate):

- **`runtime_api`** (new leaf crate): the tool trait, prepared-tool types,
  `ToolSpec` with `collision`, progress and stdin abstractions that do not
  depend on `OutputHub`, `OutputCapture`, or `ArtifactRouter`, and the runtime
  protocol types. Depends on `protocol` (or `identity`) only and knows
  nothing about MCP.
- **`mcp`** (new crate, from `engine/src/mcp.rs`): the MCP client and
  registry, shared by the global runtime in the daemon and by
  `coding_runtime`. OAuth stays on the daemon side.
- **`coding_runtime`** (from today's `tools`): `read`, `write`, `edit`, `bash`,
  `webfetch`, `fs_cap`, runtime-side MCP hosting, skills and
  `AGENTS.md` discovery, and the protocol server loop.
- **`engine`**: base layer tools (`delegate`, `message`, `skill`, `goal`,
  `read_artifact`) move in or into an engine-side module; it gains the runtime
  registry, attachment fold, composition, and the protocol client.
- **`cookie_agent`**: `cookie runtime coding --stdio --workspace <path>`. The
  subcommand needs no provider configuration and exits on stdin EOF.

## 13. Phases

Each phase lands with all gates green and updates the user-facing docs it
changes (`architecture.md`, configuration, protocol, tools, system prompt).

1. **Runtime seam, in-process, no wire change.** Extract `runtime_api`;
   split the base layer, the global runtime, and the `local` coding runtime
   behind one engine-side runtime abstraction. Move every
   workspace-semantics use of `store.cwd()` to the call's runtime:
   `working_directory.rs`, `agent_md.rs`, `tool_execution.rs` (preparation
   context, decisions, visibility), `skills.rs` visibility, `permissions.rs`
   overlay effects, `approval_flow.rs` text, and the MCP/plugin context digests.
   Remove the workspace configuration layer. Add `read_artifact`. Plugins
   inherit the daemon's environment.
   **Done when** the only `store.cwd()` reads left are the store location and
   `cwd_identity`.
2. **Attachments and composition (protocol 39).** Events, RPCs, defaults,
   collision prefixing, eager MCP connection with reconnect
   and drop notices (removing `lazy`), runtime-qualified permissions and
   grants, per-attachment `AGENTS.md` and skills, delegation narrowing, TUI
   panel. Tests attach two fake in-process coding runtimes with colliding
   tools.
3. **Runtime protocol.** Wire types, server loop in `coding_runtime`, client
   in `engine`; the `local` instance switches to it over `InProcessStream`.
   Background jobs cross the protocol.
4. **Remote runtimes.** `[runtimes.<id>]` templates, `cookie runtime`, eager
   start at session creation, reconnect and drop handling (section 6.4),
   template MCP servers, instance
   status. Verified by a test that launches `cookie runtime` as a real child
   process, then manually against labpc (installing on labpc needs the
   author's go-ahead).

## 14. Open questions

None. Every choice this draft made was reviewed on 2026-10-15.

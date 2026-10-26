# Managing Runtimes from Clients

Status: **draft** (2026-10-26); every decision in
[section 9](#9-decisions) made by the user on 2026-10-26. Builds on [runtimes](runtimes.md),
[runtime capabilities](runtime-capabilities.md), [ACP agents](acp-agents.md),
[MCP tool filters](mcp-tool-filters.md), [web MCP OAuth](web-mcp-oauth.md),
and the daemon restart of [engine configuration](engine-configuration.md)
section 11.

## 1. Goal and scope

Clients manage every connected runtime from the machines panel, not only
the daemon's own `global` runtime:

1. Read and change any runtime's configuration: its MCP servers and ACP
   agents.
2. Sign in to OAuth MCP servers that run on any runtime. The credentials
   stay on the daemon's machine. Both the TUI and the web client run the
   flow.
3. In the machines panel, expand a runtime to:
   1. list its MCP servers and each server's tools; add, remove, rename, and
      configure servers; leave out single tools and take them back;
   2. list its ACP agents; add, remove, rename, and configure them; open or
      create the agent's document;
   3. (proposal, section 5) configure MCP servers and ACP agents once for
      every runtime of a kind;
   4. restart a template runtime, or a dial-in runtime that can relaunch
      itself.

Out of scope: runtime protocol changes for plugins, and model-provider
sign-ins.

## 2. Today

- `config.get`/`config.apply` cover only the daemon's `config.toml` and agent
  documents. A **template** runtime's MCP servers and ACP agents are in that
  file (`[runtimes.<id>.mcp]`, `[runtimes.<id>.acp]`) and reach the runtime
  once, in `runtime/initialize` (`RuntimeSettings`). Editing them only lists
  `runtimes` under `restart_required`. Nothing restarts one template.
- A **dial-in** runtime (`cookie runtime serve --daemon`) reads
  `[runtimes.local]` from its own machine's `config.toml`. The daemon sends it
  no MCP servers (`DialInTemplate::connect`). The runtime polls ACP agents
  every 5 s and reads MCP servers once at startup. The daemon shows those
  servers as `source: machine` with no definition, and cannot change them.
  When `RuntimeSettings` carries servers or agents, the runtime uses them in
  place of its own (`coding_runtime` `initialize`), with no merge.
- `mcp.server.add/edit/remove/set_enabled` act on `global` only. Only
  `mcp.server.reconnect` takes `runtime_id`. `McpServerInfo` carries tool
  counts, not names, though the runtime reports names
  (`McpServerReport.tools`). Filtered-out names are not reported.
- MCP OAuth runs in the daemon, for `global` servers only. Its store is
  `<data_dir>/mcp-oauth.json`, keyed `server:sha256(resource URL)`. A runtime
  refuses a server with `oauth` enabled, and maps `NeedsAuth` to `Failed`.
  `mcp.auth.*` take a server name only.
- ACP agents reach clients as `AgentDescriptor.runtimes`. No RPC edits a
  runtime's agents.
- `RuntimeLifetime` is `builtin | dial_in | spawned`. It does not tell a
  persistent `cookie runtime serve` apart from a TUI-embedded runtime. No
  RPC restarts a runtime.

## 3. Configuration: the daemon owns every runtime's settings

**Decided 2026-10-26 (D1, D2):** every runtime's MCP servers and ACP agents
live in the daemon's `config.toml`. A dial-in machine keeps no runtime
settings of its own, only how to reach the daemon.

### 3.1 On the daemon

```toml
[runtimes.labpc]            # a dial-in runtime: no `command`
[runtimes.labpc.mcp.servers.playwright]
command = ["npx", "@playwright/mcp"]
exclude_tools = ["browser_install"]
[runtimes.labpc.acp.agents.claude]
command = ["claude-code-acp"]
```

- A `[runtimes.<id>]` table without `command` is valid for any id. It holds
  the settings of the dial-in runtime of that id, as `[runtimes.local]`
  already does for the built-in one. A dial-in id that matches a table with a
  `command` is still refused, as today.
- The daemon sends a dial-in runtime its settings in `runtime/initialize`,
  as it does for templates. No table means no MCP servers and no ACP
  agents.
- **Live settings.** A new runtime-protocol notification,
  `runtime/settings_changed { settings }`, carries the full `RuntimeSettings`
  whenever they change. The runtime diffs by name: it starts added servers
  and agents, stops removed ones, and restarts changed ones; unchanged ones
  keep running. A runtime that is not connected gets them at its next
  `runtime/initialize`, and so does a runtime older than this protocol
  minor, after which the panel says a restart applies them.
  `restart_required` then no longer lists `runtimes` for MCP or ACP edits; it
  still does for `command`, `env`, and the timeouts.
- **Clients edit through `config.apply`**, with `Set` on
  `["runtimes", <id>, "mcp", "servers", <name>]` and so on. The typed RPCs in
  section 4 wrap it for the panel. No `runtime_id` is added to `config.get`
  or `config.apply`: there is one configuration, the daemon's.
- **Secrets.** `env` values in these tables may be `${NAME}`, expanded on
  the runtime from its own environment. A token can then stay on the machine
  that uses it, as it did when the machine declared the server. A plain
  value is sent as written.

### 3.2 On a dial-in machine

- The machine's configuration holds only the connection, `[server] url`
  and `[server] token`, which `cookie runtime serve --daemon` and the TUI's
  embedded runtime already read. `--daemon <url>`, `--token`, and `--id`
  still override them.
- `cookie runtime serve --daemon` and the TUI's embedded runtime stop
  reading `[runtimes.local]`. `[runtimes.local]` stays what it is in a
  daemon's own configuration: its built-in runtime's settings.
- A dial-in runtime started on a machine whose configuration has
  `[runtimes.local.mcp]` or `[runtimes.local.acp]` entries warns once at
  startup that a dial-in runtime ignores them, and names the daemon table
  to move them to (`[runtimes.<id>]`). It does not migrate them.
- Removed: `watch_local_acp_agents`, the 5-second poll, and
  `McpConfigSource::Machine`. Every runtime's servers come from the
  daemon's configuration, with full definitions.

## 4. Machines panel

Each runtime row in the TUI `/runtime` → Machines tab and the web Machines
page gets a disclosure with two sections. The TUI uses a detail pane with
tabs, and every new target gets a `PointerTarget` and a hover.

### 4.1 MCP servers and tools

- **List.** Each server shows its state, where it is declared
  (`[runtimes.<id>]`, or `profile <name>`, section 5), scope, and message. Expanded,
  it lists every tool: offered tools first, then filtered-out ones dimmed.
  - Client protocol: `McpServerInfo.tools: Vec<McpToolInfo { name,
    filtered }>`, using the server's own names. It is omitted when empty.
  - Runtime protocol: `McpServerReport.filtered_tool_names`, beside the
    existing `tools`.
- **Leave out or take back a tool.** A per-tool toggle edits the server's
  filters:
  - Leaving a tool out adds its exact name to `exclude_tools`.
  - Taking it back removes the name from `exclude_tools`. When an
    `include_tools` list or a pattern is what filters it, the toggle adds the
    exact name to `include_tools` instead and says so.
  - Patterns are never rewritten.
- **Add, edit, remove, rename, and enable.** The existing
  `mcp.server.add/edit/remove/set_enabled` gain an optional `runtime_id`,
  defaulting to `global`. A new `mcp.server.rename { runtime_id?, name,
  new_name }` moves the table and its OAuth credential key in one
  configuration edit. For a runtime other than `global`, they edit
  `[runtimes.<id>.mcp.servers]` and `runtime/settings_changed` applies the
  edit live.

### 4.2 ACP agents

An ACP agent's ID is its declaration's key on every runtime, as today: the
`claude` that two runtimes declare is one agent, with one document,
`agents/claude.md`, and a session runs it on the attached runtime that
offers it. Templates over a declaration (`acp.agent`) resolve the same way.
The panel follows that:

- **List.** Under each runtime, the agents it declares: key, where the
  declaration comes from (`[runtimes.<id>]` or `profile <name>`), whether it
  is available (its command was found), enabled, its description, and the
  other runtimes that declare the same key.
  - A new `runtimes.agents.list { runtime_id }` returns
    `[AcpAgentEntry { agent, source, declaration, available, enabled,
    description, documents }]`.
  - `documents` are the agent documents that run the declaration: the one
    of its own ID, and every template whose `acp.agent` names it.
- **Add, edit, remove, rename, and enable.** `runtimes.agents.add/edit/
  remove/rename` edit `[runtimes.<id>.acp.agents.<agent>]` (`command`,
  `env`, `description`, `enabled`) and apply it live.
  - **Rename (D6)** renames the declaration on that runtime. When no other
    runtime or profile still declares the old key, the same `config.apply`
    also moves `agents/<old>.md` to the new key and rewrites `acp.agent` in
    the templates that name it. When another one still does, the documents
    stay with the old key, which those runtimes go on offering, and the
    confirmation says so. Existing sessions keep the agent ID they
    recorded. Where it is gone, they resume with the default agent, as for
    a removed agent today, and the confirmation says so.
- **Documents.** **Open** opens one of `documents` in the client's existing
  document editor: TUI `/config`, web Settings › Configuration. **Create
  document** writes `agents/<agent>.md` from the built-in template for an ACP
  agent (an `acp:` block, `description`, and permissions), and **New
  template** writes `agents/<name>.md` with `acp.agent: <agent>`. Both are
  shared by every runtime that declares the key.

### 4.3 Restart

See [section 6](#6-restarting-runtimes). The row shows **Restart** only when
the runtime reports that it can.

## 5. Every-runtime MCP servers and ACP agents (question 3.3)

**Answer: yes.** It fits the design of section 3. The daemon already
computes each runtime's settings before `runtime/initialize`. A profile is
one more layer in that computation:

```toml
[runtime_profiles.coding]
match = { capabilities = ["coding"] }   # or runtimes = ["labpc", "unicorn*"]
[runtime_profiles.coding.mcp.servers.context7]
url = "https://mcp.context7.com/mcp"
[runtime_profiles.coding.acp.agents.claude]
command = ["claude-code-acp"]
[runtime_profiles.coding.acp.agents.codex]
command = ["codex-acp"]
```

- **Layers**, last wins by name: the profiles that match, in name order,
  then `[runtimes.<id>]`. A runtime
  turns one entry off with `enabled = false` in `[runtimes.<id>]`, or leaves
  a profile with `[runtimes.<id>] profiles = []` (D4).
- **Matching** is on free-form labels the user writes: capability names the
  runtime reports, and runtime id globs. The daemon compares strings and
  never branches on a particular kind, so this keeps the rule that kinds
  are labels. `global` never matches.
- **When it applies:** at `runtime/initialize`, and through
  `runtime/settings_changed` when a profile changes. A runtime that connects
  later gets the profile at once, which is the "automatically configured"
  part.
- **Machines that lack the command.** Each runtime spawns its own copy, so a
  machine without `npx` or `claude-code-acp` reports that server or agent as
  failed or unavailable, in the panel. Nothing else breaks.
- **Secrets.** `${NAME}` in `env` expands on each runtime (section 3.1), so
  no secret travels from the daemon. HTTP servers that need OAuth share one
  sign-in (section 7, D3).
- **Agent documents (D5).** A profile's ACP agent is one agent with one
  document, `agents/<agent>.md`, whichever runtimes the profile adds it to,
  and so are the templates whose `acp.agent` names it. Which runtime a
  session runs it on is decided by the runtime capability the session
  attaches, as for any ACP agent.
- **`local` too.** A profile matches the daemon's built-in `local` runtime
  like any other.
- **Where it does not fit.** An HTTP MCP server that does not need the
  machine is better in `global`: one connection instead of one per runtime.
  The editor says so when a profile adds a URL server.
- UI: a "Profiles" section above the machines in the panel, with the same
  MCP and ACP editors as section 4. Each runtime's entries name the profile
  they come from.

## 6. Restarting runtimes

`runtimes.restart { runtime_id, when: "idle" | "now" }` →
`{ scheduled, active_runs, background_calls }`. The semantics mirror
`daemon.restart`, scoped to one runtime:

- **What `idle` waits for:**
  - runs whose session has an attachment on the runtime or runs one of its
    ACP agents;
  - background calls executing on the runtime.

  Meanwhile, new tool calls on that runtime wait, and calls elsewhere carry
  on. `now` ends those calls and the runtime's ACP agent processes. ACP
  sessions are reopened by the next turn, as after a runtime drop.
- **Template runtimes:** the daemon sends `runtime/shutdown` and waits the
  grace period, then starts the template again under a new generation. It
  re-reads `command` and `env`, so `restart_required: runtimes` clears for
  that runtime.
- **Dial-in runtimes:** a new runtime-protocol request, `runtime/restart`.
  `cookie runtime serve` answers, closes its connection, and relaunches its
  own executable with the same arguments: an in-place `exec` on Unix,
  keeping the PID for systemd and launchd, and a new process on Windows, as
  the daemon does. The new binary on that machine runs, so this is also how
  a machine picks up an upgrade. The daemon then waits for it to dial in
  again.
  - **Which runtimes offer it (D7).** `InitializeResult.can_restart`:
    - `true` for `cookie runtime serve`, whether it runs as a service or in a
      terminal: both can exec;
    - `false` for a runtime a client embeds (the TUI's or the web's), and for
      plugins' runtimes.

    `RuntimeListing.can_restart` exposes it, and the panel shows **Restart**
    only then.
- `global` and `local` restart with the daemon, so they show no Restart.

## 7. MCP OAuth on any runtime

The daemon runs the whole OAuth flow and keeps the credentials. The runtime
only ever holds an access token, in memory.

- **Flow.**
  1. `mcp.auth.begin/complete/cancel` gain `runtime_id?` (default `global`)
     and `workspace?` for workspace-scoped servers.
  2. The daemon runs discovery, PKCE, and the code exchange with its existing
     `oauth` code, from its own network.
  3. Redirects are unchanged: the TUI's loopback listener or paste on the
     TUI's machine, and the web page as redirect. So both clients work for a
     server on any machine.
- **Token delivery.** Two new runtime-protocol messages:
  - runtime → daemon request `mcp/token { server, resource_url,
    force_refresh }` → `{ access_token, expires_at }` or `needs_auth`. The
    runtime asks before it connects, near expiry, and after a 401.
  - daemon → runtime notification `mcp/auth_changed { server }` after a
    sign-in, a sign-out, or a refresh another process made, so the runtime
    reconnects.

  The refresh token never leaves the daemon. Refresh uses the store's
  existing cross-process claim.
- **Runtime side.** A server with `oauth` is accepted. Its transport gets an
  auth provider that calls `mcp/token`. A `needs_auth` answer reports
  `NeedsAuth` (no longer `Failed`), and the panel's **Sign in** button
  appears for it.
- **Credential key (D3).** It stays `server:sha256(resource URL)`, plus the
  client binding the store already checks. One sign-in serves every runtime
  that has the same server name and URL, which is what profiles want.
- **Network reach.** Discovery and the token exchange run from the daemon. A
  server whose authorization server only the runtime's network reaches is
  not supported, and the begin error says so. Relaying the daemon's OAuth
  HTTP through the runtime is possible later.
- **Sign out:** a new `mcp.auth.forget { runtime_id?, name }`, missing
  today, deletes the credential and sends `mcp/auth_changed`.

## 8. Protocol and phases

Client protocol minor and runtime protocol minor, all additive on the
wire. Dropping a dial-in machine's `[runtimes.local]` is a behaviour change
for its configuration, not for the protocol: an older dial-in runtime still
reads its own file, and the daemon still accepts its servers as reported.

| Phase | Content |
|---|---|
| 1 | Tool names and filtered names on the wire (`McpServerInfo.tools`, `McpServerReport.filtered_tool_names`); `runtimes.agents.list`; read-only expanded rows in both clients |
| 2 | Dial-in `[runtimes.<id>]` settings sent at initialize, dial-in machines ignoring `[runtimes.local]` with a warning, `${NAME}` in `env`, `runtime/settings_changed`; `runtime_id` on `mcp.server.*`, `mcp.server.rename`, `runtimes.agents.*`; the editors, tool toggles, and document links in both clients; the guides moved to the daemon tables |
| 3 | `runtimes.restart`, `runtime/restart` with self-exec, `can_restart` |
| 4 | Runtime MCP OAuth: `mcp/token`, `mcp/auth_changed`, `runtime_id` on `mcp.auth.*`, `mcp.auth.forget`, Sign in on any machine in both clients |
| 5 | `[runtime_profiles]`, if approved |

Each phase is its own commit series, with the configuration reference, the
protocol references and changelogs, and the TUI and web pages updated.
Tests cover:

- strict parsing of the new tables;
- profile merge order and overrides; a dial-in runtime ignoring
  `[runtimes.local]` and warning;
- the live settings diff, so that unchanged servers are not restarted;
- tool toggle edits against `include_tools`, `exclude_tools`, and patterns;
- rename moving documents and credentials;
- restart waits, refusals, and the exec relaunch in an integration test;
- the token relay, including a refresh after a 401;
- the panel targets' hover sweep.

## 9. Decisions

All made by the user on 2026-10-26.

- **D1** Per-runtime settings live in the daemon's `config.toml`.
- **D2** A dial-in machine drops its runtime settings and keeps only the
  connection (`[server] url` and `token`); see section 3.2.
- **D3** OAuth credentials are shared by server name and URL across
  runtimes.
- **D4** Build profiles (section 5), with opt-out by `enabled = false` or
  `profiles = []`.
- **D5** A profile's ACP agent has one shared document, like every ACP agent:
  its ID is the declaration's key on every runtime, and templates over it
  (`acp.agent`) resolve to whichever runtime the session attaches.
- **D6** Renaming an ACP agent moves its documents when nothing else
  declares the old key (section 4.2); old sessions fall back to the default
  agent.
- **D7** Restart is offered for every `cookie runtime serve`, not only ones
  run as a service, since both can exec.

Not yet confirmed, with the defaults this spec assumes: `${NAME}` in `env`
expands on the runtime (section 3.1); profiles match `local` (section 5);
labpc's own `[runtimes.local]` entries move into the daemon's
`[runtimes.labpc]` only when the user asks.

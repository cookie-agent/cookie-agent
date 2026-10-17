# Persistent Daemon, Dial-in Runtimes, and Runtime Capabilities

Status: **draft for review** (decisions of 2026-10-16 below; section
[Review](#10-review) lists what still needs a decision). This spec extends
the [runtimes spec](runtimes.md); where they differ, this one governs once
it is approved.

## 1. Goal and scope

Today the daemon is tied to the directory it starts in: that directory keys
its session store and is its `local` runtime's workspace. A runtime is a
process the daemon spawns for one workspace, and it offers one set of tools.
This spec changes three things:

1. **Persistent processes.** The daemon and a runtime can each run as a
   long-lived process holding many active sessions: a daemon serves sessions
   of any directory, and one runtime process serves every workspace its
   sessions use.
2. **Runtime lifetimes and capabilities.** A runtime is either short-lived
   (the daemon starts it, or a client offers it) or persistent (it runs on
   its own and dials in), with one runtime protocol for all of them. A
   runtime offers several **capabilities**, such as `coding` and
   `computer_use`, and sessions attach and detach each one separately.
3. **Client runtimes.** A client can offer a runtime over its connection;
   the runtime ends when the client's connection does.

**Decided 2026-10-16:**

- The persistent daemon is explicit: `cookie daemon` plus
  `cookie attach --url --token`. There is no discovery file or autostart, and
  plain `cookie` keeps running the daemon in-process.
- A persistent runtime dials the daemon and registers itself; the daemon
  does not dial runtimes.
- Any session may attach a client's runtime. When the client drops, the
  runtime's tools vanish but attachments stay, and they come back when a
  client offers that runtime again.
- A TUI offers its own machine as a runtime only when the daemon is on
  another machine. A daemon on the same machine serves the TUI's directory
  with its `local` runtime, so sessions keep working after the TUI closes.

**Out of scope:** a computer-use implementation (this spec adds the
mechanism and the `coding` and `global` capabilities only), daemon
discovery, a non-loopback or TLS listener, and resuming calls that were in
flight when a connection dropped.

## 2. Vocabulary

| Term | Meaning |
|---|---|
| **Runtime** | One connection that speaks the runtime protocol and offers capabilities, identified by its runtime ID. |
| **Lifetime** | How a runtime is started and ends: **spawned** (the daemon runs a template's `command` and stops it with the daemon), **dial-in** (started on its own, connects to the daemon, reconnects after drops, outlives daemon restarts), or **client** (offered by a client, ends with that client's connection). All three speak the same protocol once connected. |
| **Capability** | A named set of tools and context a runtime offers, such as `coding` or `computer_use`. It declares a free-form `kind` label and whether it takes a workspace. |
| **Capability instance** | A capability opened for one workspace (or none): (runtime ID, capability, canonical workspace). It replaces the runtimes spec's *runtime instance*. Sessions attached to it share it. |
| **Attachment** | A durable session fact binding the session to a capability instance under a session-unique attachment name. |

The `global` runtime offers one capability, `global`. The `local` runtime
offers `coding`. A runtime still never declares what kind of runtime it is
for the daemon to branch on; the daemon reads what capabilities report.

## 3. Daemon

- **Sessions of any directory.** `session.create` takes the client's `cwd`.
  It is stamped as the session's `cwd_identity`, so `session.list` with that
  identity shows the sessions of the client's directory, and it is the
  default workspace of the session's `coding` attachment. Without `cwd` the
  daemon's own directory is used, as today. The store stays where it is
  (`sessions/<daemon workdirkey>/`); see [Review](#10-review) item 1.
- **`local` serves any workspace** on the daemon's machine: it opens a
  `coding` instance per workspace a session asks for, in the daemon's
  process.
- **Persistence.** `cookie daemon` keeps sessions, runs, background calls,
  and runtimes going while no client is attached, as it does today. Plain
  `cookie` is unchanged.
- **Endpoints.** The WebSocket listener gains a `/runtime` endpoint beside
  `/ws`, for dial-in and client runtimes (sections 5 and 6). The daemon
  still binds loopback only.

## 4. Runtime protocol 5: capabilities

`runtime/initialize` describes the runtime; capability instances are opened
separately, so one connection serves every workspace and capability its
sessions use.

**`runtime/initialize`.** Params keep `version`, `runtime_id`, and
`settings` (tool output, `AGENTS.md`, and the template's MCP servers) and
lose `workspace`. The result keeps `version` and `platform` and replaces
`kind`, `attachment_name`, `workspace`, `tools`, and `skills` with:

| Result field | Meaning |
|---|---|
| `capabilities` | `[{ name, kind, takes_workspace, description }]`: what the runtime offers. `name` matches `[a-z][a-z0-9_]{0,31}`. |

**New requests:**

| Method | Direction | Purpose |
|---|---|---|
| `capability/open` | daemon → runtime | `{ capability, workspace? }` → `{ instance, attachment_name, workspace?, tools, skills }`. Opening an instance that is already open returns it. A workspace is required exactly when the capability takes one. |
| `capability/close` | daemon → runtime | `{ instance }` → `{}`, once no session uses it. |
| `capability/tools_changed` | runtime → daemon | `{ instance, tools }`; replaces `runtime/tools_changed`. |

`instance` is a runtime-chosen string, unique on its connection.
`tool/prepare`, `runtime/context`, `mcp/changed`, and `mcp/reconnect` gain
`instance`. Prepared IDs stay unique per connection, so `tool/execute`,
`revalidate`, `release`, `cancel`, `progress`, and `detachable` are
unchanged.

**MCP servers.** A template's MCP servers run in each instance of a
capability that hosts MCP (the coding capability does), in that instance's
workspace, as today.

**Producers.** A runtime produces as source `runtime:<runtime ID>` and may
address any session attached to any of its instances (today:
`runtime:<attachment name>`, attached sessions of one instance).

**Spawned runtimes.** One process per template serves all its instances.
The `{workspace}` command placeholder goes. `cookie runtime coding --stdio`
becomes `cookie runtime serve --stdio`, offering every built-in capability;
`--root <dir>` (repeatable) limits the workspaces `coding` opens.

## 5. Dial-in runtimes

```toml
[runtimes.labpc]
dial_in = true                       # no command: the runtime connects
[runtimes.labpc.mcp.servers.rust_analyzer]
command = "rust-analyzer-mcp"
```

- On the runtime's machine, `cookie runtime serve --daemon <url>
  --credential-file <path>` runs persistently. It connects to the daemon's
  `/runtime` endpoint, presents `{ runtime_id, credential }`, and then the
  daemon drives the protocol exactly as for a spawned runtime:
  `runtime/initialize`, then `capability/open` for every attachment its
  sessions hold.
- **Credentials.** `cookie runtime pair <id>` on the daemon's machine mints
  a secret for a `dial_in` template, stores its hash, and prints it once. It
  survives daemon restarts, unlike the per-run client token.
- **Reconnect.** The runtime reconnects with backoff, forever. While it is
  away, its instances are `reconnecting` and their tools are absent; the
  attachments stay. It keeps its own state across reconnects (background
  work it tracks, producer recovery).
- **Reaching the daemon.** The daemon binds loopback, so a runtime on
  another machine reaches it through an ssh reverse tunnel
  (`ssh -R 7777:127.0.0.1:7777 labpc`); the guide documents a persistent
  tunnel. See [Review](#10-review) item 2.

## 6. Client runtimes

- A client that holds the daemon token opens a `/runtime` connection with
  `{ runtime_id, client: <its client instance ID> }`. The daemon ties it to
  that client connection and closes it when the client disconnects.
- The client chooses the runtime ID; it must match no template and no live
  runtime. Any session may attach it; while it is gone, its instances are
  unavailable and attachments stay, and a later offer of the same ID, from
  any client, restores them.
- **TUI.** The daemon's handshake result gains `machine_id`, a hash that
  identifies its machine. A TUI attached to a daemon with another
  `machine_id` offers `client_<hostname>` with the `coding` capability, and
  its new sessions attach that runtime at the TUI's directory instead of
  `local`. A TUI on the daemon's machine offers nothing.

## 7. Sessions and attachments

- Attach targets name a capability: `{ runtime_id, capability, workspace? }`.
  `RuntimeAttached` gains `capability` (additive: an older event reads as
  `global` for the `global` runtime and `coding` otherwise).
- The attachment name comes from `capability/open`: `global`;
  `<runtime ID>_<workspace basename>` for `coding`; a capability without a
  workspace names itself, such as `labpc_computer_use`.
- Default attachments of `session.create`: `global`, and `coding` at the
  session's `cwd` on `local` (or on the TUI's client runtime, section 6).
- An attachment whose runtime is not connected contributes no tools, and
  sessions see a notice, as for a failed instance today. Attach to a
  runtime that is not connected fails.

## 8. Client protocol 45

- `session.create` takes `cwd`; its `runtimes` entries and
  `session.runtime.attach` take `capability`.
- `runtimes.list` returns `runtimes` (`runtime_id`, `lifetime`, connection
  status, `capabilities`) and `instances` (`runtime_id`, `capability`,
  `workspace`, `attachment_name`, status). `runtimes.reconnect` reconnects a
  spawned runtime; a dial-in or client runtime reconnects itself.
- The handshake result gains `machine_id`.
- The TUI's `/runtime` panel lists runtimes with their capabilities and
  attaches a capability, with a workspace when it takes one.

## 9. Phases

Each phase lands as its own commits with every gate green, and is pushed.

1. **Capabilities.** Runtime protocol 5, one process per spawned runtime,
   capability attachments, `global` and `coding` as capabilities, client
   protocol 45, the `/runtime` panel.
2. **Directory-independent daemon.** `session.create` `cwd`, `local` serving
   any workspace, `cookie attach` sending its directory.
3. **Dial-in runtimes.** The `/runtime` endpoint, `dial_in` templates,
   `cookie runtime pair`, `cookie runtime serve --daemon`, reconnect.
4. **Client runtimes.** Client-owned runtime connections, `machine_id`, the
   TUI's remote offer.
5. **Documentation.** Runtime guide and protocol references; the runtimes
   spec points here.

## 10. Review

1. **Store layout.** Keep `sessions/<daemon workdirkey>/` and tell
   directories apart by each session's `cwd_identity` (the client's `cwd`).
   A daemon started in `~` then holds every directory's sessions under one
   key, which is what `session.list` filters.
2. **Remote reach.** Keep the daemon loopback-only and reach it from other
   machines through an ssh reverse tunnel; a TLS listener is later work.
3. **Credentials.** Per-runtime paired secrets (`cookie runtime pair`),
   stored hashed, for dial-in runtimes; clients keep the per-run token.
4. **Spawned process model.** One process per template serving every
   workspace; drop `{workspace}`; `--root` limits workspaces.
5. **Producer source.** `runtime:<runtime ID>`, addressing any session
   attached to any of its instances.
6. **Client runtime IDs.** Chosen by the client (TUI: `client_<hostname>`),
   unique while live; any client with the token may offer an ID again.
7. **Remote detection.** `machine_id` in the handshake decides whether a
   TUI offers its machine.
8. **Computer use.** Mechanism only; no `computer_use` capability is built
   here.
9. **In-flight calls.** A call in flight when its runtime's connection
   drops fails with `runtime_unavailable`, for every lifetime; a persistent
   runtime may report finished work later through its producers.

# Persistent Daemon, Dial-in Runtimes, and Runtime Capabilities

Status: **approved; phase 1 implemented** (decisions of 2026-10-16 below and
in [Review](#10-review); see [Implementation notes](#11-implementation-notes)). This spec extends the [runtimes spec](runtimes.md);
where they differ, this one governs.

## 1. Goal and scope

Today the daemon is tied to the directory it starts in: that directory keys
its session store and is its `local` runtime's workspace. A runtime is a
process the daemon spawns for one workspace, and it offers one set of tools.
This spec changes three things:

1. **Persistent processes.** The daemon and a runtime can each run as a
   long-lived process holding many active sessions: a daemon serves sessions
   of any directory, and one runtime process serves every workspace its
   sessions use, concurrently.
2. **Runtime lifetimes and capabilities.** A runtime is either short-lived
   or persistent, with one runtime protocol for both. A runtime offers
   several **capabilities**, such as `coding` and `computer_use`, and
   sessions attach and detach each one separately.
3. **Client runtimes.** A client can offer a runtime that ends when the
   client does.

**Decided 2026-10-16:**

- The persistent daemon is explicit: `cookie daemon` plus
  `cookie attach --url --token`. There is no discovery file or autostart, and
  plain `cookie` keeps running the daemon in-process.
- The daemon can listen beyond loopback, so runtimes and clients on other
  machines connect to it directly.
- A persistent runtime dials the daemon and registers itself; the daemon
  does not dial runtimes.
- Every runtime, spawned or dial-in, serves all of its sessions, workspaces,
  and capabilities concurrently over one connection; the daemon never opens
  a second connection to the same runtime.
- A client runtime is not a protocol feature: a client that offers a
  runtime embeds a dial-in runtime in its own process, so the runtime ends
  when the client process does. The client protocol knows nothing of it.
- Any session may attach a client's runtime. While it is gone its tools
  vanish but attachments stay, and they come back when that runtime dials
  in again.
- A TUI offers its own machine as a runtime only when the daemon is on
  another machine. A daemon on the same machine serves the TUI's directory
  with its `local` runtime, so sessions keep working after the TUI closes.

**Out of scope:** a computer-use implementation (this spec adds the
mechanism and the `coding` and `global` capabilities only), daemon
discovery, and resuming calls that were in flight when a connection
dropped.

## 2. Vocabulary

| Term | Meaning |
|---|---|
| **Runtime** | One connection that speaks the runtime protocol and offers capabilities, identified by its runtime ID. |
| **Lifetime** | How the daemon gets the connection: **spawned** (the daemon runs a template's `command` over stdio and stops it with the daemon) or **dial-in** (the runtime connects to the daemon's `/runtime` endpoint). A dial-in runtime is persistent when it runs as its own service (`cookie runtime serve --daemon`), and short-lived when a client embeds it. Both speak the same protocol once connected. |
| **Capability** | A named set of tools and context a runtime offers, such as `coding` or `computer_use`. It declares a free-form `kind` label and whether it takes a workspace. |
| **Capability instance** | A capability opened for one workspace (or none): (runtime ID, capability, canonical workspace). It replaces the runtimes spec's *runtime instance*. Sessions attached to it share it. |
| **Attachment** | A durable session fact binding the session to a capability instance under a session-unique attachment name. |

The `global` runtime offers one capability, `global`. The `local` runtime
offers `coding`. The daemon never branches on a runtime's or capability's
kind; it reads what capabilities report.

## 3. Daemon

- **Sessions of any directory.** `session.create` takes the client's `cwd`.
  It is stamped as the session's `cwd_identity`, so `session.list` with that
  identity shows the sessions of the client's directory, and it is the
  default workspace of the session's `coding` attachment. Without `cwd` the
  daemon's own directory is used, as today. The store stays where it is
  (`sessions/<daemon workdirkey>/`).
- **`local` serves any workspace** on the daemon's machine: it opens a
  `coding` instance per workspace a session asks for, in the daemon's
  process.
- **Persistence.** `cookie daemon` keeps sessions, runs, background calls,
  and runtimes going while no client is attached, as it does today. Plain
  `cookie` is unchanged.
- **Listening.** `cookie daemon --listen <addr:port>` binds any address;
  loopback stays the default. `--tls-cert` and `--tls-key` serve `wss`.
  TLS is supported, not required: without it the token crosses the network
  in the clear, which the guide says plainly, so a plaintext listener off
  loopback belongs on a private network such as a VPN.
- **One durable token.** `--token-file <path>` reads the bearer token from
  a private file, creating one if it is missing, so it survives daemon
  restarts. Without it the daemon mints a per-run token as today. Clients
  and dial-in runtimes present the same token.
- **Endpoints.** `/ws` for clients and `/runtime` for dial-in runtimes.

## 4. Runtime protocol 5: capabilities

`runtime/initialize` describes the runtime; capability instances are opened
separately, so one connection serves every workspace and capability its
sessions use. Today one connection holds one workspace's tools, MCP
servers, and skills, fixed at initialize, and the daemon keys connections by
(runtime ID, workspace); that is all that ties a runtime process to one
workspace. Calls already run concurrently on one connection.

**`runtime/initialize`.** Params keep `version`, `runtime_id`, and
`settings` (tool output, `AGENTS.md`, and, for a spawned runtime, the
template's MCP servers) and lose `workspace`. The result keeps `version` and
`platform` and replaces `kind`, `attachment_name`, `workspace`, `tools`, and
`skills` with:

| Result field | Meaning |
|---|---|
| `capabilities` | `[{ name, kind, takes_workspace, description }]`: what the runtime offers. `name` matches `[a-z][a-z0-9_]{0,31}`. |

**New messages:**

| Method | Direction | Purpose |
|---|---|---|
| `capability/open` | daemon → runtime | `{ capability, workspace? }` → `{ instance, attachment_name, workspace?, tools, skills }`. Opening an instance that is already open returns it. A workspace is required exactly when the capability takes one. |
| `capability/tools_changed` | runtime → daemon | `{ instance, tools }`; replaces `runtime/tools_changed`. |

`instance` is a runtime-chosen string, unique on its connection.
`tool/prepare`, `runtime/context`, `mcp/changed`, and `mcp/reconnect` gain
`instance`. Prepared IDs stay unique per connection, so `tool/execute`,
`revalidate`, `release`, `cancel`, `progress`, and `detachable` are
unchanged.

**MCP servers.** A capability that hosts MCP (the coding capability does)
runs its runtime's MCP servers in each of its instances, in that instance's
workspace, as today. A spawned runtime gets them from its template; a
dial-in runtime reads its own machine's configuration (section 5).

**Producers.** A runtime produces as source `runtime:<runtime ID>` and may
address any session attached to any of its instances (today:
`runtime:<attachment name>`, attached sessions of one instance).

**Spawned runtimes.** One process per template serves all its instances.
The `{workspace}` command placeholder goes. `cookie runtime coding --stdio`
becomes `cookie runtime serve --stdio`, offering every built-in capability;
`--root <dir>` (repeatable) limits the workspaces `coding` opens.

## 5. Dial-in runtimes

- `cookie runtime serve --daemon <url> --token-file <path> --id <runtime ID>`
  connects to the daemon's `/runtime` endpoint and presents the token and
  its runtime ID. The daemon then drives the protocol exactly as for a
  spawned runtime: `runtime/initialize`, then `capability/open` for every
  attachment its sessions hold.
- **No template.** A dial-in runtime chooses its own ID, which must match
  no `[runtimes.<id>]` template and no connected runtime. It runs on its own
  machine, so it takes its MCP servers from that machine's
  `[runtime.mcp.servers]` configuration rather than from the daemon's.
- **Reconnect.** A dial-in runtime reconnects with backoff for as long as
  its process runs. While it is away its instances are `reconnecting` and
  their tools are absent; attachments stay, and a runtime that dials in
  again under the same ID gets them back. It keeps its own state across
  reconnects (background work it tracks, producer recovery).

## 6. Client runtimes

A client offers a runtime by running a dial-in runtime in its own process,
with the client's token; it is section 5 with nothing added. When the
client exits, the connection drops and the runtime is gone.

**TUI.** The daemon's handshake result gains `machine_id`, a hash that
identifies its machine. A TUI attached to a daemon with another
`machine_id` dials in a runtime `client_<hostname>` offering `coding`, and
its new sessions attach it at the TUI's directory instead of `local`. A TUI
on the daemon's machine offers nothing.

## 7. Sessions and attachments

- Attach targets name a capability: `{ runtime_id, capability, workspace? }`.
  `RuntimeAttached` gains `capability` (additive: an older event reads as
  `global` for the `global` runtime and `coding` otherwise).
- The attachment name comes from `capability/open`: `global`;
  `<runtime ID>_<workspace basename>` for `coding`; a capability without a
  workspace names itself, such as `labpc_computer_use`.
- Default attachments of `session.create`: `global`, and `coding` at the
  session's `cwd` on `local` (or on the TUI's runtime, section 6).
- An attachment whose runtime is not connected contributes no tools, and
  sessions see a notice, as for a failed instance today. Attaching a
  runtime that is not connected fails.

## 8. Client protocol 45

- `session.create` takes `cwd`; its `runtimes` entries and
  `session.runtime.attach` take `capability`.
- `runtimes.list` returns `runtimes` (`runtime_id`, `lifetime` (`spawned`
  or `dial_in`), connection status, `capabilities`) and `instances`
  (`runtime_id`, `capability`, `workspace`, `attachment_name`, status).
  `runtimes.reconnect` restarts a spawned runtime; a dial-in runtime
  reconnects itself.
- The handshake result gains `machine_id`.
- The TUI's `/runtime` panel lists runtimes with their capabilities and
  attaches a capability, with a workspace when it takes one.

## 9. Phases

Each phase lands as its own commits with every gate green, and is pushed.

1. **Capabilities.** Runtime protocol 5, one process per spawned runtime
   serving every workspace, capability attachments, `global` and `coding` as
   capabilities, client protocol 45, the `/runtime` panel.
2. **Directory-independent daemon.** `session.create` `cwd`, `local` serving
   any workspace, `cookie attach` sending its directory.
3. **Listening and dial-in.** `--listen`, optional TLS, `--token-file`, the
   `/runtime` endpoint, `cookie runtime serve --daemon`, reconnect.
4. **TUI runtime.** `machine_id` and the TUI's embedded runtime when the
   daemon is remote.
5. **Documentation.** Runtime guide and protocol references; the runtimes
   spec points here.

## 10. Review

Confirmed in review 2026-10-16:

- The store stays keyed by the daemon's directory; sessions are told apart
  by `cwd_identity`, the client's `cwd`.
- Producers produce as `runtime:<runtime ID>`.
- `machine_id` decides whether a TUI offers its machine.
- Computer use is mechanism only.
- A call in flight when its runtime's connection drops fails with
  `runtime_unavailable`.
- The daemon listens on any address; TLS is supported, not required.
- Clients and dial-in runtimes share one durable token.
- Dial-in runtimes choose their own IDs and bring their own MCP
  configuration.
- Spawned runtimes stay, as the short-lived way to start a runtime (for
  example over ssh), but like every runtime one connection serves all their
  sessions: one process per template, no `{workspace}` placeholder, and
  `--root` limits the workspaces `coding` opens.

## 11. Implementation notes

**Phase 1** (2026-10-16): runtime protocol 5 and client protocol 45.

- `capability/close` is not part of the protocol: an instance stays open
  for its connection's life, as runtime instances did.
- `capability` is optional where a session attaches or retries: without it
  the runtime's only capability is used, and a runtime that offers several
  asks for one. This keeps attaching a runtime that has not connected yet a
  single step.
- `runtimes.list` also reports a runtime's last connection failure
  (`message`), since an attach that names no capability has no instance to
  mark failed.
- Plugins keep `runtime/tools_changed` and send no `instance`: a plugin
  connection has one implicit instance.
- `cookie runtime coding --workspace` became
  `cookie runtime serve --stdio [--root <dir>]...`.


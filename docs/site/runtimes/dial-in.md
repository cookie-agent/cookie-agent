# Runtimes that dial in

A runtime can also run on its own and connect to the daemon, instead of the
daemon starting it from a [template](templates.md). It needs no template: it names itself, and sessions
attach it like any other runtime while it is connected.

```sh
# On the daemon's machine: a listener the runtime reaches.
cookie daemon --listen 0.0.0.0:7419
```

```sh
# On labpc: dial in, named after the host, and stay connected.
cookie runtime serve --daemon ws://laptop:7419/ws --token <token>
```

- **Daemon.** `--daemon` takes the daemon's client URL, or, without one,
  `[server] url` of this machine's `config.toml`, else the local daemon that
  table describes. `--token` is the daemon's token, `[server] token` in its
  `config.toml`, where the daemon keeps it so it survives restarts.
  `COOKIE_DAEMON_TOKEN` and `[server] token` on labpc work too; flags
  override the configuration. The runtime does not pass the token on to the
  processes it starts.
- **Identity.** `--id` is the runtime ID, `[a-z][a-z0-9_]{0,31}`; without
  it, the host name, sanitized (`host_` in front when it does not start with
  a letter). It must not be `global`, `local`, or a `[runtimes.<id>]`
  template with a `command`, and only one runtime may be connected under an
  ID at a time.
- **Refusals.** The daemon says why it refuses a runtime, and the runtime
  prints it. A refusal that retrying cannot fix, a configured or reserved
  ID or a wrong token, ends `cookie runtime serve --daemon` with an error:

  ```text
  cookie: cannot dial in to ws://laptop:7419/ws as `labpc`: the daemon refused the runtime (HTTP 403): runtime `labpc` is configured on the daemon ([runtimes.labpc]) and cannot dial in
  ```

  Any other, such as an ID a runtime is still connected under (the daemon
  may not have noticed yet that an earlier connection dropped), is printed
  and retried with the same backoff as a drop:

  ```text
  cookie runtime: cannot dial in to ws://laptop:7419/ws as `labpc`: the daemon refused the runtime (HTTP 409): runtime `labpc` is already connected; retrying in 1 s
  ```

  A dropped connection prints the reason too, such as
  ``disconnected from ws://laptop:7419/ws: the daemon closed the connection with code 1011: runtime `labpc` failed to start: …``
  for a runtime of an incompatible version. See
  [Dial-in refusals](../reference/runtime-protocol.md#dial-in-refusals).
- **Reconnecting.** The runtime reconnects after every drop, waiting
  1 s, 2 s, 4 s, and so on up to 30 s. While it is away, its instances go
  `reconnecting` and then `failed`, and their tools leave the sessions' tool
  lists; when it dials in again, the daemon reopens them over the new
  connection. `runtimes.list` shows it with lifetime `dial_in`, and, while
  it is away, why it disconnected.
- **Connections that die without closing.** A machine that sleeps, or
  loses or changes its network, can leave the WebSocket half-open: neither
  end hears a close. Both ends watch for that. The daemon pings a runtime it
  has heard nothing from for 15 s and drops it once nothing has arrived for
  45 s (``runtime `labpc` disconnected: nothing arrived from it for 45 s``);
  calls in flight then fail with `runtime_unavailable`, and a request the
  runtime does not answer in time (30 s for preparing a call) fails the
  call and has the connection checked at once. The runtime sends WebSocket
  pings once its connection has been quiet for 15 s and dials in again once
  nothing, a pong included, has arrived for 45 s
  (``disconnected from ws://laptop:7419/ws: … nothing arrived from the daemon for 45 s``).
  A runtime that dials in while the daemon still counts its old connection
  as connected is refused (HTTP 409) and the daemon checks the old
  connection at once, so the next attempt gets in.
- **Offline means refused.** While the runtime is not connected, creating a
  session with it, attaching it, delegating to it with `capabilities`, and
  creating a session in a project whose defaults name it are refused at
  once, naming the runtime and when the daemon last heard from it:
  ``runtime `labpc` is not connected (last seen 2026-10-22T18:12:51Z)``.
  The daemon never waits for a dial-in runtime to come back, and never hands
  out an instance of a connection that does not answer: it is checked first
  (see **Reuse** under [Lifecycle and failures](index.md#lifecycle-and-failures)). A
  runtime the daemon remembers from before it restarted, but that has not
  dialed in since, is refused the same way. It stops
  on a stop signal (`SIGINT`, `SIGTERM`, `SIGHUP`), so it runs as a
  service; see [Running a runtime as a service](service.md).
- **Stopping.** A stopping runtime closes its WebSocket with a close
  handshake (normal closure). The daemon records that as
  ``runtime `labpc` disconnected: the other end closed the connection``,
  in `runtimes.list` and in its instances' reasons as they go `reconnecting`
  and then `failed`; a runtime that vanished without one, such as one that
  was killed or lost its network, shows the transport's error instead, for
  example ``Connection reset without closing handshake``.
- **MCP servers and ACP agents.** It hosts the ones the daemon's
  configuration gives it: a `[runtimes.<id>]` table with no `command`, named
  after the ID it dials in with, holds its
  [MCP servers](tools.md#mcp-servers) and [ACP agents](tools.md#acp-agents),
  a `scope = "runtime"` server from the moment it connects. Changes to the
  table reach the connected runtime at once: added servers and agents
  start, removed ones stop, changed ones restart, and the rest keep running.
  The machine itself keeps only how to reach the daemon, `[server] url` and
  `token`; its own `[runtimes.local]` is its own daemon's, and a runtime
  that dials in warns at startup that it does not host it:

  ```toml
  # ~/.cookie-agent/config.toml on the daemon's machine
  [runtimes.labpc]
  [runtimes.labpc.mcp.servers.my_chrome]
  command = "npx"
  args = ["-y", "chrome-devtools-mcp@latest"]
  scope = "runtime"
  [runtimes.labpc.acp.agents.claude]
  command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
  ```
- **Roots.** `--root` limits its workspaces, as for
  [`--stdio`](templates.md#cookie-runtime-serve).
- **Clients on its machine.** Plain `cookie` and `cookie attach` on the
  runtime's machine use it for their directory when the daemon runs
  elsewhere, such as in a container; see
  [A daemon already running](../daemon/persistent.md#a-daemon-already-running).

[Running a runtime as a service](service.md) keeps such a runtime connected;
[Remote access](../daemon/remote-access.md) covers the daemon's listener and
tunnels, and [Runtimes](index.md) what every runtime shares, such as its
capabilities and machine.

# The daemon

The daemon is the gateway at the center of cookie. It owns everything that
outlives one terminal: the provider connections and the model catalog, the
sessions and their history, the agents and presets, the permission rules and
approvals, and memory. It runs the models, and it reaches the tools a session
uses through [runtimes](runtimes.md). [Clients](clients.md) only show and
steer what the daemon holds.

## In process or on its own

The same daemon runs in two ways:

- **Inside a client's process.** Plain `cookie` with no daemon around starts
  one inside the TUI's process; the bottom bar then says `standalone`.
  `cookie --standalone` always does this, and `cookie run` always runs an
  engine of its own for its one prompt. When the process ends, so does the
  daemon.
- **On its own.** `cookie daemon` serves the client protocol on a WebSocket,
  by default `ws://127.0.0.1:7419/ws`, until it is stopped. Every client that
  reaches it shares its sessions, and it can run as a service. See
  [Persistent daemon](../daemon/persistent.md).

Either way it is one engine with one store: sessions, artifacts, memory, and
the machines registry live under `~/.cookie-agent`.

## Finding a running daemon

Plain `cookie` looks for a daemon on this machine before starting its own,
but only when `config.toml` has `[server] token`, which `cookie daemon` writes
at its first start. It dials `ws://<host>:<port>/ws` from the `[server]`
table, never `[server] url`:

- nothing listening within half a second: it starts its own engine, silently;
- a daemon that accepts the handshake within two seconds: the TUI opens on it;
- something that answers but cannot be used, such as a refused token or
  another protocol major: it starts its own engine and says why.

Reaching a daemon on another machine takes `cookie attach --url`, and
listening for other machines takes `cookie daemon --listen`; see
[A daemon already running](../daemon/persistent.md#a-daemon-already-running)
and [Remote access](../daemon/remote-access.md).

## The token

Every connection presents the daemon's bearer token, `[server] token`. Clients
and runtimes on the daemon's machine read it from the same `config.toml`;
elsewhere it is passed with `--token` or `COOKIE_DAEMON_TOKEN`. Anyone holding
the token is a fully trusted client. See
[Security](../daemon/security.md#daemon-authentication-token).

## One store, one owner

A session tree is written by one process at a time, which holds its
`owner.lock`. An engine started beside a running daemon still lists the
daemon's sessions, but opens them as read-only snapshots. See
[Sessions](sessions.md#ownership).

## Related

- [Persistent daemon](../daemon/persistent.md) and
  [Remote access](../daemon/remote-access.md) for running it.
- [Server `[server]`](../engine/server.md) for its keys.
- [Architecture](../architecture.md#process-model) for how the engine, the
  server, and the clients are built.

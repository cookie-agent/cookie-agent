# Clients

A client shows the daemon's sessions and acts on them. cookie ships three:

- the **[terminal UI](../clients/tui.md)**, `cookie` or `cookie attach`;
- the **[web page](../clients/web.md)** the daemon serves on its own port,
  opened with `cookie web`;
- the **[command line](../clients/cli.md)**: `cookie config`, `memory`,
  `machine`, `mcp`, `connect`, and the other commands that call a running
  daemon. `cookie run` is the exception: it runs its own engine.

## One protocol

All of them speak the same [client protocol](../reference/protocol.md):
JSON-RPC 2.0 over a WebSocket, versioned with semver and frozen at its major
(see [Compatibility](../reference/compatibility.md)). A connection starts
with a handshake naming the protocol version and the client: a free-form
`kind` such as `tui`, `cli`, or `web`, and an `instance_id` the client keeps
across reconnects. The daemon never treats one kind differently from another,
so a client of your own is a peer of the shipped ones; see
[Write a client](../development/clients.md).

## Many clients at once

Any number of clients can attach to one daemon. Each sees every session live,
can work in any of them, and is told about changes another client makes:

- **Selections are shared.** A session has one agent, model, variant, and
  permission mode. When one client picks them, every client's draft follows,
  and the latest pick wins.
- **Steer or queue.** While a run is active, a message either *steers*, joining
  the running conversation at the next tool or completion boundary, or
  *queues*, starting a run of its own when this one completes. The daemon
  decides atomically, so a run that another client starts or that ends while
  you type only changes which happens. See
  [Steering and queueing](../clients/tui.md#steering-queueing-and-the-pending-strip).
- **Only the sender takes a message back.** A pending message shows which
  client sent it; only that client can recall it into its composer.
- **Edits check what you saw.** Reverting a session is refused when it gained
  events the client has not shown yet, so nothing unseen is deleted.

## Browsers

A browser page connects like any client, with the token in its handshake.
The daemon takes it from any origin; the token is the guard. See
[Web clients](../daemon/remote-access.md#web-clients).

## A client's machine

A TUI attached to a daemon on another machine offers its own machine to it as
a runtime that dials in, `client_<host name>`, so its sessions still work in
the directory it was started in. See [Runtimes](runtimes.md#a-clients-machine).

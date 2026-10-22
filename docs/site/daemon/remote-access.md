# Remote access

This page covers reaching a daemon from other machines: listening on another
address with TLS, web clients, and forwarding the daemon's port
through SSH, a private network, or a tunnel. [Persistent daemon](persistent.md)
covers starting the daemon and attaching clients to it,
[Runtimes](runtimes.md) the runtimes that dial in from elsewhere,
[Running a runtime as a service](runtime-service.md) keeping them connected, and
[Security](security.md) what the token and the listener protect. The
`[server]` keys are in [Server](../engine/server.md).

## Listening on other addresses

For clients and runtimes on other machines, `cookie daemon` takes:

| Option | Meaning |
|---|---|
| `--listen <addr:port>` | Bind this address instead, such as `0.0.0.0:7419`. Without TLS the token crosses the network in the clear, so a plaintext listener belongs on a private network such as a VPN. Conflicts with `--port`. |
| `--tls-cert <path>`, `--tls-key <path>` | Serve `wss` with this PEM certificate chain and private key; the ready line then shows a `wss://` URL. Both or neither. |

The daemon serves clients on `/ws` and [dial-in runtimes](runtimes.md#runtimes-that-dial-in)
on `/runtime?id=<runtime ID>`. Both present the same bearer token: a runtime
in the `Authorization` header, a client there or in its
[handshake](../reference/protocol.md#handshake-and-authentication). A
request to `/runtime` carrying a browser `Origin` header is refused; `/ws`
ignores the header.

## Web clients

A page in a browser connects with the token in its handshake. The daemon
takes it from any origin: the daemon's own page, a page served elsewhere, or
the daemon's page behind a proxy, which a browser reports under the proxy's
origin. The token is the guard, so keep it secret. A page served over HTTPS
can only connect with `wss://`, so it needs the daemon's TLS options or a
proxy that provides them.

## Forwarding and tunnels

The [web page](../clients/web.md) and `/ws` share one port, so forwarding that
port forwards both. The page then finds the daemon at its own address.

- **An SSH tunnel:** `ssh -L 7419:127.0.0.1:7419 host`. Open
  `http://127.0.0.1:7419/`.
- **Tailscale Serve** gives the loopback daemon an HTTPS address on the
  tailnet without listening anywhere else: `tailscale serve --bg 7419`
  serves `https://<host>.<tailnet>.ts.net/`. An HTTPS
  page can be [installed as an app](../clients/web.md#installing-as-an-app)
  on a phone.
- **Tailscale or a LAN.** Run `cookie daemon --listen 0.0.0.0:7419` and
  open its address, such as `http://100.101.102.103:7419/`.
  Without TLS the token crosses the network in the clear, so keep this to a
  private network.
- **A tunnel such as cloudflared** gives an HTTPS address, such as
  `https://cookie.example.com`. Point it at the daemon's port.
  - A page served over HTTPS connects with `wss://`. The tunnel provides
    that.
  - A proxy may serve the daemon under a subpath, such as
    `https://example.com/cookie/`. The page then connects to
    `wss://example.com/cookie/ws`.

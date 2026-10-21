# Remote access

This page covers reaching a daemon from other machines: listening on another
address with TLS, allowing browser origins, and forwarding the daemon's port
through SSH, a private network, or a tunnel. [Persistent daemon](persistent.md)
covers starting the daemon and attaching clients to it,
[Runtimes](runtimes.md) the runtimes that dial in from elsewhere, and
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
[handshake](../reference/protocol.md#handshake-and-authentication). A request
carrying a browser `Origin` header is refused unless `allowed_origins` lists
that origin (on `/runtime`, always).

## Web clients

A page in a browser connects with the token in its handshake, from an origin
`allowed_origins` lists. The daemon's own page is no exception:

```toml
[server]
allowed_origins = ["https://cookie.example", "http://localhost:5173"]
# or: allowed_origins = ["*"]
```

Each entry is an origin exactly as a browser sends it in its `Origin` header:
a lowercase `scheme://host`, with `:port` only when the port is not the
scheme's default, and no path or trailing slash. The single entry `"*"` allows
any origin, and combining it with other entries is a configuration error.
With `"*"`, any page can open a connection, and the token is the only
protection. A page served over HTTPS can only connect with `wss://`, so it
needs the daemon's TLS options. Changing the list takes a daemon restart.

## Forwarding and tunnels

The [web page](../clients/web.md) and `/ws` share one port, so forwarding that
port forwards both. The page then finds the daemon at its own address.

- **An SSH tunnel:** `ssh -L 7419:127.0.0.1:7419 host`. Open
  `http://127.0.0.1:7419/` and allow `http://127.0.0.1:7419`.
- **Tailscale Serve** gives the loopback daemon an HTTPS address on the
  tailnet without listening anywhere else: `tailscale serve --bg 7419`
  serves `https://<host>.<tailnet>.ts.net/`. Allow that origin. An HTTPS
  page can be [installed as an app](../clients/web.md#installing-as-an-app)
  on a phone.
- **Tailscale or a LAN.** Run `cookie daemon --listen 0.0.0.0:7419` and
  allow the address you open, such as `http://100.101.102.103:7419`.
  Without TLS the token crosses the network in the clear, so keep this to a
  private network.
- **A tunnel such as cloudflared** gives an HTTPS address. Point it at the
  daemon's port, and allow the tunnel's origin, such as
  `https://cookie.example.com`.
  - A page served over HTTPS connects with `wss://`. The tunnel provides
    that.
  - A proxy may serve the daemon under a subpath, such as
    `https://example.com/cookie/`. The page then connects to
    `wss://example.com/cookie/ws`.

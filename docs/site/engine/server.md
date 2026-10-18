# Server [server]

Configure the separate [server/client mode](../guide/run.md#separate-server-and-client).
Complete `config.toml`:

```toml
[server]
host = "127.0.0.1"
port = 7419
token = "<43-char base64url>"
url = "ws://127.0.0.1:7419/ws"
allowed_origins = []
```

Omission keeps the defaults. An authored table replaces them completely;
omitted fields use the defaults below. See [config.toml](../guide/configuration.md).
Unknown fields fail. The port accepts `0` through `65535`; `0` asks the OS for
an available port, so clients must use the actual bound port. `cookie daemon
--port <N>` overrides the configured port for that run (including `--port 0`).

By default the daemon binds IPv4 loopback at the configured port. Its bearer
token is `[server] token`: when the configuration has none, the daemon
generates one at its first start and writes it there, so it stays the same
across restarts. It also prints it in its first stdout line
(`daemon-ready {"url":...,"token":...}`). Clients and dial-in runtimes on this
machine read it from the same `config.toml`; elsewhere, pass `--token`, set
`COOKIE_DAEMON_TOKEN`, or copy it into that machine's `[server] token`. See the
[security contract](../guide/security.md#daemon-authentication-token).

For clients and runtimes on other machines, `cookie daemon` takes:

| Option | Meaning |
|---|---|
| `--listen <addr:port>` | Bind this address instead, such as `0.0.0.0:7419`. Without TLS the token crosses the network in the clear, so a plaintext listener belongs on a private network such as a VPN. Conflicts with `--port`. |
| `--tls-cert <path>`, `--tls-key <path>` | Serve `wss` with this PEM certificate chain and private key; the ready line then shows a `wss://` URL. Both or neither. |

The daemon serves clients on `/ws` and [dial-in runtimes](../guide/runtimes.md#runtimes-that-dial-in)
on `/runtime?id=<runtime ID>`. Both present the same bearer token: a runtime
in the `Authorization` header, a client there or in its
[handshake](../reference/protocol.md#handshake-and-authentication). A request
carrying a browser `Origin` header is refused unless `allowed_origins` lists
that origin (on `/runtime`, always).

## Keys

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | `"127.0.0.1"` | Interface the daemon listens on. Must be non-empty and at most 255 characters. The `cookie` binary additionally requires exactly `"127.0.0.1"` at startup; use `--listen` to bind another address. |
| `port` | integer | `7419` | TCP port for the WebSocket daemon. |
| `token` | string | *(none)* | The daemon's bearer token, exactly 43 base64url characters. The daemon writes a fresh one here when there is none; clients and dial-in runtimes on this machine use it when given no `--token`. |
| `url` | string | `ws://<host>:<port>/ws` | The daemon clients and dial-in runtimes on this machine connect to when given no `--url` (or URL after `--daemon`): a `ws://` or `wss://` URL whose path is `/ws`. Set it on a machine whose daemon runs elsewhere. The daemon itself ignores it. |
| `allowed_origins` | array of strings | `[]` | Web origins whose pages may connect to `/ws`: exact `scheme://host[:port]` origins as browsers send them, or the single entry `"*"` for any origin (see [Web clients](#web-clients)). Empty refuses every request with an `Origin` header; a request without one is unaffected. |

## Web clients

A page in a browser connects with the token in its handshake, from an origin
`allowed_origins` lists:

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

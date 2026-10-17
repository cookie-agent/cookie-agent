# Server [server]

Configure the separate [server/client mode](../guide/run.md#separate-server-and-client).
Complete `config.toml`:

```toml
[server]
host = "127.0.0.1"
port = 7419
```

Omission keeps the defaults. An authored table replaces them completely;
omitted fields use the defaults below. See [config.toml](../guide/configuration.md).
Unknown fields fail. The port accepts `0` through `65535`; `0` asks the OS for
an available port, so clients must use the actual bound port. `cookie daemon
--port <N>` overrides the configured port for that run (including `--port 0`).

By default the daemon binds IPv4 loopback at the configured port. Each run
generates a fresh in-memory bearer token and prints it once as its first
stdout line (`daemon-ready {"url":...,"token":...}`); clients connect with
`--token` or `COOKIE_DAEMON_TOKEN`. See the
[security contract](../guide/security.md#daemon-authentication-token).

For clients and runtimes on other machines, `cookie daemon` takes:

| Option | Meaning |
|---|---|
| `--listen <addr:port>` | Bind this address instead, such as `0.0.0.0:7419`. Without TLS the token crosses the network in the clear, so a plaintext listener belongs on a private network such as a VPN. Conflicts with `--port`. |
| `--tls-cert <path>`, `--tls-key <path>` | Serve `wss` with this PEM certificate chain and private key; the ready line then shows a `wss://` URL. Both or neither. |
| `--token-file <path>` | Use the token this file holds, or create the file (owner-only, `0600` on Unix) with a fresh token when it does not exist, so the token survives restarts. Dial-in runtimes need it to reconnect after a daemon restart. |

The daemon serves clients on `/ws` and [dial-in runtimes](../guide/runtimes.md#runtimes-that-dial-in)
on `/runtime?id=<runtime ID>`. Both present the same bearer token; a request
carrying a browser `Origin` header is refused.

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | `"127.0.0.1"` | Interface the daemon listens on. Must be non-empty and at most 255 characters. The `cookie` binary additionally requires exactly `"127.0.0.1"` at startup; use `--listen` to bind another address. |
| `port` | integer | `7419` | TCP port for the WebSocket daemon. |

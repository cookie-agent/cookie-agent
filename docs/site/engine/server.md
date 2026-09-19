# Server [server]

Configure the separate [server/client mode](../guide/run.md#separate-server-and-client).
Complete `config.toml`:

```toml
[server]
host = "127.0.0.1"
port = 7419
```

Omission inherits the lower layer. An authored table replaces it completely;
omitted fields use the defaults below. See [config.toml](../guide/configuration.md).
Unknown fields fail. The port accepts `0` through `65535`; `0` asks the OS for
an available port, so clients must use the actual bound port. `cookie daemon
--port <N>` overrides the configured port for that run (including `--port 0`).

The binary only binds IPv4 loopback. Remote exposure is not an implemented run
mode. Each daemon run generates a fresh in-memory bearer token and prints it
once as its first stdout line (`daemon-ready {"url":...,"token":...}`); clients
connect with `--token` or `COOKIE_DAEMON_TOKEN`. The token is never persisted.
See the [security contract](../guide/security.md#daemon-authentication-token).

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | `"127.0.0.1"` | Interface the daemon listens on. Must be non-empty and at most 255 characters. The `cookie` binary additionally requires exactly `"127.0.0.1"` at startup. |
| `port` | integer | `7419` | TCP port for the WebSocket daemon. |

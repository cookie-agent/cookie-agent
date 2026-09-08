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
an available port, so clients must use the actual bound port.

The binary only binds IPv4 loopback. Remote exposure is not an implemented run
mode. WebSocket clients authenticate with the local daemon token; see the
[security contract](../guide/security.md#private-state).

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | `"127.0.0.1"` | Interface the daemon listens on. Must be non-empty and at most 255 characters. The `cookie` binary additionally requires exactly `"127.0.0.1"` at startup. |
| `port` | integer | `7419` | TCP port for the WebSocket daemon. |

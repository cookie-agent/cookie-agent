# Public Address [public]

The address the internet reaches the daemon's public paths at, and a port
that serves those paths alone. The public paths are share links (`/s/…`)
and [webhooks](../sessions/webhooks.md) (`/hooks/…`). Complete `config.toml` for a tunnel at
`https://share.example.com`:

```toml
[public]
url = "https://share.example.com"
listen = "127.0.0.1:7420"
```

An omitted table inherits; an authored table replaces the lower table and uses
defaults for omitted fields. Unknown fields fail. See
[config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `url` | string | *(none)* | The public address share links and webhook URLs are built on: an `http://` or `https://` URL with no credentials, query, or fragment. Unset, they use `[server] page_url`, and without that the address the client reached the daemon's page at. |
| `listen` | socket address | *(none)* | A second listener, such as `"127.0.0.1:7420"`, that serves the public paths and nothing else, over plain HTTP. Port `0` is refused. Changing it takes a daemon restart. |

These were `[share] url` and `[share] listen` until webhooks shared them;
the old keys now fail as unknown. The daemon reads every key; clients learn
the base in the handshake (`ServerHello.share_url`). See
[Remote access](../daemon/remote-access.md#public-share-links) for putting
the public port behind a tunnel.

# Share links [share]

Complete `config.toml` serving share links on a port of their own, for a
tunnel at `https://share.example.com` to point at:

```toml
[share]
enabled = true
url = "https://share.example.com"
listen = "127.0.0.1:7420"
```

An omitted table inherits; an authored table replaces the lower table and uses
defaults for omitted fields. Unknown fields fail. See
[config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Whether the daemon serves share links and makes new ones. Off, every link answers as gone and no link is made; links are kept, and work again once it is on. |
| `url` | string | *(none)* | The public address links are built on: an `http://` or `https://` URL with no credentials, query, or fragment. Unset, links use `[server] page_url`, and without that the address the client reached the daemon's page at. |
| `listen` | socket address | *(none)* | A second listener, such as `"127.0.0.1:7420"`, that serves share links (`/s/…`) and nothing else, over plain HTTP. Port `0` is refused. Changing it takes a daemon restart. |

The daemon reads every key; clients learn the share base in the handshake
(`ServerHello.share_url`). A share link is a public URL for one
[asset](../sessions/assets.md): see [Share links](../sessions/sharing.md) for
making and revoking them, and
[Remote access](../daemon/remote-access.md#public-share-links) for putting
the share-only port behind a tunnel.

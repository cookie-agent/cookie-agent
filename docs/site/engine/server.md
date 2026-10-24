# Server [server]

Configure the separate [server/client mode](../daemon/persistent.md#separate-server-and-client).
Complete `config.toml`:

```toml
[server]
host = "127.0.0.1"
port = 7419
token = "<43-char base64url>"
url = "ws://127.0.0.1:7419/ws"
session_idle_eviction_after = "30m"
```

Omission keeps the defaults. An authored table replaces them completely;
omitted fields use the defaults below. See [config.toml](configuration.md).
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
[security contract](../daemon/security.md#daemon-authentication-token).

Plain `cookie` on this machine uses a daemon listening at
`ws://<host>:<port>/ws` of this table, when `token` is set, instead of
starting an engine of its own; it never reads `url` for that. See
[A daemon already running](../daemon/persistent.md#a-daemon-already-running).

For clients and runtimes on other machines, `cookie daemon` takes `--listen`
and TLS options; see [Remote access](../daemon/remote-access.md).

## Keys

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | `"127.0.0.1"` | Interface the daemon listens on. Must be non-empty and at most 255 characters. The `cookie` binary additionally requires exactly `"127.0.0.1"` at startup; use `--listen` to bind another address. |
| `port` | integer | `7419` | TCP port for the WebSocket daemon. |
| `token` | string | *(none)* | The daemon's bearer token, exactly 43 base64url characters. The daemon writes a fresh one here when there is none; clients and dial-in runtimes on this machine use it when given no `--token`. |
| `url` | string | `ws://<host>:<port>/ws` | The daemon clients and dial-in runtimes on this machine connect to when given no `--url` (or URL after `--daemon`): a `ws://` or `wss://` URL whose path is `/ws`. Set it on a machine whose daemon runs elsewhere. The daemon itself ignores it. |
| `session_idle_eviction_after` | duration string | `"30m"` | How long a root session's tree sits unused (no new event, nobody watching it, no pending work) before it is paged out of memory. Using the session again reloads it from disk. Compact `ms`, `s`, `m`, `h`, and `d` suffixes are accepted; `"0s"` pages a tree out at the next check, made every minute, once nobody watches it. Unlike the rest of the table, a change applies at once, from the next check. |

## The web page

The daemon serves the [web client](../clients/web.md) on its listener, over
TLS when it serves `wss`. The page needs no token: it holds no secret, and
the connection it opens authenticates in its handshake. `/ws` takes a
connection from any web origin, or none; the token is the guard.

| Path | Answer |
|---|---|
| `/` | `index.html`, `Cache-Control: no-cache` |
| a file of the page | the file, with its content type; `Cache-Control: public, max-age=31536000, immutable` when its name carries a hash of its content, `no-cache` otherwise |
| `/ws`, `/runtime` | the client and runtime endpoints; see [Listening on other addresses](../daemon/remote-access.md#listening-on-other-addresses) |
| anything else | 404 |

Every file carries an `ETag`, so a revalidated file costs no body. A file
that compresses is sent gzip-compressed to a browser whose `Accept-Encoding`
takes gzip (`Vary: Accept-Encoding`, with its own `ETag`). Every response
carries the page's `Content-Security-Policy`:

- scripts only from the daemon, including WebAssembly and the page's
  inline scripts by hash;
- WebSocket connections to any daemon (`ws:`, `wss:`), since the page may
  name one on another host;
- its service worker and manifest only from the daemon;
- `blob:` media;
- no framing.

The page includes a web app manifest, its icons and screenshots, and a
service worker (`sw.js`, sent with `no-cache` like `index.html`), so a
browser can install it as an app (see
[Installing as an app](../clients/web.md#installing-as-an-app)). The
service worker caches nothing, so the page always matches the daemon's
build. A
`cookie` built without the page (see
[Building the page](../clients/web.md#building-the-page)) serves a
placeholder `index.html` that says how to build it. After its ready line,
the daemon prints the page's address.

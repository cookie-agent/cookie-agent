# Web

`cookie daemon` serves a web client on the same port as its client endpoint.
Open it in a browser on the daemon's machine, or on a laptop or phone that
reaches that port. This page covers opening it and connecting it. Later
versions of the page add sessions and the rest of what the TUI does.

## Opening the page

Allow the page's origin first. A browser connection is accepted only from
an origin `[server] allowed_origins` lists (see
[Web clients](../engine/server.md#web-clients)), and the page is no
exception. For a daemon on the default port, opened on the same machine:

```toml
[server]
allowed_origins = ["http://127.0.0.1:7419"]
```

Restart the daemon after changing the list. While the list is empty, the
daemon's startup output says the page cannot connect yet.

Then print the page's address with the token:

```sh
cookie web          # prints http://127.0.0.1:7419/#token=<token>
cookie web --open   # also opens it in the system browser
```

`cookie web` finds the daemon like `cookie attach`:

- `--url` and `--token` (or `COOKIE_DAEMON_TOKEN`) first;
- then `[server] url` and `[server] token` in this machine's `config.toml`.

The page is the client URL with `ws` turned into `http` (`wss` into
`https`) and the final `/ws` dropped.

The token travels in the address's fragment (`#token=`), which a browser
never sends to a server. The page keeps the token in the browser's
`localStorage` under the daemon's URL and removes the fragment from the
address bar.

## The connect screen

- **Daemon URL.** The page fills it in from its own address:
  - `http` becomes `ws`, and `https` becomes `wss`;
  - the host and port stay the same;
  - `ws` is added to the page's directory.

  You can edit it; the page remembers the last URL it connected to.
- **Token.** It comes from a `cookie web` link, or you paste it from the
  daemon's ready line.
- **Connecting.** With both a URL and a token, the page connects without a
  click.
- **Forget this daemon** deletes the stored token.
- **A refused connection.** A browser shows a page no reason when the daemon
  closes its connection before the handshake. The usual cause is an origin
  missing from `allowed_origins`, so the page quotes its own origin as the
  entry to add.
- **Once connected**, the page shows the daemon's version.
  - **Dropped connection.** The page redials with the TUI's backoff,
    500 ms rising to 10 s, under the same client identity.
  - **Background tab.** When it becomes visible again, the page checks the
    connection.
  - **Separate clients.** Each browser tab is its own client.
- **Theme.** The theme button cycles through system, light, and dark. The
  choice stays in the browser.

## Reaching the daemon from elsewhere

The page and `/ws` share one port, so forwarding that port forwards both.
The page then finds the daemon at its own address.

- **An SSH tunnel:** `ssh -L 7419:127.0.0.1:7419 host`. Open
  `http://127.0.0.1:7419/` and allow `http://127.0.0.1:7419`.
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

## Hosting the page elsewhere

Every release attaches the page as `cookie-agent-web.tar.gz`. Its files use
relative paths, so any static host can serve them from any path. The page
then fills in a URL on that host, which you edit to name the daemon. Allow
the host's origin in the daemon's `allowed_origins`.

## Building the page

A `cookie` built from a fresh checkout serves a placeholder page.
`scripts/build-web.sh` builds the real one:

- it needs `trunk` 0.21.14 and the `wasm32-unknown-unknown` target;
- it writes the page to `crates/web_assets/bundle/`, which the next build
  of `cookie` embeds.

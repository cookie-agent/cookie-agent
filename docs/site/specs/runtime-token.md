# The runtime token: dial-in runtimes without the client token

Status: **implemented** (2026-11-04; client protocol 1.87.0). The user's decisions are in
[section 6](#6-decisions-2026-11-04).

## 1. Problem

The daemon has one secret, `[server] token`. It opens `/ws` for clients,
`/runtime` for dial-in runtimes, and plugin pages (whose ticket is a keyed
hash of it). To connect a runtime that dials in, the user copies that token
onto the runtime's machine: the web client's dial-in command embeds the
page's own token, and the `add-cookie-runtime` skill does the same.

Dial-in runtimes run on the machines the user trusts least: a shared
cluster node, a cloud VM. Whoever reads the token there is a fully trusted
client. They can drive every session and every machine, change the
configuration, and read what clients read. A runtime needs far less: it
serves tools and, if it opts in, sends producer messages to the sessions
attached to it. Rotating the client token also cuts every remote runtime
off at once.

## 2. Design

**A second secret, `[server] runtime_token`.** 43 base64url characters, like
`token`. The daemon creates it at start when the configuration has none and
writes it to `config.toml` with `token`, owner-only. It must differ from
`token`; a configuration where they match is invalid.

**Where each token works.**

| Route | `token` (client) | `runtime_token` |
|---|---|---|
| `/ws` (clients, the web page) | yes | no: refused as a wrong token |
| `/runtime` (dial-in runtimes) | yes | yes |
| `/plugins/…` (page ticket) | yes | no |

A client is fully trusted and could add a runtime anyway, so `/runtime`
still takes the client token. Runtimes already set up keep working, and
`cookie runtime serve --daemon` on the daemon's own machine needs nothing new.

**One token for every runtime.** Any holder of the runtime token may dial in
under any runtime ID the daemon would otherwise accept (not a built-in or
configured one, not one connected now). The token does not expire.

**Getting the token.** Any client may read it with
`machines.runtime_token` (result `{ "token": string | null }`, null when the
daemon serves without one, as an engine started for tests does). The CLI
prints it with `cookie machine runtime-token`. The web client's *Add a
runtime* dial-in command asks for it instead of embedding the page's token,
and still masks it on screen.

**The runtime side.** `cookie runtime serve --daemon` takes `--token`, else
`COOKIE_RUNTIME_TOKEN`, else `[server] runtime_token` from that machine's
`config.toml`, else `[server] token` there. Like `COOKIE_DAEMON_TOKEN`,
`COOKIE_RUNTIME_TOKEN` is removed from cookie's environment at start, so the
processes a runtime starts do not inherit it.

**Rotating it.** Change or remove `[server] runtime_token` and restart the
daemon (a new one is drawn when it is removed). Like `token`, a changed
`runtime_token` reads as a `server` change waiting for a restart. After the
restart, runtimes still holding the old token are refused when they dial in
again. Clients are not affected.

## 3. What does not change

- Templates the daemon starts over ssh or stdio present no token.
- The runtime protocol: the token travels in `Authorization` as before. A
  refused `/runtime` upgrade still answers 401, now saying "the runtime
  token is missing or wrong".
- Plugin pages and share links.

## 4. Affected places

- `config`: `ServerConfig::runtime_token`, validation, writing both tokens.
- `cookie daemon`: draws and writes the missing tokens, passes the runtime
  token to the listener, and notes the ones it created so they do not read as
  pending restarts.
- `server`: `/runtime` accepts either token; `machines.runtime_token`.
- `protocol`, `protocol_transport`: the method (client protocol minor bump).
- `cookie runtime serve`, `cookie machine runtime-token`, `main.rs` env scrub.
- Web: the dial-in command.
- Docs: security, remote access, dial-in, runtime service, server
  configuration, CLI, client protocol and its changelog, the daemon concept
  page.
- The official `add-cookie-runtime` skill (`cookie-agent/skills`): use the
  runtime token for dial-ins, never the client token.

## 5. Not doing

- Per-runtime tokens bound to an ID and revocable one at a time. Considered
  and declined: one shared runtime token is enough for now.
- Expiry.
- Rotating without a restart.

## 6. Decisions (2026-11-04)

1. `/runtime` keeps accepting the client token as well as the runtime token.
2. One runtime token shared by every dial-in runtime, not one per runtime.
3. The runtime token does not expire.

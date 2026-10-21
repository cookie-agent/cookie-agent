# Persistent daemon

This page covers running the daemon as a process of its own: starting it and
attaching clients to it, how plain `cookie` finds a daemon already running on
the machine, and running the daemon and its runtimes as services. Its clients
are the [terminal UI](../clients/tui.md), the [web client](../clients/web.md),
and the [command line](../clients/cli.md); [Remote access](remote-access.md)
covers reaching it from other machines, [Runtimes](runtimes.md) the runtimes
it uses, and [Security](security.md) its trust boundaries.

## Separate server and client

The daemon binds to `127.0.0.1:7419` by default:

```sh
cookie daemon
```

Its first stdout line is the machine-readable ready frame; everything the
daemon prints afterward is human logging. A supervisor writes the daemon's
stdout to a pipe, reads exactly that one line, and ignores the rest:

```text
daemon-ready {"url":"ws://127.0.0.1:7419/ws","token":"<43-char base64url>"}
```

The token is `[server] token` in `config.toml`; the daemon writes a fresh one
there at its first start when there is none, and keeps it across restarts.
Treat it, and the ready line, as a secret: anyone who reads either can
connect to the daemon.
Pass `--port 0` to let the OS pick an ephemeral port; the ready line then
carries the real port, and clients must use that URL rather than guessing 7419.

Plain `cookie` in another terminal on the same machine finds it (see
[A daemon already running](#a-daemon-already-running)). `cookie attach`
requires it:

```sh
cookie attach
```

`attach`, `web`, `connect`, `disconnect`, `mcp`, `config`, `machine`, and
`memory` find the daemon in this machine's `config.toml`: `[server] url`, else the local daemon the same
table describes (`ws://127.0.0.1:<port>/ws`), and `[server] token`. Flags
override either: `--url` (any host, `ws://` or `wss://` with the exact `/ws`
path) and `--token` (or `COOKIE_DAEMON_TOKEN`), so a local `cookie attach`
can still reach a daemon elsewhere:

```sh
cookie attach --url wss://laptop.example:7419/ws --token '<43-char base64url>'
```

A daemon on another machine listens with `--listen`; see
[Remote access](remote-access.md). The daemon also serves a web client on its
port; `cookie web` prints its address with the token (see [Web](../clients/web.md)). Unlike plain `cookie`, `cookie attach` never
falls back to an engine of its own: a daemon it cannot reach or use is an
error.

## A daemon already running

Plain `cookie` looks for a daemon on this machine before it starts an engine
of its own, so every client of a running daemon, `cookie daemon` as a
service included, sees the same sessions:

- It looks only when `config.toml` has `[server] token`, which
  `cookie daemon` writes at its first start. Without one it starts its own
  engine and touches no network.
- It looks at `ws://<[server] host>:<[server] port>/ws` and never at
  `[server] url`, which may name another machine: reaching another machine
  takes `cookie attach`. A daemon started with `--port` or `--listen` on
  another address, or serving only TLS, is not found.
- Nothing listening (the connection is refused, or it is not open within
  half a second) starts its own engine, silently.
- A daemon that answers within two seconds and accepts the handshake gets
  the TUI, exactly as `cookie attach` would: a dropped connection is
  retried, and quitting the TUI leaves the daemon running.
- Something that answers but cannot be used, such as a daemon refusing the
  token, a daemon of another protocol major, a TLS listener, or another
  program, starts its own engine with a notice that names the address and
  the reason.

When the daemon runs another version of cookie than this binary, the TUI
says so once; restart the daemon to upgrade it. `cookie --standalone`
always starts its own engine and never looks; it takes no subcommand.
`cookie run` never looks either: it always runs its own engine. An engine
of its own beside a running daemon opens the daemon's sessions as
read-only snapshots. A daemon running as another OS user is not told apart:
it reports the same machine, so its `local` runtime serves the directory
with that user's file permissions. Use `--standalone` or a
[runtime that dials in](runtimes.md#runtimes-that-dial-in) with such a
daemon.

The daemon, `cookie runtime serve`, and the TUI stop cleanly on a stop
signal: `SIGINT`, `SIGTERM` (`kill`, systemd), or `SIGHUP` (a closed
terminal); on Windows Ctrl-C, Ctrl-Break, a closed console, logoff, or
shutdown. The daemon stops listening and shuts down its engine and the
runtimes it started; a runtime cancels its calls and stops its MCP servers; the
TUI restores the terminal. A second signal during that shutdown exits the
daemon or runtime at once.

## Running as a service

Both the daemon and a runtime that dials in run until stopped, so each can
be a systemd user service. On the daemon's machine,
`~/.config/systemd/user/cookie-daemon.service`:

```ini
[Unit]
Description=Cookie Agent daemon

[Service]
ExecStart=%h/.cargo/bin/cookie daemon --listen 0.0.0.0:7419
Restart=on-failure

[Install]
WantedBy=default.target
```

On a machine whose `config.toml` names the daemon (`[server] url` and
`token`; see [Runtimes that dial in](runtimes.md#runtimes-that-dial-in)),
`~/.config/systemd/user/cookie-runtime.service`:

```ini
[Unit]
Description=Cookie Agent runtime

[Service]
ExecStart=%h/.cargo/bin/cookie runtime serve --daemon
Restart=on-failure

[Install]
WantedBy=default.target
```

Enable each with `systemctl --user enable --now <name>`, and run
`loginctl enable-linger` so they run without a login session. Either may
start first: the runtime retries until the daemon answers. The journal keeps
the daemon's ready line, token included, readable by whoever can read your
user journal. A daemon running as a service has
no `SSH_AUTH_SOCK` from your shell, so an ssh `[runtimes.<id>]` template
needs a key file in its arguments (`-i`) or an agent socket set with the
unit's `Environment=`.

One daemon serves sessions of any directory. The TUI, whether attached or
running the daemon itself, creates its sessions for the directory it was
started in: they work there with `local`'s `coding` capability, and its
session list shows that directory's sessions. Sessions stay in the store of
the directory the daemon started in.

Plain `cookie` on a found daemon and `cookie attach` choose the runtime
for the directory the same way, first match first:

1. The daemon runs on this machine (its handshake reports this machine's
   ID, or either side reports none): its `local` runtime.
2. A connected runtime of the daemon runs on this machine (the daemon
   places it on a machine whose IDs include this one's; see
   [Machines](runtimes.md#machines)) and offers a capability that takes a
   workspace: new sessions attach that capability
   at the directory. With several, the lowest runtime ID wins. An example
   is a daemon in a container with `cookie runtime serve --daemon` running
   beside it on the host.
3. Otherwise the TUI offers this machine to the daemon: it dials in a
   runtime named `client_<host name>` that serves the TUI's directory only,
   and the TUI's new sessions use its `coding` capability there. The
   runtime lives as long as the TUI; sessions keep its attachment and find
   it again when the TUI attaches again from this machine. Another TUI's
   `client_<host name>` is never chosen in step 2.

The bottom bar shows the chosen runtime and the directory.
Configured plugins and eager MCP servers, which come only from the user
`config.toml`, start during engine initialization, before a model tool call is
approved. Repository `AGENTS.md` files and project skills still enter model
context, so open unfamiliar workspaces with care.
Allowed shell commands run without a filesystem sandbox; see the
[security contract](security.md#process-boundary).

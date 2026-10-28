# Persistent daemon

This page covers running the daemon as a process of its own: starting it and
attaching clients to it, how plain `cookie` finds a daemon already running on
the machine, and running the daemon as a service on Linux, macOS, and
Windows. Its clients
are the [terminal UI](../clients/tui.md), the [web client](../clients/web.md),
and the [command line](../clients/cli.md); [Remote access](remote-access.md)
covers reaching it from other machines, [Runtimes](../runtimes/index.md) the runtimes
it uses, [Running a runtime as a service](../runtimes/service.md) keeping a
runtime on another machine connected, and [Security](security.md) its trust boundaries.

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
[runtime that dials in](../runtimes/dial-in.md) with such a
daemon.

The daemon, `cookie runtime serve`, and the TUI stop cleanly on a stop
signal: `SIGINT`, `SIGTERM` (`kill`, systemd), or `SIGHUP` (a closed
terminal); on Windows Ctrl-C, Ctrl-Break, a closed console, logoff, or
shutdown. The daemon stops listening and shuts down its engine and the
runtimes it started; a runtime cancels its calls and stops its MCP servers; the
TUI restores the terminal. A second signal during that shutdown exits the
daemon or runtime at once.

## Restarting the daemon

Some changes take effect only when the daemon starts: the `server`,
`plugins`, and `runtimes` parts of the configuration (`cookie config
status` lists those waiting), and a new cookie binary. Any client can
restart the daemon in place:

- **TUI:** `/restart`, or the *Restart daemon* button of the banner the TUI
  shows while a change waits for a restart.
- **Web client:** Settings → *Restart daemon*, or the same banner.
- **Command line:** `cookie daemon restart`, with `--now` to skip the wait:

  ```sh
  cookie daemon restart --url ws://127.0.0.1:7419/ws --token <token>
  ```

Each asks first: **when idle**, the default, waits until no turn is running
in any session and every [background call](../reference/tools.md#background-calls) has
reported its result; **now** interrupts the running turns, which end as after
any stop, and ends the background calls without a report. While a restart waits, new turns are refused with "the daemon is
restarting", queued input and producers' messages wait for the restarted
daemon (queued input starts when a client opens its session again), and a cron fire due meanwhile is recorded as skipped
(`daemon restarting`). Clients show *Restarting when idle…* until the
connection closes, then reconnect as after any dropped connection.

The daemon then shuts down as on a stop signal: it ends the runs, stops its
plugins, MCP servers, and the runtimes it started, flushes its sessions and
stores, and closes client connections saying it is restarting. Then it
starts its own executable again with the same arguments, environment, and
directory, so the restart runs the binary now on disk. It reads
`config.toml` anew and keeps its token and port. Dial-in runtimes reconnect
on their own; no session is lost.

- **Linux and macOS:** the daemon replaces itself in place (`exec`), keeping
  its process ID, so systemd and launchd see the same process and the
  ready line is printed again on the same output.
- **Windows:** the daemon starts a new process with the same command line
  and exits with status 0; the new process waits up to 30 seconds for the
  port to free. It inherits the old one's output, so the log goes on in the
  same file, but it is a new process: a scheduled task counts the old one
  as finished, and `Stop-ScheduledTask` no longer stops the daemon. Stop it
  with `cookie daemon`'s process ID, which its listening line names
  (`Stop-Process -Id <pid>`), or end the task's processes from Task
  Manager.

Only `cookie daemon` can restart itself; a daemon embedded in another
program refuses with `restart_unsupported`.

## Threads and memory

cookie runs its work on at most 8 async worker threads, fewer on a machine
with fewer cores: it waits on models, tools, and clients far more than it
computes. On Linux with glibc it also keeps at most 2 malloc arenas, so a
many-core machine does not hold memory in an arena per thread. The
`TOKIO_WORKER_THREADS` and `MALLOC_ARENA_MAX` environment variables override
either limit.

## Running as a service

The daemon runs until stopped, so a service manager can keep it up: start it
at login or boot, and restart it when it exits with an error. Pick your
platform below. Every unit runs `cookie daemon` as you, because the daemon
reads your `config.toml`, keeps its sessions under your home directory, and
runs the shell commands and MCP servers of its `local` runtime with your
permissions.

The daemon needs no token on its command line: it reads `[server] token`
from `config.toml`, writing one there at its first start. What you choose is
the listener: without flags it binds `127.0.0.1:7419`, which is all that
clients on the same machine need; `--listen`, `--tls-cert`, and `--tls-key`
are covered in [Remote access](remote-access.md). The examples below use
`--listen 0.0.0.0:7419`; drop it for a loopback-only daemon.

!!! note "The daemon already has a runtime"
    A daemon carries its own runtimes on its own machine: the built-in
    `local` runtime with the `coding` capability, and `global`. Do not run
    `cookie runtime serve` beside it. A separate runtime service is for
    *other* machines that dial in to the daemon; see
    [Running a runtime as a service](../runtimes/service.md).

Where the installer put the binary decides the path in each unit. The
shell and PowerShell installers install to Cargo's home: `~/.cargo/bin` on
Linux and macOS, `%USERPROFILE%\.cargo\bin` on Windows. Adjust the paths if
you installed elsewhere.

### Linux

A systemd user service,
`~/.config/systemd/user/cookie-daemon.service`:

```ini
[Unit]
Description=Cookie Agent daemon

[Service]
ExecStart=%h/.cargo/bin/cookie daemon --listen 0.0.0.0:7419
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

Enable and start it, and let it run without a login session, so it starts at
boot rather than at your first login:

```sh
systemctl --user daemon-reload
systemctl --user enable --now cookie-daemon
loginctl enable-linger "$USER"
```

Read its output with `journalctl --user -u cookie-daemon`; follow it with
`-f`. The journal keeps the daemon's ready line, token included, readable by
whoever can read your user journal. `systemctl --user restart cookie-daemon`
restarts it, for example to upgrade it after installing a new version.
[Remove it](#remove-the-linux-service) when you no longer want it.

A user service starts with a minimal environment, not your shell's. The
daemon's runtimes capture your shell's environment instead (see
[Shell Environment](../engine/shell_environment.md)): `bash` commands, MCP
servers, ACP agents, plugins, and `[runtimes.<id>]` template commands start
with the `PATH` and variables a new terminal of yours has. A `.bashrc` edit
takes effect at the next `cookie config reload`, with no unit change and no
restart. The daemon's own process keeps the unit's environment; set
`Environment=` in the unit only for what your shell does not set, such as an
agent socket for an ssh template when no rc file starts one (an ssh
`[runtimes.<id>]` template can also name a key file with `-i`).

#### Remove the Linux service

Stop the service and keep it from starting again, then delete its unit file
and tell systemd:

```sh
systemctl --user disable --now cookie-daemon
rm ~/.config/systemd/user/cookie-daemon.service
systemctl --user daemon-reload
```

`loginctl enable-linger` applies to your user as a whole, not to this
service. If no other user service of yours needs to run without a login
session (the [runtime service](../runtimes/service.md#linux) is one), turn it
off too:

```sh
loginctl disable-linger "$USER"
```

The daemon's output lives in the user journal, which has no file to delete;
its entries age out with the journal's own limits.

### macOS

A launchd LaunchAgent runs in your login session, which is where your
`config.toml`, keychain, and browser profile are.
`~/Library/LaunchAgents/dev.cookie-agent.daemon.plist`; launchd does not
expand `~` or `$HOME`, so write the absolute paths (`/Users/<you>/...`):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.cookie-agent.daemon</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/<you>/.cargo/bin/cookie</string>
    <string>daemon</string>
    <string>--listen</string>
    <string>0.0.0.0:7419</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>/Users/<you>/Library/Logs/cookie-daemon.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/<you>/Library/Logs/cookie-daemon.log</string>
</dict>
</plist>
```

`KeepAlive` with `SuccessfulExit` false restarts the daemon after an error
and leaves it stopped after a clean stop. Load it, and unload it again:

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.cookie-agent.daemon.plist
launchctl bootout gui/$(id -u)/dev.cookie-agent.daemon
```

`launchctl kickstart -k gui/$(id -u)/dev.cookie-agent.daemon` restarts it.
The log file holds the daemon's output, and so its ready line with the
token: `chmod 600` it. A LaunchAgent starts when you log in, not at boot.

#### Remove the macOS LaunchAgent

Unload the agent, then delete its plist and its log:

```sh
launchctl bootout gui/$(id -u)/dev.cookie-agent.daemon
rm ~/Library/LaunchAgents/dev.cookie-agent.daemon.plist
rm -f ~/Library/Logs/cookie-daemon.log
```

`bootout` fails with an input/output error when the agent is not loaded,
which is fine: carry on with the `rm` lines.

### Windows

A Task Scheduler task that starts at your logon, in your session and with
your profile, restarts the daemon after a failure, and has no time limit.
In PowerShell:

```powershell
$cookie = "$env:USERPROFILE\.cargo\bin\cookie.exe"
$logs = "$env:LOCALAPPDATA\cookie-agent"
New-Item -ItemType Directory -Force $logs | Out-Null
$action = New-ScheduledTaskAction -Execute "conhost.exe" -Argument "--headless cmd.exe /c `"`"$cookie`" daemon --listen 0.0.0.0:7419 >> `"$logs\daemon.log`" 2>&1`""
$trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$settings = New-ScheduledTaskSettingsSet `
  -ExecutionTimeLimit (New-TimeSpan) `
  -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
  -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName "Cookie Agent daemon" -Action $action `
  -Trigger $trigger -Settings $settings -RunLevel Limited
Start-ScheduledTask -TaskName "Cookie Agent daemon"
```

`Stop-ScheduledTask` stops it. The output goes to
`%LOCALAPPDATA%\cookie-agent\daemon.log`, ready line and token included:
keep that folder to your user, which it is by default.

- **No console window.** `conhost.exe --headless` runs the command without a
  window on Windows 10 and 11. It is a switch of the console host that
  Microsoft does not document, so if a Windows update changes it, drop it:
  the task then shows a console window while the daemon runs. Closing that
  window stops the daemon.
- **A Windows service** (with NSSM or `sc.exe`) also works for the daemon,
  and can start before anyone logs in. It runs as another account or as
  `LocalSystem`, so it reads that account's `config.toml` and keeps that account's
  sessions, not yours. Run it as your own account.

#### Remove the Windows task

Stop the task, unregister it, and delete its log:

```powershell
Stop-ScheduledTask -TaskName "Cookie Agent daemon" -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName "Cookie Agent daemon" -Confirm:$false
Remove-Item "$env:LOCALAPPDATA\cookie-agent\daemon.log" -ErrorAction SilentlyContinue
```

`%LOCALAPPDATA%\cookie-agent` also holds the
[runtime service's](../runtimes/service.md#windows) log; delete the folder
when neither task remains. A Windows service you created yourself goes with
the tool that made it, such as `nssm remove <name> confirm` or
`sc.exe delete <name>`.

## Choosing the runtime for a directory

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
   [Machines](../runtimes/index.md#machines)) and offers a capability that takes a
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

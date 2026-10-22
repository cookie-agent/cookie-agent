# Running a runtime as a service

This page covers keeping a [runtime that dials in](runtimes.md#runtimes-that-dial-in)
connected to its daemon: a service on Linux, macOS, or Windows that starts
`cookie runtime serve --daemon` and restarts it. To keep the daemon itself
running, see [Persistent daemon](persistent.md#running-as-a-service).

!!! note "Not next to the daemon"
    A daemon already carries a runtime on its own machine: the built-in
    `local` runtime with the `coding` capability, and `global`. Sessions
    work on the daemon's machine without anything else running. A runtime
    service is for *other* machines, such as a lab computer, a server, or
    the laptop whose browser or files a session should reach.

## What the service runs

```sh
cookie runtime serve --daemon <daemon-url> --id <id> --token <token>
```

- `<daemon-url>` is the daemon's client URL, `wss://host.example.ts.net/ws`
  or `ws://192.168.1.10:7419/ws`: `ws://` or `wss://` and the exact `/ws`
  path. Without it, `--daemon` takes `[server] url` from this machine's
  `config.toml`, else the local daemon.
- `--id <id>` names the runtime; without it, it is the host name. Only one
  runtime may be connected under an ID. See
  [Runtimes that dial in](runtimes.md#runtimes-that-dial-in).
- `--token <token>` is the daemon's token: copy it from `[server] token`
  in the daemon's `config.toml`. The examples below put it in the service
  definition. An inline token is visible in that definition and in the
  process's command line on that machine; the runtime does not pass it on
  to the processes it starts, such as its `bash` calls and MCP servers.
  `COOKIE_DAEMON_TOKEN` and `[server] token` in this machine's
  `config.toml` work too; see [Reaching a daemon](../clients/cli.md#reaching-a-daemon).
- `--root <dir>`, repeated, limits the workspaces the runtime serves.

Four things decide how a service has to be set up:

- **It retries.** The runtime does not exit when the daemon is unreachable,
  at its start or later: it prints the reason and retries after 1 s, 2 s,
  4 s, and so on up to 30 s, and reconnects after every drop. Start order
  does not matter, and a daemon restart, a network change, or a sleeping
  laptop waking up needs no help from the service manager. Its instances go
  `reconnecting` and `failed` meanwhile, and are reopened when it is back;
  see [Reconnecting](runtimes.md#runtimes-that-dial-in).
- **It exits on a refusal that retrying cannot fix**, a wrong token or an ID
  that is reserved or configured on the daemon, with an error. A service
  manager that restarts it will repeat that error; read the log rather than
  raising the restart count.
- **It stops cleanly on a stop signal** (`SIGINT`, `SIGTERM`, `SIGHUP`; on
  Windows Ctrl-C, Ctrl-Break, a closed console, logoff, or shutdown),
  cancelling its calls and stopping its MCP servers, so a service manager's
  ordinary stop is right. A clean stop exits successfully, so
  restart-on-failure leaves it stopped.
- **It runs as the user it serves.** The runtime hosts its own machine's MCP
  servers, those of `[runtimes.local.mcp.servers.*]` in that user's
  `config.toml`, and its tools see that user's files and profile. Run the
  service as the account whose configuration and browser profile it needs,
  and, for browser MCP servers, inside that user's login session.

The installers put the binary in Cargo's home: `~/.cargo/bin/cookie` on
Linux and macOS, `%USERPROFILE%\.cargo\bin\cookie.exe` on Windows. Adjust the
paths if you installed elsewhere. A sleeping machine runs nothing: the
runtime is away until the machine wakes, and then reconnects by itself.

## Linux

A systemd user service, `~/.config/systemd/user/cookie-runtime.service`:

```ini
[Unit]
Description=Cookie Agent runtime
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=%h/.cargo/bin/cookie runtime serve --daemon wss://host.example.ts.net/ws --id <id> --token <token>
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

The unit holds the token, so make it readable only by you, then start it:

```sh
chmod 600 ~/.config/systemd/user/cookie-runtime.service
systemctl --user daemon-reload
systemctl --user enable --now cookie-runtime
loginctl enable-linger "$USER"
```

`enable-linger` starts the user's services at boot without a login session.
Without it the runtime runs only while you are logged in. Follow the output
with `journalctl --user -u cookie-runtime -f`.

A user service starts with a minimal environment: MCP servers that need
`PATH` entries or variables of your shell need `Environment=` lines in the
unit.

To remove the service, stop it, delete the unit, which holds the token,
and tell systemd:

```sh
systemctl --user disable --now cookie-runtime
rm ~/.config/systemd/user/cookie-runtime.service
systemctl --user daemon-reload
```

A unit set up under earlier versions of this page read the token from
`~/.config/cookie-agent/runtime.env`; delete that file too
(`rm -f ~/.config/cookie-agent/runtime.env`).

Lingering is per user, not per service: run `loginctl disable-linger "$USER"`
only when no other user service of yours, such as the
[daemon's](persistent.md#remove-the-linux-service), needs it.

## macOS

A LaunchAgent runs in your login session, which a runtime that reaches your
browser or profile needs.
`~/Library/LaunchAgents/dev.cookie-agent.runtime.plist`, with absolute
paths because launchd expands neither `~` nor `$HOME`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.cookie-agent.runtime</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/<you>/.cargo/bin/cookie</string>
    <string>runtime</string>
    <string>serve</string>
    <string>--daemon</string>
    <string>wss://host.example.ts.net/ws</string>
    <string>--id</string>
    <string><id></string>
    <string>--token</string>
    <string><token></string>
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
  <string>/Users/<you>/Library/Logs/cookie-runtime.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/<you>/Library/Logs/cookie-runtime.log</string>
</dict>
</plist>
```

The plist holds the token, so make it readable only by you, then load it:

```sh
chmod 600 ~/Library/LaunchAgents/dev.cookie-agent.runtime.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.cookie-agent.runtime.plist
launchctl bootout gui/$(id -u)/dev.cookie-agent.runtime
```

`launchctl kickstart -k gui/$(id -u)/dev.cookie-agent.runtime` restarts it,
and `tail -f ~/Library/Logs/cookie-runtime.log` shows what it prints. A
LaunchAgent starts when you log in, not at boot.

To remove the agent, unload it and delete the plist, which holds the token,
and the log:

```sh
launchctl bootout gui/$(id -u)/dev.cookie-agent.runtime
rm ~/Library/LaunchAgents/dev.cookie-agent.runtime.plist
rm -f ~/Library/Logs/cookie-runtime.log
```

## Windows

A Task Scheduler task at your logon is the recommended path: it runs in your
session, with your profile, and so reaches your Chrome for browser MCP
servers. Register it, with the token in its action:

```powershell
$cookie = "$env:USERPROFILE\.cargo\bin\cookie.exe"
$logs = "$env:LOCALAPPDATA\cookie-agent"
New-Item -ItemType Directory -Force $logs | Out-Null
$action = New-ScheduledTaskAction -Execute "conhost.exe" -Argument "--headless cmd.exe /c `"`"$cookie`" runtime serve --daemon <daemon-url> --id <id> --token <token> >> `"$logs\runtime.log`" 2>&1`""
$trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$settings = New-ScheduledTaskSettingsSet `
  -ExecutionTimeLimit (New-TimeSpan) `
  -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
  -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName "Cookie Agent runtime" -Action $action `
  -Trigger $trigger -Settings $settings -RunLevel Limited
Start-ScheduledTask -TaskName "Cookie Agent runtime"
```

The empty `New-TimeSpan` removes the task's default three-day execution
limit. The task's output goes to `%LOCALAPPDATA%\cookie-agent\runtime.log`.
`Stop-ScheduledTask -TaskName "Cookie Agent runtime"` stops it.

- **No console window.** `conhost.exe --headless` runs the command without a
  window on Windows 10 and 11. It is a switch of the console host that
  Microsoft does not document: if a Windows update changes it, drop it, and
  the task shows a console window while the runtime runs; closing that window
  stops the runtime.
- **A Windows service** (NSSM, or `sc.exe` with a wrapper) can start before
  anyone logs in, but it runs outside your session, as another account or
  `LocalSystem`. It then reads that account's `config.toml`, and cannot reach
  your Chrome, so browser MCP servers do not work through it. Use it only
  for a runtime that needs files and shell commands, not your session.

To remove the task, which holds the token, unregister it and delete its log:

```powershell
Stop-ScheduledTask -TaskName "Cookie Agent runtime" -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName "Cookie Agent runtime" -Confirm:$false
Remove-Item "$env:LOCALAPPDATA\cookie-agent\runtime.log" -ErrorAction SilentlyContinue
```

A task set up under earlier versions of this page read the token from a
`COOKIE_DAEMON_TOKEN` user variable that `setx` stored. Delete it from the
registry (`HKCU\Environment`) for processes started afterwards:

```powershell
[Environment]::SetEnvironmentVariable('COOKIE_DAEMON_TOKEN', $null, 'User')
```

A shell that is already open keeps its copy until you close it. Do not use
`setx COOKIE_DAEMON_TOKEN ""`: that stores an empty value instead of
deleting the variable. `%LOCALAPPDATA%\cookie-agent` also holds the
[daemon task's](persistent.md#windows) log; delete the folder when neither
task remains.

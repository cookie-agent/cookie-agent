# Installation

## Shell installer

On Linux or macOS, install the latest stable release with the cargo-dist
installer:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/cookie-agent/cookie-agent/releases/latest/download/cookie_agent-installer.sh | sh
```

The installer selects the archive for the current platform, verifies its
SHA256 checksum, and installs `cookie` under `$CARGO_HOME/bin` (normally
`~/.cargo/bin`).

## PowerShell installer

On Windows x86_64 or ARM64, install the latest stable release with the
cargo-dist PowerShell installer:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/cookie-agent/cookie-agent/releases/latest/download/cookie_agent-installer.ps1 | iex"
```

The installer selects the matching MSVC ZIP archive, verifies its SHA256
checksum, and installs `cookie.exe` under `%CARGO_HOME%\bin` (normally
`%USERPROFILE%\.cargo\bin`).

## Release archives

Stable and nightly [GitHub releases](https://github.com/cookie-agent/cookie-agent/releases)
provide an archive and `.sha256` checksum for:

- Linux x86_64 with glibc
- Linux x86_64 with musl
- Linux aarch64 with glibc
- macOS Apple silicon
- macOS Intel
- Windows x86_64 (MSVC), as a ZIP archive
- Windows ARM64 (MSVC), as a ZIP archive

Linux and macOS archives use tar; Windows archives use ZIP. Download the archive
for your platform, verify its checksum, extract it, and place `cookie` or
`cookie.exe` on your `PATH`.

## Nightly builds

Once a day, when `main` changed since the last nightly and passes every check,
CI publishes it as a timestamped `v*-alpha.<timestamp>` prerelease with
archives for all seven targets. Nightly prereleases are retained for about two
weeks, with at least the newest three always kept. They are intended for
testing; `cookie --version` names the nightly and the commit it was built
from.

Open the [prerelease list](https://github.com/cookie-agent/cookie-agent/releases?q=prerelease%3Atrue)
and select a nightly to download an archive or use its generated installer
command. Each prerelease provides PowerShell (`irm ... | iex`) and shell
(`curl ... | sh`) installers pinned to that prerelease.

## Updating

```sh
cookie update
```

updates `cookie` to the newest release on the channel it came from: a stable
release to the newest stable, a nightly to the newest nightly. `cookie update
--check` only reports; `cookie update --channel nightly` (or `stable`)
switches channels, and `cookie update --version <tag>` installs one release.
It replaces the binary it runs from, wherever it was installed, after
checking the download's SHA-256 and that the new binary runs. A build from
`cargo install` or a checkout updates the way it was built, or with an
explicit `--channel`.

A running daemon keeps the version it started with until it restarts: run
`cookie daemon restart` when convenient (it waits for running turns and
background calls). Each machine's runtimes run that machine's own `cookie`,
so update it on each. Releases from before `cookie update` existed (stable
`v0.3.0` and older, and the nightlies before it) update once with
the installer above. See [`cookie update`](clients/cli.md#cookie-update).

## Connect a model

Run `cookie` in a project and connect a provider with `/connect`; the
[quickstart](quickstart.md) walks through it and a first session.
To remove cookie again, see [Uninstall](#uninstall).

## Uninstall

Remove cookie in the order below: services first, so nothing restarts the
binary you are about to delete; then the binary and what the installer set up
around it; then, only if you want it, your data. The steps for the installers
are written for the default install under Cargo's home; if you extracted a
release archive yourself, delete the `cookie` or `cookie.exe` you placed and
skip the PATH and receipt steps.

### 1. Stop and remove services

Quit any running `cookie`. Then undo whatever you set up to keep cookie
running or reachable, each in its own page:

- **The daemon as a service:** remove the
  [Linux systemd unit](daemon/persistent.md#remove-the-linux-service), the
  [macOS LaunchAgent](daemon/persistent.md#remove-the-macos-launchagent), or
  the [Windows scheduled task](daemon/persistent.md#windows).
- **A runtime service** on this or another machine: the removal steps at
  the end of each platform's section of
  [Running a runtime as a service](runtimes/service.md), including
  the token file or `COOKIE_DAEMON_TOKEN` user variable that earlier
  versions of that page set up.
- **Tailscale Serve or a tunnel** pointing at the daemon: run
  `tailscale serve reset`; see
  [Remote access](daemon/remote-access.md#forwarding-and-tunnels).
- **The web client as an installed app:**
  [uninstall it in the browser](clients/web.md#installing-as-an-app).

A daemon you started by hand stops with Ctrl-C in its terminal.

### 2. Remove the binary

The installers put `cookie` in Cargo's home and nothing else on disk besides
an install receipt (below). Delete it:

=== "Linux and macOS"

    ```sh
    rm ~/.cargo/bin/cookie
    ```

=== "Windows (PowerShell)"

    ```powershell
    Remove-Item "$env:USERPROFILE\.cargo\bin\cookie.exe"
    ```

If `CARGO_HOME` was set when you installed, the binary is in
`$CARGO_HOME/bin` (`%CARGO_HOME%\bin`) instead. `command -v cookie`
(`Get-Command cookie`) shows which `cookie` your shell runs.

The installers write an install receipt, `cookie_agent-receipt.json`, which
the cargo-dist updater reads. Cookie's release archives contain no updater,
so nothing reads it, but delete it:

=== "Linux and macOS"

    ```sh
    rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/cookie_agent/cookie_agent-receipt.json"
    rmdir "${XDG_CONFIG_HOME:-$HOME/.config}/cookie_agent" 2>/dev/null
    ```

=== "Windows (PowerShell)"

    ```powershell
    $receipts = if ($env:XDG_CONFIG_HOME) { "$env:XDG_CONFIG_HOME\cookie_agent" } else { "$env:LOCALAPPDATA\cookie_agent" }
    Remove-Item "$receipts\cookie_agent-receipt.json" -ErrorAction SilentlyContinue
    Remove-Item $receipts -ErrorAction SilentlyContinue
    ```

The last line of each fails harmlessly if the folder still holds something
else.

If you installed cookie with `cargo install` instead, `cargo uninstall
cookie_agent` removes the binary.

### 3. Undo the PATH changes

The installer put Cargo's `bin` folder on your `PATH` only if it was not on
it already. That folder is shared with Rust, so leave this step out if you
use Cargo or rustup.

On **Linux and macOS** the shell installer appended a line that sources
`~/.cargo/env` to the shell startup files that existed (it creates
`~/.profile` if none did): `. "$HOME/.cargo/env"` in `~/.profile`,
`~/.bashrc`, `~/.bash_profile`, `~/.bash_login`, `~/.zshrc`, and
`~/.zshenv`, and `source "$HOME/.cargo/env.fish"` in
`~/.config/fish/conf.d/cookie_agent.env.fish`. Remove those lines (or that
file), and `~/.cargo/env` and `~/.cargo/env.fish` if nothing else uses
them. Open a new shell afterwards. `grep -n 'cargo/env' ~/.profile ~/.bashrc
~/.bash_profile ~/.bash_login ~/.zshrc ~/.zshenv` finds the lines. The
installer adds them only if `cargo`'s `bin` was not already on `PATH`, and not
at all when `COOKIE_AGENT_NO_MODIFY_PATH=1` was set at install time.

On **Windows** the PowerShell installer added the `bin` folder to your user
`Path` (the `Path` value in `HKEY_CURRENT_USER\Environment`). Remove it:

```powershell
$bin = "$env:USERPROFILE\.cargo\bin"
$path = [Environment]::GetEnvironmentVariable('Path', 'User') -split ';' | Where-Object { $_ -and $_ -ne $bin }
[Environment]::SetEnvironmentVariable('Path', ($path -join ';'), 'User')
```

Use your `CARGO_HOME` instead of `$env:USERPROFILE\.cargo` if you set one.
The change applies to shells you open afterwards. (This rewrites the value
as plain text; a `Path` that held `%VARIABLE%` entries has them expanded.)

### 4. Remove your data (optional, destructive)

Everything cookie keeps for you lives in one folder, `~/.cookie-agent`
(`%USERPROFILE%\.cookie-agent` on Windows), apart from the logs of the
services above. Deleting it is **permanent and cannot be undone**:

- `config.toml` and `tui.toml`, with the daemon's token (`[server] token`);
- `providers/`, your connected providers' credentials;
- `mcp-oauth.json`, the OAuth credentials of remote MCP servers;
- `acp-agents.json`, what ACP agents last offered (options, file kinds), which drafts show;
- `native-compaction.json`, the providers native compaction stopped being tried for;
- `deferred-tools.json`, the models that refused deferred tools;
- `sessions/`, every session's history;
- `memory/`, `projects/`, and `machines.json`;
- `agents/` and `skills/` you wrote there;
- `catalog/`, a cache of the model catalog that cookie downloads again;
- `machine-id`, when the machine has none of its own.

Cookie keeps no data, cache, or credential outside this folder, and uses no
system keychain. Copy out what you
want to keep first, such as `config.toml`, `agents/`, `skills/`, and
`memory/`. To forget a provider's credentials but keep the rest, use
[`cookie disconnect`](clients/cli.md#cookie-disconnect) before you delete the
binary.

=== "Linux and macOS"

    ```sh
    rm -rf ~/.cookie-agent
    ```

=== "Windows (PowerShell)"

    ```powershell
    Remove-Item "$env:USERPROFILE\.cookie-agent" -Recurse -Force
    ```

Projects have their own `.cookie-agent/` folder only if you made one there
(project configuration, agents, and skills); delete it in each project that
has one. A daemon on another machine keeps its own `~/.cookie-agent` there.

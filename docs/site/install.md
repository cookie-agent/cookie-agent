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

CI publishes Windows x86_64 and ARM64 installers with nightly prereleases. The
latest tagged release, `v0.2.0`, predates those assets, so Windows installers
currently ship through nightly prereleases and will also ship with the next
tagged release.

Open a nightly prerelease and run its generated, tag-pinned PowerShell installer
command. After the next tagged release, the stable `releases/latest` installer
URL will work for Windows as well.

The installer selects the matching MSVC ZIP archive, verifies its SHA256
checksum, and installs `cookie.exe` under `%CARGO_HOME%\bin` (normally
`%USERPROFILE%\.cargo\bin`).

## Release archives

Current nightly [GitHub releases](https://github.com/cookie-agent/cookie-agent/releases)
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

Every push to `main` creates a timestamped `v*-alpha.<timestamp>` prerelease with
archives for all seven targets. Nightly prereleases are retained for about two
weeks, with at least the newest three always kept. They are intended for
testing; run `cookie --version` to see the source commit hash embedded in the
binary.

Open the [prerelease list](https://github.com/cookie-agent/cookie-agent/releases?q=prerelease%3Atrue)
and select a nightly to download an archive or use its generated installer
command. Each prerelease provides PowerShell (`irm ... | iex`) and shell
(`curl ... | sh`) installers pinned to that prerelease.

## Quick start

After installation, change to the workspace where cookie agent should operate.
Running `cookie` without a subcommand starts a local daemon and opens the TUI.

### Start without configuration

Both user and workspace configuration are optional. Start cookie agent directly:

```sh
cookie
```

When no provider is available, the TUI starts in setup mode. Type `/connect`,
select a managed provider, and fill in its recipe-defined setup and credential
fields. The durable store is global to the user, so other workspaces can use the
same compatible connection. The form does not contact the provider; the first
model request verifies the credentials.

### Optional user configuration

Create `~/.cookie-agent/config.toml` only when you have settings to author. For
example, an environment-backed managed provider can be declared as:

```toml

[providers.openai]
source = "models_dev"
api_key = "${env:OPENAI_API_KEY}"
```

Export the variable before launching cookie agent:

```sh
export OPENAI_API_KEY='your-key'
cookie
```

Workspace-specific settings use the same syntax in
`<cwd>/.cookie-agent/config.toml` and take precedence over user settings. A
same-ID workspace provider, MCP server, plugin, or agent replaces the complete
user entry; nested fields never merge.

See [Providers](guide/providers.md) for precedence rules and custom providers.

### First run

When at least one model is available, select an agent and model if needed, type
a request in the composer, and press Enter. If no authored root agent is
runnable, the engine supplies the built-in `default` coding agent.

Useful first commands are `/help`, `/sessions`, `/new`, `/compact`, and
`/cancel`. The [TUI guide](guide/tui.md) covers editing, steering, approvals,
selection, and message actions. [Agents](guide/agents.md) explains the built-in
internal agents and how to author your own.

### Separate daemon and TUI

The daemon binds to `127.0.0.1:7419` by default:

```sh
cookie daemon
```

Attach from another terminal:

```sh
cookie attach
```

The attach URL defaults to `ws://127.0.0.1:7419/ws` and may be changed with
`--url`. Only loopback WebSocket URLs with the exact `/ws` path are accepted.

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

## Build from source

Install Rust 1.88 or newer and clone the repository. Cargo fetches the locked,
git-pinned Oven SDK dependencies automatically. Install the locked workspace
version with:

```sh
cargo install --locked --path crates/cookie_agent
```

For development, `cargo build --locked -p cookie_agent` writes the binary to
`target/debug/cookie`.

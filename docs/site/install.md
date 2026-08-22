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

Windows installers will be available starting with the next Windows-enabled
tagged release. Once that release is published, Windows x86_64 and ARM64 users
can run the PowerShell installer:

```powershell
irm https://github.com/cookie-agent/cookie-agent/releases/latest/download/cookie_agent-installer.ps1 | iex
```

The installer selects the matching MSVC ZIP archive, verifies its SHA256
checksum, and installs `cookie.exe` under `%CARGO_HOME%\bin` (normally
`%USERPROFILE%\.cargo\bin`).

## Release archives

Each [GitHub release](https://github.com/cookie-agent/cookie-agent/releases)
provides an archive and `.sha256` checksum for:

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

The rolling
[`nightly` prerelease](https://github.com/cookie-agent/cookie-agent/releases/tag/nightly)
contains the same five platform archives built from the latest successful push
to `main`. Nightly builds are replaced in place and are intended for testing.
Run `cookie --version` to see the source commit hash embedded in the binary.

## Build from source

Install Rust 1.88 or newer, clone the repository, and install the locked
workspace version:

```sh
cargo install --locked --path crates/cookie_agent
```

For development, `cargo build --locked -p cookie_agent` writes the binary to
`target/debug/cookie`.

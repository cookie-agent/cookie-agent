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

## Release archives

Each [GitHub release](https://github.com/cookie-agent/cookie-agent/releases)
provides a tar archive and `.sha256` checksum for:

- Linux x86_64 with glibc
- Linux x86_64 with musl
- Linux aarch64 with glibc
- macOS Apple silicon
- macOS Intel

Download the archive for your platform, verify it with `sha256sum` or
`shasum -a 256`, extract it, and place `cookie` on your `PATH`.

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

# Building and Testing

## Toolchain and source checkout

Development requires Git and Rust 1.88 or newer. Clone the repository and enter
the workspace:

```sh
git clone https://github.com/cookie-agent/cookie-agent.git
cd cookie-agent
```

Cargo fetches the locked, git-pinned Oven SDK dependencies automatically. Build
the debug binary with:

```sh
cargo build --locked -p cookie_agent
```

The binary is `target/debug/cookie`. To install the workspace version into
`$CARGO_HOME/bin`, run:

```sh
cargo install --locked --path crates/cookie_agent
```

The workspace contains twelve crates: `coding_runtime`, `config`,
`cookie_agent`, `engine`, `identity`, `mcp`, `models`, `plugin_sdk`,
`protocol`, `runtime_api`, `server`, and `tui`.
Their public interfaces are available in the [Rust API documentation](../reference/api.md).

## Required gates

CI runs every gate below. Locally, `scripts/check-changed.sh [base]` runs
the ones a change needs: formatting always, Clippy and the tests of the
changed crates and the crates that depend on them, and the wasm32, web,
compatibility, and documentation checks only when their inputs changed (`-n`
prints the commands without running them). `AGENTS.md` lists the rules.

Use locked dependency resolution for workspace builds and tests. Tests run
under [cargo-nextest](https://nexte.st) (`cargo install cargo-nextest
--locked`), configured in `.config/nextest.toml`, and the doctests, which
nextest does not run, under `cargo test`:

```sh
cargo build --locked --workspace --all-targets
cargo nextest run --workspace --locked --no-fail-fast
cargo test --doc --workspace --locked
```

Run formatting and Clippy with warnings denied under stable and the Rust 1.88
minimum supported toolchain:

```sh
cargo fmt --all -- --check
cargo +stable clippy --locked --workspace --all-targets -- -D warnings
cargo +1.88.0 clippy --locked --workspace --all-targets -- -D warnings
```

Check the protocol compatibility baselines, documentation, and dependency policy:

```sh
crates/protocol/scripts/check-schema-additive.sh
./scripts/build-docs.sh
cargo deny --locked check advisories licenses sources
```

The compatibility check and documentation require Python 3. Before the
protocols freeze, any change to a wire type, method, or protocol version needs
regenerated baselines: run `crates/protocol/scripts/update-compat-baseline.sh`
and commit `crates/protocol/compat/`. For the documentation, install
`requirements-docs.txt` before running `./scripts/build-docs.sh`. That
script builds workspace rustdoc and runs MkDocs in strict mode;
`./scripts/build-docs.sh --no-rustdoc` skips rustdoc when only the prose
changed.

## Windows targets

CI builds and tests both `x86_64-pc-windows-msvc` and
`aarch64-pc-windows-msvc` on native Windows runners. Use the matching MSVC Rust
target when validating Windows-specific changes.

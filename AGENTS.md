# Contributor Guide

## Commits

Keep commits focused on one logical change. Use the repository's conventional
subject style: `type(scope): imperative summary`, for example
`refactor(engine): remove obsolete decoder`. Omit the scope for repository-wide
changes when no single component owns the work.

## Compatibility

The client, plugin, and runtime protocols and session history follow
`docs/site/specs/compatibility-v1.md`; `docs/site/reference/compatibility.md`
states its rules for contributors. Every wire change must pass
`crates/protocol/scripts/check-compat.py` (run by `check-schema-additive.sh`)
with the right semver bump: additions are minor, breaking changes major.
Readers tolerate information they do not know; commands and writers stay
strict. The frozen corpora in `crates/protocol/compat/corpus/` and the golden
session logs in `crates/engine/tests/fixtures/history/` are never
regenerated; a change that fails them breaks data earlier builds wrote.

Session-log converters (`crates/engine/src/events/convert/`) are the only
migrations allowed, and every history-format bump ships one. Everywhere else,
prefer removing obsolete aliases, migrations, and compatibility decoders over
carrying them forward.

## Configuration

Configuration is strict. Unknown keys, removed `schema` or `schema_version`
fields, wrong types, and malformed values are hard errors. Do not silently
ignore fields, migrate authored files, or upconvert old shapes.
Every configuration-surface change must update the configuration reference and
cover defaults, strict unknown-field handling, and boundary semantics in tests.

## TUI

Mouse targets resolve in one place, `App::pointer_target_at`
(`crates/tui/src/ui/app/pointer.rs`): clicks act on its result and hover
highlights it. A new click target is a new `PointerTarget` variant and must
paint a visible hover in `apply_hover`; only text fields, whose caret is their
affordance, and queue rows that can only say why they cannot be recalled stay
hover-quiet (`PointerTarget::is_hoverable`). The sweep in
`crates/tui/src/ui/transcript/tests/pointer.rs` checks every target hovers
visibly, so reach the new target from one of its surfaces.

## Gate Tiers

The gates below come in two tiers. Before each commit, run the local tier
for what the change touches; CI runs every gate on every push that is not
documentation only.

`scripts/check-changed.sh [base]` runs the local tier for the changes since
the merge base with `base` (default `origin/main`), uncommitted and
untracked files included; `-n` prints the commands without running them.
The local tier is:

- `cargo fmt --all -- --check`, always;
- Clippy with warnings denied and the tests of the changed crates and of
  every workspace crate that depends on them, the whole workspace when
  `Cargo.toml`, `Cargo.lock`, `test-support/`, or `examples/` changed;
- the `wasm32-unknown-unknown` Clippy command only when `crates/protocol`,
  `crates/client_state`, or `crates/web` changed;
- `check-schema-additive.sh` and `scripts/test-wasm-client.sh` only when
  `crates/protocol` changed;
- `scripts/build-web.sh --fast` and `scripts/test-web-page.sh` only when
  `crates/web`, `crates/web_assets`, or `scripts/web-e2e` changed;
- `./scripts/build-docs.sh` only when `docs/` or `mkdocs.yml` changed, with
  `--no-rustdoc` unless a crate changed too, and the tests that keep `docs/site/clients/cli.md` and the tools, protocol,
  and errors references in step with the code when those pages changed.

The CI-only tier is the Rust 1.88 MSRV build, the Windows build and tests,
the full workspace test run, the release target checks, and the release
gates (the locked release build and its secret scan, `cargo audit`, and
`cargo deny`). Run one of them locally only when the change is aimed at
it: a Windows-only path (`cargo check --target x86_64-pc-windows-msvc
--all-targets`), a language feature or dependency that may need a newer
Rust than 1.88, or release metadata.

Pushes that change only `docs/**`, Markdown files, or `mkdocs.yml` skip the
CI workflow (the Documentation workflow still builds the site), except for
the pages the tests above keep in step with the code. A documentation-only
commit on `main` therefore publishes no prerelease.

## Gates

CI runs all of these; the local tier above picks among them. Use locked
dependency resolution for builds and tests. Tests run under
[cargo-nextest](https://nexte.st) (`.config/nextest.toml`), each in its own
process and never retried, and the doctests, which nextest skips, under
`cargo test`:

```sh
cargo build --locked --workspace --all-targets
cargo nextest run --workspace --locked --no-fail-fast
cargo test --doc --workspace --locked
```

Run formatting and Clippy with warnings denied under stable and the Rust 1.88
MSRV:

```sh
cargo fmt --all -- --check
cargo +stable clippy --locked --workspace --all-targets -- -D warnings
cargo +1.88.0 clippy --locked --workspace --all-targets -- -D warnings
```

Check the browser side for `wasm32-unknown-unknown` and build the web client
bundle (`scripts/build-web.sh` needs trunk 0.21.14). `scripts/build-web.sh
--fast` builds it in seconds once warm, larger and unoptimized, for local
browser runs; CI and releases build the real one:

```sh
cargo clippy --locked --target wasm32-unknown-unknown -p cookie_agent_protocol --features websocket -p cookie_agent_client_state -p cookie_agent_web --all-targets -- -D warnings
scripts/build-web.sh
```

Run the headless-Chrome suites after changing the protocol client, the
web client, or what the daemon serves to browsers. They need Google Chrome
or Chromium (`CHROME` overrides; a snap's private `/tmp` breaks them), a
matching chromedriver, which `scripts/web-e2e/chrome.sh` downloads from
Chrome for Testing when none is on `PATH` (`CHROMEDRIVER` overrides), and,
for the first, `wasm-bindgen-test-runner` of the `wasm-bindgen` version in
`Cargo.lock`; CI runs both in its `web` job:

```sh
scripts/test-wasm-client.sh
scripts/test-web-page.sh
```

`scripts/test-web-page.sh <scenario>...` runs only the scenarios named in
`SCENARIOS` (`scripts/web-e2e/scenarios.py`) and those they build on;
without names it runs them all, its groups side by side, each with its own
daemons and browser.

Check the protocol compatibility baselines, documentation, and dependency policy:

```sh
crates/protocol/scripts/check-schema-additive.sh
./scripts/build-docs.sh
cargo deny --locked check advisories licenses sources
```

`./scripts/build-docs.sh` builds workspace rustdoc and runs MkDocs in strict
mode; `--no-rustdoc` skips rustdoc for changes to the prose alone. `check-schema-additive.sh` compares the generated protocol schemas with
the baselines in `crates/protocol/compat/`; after changing a wire type, a
method, or a protocol version, regenerate them with
`crates/protocol/scripts/update-compat-baseline.sh` and commit the result.

The three protocol versions are the semver constants
`CLIENT_PROTOCOL_VERSION`, `PLUGIN_PROTOCOL_VERSION`, and
`RUNTIME_PROTOCOL_VERSION` in `crates/protocol/src/semver.rs`. Bumping one
regenerates its baseline and, in the same commit, updates every
human-readable reference to it. For the client version: a new entry in
`docs/site/reference/protocol-changelog.md`, the version in
`docs/site/reference/protocol.md` and `docs/site/architecture.md`, and the
crate-level doc strings in `crates/protocol/src/lib.rs`,
`crates/coding_runtime/src/lib.rs`, `crates/server/src/lib.rs`, and
`crates/cookie_agent/src/main.rs`. For the plugin and runtime versions:
`docs/site/reference/runtime-protocol.md` and
`docs/site/development/plugins.md`. Search for the old version string;
`docs/site/api/**` is generated and excluded.

Tool-emitted system-role messages must not mutate the initial model-history
system turn. Materialize them at the emission point as user turns beginning
with `[tool-emitted system message; materialized as user history]`; this keeps
the cacheable system prefix stable across replay.

Tool prompt sections are composed only at run admission and frozen into
`run_started`; they must never mutate `history[0]` after freeze.

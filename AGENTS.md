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

Session-log converters (`crates/engine_store/src/events/convert/`) are the only
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

## Appearance

Do not add tests that pin colours, styles, classes, or how things look;
test behaviour. The same goes for the exact wording of labels and other
display strings: a test that pins `Worked for 1m 23s · 7 tool calls`
catches only deliberate changes. Test what the text is made from instead:
the counts and states behind it, the arithmetic that formats it (such as a
duration's boundaries), which line or row shows when, and that a cached
row draws again when what it shows changes. For the look itself, the agent
making an appearance change in either client looks at it before
committing: the TUI through the ignored `render_preview` test and
`scripts/tui_preview_png.py`, the web page in headless Chrome
(`scripts/web-screenshots.sh`, or a page driven like the ones in
`scripts/web-e2e/`), in light and dark.

## Gate Tiers

The gates below come in two tiers. Before each commit, run the local tier
for what the change touches; CI runs every gate on every push that is not
documentation only.

`scripts/check-changed.sh [base]` runs the local tier for the changes since
the merge base with `base` (default `origin/main`), uncommitted and
untracked files included; `-n` prints the commands without running them.
On Linux it runs the tests with `TMPDIR` in a directory of its own on the
`/dev/shm` tmpfs when that has room: the tests' many small writes and
fsyncs then never wait on a busy disk, which made the engine tier's run
several times faster and its timing failures go away. Run tests by hand
the same way when the disk is busy (`TMPDIR="$(mktemp -d /dev/shm/t.XXXXXX)"`,
removed afterwards). The local tier is:

- `cargo fmt --all -- --check`, always;
- Clippy with warnings denied and the tests of the changed crates and of
  every workspace crate that depends on them, the whole workspace when
  `Cargo.toml`, `Cargo.lock`, `test-support/`, or `examples/` changed;
- rustdoc of the same crates with warnings denied
  (`RUSTDOCFLAGS='-D warnings' cargo doc --no-deps`), as CI builds it on
  every push: a public doc linking a private item fails there;
- the `wasm32-unknown-unknown` Clippy command only when `crates/protocol`,
  `crates/protocol_transport`, `crates/client_state`, or `crates/web`
  changed;
- `python3 scripts/third-party-notices.py --check` only when `Cargo.lock`,
  the script, or the web page's bundled license files changed;
- `check-schema-additive.sh` only when `crates/protocol` changed, and
  `scripts/test-wasm-client.sh` when it or `crates/protocol_transport` did;
- `scripts/build-web.sh --fast` and `scripts/test-web-page.sh` only when
  `crates/web`, `crates/web_assets`, or `scripts/web-e2e` changed;
- `./scripts/build-docs.sh` only when `docs/` or `mkdocs.yml` changed, with
  `--no-rustdoc` unless a crate changed too, and the tests that keep `docs/site/clients/cli.md` and the tools, protocol,
  and errors references in step with the code when those pages changed.

The CI-only tier is the Rust 1.88 MSRV build, the Windows build and tests,
the full workspace test run, the release target checks (pull requests
only), and the release gates (`cargo audit`, `cargo deny`, and the release
workflow's secret scan of the binary it ships). Run one of them locally
only when the change is aimed at it: a Windows-only path (`cargo check --target x86_64-pc-windows-msvc
--all-targets`), a language feature or dependency that may need a newer
Rust than 1.88, or release metadata.

Pushes that change only `docs/**`, Markdown files, or `mkdocs.yml` skip the
CI workflow (the Documentation workflow still builds the site), except for
the pages the tests above keep in step with the code. Pushes publish no
prerelease: the nightly comes from CI's daily run on `main` (`RELEASING.md`).

## Build Output

Cargo never removes stale artifacts from a `target` directory. The many
profiles and targets here (debug, `ci`, `web`, wasm, Windows checks), and one
linked test binary per test target, grow it by gigabytes a day, and a
long-lived one reaches terabytes.

- A worktree builds into its own `target`. Remove the worktree, and with it
  that `target`, as soon as its branch lands or is abandoned, and delete the
  branch.
- Never point `CARGO_TARGET_DIR` at a temporary or scratch directory that
  outlives the task. Write gate and test logs there, not builds.
- Tests that need files make them under a `tempfile` directory that removes
  itself, never a fixed path under the system temp directory.
  `test-support/config_harness.rs` removes each test process's isolated home
  as the process exits.
- When a `target` grows large, `cargo clean` it. A compiler cache such as
  sccache keeps the next build reasonable.

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
cargo clippy --locked --target wasm32-unknown-unknown -p cookie_agent_protocol -p cookie_agent_protocol_transport --features cookie_agent_protocol_transport/websocket -p cookie_agent_client_state -p cookie_agent_web --all-targets -- -D warnings
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
python3 scripts/third-party-notices.py --check
```

`THIRD-PARTY-NOTICES.txt` lists every crate the `cookie` binary and the web
page are built from, with the license texts; release archives, the web
page's Settings › About, and the TUI's `/about` carry it. After changing
dependencies, regenerate it with `python3 scripts/third-party-notices.py`
and commit the result.

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
`docs/site/reference/protocol-changelog.md` and the version in
`docs/site/reference/protocol.md`. For the plugin and runtime versions:
`docs/site/reference/runtime-protocol.md` and
`docs/site/development/plugins.md`. Search for the old version string;
`docs/site/api/**` is generated and excluded.

Tool-emitted system-role messages must not mutate the initial model-history
system turn. Materialize them at the emission point as user turns beginning
with `[tool-emitted system message; materialized as user history]`; this keeps
the cacheable system prefix stable across replay.

Tool prompt sections are composed only at run admission and frozen into
`run_started`; they must never mutate `history[0]` after freeze.

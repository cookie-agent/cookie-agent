# Refactor Phase 2: Dependencies, Bindings, CI, and Syntect

Status: draft, implementation authorized on 2026-09-21. Part of the architecture refresh recorded in [Refactor phase 1](refactor-phase-1-test-layout.md), [phase 2](refactor-phase-2-dependencies-ci.md), and [phase 3](refactor-phase-3-module-splits.md). Update the status here and in the [specification index](index.md) when the phase lands on `main`.

Repository: this workspace (paths below are relative to the workspace root).
Sibling repo (author-owned): `../oven_sdk` (sibling checkout of cookie-agent/oven-sdk) (GitHub cookie-agent/oven-sdk), currently at rev 1b99edc930651a82930ab4de475a82a33ba6cdd5 which is exactly what cookie-agent pins.
Branch: `refactor/phase-2-deps-bindings-ci-syntect` (from `main`, after phase 1 has merged)

Hard constraints
- No runtime behavior change. Tests that only existed to guard the removed machinery (vendored syntect hashes, TypeScript bindings) are deleted; everything else keeps passing unchanged.
- AGENTS.md rules apply (strict config, no compat shims, conventional commits, locked builds, fmt/clippy on stable + 1.88).
- `crates/models/tests/release_integrity.rs` is a policy test suite that asserts on `Cargo.toml`, `deny.toml`, CI YAML and the vendor tree. Update its assertions to the new policy in the same commit as each change; do not weaken checks that still apply (pinned oven rev, exact internal versions, pinned CI actions, no secret material).

## Item 3: dependency graph

### 3a. Single reqwest (0.13) by updating oven-sdk

In `oven_sdk` (branch from its `main`):
- Workspace `Cargo.toml`: `reqwest = { version = "0.13", default-features = false, features = ["json", "stream", "rustls"] }` (0.13 renamed the TLS feature; adapt to whatever 0.13.4's feature names actually are).
- Fix API breaks in every crate that uses reqwest (`oven-sdk` transport, provider crates). Run `cargo build --workspace --all-targets`, `cargo test --workspace`, clippy `-D warnings`, `cargo deny check` if the repo has a deny.toml.
- reqwest types are part of oven-sdk's public API (cookie-agent constructs `reqwest::Client` and hands it to oven adapters), so bump each oven crate's minor version (0.5.0 -> 0.6.0, 0.6.0 -> 0.7.0, 0.4.0 -> 0.5.0, 0.3.0 -> 0.4.0) in the same commit.
- Commit `chore(deps)!: move to reqwest 0.13`, push to `origin main` of oven-sdk, record the new commit SHA.

In cookie-agent:
- Workspace `Cargo.toml`: every `oven-sdk*` entry gets the new `version = "=x.y.z"` and the new `rev`. Delete the `reqwest-oven` alias. Add one workspace entry `reqwest = { version = "=0.13.4", default-features = false, features = ["json", "stream", "rustls"] }` and switch `crates/engine/Cargo.toml` and `crates/tools/Cargo.toml` to `reqwest.workspace = true`; `crates/models/Cargo.toml` uses `reqwest.workspace = true` and its sources use `reqwest::` instead of `reqwest_oven::` (`catalog/transport.rs`, `adapters/oven.rs`).
- `cargo update -p reqwest@0.12.28 --precise`-style removal is not needed; just regenerate `Cargo.lock` with `cargo update -w` limited to the changed packages (`cargo update -p oven-sdk -p oven-sdk-openai ...`), then confirm `cargo tree --workspace -d -e normal` shows exactly one `reqwest`, one `hyper`, one `hyper-util`, one `rustls`, one `h2`, one `tower`, one `tower-http`.
- `deny.toml`: the comment names the old rev; update it. `release_integrity::oven_dependencies_use_one_pinned_git_revision_with_exact_publish_versions` and any other test that embeds the rev or the versions: update.

### 3b. Engine no longer depends on `oven-sdk-azure` / `oven-sdk-openai`

The only use is `crates/engine/src/runtime/compaction.rs:18-19, ~404-416`, where a native compaction request gets provider-specific `instructions` via the two extension traits, keyed on the adapter id strings `"oven.openai.responses"` and `"oven.azure.openai.responses"`.

Add to `crates/models/src/adapters/` a new module `compaction.rs`:

```rust
//! Provider-specific native compaction request options.

use oven_sdk::CompactionRequest; // use whatever the concrete request type in compaction.rs is

/// Attach provider-native compaction instructions for adapters that support them.
/// Adapters without a native option leave the request untouched.
pub fn with_native_compaction_instructions(
    request: CompactionRequest,
    adapter_id: &str,          // the same string the engine matches on today
    instructions: String,
) -> CompactionRequest
```

(If there is already an `OvenAdapterFamily` enum that these ids map to, take it instead of `&str`; the engine then passes the family it already has.) Re-export from `cookie_agent_models::adapters`. The engine's match block becomes one call. Remove `oven-sdk-azure` and `oven-sdk-openai` from `crates/engine/Cargo.toml`. Move any engine unit test that covered the two branches to the models crate, unchanged in substance.

### 3c. Remaining duplicate versions

After 3a, run `cargo tree --workspace -d -e normal --depth 1`. For each duplicate, attempt the cheapest unification and keep it only if it needs no code change beyond imports:
- `fancy-regex` 0.16 (syntect) vs 0.19 (jsonschema): after Item 6 moves syntect to crates.io, check whether a newer syntect 5.x on crates.io already uses fancy-regex 0.19 (`cargo search syntect --limit 1` / `cargo info syntect`); if so, use it (exact-pin as the workspace does).
- `base64` 0.23 (rmcp only), `sha2`/`digest`/`block-buffer`/`cpufeatures`/`crypto-common` 0.11-line (lopdf only), `getrandom` x3, `rand` x2, `hashbrown` x2, `thiserror` x2, `windows-sys` x2, `nix`, `nom`, `syn`, `bitflags`, `weezl`, `wasm-streams`, `bit-set`/`bit-vec`: these come from third-party crates; do not fork or patch to unify them. Report the final duplicate list in the summary as a before/after table.

## Item 4: drop the TypeScript bindings, keep JSON Schema for the additive checks

Decision: `schemars` derives stay everywhere (the `tools` crate embeds protocol types such as `ToolCallId` in its own JSON-schema tool argument structs, so the derives are a runtime requirement). `ts-rs` goes entirely.

- Remove `ts-rs` from `[workspace.dependencies]`, `crates/identity/Cargo.toml`, `crates/protocol/Cargo.toml`. Remove every `#[derive(... TS ...)]` entry and every `#[ts(...)]` attribute (≈634 derive sites across identity and protocol; keep the `JsonSchema` and `#[schemars(...)]` parts). Remove `use ts_rs::TS` imports.
- `crates/protocol/src/bindings.rs`: delete the TypeScript export path and `BindingExportError::TypeScript`; keep the JSON-schema export used by the additive checks. `crates/protocol/examples/generate.rs`: emits only JSON schemas.
- Delete from git: `crates/protocol/generated/` (both `json-schema/` and `typescript/` outputs), `crates/protocol/typescript/` (package.json, package-lock.json). Keep `event-payload-baseline.json` and `extension-protocol-baseline.json`; those are what the additive checks compare against.
- Replace `crates/protocol/scripts/check-bindings.sh` with `crates/protocol/scripts/check-schema-additive.sh`: `cargo run --locked -p cookie_agent_protocol --example generate -- --output <tmpdir>` then the existing `test-event-payload-additive.sh` / `test-extension-protocol-additive.sh` logic against the baselines. No npm, no node. Python 3 only if the existing comparison already uses it. The script no longer has a `--check` mode (nothing is checked in any more), so update callers.
- Update docs: `docs/site/reference/schemas.md` (no TypeScript, no checked-in generated tree), `docs/site/reference/protocol.md`, `docs/site/architecture.md` ("JSON Schema and TypeScript bindings" wording), `AGENTS.md` required gates (`check-bindings.sh --check` -> new script), and any `include-markdown` that pulled from `generated/`. `./scripts/build-docs.sh` must still pass in strict mode.
- `release_integrity` tests that reference the bindings script or npm: update.

## Item 5: CI diet

Rewrite `.github/workflows/ci.yml` (leave `docs.yml`, `nightly-prune.yml`, `release.yml` untouched). Keep every action pinned by full SHA as the repo does today; look up SHAs with `gh api repos/<owner>/<repo>/git/ref/tags/<tag>`.

```yaml
name: CI
on: [push, pull_request]
permissions: { contents: read }
jobs:
  stable:            # ubuntu-latest, toolchain stable, components rustfmt+clippy
    - Swatinem/rust-cache (pinned SHA, v2.x)
    - cargo fmt --all -- --check
    - cargo clippy --locked --workspace --all-targets -- -D warnings      # replaces build + check + clippy
    - cargo test --locked --workspace                                      # RUSTFLAGS=-D warnings
    - cargo doc --locked --workspace --no-deps                             # RUSTDOCFLAGS=-D warnings
    - crates/protocol/scripts/check-schema-additive.sh
    - cargo test --locked -p cookie_agent_models --test release_integrity
    - cargo build --release --locked -p cookie_agent                       # only the binary, only for the secret scan below
    - cargo test --locked -p cookie_agent_models --test release_integrity release_binary_contains_no_secret_material -- --ignored --exact
    - taiki-e/install-action (pinned SHA) with tool: cargo-deny@0.20.2,cargo-audit@0.22.2
    - cargo audit --file Cargo.lock --deny yanked
    - cargo deny --locked check advisories licenses sources
  msrv:              # ubuntu-latest, toolchain 1.88.0, rust-cache
    - cargo check --locked --workspace --all-targets                       # RUSTFLAGS=-D warnings
  windows:           # windows-latest x86_64 only, toolchain 1.88.0, rust-cache, timeout 90
    - cargo test --locked --workspace -- --nocapture                       # RUST_BACKTRACE=1
  release-targets:   # same 7-target matrix as today, but
    if: github.event_name == 'pull_request' || github.ref == 'refs/heads/main' || startsWith(github.ref, 'refs/tags/v')
    - cargo check --locked --target ${{ matrix.target }} -p cookie_agent
  release:           # unchanged: needs [stable, msrv, windows, release-targets], same if:, uses release.yml
```

Drop: the duplicate `cargo build --all-targets` and `cargo check --all-targets` steps, the release build of the whole workspace with all targets, the MSRV job's full test/doc/release repetition, the `windows-11-arm` test runner (it stays in `release-targets`), the `cargo install` of audit/deny.

Update `release_integrity::ci_supply_chain_and_release_gates_are_pinned` (and neighbours) to assert the new shape: every `uses:` pinned by 40-hex SHA, cargo-deny and cargo-audit versions still pinned, release job still gated on all four jobs.

## Item 6: un-vendor syntect

- Workspace `Cargo.toml`: remove `exclude = ["vendor/syntect"]` and the `[patch.crates-io] syntect = { path = "vendor/syntect" }` block. `syntect` stays exact-pinned from crates.io: `syntect = { version = "=5.3.0", default-features = false, features = ["default-syntaxes", "default-themes", "regex-fancy"] }` (or the newer 5.x chosen in 3c).
- `git rm -r vendor/`. Remove the `vendor/** -text -whitespace` line from `.gitattributes`.
- `deny.toml`: keep `ignore = ["RUSTSEC-2025-0141"]` (bincode 1.x unmaintained; syntect 5.x still needs it) but rewrite the comment so it no longer refers to vendoring.
- `crates/models/tests/release_integrity.rs`: delete `syntect_patch_is_exactly_pinned_and_declared` and `vendored_syntect_matches_declared_upstream_delta`; rename/adjust `workspace_metadata_limits_publishing_and_uses_vendored_syntect` to assert syntect is exact-pinned from crates.io with `default-features = false`; drop `vendor/syntect/Cargo.toml` from `every_internal_path_dependency_has_its_exact_package_version`; drop the `--manifest -path vendor/` expectation in `ci_supply_chain_and_release_gates_are_pinned`.
- `crates/tui/src/markdown.rs` tests around lines 2468-2488 `include_bytes!` the dump assets from `../../../vendor/syntect/assets/`. Rewrite those tests to load through syntect's public API (`SyntaxSet::load_defaults_newlines()`, `SyntaxSet::load_defaults_nonewlines()`, `ThemeSet::load_defaults()`) and keep whatever they assert about the TUI's own rendering; delete assertions that only existed to prove the vendored dump bytes matched upstream.
- Confirm `cargo deny --locked check sources` passes (crates.io source, no path patch), `cargo build --locked` resolves syntect from the registry, and the TUI highlighting tests pass.

## Commits (suggested order)
1. oven-sdk repo: `chore(deps)!: move to reqwest 0.13` (+ version bumps), pushed first.
2. `chore(deps)!: unify on reqwest 0.13 via oven-sdk <short-sha>`
3. `refactor(models): own provider-native compaction options`
4. `refactor(protocol)!: drop TypeScript bindings and the generated tree`
5. `build: replace the vendored syntect with the crates.io release`
6. `ci: single stable job with caching, check-only MSRV, x86_64 Windows`
7. `chore(deps): unify remaining duplicate versions` (only if 3c yields anything)

## Local gate
Same as phase 1 plus `crates/protocol/scripts/check-schema-additive.sh` and `./scripts/build-docs.sh` (needs `pip install -r requirements-docs.txt` in a venv if mkdocs is missing; if it cannot be installed locally, say so and rely on the `Documentation` workflow).

## Milestone
Push the branch, wait for the branch `CI` run to be green (`gh run list --branch <branch>`; `gh run watch <id> --exit-status --interval 60`, repeat on tool timeout), fix and re-push on failure. When green: `git checkout main && git merge --ff-only <branch> && git push origin main`; wait for `CI` and `Documentation` on `main` to be green. Report: oven-sdk SHA, main SHA, CI URLs, the `cargo tree -d` before/after table, package count before/after (`grep -c '^\[\[package\]\]' Cargo.lock`), and the wall time of the new CI run versus the previous ~52 min.

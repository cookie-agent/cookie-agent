# Contributor Guide

## Commits

Keep commits focused on one logical change. Use the repository's conventional
subject style: `type(scope): imperative summary`, for example
`refactor(engine): remove obsolete decoder`. Omit the scope for repository-wide
changes when no single component owns the work.

## Compatibility and Configuration

Configuration is strict. Unknown keys, removed `schema` or `schema_version`
fields, wrong types, and malformed values are hard errors. Do not silently
ignore fields, migrate authored files, or upconvert old shapes.
Every configuration-surface change must update the configuration reference and
cover defaults, strict unknown-field handling, and boundary semantics in tests.

The project does not preserve backward compatibility unless a governing
specification explicitly requires it. Prefer removing obsolete aliases,
migrations, and compatibility decoders over carrying them forward. Session
event history is the deliberate exception: it is versionless and read
best-effort according to its documented contract.

## Required Gates

Use locked dependency resolution for builds and tests:

```sh
cargo build --locked --workspace --all-targets
cargo test --locked --workspace
```

Run formatting and Clippy with warnings denied under stable and the Rust 1.88
MSRV:

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

`./scripts/build-docs.sh` builds workspace rustdoc and runs MkDocs in strict
mode. `check-schema-additive.sh` compares the generated protocol schemas with
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

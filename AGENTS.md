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

Check generated protocol bindings, documentation, and dependency policy:

```sh
crates/protocol/scripts/check-bindings.sh --check
./scripts/build-docs.sh
cargo deny --locked check advisories licenses sources
```

`./scripts/build-docs.sh` builds workspace rustdoc and runs MkDocs in strict
mode.

# AGENTS.md

## Architecture doc is the source of truth

`ARCHITECTURE.md` is the authoritative design record for this project.

**Rule:** if you change anything that alters the architecture — components,
crate boundaries, protocol surface, data models, event types, delegation or
permission semantics, transports, configuration schema, persistence format —
you **must** update `ARCHITECTURE.md` in the same commit so the doc never
drifts from the implementation.

Minor implementation details that do not drift from the documented plan
(helper functions, internal refactors within a crate, performance tweaks,
bug fixes that restore documented behavior) do **not** require a doc update.

When in doubt whether a change is architectural: it is. Update the doc.

## Zero warnings

All warnings are addressed, not ignored: `cargo build`, `cargo clippy`, and
`cargo test` across the workspace must produce **zero** warnings (rustc,
clippy, and macro-expansion warnings alike). Do not silence warnings with
`#[allow]` unless the warning is a proven false positive and the allowance
is narrowly scoped with a comment explaining why. If a dependency upgrade or
new code introduces a warning, fix it in the same change.

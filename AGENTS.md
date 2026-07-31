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

# Schema Compatibility

Writers emit the current version. Readers accept the ranges below; versions
outside those ranges and unversioned formats are rejected.

| Surface | Current write | Accepted reads |
|---|---:|---:|
| Runtime configuration | 10 | 10 |
| Agent document | 5 | 5 |
| Protocol | 9 | 9 |
| Events and session JSONL | 17 | 15-17 |
| Session metadata | 9 | 9 |
| Delegation journal | 14 | 11-14[^journal-12] |
| Runtime snapshot | 4 | 4 |
| Catalog cache | 2 | 2 |
| Provider store | 3 | 3 |
| Family recipe registry | 1 | 1 |
| Project model-snapshot manifest | 1 | 1 |

Agent snapshots embedded in accepted event and delegation-journal versions also
read the exact schema-4 snapshot shape written before agent document schema 5.
The legacy `tools` list is discarded during upconversion; subsequent writes use
schema 5 and permission-driven visibility. Unknown snapshot fields remain
rejected.

[^journal-12]: Unambiguous schema-12 records reopen. The unshipped schema-12
    resume/start encoding is rejected because it cannot distinguish a newly
    started run from attachment to an existing run. Move the affected project's
    `delegations.jsonl` aside and restart. This discards delegation recovery
    state AND historical child resumability: session event logs remain intact,
    but without the journal those child sessions no longer satisfy the
    journal-backed ownership checks required for `resume_session_id`.

The protocol crate exports JSON Schema and TypeScript binding sets for the wire
roots. The [Rust API documentation](api.md) describes the public Rust types;
the [protocol reference](protocol.md) summarizes the active method surface.

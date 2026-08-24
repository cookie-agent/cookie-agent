# Schema Compatibility

Runtime configuration and agent documents are unversioned authored files parsed
against the current shape. They reject `schema` and `schema_version` fields as
hard errors rather than migrating or ignoring them. Session event history is
also versionless and read best-effort. Versioned persisted and wire surfaces
emit only their current version and reject unsupported versions.

| Surface | Current write | Accepted reads |
|---|---:|---:|
| Runtime configuration | unversioned | unversioned current shape |
| Agent document | unversioned | unversioned current shape |
| Protocol | 11 | 11 |
| Events and session JSONL | versionless | versionless plus legacy schema markers |
| Session metadata | unversioned | unversioned current shape |
| Runtime snapshot | 5 | 5 |
| Catalog cache | 2 | 2 |
| Provider store | 3 | 3 |
| Family recipe registry | 1 | 1 |
| Project model-snapshot manifest | 1 | 1 |

Agent snapshots embedded in readable events upconvert supported historical
shapes before validation. Delegation lifecycle records are ordinary parent
session events; the removed `delegations.jsonl` surface and its schema versions
are not read.

The protocol crate exports JSON Schema and TypeScript binding sets for the wire
roots. The [Rust API documentation](api.md) describes the public Rust types;
the [protocol reference](protocol.md) summarizes the active method surface.

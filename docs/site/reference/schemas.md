# Current-only Schemas

Only these versions are accepted. Earlier and unversioned formats are rejected;
there are no migrations, aliases, compatibility readers, or dual paths.

| Surface | Version |
|---|---:|
| Runtime configuration | 10 |
| Agent document | 4 |
| Protocol | 9 |
| Events and session JSONL | 14 |
| Session metadata | 9 |
| Delegation journal | 10 |
| Runtime snapshot | 3 |
| Catalog cache | 2 |
| Provider store | 3 |
| Family recipe registry | 1 |
| Project model-snapshot manifest | 1 |

The protocol crate exports JSON Schema and TypeScript binding sets for the wire
roots. The [Rust API documentation](api.md) describes the public Rust types;
the [protocol reference](protocol.md) summarizes the active method surface.

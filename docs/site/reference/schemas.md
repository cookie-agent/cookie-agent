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
| Client protocol | 49 | 49 |
| Plugin protocol | 0.3.0 | 0.3.x |
| Runtime protocol | 0.7.0 | 0.7.x |
| Events and session JSONL | versionless | versionless |
| Session metadata | unversioned | unversioned current shape |
| Runtime snapshot | 6 | 6 |
| Catalog cache | 2 | 2 |
| Provider store | 3 | 3 |
| Family recipe registry | 1 | 1 |
| In-memory model manifest (recorded run bindings) | 1 | 1 |

The three protocols are moving to semver versions, as the
[compatibility specification](../specs/compatibility-v1.md) describes. The
plugin and runtime handshakes send `PLUGIN_PROTOCOL_VERSION` and
`RUNTIME_PROTOCOL_VERSION` and accept a peer of the same major and, before
`1.0.0`, the same minor. Until the client handshake switches, its exact
number above stays on the wire; the semver constant `CLIENT_PROTOCOL_VERSION`
(`0.49.0`) is what its baseline records. The versions stay `0.x` until the
protocols freeze at `1.0.0`.

Agent snapshots embedded in events are read in their current shape only
(`schema` 7); earlier shapes are rejected. Delegation lifecycle records are
ordinary parent session events; the removed `delegations.jsonl` surface and its
schema versions are not read.

## Baselines and the checker

`crates/protocol/compat/` holds one baseline per surface: `client`, `plugin`,
`runtime`, and `events` (the `EventPayload` set, which the client and plugin
protocols both deliver; its `version` records the client version). Each holds:

- `version`: the surface's version when the baseline was taken;
- `methods.json`: every method and notification with its direction (`to_daemon`
  or `from_daemon`), its kind (`request` or `notification`), the names of its
  params and result schema roots, and `params_role` (`command` or
  `information`), plus `information`: roots that flow as information without a
  method of their own, such as error data;
- `schemas/<Root>.schema.json`: the JSON Schema of every root.

`cargo run -p cookie_agent_protocol --example generate -- --output <directory>`
writes the same layout from the current code; the method registry it reads is
`surface_methods` in `crates/protocol/src/bindings.rs`.

`crates/protocol/scripts/check-schema-additive.sh` generates the set into a
temporary directory and compares it with the baselines using
`crates/protocol/scripts/check-compat.py`, then runs the checker's mutation
test, `test-check-compat.py`. The checker gives each type a role from
`methods.json`: params sent to the daemon are commands, except a peer's
declarations of what it has (`runtime/tools_changed`,
`capability/tools_changed`, `mcp/changed`), while params the daemon sends and
every result are information. It walks nested definitions and
computes the version bump each change requires under the rules of section 7.1
of the specification; open enums are marked `"x-open": true`.

- **Before the freeze** (a `0.x` version), the baselines must equal the
  generated set exactly, and rule findings are reported without failing.
  After any change to a wire type, method, or version constant, run
  `crates/protocol/scripts/update-compat-baseline.sh` and commit the result.
- **From `1.0.0`**, every rule is enforced: the version constant must carry at
  least the required bump against the baseline's `version`, an information
  type with `additionalProperties: false` fails outright, and new event tags
  are recorded only through the update script, which refuses an insufficient
  bump.

The [Rust API documentation](api.md) describes the public Rust types; the
[protocol reference](protocol.md) summarizes the active method surface.

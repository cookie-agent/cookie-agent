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
| Client protocol | 0.51.0 | 0.51.x |
| Plugin protocol | 0.5.0 | 0.5.x |
| Runtime protocol | 0.8.0 | 0.8.x |
| Session history (events JSONL) | history format 1 | format 1; older formats are converted |
| Session metadata | unversioned | unversioned current shape |
| Catalog cache | 2 | 2 |
| Provider store | 3 | 3 |
| Family recipe registry | 1 | 1 |
| In-memory model manifest (recorded run bindings) | 1 | 1 |

The three protocols have semver versions, as the
[compatibility specification](../specs/compatibility-v1.md) describes: the
client protocol `CLIENT_PROTOCOL_VERSION` (`0.51.0`), the plugin protocol
`PLUGIN_PROTOCOL_VERSION` (`0.5.0`), and the runtime protocol
`RUNTIME_PROTOCOL_VERSION` (`0.8.0`), all in `cookie_agent_protocol`. Each
handshake sends its side's version and accepts a peer of the same major and,
before `1.0.0`, the same minor; the patch never matters. The versions stay
`0.x` until the protocols freeze at `1.0.0`. The client protocol's versions
are listed in its [changelog](protocol-changelog.md).

Session history is versioned apart from the protocols, by the
`history_format` that `session_created` records (format 1 in this build). It
bumps only when a change cannot follow the additive rules of
[Events](events.md#history-formats); a log of a lower format is converted when
a newer build opens it, and one of a higher format opens only to be viewed.
The catalog view that `runtime.snapshot.get` returns carries no schema
version, only one opaque `revision`.

Agent snapshots embedded in events are read in their current shape only
(`schema` 7); earlier shapes are rejected. Delegation lifecycle records are
ordinary parent session events; the removed `delegations.jsonl` surface and its
schema versions are not read.

## Published schemas

The three protocols' schemas are published with this documentation, exactly
as `crates/protocol/compat/` commits them (see below):

| Surface | Methods | Schemas | Version |
|---|---|---|---|
| Client protocol | [`methods.json`](../schemas/client/methods.json) | `schemas/client/schemas/<Root>.schema.json`, such as [`RuntimeSnapshotResult`](../schemas/client/schemas/RuntimeSnapshotResult.schema.json) | [`version`](../schemas/client/version) |
| Plugin protocol | [`methods.json`](../schemas/plugin/methods.json) | `schemas/plugin/schemas/<Root>.schema.json`, such as [`PrepareResult`](../schemas/plugin/schemas/PrepareResult.schema.json) | [`version`](../schemas/plugin/version) |
| Runtime protocol | [`methods.json`](../schemas/runtime/methods.json) | `schemas/runtime/schemas/<Root>.schema.json`, such as [`ExecuteResult`](../schemas/runtime/schemas/ExecuteResult.schema.json) | [`version`](../schemas/runtime/version) |

`methods.json` names each method's params and result roots; each root's file
holds its schema and its definitions (`$defs`). A plugin or runtime written
in any language can validate what it sends against them. The
[reference plugin](../development/plugins.md#reference-plugin) does so in CI.
The client protocol's error data is not described by schemas: the
[error reference](errors.md) lists every code and its fields.

## Baselines and the checker

`crates/protocol/compat/` holds one baseline per surface: `client`, `plugin`,
`runtime`, and `events` (the `EventPayload` set, which the client and plugin
protocols both deliver; its `version` records the client version). Each holds:

- `version`: the surface's version when the baseline was taken;
- `methods.json`: every method and notification with its direction (`to_daemon`
  or `from_daemon`), its kind (`request` or `notification`), the names of its
  params and result schema roots, and `params_role` (`command` or
  `information`), plus `information`: roots that flow as information without a
  method of their own, such as the stored event set;
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

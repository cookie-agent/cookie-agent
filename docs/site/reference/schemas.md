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
| Client protocol | 1.39.0 | 1.x |
| Plugin protocol | 1.21.0 | 1.x |
| Runtime protocol | 1.8.0 | 1.x |
| Session history (events JSONL) | history format 1 | format 1; older formats are converted |
| Session metadata | unversioned | unversioned current shape |
| Machine registry (`machines.json`) | 2 | 1 (read with no runtime records), 2 |
| Catalog cache | 2 | 2 |
| Provider store | 3 | 3 |
| Family recipe registry | 1 | 1 |
| In-memory model manifest (recorded run bindings) | 1 | 1 |

The three protocols have semver versions, as the
[compatibility specification](../specs/compatibility-v1.md) describes: the
client protocol `CLIENT_PROTOCOL_VERSION` (`1.39.0`), the plugin protocol
`PLUGIN_PROTOCOL_VERSION` (`1.21.0`), and the runtime protocol
`RUNTIME_PROTOCOL_VERSION` (`1.10.0`), all in `cookie_agent_protocol`. All
three froze at `1.0.0`. Each handshake sends its side's version and accepts a
peer of the same major; the two use what the lower minor includes, and the
patch never matters. No `0.x` build from before the freeze connects to a 1.x
one. The client protocol's versions are listed in its
[changelog](protocol-changelog.md).

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

Every rule is enforced: the version constant must carry at least the
required bump against the baseline's `version`, an information type with
`additionalProperties: false` fails outright, and new event tags are recorded
only through the update script. After a change to a wire type, a method, or a
version constant, run `crates/protocol/scripts/update-compat-baseline.sh`,
which refuses an insufficient bump, and commit the result. The update
replaces only the surface directories; the frozen corpora in
`crates/protocol/compat/corpus/` are never regenerated.

## Corpora

The baselines show what a schema allows; the corpora in
`crates/protocol/compat/corpus/` show what peers actually sent, and the tests
replay them against the current code:

- `<surface>/<version>/*.jsonl`, for `client`, `plugin`, and `runtime`:
  conversations recorded at that version, one JSON-RPC message per line as
  `{"from": <side>, "message": <message>}`, in the order they crossed the
  connection. `client/1.0.0/session.jsonl` is a TUI-like session against a
  daemon with a scripted model: handshake, catalog, session, subscription,
  an image upload, a run whose `bash` call waits for approval while two
  steers arrive, the approval, and reads of the session and the artifact.
  `runtime/1.0.0/coding.jsonl` is the daemon and its local coding runtime
  during that session. `plugin/1.0.0/reference.jsonl` is the scripted
  conversation of `check_reference_plugin.py` with the reference plugin.
- `future/*.json`: what a newer peer may send each reader: unknown members,
  enum values, event tags, display block kinds, methods, and notifications.

The replays prove that a new build accepts old data. Recorded client
requests are answered by a fresh daemon as they were (a session, run,
upload, or approval ID the replay's own replies hand out replaces the
recorded one); recorded replies and notifications read with the current
client without a diagnostic or an unknown event; the daemon's plugin and
runtime messages drive a plugin built with the Rust SDK and the coding
runtime; and what plugins and runtimes sent reads as the daemon reads it.
Each test also reads the recorded information with an unknown member added
to every object, which must change nothing, and the `future/` fixtures of
its reader.

A recorded version directory is frozen: the tests check it against a digest
pinned in `cookie_agent_protocol::corpus::FROZEN` and fail when it changed,
and no `COOKIE_UPDATE_*` switch applies to it. A later minor records its own
`<surface>/<version>/` directory beside the earlier ones and pins it. The
recorders write the current versions and refuse a directory that exists:

```sh
COOKIE_RECORD_CORPUS=crates/protocol/compat/corpus \
  cargo test -p cookie_agent_server -- --ignored record_the_client_and_runtime_corpora
python3 examples/plugins/python/check_reference_plugin.py --record crates/protocol/compat/corpus
```

The module documentation of `cookie_agent_protocol::corpus` (behind the
crate's `test-support` feature) lists the tests of every crate.

The [Rust API documentation](api.md) describes the public Rust types; the
[protocol reference](protocol.md) summarizes the active method surface.

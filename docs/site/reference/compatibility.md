# Compatibility

The client protocol, the plugin protocol, the runtime protocol, and session
history carry a compatibility promise: a client, plugin, or runtime written
against 1.x keeps working with every daemon that speaks 1.x, and a session
log keeps opening in every later build. These are the rules every change to
them follows. The [compatibility specification](../specs/compatibility-v1.md)
gives the reasons; this page states the rules.

## Versions

- Each protocol has a semver version `MAJOR.MINOR.PATCH` in
  `crates/protocol/src/semver.rs`: `CLIENT_PROTOCOL_VERSION`,
  `PLUGIN_PROTOCOL_VERSION`, and `RUNTIME_PROTOCOL_VERSION`. All three froze
  at `1.0.0`.
- **Bump the major** for a breaking change: something removed or renamed, a
  meaning changed, a new required member in a message the peer receives, a
  closed enum grown. Peers of different majors never connect, and a daemon
  does not keep serving the previous major.
- **Bump the minor** for any addition: a method, a notification, a hook, an
  event type, a display-block kind, an optional member, a value of an open
  enum.
- **Bump the patch** for a change that leaves every wire shape alone: a
  documented behaviour clarified, a validation bug fixed.
- **Know which surfaces a message belongs to.** Session events belong to the
  client and the plugin protocols, so adding an event type bumps both
  minors. The `tool/*` messages belong to the plugin and the runtime
  protocols. Display blocks belong to all three.
- **Update the human-readable references in the same commit** as a bump:
  `AGENTS.md` lists them.

## Handshakes

- Read the peer's `protocol_version` from the raw JSON before decoding
  anything else (`negotiate_version`), and answer another major with
  `unsupported_protocol` naming both versions.
- Two peers of one major share the lower of their minors. **Send a command**
  (a method, a member of a command's params, a hook) **only when the
  receiver's minor includes it.** Information an older receiver ignores may
  stay in place; a value an older receiver would misread may not.
- Features carry only what depends on configuration, never what a version
  implies. A receiver ignores a feature it does not know.

## Decoding

| What arrives | Unknown member | Unknown enum value or tag |
|---|---|---|
| Information (results, notifications, events, hook params, initialize results, stored events) | Ignored | The documented fallback |
| Command (client params, `producer/*`, `tool/progress`, `tool/detachable`, `plugin/emit`) | Refused: `-32602` `unknown_field` with its JSON path | Refused: `-32602` `unknown_variant` with its JSON path |

- **Never put `#[serde(deny_unknown_fields)]` on a type that crosses a surface
  as information.** The checker rejects an information type whose schema has
  `additionalProperties: false`. Command params stay strict, and so does
  configuration.
- **Writers stay strict.** Validate every value before it is sent or
  written: bounds, required members, grammar. Strictness lives in
  constructors and in the writer's validation step, not in the decoder.
- **Make every enum a reader may meet open**, and document what its unknown
  value means where it is read: `Open<T>` (`open_enum!`) for tagged enums
  with data, an `Unknown` variant or a `#[serde(other)]` fallback for unit
  enums, validated label strings for label-like values. Mark open enums
  `x-open: true` in the schema. Close an enum only when growing it is a
  breaking change by design, and say so.
- **A known tag with a malformed body is an error**, not `Unknown`: `Unknown`
  means "from a newer peer".
- Do not add `#[non_exhaustive]` to protocol types; only the plugin SDK's own
  types use it.

## Wire types

- **No surface embeds a type owned by oven-sdk or by engine policy**, stored
  events included. Where the engine needs data it does not model, store an
  opaque value and decode it best-effort when it is used, never when loading.
- **Every error is a registry error** (`cookie_agent_protocol::errors`): a
  stable `data.code`, a `message` a user understands, an optional `hint`, and
  `detail` for technical detail. Add a code to the registry rather than
  building error data by hand; clients branch only on `data.code`.

## Making a wire change

1. Change the type or the method registry (`surface_methods` in
   `crates/protocol/src/bindings.rs`).
2. Bump each affected surface's version by what the change needs (above).
3. Run `crates/protocol/scripts/update-compat-baseline.sh`. It regenerates
   `crates/protocol/compat/<surface>/` and refuses when a version lacks the
   bump its changes require.
4. Run `crates/protocol/scripts/check-schema-additive.sh`, which CI runs: it
   compares the generated schemas with the baselines under the rules of the
   specification's section 7.1 and runs the checker's mutation test.
5. Commit the baselines with the change and the reference updates.

A change the checker calls breaking needs a major bump; there is no way to
mark a breaking change as compatible.

## Test sets

- **Frozen corpora are never regenerated.** `crates/protocol/compat/corpus/`
  holds, per surface and version, recorded conversations that the current
  build must keep accepting, and future fixtures that inject what a newer
  build could send. `crates/engine/tests/fixtures/history/<format>/` holds
  recorded session trees with snapshots of their projection and model
  history. No `COOKIE_UPDATE_*` switch applies to them, and the baseline
  update leaves them alone.
- **When a test over frozen data fails, fix the code, not the data.** A
  failure means the change breaks data an earlier build wrote or sent.
- **A minor release that adds wire shapes records its own corpus directory**
  next to the earlier ones. A history format bump records
  `history/<format>/` with its own recorder run, beside the older formats,
  which keep testing their converters.

## Session history

- **Class every event** as state (anything the projection, model history,
  delegation recovery, goals, producers, approvals, or usage read) or audit
  (everything else), and list its class in [Events](events.md). Audit events
  carry `"skippable": true`; a reader skips an unknown skippable event and
  opens a log with an unknown state event render-only.
- **Additive rules for writers:** a new optional member must be safe to
  ignore, its absence meaning the old behaviour; a member whose absence would
  be misread needs a new event type; a new open-enum value needs a fallback
  that is safe for old readers.
- **The loader never recomputes what it can read.** Trust stored
  fingerprints, quarantine a record that fails a cross-event check instead of
  failing the open, keep size caps writer-only, and keep every record's raw
  line so that a fork, a publish, or a conversion copies what this build did
  not understand.
- **Storage encodings stay on disk.** An encoding only the log holds, such
  as a tool result's display ranges of its output, is expanded when the log
  is read; nothing outside the engine meets it.
- **The history format bumps only for a change that cannot follow the
  additive rules**, and every bump ships a converter:
  `crates/engine_store/src/events/convert/v<N>_to_v<N+1>.rs`, a pure function over
  the old log's JSON lines that never uses old Rust types, registered in
  `CONVERTERS` and kept forever, so converters chain. Converters are the only
  migrations allowed; nothing else upconverts, aliases, or migrates stored
  or authored data. See [History formats](events.md#history-formats).

## Configuration

Configuration is not a compatibility surface. It stays strict: unknown keys,
removed fields, wrong types, and malformed values are hard errors, and authored
files are never migrated or upconverted. A configuration key change is a
configuration change, not a protocol change, even where `config.get` and
`config.apply` carry the document.

# Client Protocol Changelog

The client protocol's version is `MAJOR.MINOR.PATCH`
(`cookie_agent_protocol::CLIENT_PROTOCOL_VERSION`). Before the 1.0 freeze
every minor version may break the wire; from 1.0 a minor only adds and a
major breaks (see the [compatibility spec](../specs/compatibility-v1.md)
section 3).

## 0.52.0

- Every model of the catalog view states `input_media`: the kinds it accepts
  in user input, with their media types and effective byte, dimension, pixel,
  and count limits.

## 0.51.0

- The handshake's `protocol_version` is a semver string. A client of another
  major or, before 1.0, another minor is refused with `-32002`
  `unsupported_protocol`, as is a build that sends the old integer.
- The handshake takes an optional `token` and `features`, and its reply
  carries `engine_version` and `features`. The WebSocket upgrade no longer
  requires `Authorization`: a connection without it authenticates with the
  handshake's `token`, and gets `-32003` for anything else until it does.
- `[server] allowed_origins` lets web pages of listed origins connect.
- Results, notifications, and the JSON-RPC envelope are information: a
  reader ignores members it does not know, and every enum in them is open,
  read as a documented fallback or as `unknown` (see
  [Decoding](protocol.md#decoding)). Params stay strict.
- Every application error is `-32000` with a registry `data.code`, a
  `message` and `hint` from the registry, and the cause as `data.detail` (see
  [Errors](errors.md)). The old codes `-32011`, `-32012`, `-32020` to
  `-32022`, `-32030`, `-32031`, and `-32040`, the several meanings of
  `-32602` for refusals, the typed error data (`RunStartConflict`,
  `SessionRenameError`, `SessionRevertConflict`, `ApprovalRespondError`,
  `ProviderConnectError`, `ProviderDisconnectError`, `ConfigApplyError`), and
  `debug_code` are gone.

- `runtime.snapshot.get` and `runtime.changed` carry a `ModelCatalogView`
  (`revision`, `catalog`, `providers`, `models`, `agents`) in place of the
  runtime snapshot: one opaque revision replaces the snapshot schema version
  and the eleven revisions and counters, and descriptors lose their
  fingerprints. `provider.connect` and `provider.disconnect` take that
  revision as `expected_revision`; the disconnect receipt keeps its ID and
  time only. `runtime.changed` takes any number of reasons.
- `SessionMeta.short_id` is required (nullable).
- The daemon sends a connection only the display-block kinds its version
  includes, downgrading newer kinds to `text` and leaving out kinds it does not
  know.

## Before semver versions

Builds before 0.51.0 sent the protocol version as an integer, from 34 to 50;
the last of them was 50. They cannot connect to this build.

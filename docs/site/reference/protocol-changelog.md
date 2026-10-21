# Client Protocol Changelog

The client protocol's version is `MAJOR.MINOR.PATCH`
(`cookie_agent_protocol::CLIENT_PROTOCOL_VERSION`). From 1.0.0 a minor only
adds and a major breaks; before it, every minor could break the wire (see the
[compatibility spec](../specs/compatibility-v1.md) section 3).

## 1.7.0

- Paths and runtimes: `SessionMeta` gains `attachments`, the session's
  runtime attachments now (`RuntimeAttachment`: `name`, `runtime_id`,
  `capability`, `kind`, `workspace`), kept current on attach and detach,
  which also announce the session with `sessions.changed` `upserted`; session
  lists can show and filter sessions by runtime and workspace without a
  `session.get` each. `RuntimeListing` in `runtimes.list` gains `home`, the
  home directory the runtime reported (runtime protocol 1.3.0), which clients
  show the runtime's paths under as `~/…` (see [Runtimes](protocol.md#runtimes)).
  A 1.6 client ignores both; a 1.7 client reading a 1.6 daemon sees no
  attachments and no homes, and shows paths absolute.
- A new root session's `cwd_identity` is decided at creation, once its
  attachments have opened: the canonical workspace its first attachment with
  a workspace reported, else `cwd` canonicalized on the daemon's machine (as
  given when that fails). A client's `~/x` and another's `/home/ada/x`
  attaching one workspace now give one identity. Sessions created before keep
  the identity they stored.

## 1.6.0

- `memory.write` results carry an optional `notice`: what the daemon kept in
  place of the text it was given. The daemon now keeps a memory file for
  every agent and owns its description and two keywords, so a write of an
  `agent://` file that changes them keeps the daemon's and says so (see
  [Memory](protocol.md#memory)). A 1.5 client ignores the member.

## 1.5.0

- Session deletion: `session.delete { session_id, expected_last_seq }`
  deletes a root session's whole tree from disk and returns the `deleted`
  session IDs, the root first; every connection gets `sessions.changed`
  `removed` for each (see [Delete](protocol.md#delete)). It comes with the
  error codes `not_a_root_session`, `session_tree_running`, and
  `session_changed`, besides the existing `session_not_found` and
  `session_owned_elsewhere`. A 1.4 client sees only the `removed` changes; a
  1.5 client calling `session.delete` on a 1.4 daemon gets
  `method_not_found`.

## 1.4.0

- Memory for clients: `memory.list`, `memory.read`, `memory.write`, and
  `memory.delete` list, read, and change the daemon's memory files for the
  user, with no session (see [Memory](protocol.md#memory)). They come with
  the error codes `memory_url_invalid`, `memory_not_found`, `memory_exists`,
  `memory_revision_mismatch`, `memory_invalid`, `memory_refused`,
  `memory_busy`, and `memory_store_failed`. The `memory.changed`
  notification reaches every connection when a memory file changes through
  the daemon. A 1.3 client sees none of it; a 1.4 client calling a
  `memory.*` method on a 1.3 daemon gets `method_not_found`.

## 1.3.0

- A new state event, `memory_loaded`, records the memory snapshot a
  session's model sees from a run on: user categories, the run's agent's
  memory, and an index of the session's machines' memories (see
  [Events](events.md#memory-snapshots)). An event without `agent` records
  that memory is no longer available. A 1.2 client reads it as an unknown
  state event. The plugin protocol is `1.2.0` for the same event.
- `memory` and `memory_write` join the built-in permission actions that
  `session.permissions.get` lists.

## 1.2.0

- Machines: the computers runtimes run on. `machines.list`,
  `machines.rename`, `machines.assign`, `machines.unassign`,
  `machines.merge`, and `machines.forget` read and change the daemon's
  machine registry (see [Machines](protocol.md#machines)), with the error
  codes `machine_not_found`, `machine_name_taken`, `machine_in_use`,
  `machine_memory_conflict`, `machine_merge_self`, `runtime_not_pinned`, and
  `machine_store_failed`. The `machines.changed` notification reaches every
  connection when the registry changes, and each `runtimes.list` entry
  gains `machine`. A 1.1 client sees none of it; a 1.2 client calling a
  `machines.*` method on a 1.1 daemon gets `method_not_found`.

## 1.1.0

- A new audit event, `record_omitted { omitted_type }`, stands in for a record
  its writer deliberately did not carry over, at that record's sequence, run,
  and time. A fork writes one for each of its source's `delegation_reserved`,
  `delegation_progressed`, and `delegation_finished` records: the fork keeps
  the conversation but not the subagents, which stay the source's (see
  [Events](events.md#delegation)). It is skippable and changes nothing a
  reader projects; a 1.0 client reads it as an unknown event. The plugin
  protocol is `1.1.0` for the same event.

## 1.0.0

The client protocol is frozen: every later 1.x version reads and is read by
this one. A `0.x` build never interoperates with a 1.x one: the handshake
refuses another major with `-32002` `unsupported_protocol`, so a client and a
daemon from before the freeze must be upgraded together. The plugin and
runtime protocols froze at `1.0.0` too, and session history is history
format 1.

What 1.x keeps:

- **Versions.** The handshake carries `"MAJOR.MINOR.PATCH"` strings and
  connects equal majors; each side sends a method, a member, or a display
  block kind only when the other's minor includes it, and the patch never
  matters.
- **Information is tolerant, commands are strict.** Results, notifications,
  events, and the JSON-RPC envelope ignore members a reader does not know,
  and their enums are open, read as a documented fallback or as `unknown`;
  an event a client cannot read arrives as an unknown event, and a
  notification that does not decode fails no call. Params refuse an unknown
  member (`unknown_field`) or value (`unknown_variant`) with its JSON path.
- **Errors.** Every application error is `-32000` with a registry `data.code`,
  a readable `message`, an optional `hint`, and `detail` on request (see
  [Errors](errors.md)); clients branch only on `data.code`.
- **User content and media.** Inputs are `content`: text and file parts
  naming artifacts uploaded with `artifact.upload.*`, admitted against the
  `input_media` each model of the catalog view advertises, and read back with
  `artifact.read`.
- **Tool display.** Tool calls carry presentation v2 (title, hint, tool
  identity, argument and preview blocks), progress display deltas, and result
  display blocks of twelve kinds with a canonical text rendering; a connection
  receives only the kinds its version includes. A result always carries its
  whole `display`.
- **The catalog view.** `runtime.snapshot.get` and `runtime.changed` carry a
  `ModelCatalogView` with one opaque revision.

Changed since 0.52.0: a tool result's `display` is always whole and
required; `display_span` belonged to the session log's storage encoding and
no longer reaches clients.

## 0.52.0

- Every model of the catalog view states `input_media`: the kinds it accepts
  in user input, with their media types and effective byte, dimension, pixel,
  and count limits.
- `run.start`, `run.steer`, and `session.submit_input` take `content` (text
  and file parts) in place of `input`, and a `recalled` outcome of
  `session.recall_input` returns `content`.
- `artifact.upload.begin`, `artifact.upload.chunk`, `artifact.upload.finish`,
  `artifact.upload.cancel`, and `artifact.read` upload media into a session
  tree's store and read any of its artifacts back; new error codes
  `upload_offset_mismatch`, `upload_size_mismatch`, and `too_many_uploads`.
- Input file parts are admitted against the model the input reaches:
  `media_unsupported`, `media_type_unsupported`, `media_too_large`,
  `media_limit_exceeded`, and `artifact_not_found`.

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

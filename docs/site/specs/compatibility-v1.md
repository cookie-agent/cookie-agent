# Compatibility v1: Stable Protocols, Session History, Media Input, and Tool Display

Status: **draft** (decisions of 2026-10-17 below). When implemented, this
spec governs compatibility for the client protocol, the plugin protocol, the
runtime protocol, and session history. Where earlier specs or references
disagree, this one wins.

## 1. Goal and scope

Nothing outside this repository speaks cookie-agent's protocols or reads its
session logs yet. This is the last change free to break them. Afterwards,
four surfaces carry a compatibility promise:

1. **Plugins** written against plugin protocol 1.x keep working with every
   daemon that speaks 1.x, and the protocol grows without breaking them.
2. **Runtimes** written against runtime protocol 1.x likewise.
3. **Clients** (the TUI, the CLI, a future HTML client, third-party clients)
   written against client protocol 1.x likewise.
4. **Session history** keeps loading when new events and fields are added,
   and a log written under an earlier history format is converted when a
   newer build opens it.

The same change lands three features. Each one replaces an existing field,
so each must land before the freeze:

5. **User media input.** Users attach images and video (and the other kinds
   the selected model accepts) to their input.
6. **Structured tool display.** Tools describe arguments, progress, previews,
   and results as typed display blocks. Each client decides how to render
   them, and no client branches on a tool's name.
7. **Edit context.** Edits and writes show a diff with real file line numbers
   and five lines of context on each side.

**Out of scope:** a web client implementation, daemon-side media resizing or
transcoding, provider file uploads or URL reuse for media, inline image
rendering in the TUI, and new coding tools (grep, glob, multi-edit). The
display blocks are designed so that such tools need no client changes.

### Decided 2026-10-17

- **Versions.** Each protocol has a semver `MAJOR.MINOR.PATCH` version. All
  three start at `1.0.0` at the freeze. CI enforces that every change bumps
  its version correctly (section 7).
- **Runtimes.** The runtime protocol is in scope. Plugin tools speak its tool
  messages.
- **Major bumps.** A major bump of the client, plugin, or runtime protocol is
  a clean break: the daemon does not keep serving the previous major.
  **Session logs are the exception.** A log written under an earlier history
  format is converted to the current format when a newer build opens it
  (section 8.7).
- **Provider hooks.** `provider_before_request` and `provider_after_response`
  are removed.
- **Events.** The event set is merged as described in section 8.2; the
  input-lane events stay separate. An unknown *state* event in an older build
  opens the session read-only (section 8.4).
- **Old logs.** Every legacy history reader is removed. Sessions written
  before this change, labpc's included, stop opening, and no migration is
  provided.
- **Media transport.** Upload and fetch are JSON-RPC methods with chunks of
  at most 1 MiB. There are no HTTP routes.
- **Media limits.** The daemon tells clients, per model, which media it
  accepts, so clients hide or disable attachment options before the user can
  pick them. The daemon still rejects violations. A too-large asset is
  rejected; a client may offer "downscale and attach?" from that rejection.
- **Diff context.** Diffs carry five lines of context on each side of a
  change.
- **Terminal colour.** `terminal` display blocks may contain SGR colour
  sequences.
- **Browser access.** The token moves into the handshake.
  `[server] allowed_origins` lists web origins allowed to connect, and `"*"`
  allows any origin.
- **Display size.** Inline display stays at 64 KiB per result. A block that
  had to be truncated carries an artifact reference to its full content.
- **Errors.** Every error carries a stable code plus a description a user
  can understand. An optional hint says what to do next, and optional
  technical detail is shown on request (section 6).

## 2. Vocabulary

| Term | Meaning |
|---|---|
| **Surface** | One versioned contract: the client protocol, the plugin protocol, or the runtime protocol. Session history is versioned separately (section 8.7). |
| **Peer** | The other end of a connection: a client, a plugin, or a runtime. |
| **Information** | A message a receiver learns from: results, notifications, events, hook params, initialize results, stored events. |
| **Command** | A message asking the receiver to act: client RPC params, plugin and runtime requests to the daemon (`producer/*`, `tool/progress`). |
| **Open enum** | A wire enum whose readers accept values they do not know, mapped to a documented fallback. |
| **State event** | A session event that affects projection or model history replay. |
| **Audit event** | A session event a reader can skip without changing any projection. |
| **History format** | The major version of the on-disk session log layout. |
| **Display block** | One typed unit of tool presentation (section 12). |

## 3. Versions

### 3.1 Semver per surface

Each surface's version is a string `"MAJOR.MINOR.PATCH"`: three decimal
integers with no leading zeros, and no pre-release or build suffix.

| Part | Bumped for | Effect |
|---|---|---|
| MAJOR | A breaking change: something removed or renamed, a meaning changed, a new required field in a message the peer receives, a closed enum grown | Peers with different majors do not connect |
| MINOR | Any addition: a method, notification, hook, event type, display-block kind, optional field, or open-enum value | Peers use it to decide what they may send |
| PATCH | A change that leaves every wire shape alone: a documented behaviour clarified, a validation bug fixed | Informational only |

The constants live in `cookie_agent_protocol`: `CLIENT_PROTOCOL_VERSION`,
`PLUGIN_PROTOCOL_VERSION`, and `RUNTIME_PROTOCOL_VERSION`, each a
`ProtocolVersion { major, minor, patch }` that serializes as the string. The
old exact numeric wire types (`exact_numeric_wire_type!`, `ProtocolVersion`
48, `ExtensionProtocolVersion` 1, `RUNTIME_PROTOCOL_VERSION` 5, and
`RuntimeSnapshotSchemaVersion` 6) are removed. Until the freeze (phase H) the
versions stay `0.x` and change freely; the freeze stamps `1.0.0` on all
three.

Old builds send an integer version, so their decoders reject a string, and a
new build's decoder rejects an integer. The two can never be mistaken for
each other.

### 3.2 Handshake

Every handshake follows one pattern:

1. The connecting side sends its full version.
2. The receiver reads `protocol_version` from the raw JSON **before** it
   decodes anything else. A different major is answered with error
   `unsupported_protocol` (section 6), whose data names both versions. A
   malformed hello then fails as before.
3. The reply carries the receiver's own version.
4. **The shared level is the lower of the two minors.** A sender sends a
   method, field, hook, or block kind only if the receiver's version includes
   it. Because readers ignore what they do not know (section 4), a sender may
   leave additional *information* in place for older receivers; it gates only
   *commands* and anything an older receiver would misread.

Feature strings carry only what depends on configuration, never what a
version already implies. For example, `producer_messaging` is a feature
because it is present only when configured. Both sides list theirs in the
handshake. An unknown feature is ignored.

### 3.3 Which surface a message belongs to

- **Session events** (the `EventPayload` set) belong to the client protocol
  and to the plugin protocol, since both deliver them. Adding an event type
  bumps both minors.
- **The `tool/*` messages** belong to the plugin protocol and to the runtime
  protocol. A change to them bumps both. Today a runtime protocol change
  silently changes the plugin wire; runtime 3 to 5 did so while the plugin
  protocol stayed at 1.
- **Display blocks** (section 12) belong to all three.
- **Session history** is not versioned by minor. Its additions follow the
  tolerant rules of section 8.

## 4. Decoding rules

### 4.1 Tolerate information, reject unknown commands

| What arrives | Unknown field | Unknown enum value or tag |
|---|---|---|
| Information | Ignored | The open-enum fallback (4.2) |
| Command | Rejected with `-32602`, `data.code` `unknown_field`, and `data.field` set to the JSON path | Rejected with `-32602`, `data.code` `unknown_variant`, and the path |

Commands stay strict because an ignored field in a command can silently
change what happens. A sender knows the receiver's version and gates new
fields on it (3.2).

**Writers stay strict.** Every value a component builds is validated before
it is sent or written: the same bounds, the same required fields, the same
grammar as today. Strictness moves from decoders to constructors and to the
writer's validation step.

`#[serde(deny_unknown_fields)]` is removed from every type that crosses a
surface as information. It stays on command params and on configuration
(`AGENTS.md`'s strict configuration rule is unchanged). The compatibility
checker (section 7) fails when an information type's schema has
`additionalProperties: false`.

The JSON-RPC envelope is information on both sides. Unknown top-level members
are ignored, and request IDs may be integers or strings, as JSON-RPC 2.0
allows. A frame that is not JSON, or is not a request, notification, or
reply, still closes the connection.

### 4.2 Open enums

- **Tagged enums with data** (such as `EventPayload` and display blocks) use
  `Open<T>`: either `Known(T)` or `Unknown { tag, raw: Value }`. The raw JSON
  is kept so that a value can be passed on or written back unchanged.
  `open_enum!` derives the serde and schema glue.
- **Unit enums** (such as `SessionStatus` or `McpServerState`) gain an
  `Unknown` variant with `#[serde(other)]`, or become label strings.
- **Label-like values** (such as diagnostic kinds and the tool kind hint)
  are validated strings, following the existing `kind_label!` pattern.
- Every open enum documents what `Unknown` means where it is read. For
  example, an unknown session status renders as "unknown" and allows no
  action; an unknown tool collision mode behaves like `prefix`.
- An enum is closed only when growing it is a breaking change by design
  (such as `ProducerDeliveryMode`). The schema marks open enums `x-open: true`
  (section 7).

Protocol types do not get `#[non_exhaustive]`. The engine, the server, and
the TUI build and match these types across crate boundaries, so
`#[non_exhaustive]` would remove the compiler's exhaustiveness checks
internally. Rust source compatibility of the crate follows its own crate
semver. Only the plugin SDK's own types (its builders and `ToolOutput`) are
`#[non_exhaustive]`.

## 5. Wire types are not internal types

No surface, including stored events, embeds a type owned by oven-sdk or by
engine policy. Where the engine needs data it does not otherwise model, it
stores an opaque `Value`. It decodes that value best-effort when the value is
used, never when loading. These replacements apply everywhere:

| Today | Replacement |
|---|---|
| `FrozenModelBinding` (embeds `oven_sdk::LanguageModelDescriptor`, `ProviderOptions`, credential and setup bindings) in events | `PersistedModelBinding { selection, provider_id, provider_model_id, adapter: String, resolved: ResolvedModelRef, selection_fingerprint, blueprint_fingerprint, frozen: Value }`; `frozen` is decoded only to resume or recover |
| `AdaptorId`, `AdapterFamily` as closed enums on the wire | Label strings |
| `ModelCapabilities` in `tool/*` turn context | `ToolTurnModel { model, adapter, input_media: [MediaKindLabel] }` |
| `AgentSnapshot.schema` (exact 7) and its schema 5/6 decoders | No schema field; the snapshot is information |
| `PersistedToolResult` in `tool/execute` | Wire `ToolResult` (section 9.4) |
| `PreparedOperationIdentity` built by the plugin or runtime | The daemon builds the approval identity from the declared permission and resource (section 9.4) |
| `oven_sdk::HistoryTurn` JSON in `model_before_request` | `HookHistoryMessage { role, content: [HookContentPart] }`, where a part is `text`, `file` (metadata only), `tool_call`, or `tool_result` |
| Raw `EventPayload` to plugins | The public event contract (section 8) with subscription patterns (section 9.3) |
| `RuntimeSnapshotV1` and its schema version | `ModelCatalogView` (section 10.4) |
| `ExtensionInterceptionHook` enum in `PluginInfo.hooks` | Hook name strings |

## 6. Errors

All four protocols use one scheme:

| Code | Meaning |
|---|---|
| `-32700`, `-32600`, `-32601`, `-32602` | The standard JSON-RPC codes |
| `-32001` | Handshake required |
| `-32002` | `unsupported_protocol` |
| `-32003` | Unauthenticated |
| `-32000` | Any application error |

Every application error has the same shape:

```json
{ "code": -32000,
  "message": "The selected model does not accept images.",
  "data": {
    "code": "media_unsupported",
    "hint": "Choose a model that accepts images, or remove the attachment.",
    "detail": "model anthropic/claude-haiku-5-5 declares input modalities: text",
    "kind": "image", "model": "anthropic/claude-haiku-5-5" } }
```

| Member | Required | Meaning |
|---|---|---|
| `message` | yes | A description a user can understand: one or two plain sentences saying what went wrong, in the user's terms rather than the engine's. It contains no internal type or variant names, no stack traces, and no secrets. Terminal-safe, at most 1 KiB. |
| `data.code` | yes | A stable snake-case machine code from an open set. |
| `data.hint` | no | What the user can do about it, as one plain sentence. Present whenever there is a useful next step. Terminal-safe, at most 512 bytes. |
| `data.detail` | no | Technical detail for a "details" view or logs, such as the upstream cause, a path, or a revision. Terminal-safe, at most 4 KiB. It replaces today's `data.detail` and `debug_code`. |
| other `data` members | per code | Structured fields the code defines, such as `kind` and `model` above. |

The registry is one module, `cookie_agent_protocol::errors`. For each code it
lists:

- its data fields;
- a message template, filled from those fields;
- a hint template, where one applies;
- a one-line description for the docs.

The daemon always builds `message` and `hint` from the registry, so the same
code always reads the same way. The reference docs are generated from the
registry: a table of every code with its description, hint, and fields.

**How clients use it:**

- Clients branch only on `data.code`, never on `message`.
- To show an error, a client shows `message`, then `hint` when present, and
  `detail` on request.
- A client may substitute its own wording for codes it knows, for example to
  translate, or to show a "Downscale and attach?" action for
  `media_too_large`.
- For a code it does not know, `message` and `hint` are already readable, so
  new codes need no client change.

**Peers' errors.** Plugins and runtimes follow the same shape in errors they
send. The SDK fills `message` from the error the plugin returns and requires
it to be nonempty. When the daemon surfaces a peer's error to a user, such as
a failed tool call or a plugin that cannot start, it keeps the peer's
`message` and names the peer. Tool failures shown to the model keep their own
model-facing text; this shape is for people.

The existing numeric codes move into `data.code`. Examples:

| Today | `data.code` |
|---|---|
| `-32020` | `no_runnable_model` |
| `-32022` | `session_owned_elsewhere` |
| `-32030`, `-32031` | `tree_usage_*` |
| `-32040` | `config_apply_refused` |
| The provider-connect codes | Unchanged string codes |
| The runtime protocol's `-32010`/`-32011`/`-32012` | `version_mismatch`, `already_initialized`, `not_initialized` |

The several meanings `-32602` carries today (stale revert, fork idempotency,
rename, goal, producer, and plugin rejections) become `-32000` with their own
codes.

The TUI's local copy of `-32022` and its match on the message `"stale revert"`
are removed. `debug_code` is removed from error data.

## 7. Compatibility checker and test sets

### 7.1 The checker

`crates/protocol/scripts/check-compat.py` replaces
`check-event-payload-additive.py` and `check-extension-protocol-additive.py`.
`crates/protocol/scripts/check-schema-additive.sh` runs it in CI.

**Baselines** live in `crates/protocol/compat/<surface>/`, with surfaces
`client`, `plugin`, `runtime`, and `events`. Each holds:

- `version`: the version the baseline was taken at;
- `methods.json`: every method and notification with its direction, its kind
  (request or notification), and its params and result schema roots;
- `schemas/*.schema.json`: every root, generated by
  `cargo run -p cookie_agent_protocol --example generate`.

The generator covers every root, including the ones missing today:
`config.*`, `plugins.*`, `provider.connect`, and all `tool/*` messages.

**Rules.** The checker walks nested definitions and compares each type by its
role. It knows a type's role from `methods.json`: a type that is only a
command's params, a type that only flows as information, or both (strictest
rule wins).

| Change | Information type | Command params |
|---|---|---|
| Field added (optional) | minor | minor |
| Field added (required) | minor (writer always sets it) | **major** |
| Field removed or renamed | **major** | **major** |
| Field type changed | **major** | **major** |
| Optional field became required | minor | **major** |
| Bound tightened (`maxLength`, `maxItems`, range) | **major** | minor |
| Bound loosened | **major** | minor |
| Value added to an `x-open` enum | minor | minor |
| Value added to a closed enum | **major** | **major** |
| Value removed from any enum | **major** | **major** |
| `const` changed | **major** | **major** |
| `additionalProperties: false` on an information type | rejected outright | – |
| Method or notification added | minor | minor |
| Method or notification removed | **major** | **major** |
| No schema change | patch or none | patch or none |

Loosening a bound on information is major because older readers may still
enforce the old bound on data they store.

The checker computes the required bump and fails unless the surface's
version constant has at least that bump against the baseline's `version`.
`crates/protocol/scripts/update-compat-baseline.sh` regenerates a baseline and
refuses when the constant is too low.

**Events.** Every `EventPayload` tag must be in the events baseline, and new
tags are added only through the update script. Today 20 of the 86 tags are
not in the baseline at all.

The existing self-test scripts become one mutation test of `check-compat.py`
covering every row of the table.

### 7.2 Test sets

Recorded at the freeze (phase H), each in a directory the tests refuse to
regenerate (`COOKIE_UPDATE_*` switches do not apply to them):

- `crates/protocol/compat/corpus/client/1.0.0/`: a recorded TUI session's
  requests, with the notifications and replies it received.
- `crates/protocol/compat/corpus/plugin/1.0.0/` and `.../runtime/1.0.0/`:
  recorded conversations of the Python reference plugin (section 9.6) and the
  coding runtime.
- `crates/engine/tests/fixtures/history/1/`: real multi-run session trees
  from a scripted run against a fake provider (section 8.8).

What the tests prove:

- **New builds accept old data.** Recorded requests replay against the
  daemon, recorded plugin and runtime messages replay against the SDK and the
  daemon, and recorded logs open, project, and rebuild model history equal to
  a recorded snapshot.
- **Readers tolerate future data.** "Future" fixtures inject unknown fields,
  enum values, event tags, block kinds, methods, and notifications at every
  level. The client, the SDK, the daemon, and the history loader must accept
  them.

Old readers meeting new data is covered by the checker (7.1), since old code
cannot run in tests.

Each later minor release adds its own corpus directory next to the earlier
ones.

## 8. Session history

### 8.1 Legacy readers removed

Removed with all their decoders, defaults, and tests:

- the events `session_reverted`, `context_rehydrated`, `plugin_event_added`,
  `tool_stdin_submitted`, `delegate_finished`, and `user_input_recalled`
  (the old form);
- dropping stored `text_delta`, `reasoning_delta`, and `tool_call_progress`
  lines;
- the legacy `plugin_diagnostic` kinds;
- the `AgentSnapshot` schema 5 and 6 wire structs, and the `workspace`
  document-source alias;
- the `plugin { plugin }` producer owner;
- `legacy_local_runtime`;
- ignoring `event_schema_version`;
- stripping checklist item `id`s;
- the defaults for a missing `capability` (`runtime_attached`) or
  `attachment` (`agent_md_loaded`/`agent_md_skipped`), and the rule that a
  log with neither attach event is attached to `global` and `local`;
- `output_chunk` on `tool_call_progress`;
- the `GoalReminderKind` default.

`user_input_recalled_v2` and `delegate_finished_v2` take the plain names
`user_input_recalled` and `delegation_finished` (8.2).

The fixtures `events-schema-18.jsonl`, `-20.jsonl`, and `-21.jsonl` are
deleted.

### 8.2 The event set

| Today | Becomes |
|---|---|
| `plugin_diagnostic`, `producer_diagnostic`, `engine_diagnostic`, `agent_md_skipped` | `diagnostic { source, kind, message, count?, detail? }` (audit). `source` is `engine:<area>`, `plugin:<name>`, or `runtime:<id>`; `kind` is a label; `detail` is display-only JSON |
| `run_completed`, `run_failed`, `run_cancelled`, `run_interrupted` | `run_finished { outcome }`, where `outcome` is `completed { final_text? }`, `failed { error, model_error?, resolved_model? }`, `cancelled { reason? }`, or `interrupted { reason? }` (open) |
| `internal_agent_completed`, `_failed`, `_cancelled`, `_interrupted` | `internal_agent_finished { invocation_id, internal_run_id, kind, outcome }`, with `completed { result }`, `failed { failure }`, `cancelled { reason? }`, `interrupted { reason? }` |
| `delegation_started`, `delegation_run_started`, `delegation_run_attached`, `delegate_queued` | `delegation_progressed { invocation_id, stage }`, where `stage` is `started { child_session_id }`, `run_started { child_run_id }`, `run_attached { child_run_id }`, or `queued { position? }` |
| `delegation_finished`, `delegate_finished_v2` | `delegation_finished { invocation_id, child_session_id, child_run_id?, status, reason?, short_id?, preview, total_lines }` |
| `approval_evaluated`, `approval_escalated`, `approval_user_decision_recorded`, `approval_doom_loop_detected` | `approval_step { approval_id, step }`, where `step` is `evaluated { decision }`, `escalated { reason_code }`, `user_decision { client_response_id, decision, feedback? }`, or `doom_loop { operation_fingerprint, repetitions }` |
| `approval_finalized`, `approval_cancelled` | `approval_finalized { approval_id, outcome }`, where `outcome` is `decided { decision }` or `cancelled { reason_code }` |
| `user_input_recalled_v2` | `user_input_recalled { user_input_seq, content }` |
| `input: String` on every input-lane event | `content: UserContent` (section 11.1); `user_input_transformed` carries `original_content` and `content` |
| `tool_call_started.presentation` | Presentation v2 (section 12.2) |
| `tool_call_terminated` result `display: String` | `display: [DisplayBlock]` (section 12) |

The input-lane events otherwise stay as they are: `user_input_admitted`,
`user_input_submitted`, `user_input_transformed`, `user_input_applied`,
`user_input_queued`, `user_input_queue_recalled`, `user_input_queue_voided`,
`message_injected`, and `session_model_selected`. So does every other event
not listed above.

**Checked against the code (2026-10-18):**

- `delegate_queued` is written on the parent, in the parent run, when the
  invocation is at hand, so `delegation_progressed { stage: queued }` carries
  its `invocation_id`. Tree loading pairs queued children with their parent
  run through the invocation's `delegation_progressed { stage: started }`.
- `delegate_child_terminated` is written runless to the *child* session and
  sets the child's own status. It is not a parent-side delegation record, so
  it stays as is.
- `producer_diagnostic` is read only by the validator and the TUI. The goal
  hold lives in memory, not in a projection, so the event merges into
  `diagnostic`.

### 8.3 Internal types out of history

- `session_created`, `run_started`, `delegation_reserved`, and
  `FrozenInternalAgentFallback::Model` store `PersistedModelBinding`
  (section 5) instead of `FrozenModelBinding`.
- `run_started` stops storing `selected_suffix` next to an
  `agent.fallback_chain` it duplicates. It stores the fallback start index
  instead.
- The seven revision IDs repeated in `session_created`, `run_started`, and
  `delegation_reserved` are stored on `session_created` and on a later event
  only where they changed.

### 8.4 State and audit events

Every event type is classed as **state** or **audit**:

- **State:** anything the projection, model history, delegation recovery,
  goals, producers, approvals, or usage read.
- **Audit:** everything else, such as `diagnostic`, `model_request_prepared`,
  `model_replay_evaluated`, `model_tools_published`, and the `approval_step`
  kinds the projection ignores.

`events.md` lists the class of every event.

A writer stamps `"skippable": true` in the envelope of every audit event.

**Unknown events.** A reader meeting an unknown event tag:

- skips it if the envelope says `skippable`;
- otherwise loads the session in **render-only** mode. Clients can view and
  subscribe, but `run.start`, `session.submit_input`, revert, fork, and
  compaction fail with `data.code` `session_requires_newer_engine`. A
  diagnostic names the event.

**Additive rules for writers:**

- A new optional field on an existing event must be safe to ignore: its
  absence means the old behaviour.
- A field whose absence would be misread needs a new event type.
- A new open-enum value must have a fallback that is safe for old readers.

### 8.5 The loader

- Unknown envelope keys are ignored, on line 1 as well.
- **Validation:**
  - Cross-event validation failures during load quarantine the record: it is
    skipped and tainted, with a diagnostic, instead of failing the open.
  - Full validation stays strict for appends.
  - A non-increasing `seq` ends the readable log at that line. The session
    opens read-only, with a diagnostic, instead of failing.
- **Nothing derived is recomputed on read:**
  - Stored approval operation fingerprints and delegation request
    fingerprints are trusted.
  - An in-flight delegation whose reservation does not verify becomes
    unrecoverable for that delegation alone, instead of failing the tree
    load.
  - `wire_resolved` and comparisons with today's built-in internal-agent IDs
    are not used by load validation.
  - Size caps are writer-only.
- **Unknown data survives.** Each loaded record keeps its raw line.
  - Fork, publish, and conversion copy the raw bytes of records they did not
    fully understand (unknown, degraded, or quarantined).
  - Artifact liveness scans raw lines for `artifact://sha256/` references.
- A `metadata` cache that does not decode is rebuilt from `events.jsonl`,
  instead of the session disappearing from the list.
- The best-effort decoder stops re-decoding a whole payload once per bad
  leaf; it locates failures with `serde_path_to_error`.

### 8.6 Envelope

```json
{ "engine_version": "0.5.0", "session_id": "…", "run_id": null, "seq": 1,
  "timestamp": "…", "origin": "user", "skippable": true, "payload": { … } }
```

`skippable` is written only when true.

### 8.7 History format and conversion

`session_created` carries `history_format: 1`. A reader handles a log as
follows:

- **No `history_format`:** a pre-1.0 log. It is not converted, and the open
  fails with the existing unsupported-era error.
- **Same format:** read under 8.4 and 8.5.
- **Higher format:** the open fails with `data.code`
  `session_requires_newer_engine`.
- **Lower format:** converted.

The history format bumps only when a change cannot follow 8.4's additive
rules.

**Converters** are pure functions from the old log's JSON lines to new lines.
They never use old Rust types. Each lives in
`crates/engine/src/events/convert/v<N>_to_v<N+1>.rs`, is registered in one
table, and is kept, so they chain: a format-1 log opened by a format-3 build
converts 1 to 2 to 3.

**When a log is converted:**

- **Owned open:** converts the whole tree under its `owner.lock`: the root
  log, every child log, the `metadata` cache, and `subagents/index.json`.
  Each file is written next to the original and fsynced. Each original is
  renamed to `events.jsonl.v<N>.bak`, and the new file is renamed into place.
  A failure leaves the originals untouched.
- **Read-only open:** converts in memory only.
- **`cookie sessions upgrade [--all]`:** converts every session up front.

Backups are kept until the user deletes them.

A test-only converter from format 0 to format 1 exercises the machinery
before the first real converter exists.

This is the governing exception to `AGENTS.md`'s rule against migrations.

### 8.8 Golden logs

A scripted run against the engine's fake provider records real session
trees. It covers:

- text, reasoning, and native replay;
- tool calls, including background and failed ones;
- approvals (asked, granted, denied, tree grants);
- delegation (foreground, background, resumed);
- summary and native compaction;
- goals and producer messages (steer, queue, discard);
- revert and fork;
- user images and video;
- display blocks of every kind.

The trees are committed under `crates/engine/tests/fixtures/history/1/`,
with a snapshot of each tree's projection and model history. Tests open them
read-only and owned, and compare against the snapshot.

## 9. Plugin and runtime protocols

### 9.1 Handshake

`plugin/initialize` and `runtime/initialize` share one shape.

Daemon → peer params:

```json
{ "protocol_version": "1.0.0", "engine_version": "0.5.0",
  "features": ["producer_messaging"] }
```

The runtime variant keeps its existing members (`runtime_id`, settings, MCP
servers). The plugin result:

```json
{ "protocol_version": "1.0.0", "name": "issues", "version": "1.2.0",
  "features": [],
  "capabilities": { "producer_messaging": false, "subscribe_events": ["run_*", "tool_call_*"],
    "subscribe_bus": false, "publish_bus": false,
    "intercept": [{ "hook": "tool_before_call", "on_error": "block" }] },
  "tools": { "tools": [], "prompt_sections": [] } }
```

The runtime result keeps `capabilities`, its list of offered capabilities,
with the new `protocol_version` and `features`.

- Majors are compared from raw JSON first (3.2).
- **An unknown hook name** or an unknown feature in a result gets a
  diagnostic for that entry only; the plugin still starts.
- **An unknown tool-declaration field** is ignored. The writer validates its
  own declarations.
- **Known values that need a newer daemon** are skipped with a diagnostic
  when the daemon's version is below the one that introduced them, for
  example a collision mode.
- **The SDK exposes** the daemon's version, `engine_version`, and features
  to plugin code.

### 9.2 Tolerance on both ends

Section 4 applies to every `plugin/*`, `tool/*`, and `producer/*` message.

**Commands the daemon receives** (`producer/*`, `tool/progress`,
`plugin/emit`) are strict. A peer gates new fields on the daemon's version.

**What changes from today:**

- **SDK decoding:**
  - A hook whose params carry unknown fields still runs.
  - An unknown event or bus message decodes as `Unknown` instead of being
    dropped silently.
- **SDK timeouts:**
  - `plugin/emit_result` waits at most `interception_timeout_ms`; past that,
    `emit_bus` resolves as `dropped`.
  - Today a lost result hangs `emit_bus`.
- **Daemon messages from plugins:**
  - Plugin connections no longer accept the runtime-only
    `capability/tools_changed` and `mcp/changed`.
  - `tool/release` or `tool/cancel` with params the daemon cannot read fails
    loudly (a diagnostic) instead of being ignored.

### 9.3 Hooks and events

- **Hooks removed:** `provider_before_request` and `provider_after_response`.
- **`on_error`** on each intercept entry is `allow` (the default) or
  `block`. With `block`, a hook that times out, crashes, rejects, or answers
  malformed blocks its operation:
  - a tool call is blocked;
  - an input, fork, or revert is refused;
  - a model request fails the attempt.

  Today every failure fails open, so a `tool_before_call` policy plugin stops
  enforcing when its params change.
- **`model_before_request`** carries `messages: [HookHistoryMessage]`
  (section 5) and `model: { provider_id, model_id, adapter }`. A
  replacement's messages are validated as today.
- **`message_end`** carries `content: [HookAssistantPart]`, where a part is
  `text`, `reasoning` (text only), `tool_call`, or `file` (metadata only).
  It no longer carries the persisted type.
- **`user_before_input`** carries `content: UserContent`, with file parts as
  metadata only. A transform may change or remove parts but not add files.
- **`agent_before_start.prompt_context`** gets a defined, documented shape
  instead of an ad-hoc `json!`.
- **`subscribe_events`** is a list of tag patterns (`*` matches any suffix).
  Plugins receive `plugin/event { session_id, context_id, seq, event }`,
  where `event` is the stored event's payload under the public event
  contract. An unknown tag reaches the SDK as `Unknown`. An empty list
  subscribes to nothing, and `["*"]` subscribes to everything.

### 9.4 Tool messages

- **Turn context:** `tool/prepare` and `tool/execute` carry
  `turn: { agent, model: ToolTurnModel }` (section 5).
- **Prepare result:** `PreparedCall` drops `operation`. It carries:
  - `permission`, `permission_resource`, and `normalized_arguments`, as
    today;
  - presentation v2 fields the runtime owns: `title`, `hint`,
    `primary_argument`, `arguments`, and `preview` (section 12.2).

  The daemon builds the approval identity (`PreparedOperationIdentity`) from
  the declared permission, the resource, the normalized arguments, and its
  own tool identity. Non-Rust peers no longer reproduce a digest byte for
  byte.
- **Execute result:** `tool/execute` returns
  `ToolResult { output: ToolCompletionOutput, title?, display: [DisplayBlock], metadata?, attachments, additional_messages, failed }`.
  The daemon builds `PersistedToolResult` from it. `media` stays as is.
- **Progress:** `tool/progress` carries `display` as a block delta
  (section 12.3).

### 9.5 SDK

- **`ToolOutput`** becomes a builder: `text`, `title`, `display` blocks,
  `metadata`, `attachments`, and `failed`.
- **`ToolContext::progress`** can send a display delta.
- **New per-tool hook:** `prepare_presentation(call) -> Presentation` for
  the title, hint, arguments, and preview.
- **Exposed to plugin code:** the negotiated version, `engine_version`, and
  features.
- **Crate versioning:** the SDK has its own crate semver. It exposes
  `PLUGIN_PROTOCOL_VERSION`, and its major follows the plugin protocol's
  major.

### 9.6 Schemas and the reference plugin

- Schemas for every plugin and runtime message are committed under
  `crates/protocol/compat/` (section 7) and published with the docs.
- `crates/engine/tests/fixtures/fake_plugin.py` grows into
  `examples/plugins/python/`: a dependency-free reference plugin with a tool,
  a hook, events, and a producer.
- CI validates its messages against the schemas and runs it against the
  daemon.

## 10. Client protocol

### 10.1 Handshake and authentication

```json
{ "method": "handshake", "params": {
    "protocol_version": "1.0.0",
    "client": { "kind": "web", "instance_id": "…" },
    "token": "…", "features": [] } }
```

Reply:

```json
{ "protocol_version": "1.0.0", "engine_version": "0.5.0",
  "origin": "client:web-…", "machine_id": "…", "features": [] }
```

**Authentication:**

- **Where the token goes:** the WebSocket upgrade no longer requires an
  `Authorization` header. A client authenticates either with that header at
  upgrade (native clients may keep it) or with `token` in the handshake.
  Browsers cannot set headers on a WebSocket, so a web client uses the
  handshake.
- **Before authenticating:** a connection that has not authenticated may
  send only `handshake`. Anything else gets `-32003`.
- **Wrong token:** `-32003`, and the connection closes.
- **Deadline:** a connection not authenticated within 10 seconds is closed.

The `/runtime` endpoint keeps header authentication.

**Origins.** A connection whose upgrade request has no `Origin` header (any
native client) is unaffected. One with an `Origin` header is accepted only
if `[server] allowed_origins` allows it:

```toml
[server]
allowed_origins = ["https://cookie.example", "http://localhost:5173"]
# or: allowed_origins = ["*"]
```

- **Default:** empty, which refuses every origin, as today.
- **Entries:** exact `scheme://host[:port]` origins, or the single entry
  `"*"`. `"*"` combined with other entries is a configuration error.
- **With `"*"`:** any web page can open a connection, and the token is the
  only protection. The configuration reference says so.
- **TLS:** a page served over HTTPS can only connect with `wss://`, so it
  needs the daemon's TLS settings.

### 10.2 Decoding

- **Results and notifications are information (4.1).** `Open` is used for:
  - `EventPayload` and `EventSubscriptionMessage`;
  - `SessionsChangedNotification`'s change kind;
  - `RuntimeChangeReason`;
  - `McpServerState` and `PluginState`;
  - display blocks;
  - every other enum in results.
- **Notifications are isolated.** One that does not decode is dropped. The
  client gets a `ClientDelivery::Diagnostic`, and **no pending call fails**.
  Today one bad notification fails every call in flight.
- **Replay** delivers an event it cannot decode as `Unknown`; the page does
  not fail.
- **Error data** decodes per `data.code`, and an unknown code decodes as a
  generic error.
- **TUI rendering:** unknown events render as nothing (a dim row in the
  debug view), unknown states as "unknown", and unknown blocks as their text
  fallback (section 12.4).

### 10.3 Params

Params are commands (4.1). An unknown or mistyped member fails with `-32602`
naming the JSON path.

Clients gate methods and params on the daemon's minor. A `-32601` from a
method the client believed supported shows "the daemon is too old for X".

### 10.4 Payloads trimmed to client views

- **`runtime.snapshot.get` and `runtime.changed`** carry
  `ModelCatalogView { revision, providers, models, agents }`:
  - `revision` is one opaque string replacing the 11 revisions and
    counters;
  - providers, models, and agents are client-facing descriptors without
    frozen bindings, recipe IDs, or compiler versions;
  - every model states `input_media` (section 11.2).

  `RuntimeSnapshotSchemaVersion` and the cap of 9 change reasons are
  removed.
- **`SessionMeta`** keeps its revision IDs as opaque strings.
- **`config.get` and `config.apply`** stay documents in the configuration
  format. That format keeps its own strict rules, and a configuration key
  change is a configuration change, not a protocol change.

### 10.5 Docs

- **`protocol.md`:**
  - Drop the protocol 34–48 history paragraph; a changelog page lists
    versions from 1.0.0.
  - Complete the methods table (`session.tree_usage`, `mcp.auth.*`, and
    `mcp.server.*` are missing).
  - Fix the stale `42` in the handshake example.
- **`schemas.md`:** state the three versions and the history format.
- **Generated method reference:** produced from `methods.json`.

## 11. User media input

### 11.1 Content parts

```json
"content": [
  { "type": "text", "text": "What is wrong with this layout?" },
  { "type": "file", "artifact": { "sha256": "…", "byte_length": 482113,
      "media_type": "image/png", "filename": "screenshot.png" } }
]
```

`UserContent` is a nonempty list of `UserContentPart` (open): `text { text }`
or `file { artifact: ArtifactRef }`. Text parts follow today's input limits.

**Where it replaces `input: String`:**

- the params of `run.start`, `run.steer`, and `session.submit_input`;
- `SessionRecallInputResult` and `QueuedInput`;
- the input-lane events (8.2).

`message_injected` and producer bodies take `UserContent` but accept text
parts only, for now.

### 11.2 Advertised limits

Each model in `ModelCatalogView` states:

```json
"input_media": [
  { "kind": "image", "mime_types": ["image/png", "image/jpeg", "image/gif", "image/webp"],
    "max_bytes": 3932160, "max_dimension": 8000, "max_pixels": 16777216, "max_count": 20 },
  { "kind": "video", "mime_types": ["video/mp4"], "max_bytes": 19608371, "max_count": 1 }
]
```

- **`kind`** is an open label: `image`, `pdf`, `audio`, or `video`.
- **The limits are effective:** the catalog's capability, the provider and
  adapter-family delivery limits (`runtime_api::media`), and the attachment
  caps (20 MiB, or 25 MiB for video), whichever is lowest.
- **Clients** show attachment options only for kinds the session's selected
  model accepts. They filter file pickers by `mime_types`, and they check
  size and pixels before uploading.
- **Fallback models:** the limits are the selected (primary) model's. A
  fallback model without that kind gets placeholders (11.5).

### 11.3 Upload and fetch

| Method | Params | Result |
|---|---|---|
| `artifact.upload.begin` | `session_id`, `byte_length`, `filename?` | `upload_id`, `chunk_bytes` (1 MiB) |
| `artifact.upload.chunk` | `upload_id`, `offset`, `data` (base64, at most `chunk_bytes` decoded) | `{}` |
| `artifact.upload.finish` | `upload_id`, `sha256` | `ArtifactRef`, plus `image: { width, height }` for images |
| `artifact.upload.cancel` | `upload_id` | `{}` |
| `artifact.read` | `session_id`, `sha256`, `offset`, `length` (at most 1 MiB) | `data` (base64), `total_bytes`, `media_type` |

**Uploads:**

- **Storage:** uploads go to the session tree's artifact store. A client
  that attaches before its first prompt creates the session first.
- **`begin`:** refuses `byte_length` above the largest attachment cap.
- **Chunks:** must arrive in order without gaps.
- **`finish`:**
  - checks the digest;
  - sniffs the media type from the content, ignoring the filename;
  - validates the content as tool attachments are validated today;
  - stores the blob.
- **Scope:** an upload in progress belongs to its connection and is dropped
  when the connection closes.
- **Lease:** a finished upload no event references is kept for one hour,
  then swept.
- **Becoming durable:** a submitted input's event references the blob, so
  revert sweeps and fork linking cover it as they cover tool attachments.

**Fetch:**

- `artifact.read` serves any artifact in the session's tree: user uploads,
  tool attachments, and the `full` references of display blocks.
- Every authenticated client is trusted with the whole daemon, so no
  per-artifact check is added.

**Errors** (`data.code`):

| Code | Data |
|---|---|
| `media_too_large` | `kind`, `byte_length`, `max_bytes`, `width?`, `height?`, `max_dimension?`, `max_pixels?` |
| `media_type_unsupported` | `media_type` |
| `digest_mismatch` | – |
| `upload_not_found` | – |
| `artifact_not_found` | – |

A client may answer `media_too_large` by offering to downscale and attach;
browsers can do this with a canvas. The TUI shows the limit.

### 11.4 Admission

At `run.start`, `run.steer`, and `session.submit_input`, each file part must:

- name an artifact in the tree's store;
- be of a kind the model of the run accepts (`media_unsupported { kind, model }`);
- fit that model's byte and pixel limits (`media_too_large`).

The parts of one input of a kind must also not outnumber that kind's
`max_count` (`media_limit_exceeded { kind, count, max_count }`).

These checks back up the advertised limits (11.2); a well-behaved client
never triggers them.

### 11.5 Model history

- User turns with file parts become oven-sdk user messages with
  `InputPart::File`, the same way tool-emitted user messages are built today
  (`attachment_file`). The existing count elision and protection of the
  latest message apply.
- **Media fit.** A new history step runs before each request. It replaces
  every file part (user, tool-result, or tool-emitted) whose kind the
  attempt's model does not accept with a placeholder like this one:

  ```text
  [image omitted: <model> does not accept images (screenshot.png, 471 KB)]
  ```

  The placeholder is deterministic, so cache prefixes stay stable. Today a
  model switch or a fallback to a model without that kind fails the whole
  request in oven-sdk.
- Compaction summaries see the same placeholders.

### 11.6 TUI

- **`/attach <path>`** uploads a local file. The TUI may run on a different
  machine from the daemon, so it always uploads.
- **Pasted paths:** text pasted into the composer that is exactly one
  existing file path (what terminals paste on drag-and-drop) offers to
  attach it.
- **The composer** shows each attachment as `📎 name · type · size`, and
  removes it with backspace on the chip.
- **Options shown:** only kinds the selected model accepts.
- **Later, not in this spec:** clipboard image paste and inline previews.

## 12. Structured tool display

**Principle.** Tools describe what happened as typed blocks, and clients
choose how to render them. No client branches on a tool's name or parses a
tool's arguments. Display never enters model history; the model's output is
unchanged by this spec.

### 12.1 Display blocks

`DisplayBlock` is an open enum tagged by `type`:

| Type | Fields |
|---|---|
| `text` | `text` |
| `markdown` | `text` (CommonMark) |
| `code` | `text`, `language?`, `start_line?` |
| `file_excerpt` | `path`, `language?`, `start_line`, `lines: [string]`, `total_lines?`, `full?` |
| `diff` | `path`, `old_path?`, `language?`, `change` (`created`, `modified`, `deleted`, or `renamed`; open), `hunks`, `added`, `removed`, `omitted?: { hunks, lines }`, `as_of` (`prepare` or `execute`), `full?` |
| `terminal` | `segments: [{ stream, text }]`, `exit_code?`, `full?` |
| `key_value` | `entries: [{ key, value }]` |
| `table` | `columns: [string]`, `rows: [[string]]`, `omitted_rows?` |
| `json` | `value` |
| `media` | `artifact: ArtifactRef`, `alt?` |
| `link` | `url`, `label?` |
| `session_ref` | `session_id`, `short_id?`, `label?` |

**Field details:**

- **A hunk** is `{ old_start, old_lines, new_start, new_lines, lines: [{ op, text }] }`.
  `op` is `context`, `add`, or `remove`.
- **Line numbers** are 1-based file line numbers. A client numbers each line
  from the hunk's starts.
- **`full`** is an `ArtifactRef`, present only on a truncated block. It names
  the complete content: the complete diff, stream, or excerpt. A client
  fetches it with `artifact.read`.
- **Control characters:**
  - Text fields allow `\n` and `\t` only.
  - `terminal` segment text also allows SGR sequences (`ESC [ <params> m`).
  - The daemon strips every other control sequence from blocks it receives.

**Budget:**

- All display blocks of one result share 64 KiB, serialized.
- Blocks are cut as whole units: whole hunks, rows, segments, or lines.
- A cut block carries `omitted` and, where the content exists elsewhere,
  `full`.
- `full` for terminal output reuses the call's retained output artifacts.

### 12.2 Presentation v2

`tool_call_started` carries:

```json
"presentation": {
  "title": "Edit src/main.rs",
  "hint": "file_edit",
  "tool": { "name": "edit", "published_name": "labpc_edit",
            "source": { "type": "runtime", "runtime_id": "labpc", "attachment": "labpc_repo", "capability": "coding" } },
  "primary_argument": "src/main.rs",
  "arguments": [ … DisplayBlock … ],
  "preview": [ … DisplayBlock … ]
}
```

**Who supplies what:**

- **The runtime or plugin** supplies `title`, `hint`, `primary_argument`,
  `arguments`, and `preview` from prepare.
- **The daemon** fills `tool`. `source` is an open tagged value: `base`,
  `runtime { runtime_id, attachment, capability }`, `plugin { plugin }`, or
  `mcp { server, runtime_id? }`.

**Fields:**

- **`hint`** is an open label used only for icons and grouping. The
  documented values are `shell`, `file_read`, `file_write`, `file_edit`,
  `fetch`, `search`, `delegate`, `message`, `skill`, `goal`, and `generic`.
  An unknown hint renders as `generic`. The daemon never branches on it.
- **`arguments`** shows the call's arguments, such as the command as a `code`
  block or a path. Clients fall back to the raw model arguments when it is
  absent.
- **`preview`** shows what the call will do, such as a diff as of prepare.

`ApprovalRequest` gains `tool_call_id`, so approval views show the call's
presentation and preview directly instead of matching operation
fingerprints.

The background state is a typed `background: { state }` on the result,
replacing `metadata.background`.

### 12.3 Progress and results

- **`tool_call_progress.display`** is a delta:
  - `append { index, text }` appends to the text of block `index`, which
    must be a `text` or `terminal` block. For a `terminal` block it appends
    to the last segment of the same stream, or starts a new segment.
  - `replace { blocks }` replaces the live list.

  The 64 KiB cumulative budget is unchanged. The TUI also shows the progress
  `message`, which it ignores today.
- **`tool_call_terminated` results** carry `display: [DisplayBlock]`. Their
  final display replaces the live one.
- **Disk encoding:** `display_span` is kept as an internal encoding for a
  `text` or `terminal` block whose text occurs verbatim in `output`. The
  daemon expands it before anything leaves the engine.

### 12.4 Text fallback and version downgrade

- Every block kind has a canonical text rendering, `DisplayBlock::to_text`,
  defined in the protocol crate.
- **Older clients:** the daemon replaces a block whose kind is newer than the
  client's negotiated minor with a `text` block of that rendering. Every
  client sees something.
- **Unknown kinds from peers:** a block of a kind the daemon does not know
  (from a newer plugin or runtime, which should not send it) is dropped, with
  a diagnostic.
- **Unknown kinds in logs:** a block of a kind unknown to the daemon, in a
  log written by a newer minor, is dropped from what clients receive. The raw
  data stays on disk.
- **The CLI** prints results with `to_text`.

### 12.5 Built-in tools

| Tool | Arguments | Preview | Result display |
|---|---|---|---|
| `edit` | `path`; the replacement as `code` | `diff` as of prepare, from the file prepare already reads | `diff` as of execute, built from the replaced ranges |
| `write` | `path` | `diff` as of prepare (reads the existing file) | `diff`: `created`, or `modified` against the text it replaced |
| `read` | `path`, range | – | `file_excerpt`; `table` for directories; `media` for media |
| `bash` | `code` (`sh`) | – | `terminal` with stdout and stderr segments (SGR kept) and `exit_code` |
| `webfetch` | `link` | – | `key_value` (status, type, final URL), plus `text` or `markdown` for the body excerpt |
| MCP tools | `json` of the arguments | – | `text` for text content, `json` for `structured_content`, `media` for media |
| `delegate_subagent`, `get_subagent_result` | `key_value` | – | `session_ref`, plus `markdown` |
| `skill`, `goal_*`, `send_message`, `read_artifact`, `stop_background` | `key_value` | – | `markdown`, `key_value`, or `text` as fits |

**Diff details:**

- **Context:** five lines on each side of every change. Hunks whose context
  overlaps merge.
- **`edit` with `replaceAll`:** one hunk per replacement, merged where they
  overlap.
- **`write`** computes a line diff of the old and new text when both are text
  of at most 1 MiB. Otherwise it reports `change` with counts and no hunks.
  The line diff uses the `similar` crate (Apache-2.0, accepted by
  `deny.toml`).
- **As of prepare vs execute:** the prepare preview can differ from the
  execute result when the file changed in between, because execute
  recomputes the replacement. `as_of` tells clients which one a block is.

### 12.6 TUI

Rendering is driven by block type and `hint`. Every special case below is
removed:

- `tool_icon` and `external_tool_icon`: icons come from `hint` instead.
- Argument handling:
  - `abbreviate_tool_argument`;
  - `ParsedToolArguments` and its argument-shape language detection;
  - `display_tool_arguments`.
- Body rendering:
  - the bash title expansion;
  - `parse_read_output`;
  - `tool_diff` and its snippet-relative pseudo-diff.
- Approval view:
  - `approval_operation_preview`, replaced by `preview`;
  - the bash hero-line rule.
- State reduction:
  - the `delegate_subagent` name check, replaced by `session_ref` and
    `hint`;
  - `metadata.background`, replaced by the typed field.

Diffs render with file line numbers, five lines of context, and `+`/`-`
gutters. `terminal` blocks render SGR colours. A truncated block offers to
fetch `full`.

## 13. Phases

Each phase is one or more focused commits, pushed per phase. Until phase H
the version constants stay `0.x` and change freely.

| Phase | Content |
|---|---|
| **A** | This spec; `specs/index.md` and `mkdocs.yml`; amend `plugins-v1.md` (drop "every part may change") |
| **B** | Shared types and tooling, no wire change: `Open<T>`, `open_enum!`, the error registry, `ProtocolVersion` semver type, `DisplayBlock` with `to_text`, `UserContent`, `check-compat.py` with baselines from the current code |
| **C** | Session history break: 8.1–8.4, 8.6, and the history-format stamp from 8.7. Includes the new input and display shapes in events and `PersistedModelBinding`. Existing sessions stop opening |
| **D** | The tolerant loader (8.5) and the conversion machinery (8.7, test-only converter) |
| **E** | Plugin and runtime protocols (section 9): handshake, tolerance, hooks, tool messages, SDK, reference plugin |
| **F** | Client protocol (section 10), including browser access; media upload, fetch, limits, admission, and history (11.2–11.5) |
| **G** | Coding runtime and base-tool display (12.5); TUI rendering (12.6) and attachments (11.6); CLI text output |
| **H** | Freeze: stamp `1.0.0` on all three protocols; record the golden logs and corpora (7.2, 8.8); regenerate baselines; update `AGENTS.md`; mark this spec implemented |

## 14. Documentation changes

- **`AGENTS.md`:** replace "The project does not preserve backward
  compatibility…" with a compatibility section. It says that the client,
  plugin, and runtime protocols and session history follow this spec. Every
  wire change must pass `check-compat.py` with the right version bump.
  Session-log converters are the only migrations allowed.
- **`docs/site/reference/compatibility.md`** (new): the contributor rules
  from sections 3–8, written as rules.
- **Updated references:** `protocol.md`, `runtime-protocol.md`,
  `development/plugins.md`, `events.md` (the class column, the history
  format, conversion), `schemas.md`, and `tools.md` (display blocks per
  tool).
- **`guide/configuration.md`:** `[server] allowed_origins`.
- **`guide/sessions.md`:** `cookie sessions upgrade`.

## 15. Implementation notes

Deviations and choices made while implementing, by phase.

### Phase B

- **Names.** The semver type is `SemVer`; the exact numeric
  `ProtocolVersion` keeps its name and wire until the handshakes switch
  (phases E and F), which then remove it. `CLIENT_PROTOCOL_VERSION` (`0.48.0`),
  `PLUGIN_PROTOCOL_VERSION` (`0.1.0`), and `RUNTIME_PROTOCOL_VERSION`
  (`0.5.0`) are crate-root constants; the old `u32`
  `runtime_protocol::RUNTIME_PROTOCOL_VERSION` lives on beside them until
  then. `peer_protocol_version` reads a handshake's version from raw JSON.
- **`Open<T>`.** A known tag with a malformed body is a decoding error, not
  `Unknown`: `Unknown` means "from a newer peer". Tolerant readers (the
  history loader) handle the error where they know what to skip. The known
  tags come from the enum's own schema, so `open_enum!(tagged T, tag = "…")`
  cannot drift from its serde attributes.
- **Display blocks.** Cutting needs an `omitted` count on every cuttable
  kind, not only `diff` and `table`: `text`, `markdown`, `code`,
  `file_excerpt`, and `terminal` carry `omitted_lines`, and `key_value`
  carries `omitted_entries`. `terminal` blocks are cut by line within their
  segments, so a single long segment still keeps its head. `json`, `media`,
  `link`, and `session_ref` cannot be cut and are dropped when they do not
  fit. The budget leaves room for the blocks after a cut block when they fit
  whole. `diff.change` and the terminal `stream` are open (unknown reads as
  `modified` and `stdout`); `as_of` and the hunk line `op` are closed.
- **Progress delta.** `append` names no stream in 12.3, yet a terminal
  append goes to "the same stream", so `append` has an optional `stream`
  (default `stdout`; an error on a `text` block).
- **Strict commands with shared types.** New types that also flow as
  information (`ArtifactRef`, `UserContent`, display blocks) do not deny
  unknown fields. Commands that embed them (`run.start` content) need their
  strictness from the decoder (an unknown-field check that names the JSON
  path) rather than from `deny_unknown_fields`.
- **Error registry.** Today's `-32011` covers both a repeated initialize
  and a rejected one, so both `already_initialized` and
  `initialize_rejected` are registered. `version_mismatch` stays until the
  runtime handshake answers `unsupported_protocol`. `config_apply_refused`
  carries the refusal as `reason` (`conflict`, `invalid_configuration`,
  `invalid_change`). `idempotency_conflict` is one code across run, rename,
  approval, fork, and provider requests, with the request's IDs as optional
  fields. Templates may name only required fields.
- **Checker rules beyond the table.** A required field that became optional
  is major for information and minor for commands; a closed enum that became
  open is minor and the reverse major; `additionalProperties` going from
  `false` to allowed is major for information and minor for commands, and
  the reverse major for commands; any `type` change (nullability included)
  and any `patternProperties` change is major. Before the freeze (`0.x` in
  the baseline and the generated set) the baseline must equal the generated
  set exactly and findings are only reported, as phases C to G change the
  wire freely.
- **Surfaces.** The plugin surface omits `capability/tools_changed` and
  `mcp/changed`, which the daemon still accepts from plugins until 9.2
  removes them. The events baseline's `version` records the client version.
  `ModelSnapshotManifestV1` is no longer exported (it is not on any surface),
  and `ProviderConnectParams` now is: its schema describes the credential
  fields' shape, never a value.

### Phase C

- **Runless recall.** The positional `user_input_recalled { input }` was
  still written: a cancelled queued child's runless steers were withdrawn
  with it. The plain-named `user_input_recalled { user_input_seq, input }`
  therefore also exists runless, naming a runless admission, and readers
  match every recall by its admission sequence.
- **Attachments.** With the "no attach events" rule gone, a session's
  attachment set is exactly the fold of its attach and detach events, and a
  session created with no attachment writes no attachment event.
- **Event storage.** Without revert markers the visible branch is the whole
  log, so the engine keeps one event list instead of a physical and a
  visible one.
- **One delegation finish.** `delegation_finished` is written where the
  control record was, for every delegation, and carries the teaser. It is
  not model history. Every background completion already accepted its steer
  producer message before the old `delegate_finished_v2`, so the old
  suppression rule always applied to current writes; written at the control
  point, the merged record would otherwise appear in a request assembled
  before the acceptance and the result would reach the model twice. The
  result reaches the model once, as the tool result or the accepted producer
  message, and a restart re-drives a missing acceptance as before.
  `ParentRunFacts` loses `notified`: a background delegation counts as
  notified once its producer message is accepted.
- **Approval cancellation.** The pair `approval_cancelled` +
  `approval_finalized` the engine wrote for a system withdrawal is one
  `approval_finalized { outcome: cancelled { reason_code } }`, whose final
  decision is derived (`expired` for `approval_expired`, else `cancelled`;
  source `system`). A user's cancel stays a `decided` outcome, since its
  source is the user.
- **Open inner enums.** `RunOutcome`, `InternalAgentOutcome`,
  `DelegationStage`, `ApprovalStep`, and `ApprovalFinalization` are open
  (`Open<T>`, with cheap named tags from a new `open_enum!` form). Unknown
  run and internal-agent outcomes read as interrupted, an unknown approval
  outcome as cancelled, and unknown stages and steps change nothing.
- **Diagnostics.** `diagnostic.kind` is a snake-case label of at most 64
  bytes; `count` is present only from 2; `detail` is at most 16 KiB. The
  agent-md notice is `engine:agent-md` / `agent_md_too_large`, its message the
  text the TUI used to compose, and the source of a plugin's notice is
  `plugin:<name>`. The Rust kind catalogs (`PluginDiagnosticKind`,
  `ProducerDiagnosticKind`, `EngineDiagnosticKind`) stay as writer helpers.
- **`skippable` is derived.** `StoredEvent` has no `skippable` field: it is
  serialized from the payload's class and checked against it when read, so a
  value built in memory cannot disagree with its payload. A step of
  `approval_step` this build does not know accepts either value. The
  classes: `diagnostic`, `skill_invocation_noted`, `user_input_transformed`,
  `model_request_prepared`, `model_tools_published`, `model_output_started`,
  `model_replay_evaluated`, and the `doom_loop` approval step are audit;
  everything else is state (live-only events are never stored).
- **Render-only.** The event log refuses every append and truncation of a
  render-only log (`EventLogError::RequiresNewerEngine`), which covers
  automatic compaction, titles, and recovery; `run.start`,
  `session.submit_input`, revert, fork, and compaction also check before they
  start work. Adoption skips the restart repairs. A fork is refused because
  it would drop the records this build cannot read. The server answers with
  the registry's `session_requires_newer_engine` (`-32000`), naming the
  `event` or the `history_format`.
- **History format.** A `session_created` without `history_format` fails as
  the existing unsupported-era corrupt-log error; a higher one with
  `session_requires_newer_engine`; a lower one with
  `EventLogError::UnsupportedHistoryFormat`, which phase D replaces with
  conversion.
- **`truncation`.** Every writer left a tool result's `truncation` null
  since retained output moved to `retained_output`; its readers (the model's
  tool-result JSON, the TUI, the compaction retry hint) served old logs only.
  The field and `ToolOutputTruncation` are gone. `PersistedToolResult` is
  also the `tool/execute` result, so the runtime protocol moves to 6
  (`0.6.0`); the plugin protocol already moved to 2 in this step.
- **Adapter labels.** `AdaptorId` is a label (`[a-z][a-z0-9-]{0,31}`; the
  `kind_label!` grammar with room for `azure-openai-responses`) with
  constants for the adapters this build resolves to; nothing branches on it.
  `AdapterFamily` stays an enum, since media delivery branches on it, but
  reads and writes the same label grammar, and a family this build does not
  know reads as `Unknown`, which delivers no media.
- **Persisted bindings.** `PersistedModelBinding` has the shape of section
  5; `adapter` is the binding's protocol adapter (the descriptor's, such as
  `oven.openai.responses`), and the stored identity must agree with itself
  (selection, provider and model, `resolved`) when read. `decode` returns the
  frozen binding only when it reads and names the same selection,
  fingerprints, and adapter. It is called where the engine runs a stored
  binding again: the latest run's policy for manual compaction, title
  regeneration, and `get_history`, and a frozen internal agent's model
  fallbacks. A failure fails that operation with `unreadable_model_binding`
  and never the open. A restart never resumes a run with its frozen bindings:
  runs left running are interrupted, and a new run resolves its models from
  the live runtime as before. A delegation reservation's bindings are never
  decoded on read either, since recovery does not rebuild a child from them.
  `FrozenModelBinding` and the model-snapshot manifest types are no longer on
  any surface, so they lose their schemas and `LanguageModelDescriptorSchema`
  is gone; no protocol type reachable from `EventPayload` names an oven-sdk
  type.
- **Selected suffix.** The snapshot's existing `selected_suffix_start` is the
  fallback start index, so `run_started` and also `delegation_reserved`
  (which carried the same copy) drop `selected_suffix`. The run's model is
  exactly the chain entry at the start, variant included: the engine always
  froze the chain with the selection's own variant, so the old exception for
  a head of another variant never applied. Attempt and fallback validation
  compares `resolved_model` with the stored `resolved` of the entries.

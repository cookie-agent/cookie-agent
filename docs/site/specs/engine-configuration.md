# Live Engine Configuration

Status: **implemented** (2026-10-17; decisions in [Review](#9-review), see
[Implementation notes](#10-implementation-notes)). Addendum of 2026-10-23
([section 11](#11-addendum-2026-10-23-clients-restart-the-daemon)): clients
restart the daemon, draft awaiting the go-ahead.

## 1. Goal and scope

A daemon now runs for days and serves clients on other machines, but its
configuration is read once at start: changing an agent document or a
`config.toml` setting means editing files on the daemon's machine and
restarting it. This spec lets a client read and change the daemon's
configuration over the client protocol, and has the daemon apply changes
without a restart wherever it can:

1. **Engine configuration:** the user `config.toml`.
2. **Agent documents:** shared agents in `agents/*.md` and preset agents in
   `agents/<preset>/*.md`, including adding and removing documents and
   presets.
3. **Reloading** after the files were edited by hand.

It also fixes the provider-connect defects found while reviewing `/connect`
([section 2](#2-provider-connect-fixes)), which land first.

**Decided 2026-10-17:**

- Provider credentials stay with the daemon, in its provider store. Nothing
  here moves or exposes them.
- Provider setup values (endpoint, region, project) are not secret: the
  `safe_to_project` distinction is removed, and every client shows them.
- Clients edit whole documents and, in `config.toml`, single keys.
- Hand edits apply on an explicit reload; there is no file watcher.
- `[runtimes]` and `[plugins]` changes are reported as needing a restart;
  making them live is a follow-up.
- The TUI edits documents in `$VISUAL`/`$EDITOR`.

**Out of scope:** project-level configuration (the daemon still reads only
the user root), skills (still re-read when a runtime instance starts), the
TUI's own `tui.toml`, and live changes to `[server]`.

## 2. Provider connect fixes

These change no configuration and land as phase 1.

1. **Setup values are always shown.** Remove `SetupFieldDescriptor.safe_to_project`
   (client protocol bump). The runtime snapshot projects every stored setup
   value; the TUI pre-fills and shows them unmasked; `cookie connect` already
   shows them. Credentials (`auth_values`) stay masked on input and are never
   projected.
2. **One provider operation per provider, not per client.** The TUI keeps
   connect and disconnect tasks in one slot and aborts the previous task when
   a second starts, which can leave a provider stuck "in progress". Keep a
   task per provider ID and refuse a second operation on the same provider
   while one runs.
3. **Catalog races retry.** `provider.connect` carries the client's
   `expected_catalog_revision`; a background catalog refresh makes it fail
   with `catalog_revision_conflict`. Clients refetch the snapshot and retry
   once when the provider's descriptor (setup fields and auth methods) is
   unchanged; otherwise they reopen the form with the new descriptor.
4. **Error codes match the failure.**
   - Connecting a custom (`[providers.*]`) provider returns
     `custom_provider_not_store_backed`, not `runtime_compile_failed`.
   - A missing setup field or credential returns `missing_setup_field` /
     `missing_credential`, not `invalid_credential`.
   - Codes nothing can produce (`quarantined_provider`, disconnect
     `invalid_provider`) are removed.
5. **One client implementation.** `cookie connect` uses
   `Client::connect_provider` (and its lock-contention retry) instead of its
   own request type and method string, and asks in the TUI's order: auth
   method, setup values, then credentials.
6. **A failed connect keeps what was typed.** The TUI wipes credentials before
   sending, so a failed attempt (wrong region, transient error) makes the user
   retype the key. Keep the entered values, masked and zeroized on drop, until
   the connect succeeds or the form is closed. (The comment that claims this
   already happens is corrected either way.)
7. **The daemon wipes received credentials.** Frames are decoded through
   `serde_json::Value`, and only the typed `ProviderCredentialValues`
   zeroizes. After deserializing `provider.connect` params, the server
   zeroizes the strings of the decoded value and of the raw frame buffer.

## 3. Model: configuration generations

The daemon's configuration is one **configuration tree**: the text of
`config.toml` and of every agent document, keyed by path relative to the user
root (`config.toml`, `agents/reviewer.md`, `agents/python/primary.md`). Each
document has a fingerprint (SHA-256 of its bytes); the tree has a
**configuration revision** (a hash over the paths and fingerprints).

- **Loading from memory.** The config crate gains
  `load_from_tree(&ConfigTree) -> Result<LoadedConfiguration, ConfigError>`,
  the same strict decode and whole-set validation as loading from disk, with
  no files involved. `load_from_user_root` reads the tree from disk and calls
  it. Validation errors name the document and, where the parser gives one,
  the line.
- **One live generation.** The engine keeps the current
  `LoadedConfiguration` behind an `ArcSwap` instead of a plain field. Each
  run already captures its agent at admission (`FrozenRunPolicy`); it also
  captures the configuration `Arc`, so a run in progress keeps the settings it
  started with, and queued and later runs use the new generation.
- **Last good wins.** A candidate that fails validation is never applied; the
  daemon keeps running on the current generation and reports the error.

## 4. Applying a new generation

`Engine::apply_configuration(candidate)` validates, swaps the generation,
applies each part, and publishes the runtime with `config_reloaded` (and
`agent_reloaded` when any agent document changed). Parts apply as follows:

| Part | Applies |
|---|---|
| `tool_output` (engine side), `subagent_output`, `agent_md`, `loop_warning`, `approval`, `model_retry`, `session_title`, `context_compaction`, `delegation`, `messaging`, `producers` | Live: read from the generation by the next run |
| Agent documents and presets | Live: re-resolved into the published runtime; running runs and their delegated children keep their frozen agents |
| `providers`, `headers` | Live: a new `ModelManager` API recompiles authored providers and global headers; pricing is recomputed with them |
| `mcp` | Live: the user layer of the MCP store is rebased onto the new file, the runtime overlay (and its tombstones) stays on top, and changed servers are upserted or removed in the registry |
| `tool_output` (runtime side) | New runtime instances get it at initialize; open instances keep theirs until they reconnect |
| `runtimes`, `plugins` | Restart required (reported, see below) |
| `server` | Restart required |

**Restart-required parts** are recorded, not applied: the configuration
status lists each part whose new value differs from the one the daemon
started with, so a client can say "restart the daemon to apply `plugins`".

**What a removal does to existing work.** A session whose agent or preset
disappeared follows the existing fallback rules at its next run (the preset
falls back to the shared agents, the agent to `primary` or the first runnable
one); an explicit selection of a missing agent or preset is refused, as
today. Title generation for runs recorded before internal agents were frozen
falls back to the built-in title agent instead of failing.

## 5. Protocol

Client protocol bump; additive schema check updated.

| Method | Params | Result |
|---|---|---|
| `config.get` | `{}` | `{revision, documents: [{path, text, fingerprint}], status}` |
| `config.apply` | `{expected_revision, changes: [Change]}` | `{revision, status}` |
| `config.reload` | `{}` | `{revision, status}` |

- **`Change`** is one of:
  - `{"put": {path, text, expected_fingerprint?}}`: create or replace a
    document; `expected_fingerprint` is absent for a new one and required
    otherwise.
  - `{"delete": {path, expected_fingerprint}}`: remove a document. Removing
    the last document of a preset removes its directory.
  - `{"set": {key, value}}`: set one key of `config.toml`. `key` is the path
    of table and key names (`["context_compaction", "threshold"]`,
    `["mcp", "servers", "docs", "command"]`); `value` is JSON mapped to TOML
    (string, integer, float, boolean, array, or object as an inline table),
    and `null` removes the key. The edit goes through `toml_edit`, so the
    rest of the file, comments included, is untouched. A `set` and a `put`
    of `config.toml` in one change set are refused.

  `path` is `config.toml`, `agents/<id>.md`, or `agents/<preset>/<id>.md`;
  anything else is refused.
- **Atomic as a set.** The daemon builds the candidate tree with every change,
  validates it whole (so deleting a subagent and editing its delegator go
  together), then writes. Each file is written with the existing secure
  commit (0600 temp file, compare against what was read, rename, directory
  fsync); if a later write fails, the earlier ones are restored from their
  originals. The engine applies the new generation only after every write
  succeeded.
- **Conflicts.** `expected_revision` and each `expected_fingerprint` are
  checked against disk under the configuration lock; a mismatch is
  `config_conflict` and nothing is written. A client then refetches and
  merges.
- **`config.reload`** re-reads the tree from disk, for edits made by hand,
  and applies it like `config.apply` does.
- **`status`**: `{applied_revision, restart_required: [part], error?}`.
  `error` is the last failed reload's message when disk and the applied
  generation differ.
- **Notification.** `RuntimeSnapshotV1` gains `config_revision`; every applied
  change publishes `runtime.changed` with `config_reloaded` /
  `agent_reloaded` (reasons that exist today but are never sent), so other
  clients know to refetch.
- **Trust.** Documents travel as they are on disk, including
  `${env:NAME}` placeholders and any literal values in `[providers.*]`, the
  daemon token in `[server]`, and MCP headers. Anyone holding the daemon
  token is already a fully trusted client.
- **MCP methods stay.** `mcp.server.*` keeps its runtime overlay;
  `mcp.server.persist` writes through `config.apply`'s commit path, and
  persisting a removal now deletes the server's table from the file.

## 6. Clients

- **TUI `/config`.** A panel lists `config.toml`, every preset with its
  agents, and the shared agents, with the configuration status (last error,
  restart-required parts). Enter opens the document in `$VISUAL`/`$EDITOR`
  (the TUI suspends, as for an external editor), then sends it with
  `config.apply`; a validation error reopens the editor on the same text with
  the error shown. `n` creates an agent (in the selected preset or shared),
  `d` deletes one, `r` runs `config.reload`. Because the text travels over the
  protocol, this works against a remote daemon.
- **CLI.** `cookie config edit [<path>]` (default `config.toml`) does the
  same in the terminal; `cookie config set <dotted.key> <TOML value>` and
  `cookie config unset <dotted.key>` send a single `set`;
  `cookie config reload` and `cookie config status` report the status.

## 7. Phases

Each phase lands as its own commit, with every gate, docs, and tests.

1. **Provider connect fixes** ([section 2](#2-provider-connect-fixes)),
   including the `safe_to_project` removal.
2. **Generations:** `load_from_tree`, the swappable generation with run
   capture, `apply_configuration` for the live parts, restart-required
   status, `config.reload`, `config_revision`. The MCP writer's validation
   moves from a temporary directory to `load_from_tree`.
3. **Writes:** `config.get` / `config.apply` (`put`, `delete`, `set`) with
   the set-atomic commit and rollback; `mcp.server.persist` on the same path,
   including removals.
4. **Clients:** TUI `/config` and `cookie config`.

## 8. Tests

- Config crate: `load_from_tree` agrees with loading the same files from
  disk; every validation boundary that applies to files applies to the tree.
- Engine: a run in progress keeps its settings and agent across an apply;
  the next run sees the new ones; a failed candidate leaves the generation
  and the published runtime unchanged; removing a preset falls queued work
  back as documented; restart-required parts are reported only when they
  differ.
- Protocol: conflicts on revision and fingerprint, a change set that is only
  valid as a whole, rollback when a write fails, refused paths; `set` keeps
  comments and other keys, removes with `null`, and is refused for a value
  that fails validation.
- Provider fixes: per-provider task slots, the retry on a catalog conflict,
  each error code, and that the server wipes received credentials.

## 9. Review

Decided 2026-10-17:

1. **Edit unit:** whole documents and single-key `set` edits of
   `config.toml`, both in `config.apply`.
2. **Hand edits:** explicit `config.reload`; no file watcher.
3. **`runtimes` and `plugins`:** restart required for now; live in a
   follow-up.
4. **TUI editing:** `$VISUAL`/`$EDITOR`.

## 10. Implementation notes

Landed in four commits: the provider fixes (client protocol 46), reloading
(47: `config.reload`, `config_revision`), reading and writing (48:
`config.get`, `config.apply`), and the clients. Where it differs from the
sections above:

- **What a run keeps.** A run keeps what it captures at admission: its agent
  and delegated children's registry, result limits, retry policy, and
  delegation depth. Other settings (approval timeout, loop warning,
  compaction, delegation concurrency, messaging) apply from their next use,
  also inside a run in progress, rather than the run holding a whole
  configuration generation.
- **Revisions.** `config_revision` is part of `runtime_revision`, so a
  settings-only change publishes a new runtime revision.
- **`config.reload`** answers `{status}`; a configuration that fails
  validation is reported in `status.error` rather than as an RPC error.
  `config.get` also lists `empty_presets`.
- **Errors.** A refused `config.apply` is JSON-RPC error `-32040` with data
  `{code: conflict | invalid_configuration | invalid_change, message}`.
- **A refused TUI edit** is not reopened at once: the panel shows why, marks
  the document *not applied*, and Enter reopens the rejected text.
  `cookie config edit` asks whether to edit again.
- **`cookie config remove <path>`** was added for parity with the TUI's
  delete.
- **Writer.** `write_tree` writes the changed documents in path order and
  restores the earlier ones when a later write fails; the old temporary
  validation directory is gone, so `write_server_token` and every write
  validate in memory with the agent documents.
- **Persisting MCP servers** drops the runtime-layer entry once the file holds
  it, so the server's source becomes the user file.
- **Runtime changes stay off the async workers** (2026-10-23). A runtime
  change (a catalog refresh, a `config.apply` or reload, a provider connect
  or disconnect, a provider-store reload) holds the engine's runtime
  mutation lock while it recompiles, which takes seconds in a debug build.
  The catalog refresh and provider connect/disconnect already ran on the
  blocking pool, and `runtime.snapshot.get` answers with the published
  runtime instead of waiting (2026-10-23, commit `42eb99ad3`). The paths that
  must still wait for the change, so a run or session started mid-change uses
  the runtime it publishes, now wait on the blocking pool too
  (`Engine::reconcile_provider_store_off_worker`): `session.create`, run
  start, `session.select_model`, `session.goal.set` and
  `session.goal.lifecycle` with a selection, and
  `session.permission.preview`. The configuration apply recompiles there as
  well. Only the in-process `cookie run` session creation keeps waiting on
  its own worker.

## 11. Addendum 2026-10-23: clients restart the daemon

Status: **draft** (2026-10-23), settled with the user, awaiting the go-ahead
to implement.

`config.status` already reports `restart_required` parts (`server`,
`plugins`, `runtimes`) that only a restart applies, but no client can
restart the daemon. This addendum adds that, so a change made outside a
client (an edited `config.toml`, a new binary on disk) can be applied from
the TUI, the web client, or the CLI.

**Decided 2026-10-23:**

- A plain "Restart daemon" waits until no run is active; restarting now,
  interrupting running turns, is an explicit choice.
- A daemon that cannot relaunch itself refuses the call.

### 11.1 Protocol

Client protocol minor, additive:

- `daemon.restart { when: "idle" | "now" }` → `{ scheduled, active_runs }`.
  `when` defaults to `idle`. A second call while one is scheduled updates its
  `when` (`now` wins).
- `daemon.restarting` notification `{ when, active_runs, reason? }`, sent to
  every client when a restart is scheduled and as the wait progresses.
- New error `restart_unsupported` ("this daemon can't restart itself") for a
  daemon not started by `cookie daemon`, such as one embedded in a test
  harness or another program.
- Any client holding the daemon's token may call it; the token already
  grants full control.

### 11.2 Behaviour

- **`idle`:** the restart waits until no run is active in any session.
  Meanwhile new runs are refused with "the daemon is restarting" (cron fires
  are recorded as skipped for that reason), producers' messages queue as
  usual, and clients show why.
- **`now`:** running turns end as interrupted, as after any stop; recovery
  closes them out when the daemon comes back.
- **The restart:** the daemon finishes in-flight replies, flushes session
  logs and its stores, stops its runtimes and plugins as on shutdown, closes
  client connections with a `restarting` close reason, and relaunches its
  own executable with the same arguments and environment:
  - **Unix:** an in-place `exec`, keeping the process ID, so systemd and
    launchd see no change.
  - **Windows:** it starts a new process with the same command line and
    exits; the new process waits for the listening port to free before
    binding.
  - The executable is re-read from disk, so a restart after an upgrade runs
    the new build; clients already notice a changed daemon build when they
    reconnect.
- **Afterwards:** clients reconnect as after any dropped connection; dial-in
  runtimes reconnect as they already do. Sessions are on disk; nothing is
  lost.

### 11.3 Clients

- **Banner:** while `config.status` lists `restart_required` parts, the TUI
  and the web client show "Restart needed to apply: plugins, runtimes ·
  Restart daemon".
- **Always available:** TUI `/restart`; web Settings → "Restart daemon";
  `cookie daemon restart [--now] [--url <url>] [--token <token>]`.
- **Confirmation:** "Restart when idle" (the default) or "Restart now
  (interrupts N running turns)".
- **While waiting:** "Restarting when idle…" until the connection closes,
  then the client's ordinary reconnecting state.

### 11.4 Tests

- `idle` waits for an active run and refuses new runs meanwhile; `now`
  interrupts and the next start recovers the runs; a second call switching to
  `now`; `restarting` notifications; `restart_unsupported` from an embedded
  engine.
- The relaunch itself, in an integration test that starts `cookie daemon`,
  restarts it through the RPC, and reconnects (Unix; Windows in CI).
- The TUI and web banner, buttons, and confirmation.

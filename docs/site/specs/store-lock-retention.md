# Store Lock Retention and Contention Policy

Status: draft proposal. Not implemented. This document records design decisions
for how cookie-agent acquires and retains the cross-process advisory locks that
guard its `~/.cookie-agent` secure stores, and for the deprecation of the
localhost daemon bearer token. It amends the platform trust model described in
[Security boundaries](../guide/security.md). All file references cite the
current tree and were verified against source at the time of writing.

## Problem

cookie-agent coordinates cross-process writes to several files under
`~/.cookie-agent` (provider store, model catalog cache, snapshot manifests,
OAuth credential store, daemon bearer token, artifact temporaries) using
advisory OS locks — `flock(2)` on unix and `LockFileEx` on Windows, reached
through `SecureDirectory::lock` (`models/src/secure_store/mod.rs:116`,
`models/src/secure_store/windows.rs:626`) or directly via `fs2`/`rustix`.

These locks are correct for mutual exclusion but the acquisition and retention
*policies* are inconsistent across the tree, and two of them create real
failure modes:

1. **Unbounded blocking acquisition.** Several stores acquire the lock in
   blocking mode with no timeout (`directory.lock(..)`,
   `fs2::FileExt::lock_exclusive(..)`). `LockFileEx` has no OS-level timeout, so
   a Windows acquisition that meets a held lock parks the calling thread
   forever. Because the lock-taking store calls are synchronous and reached from
   async server routes, the thread that parks is a tokio worker — a permanent
   reduction in the async runtime's parallelism, not a slow request. This is a
   leading suspect for the Windows CI whole-process freeze (four delegated tests
   each parked one worker on a four-vCPU runner).

2. **Wide retention where it is load-bearing.** The provider-store transaction
   acquires the lock in `begin_transaction` (`store.rs:169`) and holds it across
   `propose_connect`, `compile_runtime`, and the caller's
   `prepare_publication` callback, releasing only after `commit`
   (`manager/mod.rs:1130-1155`). `commit` checks `base_generation` /
   `base_revision` only against the state captured at `begin`
   (`store.rs:395-412`), never against disk — so the *correctness* of the
   transaction currently depends on the wide hold. Shortening the hold without
   adding a commit-time re-read would break that.

Contrast the lock that is already correct: session ownership
(`engine/src/ownership.rs:124`) uses **non-blocking** `try_lock_exclusive` and
returns a typed result — ownership yields
`SessionOwnership::Foreign` → `SessionError::SessionLocked` (`session.rs:1355`)
with no wait, because the lock is a *claim* held for an unbounded lifetime and
waiting would gain nothing.

A separate, cleaner design emerges from comparing the two reference products:

- **Claude Code** uses `proper-lockfile` (mkdir + mtime-stale + heartbeat +
  bounded 5–10 attempt retry). Its refresh lock is held *across* the OAuth
  network call, protected by a 5 s heartbeat and a post-acquire token-compare
  that skips a refresh another process already did. All acquisition is bounded.
  Its stale/heartbeat/PID machinery exists **only** because a mkdir lock survives
  a process crash and must be detected as dead.
- **opencode** moved hot relational state to SQLite (WAL, `busy_timeout=5000` →
  retryable error) and keeps a mkdir `Flock` (heartbeat + 5-min acquisition
  ceiling) for shared stores. Its `busy_timeout` history shows SQLite does not
  remove the waiter-hang class, only reshapes it.

Neither uses `flock`/`LockFileEx`. cookie-agent's choice of OS advisory locks is
**superior** on the axis that forces their complexity: an OS advisory lock is
released by the kernel when the holding file descriptor closes, including on
crash. We therefore must **not** imitate their stale-detection / heartbeat /
PID-liveness / release-token apparatus — it would be pure liability solving a
problem we do not have. Our locks' remaining problems are narrower: unbounded
acquisition, overly wide retention, and one genuine lost-update hole in the
OAuth refresh path.

## Decisions

### Locked classes (current inventory)

| Store | Lock site(s) | Today | Decision |
|---|---|---|---|
| Provider store | `store.rs:169` | blocking, wide retention | D3 primitive + D4 commit-CAS |
| Model catalog cache | `catalog/manager.rs:299,330` | blocking, short sections | D3 primitive only |
| Snapshot manifests | `manifests/mod.rs:71,108` | blocking, short sections | D1 — delete both locks |
| OAuth credential store | `mcp.rs:592,605,622,634,908,954` | blocking; reads take the exclusive lock; refresh = 3 lock episodes | D6 — lock-free reads + bounded write RMW + D3 + CAS/claim |
| Daemon bearer token | `auth_token.rs:67` | blocking (Windows) | **D8 — delete token file; per-run token via stdout** |
| Artifact temporaries | `artifacts.rs:459,710,1296` (create); `:605,1317` (sweep) | blocking on O_EXCL-unique paths; sweep is `try_lock` | D2 — keep + document |
| Session ownership | `ownership.rs:124` | non-blocking try → typed `Foreign` | reference pattern; unchanged |
| Session-store migration (v1→v2) | `migration.rs:1226` (lock), `:1095` (batch owner claim) | non-blocking try → loud typed error | **D9 — delete whole subsystem; v1 treated as nonexistent** |

### D1 — Delete the snapshot-manifest locks

Both `scan` (`manifests/mod.rs:71`) and the write path (`manifests/mod.rs:108`)
acquire `MODEL_SNAPSHOT_LOCK_FILE` only to (a) enumerate files and (b)
check-digest-then-install. Neither needs a cross-process lock:

- Snapshot manifests are **content-addressed** (filename is the manifest's
  digest) and installed via atomic rename. Two processes installing the same
  digest write byte-identical content to the same name; last-writer-wins is a
  no-op. The existing digest-collision guard (equal digest ⇒ equal content, else
  `ModelSnapshotDigestMismatch`) is a content-addressing invariant, not a
  mutual-exclusion concern; it is enforced by reading the installed file back,
  which is safe without a lock.
- `scan` enumerates a directory and reads manifest files; a concurrent install
  mid-scan yields a torn-but-consistent view only in the sense that a
  just-installed file may or may not appear this scan — acceptable, since the
  next scan sees it, and the index is rebuilt from files present on disk.
- Retention/GC already runs against unique per-operation paths and atomic
  replaces, so removing these two locks does not expose GC to a partial write.

Remove both `self.directory.lock(MODEL_SNAPSHOT_LOCK_FILE)` acquisitions; the
`MODEL_SNAPSHOT_LOCK_FILE` constant is deleted. Keep all validation, size caps,
and the `create_new` + rename install sequence. This is the single largest
reduction in blocking acquisitions.

### D2 — Keep the artifact locks and document why (do not delete)

Investigation reversed the initial assumption that artifact locks are removable
telemetry. The `lock_exclusive` at `artifacts.rs:459,710,1296` is taken on a
freshly `O_EXCL`-created, operation-unique temporary, so it can never contend
(see D2 invariant below) — but the *sweep* paths (`artifacts.rs:605,1317`)
deliberately use non-blocking `try_lock_exclusive` and skip any temporary whose
lock is currently held. The create-lock is thus a genuine **in-use marker**:
the exclusive lock held by a live writer is what tells a concurrent GC/capture
sweep "this stale-by-mtime temporary is still owned, do not unlink it." Deleting
the create lock would let a sweep delete a temporary another process is still
writing.

Documented invariant, added as a comment at the create sites and the sweep:

> Artifact locks are taken **only** on operation-unique paths created with
> `O_EXCL` (`.<uuid>.<nanos>.<rand>.tmp`). A lock on such a path is an
> in-use marker, not a mutual-exclusion point: no other process ever opens the
> same name, so `lock_exclusive` here never waits. Sweeps use non-blocking
> `try_lock_exclusive` and skip a locked temporary as still-owned.

Consequence: the artifact class contributes **zero** unbounded-wait risk and
needs no D3 bounding. (The content-addressed blob rename at
`artifact_store/retain.rs` and the per-operation retain/capture paths are
process-exclusive by construction and stay unchanged.)

### D3 — Bounded acquisition primitive: `lock_within`

Add a bounded-timeout acquisition to the secure-store layer and route all
blocking store acquisitions through it. No new third-party dependency
(`fs4` was evaluated and rejected — it exposes no timeout API and its
`AsyncFileExt` methods are documented-synchronous; see Non-goals).

- API: `SecureDirectory::lock_within(&self, name, budget: Duration) ->
  Result<SecureDirectoryLock<'_>, SecureStoreError>`, with a new
  `SecureStoreError::LockContention { .. }` variant (D5). The existing
  `lock()` becomes an alias for `lock_within(name, DEFAULT_BUDGET)`; no lock site
  uses the unbounded form.
- unix: loop `flock(LOCK_EX | LOCK_NB)`; on `WouldBlock` back off and retry until
  the budget elapses, then return `LockContention`.
- windows: loop `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)`;
  on `ERROR_LOCK_VIOLATION` back off and retry until budget elapses. This reuses
  the shape already proven in `replace_windows_path_with_retry`
  (`session.rs:4527`), promoting it to a shared primitive.
- Backoff: exponential with jitter, starting ~2 ms, capped ~50 ms, until budget.
- Budget default: **5 s** (comfortably above healthy contention between
  co-operating local processes, far below the point where a user perceives a
  hang). Windows CI benefits directly: a bounded 5 s acquisition cannot park a
  worker past the test watchdog.

Sites migrated to `lock_within`: provider store `store.rs:169`, catalog
`catalog/manager.rs:299,330`, OAuth `mcp.rs:592,622,908` (via the
`SecureDirectory` path) and the unix `OAuthStoreLock::acquire`
`mcp.rs:954` (replace its `lock_exclusive` with a bounded `LOCK_NB` retry loop of
the same shape).

### D4 — Provider-store retention: shrink via commit-time compare-and-swap

Shorten the provider-store lock so it does not span compile or the publication
callback, while preserving the atomicity the current wide hold was masking:

1. `begin_transaction` keeps the lock only for its own re-read (already does),
   releases at end of read, and returns `{ state, transaction_id, base_generation,
   base_revision }` **plus the current on-disk stamp** it read.
2. Proposal compilation, `compile_runtime`, and `prepare_publication` run **with
   no file lock held**.
3. `commit(proposal)` re-acquires `lock_within`, **re-reads the on-disk state**,
   and compares the live `generation` / `store_revision` against the proposal's
   recorded `base_generation` / `base_revision`. Equal → atomic replace. Not
   equal → drop the lock and return `ProviderStoreError::Stale` (retryable; the
   caller re-begins).

`ProviderStoreTransaction` loses its held-`lock` field and gains a base stamp;
`commit` gains the re-read + compare. This removes the "compile under lock"
retention entirely. `ProposedProviderStore` already carries
`base_generation` / `base_revision` (`store.rs:395-412`), so no new proposal
fields are required.

**Stale handling (resolved).** A `Stale` commit surfaces as a retryable error
and is **never replayed inside the manager**. Rationale, recorded because it
was the open design question:

- The store already implements the idempotent-retry protocol one layer up:
  mutations carry a `client_request_id` and `propose_*` returns `Replay(...)`
  for an id already landed (`manager/mod.rs:1134`). A manager-internal retry
  would duplicate that protocol unseen and, worse, re-enter with the stale
  in-memory `current` Arc (its non-store halves — `authored`,
  `global_headers` — predate the race), double-invoking the caller's
  `prepare_publication` closure with no idempotency contract.
- The retry-on-lost-response case (commit succeeded, caller never saw it) is
  answered by the replay path only when the retry carries the **same**
  `client_request_id` — so the retry must originate at the request layer.

Contract: `ModelManager::connect/disconnect` map `Stale` to a distinct
retryable `ModelManagerError::Contention`; the client layer (TUI/RPC caller)
resubmits the identical request (same `client_request_id`) **exactly once**;
a second consecutive `Stale` surfaces to the user as contention while the
pending payload (e.g. the filled connect form) stays held. Post-D4 the race
window is the CAS + atomic rename (milliseconds), so the human-visible
re-click path should be near-unreachable.

### D5 — Contention becomes a typed, retryable, surfaced outcome

- New `SecureStoreError::LockContention { path, waited_ms }`.
- Store/manager/callers map it to a distinct retryable error
  (`ProviderStoreError::Contention`, catalog equivalent, OAuth `AuthError`
  transient variant) rather than a generic I/O failure.
- RPC wire code: **`"lock_contention"`** — snake_case, matching the existing
  `SessionError::SessionLocked(_) => "session_locked"` mapping
  (`server/src/rpc.rs:302`); added to the same `error_code` classifier.
- TUI copy: **"Another cookie-agent process is writing the {store} — try
  again."** where `{store}` is a human noun from the error site
  (providers / model catalog / credentials). No auto-retry in the TUI for
  store contention (unlike D4's client-layer resubmit, which is request-id
  idempotent; a raw store write has no such idempotency handle).

### D6 — OAuth store: lock-free reads + refresh discipline

- **Reads (`get`/`load`) drop the lock entirely — both platforms.** A read is
  not a read-modify-write, and the store is mutated only via temp-file +
  atomic replace, so a reader always observes a complete old-or-new document:
  there is no torn state to guard. This is correct on Windows too because the
  general data reader (`windows.rs:486-491`) opens with
  `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`; the `FILE_SHARE_DELETE`
  is load-bearing — it means a lock-free reader does not block the writer's
  `MoveFileExW(REPLACE_EXISTING)` (`windows.rs:200-207`), and the replace is
  atomic, so the reader sees the old bytes to close (pending-delete) and the
  next reader sees the new file. Unix already reads this way
  (`load_oauth_store`, no lock) via `SecureDirectory::read` (`mod.rs:102`). So
  no shared-lock primitive is introduced. The `R | W`-only share mode is kept
  **solely for lock files** (the ownership deny-delete guard), untouched.
- **Writes (`update`/`save`) keep the bounded exclusive lock** (`lock_within`,
  D3) because the RMW read→compare→replace must be atomic across processes;
  that is the only place the lock earns its keep. Network token refresh stays
  **outside** every lock (unchanged — `load`/`save` are separate
  `CredentialStore` calls, and refreshes are rare).
- **Refresh-transaction correctness (D6-correctness, resolved).** rmcp 3.1.2
  issues refresh as `load` → token-endpoint HTTP → `save` with the network call
  unbracketed between two store episodes (`rmcp/src/transport/auth.rs:2168,
  :2189-2194, :2226`). **In-process it is already serialized** by `AuthClient`'s
  manager mutex (`transport/common/auth/streamable_http_client.rs:51`), so the race
  is purely **cross-process** (TUI + daemon sharing `mcp-auth.json`). Two
  cooperating mechanisms close it, neither holding a lock across network I/O:
  1. **Compare-before-write CAS in `save`** (`mcp.rs:657`): under the existing
     bounded `lock_within` episode, skip the write (return `Ok(())`) when the
     on-disk entry is strictly newer by `token_received_at`, or same-timestamp
     with a *different* refresh token; allow equal-token idempotent writes and
     `token_response: None` administrative downgrades. Persisted state can never
     regress to a rotated-away refresh token.
  2. **Pre-flight refresh claim in `McpOAuthHttpClient::execute`**
     (`mcp.rs:815`): for token-endpoint requests carrying
     `grant_type=refresh_token` (visible in the form body we already parse at
     `mcp.rs:782-812`), take a bounded lock, compare the request's refresh token
     against disk, and if disk has rotated, **short-circuit with a synthesized
     200 adopting the disk credentials** — no second IdP call, no spurious
     `NeedsAuth`. On a 400 `invalid_grant`, one bounded disk re-check adopts a
     concurrently-landed rotation the same way; unchanged disk passes the
     rejection through to rmcp's `TokenRefreshRejected` → re-login. Lock
     contention on the pre-flight **skips the check** (never fails the refresh);
     CAS remains the correctness layer.
  Residual race — both processes inside the token-endpoint window — costs at
  most one `invalid_grant`, recovered by disk-adoption; persistence corruption
  is eliminated in all interleavings. `McpOAuthHttpClient` gains a reference to
  the `OAuthCredentialFile` + key/binding (available at construction from
  `authorization_manager`, `mcp.rs:1796-1817`); rmcp stays a stock `=3.1.2`
  dependency. The request-sniffing (`grant_type=refresh_token` parse) is the
  most fragile element and carries a debug test that fails loudly if a future
  rmcp stops routing refresh through our client.

### D7 — Keep lock-taking store calls off async workers

Even bounded, a 5 s lock retry should not spin a tokio worker. Route the
synchronous store-mutating calls reached from async server routes (provider
connect/update/disconnect, catalog refresh commit, OAuth store writes) through
`spawn_blocking`, matching what the artifact store already does. This is
orthogonal to D3 and independently safe.

### D8 — Ephemeral per-run daemon token handed over stdout; delete the token file and its lock

The daemon bearer token is currently persisted at `daemon/token-v1` with a
load-or-create path whose Windows branch takes the only remaining
blocking lock in this class (`auth_token.rs:67`). The file exists for exactly
one reason: an *unrelated* client process needs to discover the same secret.

Decision: replace persistence with **per-run generation plus stdout handoff**.
The daemon generates a fresh random 32-byte token at startup, keeps it only in
memory (`Zeroizing<String>`), and prints it once on stdout for its supervisor;
the token file, `standard_token_path`, `load_or_create_token`, and
`.token-v1.lock` are deleted. The `auth_token` module is removed.

Mechanics:

1. **Ready line.** As the first stdout line, the daemon emits exactly one
   machine-readable frame:
   `daemon-ready {"url":"ws://127.0.0.1:<port>/ws","token":"<43-char base64url>"}`
   followed by the existing human banner. Parsers match the literal
   `daemon-ready ` prefix and ignore all other lines. The token is never
   echoed to stderr or logged after startup.
2. **Server validation is unchanged in kind.** The WebSocket handshake keeps
   the constant-time compare (`authorized(...)` / `ct_eq` in
   `service/websocket.rs`); only the token's *source* changes — a value passed
   into `WebSocketService` at construction instead of a file read at startup.
   The listener stays loopback-bound.
3. **Supervised clients** (the intended primary attach mode) receive the token
   over the private stdout/pipe channel from the spawner and connect via the
   existing `WebSocketTransport::connect_with_token` /
   `Client::connect_with_token` (already public today, used by tests). A pipe
   is readable only by its two endpoints, so this handoff is strictly stronger
   than the shared file: unrelated same-machine processes cannot observe it.
4. **Manual attach** (`cookie connect`, `cookie --attach` in another terminal)
   can no longer discover the secret *implicitly* — no file remains to read,
   and only a cooperating reader of the daemon's stdout can obtain the token.
   Token supply is a **required parameter with env fallback**, on every
   WS-attaching subcommand (`attach`, `connect`, `disconnect`, `mcp` — the
   four that currently build `Client::connect_websocket` with a `url`):
   `#[arg(long, env = "COOKIE_DAEMON_TOKEN")] token: String`. The `url` arg
   keeps its `default_value` (7419); the token gets no default, so clap's
   standard "required argument not provided" error is the missing-token UX —
   no bespoke prompt. Three legitimate delivery channels: (a) the daemon's
   parent reads the pipe; (b) a human reads the ready line on the terminal
   and pastes `--token` or exports `COOKIE_DAEMON_TOKEN`; (c) a launcher
   script captures stdout and relays it to clients it spawns.
   `WebSocketTransport::connect` (the file-reading default) is removed;
   tokenless `connect(url)` becomes `connect_with_token` only. Accepted
   consequence: a fully unattended client attaching to a daemon it did not
   start (e.g. a cron job) must carry the token in its own launch config; no
   global readable fallback exists any more.
5. **Port selection: static and ephemeral both supported (resolved).**
   `serve(port)` already binds any supplied port and reports the actual
   `local_addr()` (`service/websocket.rs:70`, banner at
   `cookie_agent/src/main.rs:1006`), so no mechanism is needed — only a
   policy:
   - `--port <N>` / `[server] port = N` → static bind (unchanged behavior).
   - `--port 0` / `port = 0` → ephemeral bind; the ready line carries the
     real port; this is the supported mode for supervisors, CI, and
     concurrent daemons.
   - **No configuration → default 7419 (unchanged).** Keeping the static
     default preserves (a) the bind collision as the sole "a daemon is
     already running" signal, and (b) argument-less `cookie connect`, which
     requires a predictable port; with the per-run token already imposing
     one paste step (the token), an ephemeral default would force a second
     mandatory paste (the URL).
   - Clients must never guess an ephemeral port: the ready-line URL is
     authoritative whenever it is used.

Security posture, stated honestly against [Security
boundaries](../guide/security.md):

- **Stronger than the file:** nothing on disk; the secret dies with the run;
  a leaked token cannot outlive the daemon; two daemons can never share a
  stale secret (today's file means one daemon's accepted client could
  authenticate to any other daemon started since the file was created).
- **Same residual boundary as today, minus one class:** a same-user process
  that can ptrace/inspect the daemon still wins (unchanged — that threat was
  never file-vs-pipe addressable); a same-user process that could simply
  *read the token file* can no longer get a durable secret unless it also
  captures the parent's pipe or the terminal output.
- **New, smaller caveats:** supervisors that log daemon stdout verbatim
  persist the token in logs — the ready line must be treated as secret by
  supervisors (spec: supervisors pipe stdout, capture one line, and do not
  archive it); a human pasting `--token` into shell history is a
  same-user-visible copy. Both are documented, not solved, here.

Effect on this spec's lock inventory: the daemon-token class **leaves the
locking problem entirely** — it contributes zero locks to D3 migration and
retires the last `directory.lock` call site outside the provider/catalog/
OAuth/snapshot set.

### D9 — Delete the one-shot v1→v2 session-store migration subsystem

Product decision: the hierarchical (v2) store was shipped under an explicit
no-backward-compatibility directive; the flat (`projects/<hash>`) layout is
not supported. The one-shot promotion machinery therefore retires in full —
**no migration code and no migration lock at all** — superseding the earlier
design question of whether the migration lock merges with the session
ownership lock (neither survives as a concern: the ownership lock stays;
the migration subsystem goes).

Deletion surface (verified):

- `crates/engine/src/migration.rs` — entire file and module declaration
  (2,698 lines: production body `:1-1176`, the rest tests). Includes the
  `MigrationLock` (`:1220-1253`), `OwnerLocks` batch claim/release with the
  Windows sidecar-move handling (`:1095-1169`, `d08d3292` era), the
  journal/resume machinery, verification passes, and the `MIGRATED`
  tombstone writer.
- One production call site: `session.rs:772`
  (`migration::run_if_needed` in `open_with_layout`, reached only via
  `LayoutChoice::PreferV2`, which is what `SessionStore::open` (`:742`) uses).
- `LayoutChoice` itself: with no promotion path, `PreferV2` collapses to
  "v2 always"; `ForceFlat` (`:752`) and its `flat_layout` branch
  (`session.rs:779-790`) exist only for migration fixtures — deleted, with
  their uses migrated to direct v2 paths in tests.
- `SessionError::Migration` variant: deleted outright. No replacement error
  is introduced — v1 is not detected, so it cannot be reported (see
  replacement behavior below).
- `layout.json`'s embedded `"migration": "complete"` field and the
  `.migrated` / `.migrating` marker semantics: new code neither reads nor
  writes them. The v2 `LAYOUT_MARKER_FILE` version marker **stays** — it is
  how a directory is identified as a v2 store.
- Existing migrated stores carry inert leftovers (`layout.json` migration
  field, possibly empty `projects/` parents). Not touched — no cleanup pass
  reintroduces migration-era filesystem dancing.
- Docs: `architecture.md` migration paragraphs (lock/journal/tombstone
  contract, the "first open promotes" description) are deleted, not replaced
  — no v1/v2 migration language remains anywhere in the site.

Replacement behavior: **none — v1 is treated as if it never existed**.
`SessionStore::open` unconditionally opens the v2 hierarchical store
(`sessions/<workdirkey>`). There is no legacy detection, no legacy error
variant, no tombstone, no read of `projects/` from any v2 code path.
`SessionStore::project_dir` (the `projects/<hash>` builder,
`session.rs:696`) and the `legacy_root`/`legacy_dir` computations in
`open_with_layout` (`:775-791`) are deleted; `project_hash` stays because v2
directory naming uses it. A leftover `projects/` directory on some disk is
simply another directory: ignored, never inspected, never mentioned in
docs. The consequence of that stance is recorded here for honesty: any
store that was never promoted now surfaces as an empty session list for
that work dir — accepted by directive.

Ripened side-effects:

- Windows CI: the migration test suite (which exercised deny-delete
  sidecars, resume journals, and long-watchdog paths) disappears, removing
  a whole class of the thread-parking suspects from the hang investigation.
  D3's bounded `lock_within` still lands for the stores that remain.
  (Separately: the `test_timeout` ×10 Windows multiplier has been removed in
  favor of the same 60 s budget as unix — the earlier "600 s watchdog"
  framing no longer applies.)
- `replace_windows_path_with_retry` (`session.rs:4527`) stays — it is used
  by regular session writes (`:4349,:4429,:4511,:4646`), independent of
  migration.
- The shared `AdvisoryClaim` primitive: with migration gone, two hand-rolled
  try-lock sites remain (`ownership.rs:124`, the artifact sweeps at
  `artifacts.rs:605,1317`) that duplicate the open + `try_lock_exclusive` +
  `Drop`-unlock + typed-`WouldBlock` shape. **Committed (Q5): consolidate.**
  Extract one helper with the two canonical forms — `try_lock_once()` (claim,
  no wait) and `lock_within(budget)` (D3's bounded retry) — into a shared home
  (next to `SecureDirectory`); `ownership.rs` layers `WriteAuthority` + the
  Windows deny-delete sidecar policy on top, artifact sweeps use
  `try_lock_once()` unchanged. Behavior-neutral: same paths, same share modes,
  same error classification. Lands inside the D3 PR.

## Non-goals

- **Adopting fs4.** Rejected: fs4 1.1.0 has no timeout API
  (`lock_for`/`lock_timeout` do not exist) and its `AsyncFileExt::lock` is
  documented-synchronous — a pure API-modernization migration with zero benefit
  to the hang or retention problems.
- **Adopting `filelock` (aisk).** Rejected for this pass: it is the only crate
  offering tokio-native async acquisition, but it is a very new, single-author,
  low-adoption dependency on a credential path, and its async acquisition still
  polls. D3 + D7 give the same reactor-safety with no new dependency.
- **Heartbeat / mtime-stale / PID-liveness / release-token / TTL files.**
  Rejected outright: our OS advisory locks auto-release on fd close and on crash,
  so these mechanisms — which exist only because `mkdir`-style locks survive
  crashes — would be dead weight.
- **SQLite migration.** Rejected: it does not remove the waiter-hang class
  (opencode's `busy_timeout` history) and it forfeits the filesystem trust model.
- **Shared lock modes.** Obsoleted by D6: reads are lock-free, so there is no
  reader to make shared; the lock exists only for writers' RMW atomicity.
- **`fcntl`/OFD locks.** Any lock we add must be `flock`-family so it
  interoperates with the existing locks on the same files. Do not mix lock
  families on one file.

## Rollout order

1. **D9** — delete the session-store migration subsystem + `architecture.md`
   cleanup (independent; unblocks the largest Windows-CI thread-parking
   class; no replacement code). Land first.
2. **D1** — delete snapshot-manifest locks (self-contained, largest lock
   win, lowest risk).
3. **D3 + D5 + D9-claim** — `lock_within` + `try_lock_once` shared primitive,
   contention error type + wire code; migrate provider-store (with a
   temporary wide hold retained), catalog, OAuth write sites; consolidate the
   ownership/artifact try-lock callers. Independent of Windows CI logs; safe
   to land alongside D9.
4. **D7** — `spawn_blocking` for store mutations from async routes.
5. **D4** — provider-store commit-CAS (depends on D3/D5 landing).
6. **D8** — per-run stdout token + delete `token-v1`/`auth_token.rs`/
   `.token-v1.lock` + update `security.md` and `run.md`/`server.md` (independent;
   product decision already made).
7. **D6-correctness** (CAS save + pre-flight refresh claim) — designed
   (above); lands with or just after the D3 OAuth migration since both touch
   `mcp.rs` store paths. Test plan: cross-process child-process race harness
   with a single-use-rotating fake IdP (extends `OAuthFixture`,
   `mcp/oauth_tests.rs:277-341`), plus in-process CAS unit tests.

## Resolved questions

- **D4 stale replay vs surface** — resolved: surface as retryable
  `Contention`, single automatic same-`client_request_id` resubmission at the
  client layer, no manager-internal replay. Rationale in D4.
- **D8 port policy** — resolved: default 7419 unchanged, `--port 0` as the
  supported ephemeral opt-in, ready-line URL authoritative. Rationale in D8.
- **D8 token UX** — resolved: `--token` required with `env` fallback on the
  four WS subcommands, no default, no interactive prompt. Mechanics in D8 §4.
- **Test watchdog** — resolved: the Windows `test_timeout` ×10 multiplier is
  removed; Windows and unix share the same 60 s budget.
- The earlier "authorization-less daemon" question is superseded by the
  per-run token design: authentication is **kept** (constant-time bearer
  compare, loopback bind), only the token's persistence and handoff change.

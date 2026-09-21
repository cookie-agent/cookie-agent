# Tree-Scoped Session Ownership Lock

Status: implemented on 2026-09-21. This document records the decision to move
cross-process session ownership from a per-session lock to a single lock per
root session tree. It amends the ownership rules described in
[Architecture](../architecture.md) and the session guarantees in
[Sessions](../guide/sessions.md); it does not touch the `~/.cookie-agent` store
locks covered by [Store lock retention](store-lock-retention.md).

## Problem

Ownership is keyed per session today. `SessionStore` carries
`ownership: HashMap<SessionId, StoreOwnership>`
(`crates/engine/src/session/mod.rs`, around lines 216 and 255), and *every*
session — each root and each delegated child — acquires a lock of its own. A
lock is taken at publication (the fork path around lines 1367-1395, the
buffered-publish path around lines 1446-1478) or at adoption
(`begin_write_locked`, around line 833).

On unix that lock is `<session-dir>/owner.lock`. On Windows it is a
`<session-id>.owner.lock` sidecar placed *next to* the session directory
(`crates/engine/src/ownership.rs`, `owner_lock_path`), because a handle held
inside the directory would block the publication rename. A tree with N children
therefore litters `<root>/subagents/` with N sidecars, plus one beside the root.

The per-child locks buy nothing:

- A child never has a different writer than its root. Delegation only creates
  and resumes children from a process that already owns the parent.
- A tree is loaded, folded, reconciled, and published as a unit.
- Residency and eviction already retain locks for the lifetime of the process,
  so the per-child locks are never released early anyway.

## Decision

- **One lock per tree.** A root tree is guarded by exactly one ownership lock:
  `<root-dir>/owner.lock` on unix, and a `<root-id>.owner.lock` sidecar beside
  the root directory on Windows. The Windows sidecar stays a sidecar so the
  temporary-directory rename at publication is not blocked by the open handle.
  Children have no lock file of any kind. Holding a root's lock guarantees that
  only its holder writes anywhere under that root's directory, children
  included.
- **The ownership map is keyed by root.** `SessionStore.ownership` holds
  `TreeOwnership::{PendingPublish { authority }, Adopting { lock, authority },
  Owned { lock, authority }, Foreign}` (the `StoreOwnership` enum is renamed).
  There is one `WriteAuthority` per tree, and every `EventLog::open_owned` in
  the tree is handed that tree's capability, so dropping the tree lock
  invalidates every log in the tree at once.
- **Per-session adoption bookkeeping stays.** A session is writable only when
  its tree is owned by this process *and* the session was either created in
  this process or adopted in it — reconciled by `Engine::reconcile_session` and
  committed. `WriteOpen::{AlreadyOwned, Adopting}` keeps its per-session
  meaning; the created/adopted sessions are tracked in a per-store set beside
  the tree map.
- **`begin_write(id)`** resolves `root = root_of(id)` first. If the tree is
  unowned it `try_acquire`s the root lock; a foreign or unreadable lock is
  cached per root and reported as `SessionLocked(id)` for every session in that
  tree, exactly as the per-session cache does today. If the tree is already
  owned there is no lock activity at all. The per-session step follows: a
  created or adopted session reports `AlreadyOwned` (reopening its log as
  today), and any other session has its log opened for adoption and reports
  `Adopting`. Acquiring the tree lock through a child does **not** reconcile the
  root; the root reconciles when it is first written, through its own adoption.
- **Publication.** A new root or a fork of a root acquires the root lock as it
  does today — inside the temporary directory before the rename on unix, from
  the sidecar path derived from the final directory on Windows. Publishing a
  child creates no lock of its own; it requires the tree to be `PendingPublish`
  or `Owned` by this process and fails with `SessionLocked` otherwise. A *fork*
  of a delegated session is the one publication that can be the first write in a
  tree — nothing else in the tree has to be open for it — so it acquires the
  root lock when the tree is free, the same way an adoption reached through a
  child does, and fails as foreign-owned when it is not. Its temporary directory
  is prepared inside the root's directory, so the lock is taken before it is
  created.
- **`is_owned(id)`** is answered from the tree of `id`, *and* from the
  per-session record: it is true when the tree is held and settled here and `id`
  was created or adopted here. That keeps its meaning at the call sites that use
  it as a write gate, while a foreign root answers `false` for every session in
  its tree. `release_ownership` drops every tree lock the store holds. Eviction
  is unchanged: a tree lock is retained for the lifetime of the process even
  after every session in the tree has been evicted.
- **Legacy child lock files** (`<child>/owner.lock` and
  `<child-id>.owner.lock`) are ignored. The tree's single load pass removes them
  best-effort. There is no migration and no compatibility decoding.
- **Errors are unchanged.** `SessionError::SessionLocked`,
  `EngineError::SessionOwnedByAnotherProcess`, and the server's error mapping
  keep their current shape; a foreign root simply makes every session in its
  tree report that error.

## Non-goals

Cross-tree locks, work-dir-level locks, any change to the artifact or
provider-store locks (see [Store lock retention](store-lock-retention.md)), and
any change to adoption reconciliation semantics.

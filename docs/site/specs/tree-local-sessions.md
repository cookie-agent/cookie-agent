# Tree-Local Sessions

Status: approved 2026-09-25; implementation in progress. This document amends
the startup, listing, delegation, approval, producer, and artifact rules in
[Architecture](../architecture.md), removes two usage methods from the
[Protocol](../reference/protocol.md), and extends the
[Tree-scoped session ownership lock](tree-ownership-lock.md) decision from
ownership to all session state.

## Problem

Launching `cookie` in a project with 22 root sessions (13.4 MB of root logs)
takes 2.9 s of single-threaded CPU before the first frame; the same binary in a
project without sessions takes 0.8 s. The difference is `Engine::open`
replaying the same root logs again and again: 91 folds of 24 distinct logs in
one launch, plus a background scan of every root log right after it.

| Step | Cost | Reads |
|---|---|---|
| Session discovery (`refresh_discovered_roots`) | 0.37 s | every root log, to compute usage |
| `DelegationEventStore::open` | 0.39 s | every root log |
| `rebuild_approvals` | 0.37 s | every root log |
| `rebuild_delegation_registry` → `parent_run_facts` | 1.00 s | the parent's log once per delegation |
| Plugin producer reconciliation (background) | — | every root log, even with no plugins |

None of it is needed to list sessions. `session.list` returns `SessionMeta`,
which each root's `metadata` file already holds. The folds exist to serve
engine-wide state: usage summaries for `agent.usage` and `usage.global`, a
delegation index over every tree, grants from every tree, producer state from
every tree, and a cross-tree artifact index. The same pattern appears outside
startup: a session revert rescans every root log to rebuild grants.

## Principle

**Tree isolation.** Every piece of session state belongs to exactly one root
session tree, is stored inside that tree's directory, and is loaded only when
that tree is loaded. No code path reads, scans, indexes, or waits on another
tree's data. The only work-dir-level session operation is listing root
`metadata` files.

A tree is *loaded* when it is first used — resume, open for mutation, fork
source, `session.get`, `session.tree`, `session.children`, `session.usage`,
`session.tree_usage`, or any child access — through the existing single bulk
pass. Nothing else counts as use.

## Requirements

### A. Remove cross-session rollups

- **A1.** Remove the `agent.usage` and `usage.global` RPC methods: method
  names, server and client trait methods, `AgentUsageParams`,
  `AgentUsageResult`, `GlobalUsageParams`, `GlobalUsageResult`, server routes,
  and `Engine::agent_usage` / `Engine::global_usage`.
- **A2.** Remove per-agent usage: the `agent_usage` map in `SessionProjection`
  and `SessionSummary` and its fold. Session and tree totals are a separate map
  and do not change.
- **A3.** Bump `PROTOCOL_VERSION` from 20 to 21 together with every documented
  version reference (see `AGENTS.md`), and remove both methods from the
  protocol reference and usage guide. Regenerate bindings and schemas.
- **A4.** Existing `subagents/index.json` files that still carry
  `agent_usage` remain readable; the field is ignored.
- **A5.** `session.usage` and `session.tree_usage` return the same totals and
  per-model rows as before; the TUI `/usage` panel and bottom-bar cost do not
  change.

### B. Startup and listing read no event logs

- **B1.** `Engine::open` opens no `events.jsonl` and no `subagents/index.json`.
  Session discovery is a directory scan.
- **B2.** `session.list` parses each root's `metadata` once and returns the
  same `SessionMeta` as today. It never folds a log, including for roots first
  discovered after startup, and does no usage, delegation, approval, or
  producer work.
- **B3.** `metadata` handling is unchanged: a root without `metadata` is
  skipped silently and retried on the next listing; a corrupt file or an ID
  that disagrees with its directory reports a diagnostic and is skipped.
- **B4.** Sessions created by other processes appear on the next
  `session.list`; listing takes no locks.
- **B5.** The store's set of known sessions is separate from usage summaries. A
  session can be known without any of its logs having been read; only sessions
  whose logs were folded carry usage.

### C. Delegation, approvals, and producers are per tree

- **C1.** The engine keeps no work-dir-wide delegation index. A tree's
  delegation records are added by that tree's load pass, from the root log and
  every descendant log (a record lives in its parent's log).
- **C2.** Every consumer of delegation records ensures the tree is loaded
  first: adoption recovery, missing-child recovery, the resume-a-prior-child
  check, run cancellation cascading to children, `session.children`, mailbox
  delivery, and the registry rebuild.
- **C3.** The delegation registry is rebuilt once per tree load, with parent
  facts (including a root parent's) supplied by that load pass. No parent log
  is re-read per delegation.
- **C4.** Restart-stable tree grants are installed during the tree's load,
  before any approval decision in that tree. The startup `rebuild_approvals`
  scan is removed.
- **C5.** Crash recovery is unchanged: interrupted runs and nonterminal
  background delegations are repaired when a tree is adopted, including the
  bounded resume wait. Startup performs no recovery today
  (`recover = false`), so nothing moves out of startup.
- **C6.** A tree load reads each log in the tree at most once.
- **C7.** Plugin producer reconciliation runs per tree: during a tree's load
  pass and for already-loaded trees. A producer change reconciles only the
  loaded trees it concerns. With no producer plugin configured it does
  nothing.
- **C8.** Grant invalidations move from the work-dir `grant-invalidations.jsonl`
  into each root's directory and load with that tree. A session revert rebuilds
  grants only for its own tree.
- **C9.** Delegation runtime state (records, admission queue, inflight
  bookkeeping, recovery claims) is keyed by root; every lookup, promotion, and
  limit is evaluated within one root. Background concurrency is already per
  root (`background_slot_unavailable`).
- **C10.** `delegation.max_resident_subagents` applies per tree: idle-eviction
  candidates are chosen within the tree whose resident child count exceeds the
  cap.

### D. Artifacts are per tree

- **D1.** Artifact reads resolve only inside the reading session's own tree.
  The process-wide digest index, the directory-scan fallback into other trees,
  and the `cross-refs.jsonl` ledger are removed.
- **D2.** A fork copies every artifact its copied history references into the
  new tree's `artifacts/` (hard-linking where the filesystem allows) before the
  fork is published, so a forked tree never depends on its source.
- **D3.** Every artifact write is attributed to a tree; the `artifacts.shared/`
  store is removed. Production writes are already routed through the tree
  resolver installed by `Engine::open`, which always yields a root.
- **D4.** Garbage collection stays per tree and needs no information from any
  other tree.

## Out of scope

- The fixed ~0.7 s of catalog loading (0.19 s) and model-manager compilation
  (0.47 s), tracked separately.
- Event-log and `metadata` formats.
- The TUI tree panel for an unloaded root, which keeps reading that root's
  `subagents/index.json` only when the tree is shown.
- Existing on-disk `artifacts.shared/`, `cross-refs.jsonl`, and work-dir
  `grant-invalidations.jsonl` data: breaking changes are acceptable, and old
  cross-tree references stop resolving.

## Verification

- `Engine::open` and `session.list` open zero event logs (store log-open
  counter) in a work dir containing roots with delegations, grants, and
  producer state.
- A tree load opens each of its logs exactly once and opens no file under any
  other root.
- Lazy consumers work after a cold start: resuming a prior child, run
  cancellation cascading to children, `session.children`, adoption recovery of
  a nonterminal background delegation, missing-child recovery, and a
  pre-restart grant applying after its tree is resumed.
- A grant invalidation survives a restart when only its own tree is loaded.
- Producer reconciliation with no producer plugin opens no log.
- A fork's artifacts still resolve after the source tree is deleted.
- `session.usage` and `session.tree_usage` totals are unchanged; `agent.usage`
  and `usage.global` return method-not-found.
- Launching in the 22-root project takes about as long as in an empty project.

## Implementation order

1. Rollup removal and the protocol bump (A).
2. Metadata-only discovery and listing, with the known-session set separated
   from summaries (B).
3. Per-tree delegation index, registry, grants, grant journal, producers, and
   delegation runtime state (C1–C9).
4. Per-tree artifacts and fork copying (D).
5. Per-tree residency cap (C10).

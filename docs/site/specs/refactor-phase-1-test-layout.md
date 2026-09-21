# Refactor Phase 1: Test Layout and Build Hygiene

Status: implemented. Part of the architecture refresh recorded in [Refactor phase 1](refactor-phase-1-test-layout.md), [phase 2](refactor-phase-2-dependencies-ci.md), and [phase 3](refactor-phase-3-module-splits.md).

Repository: this workspace (paths below are relative to the workspace root).
Branch: `refactor/phase-1-tests-and-build-hygiene` (from `main`)

Hard constraints
- Pure reorganisation. No production behavior change. No test body edits beyond `use` paths, module paths, and visibility.
- Test counts must not change: `cargo test -p cookie_agent_engine --lib -- --list | grep -c ': test$'` is 637 before and after; `cargo test -p cookie_agent_tui --lib -- --list | grep -c ': test$'` is 535 before and after. Check every crate you touch the same way.
- Follow AGENTS.md: conventional commit subjects (`refactor(engine): ...`), locked builds, fmt + clippy `-D warnings` on stable and `+1.88.0`.
- Keep insta snapshots byte-identical. Snapshot files move with their tests; only their file names may change when a module path changes.

## Part A: engine test relocation

### A1. Move `runtime_tests.rs` and its six child files into `crates/engine/src/runtime/tests/`

Today `crates/engine/src/lib.rs` declares `#[cfg(test)] mod runtime_tests;` and `runtime_tests.rs` (24.5k lines, 178 tests) declares six `#[path]` children: `producer_runtime_tests` (which itself has child `goal_reminder_runtime_tests`), `producer_plugin_tests`, `producer_discard_tests`, `delegation_revert_tests`, `goal_selection_tests`, `messaging_tests`.

Target layout:

```
crates/engine/src/runtime/tests/
  mod.rs            // #[cfg(test)] root: `mod support; mod admission; ...` plus nothing else
  support.rs        // shared fixtures moved verbatim from runtime_tests.rs:
                    //   pub(crate) struct Fixture, private_tempdir, create_private_test_dir,
                    //   write_private_test_file, copy_private_test_tree, python_command,
                    //   test_timeout, TestFlag, PanicResistantTempDir, test_turn_context,
                    //   every Test*Provider / Test*Executor / OrderedToolDefinitionProvider, etc.
  <topic>.rs        // one file per topic, each `use super::support::*;` (and whatever else it needs)
  producers.rs               // was producer_runtime_tests.rs
  goal_reminders.rs          // was goal_reminder_runtime_tests.rs (child of producers.rs today; make it a sibling)
  producers_plugin.rs        // was producer_plugin_tests.rs
  producers_discard.rs       // was producer_discard_tests.rs
  delegation_revert.rs       // was delegation_revert_tests.rs
  goal_selection.rs          // was goal_selection_tests.rs
  messaging.rs               // was messaging_tests.rs
```

Declaration: in `crates/engine/src/runtime.rs` add `#[cfg(test)] mod tests;` (Rust resolves `runtime/tests/mod.rs` automatically). Remove `mod runtime_tests;` from `lib.rs`. Delete the seven old files.

Topic split of the 178 tests currently in `runtime_tests.rs`: group by what the test name and body exercise. Suggested files (create only the ones that get tests; add others if a clear cluster emerges): `admission.rs`, `approvals.rs`, `artifacts.rs`, `compaction.rs`, `delegation.rs`, `mailbox.rs` (steer / cancel / stdin / subscribe), `model_loop.rs` (retry, fallback, attempts), `permissions.rs`, `plugins.rs`, `recovery.rs`, `sessions.rs` (create / resume / revert / fork / rename), `skills.rs`, `tool_execution.rs`, `usage.rs`, `misc.rs` for the remainder. No file over ~3000 lines. Test function names are unchanged.

Anything in `runtime_tests.rs` that is referenced from non-test code paths via `pub(crate)` (e.g. `Fixture`) keeps `pub(crate)` in `support.rs`; grep for `runtime_tests::` across the crate and fix every path.

### A2. Collapse the 26 `#[cfg(test)]` hook fields on `Inner` into one struct

New file `crates/engine/src/runtime/test_hooks.rs`, whole file `#![cfg(test)]` (declare with `#[cfg(test)] pub(crate) mod test_hooks;` in `runtime.rs`).

Move these type definitions there from `runtime.rs` (lines ~594-712 today, all already `#[cfg(test)]`): `PromptSnapshotHook`, `PagingRaceHook`, `ToolProgressAppendBlock`, `ReadOnlyReopenHook`, `ApprovalEvaluationHook`, `ModelRetrySleepMode`, `ModelRetrySleepHook` (+ its impl), `AdmissionConfirmationHook`, `ResumeAdmissionHook`, `AdmissionBlockingHook`, `AbandonedSweepHook`. Give them `pub(crate)` visibility.

```rust
#[derive(Default)]
pub(crate) struct TestHooks {
    pub(crate) prompt_snapshot_hook: Mutex<Option<Arc<PromptSnapshotHook>>>,
    pub(crate) prompt_before_claim_hook: Mutex<Option<Arc<PromptSnapshotHook>>>,
    pub(crate) janitor_before_barrier_hook: Mutex<Option<Arc<PagingRaceHook>>>,
    pub(crate) compaction_execution_hook: Mutex<Option<Arc<PagingRaceHook>>>,
    pub(crate) read_only_reopen_hook: Mutex<Option<ReadOnlyReopenHook>>,
    pub(crate) approval_evaluation_hook: Mutex<Option<Arc<ApprovalEvaluationHook>>>,
    pub(crate) model_retry_sleep_hook: ModelRetrySleepHook,
    pub(crate) pending_approval_ready: tokio::sync::Notify,
    pub(crate) admission_confirmation_hook: Mutex<Option<Arc<AdmissionConfirmationHook>>>,
    pub(crate) resume_admission_hook: Mutex<Option<Arc<ResumeAdmissionHook>>>,
    pub(crate) resume_attachment_hook: Mutex<Option<Arc<ResumeAdmissionHook>>>,
    pub(crate) skill_fork_reservation_hook: Mutex<Option<Arc<PagingRaceHook>>>,
    pub(crate) producer_wake_hook: Mutex<Option<Arc<PagingRaceHook>>>,
    pub(crate) delegation_reservation_hook: Mutex<Option<Arc<PagingRaceHook>>>,
    pub(crate) resume_rollback_hook: Mutex<Option<Arc<ResumeAdmissionHook>>>,
    pub(crate) admission_blocking_hook: Mutex<Option<AdmissionBlockingHook>>,
    pub(crate) abandoned_sweep_hook: Mutex<Option<AbandonedSweepHook>>,
    pub(crate) plugin_diagnostic_append_block: Mutex<Option<Arc<tokio::sync::Notify>>>,
    pub(crate) tool_progress_append_block: Mutex<Option<Arc<ToolProgressAppendBlock>>>,
    pub(crate) publication_failure: AtomicBool,
    pub(crate) delegate_start_failures: AtomicU64,
    pub(crate) delegate_start_failure_observed: tokio::sync::Notify,
    pub(crate) delegate_terminal_append_failures: AtomicU64,
    pub(crate) run_setup_append_failures: AtomicU64,
    pub(crate) resume_monitor_failures: AtomicU64,
    pub(crate) adoption_reconcile_failures: AtomicU64,
}
```

Field names are kept exactly so the change is a prefix rewrite. `Inner` gets a single `#[cfg(test)] pub(crate) test_hooks: TestHooks,` and every `self.inner.<hook>` / `inner.<hook>` becomes `self.inner.test_hooks.<hook>`. If `ModelRetrySleepHook::default()` is not derivable, implement `Default` for `TestHooks` by hand. `Engine::open` initialises the field with `TestHooks::default()`.

### A3. Move every inline test module of 500+ lines into its own file (workspace-wide)

Rule: if a `#[cfg(test)] mod tests { ... }` (or similarly named) block at the end of `src/<name>.rs` is 500 lines or more, cut its body into `src/<name>/tests.rs` and replace the block with `#[cfg(test)] mod tests;`. The module path is unchanged, so `use super::*;` keeps working. If `<name>.rs` already has a `<name>/` directory (e.g. `session/projection.rs`, `runtime/compaction/tests.rs`), place the file there; where a `tests.rs` already exists, name the new one after its module (e.g. `windows_tests.rs`) and keep the declaration.

Known candidates (line counts of the test tail today): engine `session.rs` 3449 (two modules: `tests` and `windows_tests`), `events.rs` 3247, `model_history.rs` 2571, `permissions.rs` 1632, `plugin.rs` 1091, `goal_projection.rs` 821, `runtime/output_capture.rs` 654, `runtime/artifacts.rs` 593, `runtime/compaction.rs` 550; tui `state/mod.rs` 2982 (becomes `state/tests.rs`), `markdown.rs` 1194, `ui/input.rs` 831, `ui/goal.rs` 814, `lib.rs` 793 (becomes `src/tests.rs`), `theme.rs` 745, `ui/management.rs` 571, `ui/pickers.rs` 507; protocol `session/client.rs` 1196 (already has `session/client/tests.rs`, so name it accordingly or merge if it is the same module); plugin_sdk `server.rs` 1109; cookie_agent `main.rs` 1124 (it is a binary; `src/tests.rs` declared from `main.rs`); models `manager/mod.rs` 911; tools `read.rs` 861, `lib.rs` 498 (skip, under threshold), `edit.rs` 493 (skip). Re-derive the list with a script rather than trusting these numbers.

Insta: snapshot files live in `snapshots/` next to the source file that contains the test. Moving a test module into a subdirectory moves its expected snapshot directory. `git mv` the affected `.snap` files, then run `cargo insta test --workspace --unreferenced=reject` (or `cargo insta test` followed by `cargo insta pending-snapshots`) and confirm zero new or changed snapshots. Snapshot contents must not change.

### A4. TUI `transcript.rs` test split

`crates/tui/src/ui/transcript.rs` has 140 tests in `mod tests` from line ~4789 to the end (20.4k lines). Target:

```
crates/tui/src/ui/transcript/tests/
  mod.rs        // `mod support; mod layout; ...`
  support.rs    // shared helpers / fixtures currently at the top of `mod tests`
  <topic>.rs    // e.g. layout.rs, scrolling.rs, assistant.rs, tool_rows.rs, diffs.rs, read_output.rs,
                //      approvals.rs, pickers.rs, selection.rs, goal.rs, producers.rs, misc.rs
```

Declare with `#[cfg(test)] mod tests;` in `transcript.rs`. Keep the small `#[cfg(test)]` helper functions that live in the production part of `transcript.rs` (e.g. `transcript_layout_with*`) where they are. The 10 insta snapshots named `cookie_agent_tui__ui__transcript__tests__<name>.snap` become `cookie_agent_tui__ui__transcript__tests__<topic>__<name>.snap`; rename them with `git mv` into `crates/tui/src/ui/transcript/tests/snapshots/`, then verify with `cargo insta test` that nothing is new or changed. No test file over ~3000 lines.

`ui/app.rs` keeps `#[path = "fallback_tests.rs"] mod fallback_tests;` as is (already external). Its tiny `post_teardown_tests` module may stay inline.

## Part B: build hygiene (item 7)

B1. Workspace `Cargo.toml`: add

```toml
[profile.dev]
debug = "line-tables-only"
```

(keep `[profile.dist]` as is; `test` inherits `dev`). Do not add `split-debuginfo`.

B2. Delete the two untracked stray build directories: `vendor/syntect/target` (788 MB) and `vendor/bincode-compat/` (only contains `target/`, 99 MB, not tracked by git). Confirm with `git status --short vendor` that nothing tracked is affected.

B3. After B1 and after all of Part A is committed and verified, run `cargo clean` once in the workspace root (the target dir is 649 GB; the profile change makes the old artifacts stale anyway). Then run the full local gate one final time from a cold cache so the final push is verified against what CI will build. Report the new `du -sh target` after the cold gate.

## Commits (suggested order)
1. `refactor(engine): move runtime tests under runtime/tests and split by topic`
2. `refactor(engine): collect Inner test hooks into TestHooks`
3. `refactor: move large inline test modules into sibling files` (may be several commits per crate)
4. `refactor(tui): split transcript tests by topic`
5. `build: emit line-tables-only debuginfo in dev profile`

## Local gate (run before every push)
```sh
cargo fmt --all -- --check
cargo build --locked --workspace --all-targets
cargo +stable clippy --locked --workspace --all-targets -- -D warnings
cargo +1.88.0 clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo doc --locked --workspace --no-deps   # with RUSTDOCFLAGS=-D warnings
cargo deny --locked check advisories licenses sources
cargo insta test --workspace   # zero new/changed snapshots
```
(`crates/protocol/scripts/check-bindings.sh --check` is unaffected by this phase; run it only if you touch `crates/protocol`.)

## Milestone
Push the branch, wait for the GitHub Actions `CI` workflow on the branch to be green (`gh run list --branch <branch>`; `gh run watch <id> --exit-status --interval 60`, repeatedly if it exceeds a tool timeout; a run takes ~50 min). Fix and re-push on failure. When green: `git checkout main && git merge --ff-only <branch> && git push origin main`, then wait for the `CI` run on `main` to be green as well. Report: main SHA, CI run URLs, before/after test counts per crate, the largest remaining file sizes, and `du -sh target`.

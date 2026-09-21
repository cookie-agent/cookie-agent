# Refactor Phase 3: Module Splits

Status: implemented on 2026-09-21. Part of the architecture refresh recorded in [Refactor phase 1](refactor-phase-1-test-layout.md), [phase 2](refactor-phase-2-dependencies-ci.md), and [phase 3](refactor-phase-3-module-splits.md).

Repository: this workspace (paths below are relative to the workspace root).
Branch: `refactor/phase-3-split-monoliths` (from `main`, after phase 2 has merged)

Hard constraints
- No behavior change, no public API change for downstream crates (`server`, `tools`, `cookie_agent` keep compiling without edits except import paths if a `pub` item's module moves; prefer re-exports so they need none).
- Tests: no body edits beyond paths and renamed field accesses. Test counts unchanged.
- One commit per sub-part (3A..3E), each leaving the workspace green on the full local gate.
- AGENTS.md rules (conventional commits, locked builds, fmt/clippy on stable + 1.88).

## 3A. Engine `Inner`: group state by subsystem

`crates/engine/src/runtime.rs` `pub(crate) struct Inner` has 82 fields (52 mutexes). Keep the shared infrastructure flat and move per-subsystem state into structs owned by the module that already implements that subsystem. Field names drop their subsystem prefix. All new structs are `pub(crate)`, `#[derive(Default)]` where every member is defaultable, constructed in `Engine::open`.

Stays flat on `Inner`: `config`, `config_store`, `artifacts`, `store`, `delegation_events`, `grant_journal`, `model_manager`, `published_runtime`, `runtime_mutation`, `runtime_notifications`, `engine_events`, `manifest_store`, `tools`, `provider_ids`, `mcp`, `plugins`, `mcp_mutation`, `permissions`, `skills` (the registry), `runtime`, `runtime_revision_index`, `mutation_locks`, `janitor_task`, `#[cfg(test)] test_hooks`.

New groups:

```rust
// runtime/delegation.rs
pub(crate) struct DelegationState {
    pub(crate) inflight: Mutex<HashMap<InvocationId, HashMap<u64, InflightDelegation>>>,   // was inflight_delegations
    pub(crate) by_session: Mutex<HashMap<SessionId, DelegationRecord>>,                    // was delegations_by_session
    pub(crate) queue: Mutex<VecDeque<SessionId>>,                                          // was delegation_queue
    pub(crate) admission: tokio::sync::Mutex<()>,                                          // was delegation_admission
    pub(crate) reconciliation_running: AtomicBool,
    pub(crate) reconciliation_requested: AtomicBool,
    pub(crate) recovery_stale_producers: Mutex<Vec<(SessionId, InvocationId, ProducerId)>>,
    pub(crate) next_admission_generation: AtomicU64,
    pub(crate) admission_tasks: Mutex<Vec<JoinHandle<()>>>,
    pub(crate) admission_blocking_tasks: Mutex<Vec<JoinHandle<()>>>,
    pub(crate) admission_tasks_closing: AtomicBool,
    pub(crate) recovery_waiters: Mutex<HashSet<(SessionId, RunId, ToolCallId)>>,
}

// runtime/approval_flow.rs
pub(crate) struct ApprovalRuntimeState {
    pub(crate) store: ApprovalStore,                                                        // was approvals
    pub(crate) pending: Mutex<HashMap<(SessionId, ApprovalId), PendingApproval>>,           // was pending_approvals
    pub(crate) permission_modes: Mutex<HashMap<SessionId, PermissionMode>>,
    pub(crate) permission_overlay_mutation: tokio::sync::Mutex<()>,
}

// runtime/skills.rs
pub(crate) struct SkillRuntimeState {
    pub(crate) grants: Mutex<HashMap<SessionId, BTreeMap<String, SkillGrantOverlay>>>,      // was skill_grants
    pub(crate) models: Mutex<HashMap<SessionId, ModelKey>>,                                 // was skill_models
    pub(crate) pending_forks: Mutex<HashMap<ToolCallId, PreparedSkillInvocation>>,          // was pending_skill_forks
    pub(crate) pending_child: Mutex<HashMap<SessionId, PreparedSkillInvocation>>,           // was pending_child_skills
    pub(crate) direct_calls: Mutex<HashSet<ToolCallId>>,                                    // was direct_skill_calls
}

// runtime/output_capture.rs
pub(crate) struct OutputState {
    pub(crate) hubs: Mutex<HashMap<ToolCallId, OutputHub>>,                                 // was output_hubs
    pub(crate) captures: Mutex<HashMap<ToolCallId, OutputCapture>>,                         // was output_captures
    pub(crate) finalized_hubs: Mutex<VecDeque<ToolCallId>>,                                 // was finalized_output_hubs
}

// runtime/compaction.rs
pub(crate) struct CompactionState {
    pub(crate) in_progress: Mutex<HashSet<SessionId>>,                                      // was compaction_in_progress
    pub(crate) deferred: Mutex<HashMap<SessionId, VecDeque<SessionCommand>>>,               // was compaction_deferred
    pub(crate) context_token_estimators: Mutex<HashMap<SessionId, ContextTokenEstimator>>,
}

// runtime/mailbox.rs
pub(crate) struct SessionRuntimeState {
    pub(crate) actors: Mutex<HashMap<SessionId, SessionActor<SessionCommand>>>,
    pub(crate) active: Mutex<HashMap<RunId, Arc<ActiveRun>>>,
    pub(crate) producers: Mutex<HashMap<SessionId, producers::SessionProducers>>,
    pub(crate) residency_mutation: tokio::sync::Mutex<()>,
}

// plugin.rs (or runtime.rs if the accumulator type lives there)
pub(crate) struct PluginDiagnosticsState {
    pub(crate) accumulator: Arc<PluginDiagnosticAccumulator>,                               // was plugin_diagnostics
    pub(crate) task: Mutex<Option<JoinHandle<()>>>,                                         // was plugin_diagnostic_task
}
```

`Inner` then holds `pub(crate) delegation: DelegationState`, `approvals: ApprovalRuntimeState`, `skills_runtime: SkillRuntimeState`, `output: OutputState`, `compaction: CompactionState`, `sessions: SessionRuntimeState`, `plugin_diagnostics: PluginDiagnosticsState`. Every access is a mechanical rewrite (`self.inner.delegation_queue` -> `self.inner.delegation.queue`). Also move `ContextTokenEstimator`, `PredictiveCompactionInput` and `CompactionDeferredKind` to `runtime/compaction.rs`; `PendingApproval`, `ApprovalOutcome`, `PreparedApprovalInvalidation`, `ApprovalTerminal`, `ApprovalEvaluationTransition`, `ApprovalToolInput`, `ModelApprovalInput` to `runtime/approval_flow.rs`; `InternalAgent*` types to `runtime/internal_agents.rs`; `PluginDiagnostic*` types to `plugin.rs` or a new `runtime/plugin_diagnostics.rs`. Box the largest `SessionCommand` variants so the `#[allow(clippy::large_enum_variant)]` can go. Target: `runtime.rs` under ~1200 lines and `Inner` under 40 fields.

## 3B. TUI `App`: split the 8k-line impl block into a module directory

Move `crates/tui/src/ui/app.rs` to `crates/tui/src/ui/app/mod.rs` (fix the two `#[path]` children: `#[path = "../goal.rs"] mod goal;` and `#[cfg(test)] #[path = "../fallback_tests.rs"] mod fallback_tests;`, or move those files into `app/` and drop the `#[path]`s). Keep `pub struct App` and its constructor, `run_with_client`, `run_with_new_session`, the terminal loop, and `impl Drop for App` in `mod.rs`. Move the remaining `impl App` methods into sibling files by concern, each as `impl App { ... }` with `pub(super)` methods:

```
ui/app/
  mod.rs          // struct App, construction, run loop, Drop, teardown messages
  keys.rs         // key/mouse event dispatch: handle_key, handle_mouse, is_* key predicates, edit_*_input
  composer.rs     // composer / input state, submit, steer, drafts, RunSelection handling
  approvals.rs    // approval panel state + rendering helpers (approval_*_hits, render_approval_actions, decision_tone)
  agents.rs       // agent panel / session tree: collect_tree_session_ids, find_node*, patch_tree_node_*, agent cycling
  sessions.rs     // session search rows cache, session picker, titles (collect_known_titles, title_change_from_event)
  pickers.rs      // agent/model picker rows (agent_picker_row, model_picker_row) — or fold into ui/pickers.rs if that already owns them
  draw.rs         // draw, draw_for_test, layout composition, render_connect_button, centered, inner_rect
  refresh.rs      // runtime.changed refresh scheduling (the `dirty` / `scheduled` / `in_flight` state machine)
```

Free functions at the bottom of today's `app.rs` (`latest_resolved_model_key`, `shorten_home`, `client_run_id`, `client_response_id`, `contains`, ...) go with the file that uses them most. No file over ~2000 lines. `ui/mod.rs` keeps `pub use app::{App, run_with_client, run_with_new_session};`.

## 3C. TUI transcript renderer: split `transcript.rs` (production part, ~4.8k lines)

Move to `crates/tui/src/ui/transcript/mod.rs` and split:

```
ui/transcript/
  mod.rs          // ScrollbarGeometry, ConversationScroll, BlockRegion, ScrollAnchorPoint, LayoutCache and the cached-layout key/entry structs,
                  // TranscriptRenderContext, the `impl App` entry points, transcript_layout_with* test helpers
  items.rs        // transcript_item_layout, item_layout_key*, item_block_ids, item_interaction*, item_is_live, for_each_item_block_id, Role
  assistant.rs    // assistant_item_layout, assistant_part_layout_key, splice_active_assistant_part, assistant_child_layout,
                  // assistant_header / attribution_line / assistant_footer_line / assistant_body_line / thinking_body_lines / format_thinking_duration
  tool_rows.rs    // tool_icon, tool_row_*, tool_header_*, abbreviate_tool_argument, tool_header_title, tool_child_layout,
                  // ParsedToolArguments, display_tool_arguments, BlockKey, ToolBodyLine(Kind), RenderBudget, SectionRenderer,
                  // output_section_limits, tool_body_lines, ToolBlockLayout, tool_block_lines, header_gutter_columns
  output.rs       // ReadOutput, parse_read_output, parse_numbered_read_line, render_read_output, OutputText, generic_output_lines, append_output_notice,
                  // RenderLimits, bounded_safe_display_*, safe_display_text, sanitized_display_prefix, display_line_count, path_extension
  diff.rs         // DiffRowKind, DiffRow, ToolDiff, tool_diff, is_unified_diff, parse_hunk_starts, diff_range, for_each_diff_row,
                  // for_each_unified_diff_row, RenderedDiffRow, diff_total_lines, render_diff_output
  wrap.rs         // role_block, role_block_lines, prefixed_unwrapped_line, repeated_prefixed_hanging_line, split_spans_at_width,
                  // unbreakable_columns, gutter_fits, prefixed_wrapped_line, repeated_prefixed_wrapped_line, leading_gutter_token, extract_line
  events.rs       // goal_activation_layout, goal_layout, producer_*_layout, producer_mode_label, goal_status_label, system_prompt_layout,
                  // compaction_layout, plugin_message_layout, collapsible_event_block, media_file_layout, empty_conversation_lines
  tests/          // already split in phase 1; update `use super::` paths
```

Visibility `pub(super)` inside the directory; nothing new becomes `pub(crate)` unless another `ui` module already used it.

## 3D. Engine `SessionStore`: split `session.rs` (~4.5k production lines)

Move to `crates/engine/src/session/mod.rs` (the directory already holds `projection.rs` and, after phase 1, `tests.rs` / `windows_tests.rs`) and split:

```
session/
  mod.rs          // SessionError, SessionLocation, RunProjection, SessionProjection, SessionSummary, SessionResidency, StoreOwnership,
                  // WriteOpen, SessionStore struct + the public `impl SessionStore` API (open, create, append, load, list, revert, fork, rename, evict...)
  tree_load.rs    // TreeState, TreeLoadProducts, ParentRunFacts, TreeLoadStatus, TreeGate, PendingLoads, TreeLoadObserver, Install, LogFingerprint,
                  // TreeFold, TreeLoadDriver, TreeLoadReads, TreeLoadReadHook, EvictionTransitionHook, PublishHook and the `impl SessionStore` methods
                  // that drive tree loading (keep as a second `impl SessionStore` block in this file)
  fold.rs         // fold_consumed, projection_fold_count, assert_projection_equivalent, projection, projection_fold, terminal_run_of,
                  // summary_from_projection, is_terminal_status, restart_stable_grant, add_usage, turns_tool_name, fork_title
  workdir.rs      // workdir_key_suffix, write_layout_marker_if_absent, scaffold_listing, write_workdir_cwd (both cfgs), workdir_cwd_is_current (both cfgs),
                  // create_unix_session_directory_all, create_windows_session_directory, create_windows_session_file
  cache.rs        // meta_path, write_cache, read_cache, write_index_json, replace_windows_path_with_retry, windows_replace_is_contended, MutationGuard
  projection.rs   // unchanged
```

Re-export from `session/mod.rs` everything that `crate::session::` paths outside the module use today (`pub(crate) use` as needed) so `runtime/*` and `server` do not change.

## 3E. TUI reducer: split `state/mod.rs` (~3.6k production lines)

```
state/
  mod.rs          // the public types (ToolStatus, ToolCallState, ApprovalState, EventLevel, TranscriptItem, AssistantChild, SessionState, StateStore,
                  // DeliveryOutcome, ...) and `impl StateStore` / `impl SessionState`
  reduce.rs       // reduce_event, reduce_session_events and every helper below them (goal_status_is_terminal, valid_*_change, move_input_to_boundary,
                  // rebind_pending_attempt, producer message helpers, open_assistant_item, append_attribution, prune_abandoned_attempt, mark_committed,
                  // index_turn_tool_content, rebuild_committed_children, append_assistant_delta, place_tool_rows, link_tool_child, void_pending_inputs,
                  // close_open_assistant, seal_open_thinking, push_item, push_event)
  runtime.rs      // unchanged
  tests.rs        // from phase 1; paths updated
```

## Commits
1. `refactor(engine): group Inner runtime state by subsystem`
2. `refactor(tui): split App into a module directory by concern`
3. `refactor(tui): split the transcript renderer into focused modules`
4. `refactor(engine): split SessionStore into tree-load, fold, workdir and cache modules`
5. `refactor(tui): move the event reducer into state/reduce.rs`

## Local gate
Same as phase 1/2 (fmt, clippy stable + 1.88, test, doc, deny, schema-additive check, `cargo insta test` with zero new/changed snapshots). Also `cargo check --locked --target x86_64-pc-windows-msvc -p cookie_agent` if `rustup target list --installed` includes it (the machine has an xwin cross setup in `~/.cargo/config.toml`), because `session/workdir.rs` and `cache.rs` carry `cfg(windows)` code.

## Milestone
Push the branch, wait for the branch `CI` run to be green (`gh run list --branch <branch>`; `gh run watch <id> --exit-status --interval 60`, repeat on tool timeout), fix and re-push on failure. When green: `git checkout main && git merge --ff-only <branch> && git push origin main`; wait for `CI` on `main` to be green. Report: main SHA, CI URLs, `Inner` field count before/after, the new largest-file table (`find crates -name '*.rs' -path '*/src/*' -not -path '*/tests/*' | xargs wc -l | sort -rn | head -15`).

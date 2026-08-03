# cookie_code Architecture

**Status:** protocol foundation implemented — unified protocol/event schema v6 only.
**Tagline:** subagent-first coding harness.

cookie_code is a full-stack coding-agent harness written in Rust. A single
local daemon owns all agent behavior; thin clients (TUI today, web and VS Code
later) connect over a versioned protocol. Subagents are a structural feature:
every agent conversation can spawn isolated child conversations, and every
client can observe and interact with the whole tree in real time.

---

## 1. Design principles

1. **Subagent-first.** Any conversation may delegate to isolated child
   conversations with their own models, contexts, and profiles. The delegation
   tree is queryable, observable, and steerable by clients through ordinary
   engine APIs — not a black box that returns text.
2. **One engine, many frontends.** The daemon is the only place agent logic
   exists. Terminal, web, and editor clients are pure protocol consumers.
3. **Event-driven.** Commands flow in, typed events flow out. Session event
   logs and the delegation journal are the durable sources of truth — logs
   for conversation history, the journal for delegation reservations and
   edges. UI state is a disposable projection.
4. **Explicit capability-aware models.** Every runnable alias binds one
   concrete Oven adapter and an explicit descriptor. Models.dev supplies a
   discoverable catalog and `/connect`-style credential onboarding, but model
   IDs and catalog metadata never infer adapter behavior, routing, or request
   defaults.
5. **TOML configuration.** Layered, profile-based, snapshotted at session
   creation. Live config edits never mutate in-flight sessions.
6. **Delegate-only delegation.** The only way a child session comes into
   existence is a model calling the `delegate` tool. There is no declarative
   workflow engine and no client-side fan-out API.
7. **Binary-only internal distribution.** All Cookie Agent workspace crates are
   nonpublishable application components. Releases are locked workspace builds
   of the `cookie` binary; crates.io packages and `target/package` archives are
   not product artifacts. The root `Cargo.lock` is the sole authoritative
   dependency graph; vendored libraries do not maintain or execute independent
   lockfiles or unsupported feature graphs.

Explicit non-goals for the MVP: OS sandboxing, MCP, plugins, remote
deployment, budget accounting, parallelism caps.

---

## 2. System overview

```
┌────────┐  ┌────────┐  ┌─────────┐  ┌────────┐
│  TUI   │  │  CLI   │  │   Web   │  │ VS Code│        (thin clients)
└───┬────┘  └───┬────┘  └────┬────┘  └───┬────┘
    └───────────┴──────┬─────┴───────────┘
                       │ JSON-RPC 2.0 over abstract message streams
                       │ (WebSocket default · stdio · Unix socket · in-process)
              ┌────────▼─────────┐
              │      server      │  axum: WS transport, JSON-RPC routing
              ├──────────────────┤
              │      engine      │  single-conversation runtime:
              │                  │  sessions, runs, agent loop, tools,
              │                  │  events, persistence, permissions,
              │                  │  provenance + tree queries
              ├──────────────────┤
              │    delegation    │  tool-provider implementing `delegate`;
              │                  │  spawns child sessions via engine API
              ├──────────────────┤
               │     models       │  immutable ModelSet of configured
               │  ┌─────────────┐ │  Oven LanguageModel adapters
               │  │    Oven     │ │  selected only by config alias
               │  └─────────────┘ │
              └──────────────────┘
```

The **engine** is a single-conversation runtime. It implements no spawning
policy itself: the **delegate tool provider** (living in `tools`, deliberately
*not* in `engine`, so the engine stays a pure single-conversation runtime
even at the module level) decides per session whether to expose the tool,
performs admission, and creates child sessions — always through the engine's
privileged child-creation API. The engine's role in delegation is generic:
tool registration, the permissions pipeline, provenance derivation, and
durable session storage. The daemon composes both.

Dependency direction (no cycles). Arrows read "depends on"; `cookie_agent` is
the sole composition root:

```
tui ──────► protocol ◄────── server
               ▲              │
               │              ▼
models ◄──── engine ◄────── tools        (built-ins + delegate tool)
  ▲            │
  └── config ◄─┘
```

- `server` → `protocol`, `engine`
- `engine` → `protocol`, `models`, `config`
- `tools` → `engine` (implements `ToolProvider`; delegate reaches the engine
  exclusively through its client API — an in-process handle — which is what
  keeps it splittable into a separate binary later)
- `tui` → `protocol`, `server` (client side only; `server` supplies the
  current `MessageStream` transport adapters for in-process and WebSocket
  connections, never engine APIs)
- `cookie_agent` → `engine`, `models`, `tools`, `config`, `server`, `tui`

`engine` never imports `tools`; the composition root registers the built-in
and delegate tool providers into the engine's tool registry.
`cookie_agent` eagerly constructs one immutable `ModelSet` through
`Config::build_model_set` and the explicit Oven constructors in `crates/models`,
passes that exact set into `EngineOptions`, and gives the server a clone solely
for the safe `model.list` projection. The daemon additionally owns the
revisioned models.dev catalog/cache and connected-provider credential store;
catalog entries become runnable only after explicit supported-adapter
materialization. Daemon, in-process TUI, and WebSocket attachment therefore
observe the same runnable model revision and safe descriptors.

---

## 3. Workspace layout

```
crates/
  protocol/              # JSON-RPC types: commands, events, session/tree models
                         # schemars (JSON Schema) + ts-rs (TypeScript bindings)
                         # + transport layer (§11): stream abstraction,
                         #   websocket (default) · stdio · unix socket · in-process
                         #   (ws behind a feature flag to keep the crate lean)
  models/                # immutable ModelSet/ModelEntry/FrozenModelBinding,
                         # explicit Oven adapter construction + ScriptedModel
  engine/                # session/run actors, agent loop, event log,
                         # provenance, permissions, compaction, tool runtime,
                         # ToolProvider trait
  tools/                 # built-in tools: read, write, edit, bash, grep, glob
                         # + delegate tool provider (§5) — a tool provider that
                         #   calls the engine only through its client API
  config/                # layered TOML (figment), profiles, policy snapshots
                         # + explicit [models.<alias>] declarations
  server/                # axum daemon: WS listener, daemon lifecycle,
                         # session/run service behind the protocol
  tui/                   # ratatui client (pure protocol consumer)
    src/ui/              # layout/hit map, app loop, event helpers, transcript, input, slash palette, pickers
  cookie_agent/            # thin binary (composition root):
                         #   `cookie` (TUI), `cookie daemon`, ...
```

---

## 4. Core concepts

### 4.1 Sessions and runs

- A **session** is one isolated conversation: system prompt, message history,
  resolved profile, tool set, working directory, event log.
- A **run** (turn) is one agent-loop execution within a session: submit input
  → model streams → tool calls execute → repeat → terminal state.
- Each session is owned by **one actor task**; history mutations are
  serialized through it. **The actor never awaits tool futures.** Tool calls
  (including `delegate`) are spawned as separate tasks; the actor keeps
  processing its mailbox — cancellation, steering, `ToolCallLinked` appends —
  while tools run, and tool results re-enter as mailbox messages. This is what
  makes the delegation lifecycle deadlock-free: a delegate invocation may call
  back into the engine (child creation, parent-log appends) while the parent
  actor remains responsive. Parallel tool results are committed in
  deterministic model tool-call order.
- Concurrent tool execution is *backpressured* by bounded channels — an
  implementation detail, not a policy limit. There are no user-visible
  parallelism caps by design (§1).
- **Steering**: clients may inject input into a running turn; accepted input is
  persisted as `UserInputSubmitted`. The actor persists `UserInputApplied`
  before the next safe model attempt (never mid-tool-execution), including
  retry and fallback attempts. Prompt assembly uses that durable association to
  place input after the active assistant turn and before that attempt.
- **Cancellation**: every run and tool call carries a `CancellationToken`.
  Cancelling a session stops streaming *and* in-flight tool execution.

### 4.2 Events

The engine emits typed, ordered events. Each session's event log is the
source of truth; everything else is a projection.

```
SessionCreated          RunStarted              UserInputSubmitted | UserInputApplied
RunCompleted | RunFailed | RunCancelled         RunInterrupted
TextDelta               ReasoningDelta
ToolCallStarted         ToolCallProgress        ToolCallCompleted
ToolCallFailed          ApprovalRequested       ApprovalEvaluated
ApprovalEscalated       ApprovalUserDecisionRecorded
ApprovalFinalized       ApprovalCancelled       ApprovalDoomLoopDetected
TreeApprovalGrantCommitted
ToolStdinSubmitted      (redacted audit: byte count only, §7.1)
ToolCallLinked          (delegate call → child session backlink)
AttemptAbandoned        (failed model-attempt boundary; not prompt history, §6.3)
ModelReplayEvaluated    (ordered scoped replay decisions for an attempt, §6.1)
ModelTurnCommitted      (model identity + complete PersistedModelTurn, §6.1)
ModelFallback           (chain advance: from, to, error, attempts, §6.3)
InternalAgentStarted | Completed | Failed | Cancelled | Interrupted | Fallback
ContextCheckpointCommitted       SessionTitleCommitted
```

Every **persisted** event carries the exact unforgeable
`EventSchemaVersion(6)`, session ID, run ID (when applicable), a
per-session monotonic sequence number (authoritative ordering — never
inferred from IDs or timestamps), and an RFC 3339 timestamp. Cursor replay
and sequence numbers apply to persisted events only. A bounded persisted-event
live subscription emits an ephemeral `Gap { session_id, last_delivered_seq }` control message before
closing when it falls behind; its `last_delivered_seq` is the exclusive cursor
clients use to replay the first omitted event.
Live tool output
streaming (§7.1) uses a separate **ephemeral notification envelope** with
its own per-call byte offsets — it is not part of this sequence, never
written to the log, and never cursor-replayed.

### 4.3 Provenance (the subagent-first primitive)

Every session records its origin as an enum:

```
SessionMeta {
    id,
    origin: SessionOrigin,
    cwd,
    profile: ProfileSnapshot,
    title: Option<SessionTitle>,
}

SessionOrigin =
    Root
  | Delegated {
        root_session_id,      // derived by the engine from the parent,
        parent_session_id,    // never accepted from the caller
        parent_run_id,
        parent_tool_call_id,
        invocation_id,        // idempotency key (§5.4)
        depth,                // engine-derived: parent.depth + 1
    }
  | Forked {
        source_session_id,
        source_event_seq,     // branch point in the source log
    }
```

The authoritative origin record is the `SessionCreated` event — the first
entry in the session's own event log. `meta.json` is a rebuildable cache, not
a second source of truth.

The parent/child edge is recorded **by the engine** before the child starts,
through the durable creation protocol in §5.4 (journalled, not a single
transaction — per-session JSONL files cannot provide cross-file atomicity).
When a child is created, the engine also emits `ToolCallLinked` into the
*parent's* log, annotating the originating delegate call with the child
session ID. Clients can therefore render any delegate tool call as an
expandable live view.

Tree queries follow `Delegated` edges only. Deferred forks do not
participate in the delegation tree; they are linked sideways via
`Forked.source_session_id`.

Engine tree queries (ordinary protocol methods):

```
session.children(session_id) -> [ChildSummary]   // profile, task excerpt, status, usage
session.tree(session_id)     -> SessionTree
```

### 4.4 Client interaction with subagents

Because children are ordinary sessions, every client interaction reuses the
same APIs as for the root:

| Interaction | Mechanism |
|---|---|
| Observe live | `events.subscribe(child_session_id)` |
| Steer mid-run | `run.steer(child_run_id, input)` — persists `UserInputSubmitted`, then a durable `UserInputApplied` boundary before model consumption |
| Cancel | `run.cancel` / `session.cancel` |
| Follow up on a completed child | **forks** the child (deferred); original stays immutable as the record of what fed the parent. Current children are read-only after completion |

Directly cancelling a child resolves the parent's pending delegate call: the
delegation service observes the child's terminal state and returns a
structured tool result (`{status: "cancelled", partial_report}`), so the
parent's model can react. Cancellation never wedges the graph — it becomes
information.

### 4.5 Policy snapshots

At session creation the engine materializes and stores the effective
**configured** policy: resolved profile, model, tool set, permission rules,
delegation policy (`allowed_profiles`, effective depth limit, result
limits). All configured-policy decisions during a session derive from this
snapshot, never from live TOML.

Permission rules in the snapshot are resolved exactly as §8 defines: global
default configuration **plus the session's own profile overlay — never
parent-profile rules**. A child's snapshot therefore contains no permission
state inherited from its parent.

Two things deliberately live *outside* the snapshot: the tree-shared
runtime approval store (mutable, event-sourced, §8.5) and engine-derived
provenance (depth, root ID). Approvals are runtime state, not configured
policy.

### 4.6 Per-run draft profile switching

The session profile remains the creation-time configured default, but
`run.start` accepts an optional `profile` override for draft-agent switching.
The client draft is local UI state: selecting it sends no RPC and mutates no
session, watch, or cache state until the next `run.start`. The override is
resolved before the run begins and does not mutate session metadata.
`RunStarted` freezes both the complete effective `ProfileSnapshot`
for that run and the selected `ProfileIdentity`; retries, fallback attempts,
tool exposure, approval policy, and internal work spawned by that run use this
frozen run profile. Reusing a `client_run_id` with a different input or profile
is an `idempotency_conflict`.

### 4.7 Internal agents, compaction, and titles

Engine-owned work uses `InternalAgentKind::{Approval, ContextCompaction,
SessionTitle}` with distinct invocation and internal-run UUIDs. Generic
lifecycle events record a safe call digest/summary, selected backend, terminal
safe result or failure, cancellation/interruption, and fallback. Raw prompts,
provider bodies, native payloads, and credentials are not lifecycle fields.
For builtin backends, `revision` identifies the exact per-kind prompt/runtime
semantic contract; it is intentionally independent of the protocol and event
schema version.

Context compaction is active in v6. A `ContextCheckpointCommitted` contains
frozen sequence boundaries and budgets plus exactly one checkpoint:

- `provider_native`: an Oven native-context artifact reference with exact
  adapter and `NativeContextScope`; the private payload is bounded to 32 MiB;
- `internal_summary`: UTF-8 summary text bounded by both its configured
  `max_summary_bytes` and the global 2 MiB ceiling. Its declared byte length
  and canonical SHA-256 must exactly match the text on construction and decode.

Raw events are never deleted. `ModelTurnCommitted.input_through_seq` and the
checkpoint boundary identify exactly which durable input was consumed. Native
replay payloads remain independently bounded to 2 MiB.

Session titles are a durable projection in `SessionMeta.title`. `SessionTitle`
retains exact authored text but rejects blank values, control characters, and
UTF-8 encodings over 512 bytes.

`SessionTitleCommitted` contains `input_through_seq` plus one strict tagged
`SessionTitleCommit` payload; loose source/operation/title fields do not exist:

- `user_set { title, client_rename_id }` installs a validated user title;
- `user_clear { client_rename_id }` deliberately keeps the session untitled;
- `user_reset { client_rename_id }` removes the user override so automatic
  title generation may run again;
- `internal_agent_set { title, invocation_id }` records model/internal title
  generation;
- `fallback_set { title }` records deterministic fallback generation.

User variants require a validated non-empty `client_rename_id` (maximum 256
UTF-8 bytes, no control characters). Internal/fallback variants cannot carry
that ID; they can only set a valid title and cannot clear/reset. Conversely,
clear/reset are user-only and cannot carry title or invocation fields. These
rules are structural in the tagged enum and are also enforced by strict
deserialization.

On restart, replay extracts a `SessionRenameRecord { client_rename_id, change }`
from every user title commit and rebuilds the rename idempotency index. Reusing
an ID with the exact same `Set`/`Clear`/`Reset` payload returns the original
result; reusing it with any different operation or title is the stable
`idempotency_conflict`. Internal/fallback commits never enter this index.

---

## 5. Delegation

### 5.1 The delegate tool

`delegate` is registered with the engine for every session, but the delegate
provider's `tools_for_session` decides per session whether to expose it: the
tool is offered only when the session's policy snapshot has delegation
enabled and its effective depth limit `allows_delegation()` (§5.2). Users
never list it manually; models cannot request it when it isn't offered. Its
`profile` argument is generated as an enum of exactly the allowed child
profiles, **restricted to profiles of type `subagent` or `all`** (§9; never
`primary` or `internal`) — naming a `primary`-only or `internal` profile in
`allowed_profiles` is a config validation error.

The schema is only an exposure filter: engine child admission authoritatively
revalidates the target profile's type, the parent's allowed-profile set, and
the frozen delegation limit. A tool provider cannot admit an ineligible target.
The composition root must construct `DelegateToolProvider` from the same
validated configuration supplied to the engine; this shared configuration is a
composition-root invariant.

Tool schema (sketch):

```json
{
  "task":             "focused objective for the child (required)",
  "profile":          "one of the allowed profile names",
  "context":          ["text | file-ref | artifact-ref entries, size-bounded"],
  "success_criteria": ["..."],
  "expected_output":  {"description": "...", "format": "text|json"}
}
```

Deliberately absent: raw model IDs, tool lists, working directories, depth,
network/sandbox settings. Those are profile concerns.

### 5.2 Depth limit

Each session carries an effective depth limit — the number of further
delegation generations permitted beneath it:

```rust
enum DepthLimit { Finite(u32), Unlimited }
```

A child's limit is computed at admission (and frozen into the child's policy
snapshot):

| child profile limit | parent effective limit | child effective limit |
|---|---|---|
| `Finite(c)` | `Finite(p)`, p ≥ 1 | `Finite(min(c, p - 1))` |
| `Finite(c)` | `Unlimited` | `Finite(c)` |
| `Unlimited` (unset) | `Finite(p)`, p ≥ 1 | `Finite(p - 1)` |
| `Unlimited` (unset) | `Unlimited` | `Unlimited` |

Admission requires `allows_delegation()`, defined as `limit != Finite(0)`;
`Finite(0)` means `delegate` is not exposed in that session at all.

- For **finite** limits, the value strictly decreases every generation, so
  tree height is bounded by the root's limit. No per-profile misconfiguration
  can produce infinite recursion. (An `Unlimited` parent with an `Unlimited`
  child does not decrease — that is the explicit opt-out, available only when
  the entire chain from the root is unset.)
- The engine derives `depth` and `root_session_id` from the parent record
  itself; it never trusts values supplied by the tool provider.
- There are **no caps on active children and no budgets** by design.

### 5.3 Lifecycle of one delegate call

```
1.  Parent model emits delegate(args)
2.  Engine validates args, applies permission policy (delegation may require approval)
3.  Engine commits ToolCallStarted and invokes the delegation provider with
    a stable invocation_id derived from the engine-generated
    (parent_session_id, parent_run_id, parent_tool_call_id) tuple
4.  Delegation service: lookup by invocation_id in the journal (§5.4)
      - invocation known  → attach to the existing child / reconstruct result
      - otherwise         → admit: profile allowed? parent allows_delegation()?
5.  Privileged session.create_child(idempotency_key = invocation_id, origin, profile)
      → engine runs the durable creation protocol (§5.4):
        journal record → child session files → ToolCallLinked in parent log
6.  Start child run (client_run_id derived from invocation_id), subscribe to events
7.  Optionally emit coarse ToolCallProgress into the parent (no token relay —
    clients wanting detail subscribe to the child directly)
8.  Terminal child state → extract final report → bound size → tool result
9.  Engine commits ToolCallCompleted/Failed, parent loop resumes
```

Delegation preserves session-local diagnostic ownership. A successful parent
`ToolResult` is constructed identically for live completion and restart
recovery from only the child's terminal status, final report, truncation state,
and child-session link. Child `ModelTurnCommitted.warnings` and
`ModelReplayEvaluated` diagnostics remain solely in the child log; they are
never copied into the parent's tool output. Parent model warnings likewise
remain on the parent's own committed turn. Warning text is not filtered or used
to reinterpret otherwise valid final text as failure.

Result bounding: profile-level cap (16–32 KiB model-visible) with structured
truncation metadata (original byte/line counts and an opaque retained-artifact
reference). The engine atomically retains the complete output before exposing
a preview; retention failure fails the tool call closed.

### 5.4 Durability, idempotency, and crash windows

Per-session JSONL files cannot provide cross-file atomicity or uniqueness
constraints, so delegation durability is built from an explicit journal plus
a single-writer actor instead of an implied transaction.

**The delegation journal and its actor.** Each project directory holds an
append-only `delegations.jsonl`, owned by one project-scoped **journal
actor** in the engine. All admissions go through it:

```
reserve(invocation) -> Reserved(existing_child?) | AlreadyExists
```

The actor holds the in-memory invocation index (rebuilt from the journal at
startup) and performs check-and-reserve **atomically in memory before any
append**. Two concurrent admissions of the same invocation therefore cannot
both pass — the second observes the reservation and attaches. "Last record
per `invocation_id` wins" applies only to crash recovery of the on-disk log,
never to live admission. A journal record mapping a known `invocation_id` to
*different* parameters is corruption: the actor refuses it and surfaces an
error rather than silently choosing one.

If any journal append/fsync **fails**, the actor enters a poisoned fail-stop
state and rejects every later mutation until engine reopen. The failed write
may have reached the file despite the error, so allocating a fresh reservation
in the same process could conflict with a durable or torn record. On reopen,
the normal JSONL torn-tail recovery establishes the authoritative state.
The mutation that first fails is returned to its caller as its underlying I/O
error; the poisoned state makes later mutations return `Poisoned`. Startup
reconciliation propagates that first repair error from `Engine::open` rather
than silently publishing a half-repaired engine.

Run cancellation retries its terminal JSONL append a bounded number of times.
If all attempts fail, the cancellation error remains surfaced and the active
run is retained as a cancellation-pending tombstone; no further in-process
retry is attempted, so reopen is required to reconcile it rather than silently
leaving a durable `Running` projection.

The same retained-active/reopen-required rule applies if persisting model
attempt state fails, including stream deltas, `ModelReplayEvaluated`, or the
terminal `ModelTurnCommitted` carrying its `PersistedModelTurn`. The model loop
surfaces the error and attempts its terminal append once; if that append also
fails, the active entry remains as a tombstone and no in-process recovery is
attempted.

`invocation_id` is derived by the engine from
`(parent_session_id, parent_run_id, parent_tool_call_id)` — all
engine-generated, so the tuple is globally unique by construction; it never
depends on a model `model_call_id` or `provider_item_id`.

**Durable creation protocol** (in order, each step fsynced):

```
1. Journal actor: reserve(invocation) — atomic check-and-reserve
2. Append DelegationStarted { invocation_id, parent ids, child_session_id,
    immutable request payload { task, context, success criteria, expected output },
    effective child policy, immutable-argument fingerprint } to
   delegations.jsonl. The fingerprint covers task, context, success criteria,
   expected output, profile, and effective child policy; a duplicate invocation
   must match all of them.
3. Build the child session directory under a TEMPORARY name:
   write events.jsonl containing a complete, fsynced SessionCreated event
   { origin: Delegated{...} }; write meta.json; fsync the directory.
   Then atomically rename into place and fsync the parent directory.
   A child is VALID iff the rename completed and its events.jsonl parses
   with a valid SessionCreated.
4. Send `EnsureToolCallLinked { tool_call_id, child_session_id }` to the parent
   session actor. The actor atomically checks its log and appends the backlink
   only if absent. Every creator and re-delivery uses this command before
   journal-link/start; startup recovery performs the same actor-atomic ensure
   before the engine is published. The child run cannot start before this
   durable backlink exists.
5. Append DelegationLinked { invocation_id } to the journal
6. Start the child run (client_run_id derived from invocation_id); the
   engine-owned admission task appends DelegationRunStarted { invocation_id,
   child_run_id } even if the delegating caller has gone away. If its last
   observer abandons the admission, that task/sweeper cancels the child and
   resolves the pending parent call.
```

The child's initial input is rendered deterministically as four labelled
sections: `Task:` followed by task text, then `Context:`, `Success criteria:`,
and `Expected output:` followed by compact JSON for each corresponding payload
value. The payload, not the fingerprint, is used when an unstarted child is
recovered.

**JSONL torn-tail recovery.** Any JSONL file (session logs, journal) may end
in a partial record after a crash. On load, every file is truncated to its
last complete newline-delimited record before projections are built.

**Startup reconciliation** handles every partial state:

| State on startup | Reconciliation |
|---|---|
| Reserved/started, no valid child | Delegation failed before creation; parent resume resolves the tool call with a structured failure |
| Valid child, no `ToolCallLinked` in parent | Append the missing `ToolCallLinked` (journal is authoritative for the edge; parent log for rendering) |
| Linked, no `DelegationLinked` confirmation | Append the confirmation |
| **Valid child, no `RunStarted` in its log** (crash between steps 5 and 6) | Child is *unstarted*. On parent resume, delegation starts the run **exactly once** — the idempotent `client_run_id` makes a double start impossible — and the invocation proceeds normally. If the parent session is never resumed, nothing runs |
| Child session with no journal record (foreign/orphan) | Mark every non-interrupted run `interrupted`, keep queryable; exclude it from tree projections and never auto-attach |
| Journaled child whose parent run is cancelled | Mark an active/interrupted child run `cancelled`; parent resume records a structured cancelled delegate result |
| Pending delegate call in a session log, **no journal reservation** (crash between the in-memory reservation and the `DelegationStarted` append) | Nothing durable was ever created. On parent resume the call resolves as a synthetic interrupted failure — the parent's model may simply retry the delegation in the next run |

**Run resume semantics.** On restart, every non-terminal run (parent or
child) is marked `RunInterrupted` — terminal for that run; its agent loop
never restarts. Re-resolution happens on `session.resume` (or before the
next `run.start` on that session): the engine appends synthetic tool-result
events to the session log for calls left pending by the interruption. These
are **post-terminal annotations** on the interrupted run — they are consumed
by the *next* run's prompt assembly (so the model sees why its tool calls
failed) and never revive the old loop. Assembly relocates a delayed synthetic
result beside its originating assistant call, before any later user/model turn,
so the persisted log's annotation position cannot produce invalid model-history
ordering. Re-resolution **never re-executes**
pending calls:

- Built-in tool calls (read/write/edit/bash): synthetic failure
  "interrupted by daemon restart". Re-executing a possibly-applied side
  effect is never safe.
- Delegate calls: re-resolved through the journal. Child completed before
  the crash → result reconstructed from the child's events. Child unstarted
  → started exactly once (above). Child interrupted → structured failure
  naming the (still queryable) child session. The child is never re-run
  under the same invocation.
  A cancelled parent is distinct from an orphan: its journaled child is
  cancelled and its pending delegate call receives a structured cancelled
  result, whereas only parentless/unjournaled children are interrupted.

For an unstarted child, the parent actor registers one recovery waiter keyed by
the pending parent call. Repeated `session.resume` operations observe that
waiter rather than re-resolving the call; its eventual result returns through
the parent actor's normal tool-result command.

All synthetic delegate failures use the parent actor's atomic
`ResolveDelegateFailureIfPending` command. A late tool result for an already
resolved call is acknowledged as a no-op, so it cannot abort the parent loop
or prevent later parallel tool results from being committed. If journal run
confirmation fails after child start, the engine cancels the child before
resolving the parent failure. The cancellation event is asynchronous; a crash
before it is durable reopens the child as interrupted and recovery resolves it.

**Crash windows while the daemon is live** are covered by the same
reservation: re-delivery of an invocation (e.g. a retried tool call) finds
the existing reservation, repairs a missing parent link or journal confirmation,
and attaches rather than duplicating. Parent cancellation propagates through
the complete journal tree (including in-flight pre-confirmation admissions) →
delegation cancels every descendant → a late child success is discarded, never
injected into the cancelled parent. Cancellation can race model scheduling:
the engine catches it immediately after run start, while child tool execution
remains permission/cancellation guarded. Abandoning a delegate-result wait
schedules the same child cancellation when a Tokio runtime handle is available;
teardown outside any live runtime is best-effort.

---

## 6. Explicit Oven models

`cookie_agent_models` is the sole model-construction boundary. One immutable
`ModelSet` maps each configured alias to a `ModelEntry` containing exactly one
`Arc<dyn oven_sdk::LanguageModel>`, the descriptor returned by that model, and
immutable `RequestDefaults`. Construction is eager and fail-closed: duplicate
aliases, invalid declarations, unsupported auth/settings combinations, and
dishonest capability declarations fail configuration loading.

```rust
struct ModelSet { /* immutable alias -> ModelEntry */ }

struct ModelEntry {
    alias: String,
    model: Arc<dyn oven_sdk::LanguageModel>,
    descriptor: oven_sdk::LanguageModelDescriptor,
    defaults: RequestDefaults,
    behavior_fingerprint: ConfigurationFingerprint,
}

struct FrozenModelBinding {
    alias: String,
    descriptor: oven_sdk::LanguageModelDescriptor,
    defaults: RequestDefaults,
    behavior_fingerprint: ConfigurationFingerprint,
    configuration_fingerprint: ConfigurationFingerprint,
}
```

The concrete retained adapters are Anthropic Messages; official OpenAI Chat
and Responses; caller-identified OpenAI-compatible Chat; Google Gemini;
Google Vertex Gemini; Amazon Bedrock Converse; Azure OpenAI Chat and Responses;
Cohere v2 Chat; and standardized Open Responses. MiniMax and Claude Platform
on AWS are not exposed. Oven versions are exact published pins. There is no
model-name inference in `cookie_agent_models` or Oven. Models.dev catalog data
is a separate daemon-owned discovery/onboarding projection; it never constructs
an adapter or silently changes a configured binding.
Runtime model composition is registry-free: `compose_models()` at the binary
composition root calls `Config::build_model_set`, `crates/models` constructs
the explicitly selected published Oven adapters, and the resulting immutable
set enters the engine through `EngineOptions::model_set`.

Every `[models.<alias>]` entry explicitly declares provider/model identity,
endpoint, resolved auth, static headers, capabilities, limits, modalities,
exact media rules, cancellation/replay semantics, structural adapter settings,
common request defaults, and typed provider options. The selected `adaptor`
tag chooses only the concrete constructor. The arbitrary `model_id` is sent to
that already-selected adapter and never changes behavior.

`provider_id` and `adaptor` are intentionally distinct fields. `provider_id`
is the caller-defined stable serving-provider identity retained in model
descriptors, native replay/context scopes, and behavior/configuration
fingerprints. It must remain stable for the same serving identity and need not
equal an adapter name. `adaptor` is only the concrete Oven adapter/wire
protocol discriminator. Representative pairs are `anthropic` / `anthropic`,
`openai` / `openai-responses`, and `quantumcookie.gateway` /
`openai-compatible`.

The model-set fingerprint is canonical SHA-256 over sorted aliases and all
non-secret behavior configuration. Credential values are excluded while the
non-secret auth shape is retained. Static header names are included but values
are excluded. A policy snapshot stores `FrozenModelBinding`, not live models,
auth, headers, or a mutable lookup object; resolving it later requires the exact
model-set fingerprint, descriptor, defaults, and complete per-entry behavior
fingerprint.

`ModelSetManager` retention is intentionally **daemon-process-local**. During
one daemon lifetime it retains older model-set fingerprints so an in-memory
session binding can continue to resolve after compatible provider additions.
Every credential refresh first constructs the complete current candidate, then
rebuilds every retained snapshot from candidate entries whose alias,
descriptor, defaults, and behavior fingerprint all match. This replaces all
retained concrete adapters with adapters created from the latest credential
generation. A retained fingerprint with any missing or mismatched entry is
dropped. Publication and frozen-binding resolution are serialized around the
atomic current-snapshot swap: an adapter handle acquired before publication may
finish, but no later resolution returns a stale credential generation.

The retained-fingerprint map is never persisted or reconstructed from session
data. After daemon restart it contains only the current snapshot rebuilt from
validated current config, the pinned catalog, and latest durable credentials. An
obsolete persisted `FrozenModelBinding` remains decodable audit/session data,
but execution fails closed when its exact fingerprint is absent; resolution
never falls back by alias. A binding whose behavior fingerprint is still
current, including across a secret-only credential rotation, resolves to the
current adapter and therefore the current credentials.

`ScriptedModel` is the deterministic engine-test implementation of Oven's
`LanguageModel`. Each call consumes one FIFO script, captures the validated
request, and either returns a delayed pre-stream error or a cancellation-aware
stream. Stream scripts queue `Result<StreamPart, ModelError>` items plus delays,
so tests can place failures before meaningful output, after partial output, or
while blocked. The abort signal is checked before consuming a script, during
stream creation delay, before each emitted item, and while awaiting a
mid-stream delay.

### 6.1 Replay and round-trip fidelity

Oven normalized history is the behavioral contract, while
`NativeReplayArtifact` preserves bounded provider-native continuation state
when an adapter declares replay support. Each artifact is tied to an exact
adapter ID and `NativeContextScope` containing provider, model, and a safe resource
identity. The payload is redacted from debug output and bounded by Oven.

The explicit capability declaration states replay policy (`never`, `if_valid`,
or `always`), capability (`unsupported`, `optional`, or `required`), and whether
the artifact carries provider-authoritative reasoning state. Request encoding
persists `ModelReplayEvaluated` with one ordered `ReplayDecision` per assistant
history turn. Each decision contains the current `ReplayDisposition`: replayed,
no artifact, discarded foreign adapter, discarded foreign `NativeContextScope`,
discarded invalid payload, or reconstructed normalized history. Foreign or
invalid artifacts are never guessed into another adapter.

Provider-native artifacts preserve details such as Anthropic signed thinking,
OpenAI Responses encrypted reasoning/items, exact Chat tool-call echoes,
Gemini/Vertex thought signatures, Bedrock signed reasoning, and provider item
identities. After a valid Oven finish, `ModelTurnCommitted` stores the exact
model identity and one complete `PersistedModelTurn`. Its `native_replay` field
contains the optional `NativeReplayArtifact`, whose adapter ID and
`NativeContextScope` define where that artifact may be replayed.

### 6.2 Models.dev catalog and provider connection

The daemon maintains a revisioned safe snapshot of models.dev provider/model
metadata. `catalog.provider.list` and `catalog.model.list` return that snapshot
identity plus provider IDs, model IDs, display metadata, declared capabilities,
limits, modalities, status, and credential **field names**. Catalog metadata is
advisory discovery data; runnable `ModelRef` entries still require an explicit
supported Oven adapter and validated local materialization.

`provider.connect` is the `/connect` protocol equivalent. Its request is the
only protocol object that can contain credential values. Those values travel
only in the request transport, have manually redacted `Debug`, and are consumed
into the daemon credential store. They are forbidden from events, results,
typed errors, schema examples, logs, persistence records, and TypeScript
result projections. The result returns only provider identity, credential field
names, catalog revision, connection timestamp, and the new safe model revision.
Stable catalog/connect error identifiers are protocol data.

The generic JSON-RPC `Request` and `Notification` envelopes redact raw
`params: Value` in `Debug`; generic success `result` and error `data` values are
redacted for the same future-tracing boundary. Serialization of credentials is
still permitted only for the inbound `provider.connect` request transport.
Owned CLI/TUI credential-entry buffers and typed credential containers use
best-effort zeroization/redaction as soon as ownership permits. This is process
hygiene, not secure memory: serde values, WebSocket/framing buffers, kernel
socket buffers, allocator copies, and ordinary temporary strings may retain
copies until their normal lifecycle ends, and the protocol makes no locked,
non-pageable, or forensic-erasure guarantee.

`model.list` returns the current configured/connected runnable model snapshot
with its own revision, generation timestamp, and optional source catalog
revision. It is not the catalog endpoint.

### 6.3 Fallback chains

Each agent profile configures an ordered chain of **model aliases**:

```toml
[agents.primary]
models = [
  "sonnet",
  "gpt-responses",
  "local-qwen",
]
```

Semantics:

- **Error classification** drives behavior. Oven `ModelError.kind`, its
  retryability hint, and typed diagnostics are the only error inputs:
  - *entry-retryable* — rate limit (429), overloaded, 5xx, network/timeout,
    dropped stream: retry the same entry with exponential backoff (default
    2 retries), then advance to the next chain entry.
  - *entry-terminal* — auth failure, invalid request, or model not found:
    skip the entry immediately (no retries), advance to the next entry. Known
    provider error-body `code` values `model_not_found`, `invalid_model`,
    `model_does_not_exist` (including `model_doesnt_exist` and
    `model_not_exist`) take precedence over HTTP status heuristics, including
    5xx responses.
  - *run-terminal* — cancellation fails the run. Context pressure invokes the
    v6 compaction path (§4.7); a compaction failure is surfaced explicitly.
- **Resolution is immutable**: session policy contains ordered
  `FrozenModelBinding` values. Each attempt resolves its binding through the
  exact `ModelSet`, applies that entry's `RequestDefaults`, then validates the
  request against that entry's explicit Oven capabilities.
- **Partial output is abandoned on fallback**: if a stream fails midway, the
  partial deltas are discarded client-visibly (marked abandoned), and the
  completion restarts on the next entry against the same committed
  conversation state. The engine keeps the raw events in the JSONL audit log,
  then appends `AttemptAbandoned`; prompt assembly discards the accumulated
  assistant state for that attempt at this boundary. Earlier committed turns
  and tool results remain in the next request. `ModelFallback` remains the
  sole model-chain advance/degradation signal. Committed tool results are
  never replayed or lost. Prompt assembly maintains an active attempt segment
  and an emitted-call set: `AttemptAbandoned` clears only active calls and
  their native IDs, while calls already emitted in an earlier committed segment
  remain eligible for their result. A terminal run boundary closes the active
  segment and promotes only calls that have a later result into that emitted
  set. A result is emitted only when its call is in the set, then consumes the
  entry; calls without any result are never emitted. Thus a late result after
  an abandoned call is omitted, while a synthetic post-terminal recovery result
  retains an already-emitted matching call and is replayed immediately after
  that call, before later turns. Pairing keys each occurrence by the
  engine-generated `(tool_call_id, run_id)` tuple; a result without a run
  association, or without an exact emitted occurrence, is omitted rather than
  guessed. Oven replay outcomes explicitly report whether native state was
  replayed or normalized history was reconstructed.
- **Meaningful-output retry guard:** the engine's streaming attempt runner is
  the single same-entry retry layer:
  retry only before text, reasoning, or tool-call output has been observed.
  A retryable failure after meaningful output advances directly to the next
  fallback entry, never receives a second same-entry retry.
- **Per-run stickiness**: once the chain advances, the remainder of the run
  stays on the new entry (no flip-flopping under sustained rate limiting);
  the next run starts again from the chain head.
- Every advance and usage record names the exact configured model identity, so
  failures, cost, and behavior remain attributable.
- The resolved chain lives in the session's policy snapshot (§4.5); editing
  TOML mid-run never reorders a live session's chain. Children get their own
  chains from their own profiles — or **inherit the parent's resolved chain**
  when their profile's `models` is empty (§9). Internal agents (compaction)
  run on the inheriting session's chain by default.

---

## 7. Tools

The engine exposes a generic tool-provider interface:

```rust
// dyn-compatible runtime set of tool providers; unrelated to model selection
#[async_trait]
trait ToolProvider: Send + Sync {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>>;
    async fn invoke(&self, ctx: ToolInvocationContext, call: ToolCall)
        -> Result<ToolResult, ToolError>;
}

struct ToolInvocationContext {
    session: SessionId,
    run: RunId,
    progress: ProgressSink,          // ToolCallProgress + output deltas (§7.1)
    cancellation: CancellationToken,
    // ...workspace, policy snapshot, engine client handle
}
```

Tool futures are spawned outside the session actor's mailbox (§4.1), so an
`invoke` implementation may safely call back into the engine client API.

In-process tool providers for MVP (built-ins + delegation); the same interface
later covers remote tool servers (MCP).

MVP built-ins (modeled on OpenCode's basic set):

| Tool | Notes |
|---|---|---|
| `read` | descriptor-backed UTF-8 files use an 8 KiB fixed-chunk scanner with a 2,000-line/64 KiB content page budget and `(offset, byteOffset)` continuation cursor; directory listings are paginated; approved PNG, JPEG, GIF, WebP, and PDF inputs become durable attachments; unsupported or malformed binary files are rejected |
| `grep` | `ignore` + `regex` as libraries; honors .gitignore |
| `glob` | `ignore` traversal |
| `write` | atomic (temp + rename) |
| `edit` | optimistic exact-match editing: verify expected occurrence count, replace, **re-verify the file hash immediately before the atomic rename**; on mismatch, fail with a conflict result. The engine serializes write/edit calls by canonical path. Concurrent *external* writers can still interleave between read and rename — documented limitation, mitigated by the pre-rename hash check. **No fuzzy matching.** `similar` for diff display |
| `bash` | `process-wrap` process groups and timeout group kills |

Every tool returns one rich `ToolResult` containing a human-facing `title`,
model-visible textual `output`, structured `metadata`, optional
engine-authored truncation/retention details, and zero or more durable
attachment descriptors.
Every built-in returns its complete textual output; the engine is the sole
model-facing output-bounding and retention layer. `tool_output.max_lines`
(default 2,000) and `tool_output.max_bytes` (default 50 KiB) are frozen in each
session policy. When either limit is exceeded, the engine atomically writes
the complete text with private permissions to the project artifact store and
returns a head preview that itself remains inside both limits; structured
truncation metadata carries the opaque `artifact://sha256/<digest>` reference,
original byte count, and original line count outside that textual budget. A retention failure fails the
tool call instead of exposing an incomplete preview. (Live streamed output is
bounded in retention, not in total volume — §7.1.) Every tool call passes
through the permission pipeline before execution. Filesystem tools retain
canonical, symlink-aware containment checks rather than adopting OpenCode's
lexical-only Unix behavior.

Tool execution uses a strict **prepare once, approve, execute capability**
flow:

1. The tool validates and normalizes arguments, verifies that the current
   platform can provide the required descriptor/handle guarantees, resolves
   every resource once, and acquires the process-local execution capability.
   Unsupported platforms fail with `unsupported_platform` before any approval
   event is created.
2. Preparation creates immutable `PreparedApprovalResource` values. Each has a
   stable logical `PreparedResourceIdentity`, an exact capability and boundary,
   a `PreparedBindingLifetime`, and a `PreparedResourceDigest` computed with
   `cookie-agent.prepared-resource-digest.v6\0`. The digest binds the held
   descriptor/handle and validated object metadata; its canonical input never
   contains a raw path, file-descriptor/handle number, or temporary filename.
   `ApprovalResourceSource` records provenance only.
3. The engine constructs one `PreparedOperationIdentity` from the digest of
   normalized arguments (where resource arguments are replaced by logical
   resource/digest references), the complete sorted capability set, complete
   sorted prepared resources, the execution-context digest, and the fixed
   `process_local` capability-lifetime marker. The canonical identity includes
   each resource's capability, logical identity, and binding digest/lifetime.
   Approval boundaries are revision-bound consent policy and `source` is audit
   provenance; neither is execution identity. No raw path, FD/handle number,
   or temp identifier is permitted in this identity.
4. `OperationFingerprint` hashes that canonical identity with the explicit
   `cookie-agent.operation-fingerprint.v6\0` domain and length framing. The
   immutable identity, fingerprint, evaluations, and constraints are then
   presented for approval.
5. Approval authorizes only the already-held prepared capability. Execution
   consumes that capability directly; it never reopens a pathname, reruns path
   canonicalization, substitutes a new descriptor, or reconstructs a temp
   target from durable fields. A changed/replaced resource or lost binding
   fails closed as `operation_changed` (or `prepared_capability_lost`) rather
   than preparing or executing a different operation.

Prepared execution capabilities are process-local and non-serializable. On
daemon restart, every pending/approved-but-unexecuted prepared operation is
cancelled, recorded as interrupted/capability-lost, and never automatically
re-executed. A user/model retry creates and evaluates a fresh prepared
operation with a fresh binding digest and fingerprint. Replay rebuilds audit,
approval, title, and idempotency projections only; it cannot recreate an OS
capability.

`read` media attachments use the same engine-owned artifact boundary. The
engine resolves and opens the exact canonical read object with no-follow
semantics before any pending approval wait, then carries that held descriptor
through invocation. A symlink or path replacement while approval is pending
cannot redirect the eventual read or attachment. Before persistence the
engine requires a regular file, enforces a 20 MiB media limit, and performs
format-aware structural validation (PNG chunks/CRC/order plus bounded zlib
decode with exact scanline/filter validation, JPEG segments/scans, GIF blocks,
WebP RIFF/chunks, and PDF xref/trailer boundaries). Artifacts are
content-addressed by SHA-256 and created atomically relative to a held private
directory descriptor. Store roots, digest objects, and temporary objects must
be non-symlink objects of the expected type and current-user-owned where the
platform exposes ownership; existing and new artifacts are forced to mode
0600 and the root to mode 0700. Open-time cleanup removes only strictly named,
owned, non-symlink regular crash temps. Artifacts are represented in events
only by MIME, byte length, digest, and opaque reference. Arbitrary
binary/base64 is never written to session JSONL.
Attachments remain durable across daemon restart so Oven history replay
preserves them without writing arbitrary binary/base64 into JSONL.

Runtime `always` approvals remain durable and scoped to a delegation tree's
root session, rather than OpenCode's process-wide approval list. A configured
final wildcard deny hides a non-delegation built-in from the model-facing tool
definition; `edit` and `write` share the write permission alias.

### 7.1 Streaming and interactive tools

Tools may stream while executing. `ToolInvocationContext` carries a
**progress sink** and a **cancellation token**; through the sink a tool can
emit two kinds of output, which are deliberately distinct:

```
ToolCallProgress       structured status → session actor mailbox → persisted
                       (delegate uses this for coarse child progress)
ToolCallOutputDelta    raw output chunks → per-call output hub → live clients
                       (bash uses this for stdout/stderr), NEVER persisted
```

The three channels of an interactive call:

```
process stdout/stderr ──► clients, live (ephemeral deltas)
user input            ──► process stdin (run.tool_stdin, redacted audit event)
process exit          ──► bounded final result ──► model + session log
```

**Output hub.** Each streaming call owns an output hub, driven by the tool
task (never the session actor, so slow subscribers can neither block the
actor nor stall process draining). The hub:

- drains stdout/stderr into a bounded per-call ring buffer (a retention cap —
  the live stream itself is never truncated, only what the hub retains);
- emits deltas as envelopes `{ call_id, stream, byte_offset, data }` where
  `data` is **base64** (process output may be arbitrary non-UTF-8 bytes) and
  `byte_offset` counts **decoded** bytes, monotonic per `(call_id, stream)`;
- treats offsets as half-open ranges: a subscription snapshot covers
  `[start_offset, end_offset)` — with a `gap` marker when `start_offset > 0`
  (older bytes already evicted) — followed only by deltas starting at
  `>= end_offset`;
- queues that snapshot gap as `{ next_offset: start_offset }` on the live
  receiver too; `next_offset` is the first retained decoded-byte offset, so
  consumers know the exact lost half-open prefix `[0, next_offset)`;
- fans out to subscribers with bounded per-subscriber queues: a lagging
  subscriber receives a `{ gap: true, next_offset }` marker instead of
  backpressure; disconnected subscribers are dropped;
- on subscription, hands over **snapshot + registration atomically**: the
  client receives the current buffer content with its end offset, then only
  deltas with `byte_offset > snapshot_offset` — no lost or duplicated bytes;
- flushes all remaining output before the tool task returns, so the final
  deltas always precede the actor-persisted `ToolCallCompleted`.
- retains finalized hubs for the most recent 128 calls. A subscription to one
  of those retained finalized hubs receives its snapshot and an already-closed
  live receiver; it never occupies a subscriber slot until eviction.

**The model still receives exactly one bounded final result** when the call
completes — model adapters accept tool results only as complete messages, so
mid-call streaming is a human-facing feature, not a model-facing one.
For `bash`, stdout and stderr are simultaneously spooled to engine-owned
private temporary artifacts as they pass through the output hub; the tool
itself never accumulates unbounded vectors. Finalization atomically commits
the complete per-stream artifacts, adds their safe references and byte counts
to structured metadata, and constructs only a budget-bounded textual preview.
Retention/capture failure fails the call closed, while cancellation discards
live temps and startup removes validated crash leftovers. Stream offsets and
live subscriber ordering remain owned by the output hub and are unchanged.

**User → process stdin.** `run.tool_stdin { call_id, data?, eof? }`:

- `data` is base64-encoded bytes; writes to one call are ordered;
- `eof: true` closes the process's stdin (interactive programs often
  require explicit EOF);
- rejected with an error if the call is not running or is not
  stdin-interactive; pending input is discarded on cancellation;
- stdin input is user-driven, so it needs no separate permission grant
  beyond the call's own approval;
- **content is never persisted** (it may contain secrets, e.g. sudo
  passwords); each write records only a redacted audit event
  `ToolStdinSubmitted { call_id, byte_count }` in the session log.

bash runs with piped stdio in v0.1 (line-buffered programs behave well; some
programs buffer when not attached to a terminal). A `pty` option for fully
interactive programs (REPLs, debuggers) is deferred.

---

## 8. Permissions

Adapted from OpenCode's ordered-rule permission model.

### 8.1 Rules

```toml
[agents.primary.permissions]
[[agents.primary.permissions.rules]]
id = "no-force-push"
action = "bash"
resource = "git push --force *"
effect = "deny"

[[agents.primary.permissions.rules]]
id = "git-readonly"
action = "bash"
resource = "git status *"
effect = "allow"
```

- `action`: a permission capability — `read`, `write`, `bash`, `grep`,
  `glob`, `delegate`, `external_directory`, plus future capabilities.
  Rules always use **action** names, never tool names.
- Tool → action mapping: every tool maps to its same-named action, with one
  exception — the `edit` tool maps to the **`write`** action (both
  file-mutation tools share one capability, as in OpenCode's model).
- **No parent → child permission inheritance.** Every session's permissions
  are resolved fresh: global default configuration + the session's own
  profile overrides. Parent-profile rules never flow down. Global rules
  apply to every session in the tree. The
   parent's control over children is exercised through `allowed_profiles` —
   which profiles may be spawned — not through permission leakage.
- Rules are the complete configured policy surface; an action with no matching
  rule asks by default.

### 8.2 Pattern language: "simple wildcard"

Explicitly **not** filesystem globs (OpenCode's naming trap):

- `*` = zero or more of any character, **including** `/`
- `?` = exactly one character
- no globstar semantics, no escaping
- a trailing `" *"` is optional: `git status *` also matches `git status`

### 8.3 Matched resource per action

| Action | Resource matched |
|---|---|
| `bash` | each parsed sub-command's source (tree-sitter-bash extracts commands from pipelines, `&&`, substitutions); whole string if unparseable |
| `read`, `write` | canonical path, workspace-relative; paths outside the workspace are canonicalized (symlink-safe) and matched as absolute, gated by `external_directory` first |
| `grep` | the regex string |
| `glob` | the pattern string |
| `delegate` | target profile name |
| `external_directory` | canonical absolute `dir/*` patterns; evaluated before the underlying read/write |

Canonicalization of a path that does not yet exist (new `write` targets):
canonicalize the **nearest existing ancestor**, resolve symlinks there, then
append the remaining components lexically. Workspace/external classification
happens on the result.

### 8.4 Evaluation

1. Runtime approval store hit → `allow`.
2. Ordered rules, **last match wins**. Built-in guard defaults (`.env`,
   `external_directory`) are the lowest-priority rules; config layers append
   after them in the full configuration order
   (user < workspace < selected-profile overlay), so deeper layers win.
3. No matching rule → `ask`.

The `write` action is the shared alias for both `write` and `edit` tools.
External-directory approvals match canonical absolute patterns (normally a
directory pattern such as `/tmp/project/*`) before the underlying file action
is evaluated. Tool-local approval waits are asynchronous and cancellation
aware, so concurrent calls can each await their own decision without blocking
the session actor.

Multi-resource calls: every resource is normalized once and evaluated once;
the aggregate is deny, then ask, then allow.

**Every decision is explainable**: the trace (matched rule id, source layer,
all candidate matches, normalized resource, precedence reason) is emitted
with `ApprovalRequested` for durable internal evaluation. Clients render that
request as a user prompt only after its matching `ApprovalEscalated` event.
OpenCode's evaluator returns only a bare effect; we keep the full derivation.

### 8.5 Approvals

- Approval v6 is immutable, prepared, and fingerprinted. Every
  `ApprovalRequest` contains the trigger, one complete
  `PreparedOperationIdentity`, per-resource evaluations, response constraints,
  a revision, and the identity's exact `OperationFingerprint`. Prepared wire
  resources carry immutable logical canonical identities and domain-separated
  SHA-256 binding digests, not reopenable paths or serialized OS handles.
  Digests are exactly 64 lowercase hexadecimal characters; malformed,
  uppercase, empty, or wrong-length values are invalid. The resource `source`
  field is audit provenance and never changes the fingerprint.
- `ApprovalRequested` starts durable internal request/evaluation state and is
  never itself a user prompt. Internal policy/approval-agent work records an
  `ApprovalInternalDecision`: allow or deny finalizes without user UI, while
  ask/escalate emits `ApprovalEscalated` for the already-recorded exact request.
  Only that escalation makes the pending, unexpired request visible and
  respondable. Clients answer through `approval.respond` with `approve_once`,
  `approve_tree`, `reject`, or `cancel` plus optional feedback. There is **no
  scope editor**.
- `approval.respond` is accepted only for the exact `(session_id, approval_id,
  request_revision, operation_fingerprint)` and is idempotent by
  `client_response_id`. Reusing that ID with different parameters is an
  `idempotency_conflict`; stale revisions, changed fingerprints, and a binding
  that no longer names the prepared operation have stable typed
  `operation_changed` failures. Approval never causes argument/path/resource
  recomputation.
- `approve_tree` commits a server-authored `TreeApprovalGrant` containing the
  exact root session, capabilities, canonical resources/boundaries, and
  operation fingerprint. Grants are rebuilt from
  `TreeApprovalGrantCommitted` events, shared only by that delegation tree, and
  never broadened by client text. A process-local prepared resource cannot be
  constructed or decoded as a durable tree grant. Handle-bound filesystem
  operations therefore set `allow_tree_grant = false`; filesystem tree grants
  do not survive or apply across restart. Restart-stable non-filesystem grants
  are consent records only: a later call still prepares a new capability and
  must match the grant's complete stable identity before execution.
- `ApprovalUserDecisionRecorded` preserves the user answer;
  `ApprovalFinalized` records the final source/status/reason/feedback. Separate
  escalation, cancellation, operation-change/capability-loss, and doom-loop
  events make those states observable.
  Reject/cancel affects only the addressed pending operation. `approval.list`
  returns strict approval records and active tree grants for one root.
- Unattended/headless `ask` resolves to a final denial unless configured policy
  supplies another internal decision. The model receives a structured refusal
  and user feedback when present.

### 8.6 Built-in guards

- `.env` / `.env.*` reads default to `ask` (`*.example` allowed)
- `external_directory` defaults to `ask`

Permissions are consent control, not OS isolation — the harness runs with the
launching user's privileges (sandboxing is a documented non-goal), and bash in
particular should be treated as host-authority execution: command parsing
informs approvals but is not a security boundary. Descriptor-bound preparation
is nevertheless a security invariant: unsupported platforms fail before
approval, approval never authorizes a later path lookup, and a binding mismatch
fails closed instead of falling back to raw-path recomputation.

---

## 9. Configuration

Composed via Figment from built-in defaults and TOML only (later layers win):

```
built-in defaults
  < user TOML          ~/.config/cookie_agent/config.toml
  < workspace TOML     <repo>/.cookie_agent/config.toml
```

Abbreviated sketch (the checked-in `.cookie_agent/config.toml` is the complete
executable fixture):

```toml
schema_version = 5

[server]
host = "127.0.0.1"
port = 7419

[models.sonnet]
provider_id = "anthropic"
model_id = "claude-sonnet-4-6"
endpoint = "https://api.anthropic.com/v1"
adaptor = "anthropic"

[models.sonnet.auth]
type = "api_key"
value = "${env:ANTHROPIC_API_KEY}"

[models.sonnet.capabilities]
features = ["tool_calling", "reasoning", "max_output_tokens", "prompt_caching", "usage"]
cancellation = "local_only"
compaction = "unsupported"

[models.sonnet.capabilities.limits]
context = 200000
output = 64000

[models.sonnet.capabilities.modalities]
input = ["text"]
output = ["text"]

[models.sonnet.capabilities.media]
input = {}

[models.sonnet.capabilities.replay]
policy = "if_valid"
capability = "optional"
reasoning = true

[models.sonnet.settings]
thinking = "extended"
thinking_default_active = false
thinking_disable_allowed = true
effort = true
assistant_prefill = false
reject_non_default_sampling = false

[internal_agents.approval]
models = []
max_input_tokens = 16384
max_output_tokens = 2048
timeout_ms = 30000

[internal_agents.context_compaction]
soft_threshold_percent = 70
hard_threshold_percent = 85
target_percent = 50
max_summary_bytes = 262144
max_native_context_bytes = 2097152
persistence = "native_preferred"

[internal_agents.context_compaction.profile]
models = []
max_input_tokens = 16384
max_output_tokens = 2048
timeout_ms = 30000

[internal_agents.session_title.profile]
models = []
max_input_tokens = 4096
max_output_tokens = 128
timeout_ms = 10000

[internal_agents.session_title.policy]
max_chars = 80
max_input_messages = 4
generate_on_first_turn = true
fallback_to_input_excerpt = true

[agents.primary]
type = "primary"             # primary | subagent | all | internal (default: all) — see below
models = [
  "sonnet",
  "gpt",                    # another explicit [models.gpt] declaration
]
tools = ["read", "grep", "glob", "write", "edit", "bash"]

[agents.primary.delegation]
enabled = true
allowed_profiles = ["explorer", "reviewer"]
limit = 4                    # depth limit; unset root = Unlimited;
                             # unset child inherits parent's decremented limit

[agents.explorer]
type = "subagent"            # only spawnable via delegate; not user-invocable
models = ["gpt-mini"]
tools = ["read", "grep", "glob"]

[agents.explorer.delegation]
enabled = false
```

Notes:

- **Agent types**: every profile declares `type` —
  - `primary`: user-invocable root sessions only; listed in the UI.
  - `subagent`: spawnable only via `delegate`; hidden from user-facing
    profile lists.
  - `all`: either. Default when unset.
  - `internal`: engine-internal agents (e.g. the compaction agent). Never
    user-invocable, never delegate-spawnable, and **cannot be disabled** —
    they ship as built-in defaults and `[agents.*] enabled = false` is a
    validation error for them. Other types accept `enabled = false`.
  Enforced at two points: `session.create` rejects a `subagent`-only or
  `internal` profile for a root session, and delegate schema generation
  restricts its `profile` enum to `subagent`/`all` profiles (§5.1).
- **Model chain inheritance**: a profile with an **empty `models` chain
  inherits the parent session's resolved chain** (§6.3) — the default for
  subagent profiles that should use the parent's frozen model bindings, and for
  internal agents (compaction, title, and approval agents use the owning
  session/run's frozen model chain). Root sessions cannot use an inheriting profile: `session.create`
  fails validation when the resolved chain would be empty. `type = "all"`
   profiles with no chain are legal but fail as roots.
- **Engine-owned internal agents**: approval, context compaction, and session
  title generation use the bounded `[internal_agents.*]` fields shown above.
  Their empty model chains inherit the owning run's frozen chain. Generic
  `[agents.*] type = "internal"` profiles remain non-user/non-delegation
  profiles, but they do not configure those three engine-owned agents.
- **Model aliases**: chain entries resolve only through the immutable
  `ModelSet`. Unknown aliases fail validation. Policy snapshots retain
  `FrozenModelBinding` values, never auth/header secrets or live model handles.
- **Environment interpolation**: user/workspace TOML resolves `${env:NAME}`
  only at `models.<alias>.endpoint`, supported credential fields under
  `models.<alias>.auth`, and values under `models.<alias>.headers`. Model IDs,
  capabilities, settings, request defaults, provider options, agent aliases,
  and built-in defaults are never interpolated.
  Resolution is single-pass; `$$` escapes a literal dollar. Missing or
  non-UTF-8 allowed values fail without including resolved secrets in errors.
  There is no environment configuration layer: arbitrary environment variables
  never become config keys. They are available only to explicit `${env:...}`
  interpolation at the approved paths above. The TUI uses `COOKIE_THEME`, plus
  standard `NO_COLOR`, `TERM`, and `COLORTERM` hints.
- **Merge semantics**: figment's `merge` *replaces* arrays, which is wrong
  for permission rules (deeper layers must append). The config crate
  implements a custom layered merge: permission-rule arrays are concatenated
  across all layers in the order
  `built-in < user < workspace < selected-profile overlay`
  (each rule tagged with its source layer for decision traces); every other
  array (`tools`, `allowed_profiles`, …) is replaced by the deeper layer as
  usual.
- **Workspace loading and authority**: local `cookie` and `cookie daemon`
  startup each load `<cwd>/.cookie_agent/config.toml` unconditionally through
  the ordinary layered loader exactly once and validate the merged result
  before runtime composition. `attach` and `connect` do not acquire a current
  directory or inspect workspace configuration. There is no persisted
  workspace-acceptance state. A stale
  `~/.local/share/cookie_agent/trust.json` is inert: startup never locates,
  opens, parses, writes, migrates, warns about, or deletes it, including when it
  is malformed, a symlink, or a FIFO.
- **Threat-model consequence**: workspace configuration is repository-controlled
  authority input. Because permission arrays append in layer order and the last
  matching rule wins, a later workspace `allow` can override a matching user
  `deny`. The workspace layer can also select model endpoints and provide
  supported auth/header values through `${env:NAME}` interpolation. Users must
  therefore start a local runtime only in workspaces whose configuration they
  intend to apply. Configuration does not itself execute an operation:
  operation authority remains the frozen effective policy, any exact
  approval/tree grant required by that policy, and the descriptor-bound
  prepared capability identity checked immediately before execution.
- **Checked workspace secret policy**: the repository fixture's final rules
  allow ordinary canonical source-file reads, deny root/nested `.env` and
  documented credential/token/private-key paths, then deny all grep/glob
  enumeration. `.env.example` is the explicit non-secret exception. The
  enumeration deny is intentionally broad because current grep/glob prepared
  manifests expose root/pattern labels, not a complete per-file authorization
  surface; a path-specific rule could otherwise be bypassed by searching a
  broader root.
- Effective policy is snapshotted at session creation (§4.5).

---

## 10. Persistence

- **Authoritative format**: append-only JSONL event log per session
  (`events.jsonl`), whose first event (`SessionCreated`) carries the origin
  and policy snapshot. `meta.json` is a rebuildable cache for fast listing,
  never a second source of truth.
- **Delegation journal**: one append-only `delegations.jsonl` per project —
  the durability and uniqueness mechanism for delegate invocations (§5.4).
- Layout:

```
~/.local/share/cookie_agent/projects/<cwd-hash>/
    cwd                  (0600, exact canonical Unix path bytes; informational)
    delegations.jsonl
    grant-invalidations.jsonl
    artifacts/<sha256>  (private retained output and attachments)
    sessions/<session-id>/
        events.jsonl
        meta.json        (cache)
```

Project selection remains the existing `<cwd-hash>` behavior. The `cwd` file
does not participate in hashing, selection, collision detection, validation,
or migration; it exists only so future discovery code can identify candidate
folders without reversing the hash. On a normal open, a canonicalizable cwd is
written as its complete Unix `OsStr` byte sequence with no text conversion or
terminator. A missing, stale, or incorrectly-modeed file is replaced by a
private 0600 temporary file, fsynced, atomically renamed to `cwd`, and followed
by a project-directory fsync. An already-correct private file is retained.
Existing project directories gain the file when next opened, while sessions,
artifacts, and journals remain in place and project selection is unchanged.

- **In-memory projections** rebuilt from logs (and the journal) at daemon
  startup serve tree queries, listing, resume, and delegation recovery.
  All JSONL files undergo torn-tail truncation on load — every file is
  truncated to its last complete record before projections are built (§5.4).
  SQLite (`rusqlite`) is a future rebuildable projection if full-text search
  or large-history queries demand it — never the source of truth.
- Every persisted integrity digest uses the single `Sha256Digest` wire type:
  exactly 64 lowercase hexadecimal characters, validated on construction and
  decode. Prepared-resource bindings use
  `cookie-agent.prepared-resource-digest.v6\0`; operation fingerprints use
  `cookie-agent.operation-fingerprint.v6\0`. Both hashes length-frame their
  canonical bytes. The prepared-operation canonical bytes contain the
  normalized-arguments digest, sorted complete capabilities, sorted complete
  prepared resource identities/binding digests/lifetimes, the execution-context
  digest, and the process-local lifetime marker. Approval boundaries,
  provenance, raw paths, OS descriptor/handle numbers, and temporary
  identifiers are not canonical execution identity. Boundaries remain guarded
  by the immutable request revision. Content-integrity digests hash exact
  content bytes.
- Every stored session-event record carries the exact unforgeable
  `schema_version = 6`. Construction and decoding reject every other value;
  there is no old-event decoder.
- Crash recovery: incomplete runs are marked `interrupted` on restart. Pending
  delegation calls retain the idempotent journal recovery in §5.4, but pending
  prepared local tool operations are cancelled as
  `prepared_capability_lost` and never re-executed: OS capabilities are
  process-local and cannot be reconstructed from JSONL. Resume may start a new
  preparation only in response to a fresh model/user tool attempt.
- Compaction is event-sourced as described in §4.7. Raw history is always
  retained; only the assembled model-visible prompt uses the latest valid
  checkpoint. Native replay payloads are capped at exactly 2 MiB, native
  context payloads at exactly 32 MiB, and internal summaries at the configured
  limit with an absolute 2 MiB ceiling.

---

## 11. Protocol and transports

The protocol is layered so that **message semantics never depend on the
communication channel**:

```
Layer 1 — message model   JSON-RPC 2.0 requests/responses/notifications,
                          versioned via a protocol_version handshake
Layer 2 — stream          transport-agnostic duplex message stream:
                          abstraction       Stream<Item = Result<Message>> + Sink<Message>
Layer 3 — transports      framing adapters, interchangeable per deployment:
                            websocket   message-boundary-preserving (DEFAULT)  [v0.1]
                            in-process  tokio duplex channel (no serialization [v0.1]
                                        required for co-located clients)
                            stdio       newline-delimited JSON (NDJSON)        [deferred]
                            unix socket NDJSON                                 [deferred]
```

The engine-facing service behind the protocol speaks Layer 1/2 only: the
server owns JSON-RPC routing over a typed message stream, and engine APIs are
entirely transport-free. Adding a transport (TCP, named pipes, QUIC, a VS Code
IPC bridge, …) means writing a new Layer 3 adapter — the protocol, clients,
and engine are untouched. Codex's app-server and LSP are precedent for one
protocol riding multiple transports (their framing differs from ours; we cite
the idea, not the wire details).

Immediate consequences:

- **WebSocket is the default** network transport (works for TUI, web, VS Code).
- **The TUI uses the in-process transport** when it spawns/owns the daemon in
  the same binary, and WS when attaching to an already-running daemon — same
  client code either way.
- **v0.1 ships exactly two transports: in-process and WebSocket.** stdio and
  Unix sockets are designed but deferred (stdio then enables editor
  integrations that prefer child-process models).
- All network transports are localhost-only for the MVP.

Protocol surface (transport-independent):

- Every JSON-RPC envelope uses an exact `JsonRpcVersion` that can only emit or
  accept the string `"2.0"`.
- Handshake uses an exact `ProtocolVersion` that can only emit or accept the
  JSON number `6`.
- Durable events use an exact `EventSchemaVersion` that can only emit or accept
  the JSON number `6`. Every earlier protocol/event version, including 5, is
  rejected; no compatibility path exists.
- Methods (sketch): `session.create`, `session.list`, `session.get`,
  `session.children`, `session.tree`, `session.resume` (re-resolves
  interrupted pending tool calls, §5.4), `run.start`
  (optional frozen per-run `profile` override; idempotent via `client_run_id`,
  durably enforced by the engine; a
  conflicting reuse returns JSON-RPC `-32602` with
  `data.code = "idempotency_conflict"` plus `session_id` and
  `client_run_id`), `run.steer`, `run.cancel`,
  `run.tool_stdin` (ordered base64 writes + `eof` to a running tool call,
  §7.1),
   `events.subscribe` (cursor-based replay + live tail), `session.rename`
   (`Set`/`Clear`/`Reset`, idempotent through the replay-rebuilt
   `client_rename_id` index),
    `approval.respond`, `approval.list`, `catalog.provider.list`,
    `catalog.model.list`, `provider.connect`, `model.list` (revisioned runnable
    model snapshot), `agent.list` (user-invocable profiles: types
   `primary` and `all`, §9). Deferred additions: `session.fork`.
- Tool providers registered after the engine opens are visible only to a later
  model turn; an in-flight turn retains the tool set assembled when it began.
- Server → client notifications carry persisted events with per-session
  cursors; a lagging `events.subscribe` tail ends with `Gap { session_id,
  last_delivered_seq }`, which identifies the lagged subscription and carries
  the exclusive replay
  cursor. They also carry ephemeral output-delta envelopes (§7.1) with per-call
  byte offsets and atomic snapshot-to-live handoff for in-flight streaming
  calls. An output snapshot includes its `stdout` or `stderr` stream explicitly
  so an empty snapshot remains unambiguous. `approval.respond` includes the
  owning `session_id`, because approval IDs are resolved by that session's
  event log. It also carries the exact request revision and operation
  fingerprint; no scope field exists.
- `ts-rs` generates TypeScript bindings from `protocol` types; `schemars`
  generates JSON Schemas (tool parameters, protocol). Protocol-owned TS
  generation maps JSON integers to `number`, never `bigint`, and every
  `Option` omitted by Serde is an optional property (nullable on input).
- Wire enum encoding is stable: data-carrying protocol enums use internally
  tagged `snake_case` objects with a `type` discriminator, while unit enums
  are `snake_case` strings; `DepthLimit` uses adjacent `kind`/`value` tags,
  and JSON-RPC IDs/responses remain untagged as required by JSON-RPC 2.0;
  request IDs may be strings, integer numbers, or explicit `null` (which is
  distinct from an absent notification ID).

---

## 12. Frontends

- **TUI (MVP)**: ratatui client. Connects through the transport abstraction
  like any other client — in-process when it owns the daemon, WebSocket when
  attaching to a running one. No privileged access to engine internals.
  Renders the session tree, live child streams, approval prompts, and live
  tool output with stdin interaction (§7.1).
- `cookie_agent` binary crate: `cookie` (TUI, auto-spawns/connects daemon and
  opens a fresh root session by default), `cookie attach` (TUI attached to an
  existing daemon without replacing its session selection), `cookie daemon`
  (WebSocket transport; stdio deferred), plus non-interactive conveniences
  later.
- In the ordinary TUI view, Tab and Shift-Tab cycle a **client-local draft
  profile** across enabled `primary`/`all` profiles; `subagent`, `internal`, and
  disabled profiles are never user-selectable. Cycling sends no RPC, creates no
  session, changes no watched-session/cache state, and does not mutate the
  current session metadata. The draft is merely the optional `profile` field
  attached to the next `run.start` submitted for the current session. A failed
  submission leaves the local draft available for retry. Once `run.start` is
  accepted, `RunStarted` is authoritative and freezes the complete effective
  profile for that run as specified in §4.6. Further Tab changes affect only a
  later run; an accepted active run, all of its retries/fallbacks/tools,
  approvals, and internal work remain on their frozen profile. The new-session
  profile picker retains its own local Tab navigation while open, independent
  of this per-run draft.
- The TUI always stacks full-width regions vertically: a bounded Agents region
  at the top, the transcript, a status row when space permits, and the input at
  the terminal bottom. On tiny terminals the Agents region shrinks before it
  can displace or overlap the input. The input is a grapheme-safe multiline
  editor with three content rows by default (its border is outside those rows),
  display-column wrapping at the actual inner width, and a vertically scrolling
  viewport that keeps the cursor visible after edits and terminal resizes. A
  buffer ending exactly at the inner width has a trailing empty visual row, so
  the insertion cursor is at column zero on the following row. Crossterm resize
  events explicitly autoresize the ratatui terminal, invalidate rendering, and
  schedule an immediate redraw before later navigation uses the new width.
- Message and tool-stdin submission keep bare Enter as submit/send. Newlines use
  OpenCode's practical defaults: Shift-Enter, Ctrl-Enter, Alt-Enter, or Ctrl-J.
  This was compared against OpenCode commit
  `32f278b48f1a495611165d8a9f1ace0b512933e2`, specifically
  `packages/tui/src/config/keybind.ts` (`input_submit = return`,
  `input_newline = shift+return,ctrl+return,alt+return,ctrl+j`),
  `packages/tui/src/keymap.tsx` (the managed textarea input layer), and
  `packages/tui/src/component/prompt/index.tsx` (the multiline textarea and
  submit path). cookie_code requests crossterm's keyboard-enhancement protocol,
  so modified Enter works when the terminal/multiplexer reports it; legacy
  terminals may collapse modified Enter to bare Enter, which is
  indistinguishable and therefore submits. Ctrl-J remains the portable
  explicitly reported fallback in crossterm raw mode. The on-screen hint names
  Ctrl-J plus Shift/Alt-Enter and marks modified Enter as terminal-dependent.
- The conversation pane reserves its rightmost inner column for a scrollbar
  whenever its session can overflow. Content layout and block hit regions never
  extend into that column, so the track is always grabbable. Scrollbar state is
  the exact top offset plus the total rendered line height, resolved through
  one shared geometry helper used by rendering, hit testing, and drag math —
  no ratatui `ScrollbarState` position/content-length folding. Thumb **height**
  is strictly a function of total content height and viewport height
  (`ceil(viewport² / content)` clamped to the track), never of scroll offset,
  position, or follow state: for unchanged content and viewport, dragging
  top→middle→bottom keeps an identical thumb rect height, and at the maximum
  offset the thumb sits flush against the track bottom at full size while
  following re-engages without hiding or shrinking it. Thumb **top** is the
  only position-dependent value: offset 0 maps to the first track row and the
  maximum valid top offset `content − viewport` maps flush to the last.
  Mouse presses on the thumb capture a grab-anchored drag that resolves against
  the press-time geometry even when the pointer leaves the track; presses on
  the bare track page to the pressed position; release ends the capture.
  Resize and content mutation re-resolve geometry every frame from the same
  cached transcript layout, so the offset is always clamped into the valid
  range and the thumb height changes only when that geometry truly changes.
  Mouse wheel over the scrollbar column takes priority over content scrolling,
  and any scroll landing exactly on the last valid top offset re-engages
  live-tail following.
- Bracketed paste is enabled and normalized from CRLF/CR to LF before one
  atomic editor insertion, so pasted newlines never act as submit keys. A
  submission containing only whitespace is retained and ignored. Otherwise the
  exact multiline text is sent to `run.start`, `run.steer`, or tool stdin.
  Client slash commands are recognized only for single-line input; any input
  containing a newline is always sent verbatim as a prompt, preventing pasted
  multiline text beginning with `/` from executing a client command.
- Completed tool blocks render rich titles, text, structured metadata,
  truncation references, and attachment MIME/length/digest/reference metadata.
  They never render attachment bytes or raw base64.
- Protocol-v6 model projections render the exact safe configured/connected model
  identity, ordered replay dispositions, committed normalized usage and finish
  data, structured fallback errors, Oven model/provider tool-call identities,
  and whether an approval originated in policy or in the model. Native replay
  payloads and unsafe provider bodies are never rendered.
- Approval responses are optimistic and view-only: clicking or keying
  approve/reject/cancel atomically captures the exact
  (approval id, request revision, operation fingerprint, decision) tuple,
  dismisses the modal before any await, marks the request as submitting so
  duplicate actions are ignored, and shows a concise `Approval submitted…`
  status while `approval.respond` proceeds asynchronously. Nothing executes
  locally. The TUI's root/child queues, replay/live projection, and
  `approval.list` refresh admit only exact `Escalated`, pending, unexpired
  records; internal `Pending` requests remain durable but hidden and cannot
  produce `approval.respond`. Approval submission is global single-flight across sessions: while
  its pending marker exists, every queued approval remains hidden, and switching
  sessions does not clear that marker. The matching response clears the marker,
  while durable event/replay/list reconciliation may clear it first when the
  exact captured approval is no longer pending. Exact approval identity and
  monotonically assigned local request IDs make delayed callbacks no-ops, so
  they cannot clear a newer submission, overwrite its status, or restore stale
  UI. A transport/typed failure restores the modal with its exact captured
  identity only while the request is still escalated and unexpired —
  cancellation or expiry in flight never resurrects stale UI, and
  revision/fingerprint conflicts (`approval_revision_conflict`,
  `operation_fingerprint_mismatch`, `operation_changed`) trigger an
  approval-list refresh and are never silently resubmitted. Only after the
  in-flight submission resolves may the next queued approval become visible.
- `TextDelta` and `ReasoningDelta` remain unkeyed durable protocol events; the
  TUI projects their authoritative sequence order into one assistant transcript
  item per model attempt. Each item owns ordered `Text` and `Thinking` child
  segments. Consecutive deltas of the same kind merge into one child keyed by
  the first delta sequence; a kind transition creates a new stable child.
  `ModelReplayEvaluated` (new attempt), `ModelTurnCommitted`,
  `AttemptAbandoned`, `ToolCallStarted`, run terminal events, and user-turn
  boundaries close the open projection. The open assistant is tracked directly,
  not inferred from the transcript tail, so reasoning-before-text,
  text/thinking alternation, thinking-only output, and interrupted partial
  output remain one assistant item for that attempt. Tool items remain separate
  siblings. Replay continues to build a scratch projection and swaps it only at
  a validated `ReplayEnd`; an incremental `ReplayStart` closes the cloned open
  assistant before reducing replayed events so segments never merge backward
  across the replay boundary. No protocol/event or persistence schema changes
  are involved.
- Assistant `Text` children are parsed into a TUI-owned block model from
  CommonMark/GFM events with `pulldown-cmark` 0.13.4 and rendered directly as
  ratatui spans. Headings, emphasis, inline code,
  links (including a visible destination), lists, quotes, task markers,
  rules, and fenced code have terminal-native layouts. GFM tables render as
  semantic grids — box borders, a bold (never color-only) header row,
  per-column left/center/right alignment — computed from display widths with
  deterministic shrink-widest-first allocation; cells wrap on grapheme
  boundaries without overflowing, and below the minimum useful width the
  table degrades to a readable stacked `Header: value` form. Cell inline
  markup keeps its semantic styles (inline code has no contrasting
  background), cell text is control-character sanitized, and streamed
  incomplete tables complete through the ordinary tail reparse. Table rows
  participate in the owning assistant gutter, layout cache, hit regions, and
  resize reflow like any other content. Fenced code is
  highlighted through a TUI-owned `Highlighter` trait backed by `syntect` 5.3.0;
  unknown languages, unclosed fences, unavailable syntax/theme data, and
  highlighting errors fall back to plain text rather than dropping content.
  Inline code never uses a contrasting background: it keeps the surrounding
  assistant-text background and is distinguished by a semantic foreground plus
  bold, with the source backticks remaining visible, so the distinction never
  relies on color alone in mono/no-color or high-contrast themes.
- Expanded `read` tool output is syntax-highlighted through the same
  highlighter and theme quantization. The language is inferred
  deterministically from the extension of the exact `path` argument recorded
  in the tool call; unknown extensions, failed calls, binary/image/PDF
  summaries, and trailing engine-authored metadata (truncation retention
  references, attachment descriptors) always render plain. Highlighting is a
  render-layer concern only: the tool gutter, wrapping, truncation/artifact
  references, and the per-item layout cache (keyed by item identity/version,
  width, theme, and interaction state) are unchanged.
- Streaming Markdown uses an incrementally committed stable prefix plus a
  reparsed open tail. An unfinished paragraph/list/fence remains in the open
  tail until a subsequent block makes the preceding block stable. Stable blocks
  record reference-link/image dependencies and the effective definition
  signatures that resolved them. A new or still-streaming definition that
  changes one of those signatures triggers a bounded full-item reparse, keeping
  incremental output semantically identical to full CommonMark parsing without
  penalizing ordinary reference-free streaming.
- Transcript layout is cached per item by stable item identity, item version,
  width, expansion state, session generation, and theme. Assistant child
  layouts are additionally cached by stable child identity/version plus width,
  theme, expansion, selection, and render-only streaming state. A streamed
  child delta or tool-output delta therefore invalidates only its owning child
  or item rather than the complete transcript. The assembled line list and
  block hit regions come from the same cached layouts, preserving scroll and
  mouse-hit safety.
- User, assistant, tool, error, and internal-event items have explicit textual
  headers and distinct gutter shapes; color is supplemental, never the only
  role or state signal. An assistant item has exactly one `ASSISTANT` header and
  continuous outer gutter. A `Thinking` child is an in-place collapsible row
  (`▸ thinking (N lines hidden)` / `▾ thinking`) whose expanded body is plain,
  wrapped reasoning-styled text under an inner sub-gutter; it is never parsed as
  Markdown or syntax highlighted. Only the latest open thinking child adds the
  render-only streaming `…`, removed at a kind/attempt/run boundary. There is
  no top-level reasoning item or `REASONING` header. Narrow terminals use one
  compact `[A]` tag and never an `[R]` tag while the existing always-top Agents,
  transcript, status, and bottom multiline-input stack remains authoritative.
- Collapsible thinking/tool blocks are mouse-first: clicking a block's hit
  region selects and toggles it, and `/block next|previous|toggle|clear`
  provides the discoverable keyboard-accessible command path. Navigation,
  reveal order, and mouse hit regions flatten thinking children and tool
  siblings in actual render order. Expansion is keyed per session and
  child/tool identity, so it survives streaming, replay swaps, and session
  switches; selection is stable across scrolling and resize and is cleared when
  switching sessions or when the selected segment disappears. Every toggle row
  shows **exactly one chevron** — `▸` collapsed, `▾` expanded; selection,
  hover, and focus are conveyed only through style (underline emphasis on the
  role color, distinct in mono/no-color), never through a second triangle or
  other selection glyph.
- The Agents tree has a stable hierarchy root independent of the currently
  viewed session. Clicking or `/watch`-ing a descendant changes only the
  conversation and the highlight: the root snapshot is retained, every tree
  refresh keeps querying the original root, and the cursor is retained by
  `SessionId` across refreshes rather than by row index. The watched session
  carries a distinct `●` marker; the cursor row is styled separately. Selecting
  a session in the `/sessions` picker is the intentional reroot action and is
  the only gesture that replaces the root. Tree rows show the durable session
  title prominently (profile name as fallback) with only a shortened ID as
  subdued secondary metadata — never the full session UUID.
- The `/connect` provider picker and the `/sessions` picker filter
  responsively while typing. Provider matching covers name, ID, documentation
  URL, endpoint, and credential field labels (the matched field is annotated);
  session matching covers title, profile, and full ID. Backspace edits the
  query, Ctrl-U clears it, an empty result renders a no-results state, and
  keyboard and mouse selection operate on the filtered rows. Credential
  masking, buffer zeroization, and request redaction are unchanged by
  filtering.
- The TUI loads one strict, independent configuration file —
  `$XDG_CONFIG_HOME/cookie_agent/tui.toml`, falling back to
  `~/.config/cookie_agent/tui.toml` — with no workspace layer and no
  environment-variable override. Schema version 1 has exactly two optional
  keys: `minimum_event_level` (`debug|info|warning|error`, default
  `warning`) and `theme` (`default|mono|high-contrast`). Precedence is
  `tui.toml theme` > `COOKIE_THEME` > terminal detection, with `NO_COLOR` and
  `TERM=dumb` always forcing monochrome. A missing file yields defaults;
  unknown keys, wrong types, and invalid values are rejected with an
  actionable path/key error that never echoes file contents. The file loads
  identically for the in-process and attached TUI without touching engine
  config or protocol.
- Model warnings are projected as dedicated warning-level diagnostic items —
  never error items. Each warning names its exact owning configured/connected
  model identity (the model recorded by the owning `ModelTurnCommitted`),
  renders with the warning (yellow) semantic style, and carries a textual
  `[W]` header so a child-session warning is never attributable to the parent
  and mono/no-color output keeps the distinction. Ownership stays durable in
  the child session's own event projection: warnings are never injected into
  the parent's transcript, tool results, or model-visible context. Because the
  TUI subscribes every session in the watched delegation tree, the conversation
  view additionally aggregates warnings from strict descendants of the viewed
  session as read-only warning rows attributed with the child session title
  (profile fallback), the shortened session ID as secondary metadata, and the
  owning model identity — nested descendants surface all the way to the root
  view. The viewed session's own warnings render locally inside its transcript,
  so a warning never appears twice in one view. Replay swaps and tree refreshes
  reproduce the same attribution because the aggregate is recomputed per frame
  from the durable per-session projections and the current tree.
- Diagnostic rows (formerly undifferentiated internal/error notices) are a
  TUI-only leveled projection: each reduced diagnostic carries an exact
  `EventLevel { Debug, Info, Warning, Error }` classified centrally at
  reduction time; durable protocol events are unchanged. DEBUG is low-level
  replay decisions/details, cache IDs/scopes, and subscription/recovery
  internals; INFO is routine run/model/internal-agent lifecycle plus
  successful checkpoints and title commits; WARNING is model warnings, any
  discarded/foreign/invalid/reconstructed replay or context-cache
  disposition, compaction fallback, truncation, retries, and abandoned
  attempts; ERROR is run/tool/internal-agent/approval/protocol/replay
  failures and unrecoverable storage/transport errors. Context/native replay
  discard dispositions render as WARNING with the exact model and a concise
  human-readable reason, never a generic error. Filtering is view-only: it
  applies only to diagnostic rows — never user/assistant text, thinking,
  tool results, approvals, or session state — hidden rows remain in the
  projection and replay, and lowering the threshold reveals them without
  refetch. The conversation title shows the active filter, and
  `/events debug|info|warning|error` changes it for the current view without
  rewriting `tui.toml`. Every row renders a textual badge (`[D] [I] [W] [E]`)
  with the level's semantic style, so levels are distinguishable in
  mono/no-color. Root aggregation of child warnings remains attributed by
  title/profile/model/short session id and uses the viewing TUI's threshold;
  persistence and model context are never affected.
- The TUI theme layer provides `default`, `mono`, and `high-contrast` semantic
  palettes. `COOKIE_THEME=mono`, `NO_COLOR`, or `TERM=dumb` forces
  monochrome regardless detected terminal capability. High contrast uses a
  distinct bright ANSI-16 palette (plus explicit modifiers for inline code and
  selection), rather than default RGB colors with added bold. Default-theme
  RGB colors are quantized to true-color, ANSI-256, or ANSI-16 capabilities
  before ratatui rendering. Styling remains readable in monochrome through
  labels, glyphs, borders, and text modifiers.
- Web and VS Code are deferred frontends consuming the generated TS bindings.

---

## 13. Tech stack

| Area | Crate |
|---|---|
| Runtime | tokio (rt-multi-thread, macros, sync, time, process, signal, fs, io-util, net) |
| Server | axum 0.8 (ws), tower-http (cors, request-id, trace) |
| Wire | serde, serde_json; schemars; ts-rs |
| Models | exactly pinned Oven SDK and explicit adapter crates |
| Config | Figment (built-in defaults + TOML composition only; no environment layer) |
| CLI | clap 4 (derive) |
| Process exec | process-wrap with the `tokio1` feature (process groups / job objects) |
| Search | ignore, regex |
| Shell parsing | tree-sitter-bash (sub-command extraction for permissions) |
| Diff display | similar (rendering only) |
| IDs / time | uuid v7, jiff (RFC 3339 serialization) |
| Errors / observability | thiserror, anyhow, async-trait, tracing + tracing-subscriber (env-filter) |
| TUI (MVP) | ratatui 0.30, crossterm, pulldown-cmark 0.13.4, syntect 5.3.0 (`regex-fancy`) |
| Testing | `ScriptedModel`, insta (json + redactions), tempfile; published Oven adapter suites |

MSRV: Rust 1.88 (ts-rs floor).

---

## 14. Testing strategy

1. **`ScriptedModel`**: deterministic in-memory Oven model queuing
   `Result<StreamPart, ModelError>` items, delays, and cancellation points —
   the primary agent-loop test double.
2. **Snapshot tests** (insta) for protocol events, tool transcripts, and
   config snapshots; IDs/timestamps redacted.
3. **Published Oven adapter suites** own HTTP auth, error, framing, and stream
   conformance; this workspace verifies exact adapter pins, explicit
   construction, capability honesty, and request/replay integration.
4. **Delegation lifecycle tests**: idempotent re-delivery at every crash
   window in §5.4 (including the linked-but-unstarted child, the
   reservation-without-journal-record window, journal append-failure
   rollback, and journal reservation races); depth-limit arithmetic;
   cancellation propagation; torn-tail truncation.
5. **Permission evaluation tests**: rule precedence, bash sub-command
   parsing, path matching, decision traces, and the explicit layer consequence
   that a later workspace allow overrides a matching user deny. Protocol-v6 approval tests cover
   exact version-6 handshakes/events, strict version-5 rejection, golden
   operation/resource domain hashes, complete prepared identity, provenance
   exclusion, invalid/duplicate resource bindings, process-local tree-grant
   rejection, `operation_changed`, unsupported-platform pre-approval failure,
   restart cancellation, and proof that replay never executes or reconstructs
   a prepared capability. JSON Schema and TypeScript snapshots are regenerated
   from the same strict types.
6. **CLI/config startup tests**: removed workspace-acceptance CLI input is
   rejected before and after `daemon`; local noninteractive startup loads and
   validates workspace TOML directly; logical paths survive diagnostics;
   attach/connect remain workspace-independent; no acceptance artifact is
   created; malformed, symlink, and FIFO stale `trust.json` objects remain
   untouched without blocking startup.
7. Tool tests on `tempfile` workspaces (edit conflict handling, atomic writes).
   Media coverage includes approved image/PDF persistence, malformed and
   oversize rejection, path-scoped permission enforcement, private atomic
   storage, restart/replay, and absence of binary/secret bytes in JSONL.
8. **Streaming/interaction tests** (§7.1): delta ordering and stdout/stderr
   separation, ring-buffer truncation, gap markers for lagging subscribers,
   atomic snapshot-to-live handoff (no lost/duplicated bytes), disconnected
   clients, stdin ordering/EOF/rejection-for-non-running calls, cancellation
   discarding pending stdin, final deltas preceding `ToolCallCompleted`, and
   only the bounded final result entering model history.
9. **TUI rendering tests**: deterministic inline snapshots cover Markdown,
    syntax-highlighting fallback/aliases, and semantic themes; Unicode wrapping,
    forward reference links/images, tiny terminals, monochrome operation,
    merged assistant thinking/text ordering, attempt/tool/terminal boundaries,
    replay-stable child IDs, streaming indicators, independent thinking/tool
    navigation and hit regions, keyboard-only operation, and block hit regions
    are exercised directly. Parse-byte/pass plus per-item and per-child
    layout-pass counters enforce deterministic streaming budgets without
    wall-clock assertions.

CI runs protocol formatting, checking, warning-free clippy, tests, snapshot
verification, and rustdoc on stable Rust and the Rust 1.88 MSRV. Version-5
goldens are rejection fixtures only; no compatibility decoder is built.

---

## 15. Current implementation and v6 foundation

The completed v0.1/Oven integration is defined by these end-to-end properties:

**Daemon and engine**
- engine + server composed by `cookie_agent`; JSON-RPC over the transport
  abstraction with exactly two transports: **in-process** and **WebSocket**
- per-session actors with out-of-mailbox tool execution (§4.1), steering at
  turn boundaries, full cancellation

**Models and tools**
- Models: immutable configured `ModelSet` with explicit Oven adapters;
  alias-only per-agent fallback chains with error classification, per-run
  stickiness, and `ModelFallback` events (§6.3)
- revisioned models.dev provider/model catalog, request-only provider
  credential connection, and revisioned runnable `model.list` projections
- Tools: read (including directory listings), write, edit (optimistic exact-match), bash (process groups,
  streamed stdout/stderr, user stdin), grep, glob — plus
  auto-injected `delegate`
- All tool outputs capped with truncation signaling

**Configuration and permissions**
- Figment-composed configuration layers (`built-in defaults < user TOML <
  workspace TOML`) with the custom permission-rule concatenation merge;
  environment values are available only through restricted explicit
  `${env:NAME}` interpolation in approved model fields (§9)
- agent profiles with delegation blocks; policy snapshots at session
  creation (child snapshots contain global rules + their own profile only —
  never parent-profile rules)
- agent types (`primary` / `subagent` / `all`) enforced at `session.create`
  and in delegate schema generation (§9)
- unconditional validated workspace loading with no persisted acceptance state;
  stale `trust.json` objects are ignored untouched
- ordered permission rules with ask-by-default fallback and decision traces;
  exact approval-v6 requests, decisions, doom-loop/cancellation events, and
  server-authored tree grants (§8)

**Sessions and subagents**
- JSONL event logs + delegation journal (journal actor, torn-tail recovery);
  resume with pending-tool-call re-resolution; startup reconciliation of
  partial delegations (§5.4)
- provenance (`Root`/`Delegated`), tree queries, `ToolCallLinked` backlinks
- delegate lifecycle: depth-limit arithmetic, journal idempotency, cancellation
  propagation, bounded results
- clients can observe and steer any session in the tree live
- durable bounded context checkpoints, generic internal-agent lifecycle,
  session titles/rename semantics, and frozen per-run profile overrides

**Frontend**
- ratatui TUI over in-process or WS transport: stream, steer, answer
  approvals, watch live tool output and respond over stdin, browse/expand
  the live subagent tree; incrementally render Markdown and highlighted fenced
  code with semantic no-color-safe message blocks

**Explicitly deferred beyond the current implementation**: `session.fork`,
stdio/Unix-socket transports, MCP, web frontend, VS Code extension, SQLite
projection, project-persistent approvals outside a session tree, and plugin
models. Compaction, titles, approval v6, draft-profile switching, and the
models.dev catalog/connect protocol are current v6 design, not deferred work.

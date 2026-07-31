# cookie_code Architecture

**Status:** working draft — the design record we iterate against.
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
4. **Capability-aware providers.** Providers differ materially (tool calls,
   reasoning, caching, structured output). The engine negotiates capabilities
   instead of reducing everyone to a lowest common denominator.
5. **TOML configuration.** Layered, profile-based, snapshotted at session
   creation. Live config edits never mutate in-flight sessions.
6. **Delegate-only delegation.** The only way a child session comes into
   existence is a model calling the `delegate` tool. There is no declarative
   workflow engine and no client-side fan-out API.

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
              │    providers     │  capability-aware provider trait
              │  ┌─────────────┐ │  ┌ anthropic · openai (completions +
              │  │  adapters   │ │  │ responses) · openai-compatible
              │  └─────────────┘ │  └
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
providers ◄── engine ◄────── tools        (built-ins + delegate tool)
               │
               ▼
             config
```

- `server` → `protocol`, `engine`
- `engine` → `protocol`, `providers`, `config`
- `tools` → `engine` (implements `ToolProvider`; delegate reaches the engine
  exclusively through its client API — an in-process handle — which is what
  keeps it splittable into a separate binary later)
- `tui` → `protocol`, `server` (client side only; `server` supplies the
  current `MessageStream` transport adapters for in-process and WebSocket
  connections, never engine APIs)
- `cookie_agent` → `engine`, `providers`, `tools`, `config`, `server`, `tui`

`engine` never imports `tools`; the composition root registers the built-in
and delegate providers into the engine's tool registry.

---

## 3. Workspace layout

```
crates/
  protocol/              # JSON-RPC types: commands, events, session/tree models
                         # schemars (JSON Schema) + ts-rs (TypeScript bindings)
                         # + transport layer (§11): stream abstraction,
                         #   websocket (default) · stdio · unix socket · in-process
                         #   (ws behind a feature flag to keep the crate lean)
  providers/             # Provider trait, capabilities, normalized events
                         # + adapters as modules:
                         #   anthropic (reqwest SSE) · openai (Completions +
                         #   Responses) · openai-compatible (base_url config)
  engine/                # session/run actors, agent loop, event log,
                         # provenance, permissions, compaction, tool runtime,
                         # ToolProvider trait
  tools/                 # built-in tools: read, write, edit, bash, grep, glob, list
                         # + delegate tool provider (§5) — a tool provider that
                         #   calls the engine only through its client API
  config/                # layered TOML (figment), profiles, policy snapshots
  server/                # axum daemon: WS listener, daemon lifecycle,
                         # session/run service behind the protocol
  tui/                   # ratatui client (pure protocol consumer)
  cookie_agent/            # thin binary (composition root):
                         #   `cookie_agent` (TUI), `cookie_agent daemon`, ...
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
  deterministic provider tool-call order.
- Concurrent tool execution is *backpressured* by bounded channels — an
  implementation detail, not a policy limit. There are no user-visible
  parallelism caps by design (§1).
- **Steering**: clients may inject input into a running turn; accepted input is
  persisted as `UserInputSubmitted`. The actor persists `UserInputApplied`
  before the next safe provider attempt (never mid-tool-execution), including
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
ToolCallFailed          ApprovalRequested       ApprovalResolved
ToolStdinSubmitted      (redacted audit: byte count only, §7.1)
ToolCallLinked          (delegate call → child session backlink)
AttemptAbandoned        (failed model-attempt boundary; not prompt history, §6.1)
TurnOpaque              (provider-native assistant continuation artifact, §6.2)
ModelFallback           (chain advance: from model, to model, reason, §6.1)
UsageReported
```

Every **persisted** event carries: session ID, run ID (when applicable), a
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

Tree queries follow `Delegated` edges only. Forks (post-MVP) do not
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
| Follow up on a completed child | **forks** the child (post-MVP); original stays immutable as the record of what fed the parent. v0.1 children are read-only after completion |

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

Result bounding: profile-level cap (16–32 KiB model-visible) with truncation
signaling (`truncated: true`, byte count). Full-output artifact storage is a
post-MVP addition; MVP truncates and says so.

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

The same retained-active/reopen-required rule applies if persisting provider
attempt state (deltas, usage, or `TurnOpaque`) fails: the provider loop surfaces
the error and attempts its terminal append once; if that append also fails, the
active entry remains as a tombstone and no in-process recovery is attempted.

`invocation_id` is derived by the engine from
`(parent_session_id, parent_run_id, parent_tool_call_id)` — all
engine-generated, so the tuple is globally unique by construction; it never
depends on provider-supplied tool-call IDs.

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
so the persisted log's annotation position cannot produce invalid provider
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
injected into the cancelled parent. Cancellation can race provider scheduling:
the engine catches it immediately after run start, while child tool execution
remains permission/cancellation guarded. Abandoning a delegate-result wait
schedules the same child cancellation when a Tokio runtime handle is available;
teardown outside any live runtime is best-effort.

---

## 6. Providers

```rust
// dyn-compatible: providers are registered as trait objects selected at runtime
#[async_trait]
trait Provider: Send + Sync {
    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities;
    async fn stream(&self, request: ProviderRequest)
        -> Result<BoxStream<'static, Result<NormalizedEvent, ProviderError>>, ProviderError>;
}
```

`ProviderCapabilities` covers: tool calling, parallel tool calls, streaming
tool-argument deltas, reasoning deltas (and replayability), image/PDF input,
structured output, prompt caching, context/output limits, usage reporting,
cancellation semantics.

Normalized stream events: `TextDelta`, `ReasoningDelta`, `ToolCallStart`,
`ToolArgsDelta`, `ToolCallEnd`, `Usage`, `Stop`. Raw provider payloads (or
stable references) are preserved in the event log for debugging, but behavior
is driven only by normalized events. No SDK types leak beyond adapter crates.

MVP adapters:

| Module | API | Notes |
|---|---|---|
| `providers::anthropic` | Anthropic Messages (SSE) | reqwest + eventsource-stream |
| `providers::openai` | Chat Completions **and** Responses | distinct streaming semantics; selected per model/endpoint config |
| `providers::openai_compatible` | Chat Completions + custom `base_url` | covers Ollama, LM Studio, OpenRouter, vLLM, … |

Credentials: environment variables only, referenced from TOML
(`api_key_env = "ANTHROPIC_API_KEY"`). No secrets in config files.

### 6.2 Round-trip fidelity

Normalized events drive *behavior*; they are **not sufficient to reconstruct
provider requests**. Several formats carry opaque continuation state that
must be replayed verbatim in later requests:

- Anthropic: signed `thinking` / `redacted_thinking` blocks, exact block
  order, `tool_use` IDs, `cache_control` positions
- OpenAI Responses: `reasoning` items with `encrypted_content`, item IDs,
  `function_call.call_id` pairing, hosted-tool items
- OpenAI Completions: `reasoning_content` where the endpoint requires replay,
  exact `tool_calls` echo with serialized arguments

Therefore: adapters emit **opaque provider-state artifacts** alongside
normalized events (per assistant turn and per block where applicable). The
engine persists each one as the durable `TurnOpaque` event in the session log
with its assistant turn, and request assembly
replays them through the same adapter's history encoder — provider-native
data takes precedence over normalized reconstruction for every turn it
exists for; normalized reconstruction is the fallback for synthetic turns
(steering, future compaction summaries). An adapter that cannot honor an
opaque artifact (e.g. provider changed across a fallback advance) discards it
explicitly and degrades to normalized replay.

The provider-domain representation is
`NormalizedEvent::TurnOpaque { state: AssistantTurnOpaque }`. An artifact is
tagged with its exact `ProviderProtocol` and holds an untyped JSON `payload`;
the payload is intentionally provider-native rather than a lossy common
schema. The persistence/assembly boundary is:

```rust
struct PersistedTurn { message: ProviderMessage, opaque: Option<AssistantTurnOpaque> }
struct EncodedHistory { system: Vec<Value>, messages: Vec<Value>, discarded_opaque: bool }
```

`ProviderRequest` carries `persisted_turns: Vec<PersistedTurn>` for request
assembly. When it is non-empty, the selected adapter invokes its history
encoder while building the actual HTTP body; `messages` remains the
normalized-only path for callers that have no persisted transcript.

`TurnOpaque` is an ordinary persisted protocol event (and therefore survives
JSONL reopen), tagged with its `ProviderProtocol` and untyped native payload.
`ToolCallStarted` also retains the issuing protocol and provider-native call
ID beside the engine-generated call ID. On same-protocol replay, prompt
assembly uses those native IDs verbatim for tool calls and results; on a
cross-protocol fallback it uses engine IDs while the selected adapter marks
the foreign artifact discarded and reconstructs that turn normally. Artifacts
remain in the log after a fallback, so a later run starting at the chain head
can replay them again.

The Anthropic and compatible adapters expose
`encode_history(&[PersistedTurn]) -> EncodedHistory`; OpenAI exposes
`encode_history(&[PersistedTurn], OpenAiEndpoint) -> EncodedHistory` to select
Chat versus Responses. A matching artifact contributes its native assistant
message/items verbatim;
an artifact tagged for another protocol sets `discarded_opaque` and the
adapter rebuilds only that turn from `ProviderMessage`. Anthropic artifacts
contain the ordered assistant content blocks plus stop/usage state; Chat
artifacts contain the assistant message including exact tool-call echoes and
reasoning fields; Responses artifacts contain ordered output items including
encrypted reasoning and hosted-tool items.

Each adapter is implemented and tested against the conformance checklist in
`docs/provider-conformance.md` (derived from OpenCode's provider layer).
MVP scope is the three formats below; Gemini, Bedrock, and Azure adapters
are post-MVP and target the same checklist. An `openai_compatible` endpoint
may claim only: chat text, tool-call echo, tool-result pairing, basic
SSE/429 handling — every advanced feature is capability-probed before use.

### 6.1 Fallback chains

Each agent profile configures an **ordered model chain** instead of a single
model:

```toml
[agents.primary]
models = [
  { provider = "anthropic", model = "claude-sonnet-4-6" },
  { provider = "openai",    model = "gpt-5" },
  { provider = "local",     model = "qwen3-coder" },
]
```

Semantics:

- **Error classification** drives behavior. `ProviderError` carries a class:
  - *entry-retryable* — rate limit (429), overloaded, 5xx, network/timeout,
    dropped stream: retry the same entry with exponential backoff (default
    2 retries), then advance to the next chain entry.
  - *entry-terminal* — auth failure, invalid request, or model not found:
    skip the entry immediately (no retries), advance to the next entry. Known
    provider error-body `code` values `model_not_found`, `invalid_model`,
    `model_does_not_exist` (including `model_doesnt_exist` and
    `model_not_exist`) take precedence over HTTP status heuristics, including
    5xx responses.
  - *run-terminal* — context overflow (MVP), cancellation: fail the run;
    no fallback. (Post-MVP, overflow triggers compaction instead.)
- **Request assembly is per-attempt**: each entry's request is built against
  *that* model's capability set. If the primary supports reasoning or prompt
  caching and the fallback doesn't, the fallback request simply omits them.
  Tool schemas are provider-normalized, so they carry across entries.
- **Partial output is abandoned on fallback**: if a stream fails midway, the
  partial deltas are discarded client-visibly (marked abandoned), and the
  completion restarts on the next entry against the same committed
  conversation state. The engine keeps the raw events in the JSONL audit log,
  then appends `AttemptAbandoned`; prompt assembly discards the accumulated
  assistant state for that attempt at this boundary. Earlier committed turns
  and tool results remain in the next request. `ModelFallback` remains the
  sole provider-chain advance/degradation signal. Committed tool results are
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
  guessed. If omitting a call would make an opaque
  assistant artifact invalid, that turn falls back to normalized replay.
- **Meaningful-output retry guard:** the engine's streaming attempt runner is
  the single same-entry retry layer and uses the provider executor's rule:
  retry only before text, reasoning, or tool-call output has been observed.
  A retryable failure after meaningful output advances directly to the next
  fallback entry, never receives a second same-entry retry.
- **Per-run stickiness**: once the chain advances, the remainder of the run
  stays on the new entry (no flip-flopping under sustained rate limiting);
  the next run starts again from the chain head.
- Every advance is persisted as `ModelFallback { from, to, reason, attempts }`,
  and `UsageReported` always records which model actually served, so cost and
  behavior stay attributable.
- The resolved chain lives in the session's policy snapshot (§4.5); editing
  TOML mid-run never reorders a live session's chain. Children get their own
  chains from their own profiles — or **inherit the parent's resolved chain**
  when their profile's `models` is empty (§9). Internal agents (compaction)
  run on the inheriting session's chain by default.

---

## 7. Tools

The engine exposes a generic tool-provider interface:

```rust
// dyn-compatible for the same reason: a runtime registry of providers
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

In-process providers for MVP (built-ins + delegation); the same interface
later covers remote tool servers (MCP).

MVP built-ins (modeled on OpenCode's basic set):

| Tool | Tier | Notes |
|---|---|---|
| `read` | read | line ranges, size caps |
| `list` | read | directory listing |
| `grep` | read | `ignore` + `regex` as libraries; honors .gitignore |
| `glob` | read | `ignore` traversal |
| `write` | write | atomic (temp + rename) |
| `edit` | write | optimistic exact-match editing: verify expected occurrence count, replace, **re-verify the file hash immediately before the atomic rename**; on mismatch, fail with a conflict result. Engine writes are serialized per path. Concurrent *external* writers can still interleave between read and rename — documented limitation, mitigated by the pre-rename hash check. **No fuzzy matching.** `similar` for diff display |
| `bash` | exec | `process-wrap` process groups, timeout kills the group, output caps |

All tool results returned to the model are size-capped with explicit
truncation signaling. (Live streamed output is bounded in retention, not in
total volume — §7.1.) Every tool call passes through the permission pipeline
before execution.

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
completes — providers accept tool results only as complete messages, so
mid-call streaming is a human-facing feature, not a model-facing one.

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
interactive programs (REPLs, debuggers) is post-MVP.

---

## 8. Permissions

Adapted from OpenCode's permission model (specifically its newer ordered-rule
"V2" design), with its documented weaknesses fixed.

### 8.1 Rules

```toml
[agents.primary.permissions]
# tier defaults — fallback when no rule matches
read = "allow"
write = "ask"               # covers the `write` and `edit` tools
exec = "ask"                # covers `bash`
delegate = "allow"          # spawning is cheap to ask about and annoying to
                            # gate in a subagent-first harness; child tools
                            # are governed by the child's own permissions

[[agents.primary.permissions.rules]]
id = "no-force-push"
action = "bash"
resource = "git push --force *"
effect = "deny"
hard = true                 # evaluated first; cannot be overridden by later
                            # rules or runtime approvals

[[agents.primary.permissions.rules]]
id = "git-readonly"
action = "bash"
resource = "git status *"
effect = "allow"
```

- `action`: a permission capability — `read`, `write`, `bash`, `grep`,
  `glob`, `list`, `delegate`, `external_directory`, plus future capabilities.
  Rules always use **action** names, never tool names.
- Tool → action mapping: every tool maps to its same-named action, with one
  exception — the `edit` tool maps to the **`write`** action (both
  file-mutation tools share one capability, as in OpenCode's model).
- Action → tier mapping (for tier-default fallback): `read`, `list`,
  `grep`, `glob` → read tier; `write` → write tier; `bash` → exec tier;
  `delegate` and `external_directory` have their own explicit defaults.
- **No parent → child permission inheritance.** Every session's permissions
  are resolved fresh: global default configuration + the session's own
  profile overrides. Parent-profile rules never flow down. Global rules
  (including global `hard` denies) apply to every session in the tree. The
  parent's control over children is exercised through `allowed_profiles` —
  which profiles may be spawned — not through permission leakage.
- `hard` denies apply within a session's resolved policy: they cannot be
  overridden by later rules or runtime approvals.

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
| `read`, `write`, `list` | canonical path, workspace-relative; paths outside the workspace are canonicalized (symlink-safe) and matched as absolute, gated by `external_directory` first |
| `grep` | the regex string |
| `glob` | the pattern string |
| `delegate` | target profile name |
| `external_directory` | canonical absolute `dir/*` patterns; evaluated before the underlying read/write |

Canonicalization of a path that does not yet exist (new `write` targets):
canonicalize the **nearest existing ancestor**, resolve symlinks there, then
append the remaining components lexically. Workspace/external classification
happens on the result.

### 8.4 Evaluation

1. Hard denies → `deny` (checked before runtime approvals, so a saved
   "always" can never override a configured hard deny).
2. Doom-loop guard (§8.6): if triggered, `ask` — and **no "always" option is
   offered**, so saved approvals can never bypass it.
3. Runtime approval store hit → `allow`.
4. Ordered rules, **last match wins**. Built-in guard defaults (`.env`,
   `external_directory`) are the lowest-priority rules; config layers append
   after them in the full configuration order
   (user < workspace < environment < selected-profile overlay), so deeper
   layers win. Environment-supplied rules concatenate like any other layer.
5. Tier default fallback (read/write/exec/delegate per §8.1).

Multi-resource calls: every resource is normalized once and evaluated once;
the aggregate is hard-deny/deny, then ask, then allow. Doom-loop accounting is
one increment for the complete normalized call signature, not each subcommand.

**Every decision is explainable**: the trace (matched rule id, source layer,
all candidate matches, normalized resource, precedence reason) is emitted
with `ApprovalRequested` and rendered by clients. OpenCode's evaluator
returns only a bare effect; we keep the full derivation.

### 8.5 Approvals

- Pending approvals are emitted as `ApprovalRequested`; clients answer via
  `approval.respond` with **once** / **always** / **reject** (+ optional
  feedback text).
- "always" writes to a **runtime approval store** (never TOML), owned by the
  engine and **shared across the whole session tree** — parent, children,
  grandchildren all resolve `ask` against the same store. It is keyed by
  `(root_session_id, action, suggested pattern)`. Every grant is recorded as
  an `ApprovalResolved` event (including the approved scope) in the granting
  session's event log; at daemon startup the store is rebuilt from the
  `ApprovalResolved` events of every session in the tree. So approvals
  survive a daemon restart but expire with the tree: deleting the root
  session's history ends them. Suggested patterns come from the tool (e.g.
  bash suggests a command prefix like `git status *`); the client may edit
   the scope before confirming. Post-MVP: project-persistent approvals with
   TTL (OpenCode V2's durable model).
- A default-accepted suggested scope is persisted as that effective scope, not
  `None`. A caller override applies only to the primary resource; secondary
  resources retain their own suggestions. Multi-resource `always` approvals persist and rebuild one
  `(action, resource, scope)` grant per disclosed resource. Doom-loop prompts
  reject an attempted `always` decision.
- Reject affects **only** the pending call — OpenCode's surprising
  reject-fan-out to unrelated pending requests in the session is not copied.
- The model receives a structured refusal as the tool result, plus feedback
  text if supplied.
- Unattended/headless runs treat `ask` as deny unless the profile overrides.

### 8.6 Built-in guards

- `.env` / `.env.*` reads default to `ask` (`*.example` allowed)
- `external_directory` defaults to `ask`
- Doom-loop guard: third consecutive identical tool call → `ask`

Permissions are consent control, not OS isolation — the harness runs with the
launching user's privileges (sandboxing is a documented non-goal), and bash in
particular should be treated as host-authority execution: command parsing
informs approvals but is not a security boundary.

---

## 9. Configuration

Layered via figment (later layers win):

```
built-in defaults
  < user config        ~/.config/cookie_agent/config.toml
  < workspace config   <repo>/.cookie_agent/config.toml
  < environment        COOKIE_AGENT_*
```

Sketch:

```toml
[server]
host = "127.0.0.1"
port = 7419

[providers.anthropic]
type = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"

[providers.openai]
type = "openai"
api_key_env = "OPENAI_API_KEY"
api = "responses"            # or "completions", per-endpoint override

[providers.local]
type = "openai-compatible"
base_url = "http://localhost:11434/v1"

[agents.primary]
type = "primary"             # primary | subagent | all | internal (default: all) — see below
models = [
  { provider = "anthropic", model = "claude-sonnet-4-6" },
  { provider = "openai",    model = "gpt-5" },   # fallback (§6.1)
]
tools = ["read", "list", "grep", "glob", "write", "edit", "bash"]

[agents.primary.delegation]
enabled = true
allowed_profiles = ["explorer", "reviewer"]
limit = 4                    # depth limit; unset root = Unlimited;
                             # unset child inherits parent's decremented limit

[agents.explorer]
type = "subagent"            # only spawnable via delegate; not user-invocable
models = [{ provider = "openai", model = "gpt-5-mini" }]
tools = ["read", "list", "grep", "glob"]

[agents.compaction]
type = "internal"            # engine-internal; cannot be disabled (§9)
# no models — inherits the compacted session's chain (§6.1)
tools = []

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
  inherits the parent session's resolved chain** (§6.1) — the default for
  subagent profiles that should ride the parent's provider choice, and for
  internal agents (the compaction agent compacts with the session's own
  models). Root sessions cannot use an inheriting profile: `session.create`
  fails validation when the resolved chain would be empty. `type = "all"`
  profiles with no chain are legal but fail as roots.
- **Merge semantics**: figment's `merge` *replaces* arrays, which is wrong
  for permission rules (deeper layers must append). The config crate
  implements a custom layered merge: permission-rule arrays are concatenated
  across all layers in the order
  `built-in < user < workspace < environment < selected-profile overlay`
  (each rule tagged with its source layer for decision traces); every other
  array (`tools`, `allowed_profiles`, …) is replaced by the deeper layer as
  usual.
- **Project trust**: workspace config from a repository is untrusted input.
  First use prompts for trust before it is applied (it can enable permissive
  tools in an unsandboxed harness). Trust decisions are stored in
  `~/.local/share/cookie_agent/trust.json`, keyed by canonical workspace path
  plus a content hash of the workspace config file — editing the file
  re-prompts. The `cookie_agent` CLI prompts only when stdin and stdout are
  TTYs; an untrusted config in a non-TTY invocation (including `daemon`) is
  refused unless the user explicitly passes `--trust-workspace`, which records
  trust for the current config contents.
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
    delegations.jsonl
    sessions/<session-id>/
        events.jsonl
        meta.json        (cache)
```

- **In-memory projections** rebuilt from logs (and the journal) at daemon
  startup serve tree queries, listing, resume, and delegation recovery.
  All JSONL files undergo torn-tail truncation on load — every file is
  truncated to its last complete record before projections are built (§5.4).
  SQLite (`rusqlite`) is a future rebuildable projection if full-text search
  or large-history queries demand it — never the source of truth.
- Crash recovery: incomplete runs are marked `interrupted` on restart;
  sessions remain resumable with re-resolution of pending tool calls (§5.4).
- Compaction: post-MVP. The event model reserves checkpoint event types; raw
  history is always retained, and only the model-visible prompt is compacted.

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
                            stdio       newline-delimited JSON (NDJSON)        [post-MVP]
                            unix socket NDJSON                                 [post-MVP]
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
  Unix sockets are designed for but land post-MVP (stdio then enables editor
  integrations that prefer child-process models).
- All network transports are localhost-only for the MVP.

Protocol surface (transport-independent):

- Handshake negotiates `protocol_version`.
- Methods (sketch): `session.create`, `session.list`, `session.get`,
  `session.children`, `session.tree`, `session.resume` (re-resolves
  interrupted pending tool calls, §5.4), `run.start`
  (idempotent via `client_run_id`, durably enforced by the engine; a
  conflicting reuse returns JSON-RPC `-32602` with
  `data.code = "idempotency_conflict"` plus `session_id` and
  `client_run_id`), `run.steer`, `run.cancel`,
  `run.tool_stdin` (ordered base64 writes + `eof` to a running tool call,
  §7.1),
   `events.subscribe` (cursor-based replay + live tail), `approval.respond`,
   `provider.list_models`, `agent.list` (user-invocable profiles: types
   `primary` and `all`, §9). Post-MVP additions: `session.fork`.
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
  event log.
- `ts-rs` generates TypeScript bindings from `protocol` types; `schemars`
  generates JSON Schemas (tool parameters, protocol).
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
- `cookie_agent` binary crate: `cookie_agent` (TUI, auto-spawns/connects daemon),
  `cookie_agent daemon` (WebSocket transport; stdio post-MVP), plus
  non-interactive conveniences later.
- Web and VS Code: post-MVP, consuming the generated TS bindings.

---

## 13. Tech stack

| Area | Crate |
|---|---|
| Runtime | tokio (rt-multi-thread, macros, sync, time, process, signal, fs, io-util, net) |
| Server | axum 0.8 (ws), tower-http (cors, request-id, trace) |
| Wire | serde, serde_json; schemars; ts-rs |
| Providers | reqwest 0.13 (rustls, stream), eventsource-stream, async-openai |
| Config | figment (toml, env) |
| CLI | clap 4 (derive) |
| Process exec | process-wrap with the `tokio1` feature (process groups / job objects) |
| Search | ignore, regex |
| Shell parsing | tree-sitter-bash (sub-command extraction for permissions) |
| Diff display | similar (rendering only) |
| IDs / time | uuid v7, jiff (RFC 3339 serialization) |
| Errors / observability | thiserror, anyhow, async-trait, tracing + tracing-subscriber (env-filter) |
| TUI (MVP) | ratatui 0.30, crossterm |
| Testing | scripted fake provider, insta (json + redactions), tempfile, wiremock |

MSRV: Rust 1.88 (ts-rs floor).

---

## 14. Testing strategy

1. **Scripted fake provider**: deterministic in-memory provider yielding
   scripted normalized event streams — the primary agent-loop test double.
2. **Snapshot tests** (insta) for protocol events, tool transcripts, and
   config snapshots; IDs/timestamps redacted.
3. **wiremock** for provider HTTP adapters (auth, errors, SSE chunking).
4. **Delegation lifecycle tests**: idempotent re-delivery at every crash
   window in §5.4 (including the linked-but-unstarted child, the
   reservation-without-journal-record window, journal append-failure
   rollback, and journal reservation races); depth-limit arithmetic;
   cancellation propagation; torn-tail truncation.
5. **Permission evaluation tests**: rule precedence, bash sub-command
   parsing, path matching, decision traces.
6. Tool tests on `tempfile` workspaces (edit conflict handling, atomic writes).
7. **Streaming/interaction tests** (§7.1): delta ordering and stdout/stderr
   separation, ring-buffer truncation, gap markers for lagging subscribers,
   atomic snapshot-to-live handoff (no lost/duplicated bytes), disconnected
   clients, stdin ordering/EOF/rejection-for-non-running calls, cancellation
   discarding pending stdin, final deltas preceding `ToolCallCompleted`, and
   only the bounded final result entering model history.

---

## 15. MVP definition

v0.1 ships when all of these work end-to-end:

**Daemon and engine**
- engine + server composed by `cookie_agent`; JSON-RPC over the transport
  abstraction with exactly two transports: **in-process** and **WebSocket**
- per-session actors with out-of-mailbox tool execution (§4.1), steering at
  turn boundaries, full cancellation

**Providers and tools**
- Providers: Anthropic; OpenAI (Completions **and** Responses);
  OpenAI-compatible (`base_url`); per-agent **fallback chains** with error
  classification, per-run stickiness, and `ModelFallback` events (§6.1)
- Tools: read, write, edit (optimistic exact-match), bash (process groups,
  streamed stdout/stderr, user stdin), grep, glob, list — plus
  auto-injected `delegate`
- All tool outputs capped with truncation signaling

**Configuration and permissions**
- figment-layered TOML (user / workspace / env) with the custom
  permission-rule concatenation merge (§9)
- agent profiles with delegation blocks; policy snapshots at session
  creation (child snapshots contain global rules + their own profile only —
  never parent-profile rules)
- agent types (`primary` / `subagent` / `all`) enforced at `session.create`
  and in delegate schema generation (§9)
- workspace trust prompt backed by `trust.json`
- tiered ask-on-write permissions with rules, hard denies, decision traces,
  and the tree-shared runtime approval store (§8)

**Sessions and subagents**
- JSONL event logs + delegation journal (journal actor, torn-tail recovery);
  resume with pending-tool-call re-resolution; startup reconciliation of
  partial delegations (§5.4)
- provenance (`Root`/`Delegated`), tree queries, `ToolCallLinked` backlinks
- delegate lifecycle: depth-limit arithmetic, journal idempotency, cancellation
  propagation, bounded results
- clients can observe and steer any session in the tree live

**Frontend**
- ratatui TUI over in-process or WS transport: stream, steer, answer
  approvals, watch live tool output and respond over stdin, browse/expand
  the live subagent tree

**Explicitly post-MVP**: `session.fork`, stdio/Unix-socket transports,
compaction, MCP, web frontend, VS Code extension, SQLite projection, artifact
store, project-persistent approvals, plugin model.

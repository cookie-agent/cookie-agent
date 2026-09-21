# Architecture

cookie agent is a subagent-first coding harness built as a Rust workspace of ten
crates. A local daemon owns provider connections, sessions, model execution,
permissions, and persistence; a terminal UI communicates with it over a versioned
JSON-RPC WebSocket protocol. Session history is a versionless, best-effort-read
event log; other persisted and wire surfaces remain current-only.

## Process model

The `cookie` binary (`crates/cookie_agent`) is a thin composition root. Its CLI
has six top-level subcommands plus a default mode:

| Command | Behavior |
|---|---|
| *(none)* | Start an in-process daemon and open the TUI over an in-memory stream |
| `run [options] <prompt>` | Run one prompt headlessly through the local engine |
| `daemon [--port]` | Run only the daemon, listening on `ws://127.0.0.1:7419/ws` by default; `--port 0` picks an ephemeral port |
| `attach [--url] --token` | Attach the TUI to an existing daemon WebSocket |
| `connect [provider_id] --token` | Interactive durable managed-provider connection (TTY only) |
| `disconnect [provider_id] --token` | Interactive durable managed-provider disconnection (TTY only) |
| `mcp --token list` | List configured MCP servers |
| `mcp --token auth <server>` | Start OAuth authorization for a remote MCP server |

The daemon binds only to the configured port and authenticates every WebSocket
with a fresh per-run bearer token. It generates the token at startup, keeps it
only in memory, and prints it once as its first stdout line, so there is no
token file on disk. `attach`, `connect`, `disconnect`, and `mcp` require the
token via `--token` or `COOKIE_DAEMON_TOKEN` and accept only loopback `ws`/`wss`
URLs whose path is exactly `/ws`. They use the same shared protocol client as
the TUI, so the CLI keeps working even when built without the `tui` feature.

## Crate layering

The workspace is layered bottom-up by dependency:

```text
identity
  └─ protocol        (wire types + session transport/client/server traits)
       ├─ plugin_sdk
       ├─ models
       │    └─ config
       │         └─ engine
       │              ├─ tools
       │              └─ server
       │                   └─ tui
       └─ cookie_agent (binary: composes every layer)
```

| Crate | Responsibility |
|---|---|
| `identity` | Strict shared identities: agent IDs, provider IDs, model keys, variants, wildcard patterns, revisions. The bottom of the stack with no `cookie_agent_*` dependencies. |
| `protocol` | Current-only wire contracts **and the protocol session layer**: RPC roots, events, session metadata, agent snapshots, JSON Schema bindings, the frame-level `Transport` trait, the `ClientProtocol`/`ServerProtocol` traits, the shared `Client`, `ServerContext`, protocol-owned `serve`, and shared setup-value parsing. Re-exports `identity` and hosts the unified wire types. |
| `plugin_sdk` | Official Rust SDK for out-of-process plugins: JSON-RPC framing, handler registration, tool declarations, event publication, and interception hooks. |
| `models` | Dynamic provider/model runtime: models.dev catalog, family recipe registry, provider store, Oven adapters, compiled model manifests. Re-exports the capability wire types from `protocol`. |
| `config` | Strict runtime configuration and Markdown agent documents; layered user/workspace loading with secret zeroization. Re-exports `AgentMode`, `PermissionAction`, `PermissionEffect`, `PermissionRule`, and `AgentDocumentSource` from `protocol`. |
| `engine` | Session actors, run loops, permissions, approvals, delegation, compaction, internal agents, persistence. |
| `tools` | Built-in `read` (filesystem and artifact URIs), `write`, `edit`, `bash`, and `webfetch` tools plus the delegation (`delegate_subagent`, `get_subagent_result`, `cancel_subagent`), messaging (`send_message`), `skill`, and goal providers. |
| `server` | The `ServerProtocol` implementation over `Engine`, concrete transports (WebSocket + `InProcessStream`), a thin connection wrapper, and the public per-run token / `validate_websocket_url` APIs. |
| `tui` | ratatui terminal client: composer, transcript, approvals, sessions, provider connect flow. Its client is a thin adapter re-exporting the shared protocol client. |
| `cookie_agent` | CLI and composition root wiring every crate together. The only binary. |

### Type unification

The refactor consolidated shared wire types in `protocol`:

- **Unified:** `config` re-exports `AgentMode`, `PermissionAction`,
  `PermissionEffect`, `PermissionRule`, and `AgentDocumentSource` from
  `protocol`; `models` re-exports `Modality`, `MediaKind`, `MimeType`,
  `MediaCapability`, `ReplayCapability`, `CancellationCapability`, `FiniteF32`,
  and `ReasoningEffort` from `protocol`.
- **Deliberately separate:** `Sha256Digest`, `ModelCapabilities`, `ToolChoice`,
  `RequestDefaults`, `ResolvedRequestDefaults`, and `ProviderOptions` stay
  `models`-side. Their decoding is intentionally more lenient so compiled model
  state and manifests written by earlier versions keep loading.

## Client/server protocol flow

The session layer lives in the `protocol` crate so every frontend shares one
implementation of the protocol mechanics.

### Transport

`protocol::Transport` is a frame-level channel: `send(MessageFrame)` /
`recv() -> Option<MessageFrame>`, with `MessageFrame` either a `Text` string or a
`Value`. It has no JSON-RPC semantics. The `server` crate provides the concrete
transports: `WebSocketTransport` (tokio-tungstenite, used by the TUI and CLI to
reach a daemon) and `InProcessStream` (used by the local frontend over an
in-memory mpsc pair). The axum WebSocket accept path implements `MessageStream`
(the `Transport` alias) server-side.

### Server side

`protocol::ServerProtocol` is the server contract: one async method per RPC
method plus `connected` and `subscribe_events` (which receives a
`ServerContext`). The `server` crate implements it in `service/routes.rs`,
delegating every call to `Engine`.

`protocol::serve` drives one complete server-side session over a `Transport`:

- **Handshake gating.** Every request before a valid `handshake` is rejected with
  error code `-32001`; the exact-version handshake is answered with
  `ServerHello`, then `connected` is invoked on the implementation.
- **Request dispatch.** Incoming requests are correlated by JSON-RPC id and
  dispatched to the `ServerProtocol` implementation; `runtime.snapshot.get`,
  `provider.connect`, and `provider.disconnect` additionally require a request id.
- **Notification demux.** `ServerContext::notify` queues one JSON-RPC
  notification per connection; the session loop interleaves it with incoming
  frames. The server implementation uses this for `runtime.changed`, event
  tails, and streamed tool output.
- **Shutdown.** A per-connection cancellation token ends the loop and drops
  outstanding state.

### Client side

`protocol::ClientProtocol` is the client contract; the shared concrete `Client`
implements it over any `Transport`. A connection task owns the stream and
handles:

- **Request/response correlation** by id, with a bounded command queue and a
  sole ordered delivery channel.
- **Notification demux** into `ClientDelivery` variants: live
  `events.subscription`, tool-output snapshot/delta/gap, and `runtime.changed`.
- **Replay and gap recovery.** `events.subscribe` runs a cursor replay that is
  injected into the delivery stream before buffered live notifications; a
  recovery worker re-subscribes with backoff and emits `RecoveryFailed` when it
  exhausts its attempts.
- **Sensitive-frame wiping.** `provider.connect` and other secret-bearing calls
  serialize into a zeroizing buffer that is wiped on dispatch or cancellation.
- **Shutdown** via a cancellation token that fails outstanding calls with
  `ClientError::Closed`.

The `server` crate wraps this in a thin `Client` (Deref to `protocol::Client`)
and adds `connect_websocket`/`connect_in_process`/`connect_stream`. The **TUI
client is a ~6-line adapter** re-exporting it, and the CLI uses the same `Client`
through the `ClientProtocol` trait.

## Configuration and layering

The `config` crate reads two optional authored layers:

```text
~/.cookie-agent/config.toml                 # user layer
~/.cookie-agent/agents/<agent-id>.md
<exact-cwd>/.cookie-agent/config.toml       # workspace layer
<exact-cwd>/.cookie-agent/agents/<agent-id>.md
```

There is no upward workspace search. A workspace setting replaces the
corresponding user setting; a same-ID workspace provider or agent replaces the
complete user definition. Unknown keys, leftover schema/version fields, wrong
types, and malformed values are rejected without migration or silent ignores.
The TUI additionally reads an independent `~/.cookie-agent/tui.toml`.

See [Configuration](guide/configuration.md) and the
[configuration reference](guide/configuration.md) for every key.

## Provider and model runtime

`crates/models` owns the full dynamic provider pipeline:

1. **Catalog.** The daemon refreshes the fixed models.dev catalog
   (`https://models.dev/catalog.json`) hourly, with a validated ETag cache and a
   bundled integrity-checked bootstrap as fallbacks. Catalog selection is network,
   cache, or bootstrap.
2. **Family registry.** A code-owned recipe registry (schema 1) maps the catalog's
   npm package names to protocol families: OpenAI, OpenAI-compatible chat,
   Anthropic, Google, Vertex, Bedrock, Azure, and Cohere. Each recipe declares a
   default endpoint, allowed authentication methods, and credential shapes.
3. **Compiler.** `DynamicCompiler` compiles managed catalog rows (with optional
   authored overrides) and authored custom providers into
   `CompiledDynamicModel` entries with concrete endpoints, validated setup,
   authentication, capabilities, request defaults, and variants.
4. **Executable adapters.** The Oven crates (`oven-sdk`, `oven-sdk-openai`,
   `oven-sdk-anthropic`, `oven-sdk-google`, `oven-sdk-google-vertex`,
   `oven-sdk-bedrock`, `oven-sdk-azure`, `oven-sdk-cohere`, and the
   `reqwest` HTTP transport) provide normalized language-model
   implementations. The `models` crate selects the adapter for each compiled
   model and freezes it into user manifests.
5. **Provider store.** Managed connections live in a global per-user provider
   store (`~/.cookie-agent/providers/store-v3.json`) and are shared
   across workspaces. Credentials are checked on first use, not at connect time.

Each accepted run freezes its model selection into a global user manifest under
`~/.cookie-agent/model-snapshots/`, so later catalog, configuration, or store
changes cannot silently change an accepted run's model behavior. The manifests
are shared across workspaces.

Local selection/pricing identity is distinct from the effective provider wire
model ID. Model and variant overrides resolve before execution; frozen bindings
retain the selected wire ID and endpoint. Concrete SDK constructors provide
their own request and capture attribution, including unauthenticated OpenAI
Chat; no attribution-rewriting wrapper is used.

History reconstruction validates saved provenance against its original turn,
then leaves block eligibility to the target codec. Standard supported blocks
are not restricted by source provider, header, or model fingerprints. Encrypted
reasoning requires equal known effective wire model IDs and target support;
required continuation evidence survives persistence so missing native state
cannot silently normalize away. Integrity markers alone do not establish a
required continuation. Native-compaction scopes are a separate contract. See
[Providers](guide/providers.md#replay-and-cancellation) for operational effects.

## Engine

`crates/engine` runs one actor per session. Actors serialize all mutations to a
session and drive the run loop:

- **Run loop.** Each run walks the agent's model fallback chain. The frozen
  `model_retry` policy defaults to three retries (four total attempts) for
  ordinary retryable failures and five retries (six total attempts) for overload
  errors or retryable HTTP 429/503 responses. Zero disables retries for a class;
  a negative count retries indefinitely. Both classes use
  exponential backoff from one second and 25% jitter. The configurable local
  ceiling defaults to 60 seconds; a longer provider `Retry-After` may exceed it
  without a cap, while remaining promptly cancellable. Every failed attempt
  emits `attempt_abandoned`, which is visible in the TUI. When its budget is
  exhausted, the loop advances to the next fallback and emits
  `model_fallback`; the fallback position is sticky. Before each
  request the loop runs predictive compaction and, after a completed turn,
  post-check compaction (see [Compaction](guide/compaction.md)).
- **Permissions.** Every prepared tool call is matched against the agent's
  ordered permission rules (see [Permissions](guide/agents.md#permissions)), and
  unmatched checks deny by default. Tool visibility is gated the same way: a
  tool is advertised to the model only when its action has an `allow` or `ask`
  rule in the agent document or session overlay. The
  permission pipeline is stateless; approvals and tree grants live in an
  in-memory `ApprovalStore` rebuilt from durable events.
- **Approvals.** A stateless approval evaluator (the `approval` internal agent)
  classifies asks in the three `auto_approve` modes; classifier escalations go
  to the user, reject automatically, or approve once automatically according to
  the mode. `ask` skips the classifier, and `yolo` approves immediately. A
  doom-loop guard rejects repeated identical approvals.
- **Tool dispatch.** Calls from one committed model turn are prepared and
  started in model order, then all permission decisions and serialized approval
  prompts resolve before execution begins. Tools explicitly marked parallel are
  dispatched together without a fan-out limit; exclusive tools run sequentially
  after that set. Prepared serialization keys still prevent conflicting
  mutations from overlapping. Terminal events commit as calls finish.
- **Delegation.** The `delegate_subagent` tool reserves a child session by
  appending lifecycle events to the parent session, then runs the target
  subagent with an inherited model suffix. Each session is assigned a short,
  tree-unique handle (`<agent_type>_<8 hex>`) recorded on its creation event,
  and the model-facing delegation tools accept that handle interchangeably with
  the full session UUID. Depth and concurrency limits come from `delegation`
  configuration.
- **Internal agents.** The approval, context-compaction, and session-title
  agents run with no tools and a strict text-only output contract, normally on
  the parent run's model via `${parent_model}`. See
  [Internal agents](guide/agents.md#internal-agents).

## Sessions and persistence

Sessions are append-only event logs. A new session exists only in memory until
its first user message, when its directory, `events.jsonl`, and `metadata`
cache are published atomically while its ownership lock is held. Revert appends
a `session_reverted` marker and fork copies a persisted prefix under a new
session ID; neither truncates physical events.

### Work-dir layout

Session data lives under the user data directory in one store per canonical
working directory:

```text
~/.cookie-agent/
  providers/store-v3.json          # durable managed connections
  catalog/                         # validated models.dev cache
  sessions/<workdirkey>/           # one store per canonical cwd
    layout.json                    # {"version":2}
    cwd                            # canonical work-dir path
    grant-invalidations.jsonl      # tree-grant invalidation journal
    runtime-revisions-v8.jsonl     # runtime revision index
    artifacts.shared/              # cross-tree and orphaned tool output
    cross-refs.jsonl               # cross-tree artifact reference ledger
    <root-session-id>/             # root sessions live directly inside
      metadata                     # derived session cache
      events.jsonl                 # append-only root log
      owner.lock                   # Unix in-directory ownership lock
      artifacts/                   # artifacts owned by this root's tree
      <root-session-id>.owner.lock # Windows sidecar for a root
      subagents/
        index.json                 # child-summary cache, {"version":1,...}
        <child-session-id>/        # metadata, events.jsonl, owner.lock
        <child-session-id>.owner.lock # Windows sidecar for a child
```

`<workdirkey>` is `<16-hex-hash>-<sanitized-basename>`, where the hash is the
`DefaultHasher` of the canonical path string and the suffix is the lowercased
final path component with every character outside `[a-z0-9._-]` replaced by
`-`, repeats collapsed, length capped at 32, and edge `-`/`_` trimmed. The
suffix exists for browsability only and is computed once at directory creation:
resolution scans `sessions/` for the entry equal to the hash or prefixed by
`<hash>-`, so the hash alone is the addressing key and a stale suffix after a
cwd rename is harmless. An empty suffix leaves the bare hash as the whole key.

Directories and files are created private (`0o700` / `0o600` on Unix).

### Root-only startup and lazy children

Opening a store reads root `metadata` files and at most one
`subagents/index.json` per root; it never opens a child `events.jsonl`. That
cache carries each child's `SessionSummary` and its terminal run statuses, so
listing, usage rollups, and the session tree stay correct at startup. It is a
cache only: a missing, corrupt, or stale entry simply means the children are
unknown until the tree is loaded, and `metadata` remains authoritative per
session.

When a root first comes into use — resume, open for mutation, fork source, tree
or child access — every descendant of that root is read and folded exactly once
in a single bulk pass. That pass assembles the tree, harvests the artifact
digests the tree references, and produces the delegation-registry and
approval-grant records the engine needs. The children then leave the pass
without ever entering residency, so residency caps and janitor pressure are
unaffected; a child that actually runs becomes resident through the ordinary
write path and later evicts like any other session.

### Artifacts

Tool output is content-addressed under a digest-named file. A v2 store
partitions artifacts by tree: output written by a root or one of its descendants
lands in that root's `artifacts/`, while cross-tree references, orphans, and
unplaced writes land in the work-dir's `artifacts.shared/`. Collection is
therefore per tree, and a digest is retained while any live session references
it — the root log, the log of any child in a loaded tree, or the
`cross-refs.jsonl` ledger that records references made from another tree.
Unloaded trees are never collected, which keeps an unread child log from
looking unreferenced. Expired, unreferenced digests are unlinked after a grace
period that protects newly published files.

### Delegation and ownership

Delegation reservations, child publication, run start/attachment, and terminal
state use the parent session's `events.jsonl`. On open, the engine projects
these records from root logs while it recovers nonterminal delegations, and
completes the projection for a tree during that tree's single load pass.
The reservation fingerprint is recomputed from the replayed request, child
agent snapshot, selected model suffix, and staged-skill provenance; a mismatch
rejects recovery. A background delegation the durable facts leave nonterminal is
recovered by adopting the child, whose own reconciliation terminalizes the run
that died with the previous process; the completion monitor then releases the
parent's slot and promotes whatever was queued behind it. A delegation event skipped by best-effort reading is absent
from the recovery projection and appears in the session's skipped-event
diagnostics, while other delegations continue to load.

Multiple cookie processes may share this project data directory. Ownership is
per session, not per work dir: the process that successfully locks `owner.lock`
is the only writer and retains that lock until process exit, including while an
idle session is evicted from memory. On Unix the lock is
`<session-dir>/owner.lock`; on Windows it is the adjacent
`<session-id>.owner.lock` sidecar so its open handle does not prevent
directory renames. New-session and fork publication acquire the Windows sidecar
derived from the final directory path before renaming the temporary directory.
Session discovery ignores the sidecar because it scans only directories.
Session listing reads `metadata` without locking. Opening an existing session
for mutation attempts the lock; success enters a non-writable adoption state,
reconciles only that session's interrupted work, and then publishes ownership.
Adoption also schedules delegation recovery for the session's tree, and a resume
waits — bounded, a few seconds — for that recovery to settle before it returns,
so background-delegation capacity never reads transiently over-counted; if the
bound expires the resume still succeeds and recovery finishes in the background.
A clean shutdown is the other half of that contract: it cancels its in-flight
runs and then waits, under its own bound, for those run tasks to record
`RunCancelled` while the actors and the store are still up. Only a crash, or a
task still wedged when that bound expires and is aborted, leaves a run for the
next startup to repair as interrupted by daemon restart.
Reconciliation failure revokes the log's write capability and releases the lock
so a later attempt can retry. A retained event-log projection cannot append
after its store drops ownership. A live foreign owner produces `session is
owned by another cookie process`. Classification failures fail closed as
foreign-owned.

Foreign sessions remain inspectable as read-only snapshots. The TUI disables
input for them and refreshes the snapshot when reopened; there is no live event
tail. Forking a foreign snapshot is allowed because the new fork has its own
lock. Grants and grant invalidations committed by another process become
visible after restart. Concurrent MCP configuration edits remain last-writer
wins. Ownership failures in protocol 20 use an ordinary fault
message rather than a new wire error.

The data directory must be on a local filesystem with correct `flock` or
`LockFileEx` semantics. NFS-class and other network filesystems are unsupported
for this directory because they cannot guarantee single-writer ownership.

Legacy project-level `delegations.jsonl` files are ignored. In-flight
delegations that existed only in that pre-release journal are not recovered;
their child directories remain ordinary sessions available for inspection.

Model-snapshot manifests live in the global user directory at
`~/.cookie-agent/model-snapshots/` and are shared across workspaces.

### Event bus

The durable session append path publishes raw events to independent bounded
plugin streams only after the JSONL append and projection publication complete.
These streams are best-effort observers and never participate in consistency or
backpressure. The non-durable engine bus accepts plugin sources as
`EngineEvent::PluginEvent`; it fans out to RPC frontends and other subscribed
plugins with session identity but without replay or cursor semantics. RPC fan-out
is connection-local and requires a successful event subscription for that session. A plugin's
own publication is excluded from both plugin fan-out paths.

Plugin diagnostics use a mutex-protected coalescing counter rather than a
message queue. Producers only increment a normalized key and wake a periodic
flusher; detailed message cardinality is capped and excess keys use exact
per-session/plugin/kind overflow counters. Appends and shutdown draining have
deadlines, with incomplete drains surfaced on plugin status before the flusher
is aborted. Plugin
publish contexts are expiring one-shot grants activated at outbound delivery,
so a token cannot be replayed or retargeted to another session.

See [Plugins](guide/plugins.md) for installation and configuration, and
[Plugin development](development/plugins.md) for SDK and extension-protocol
details.

## Protocol surface

The wire protocol is unchanged by the session-layer refactor: JSON-RPC 2.0 over
an authenticated WebSocket at `/ws`, protocol 20 current-only, `handshake` first.
Discovery is a single `runtime.snapshot.get` call that returns one coherent
runtime snapshot (schema 5). Session events stream through `events.subscribe`,
plugin bus events through `events.plugin`, and tool output through separate
snapshot/delta/gap notifications.

What moved is where the mechanics live: handshake, request/response
correlation, notification demux, replay/gap recovery, and shutdown are now
implemented once in the `protocol` crate and shared by the server, the TUI, and
the CLI.

See [Protocol](reference/protocol.md), [Events](reference/events.md), and
[Schemas](reference/schemas.md) for the wire details.

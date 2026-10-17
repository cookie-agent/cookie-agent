# Architecture

cookie agent is a subagent-first coding harness built as a Rust workspace of twelve
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
| `mcp --token list` | List the MCP servers of every runtime with their state and hosting runtime |
| `mcp --token auth <server>` | Start OAuth authorization for a remote MCP server |

The daemon authenticates every WebSocket with a bearer token kept in
`[server] token` of `config.toml`, which it writes at its first start when
there is none. `attach`, `connect`, `disconnect`, and `mcp` take the token
from `--token`, `COOKIE_DAEMON_TOKEN`, or the same configuration, and accept
`ws`/`wss` URLs whose path is exactly `/ws`. They use the same shared protocol client as
the TUI, so the CLI keeps working even when built without the `tui` feature.

Tools run in *runtimes* (see the [runtimes specification](specs/runtimes.md)).
The daemon itself hosts the base layer and the global runtime (MCP servers and
plugins). The `local` coding runtime, with `read`, `write`, `edit`, `bash`,
and `webfetch` in the daemon's working directory, runs on a task of the daemon
process and is reached over the [runtime protocol](reference/runtime-protocol.md)
through an in-memory frame pair. A configured
[`[runtimes.<id>]`](guide/runtimes.md) template starts runtimes in child
processes (typically `cookie runtime serve --stdio` on another machine over
ssh) that speak the same protocol over stdio.

Any number of clients may attach to one daemon at once. Each sees every
session's events live, can submit input to any of them, and is told of the
others' changes (see [the protocol](reference/protocol.md#daemon-wide-changes)).
Plain `cookie` keeps its daemon private to its own TUI. An attached TUI that
loses its connection redials the daemon with backoff under the same client
identity, then rebuilds the viewed session from a full replay, which also
restores output that streamed while it was away. When the client gives up
recovering the viewed session's subscription, the TUI subscribes it afresh
after a short pause.

## Crate layering

The workspace is layered bottom-up by dependency:

```text
identity
  └─ protocol        (wire types + session layer: transports, client, serve)
       ├─ plugin_sdk
       ├─ tui           (depends on protocol only; never on the engine)
       ├─ runtime_api   (tool contract + runtime protocol; never on the engine)
       ├─ models
       │    └─ config
       │         └─ mcp     (also on runtime_api; never on the engine)
       │              ├─ coding_runtime  (never on the engine)
       │              └─ engine  (enables mcp's `oauth` feature)
       │                   └─ server
       └─ cookie_agent (binary: composes every layer)
```

The TUI deliberately sits beside the engine rather than above it: it speaks
only the protocol, receives an already-connected `Client`, and the
`cookie_agent` binary does the in-process wiring to `server`. Engine edits
therefore never rebuild the TUI library, and the two compile in parallel.
The TUI's own tests still drive a real `server` through a dev-dependency.

| Crate | Responsibility |
|---|---|
| `identity` | Strict shared identities: agent IDs, provider IDs, model keys, variants, wildcard patterns, revisions. The bottom of the stack with no `cookie_agent_*` dependencies. |
| `protocol` | Current-only wire contracts **and the protocol session layer**: RPC roots, events, session metadata, agent snapshots, JSON Schema bindings, the frame-level `Transport` trait, the in-memory `InProcessStream` pair, the `ServerProtocol` trait, the shared `Client`, `ServerContext`, protocol-owned `serve`, and shared setup-value parsing. The opt-in `websocket` feature adds the daemon client transport (`WebSocketTransport`, `Client::connect_websocket_with_token`, `validate_websocket_url`). Re-exports `identity` and hosts the unified wire types. |
| `plugin_sdk` | Official Rust SDK for out-of-process plugins, on the protocol crate's JSON-RPC peer: handler registration, tools over the runtime protocol's tool messages, interception hooks, event and bus handlers, and producers. |
| `models` | Dynamic provider/model runtime: models.dev catalog, family recipe registry, provider store, Oven adapters, compiled model manifests. Re-exports the capability wire types from `protocol`. |
| `config` | Strict runtime configuration and Markdown agent documents from the user layer, with secret zeroization; also the `SKILL.md` parser and project skill discovery that coding runtimes use. Re-exports `AgentMode`, `PermissionAction`, `PermissionEffect`, `PermissionRule`, and `AgentDocumentSource` from `protocol`. |
| `runtime_api` | The contract between the engine and the tools it runs: `ToolProvider`, `ToolSpec`, prepared calls, preparation and execution contexts, progress, the traits through which the engine provides output capture and artifact retention, the detach point through which a background-capable tool says when it may be backgrounded, runtime identity (`ToolOrigin`, `RuntimeInfo`, `WorkspaceContext`), tool attachment validation, and the `AGENTS.md` lookup. It re-exports the [runtime protocol](reference/runtime-protocol.md)'s wire types (`protocol`) and the symmetric JSON-RPC peer both ends run with a newline-delimited stdio transport (`rpc`), which live in the protocol crate so the plugin SDK shares them, and holds `RuntimeConnector`, which starts a runtime for a requested workspace and returns its transport with a bounded `DiagnosticTail` of what it writes outside the protocol. Depends on `protocol` only. |
| `mcp` | The MCP client (`cookie_agent_mcp`): an `McpRegistry` per runtime that connects its servers eagerly over stdio or Streamable HTTP, publishes their tools as `<server>_<tool>` with untrusted-output markings, maps result media to attachments, reconnects dropped servers with the shared reconnect policy, and reports status and failures to subscribers and an `McpObserver`. The `oauth` feature, which only the engine enables, adds OAuth for remote servers and the `mcp-oauth.json` credential store. Depends on `runtime_api` and `config`, never on the engine. |
| `engine` | Session actors, run loops, permissions, approvals, delegation, compaction, internal agents, persistence, the base layer tools (delegation: `delegate_subagent`, `get_subagent_result`; messaging: `send_message`; `skill`; goals; `read_artifact`; `stop_background`), background calls (`runtime/background_tasks.rs`), subagent reports (`runtime/subagent_reports.rs`), and the runtime registry with the runtime protocol client (`runtimes::remote`), the child-process connector of configured templates (`runtimes::process`), instance reconnects, and the global runtime's `McpRegistry` with the TUI's runtime-layer MCP edits and their write-back (`config_store`). |
| `coding_runtime` | The coding runtime: its `read`, `write`, `edit`, `bash`, and `webfetch` tools, working in the runtime's workspace, the global runtime's fallback `webfetch`, and the runtime protocol server (`server::serve`) that answers a daemon with them, the workspace's project skills, and its `AGENTS.md`, plus the in-process connector the daemon's `local` runtime uses and `server::serve_stdio`, which `cookie runtime serve --stdio` runs. The server hosts its template's MCP servers with an `McpRegistry` without OAuth. Depends on `runtime_api`, `mcp`, and `config` (the skill parser), never on the engine. |
| `server` | The `ServerProtocol` implementation over `Engine`, the axum WebSocket accept path, `Server::connect_in_process`, and the per-run token / ready-line APIs. Server side only. |
| `tui` | ratatui terminal client: composer, transcript, approvals, sessions, provider connect flow. Depends only on `protocol` and runs over a `protocol::Client` it is handed. |
| `cookie_agent` | CLI and composition root wiring every crate together, and `cookie runtime serve --stdio`, which serves a coding runtime to a daemon without reading any configuration. The only binary. |

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
`Value`. It has no JSON-RPC semantics. The `protocol` crate also provides the
concrete client transports: `InProcessStream` (an in-memory mpsc pair used by
the local frontend and tests) and, behind the `websocket` feature,
`WebSocketTransport` (tokio-tungstenite, used by the TUI and CLI to reach a
daemon). The `server` crate's axum WebSocket accept path implements
`MessageStream` (the `Transport` alias) server-side.

### Server side

`protocol::ServerProtocol` is the server contract: one async method per RPC
method plus `connected` and `subscribe_events` (which receives a
`ServerContext`). The `server` crate implements it in `service/routes.rs`,
delegating every call to `Engine`.

`protocol::serve` drives one complete server-side session over a `Transport`:

- **Handshake gating.** Every request before a valid `handshake` is rejected with
  error code `-32001`; the exact-version handshake is answered with
  `ServerHello`, then `connected` is invoked on the implementation. The
  handshake's client identity fixes the connection's origin, which
  `ServerContext::client_origin` hands to every mutating method.
- **Request dispatch.** Incoming requests are correlated by JSON-RPC id and
  dispatched to the `ServerProtocol` implementation; `runtime.snapshot.get`,
  `provider.connect`, and `provider.disconnect` additionally require a request id.
- **Notification demux.** `ServerContext::notify` queues one JSON-RPC
  notification per connection; the session loop interleaves it with incoming
  frames. The server implementation uses this for `runtime.changed`, event
  tails, and plugin bus events.
- **Shutdown.** A per-connection cancellation token ends the loop and drops
  outstanding state.

### Client side

The shared concrete `protocol::Client` runs over any `Transport`. A connection task owns the stream and
handles:

- **Request/response correlation** by id, with a bounded command queue and a
  sole ordered delivery channel.
- **Notification demux** into `ClientDelivery` variants: live
  `events.subscription`, `events.plugin`, and `runtime.changed`.
- **Replay and gap recovery.** `events.subscribe` runs a cursor replay that is
  injected into the delivery stream before buffered live notifications; a
  recovery worker re-subscribes with backoff and emits `RecoveryFailed` when it
  exhausts its attempts. Replays are fetched in pages, and the connection task
  holds the pages until the final one so the UI still sees one replay. Each
  page must be answered within 5 seconds of its request being written; time
  spent in the client's own queue does not count. A replay runs to completion
  even if the caller stops waiting for it.
- **Sensitive-frame wiping.** `provider.connect` and other secret-bearing calls
  serialize into a zeroizing buffer that is wiped on dispatch or cancellation.
- **Shutdown** via a cancellation token that fails outstanding calls with
  `ClientError::Closed`.

Connections are made with `Client::connect_stream` over any transport,
`Client::connect_websocket_with_token` (the `websocket` feature), or
`Server::connect_in_process` in the `server` crate. The TUI and the CLI use this
same `protocol::Client` directly; there is no per-frontend wrapper.

Each `events.subscribe` costs a paged replay and replaces the connection's live
tail for that session, so the TUI subscribes a session at most once per
connection while that subscription is healthy. Opening, reselecting, or watching
a session it already follows live only switches the view. What replays a
session again is a recovery, reopening a foreign read-only snapshot (which has
no tail to keep it current), a session adopted after a snapshot subscription
(which needs its first live tail), and a new connection.

## Configuration and layering

The `config` crate reads built-in defaults and one optional authored layer, the
user layer:

```text
~/.cookie-agent/config.toml
~/.cookie-agent/agents/<agent-id>.md
~/.cookie-agent/agents/<preset>/<agent-id>.md
```

There is no workspace configuration layer: `<cwd>/.cookie-agent/config.toml`
and `<cwd>/.cookie-agent/agents/` are never opened. The working directory is
the local coding runtime's workspace and only contributes context, `AGENTS.md`
files and project skills. Unknown keys, leftover schema/version fields, wrong
types, and malformed values are rejected without migration or silent ignores.
The TUI additionally reads an independent `~/.cookie-agent/tui.toml`.

See [Configuration](guide/configuration.md) and the
[configuration reference](guide/configuration.md) for every key.

## Provider and model runtime

`crates/models` owns the full dynamic provider pipeline:

1. **Catalog.** The daemon starts from the validated catalog cache, or the
   bundled bootstrap (its size and digest are pinned and checked by tests) when
   there is none, without waiting on the network. It then refreshes the fixed
   models.dev catalog (`https://models.dev/catalog.json`) in the background: once
   right away, then hourly, with an ETag, accepting gzip (the 64 MiB cap
   applies to the decoded body). A `304 Not Modified` rewrites only the
   cache metadata and reuses the catalog already parsed in memory while the
   on-disk metadata still names the same body revision, and models are
   recompiled only when the catalog revision or availability changes. A new
   body is installed by atomically replacing the body and then the metadata
   that names its SHA-256 revision; a pair torn by a crash fails validation,
   so startup falls back to the bootstrap and the next refresh repairs it. A
   refresh that would replace a cache validated within the last seven days
   with fewer than half of its usable models is refused: the cache stays in
   use, marked stale with a `catalog_model_loss_rejected` error. Catalog
   selection is network, cache, or bootstrap.
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

Each accepted run freezes its model bindings against the runtime it was admitted
with and keeps that runtime in memory until it ends, so configuration, catalog,
or store changes cannot change a run's model behavior midway. Nothing is
persisted beyond the run's events, which record the bindings as a description
of what ran. A later turn resolves afresh and best-effort: a history-derived
selection whose agent, model, or variant is gone falls back to the default
agent (`primary`, else the first root-runnable one) or the agent's default
model, while earlier turns keep showing what they actually used.

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
  request the loop compacts when the latest turn's reported usage reaches the
  threshold (see [Compaction](guide/compaction.md)).
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
  the mode. Each evaluation is one fresh model call that sees the latest user
  request, up to five recent user, tree-grant, or classifier decisions for the
  same permission action from the tree's resident sessions, the runtime the
  call runs on with its workspace, and the tool call's normalized parameters.
  `ask` skips the classifier, and `yolo` approves immediately. A doom-loop
  guard rejects repeated identical approvals.
- **Tool layers and runtimes.** The engine composes each session's tool list
  from the base layer and the session's runtime attachments
  (`crates/engine/src/runtimes`; see the
  [runtimes specification](specs/runtimes.md)):
  - the *base layer*: the daemon's own orchestration tools (delegation,
    `send_message`, `skill`, goals, `read_artifact`), which the engine
    implements and registers itself when it opens
    (`crates/engine/src/base_tools`). It has no workspace and every session
    has it.
  - *runtime instances*, started from *templates* by a registry that keeps one
    instance per (runtime ID, canonical workspace) and shares it with every
    session attached to it. The built-in templates are `global` (top-level MCP
    servers, plugins, and a `webfetch` that is only a fallback; no workspace)
    and `local` (kind `coding`: `read`, `write`, `edit`, `bash`, and
    `webfetch` in the daemon's working directory, and the
    `[runtimes.local.mcp]` servers). A coding runtime hosts its template's
    MCP servers itself, on its own machine: it connects them before it
    answers `runtime/initialize`, declares their tools next to its built-in
    ones, and reports their status (`mcp/changed`). Both instances start with
    the daemon (`Engine::open_async`). Each configured `[runtimes.<id>]`
    adds a protocol template whose `ProcessConnector` runs the template's
    command (`runtimes/process.rs`); its instances start when a session
    needs them and are keyed by the canonical workspace the runtime reports,
    never by a path the daemon resolved. The global runtime is called
    in-process. `local` and the configured runtimes run behind the
    [runtime protocol](reference/runtime-protocol.md): a protocol template
    starts an instance with a `RuntimeConnector` and initializes it, and the
    client publishes the tools it declared, proxies preparation and
    execution, routes its progress into the daemon's output capture, retains
    the media it sends, and hands `tool/detachable` to the call's detach
    point. When a protocol connection is lost, calls in flight, backgrounded
    ones included, fail with `runtime_unavailable`;
    the instance reconnects (1 s, 2 s, 4 s) with its tools still published,
    and a restart revives it under a new generation. When every attempt
    fails it becomes failed and its tools are dropped.
  - *attachments*: a session records the instances it uses with
    `runtime_attached` and `runtime_detached` events. New sessions attach
    `global` and `local`; a log with neither event reads the same way. Each
    attachment has a session-unique name, the name its instance reports
    (`local_<workspace basename>`, sanitized to `[a-z0-9_]` and capped at 32
    bytes, for a coding runtime), suffixed `_2`, `_3`, ... on a clash.
  - *connections*: everything a session uses is connected before its first
    run. The global runtime's MCP servers connect eagerly when the daemon
    starts, and the first run waits for those attempts; a coding runtime's
    servers have finished theirs when its instance starts. Creating or
    opening a session starts its attached instances and retries the failed
    MCP servers of every attached instance (`mcp/reconnect` for a coding
    runtime); its next run waits for them. A connected MCP server that drops
    is restarted by the shared reconnect policy
    (`cookie_agent_runtime_api::reconnect`: three attempts after 1 s, 2 s,
    and 4 s) in the runtime that hosts it; meanwhile its tools stay published
    and calls fail fast with `runtime_unavailable`, and when every attempt
    fails its tools are dropped. Connect failures, exhausted reconnects, and
    failed instance starts are broadcast by the runtime registry and the MCP
    hub, which merges the global runtime's servers with those coding
    runtimes report; the server turns them into `mcp.changed` and
    `runtimes.changed` notifications that name the affected sessions each
    connection subscribes to.

  Every published tool records which layer or attachment offers it, and the
  instance generation it came from. Preparation, permission decisions, and
  visibility use that runtime's workspace: a tool without one never matches a
  `${workspace_dir}` rule or a rule for a relative path, and its operations
  digest the runtime ID instead of a path. Names are recomputed every model
  turn: a name offered by one source is published bare; when several runtime
  tools offer one name, or a runtime tool takes a base-layer name, each runtime
  tool is published as `<attachment>_<name>` (the base tool keeps its name); a
  `fallback` tool is hidden whenever a regular tool offers its name, so the
  global `webfetch` only appears without a coding runtime; and a published name
  longer than 64 bytes is dropped with a diagnostic. A call to a published name
  runs on its attachment under the runtime's own name for the tool, and fails
  as `operation_changed` when that attachment, instance, or declaration
  changed since publication. The daemon's working directory itself only
  locates the session store and stamps each session's `cwd_identity`.
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
cache are published atomically while its ownership lock is held. Revert
truncates the log after its target, closes whatever the cut left open, and
deletes the subagent sessions the removed delegations created; live
subscribers get a `rewound` notification. Fork copies a persisted prefix under
a new session ID.

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
    runtime-revisions-v8.jsonl     # runtime revision index
    <root-session-id>/             # root sessions live directly inside
      metadata                     # derived session cache
      events.jsonl                 # append-only root log
      grant-invalidations.jsonl    # this tree's grant invalidation journal
      owner.lock                   # Unix in-directory lock for the whole tree
      artifacts/                   # artifacts owned by this root's tree
      <root-session-id>.owner.lock # Windows sidecar, also for the whole tree
      subagents/
        index.json                 # child-summary cache, {"version":1,...}
        <child-session-id>/        # metadata, events.jsonl — no lock file
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

### Metadata-only listing and lazy trees

Opening a store reads nothing under the work dir. Listing root sessions parses
each root's `metadata` once, or answers from memory for a root this process
holds; it never opens an `events.jsonl` and keeps no cache of its own, so the
next listing sees roots other processes created or changed. Everything else
about a tree — its children, usage, delegations, and grants — is read when the
tree is first used (see [Tree-local sessions](specs/tree-local-sessions.md)).
`subagents/index.json` carries each child's `SessionSummary` and terminal run
statuses for that tree only. It is a cache: a missing, corrupt, or stale entry
simply means the children are unknown until the tree is loaded, and `metadata`
remains authoritative per session.

A root directory whose `metadata` is absent is skipped silently, so the next
listing retries it. That shape is transient rather
than faulty: `<root>/subagents/` exists on its own whenever a child publishes
before its root, and the same shape appears while a prepared directory is
merged into it. Every other failure to read a root `metadata` — a corrupt cache,
an ID that disagrees with its directory name — still reports a diagnostic and
leaves the root uncached for the next pass.

When a root first comes into use — resume, open for mutation, fork source, tree
or child access — the root and every descendant are read and folded exactly
once in a single bulk pass. That pass assembles the tree, harvests the artifact
digests the tree references, and produces the tree's delegation-registry,
approval-grant, and goal-producer records; nothing produces them for a tree
that was never used. The children then leave the pass
without ever entering residency, so residency caps and janitor pressure are
unaffected; a child that actually runs becomes resident through the ordinary
write path and later evicts like any other session.

### Artifacts

Tool output is content-addressed under a digest-named file, and every artifact
belongs to exactly one tree: output written by a root or one of its descendants
lands in that root's `artifacts/`. A session reads only its own tree's
artifacts, so a `read_artifact` digest naming content another tree stored does not
resolve there. `read_artifact` therefore needs no permission rule: it is a
base-layer tool that declares no permission, always published and always
allowed.
A fork of a root hard-links every artifact its history references — and the
streams a named-stream manifest lists — into the new tree before the fork is
published, so it never depends on its source; a link that fails fails the fork. Collection is therefore per tree
and needs nothing from any other tree. It runs only on revert: a digest the
reverted events referenced is unlinked unless the tree's remaining root or
child logs still reference it, and the sweep is skipped while the tree runs or
a capture is publishing. Other orphans stay until the tree is deleted.

### Delegation and ownership

Delegation reservations, child publication, run start/attachment, and terminal
state use the parent session's `events.jsonl`. The engine projects a tree's
records during that tree's single load pass, root log included, and rebuilds
only that tree's registry entries; opening the engine projects nothing.
The reservation fingerprint is recomputed from the replayed request, child
agent snapshot, selected model suffix, and staged-skill provenance; a mismatch
rejects recovery. A background delegation the durable facts leave nonterminal is
recovered by adopting the child, whose own reconciliation terminalizes the run
that died with the previous process; the completion monitor then releases the
parent's slot and promotes whatever was queued behind it. A delegation event skipped by best-effort reading is absent
from the recovery projection and appears in the session's skipped-event
diagnostics, while other delegations continue to load.

Multiple cookie processes may share this project data directory. Ownership is
per root session tree, not per session and not per work dir: one `owner.lock`
guards a root and every delegated session filed under it, and the process that
locks it is the only writer anywhere inside that root's directory. It retains
the lock until process exit, including while every session in the tree is
evicted from memory. On Unix the lock is `<root-dir>/owner.lock`; on Windows it
is the adjacent `<root-session-id>.owner.lock` sidecar, so its open handle does
not prevent directory renames. Children have no lock file of their own in
either layout, and locks an older build wrote under `subagents/` are ignored
and swept away by the tree's load pass. New-root and fork publication acquire
the Windows sidecar derived from the final directory path before renaming the
temporary directory; publishing a child takes no lock, because its tree is
already held. Session discovery ignores the sidecar because it scans only
directories. Session listing reads `metadata` without locking, which is safe
because every rewrite of that cache stages a sibling temporary and publishes it
with a replacement that keeps the name resolvable throughout: `rename` on Unix,
and a superseding POSIX rename on Windows.

One tree lock does not make a whole tree writable. Opening a session for
mutation resolves its root and attempts that root's lock — which is where an
adoption reached through a child starts — and then adopts the session itself:
success enters a non-writable adoption state, reconciles only that session's
interrupted work, and then publishes ownership. Taking the tree through a child
therefore does not reconcile the root; the root reconciles when it is first
written, through its own adoption, which needs no second lock. A session is
writable only once its tree is held by this process *and* the session was
created here or adopted here.
Adoption also schedules delegation recovery for the session's tree, and a resume
waits — bounded, a few seconds — for that recovery to settle before it returns,
so background-delegation capacity never reads transiently over-counted; if the
bound expires the resume still succeeds and recovery finishes in the background.
A clean shutdown is the other half of that contract: it cancels its in-flight
runs and then waits, under its own bound, for those run tasks to record
`RunCancelled` while the actors and the store are still up. Only a crash, or a
task still wedged when that bound expires and is aborted, leaves a run for the
next startup to repair as interrupted by daemon restart.
Reconciliation failure revokes the tree's write capability for that session and,
when the failed adoption is the only thing holding the tree, releases the tree
lock so a later attempt can retry. Because one authority serves the whole tree,
dropping it invalidates every log in the tree at once: a retained event-log
projection cannot append after its store drops ownership. A live foreign owner
produces `session is owned by another cookie process` for every session in its
tree. Classification failures fail closed as foreign-owned.

Foreign sessions remain inspectable as read-only snapshots. The TUI disables
input for them and refreshes the snapshot when reopened; there is no live event
tail. Forking a foreign *root* is allowed because the fork is a new root with a
tree and lock of its own; forking a delegated session publishes into its
source's tree and therefore needs that tree's lock, taking it when it is free
and failing as foreign-owned when it is not. Grants and grant invalidations
committed by another process become
visible after restart. Concurrent MCP configuration edits remain last-writer
wins. Ownership failures in protocol 45 use an ordinary fault
message rather than a new wire error.

The data directory must be on a local filesystem with correct `flock` or
`LockFileEx` semantics. NFS-class and other network filesystems are unsupported
for this directory because they cannot guarantee single-writer ownership.

Legacy project-level `delegations.jsonl` files are ignored. In-flight
delegations that existed only in that pre-release journal are not recovered;
their child directories remain ordinary sessions available for inspection.

### Event bus

The durable session append path publishes raw events to independent bounded
plugin streams only after the JSONL append and projection publication complete.
These streams are best-effort observers and never participate in consistency or
backpressure. The non-durable engine bus accepts plugin sources as
`EngineEvent::PluginEvent`; it fans out to RPC frontends and other subscribed
plugins with session identity but without replay or cursor semantics. RPC fan-out
is connection-local and requires a successful event subscription for that session. A plugin
never receives its own bus events.

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
an authenticated WebSocket at `/ws`, protocol 45 current-only, `handshake` first.
Discovery is a single `runtime.snapshot.get` call that returns one coherent
runtime snapshot (schema 5). Session events stream through `events.subscribe`
and plugin bus events through `events.plugin`. Raw tool output is not streamed
to clients: displays come from live-only `tool_call_progress` and durable
terminal events, and retained output is read back through artifacts.

What moved is where the mechanics live: handshake, request/response
correlation, notification demux, replay/gap recovery, and shutdown are now
implemented once in the `protocol` crate and shared by the server, the TUI, and
the CLI.

See [Protocol](reference/protocol.md), [Events](reference/events.md), and
[Schemas](reference/schemas.md) for the wire details.

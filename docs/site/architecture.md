# Architecture

cookie agent is a subagent-first coding harness built as a Rust workspace of nine
crates. A local daemon owns provider connections, sessions, model execution,
permissions, and persistence; a terminal UI communicates with it over a versioned
JSON-RPC WebSocket protocol. Writers emit only the current schema for each
durable surface. Event logs reopen schemas 15-17 and delegation journals reopen
schemas 11-14; other surfaces remain current-only.

## Process model

The `cookie` binary (`crates/cookie_agent`) is a thin composition root. Its CLI has
four subcommands plus a default mode:

| Command | Behavior |
|---|---|
| *(none)* | Start an in-process daemon and open the TUI over an in-memory stream |
| `daemon` | Run only the daemon, listening on `ws://127.0.0.1:7419/ws` by default |
| `attach [--url]` | Attach the TUI to an existing daemon WebSocket |
| `connect [provider_id]` | Interactive durable managed-provider connection (TTY only) |
| `disconnect [provider_id]` | Interactive durable managed-provider disconnection (TTY only) |

The daemon binds only to the configured port and authenticates every WebSocket
with a bearer token stored at `~/.local/share/cookie_agent/daemon/token-v1`.
`attach`, `connect`, and `disconnect` accept only loopback `ws`/`wss` URLs whose
path is exactly `/ws`. All three use the same shared protocol client as the TUI,
so the CLI keeps working even when built without the `tui` feature.

## Crate layering

The workspace is layered bottom-up by dependency:

```text
identity
  └─ protocol        (wire types + session transport/client/server traits)
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
| `protocol` | Current-only wire contracts **and the protocol session layer**: RPC roots, events, session metadata, agent snapshots, JSON Schema and TypeScript bindings, the frame-level `Transport` trait, the `ClientProtocol`/`ServerProtocol` traits, the shared `Client`, `ServerContext`, protocol-owned `serve`, and shared setup-value parsing. Re-exports `identity` and hosts the unified wire types. |
| `models` | Dynamic provider/model runtime: models.dev catalog, family recipe registry, provider store, Oven adapters, compiled model manifests. Re-exports the capability wire types from `protocol`. |
| `config` | Strict runtime configuration and Markdown agent documents; layered user/workspace loading with secret zeroization. Re-exports `AgentMode`, `PermissionAction`, `PermissionEffect`, `PermissionRule`, and `AgentDocumentSource` from `protocol`. |
| `engine` | Session actors, run loops, permissions, approvals, delegation, compaction, internal agents, persistence. |
| `tools` | Built-in `read`, `write`, `edit`, and `bash` tools plus the `delegate_subagent`, `get_subagent_result`, and `cancel_subagent` provider. |
| `server` | The `ServerProtocol` implementation over `Engine`, concrete transports (WebSocket + `InProcessStream`), a thin connection wrapper, and the public `load_auth_token` / `validate_websocket_url` APIs. |
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
~/.config/cookie_agent/config.toml          # user layer
~/.config/cookie_agent/agents/<agent-id>.md
<exact-cwd>/.cookie-agent/config.toml       # workspace layer
<exact-cwd>/.cookie-agent/agents/<agent-id>.md
```

There is no upward workspace search. A workspace setting replaces the
corresponding user setting; a same-ID workspace provider or agent replaces the
complete user definition. Unknown keys, leftover schema/version fields, wrong
types, and malformed values are rejected without migration or silent ignores.
The TUI additionally reads an independent
`~/.config/cookie_agent/tui.toml` (or `$XDG_CONFIG_HOME/cookie_agent/tui.toml`).

See [Configuration](guide/configuration.md) and the
[configuration reference](reference/configuration.md) for every key.

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
4. **Executable adapters.** The Oven SDK crates
   (`oven-sdk-openai`, `oven-sdk-anthropic`, `oven-sdk-google`,
   `oven-sdk-google-vertex`, `oven-sdk-bedrock`, `oven-sdk-azure`,
   `oven-sdk-cohere`, `oven-sdk-open-responses`) provide normalized language-model
   implementations. The `models` crate selects the adapter for each compiled
   model and freezes it into project manifests.
5. **Provider store.** Managed connections live in a global per-user provider
   store (`~/.local/share/cookie_agent/providers/store-v3.json`) and are shared
   across workspaces. Credentials are checked on first use, not at connect time.

Each accepted run freezes its model selection into a project manifest under
`<cwd>/.cookie-agent/model-snapshots/`, so later catalog, configuration, or store
changes cannot silently change an accepted run's model behavior.

## Engine

`crates/engine` runs one actor per session. Actors serialize all mutations to a
session and drive the run loop:

- **Run loop.** Each run walks the agent's model fallback chain. On a retryable
  provider failure it advances to the next fallback and emits a `model_fallback`
  event; the fallback position is sticky for the rest of the run. Before each
  request the loop runs predictive compaction and, after a completed turn,
  post-check compaction (see [Compaction](guide/compaction.md)).
- **Permissions.** Every prepared tool call is matched against the agent's
  ordered permission rules (see [Permissions](guide/permissions.md)). The
  permission pipeline is stateless; approvals and tree grants live in an
  in-memory `ApprovalStore` rebuilt from durable events.
- **Approvals.** A stateless approval evaluator (the `approval` internal agent)
  classifies asks in `auto_approve` mode. `ask` skips the classifier, and `yolo`
  approves immediately. A doom-loop guard rejects repeated identical approvals.
- **Delegation.** The `delegate_subagent` tool reserves a child session under the parent,
  journals the invocation, and runs the target subagent with an inherited model
  suffix. Depth and concurrency limits come from `delegation` configuration.
- **Internal agents.** The approval, context-compaction, and session-title
  agents run with no tools and a strict text-only output contract, normally on
  the parent run's model via `${parent_model}`. See
  [Internal agents](guide/agents.md#internal-agents).

## Sessions and persistence

Sessions are append-only event logs. A new session exists only in memory until
its first user message, when its directory, `events.jsonl`, and `meta.json`
cache are created atomically. Revert appends a `session_reverted` marker and
fork copies a persisted prefix under a new session ID; neither truncates
physical events.

Session data lives under the user data directory keyed by a hash of the
canonical working directory:

```text
~/.local/share/cookie_agent/
  daemon/token-v1                  # daemon bearer token
  providers/store-v3.json          # durable managed connections
  catalog/                         # validated models.dev cache
  projects/<16-hex-cwd-hash>/
    cwd                            # canonical project path
    sessions/<session-id>/         # events.jsonl + meta.json cache
    artifacts/                     # content-addressed tool output
    delegations.jsonl              # delegation journal (writes 14; reads 11-14)
    grant-invalidations.jsonl      # tree-grant invalidation journal
    runtime-revisions-v8.jsonl     # runtime revision index
```

Schema 12 is readable for unambiguous records. Its unshipped intermediate
encoding used `delegation_run_started` for both a newly started resumed run and
an attachment to an existing run; schema-12 resume/start pairs are rejected
because their meaning cannot be recovered soundly. The error directs operators
to move the affected project's `delegations.jsonl` aside and restart. This
discards in-flight delegation recovery state AND historical child resumability:
child session event logs remain intact, but without the journal those sessions
no longer satisfy the journal-backed ownership checks required for
`resume_session_id`.

Project model-snapshot manifests live inside the workspace at
`.cookie-agent/model-snapshots/` and are the only per-project engine state the
workspace owns.

## Protocol surface

The wire protocol is unchanged by the session-layer refactor: JSON-RPC 2.0 over
an authenticated WebSocket at `/ws`, protocol 9 current-only, `handshake` first.
Discovery is a single `runtime.snapshot.get` call that returns one coherent
runtime snapshot (schema 4). Session events stream through `events.subscribe`
and tool output streams through separate snapshot/delta/gap notifications.

What moved is where the mechanics live: handshake, request/response
correlation, notification demux, replay/gap recovery, and shutdown are now
implemented once in the `protocol` crate and shared by the server, the TUI, and
the CLI.

See [Protocol](reference/protocol.md), [Events](reference/events.md), and
[Schemas](reference/schemas.md) for the wire details.

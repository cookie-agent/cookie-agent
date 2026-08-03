# Oven 0.4 model and catalog conformance

`crates/models` is cookie-agent's model-composition boundary. It constructs
published Oven adapters from explicit static declarations and from reviewed,
credential-backed models.dev recipes. It performs no model-name inference,
network discovery, build-time download, runtime catalog refresh, or provider
probe.

## Published Oven pins

The workspace pins exactly:

| Package | Version |
|---|---:|
| `oven-sdk` | 0.4.0 |
| `oven-sdk-anthropic` | 0.5.0 |
| `oven-sdk-openai` | 0.4.0 |
| `oven-sdk-google` | 0.4.0 |
| `oven-sdk-google-vertex` | 0.4.0 |
| `oven-sdk-bedrock` | 0.3.0 |
| `oven-sdk-azure` | 0.3.0 |
| `oven-sdk-cohere` | 0.2.0 |
| `oven-sdk-open-responses` | 0.2.0 |

Static declarations retain all listed adapters except MiniMax and Claude
Platform on AWS. Static Anthropic configuration uses
`native_context_discriminator`; Vertex derives
`google_vertex_native_context_scope`; official OpenAI and Azure Responses
require explicit compaction settings whenever `CompactionCapability::Native`
is declared. No former discriminator or Vertex scope names are accepted.

`ScriptedModel` implements deterministic FIFO streaming and native compaction,
including captured requests, delay, cancellation, exhaustion, and planned
errors.

## Pinned models.dev catalog

The exact upstream `snapshotPayload` from `anomalyco/models.dev` commit
`c3057690bbb8bd41cafdefadcd2a7b958e2a4642` is vendored at
`crates/models/catalog/models-dev.json`:

- size: 3,567,054 bytes;
- SHA-256: `d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a`;
- no trailing newline;
- MIT attribution: copyright 2025 models.dev.

The authoritative inputs are the upstream provider/model TOMLs, schema, and
generator. The upstream repository-root `models.json` is not the vendored
artifact. `scripts/update_models_dev.py --check --source ...` is strictly
offline and requires an already-prepared pinned checkout; it never clones or
runs `bun install`. Network cloning/dependency installation is isolated behind
explicit opt-in `--update`. Cargo builds, tests, and runtime code never invoke
the updater or access the network.

The parser retains the complete upstream document internally, applies bounded
record/string/date/limit validation, and emits provider/model projections in
stable provider/model order. Catalog revision is `sha256:<hex>` over the
canonical secret-free projection, including behavior-affecting effective
package/API recipe inputs. Canonical model IDs are emitted only when the exact
`<provider>/<model>` metadata key exists. Wrapper models are never guessed into
a canonical family.

Known and supported are distinct states. A provider can be present in the
catalog and still have no safe construction recipe.

## Generated recipe allowlist

Initial generated support is deliberately limited to:

- first-party Anthropic Messages;
- exact hand-reviewed OpenAI model IDs mapped to either Responses or Chat;
- first-party Google generateContent;
- first-party Cohere v2 Chat;
- OpenRouter's reviewed HTTPS compatible Chat endpoint;
- effective `@ai-sdk/openai-compatible` models whose endpoint is HTTPS and
  whose provider declares exactly one credential field.

Vertex, Azure, Bedrock, standardized Open Responses, MiniMax, Claude Platform
on AWS, ambiguous package reuse, insecure endpoints, deprecated offerings, and
records requiring provider body/header injection remain explicit unsupported
states. Experimental provider bodies/headers are ignored and never injected.

Generated descriptors are text-in/text-out only. They may use explicit catalog
tool-calling, structured-output, and temperature booleans, but never infer
parallel tools, tool-input deltas, top-p, media, reasoning, native replay, or
native compaction. Cancellation is local. Default maximum output is
`min(16_384, catalog_output_limit)`.

Generated aliases are exact `provider_id/model_id` strings. Static aliases may
not collide with them.

## Credential persistence

`provider.connect` values are not configuration fields. On Unix they are stored
at `~/.local/share/cookie_agent/credentials/store-v1.json` with a sibling lock:

- directories are current-user-owned mode 0700;
- store, lock, and temporary files are current-user-owned regular mode 0600;
- traversal is anchored at a current-user-owned, non-group/world-writable home
  or data directory and uses dirfd-relative no-follow opens for every component;
- symlinks, ancestor replacement attempts, hard-linked files, weak modes, and
  unexpected object types are rejected throughout traversal;
- every transaction takes an advisory cross-process lock and rereads under it;
- writes use a same-directory exclusive temp, file fsync, atomic rename, and
  parent-directory fsync;
- malformed, oversized, wrongly owned, or weak-permission state fails closed.

The store contains sorted credentials, connection timestamp, generation UUID,
catalog revision, a random local HMAC key, and durable idempotency receipts.
Receipts contain only HMAC-SHA256 over the canonical secret-bearing request,
never the raw secret. Reusing an ID with the same request returns the original
result; a different request conflicts. Persistent connect is disabled on
non-Unix platforms until equivalent ACL guarantees are implemented.

CLI and TUI credential entry use best-effort process-memory hygiene. Owned
input, request, and serialized-parameter buffers are wiped on submission,
cancellation, connection loss, and drop where ownership permits. The client
keeps a cancellation-safe guard around both the source request and its queued
serialized parameters; unavoidable allocator, transport, kernel, terminal, and
daemon-side copies remain outside that guarantee. Credential values are moved
between owned buffers rather than cloned for convenience.

Connect reporting is phase-specific: `provider.connect` acceptance, subsequent
`model.list` refresh, `agent.list` refresh, and optional initial
`session.create` are separate outcomes. An empty profile configuration is a
valid setup state and does not issue `session.create`. A configured enabled
profile whose models were unresolved may become runnable after connection;
profiles explicitly configured disabled remain disabled.

## Atomic model snapshots

`ModelSetManager` publishes immutable `Arc<ModelSnapshot>` values through an
atomic swap and serializes refresh/connect with a mutex. A candidate unions
explicit static models with eligible models from connected providers and is
fully constructed and validated before the credential transaction is committed
and before publication. There is no network probe.

Snapshots are retained by configuration fingerprint for the current daemon
lifetime only. On every credential refresh, each retained fingerprint is
rebuilt exclusively from the new candidate's concrete adapters when alias,
descriptor, defaults, and complete behavior fingerprint all match. Any retained
snapshot with an unmatched entry is dropped. Publication and frozen resolution
are serialized around the atomic swap, so already-acquired adapter handles may
finish while every later resolution uses the latest credential generation.

The retained map is not persisted. Restart rebuilds only the current snapshot
from validated current config, the pinned catalog, and latest credentials.
Obsolete persisted frozen bindings remain readable but fail execution when
their exact fingerprint is absent; there is no alias fallback. A binding whose
behavior is unchanged across secret-only rotation retains its revision and
resolves through the current adapter/current credentials.

Revisions include all safe behavior-affecting descriptor/default/catalog data
and exclude credential values; rotating only a secret can therefore preserve
the model revision.
Static config congruence uses the same per-entry behavior fingerprint, covering
endpoint identity, auth shape without credential values, adapter settings,
header names without values, declarations, defaults, native-context scope
inputs, and compaction settings.
Debug output and errors expose no endpoint credentials, header values, HMAC
keys, or credential values.

## Workspace configuration authority

Local `cookie` and `cookie daemon` startup unconditionally load and validate the
ordinary Figment-composed configuration stack (`built-in defaults < user TOML <
workspace TOML`) once for the current directory. Figment is used only for
default/TOML composition; environment is not a configuration layer and is
available solely through restricted explicit `${env:NAME}` interpolation in
approved model fields. There is no persisted workspace-acceptance state. A stale
`~/.local/share/cookie_agent/trust.json` is never inspected or modified, even
when malformed or represented by a symlink or FIFO; `attach` and `connect` do
not inspect current-directory configuration at all.

This makes the workspace layer authoritative configuration input: it may set
model endpoints and supported auth/header values through environment
interpolation, and its later permission rules may override matching user rules
under last-match ordering. Runtime operation authority is still the frozen
effective policy, any exact approval/tree grant it requires, and the validated
descriptor-bound prepared capability checked before execution.

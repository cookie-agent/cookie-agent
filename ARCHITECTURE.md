# cookie_code Architecture

**Status:** frozen current implementation contract

**Required versions:** configuration schema 7; agent document schema 1;
protocol 8; event schema 8; session JSONL 8; session metadata 8;
delegation-journal schema 8; runtime snapshot schema 1; catalog cache schema 2;
provider store schema 3; family recipe registry schema 1; project model-snapshot
manifest schema 1.

Only those versions are accepted. Configuration schema 6, protocol/event/
persistence 7, catalog cache 1, provider stores 1/2, and every unversioned or earlier replacement
are rejected. There are no migrations, compatibility readers, aliases, or dual
paths.

This file is authoritative. Exact types, ordering, failure behavior, startup,
RPC, persistence, and TUI semantics are in
[docs/agent-model-variant-redesign.md](docs/agent-model-variant-redesign.md).
Family registry 1 is described in
[docs/provider-conformance.md](docs/provider-conformance.md).

## 1. Core invariants

1. The daemon refreshes only `https://models.dev/catalog.json`. The URL is
   code-owned, receives no provider credentials, and never redirects.
2. Catalog startup selection is exactly network, validated cache schema 2, then
   bundled bootstrap. A selected body's revision is
   `sha256:<lowercase SHA-256 digest of the exact selected body bytes>`.
3. Structural candidate failure rejects one network/cache/bootstrap candidate.
   A bounded, structurally valid candidate quarantines malformed or ambiguous
   provider/model records independently so valid siblings survive.
4. `/connect` lists every valid current catalog provider and every configured
   managed provider removed from the current catalog. “Configured managed” is
   the union of authored `source = "models_dev"` definitions and provider-store
   records. Custom providers never appear in `/connect` and never use store 3.
5. Runtime TOML has exactly one `[providers.<id>]` namespace. Definitions are
   tagged `source = "models_dev" | "custom"`. Custom IDs begin with `custom.`.
6. A same-ID user/workspace provider definition is an atomic replacement. No
   provider, model, map, array, auth, or override field merges across layers.
7. A managed endpoint is an authored `base_url`, otherwise catalog `api`, otherwise
   the npm-family default. Catalog endpoint metadata is authoritative. An authored
   base URL requires authored auth in that same provider definition.
8. Credentials are semantic values. `api_key` is permitted only for an
   unambiguous one-secret default API-key method. `auth_override` is exactly
   `{ method, values }`. A recipe owns the reviewed wire header/signing form;
   registry 1 emits no credential query.
9. Every authored/catalog endpoint is absolute, bounded, query-free,
   userinfo-free, fragment-free, and HTTPS, except an exact code-reviewed
   loopback HTTP policy. Query strings are rejected, including empty `?`.
10. All managed supported, non-deprecated text-output models are automatic.
    Managed TOML has sparse `model_overrides`; it has no inclusion map.
11. Provider protocol/auth behavior comes from family registry schema 1, keyed by
    catalog `npm`. Catalog endpoint, shape, capability, modality, limit, and nested
    provider override values are compiled directly. Unknown npm families are unsupported.
12. `provider.connect` and `provider.disconnect` mutate only managed provider
    store state. Connect compiles before a single durable transaction; after a
    successful transaction publication is an infallible in-memory swap.
13. Runtime snapshot schema 1 is the sole coherent discovery surface. Legacy
    independently refreshed provider/model/agent list flows do not exist in
    protocol 8.
14. Runs persist exact safe model bindings, source kind, credential source, and
    config-override fingerprint. Rehydration never changes credential source.
15. Model keys split at the first `/`; a model ID may contain `/`.
16. Secrets never enter caches, revisions, snapshots, events, errors, logs,
    generated artifacts, session files, or TUI projections.
17. **Empty setup is valid.** Config schema 7 permits `providers` to be omitted
    or empty. When provider store 3 is also empty and no effective authored
    custom provider exists, startup publishes zero models/root-runnable agents
    and opens the TUI so `/connect` can bootstrap setup. Empty TOML does not hide
    existing per-user stored managed connections.

## 2. Component boundary

```text
TUI / CLI
    │ protocol 8
    ▼
server ─────────────── runtime.changed notifications
    │
    ▼
engine ── version-8 events/sessions and frozen run policy
    │
    ▼
model manager ── atomic RuntimeSnapshot schema 1
    │
    ├── catalog manager ── fixed HTTPS / cache 2 / bootstrap
    ├── family registry 1 ── npm-family/protocol/auth compilers
    ├── config loader ── schema 7 and agent schema 1
    └── provider store 3 ── managed durable connections only
```

The catalog manager validates catalog structure. The family registry classifies npm
packages and decides protocol/auth semantics; catalog records decide endpoints and capabilities.
and Oven construction. The model manager compiles a complete candidate runtime
before one atomic publication. The engine freezes one published snapshot at run
admission. Clients consume one coherent snapshot and notifications.

## 3. Filesystem layout and protection

Workspace configuration is only:

```text
<exact-cwd>/.cookie-agent/
  config.toml
  agents/
    <agent-id>.md
```

User configuration is only:

```text
~/.config/cookie_agent/
  config.toml
  agents/
    <agent-id>.md
```

There is no upward search. User and workspace configuration uses ordinary
filesystem reads. Config and agent files may be reached through symlinks and do
not have owner, mode, or hard-link restrictions. Wrong object types, oversize
content, duplicate keys, and unknown fields still fail closed.

On Unix, runtime user data is fixed below:

```text
~/.local/share/cookie_agent/
  catalog/
    models-dev-v2.json
    models-dev-v2.meta.json
    models-dev-v2.lock
  providers/
    store-v3.json
    store-v3.lock
  sessions/
```

Each project may additionally contain the private runtime subtree:

```text
<exact-cwd>/.cookie-agent/model-snapshots/
  <64-lowercase-hex>.json
  model-snapshots-v1.lock
```

The project cwd anchor may be an ordinary shared or worktree directory with any
owner-write/group-write mode. Its `.cookie-agent/model-snapshots` storage subtree,
like the global `cookie_agent`, `catalog`, `providers`, and session directories,
is current-user-owned mode `0700`. Cache/store/body/meta/manifest/lock/temp files
are current-user-owned mode `0600`, regular, and single-link. Traversal is
descriptor-relative and no-follow. A writable project anchor permits
collaborators to remove the private subtree and deny service, but not to inject
accepted storage objects. Every mutation locks and rereads, writes an exclusive
same-directory temporary file, fsyncs it, atomically renames it, and fsyncs the
parent. Unsafe private state fails closed; no permissive fallback path exists.

## 4. Configuration layering

Runtime defaults and catalog/recipes are not TOML layers. The only authored
layer precedence is:

```text
user config schema 7 < exact-cwd workspace config schema 7
user agent document < same-ID workspace agent document
```

A provider definition is atomic by `ProviderId`: if workspace TOML defines an
ID, the entire user definition with that ID is discarded before parsing and
semantic validation. Provider fields, model maps, overrides, variants, headers,
auth values, and arrays never merge. Agent documents are likewise atomic.

Managed `source = "models_dev"` definitions may directly author credentials in
either user or workspace TOML using `api_key` or typed `auth_override`. The
effective atomic definition's authored credential always takes precedence over
provider-store credentials. A workspace replacement never inherits a user-layer
credential. On recomposition, removing authored auth from a definition that has
no authored `base_url` makes an exact eligible provider-store credential
effective. If authored `base_url` remains, removing same-definition authored
auth is invalid rather than falling through to the store.

`${env:NAME}` interpolation is single-pass and allowed only in schema-approved
secret values and authored endpoint strings. Environment variables do not form
a config layer. Documentation recommends interpolation or `/connect` provider
store input rather than plaintext. Parsed and interpolated secrets live in
`SecretString`; owned buffers are best-effort zeroized on drop and after
dispatch. Transport/kernel copies cannot be promised forensic erasure.

## 5. Catalog architecture

The fixed request rejects redirects, cookies, configurable headers, auth,
userinfo, fragments, and every query. If cache schema 2 has a validated ETag,
only `If-None-Match` is added.

The request sends `Accept-Encoding: identity`. Any non-identity
`Content-Encoding` is rejected. A `Content-Length` greater than 16 MiB is
rejected before body read; absent or acceptable length is still streamed through
a hard 16 MiB counter before buffering, UTF-8 decoding, or JSON parsing. JSON is
bounded to depth 32, 4096 providers, 65,536 provider models per provider, 65,536
root canonical models, 1,000,000 total object/array entries, and 256 KiB per
string before narrower field limits.

Candidate boundaries are exact:

- the strict root is exactly `{ providers, models }`; both maps are required,
  bounded, and nonempty, and every other root field is unknown;
- invalid UTF-8, invalid JSON, non-object root, missing/non-object/empty
  `providers` or `models`, unknown top-level fields, root duplicate keys, or exceeded
  candidate/depth/container/string bounds is a **candidate failure**;
- once the bounded root provider map is structurally recoverable, each provider
  value is parsed independently;
- malformed provider-local shape, duplicate provider-local keys, invalid unique
  provider identity, or ambiguous normalized provider identity quarantines that
  provider and all of its models;
- within a valid provider, each model value is parsed independently; malformed
  model-local shape, duplicate model-local keys, invalid identity, or ambiguous
  normalized model identity quarantines only that model and all colliding peers;
- executable metadata is classified directly. Provider and nested
  model `npm` values classify protocol families; `api` and `shape` are authority.
  Unknown nested npm or shape values make only that model unsupported.

`providers` contains provider-scoped executable metadata: provider identity,
npm/API/env values, provider model records, and optional model provider
overrides. `models` contains canonical metadata/provenance keyed by canonical
model ID. It never supplies endpoint, protocol, auth, setup, package selection,
or executable inclusion. Provider model keys/embedded IDs and canonical model
keys/embedded IDs must agree. An exact provider-model key match to `models` is an
optional provenance cross-reference only; provider-scoped executable metadata wins
on disagreement, absence of a canonical record is allowed, and a malformed
canonical record quarantines only that canonical record/cross-reference.

Quarantine entries are safe diagnostics and never executable. Valid siblings in
the same candidate remain eligible. The candidate may publish when at least one
provider record is valid or safely quarantined with a recoverable unique ID.

Cache metadata schema 2 records the catalog revision exactly as
`sha256:<lowercase SHA-256 digest of the exact selected body bytes>`, ETag, byte length,
validation time, last-check time, source, stale flag, structural diagnostics,
and `last_error { code, safe_message, occurred_at } | null`. Network/cache/
bootstrap fallback updates stale/error metadata atomically even when body bytes
remain unchanged. Errors never contain bodies, URLs with secrets, or credentials.

## 6. Provider and endpoint architecture

All provider definitions live in `[providers.<id>]`:

- `source = "models_dev"`: optional `base_url`, `shape = "chat" | "responses"`,
  typed `setup`, `api_key`, `auth_override`, and sparse model overrides whose
  per-model fields may also include `shape`;
- `source = "custom"`: required `endpoint`, `adaptor`, `auth`, and nonempty
  explicit `models`; optional typed adapter `setup` and strict `headers`.

The top-level `providers` map defaults to `{}` and may be empty. It produces the
zero-model setup state only when provider store 3 is empty and no effective
authored custom provider exists; otherwise stored managed connections or
authored custom providers still materialize. Empty setup is not a configuration
error and must never produce `runtime providers must be nonempty`. Agent
fallback references with no current model are retained as unavailable
descriptors; their agents are not root-runnable.

Custom IDs must begin `custom.`; managed IDs must not. Custom providers are
config-only, are not `/connect` rows, and never read or write provider store 3.

Custom static headers are non-secret behavior metadata with bounded RFC-token
names and control/CRLF/NUL-free safe values. They never interpolate and are
included exactly, sorted by canonical lowercase name plus value, in custom
behavior/config fingerprints and model-snapshot manifests. They reject
case-insensitive duplicates and transport/protocol/auth-owned names, including at
least authorization, host, content-length, transfer-encoding, connection,
proxy-authorization, cookie/set-cookie, accept, content-type, and user-agent.
Managed and custom model requests use opencode 1.18.2's provider compatibility
identity, exactly `opencode/1.18.2 ai-sdk/provider-utils/4.0.27
runtime/bun/1.3.14`, through the transport-owned user-agent header.
Adaptor recipes extend the owned set. Secret header auth must use a typed auth
method; static header values are never secret/redacted.

For managed providers the endpoint precedence is exactly:

```text
same-definition authored base_url > catalog api > npm-family default
```

Neither provider store, another config layer's replaced definition, nor ambient
environment values supply an endpoint. Catalog API metadata is authoritative.
`${VAR}` placeholders derive required setup fields and are substituted before
endpoint validation.

An authored `base_url` requires `api_key` or `auth_override` in that same atomic
definition unless the recipe's selected method is reviewed no-auth. It also
requires every non-defaulted setup field in that same definition; only setup
fields explicitly declared defaultable by the recipe may use recipe defaults.
It cannot inherit store setup or credentials. This blocks store or replaced-user
state from attaching to a new endpoint.

## 7. Authentication architecture

Managed auth precedence is exact:

```text
same-definition api_key mapped by the recipe default method
> same-definition auth_override
> exact active provider-store-3 connection only without authored base_url
> reviewed no-auth
> unavailable
```

`auth_override` and `api_key` are mutually exclusive. `auth_override` is exactly
`{ method = <AuthMethodId>, values = { ... } }`; keys must exactly equal that
auth method's credential schema. Provider `setup` is separate and owns only
typed endpoint/resource fields. `api_key` is legal only when the recipe has one
default auth method with exactly one required credential field classified
`api_key`. Multiple auth methods, any additional required credential, or a
differently classified credential is ambiguous and requires `auth_override`.

Provider-store auth is considered only when there is no authored auth and no
authored `base_url`. Its compatibility scope is provider ID, code-owned
`recipe_fingerprint`, recipe default normalized base URL/endpoint identity,
canonical safe setup fingerprint, and auth method/credential compatibility. For
nested npm families, compatible methods and semantic fields are mapped explicitly
(for example API-key to bearer API-key, or OAuth access token to bearer API-key).
Azure Foundry models classified as `@ai-sdk/anthropic` use the package's
`x-api-key` API-key behavior. Bedrock Converse uses SigV4 credentials, while
nested Bedrock Mantle Responses requires a Bedrock API key as bearer material;
models incompatible with the selected connection auth remain unavailable.
No endpoint,
host-family, method, provider-name, or identity fallback exists.

`source_record_digest` and `recipe_fingerprint` are independent secret-free
identities. `source_record_digest` is immutable provenance: SHA-256 over the
canonical exact validated catalog provider record selected to compile that
runtime or manifest. `recipe_fingerprint` is compatibility: a canonical hash of
Family registry 1 revision/schema, compiler version, selected family/adapter/
setup/auth/endpoint identity, and catalog-derived model semantics.
Provider store 3 persists both. A harmless catalog metadata refresh produces a
new source-record digest but the same recipe fingerprint, so the unchanged
stored setup and credentials remain eligible. Family, adapter, protocol, setup,
auth, endpoint-policy, compiler, registry, or model-exception drift changes or
quarantines the recipe identity and blocks reuse.

Managed setup precedence is complete same-definition authored `setup`, then
provider-store setup, then explicitly declared recipe defaults, then
unavailable. An authored setup map never field-merges with store or the replaced
user definition; it must supply every non-defaulted field, while recipe defaults
may fill only fields declared defaultable. Provider store 3 stores normalized
non-secret setup values and secret auth credentials plus policy/scope, exact
source-record provenance, recipe compatibility fingerprint, generation, and
receipt metadata. Store state revisions and connect receipt digests cover both
fingerprints. Any non-schema-3 store, including `store-v2.json`, is rejected.
Custom setup is authored-only and custom compilation/
fingerprints/rehydration never depend on provider store. Every family-registry setup
field is non-secret behavioral metadata and its normalized value participates
directly in safe behavior/config fingerprints where relevant. Every secret or
sensitive value belongs to auth, is excluded from fingerprints, and may rotate
without behavior-fingerprint change. A future non-auth sensitive setup need
requires a future schema/recipe version and independent mechanism.

If a managed definition has no authored `setup` and no authored `base_url`, its
exact stored setup may become effective. Removing authored setup, auth, and
`base_url` on recomposition permits exact stored setup and stored auth to become
effective. An authored `base_url` disables all stored setup and stored auth
inheritance.

## 8. Providers and models

Managed provider support is `npm`-family based. The static registry is keyed by
the package values documented in `docs/provider-conformance.md`; unknown package
families use `no_known_protocol_family`. Each model first applies its nested
`provider { npm, api, shape }` override. Unknown nested families affect only that
model. OpenAI defaults to Responses; authored provider/model shape may select
Chat or Responses, and catalog `responses`/`completions` shapes route likewise.

Capabilities are derived from catalog `tool_call`, `structured_output`,
`temperature`, `reasoning`, `reasoning_options`, input modalities, and context/
output limits, plus protocol-required native replay. Anthropic-compatible models
with reasoning require native replay so signed thinking blocks survive tool-use
continuations. Interleaved reasoning metadata selects the compatible reasoning
field. Deprecated and non-text-output models are omitted.

`/connect` contains every current structurally valid/quarantined catalog provider
with recoverable ID plus authored or store-backed managed providers absent from
the current catalog. Removed configured providers use state `removed`; they are
connectable only when family registry 1 still has an exact family/package/
source recipe match retained in a prior durable connection or other validated
safe source projection. Otherwise they remain visible with
`removed_without_retained_recipe_match`. Custom providers never appear.

For each supported managed provider, the runtime automatically includes every
non-deprecated, text-output model that compiles honestly. Sparse model
overrides may disable a model or alter only recipe-approved defaults, variants,
and display text. They cannot invent an absent model, capabilities, protocol,
auth, package, or endpoint.

Custom providers manually define every model and complete honest capabilities.
A custom model ID may contain `/`. A model key splits once at its first slash.

## 9. Connect, disconnect, and publication

`provider.connect` requires an expected catalog revision exactly equal to
`sha256:<lowercase SHA-256 digest of the exact selected body bytes>`. It accepts current managed providers and
configured managed providers absent from the catalog when recipe/source matching
succeeds. Absence of a prior store record is a normal upsert.

The service holds the serialized runtime-mutation lock, then the provider-store
lock; rereads the store; assigns the final store revision, connection generation,
and receipt into a proposed state; validates exact recipe-typed setup and auth
fields; and compiles the complete provider/model/agent/runtime candidate against
exactly that proposed state. Only then does one provider-store transaction write
normalized setup, credentials, connection policy metadata, and idempotency
receipt. After durable commit, publishing the already compiled
`Arc<RuntimeSnapshot>` is an infallible atomic swap. No fallible refresh or
revision allocation occurs between durable write and publication.

The response contains `durable_connection`, `effective_auth_source`, the full
coherent runtime snapshot, and `replayed`. A stored update never outranks
same-definition authored auth; therefore a successful durable update may report
effective source `authored_api_key` or `authored_override`.

`provider.disconnect` is managed/store-only. It removes the exact stored active
setup and credentials with the connection and commits its receipt atomically,
compiles before commit, then
infallibly publishes. It never edits TOML, removes authored auth, touches custom
providers, or deletes a different scope. Custom disconnect returns typed
`custom_provider_not_store_backed`.

Disconnect params contain provider ID, client idempotency ID, expected runtime
and provider-state revisions, and optional expected connection generation. Same
ID/same canonical payload replays the durable result; same ID/different payload
is `idempotency_conflict`. An absent managed provider is a successful idempotent
disconnect with a durable receipt. The result includes receipt, provider ID,
`disconnected = true`, post-removal effective auth state, coherent
`RuntimeSnapshotResult`, and `replayed`. Present removal or absent no-op plus
receipt is one atomic store transaction after candidate compilation, followed by
infallible publication with `ProviderDisconnected`; replay publishes nothing.

## 10. Runtime snapshot and notifications

Runtime snapshot schema 1 contains the snapshot schema version, recipe registry
revision, catalog revision/source/cache state, provider-state revision, model
revision, provider-store generation, agent revision, aggregate runtime revision,
provider descriptors, model descriptors, and materialized agent descriptors.

`runtime.snapshot.get` is mandatory and atomically returns the entire object.
Protocol 8 removes legacy independently refreshed catalog/provider/model/agent
list RPCs and list-refresh choreography. Every mutation returns the published
snapshot. The server emits `runtime.changed { previous_revision, snapshot,
reasons }`; Rust reason variants and wire values include
`ProviderConnected`/`provider_connected`,
`ProviderDisconnected`/`provider_disconnected`,
`ProviderStoreChanged`/`provider_store_changed`, and
`ProviderStoreReloaded`/`provider_store_reloaded`, in addition to startup,
catalog, config, and agent reasons.

The local durable connect transaction publishes `ProviderConnected`; local
disconnect publishes `ProviderDisconnected`. Detecting another process's newer
store generation publishes the pair `ProviderStoreChanged` and
`ProviderStoreReloaded` after successful coherent recompilation.

Provider store 3 is per-user global across workspaces. Before discovery RPCs,
session admission, or root-run admission, the process locks and rereads the
store generation. A changed generation blocks admission, recompiles and
publishes a coherent runtime with both `ProviderStoreChanged` and
`ProviderStoreReloaded`, then retries against that snapshot. Reload failure
returns `provider_store_reload_failed`; accepted runs remain unchanged.

## 11. Project model-snapshot manifests and exact rehydration

Project model-snapshot manifest schema 1 is stored only at
`<exact-cwd>/.cookie-agent/model-snapshots/<64-lowercase-hex>.json`, with exact
lock `model-snapshots-v1.lock`. The filename digest and manifest
`revision = "sha256:<digest>"` are SHA-256 over RFC 8785 JCS bytes of the
self-contained secret-free `payload` only; envelope schema/revision are excluded
to avoid self-reference. Each manifest contains catalog, recipe
registry, provider-state, and model revisions plus compiled safe blueprints.

Each blueprint records exact model/variant selection, descriptor/capabilities,
safe endpoint identity, provider/protocol/auth recipe IDs and compiler versions,
source kind, exact source-record digest, independent recipe fingerprint,
config-override fingerprint, credential
binding source/method/semantic field names, normalized defaults/options/variants,
and behavior fingerprints. Custom static header names/values are included as safe
behavior metadata. Auth credential values, generated auth-owned header values,
environment values, live handles, raw catalog records, and provider-native
private payloads are forbidden.

The cwd anchor need only be an actual directory and may be shared or writable;
the `.cookie-agent/model-snapshots` subtree is current-user-owned `0700` and
manifests/lock/temp files are current-user-owned `0600`, regular, single-link,
descriptor-relative/no-follow, bounded, and atomically written by lock/reread,
exclusive sibling temp, fsync, rename, and parent fsync. A manifest is durable
before any version-8 event may reference its revision. Referenced manifests are
retained for the lifetime of their sessions and delegation journals and are
never garbage-collected; family registry 1 performs no automatic manifest GC.

Startup scans direct matching filenames in sorted byte order, validates filename
digest, strict schema, RFC-8785 reserialization/payload digest, unique blueprint
fingerprints, and bounds, then indexes them by revision. Maps use JCS ordering;
models/variants/setup/binding-name arrays are ID-sorted, semantically ordered
arrays are preserved, JSON floats are forbidden, decimal domain values are
normalized strings, integers are I-JSON-safe, and Unicode code points are
preserved without normalization.
Unsafe objects or malformed matching
files fail project open. Session/journal references to absent manifests fail
reconciliation without deleting history.

Version-8 frozen bindings reference one manifest revision/blueprint and record
the accepted exact selection. Their managed source carries both the immutable
source-record provenance and recipe compatibility fingerprint. Runtime provider
descriptors expose the safe recipe fingerprint, and persisted events/delegation
journals retain it transitively through their exact frozen bindings. No secret
value is stored.

Credential source is frozen as `authored_api_key`, `authored_override`,
`provider_store`, or `no_auth`. Rehydration never changes it:

- authored config sources require the current same-ID atomic provider definition
  to have the same source kind and safe config-override fingerprint; missing or
  changed config fails and never falls to provider store;
- provider-store sources require the exact managed provider, current
  `recipe_fingerprint`, recipe endpoint policy, normalized setup values/
  fingerprint, and auth method/shape; config is not substituted. They do not
  require the store connection's historical `source_record_digest` to equal a
  newly refreshed manifest's source digest;
- managed bindings reconstruct from persisted safe source projection only when
  family registry 1 still has the exact family fingerprint/package/protocol/
  compiler match. The binding and referenced blueprint must retain their own
  exact source-record digest, but source provenance does not scope credential
  compatibility; current catalog presence is unnecessary;
- custom bindings require a current `source = "custom"` definition whose safe
  definition fingerprint exactly matches the frozen fingerprint; custom has no
  store fallback;
- every reconstruction must reproduce the frozen behavior fingerprint.

New root runs always resolve against the current coherent runtime and durably
write/reference its manifest. A harmless catalog refresh therefore freezes the
new source-record digest while retaining the same compatible store credentials.
Previously accepted runs retain their older manifest/source provenance and may
rehydrate the same store secret through the unchanged recipe/auth/setup identity.
Delegated sessions remain pinned to the invoking parent's accepted manifest and
frozen suffix. Once a run is accepted, catalog, config, provider-store,
manifest-directory, or runtime changes do not reinterpret it.

Typed failures leave history readable and never substitute another model:
`snapshot_config_mismatch`, `snapshot_credentials_unavailable`,
`unsupported_snapshot_recipe`, and `snapshot_rehydration_mismatch`.

## 12. Normative startup order

Startup order is frozen and must not be reordered:

1. **Schema 7 and agents:** securely open roots, load family registry 1, then
   strictly load atomic config schema 7 and agent documents.
2. **Catalog:** securely open cache schema 2, perform the bounded identity-only
   network request, then select network, validated cache, or bundled bootstrap
   and apply record quarantine.
3. **Provider store:** lock and load provider store 3 and its generation.
4. **Effective providers:** combine authored definitions, global stored managed
   connections, catalog metadata, and code-owned recipes.
5. **Coherent runtime:** compile all effective providers/models/agents and build
   one runtime snapshot 1; an empty effective set is valid.
6. **Project manifests:** scan/validate model-snapshot manifests 1 and rehydrate
   every referenced safe blueprint needed by project sessions.
7. **Engine:** open version-8 session/event/delegation state and reconcile it
   against the manifest index; accepted/delegated bindings stay pinned.
8. **Service:** atomically publish runtime, open server/TUI, then emit startup
   `runtime.changed`.

Any failure before step 8 opens no server/TUI except the documented usable
catalog fallback paths. Cross-process provider-store generation reconciliation
is mandatory again before discovery, session admission, and root-run admission.

## 13. TUI contract

The TUI consumes only runtime snapshot schema 1 and `runtime.changed`. Required
global/row states are:

- `loading`: no snapshot yet; controls disabled;
- `empty`: after authored providers and global store records are applied, a valid
  snapshot has zero models or zero root-runnable agents. The
  Message model/draft display is exactly `type /connect to continue`; it has no
  Model or Variant hit region, ordinary text/run submission is blocked with the
  same guidance, and `/connect` remains accepted;
- `ready`: live or not-modified catalog selected with no unresolved global error;
- `stale`: validated cache used after refresh failure; existing rows remain
  usable and a durable global explanation names the safe error/time;
- `bootstrap`: bundled catalog used; durable global explanation remains visible;
- `unsupported`: row has typed unsupported reason; Enter opens details only and
  never starts connect;
- `disconnected`: supported managed row lacks complete effective setup and/or
  auth; Enter opens separate public setup and secret credential controls from
  recipe descriptors;
- `connected-reconnect`: effective stored state exists; Enter opens reconnect/
  update with public setup prefilled and secret credentials blank; disconnect
  removes both stored setup and credentials;
- `removed`: configured managed provider or retained session model is absent
  from current catalog; details explain whether recipe-matched connect or session
  rehydration is available;
- `error-retry`: no runtime snapshot or an operation failed without a usable
  fallback; Enter/retry performs the explicit operation.

Global stale/bootstrap/error explanations are durable application state, not
toasts, and remain visible across picker navigation until a newer snapshot
clears or replaces them. Active frozen runs never mutate on refresh/connect/
disconnect. Unsupported Enter is details-only.

`loading`, `empty`, and `error-retry` are distinct: loading has no snapshot;
empty has a valid coherent snapshot but no runnable draft; error-retry has no
usable snapshot or a failed explicit operation. After connect publishes a
coherent snapshot with a root-runnable agent/model selection, normal structured
draft attribution replaces the exact guidance and its Model/Variant hit regions
become active. If a durable connection still yields no root-runnable selection,
the valid `empty` state and guidance remain; no model is fabricated.

`/connect` always displays the exact durable copy
`Stored setup, connections, and credentials are per-user and shared across workspaces.`
Stored setup is non-secret and projected only where recipe policy permits;
credential values remain secret/redacted.
Connect/disconnect changes become visible to other workspace daemons through
provider-store generation reconciliation.

## 14. Validation ownership

Required validation covers every version rejection, secure file mode/ownership/
link attack, exact startup order, identity-only streamed 16 MiB acquisition,
ETag and stale/error metadata, candidate versus exact-record quarantine,
strict root `providers`/`models` roles and cross-references, bundled/live npm/API/
env/model-override metadata, Vertex family rejection, atomic provider replacement,
separate provider setup schemas and auth methods, complete custom-model validation, endpoint/auth
precedence, all-input-query rejection, connect/disconnect absent/replay/conflict
transactions, cross-process store-generation reload, infallible publication,
coherent notifications, RFC-8785 manifest digest/ordering/mismatch/retention/
rehydration, slash-containing IDs, frozen credential-source rehydration, and
every TUI state.

TUI validation must exercise the actual rendered/input buffer and hit map, not
only helper strings: absent/empty provider TOML plus empty provider store and no
effective custom provider; nonempty store with empty TOML; zero-model snapshot;
byte-exact guidance; no Model/Variant target; blocked ordinary submission with
no run RPC; accepted `/connect`; all-provider discovery RPC; and guidance removal
after a coherent runnable refresh. Connect tests must cover recipe-derived setup
and auth descriptors, public/secret controls, defaults, missing/extra rejection,
absent-provider upsert, reconnect replacement, cross-workspace setup persistence,
and disconnect removal of setup plus credentials. Server tests must prove empty startup and
typed run rejection without `runtime providers must be nonempty`.

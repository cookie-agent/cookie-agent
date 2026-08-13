# Dynamic Provider, Catalog, and Session Contract

**Status:** frozen implementation specification

**Versions:** config 7; agent document 1; protocol/event/session 8; runtime
snapshot 1; catalog cache 2; provider store 3; family registry 1; project
model-snapshot manifest 1. Every prior project version is rejected without
migration or compatibility decoding.

[ARCHITECTURE.md](../ARCHITECTURE.md) is authoritative. This document fixes the
exact schema and behavior for implementation.

## 1. Config schema 7

There is exactly one provider map:

```rust
pub struct RuntimeConfig {
    pub schema_version: ConfigSchemaVersion, // exactly 7
    pub server: ServerConfig,
    pub tool_output: ToolOutputConfig,
    pub approval: ApprovalConfig,
    pub context_compaction: ContextCompactionConfig,
    pub session_title: SessionTitleConfig,
    pub providers: BTreeMap<ProviderId, ProviderDefinition>,
}

pub enum ProviderDefinition {
    ModelsDev(ModelsDevProvider), // source = "models_dev"
    Custom(CustomProvider),       // source = "custom"
}

pub struct ModelsDevProvider {
    pub source: ModelsDevTag,
    pub base_url: Option<EndpointUrl>,
    pub setup: BTreeMap<SetupFieldId, ConfigSetupValue>,
    pub api_key: Option<SecretString>,
    pub auth_override: Option<AuthOverride>,
    pub model_overrides: BTreeMap<ProviderModelId, ManagedModelOverride>,
}

pub struct AuthOverride {
    pub method: AuthMethodId,
    pub values: BTreeMap<AuthFieldName, SecretString>,
}

pub struct CustomProvider {
    pub source: CustomTag,
    pub endpoint: EndpointUrl,
    pub adaptor: AdapterId,
    pub setup: BTreeMap<SetupFieldId, ConfigSetupValue>,
    pub auth: AuthDefinition,
    pub headers: BTreeMap<HeaderName, SafeStaticHeaderValue>,
    pub models: BTreeMap<ProviderModelId, CustomModelDefinition>,
}

pub struct AuthDefinition {
    pub method: AuthMethodId,
    pub parameters: BTreeMap<AuthParameterId, SafeAuthParameterValue>,
    pub values: BTreeMap<AuthFieldName, SecretString>,
}

pub enum SafeSetupValue {
    String(BoundedSetupString),
    Code(SafeCode),
    Integer(i64),
    Bool(bool),
}

pub type ConfigSetupValue = SafeSetupValue;
```

TOML setup values use the separate `setup` map and exact `SafeSetupValue` scalar
forms. The selected
code-owned provider setup recipe validates each exact setup field as bounded,
non-secret behavioral routing/configuration metadata. Family registry 1 has no
sensitive setup field. `auth_override` and custom `auth` own credential fields
only.

Custom `auth.parameters` defaults to `{}` and contains method-specific safe
parameters only, such as an allowlisted `header_name` for
`api-key-header-v1`. Custom `auth.values` contains secret credentials only.
The selected auth method strictly defines required/optional parameter and
credential fields; missing, extra, or unknown fields fail.

`schema_version` is required. `providers` is optional, defaults to `{}`, and may
be explicitly empty. Provider structs deny unknown fields. Managed providers
have only `source`, `base_url`, `setup`, `api_key`, `auth_override`, and
`model_overrides`; all except `source` are optional and empty by omission.
Custom providers have only `source`, `endpoint`, `adaptor`, `setup`, `auth`,
`headers`, and `models`; `setup`/`headers` default empty and `models` is
required nonempty. There is no `custom_providers`, `catalog_revision`,
`catalog_url`, managed `models` inclusion map, `endpoint` on managed providers,
or `base_url` on custom providers. Empty configuration must not fail with
`runtime providers must be nonempty` or any equivalent nonempty-map check. It
yields zero models only when provider store 3 is also empty and no effective
authored custom provider exists; stored per-user managed connections still apply
when TOML providers are absent or empty.

Managed provider IDs must not start `custom.`. Custom provider IDs must start
`custom.`. An ID cannot change source kind through catalog data. A model ID is
bounded visible UTF-8 and may contain `/`. `ModelKey` splits at the first `/`.

### 1.1 Layering

User and exact-cwd workspace TOML are the only authored layers. A workspace
provider with the same ID discards the whole user provider before decode and
validation. Definitions are atomic; no field, auth map, model map, override,
variant, header, or array merges. Same-ID agent documents are also atomic.

Managed provider credentials are valid authored config fields in either layer.
The effective atomic definition may contain ergonomic `api_key` or typed
`auth_override`; that authored credential outranks any provider-store record. A
workspace same-ID replacement cannot inherit the discarded user definition's
credential. After reload/recomposition, removing authored auth while leaving
`base_url` absent permits an exact provider-store credential to become effective.
Keeping authored `base_url` while removing same-definition auth fails validation
and never falls through to store credentials.

### 1.2 Managed model overrides

```rust
pub struct ManagedModelOverride {
    pub enabled: Option<bool>,
    pub display_name: Option<String>,
    pub defaults: PartialRequestDefaults,
    pub variants: BTreeMap<VariantId, VariantDirective>,
    pub default_variant: Option<ConfiguredModelDefault>,
}
```

Overrides are sparse and may target only a model present in the selected
catalog source projection retained for that provider. They cannot invent a
model or author capabilities, protocol options, package, endpoint, or auth.
Every reviewed supported non-deprecated text-output model is included unless an
effective override explicitly disables it.

Google Vertex additionally requires the exact Registry-1 unoverridden Gemini
predicate in [provider-conformance.md](provider-conformance.md). Nonmatching
records, including `openai/gpt-oss-20b-maas` and
`openai/gpt-oss-120b-maas`, quarantine as
`unsupported_vertex_model_family`; they never route through Gemini.

Custom models state complete display, capabilities, defaults, variants, and all
adaptor-required options. No behavior is inferred from an ID.

### 1.3 Custom model definition

```rust
pub struct CustomModelDefinition {
    pub enabled: bool, // default true
    pub display_name: String, // required
    pub capabilities: ModelCapabilities, // required and complete
    pub defaults: RequestDefaults, // default {}
    pub options: CustomProviderOptions, // default {}
    pub variants: BTreeMap<VariantId, VariantDirective>, // default {}
    pub default_variant: Option<ConfiguredModelDefault>, // omitted = base
}
```

Every level denies unknown/duplicate fields. Custom definitions derive nothing
from either catalog root map. `display_name` is nonblank, control-free UTF-8 of
1..=512 bytes. `enabled = false` retains validation but excludes selection.

Complete capabilities require nonempty input/output modality sets, positive
`context_tokens` and `output_tokens` with output not exceeding context, and
explicit booleans for tool calling, parallel tool calls, structured output,
reasoning, temperature, top-p, and seed. They also require explicit
`native_replay` and `cancellation` enums plus a complete
media map. Parallel tools require tool calling. Each declared non-text input
modality requires exactly one matching media entry with nonempty MIME types and
positive byte/count limits; undeclared modalities and output modalities cannot
have media entries.

Defaults must fit limits/capabilities. Tool choice requires tool support.
Reasoning is authorable only by a variant reasoning directive and must compile
losslessly. Replay/compaction/cancellation capabilities must be implemented by the
selected adaptor; no optimistic declaration is accepted. `options` is the
strict adapter-specific option type selected by `adaptor`; missing required or
unknown options fail.

`ProviderModelId` is 1..=384 bytes of visible UTF-8, has no controls or leading/
trailing whitespace, may contain `/`, and may not contain `[` or `]`. The model
key remains bounded to 512 bytes and splits only at its first `/`. `VariantId`
matches `[a-z0-9][a-z0-9._-]{0,63}`; `base` is reserved. Variant directives are
strict `add|replace|disable`; base cannot be targeted. Omitted
`default_variant` means exact base for a custom model, explicit `base` also means
base, and any other value must name an enabled final variant.

The custom provider `endpoint` and `adaptor` select transport construction.
Adapter-specific `setup` supplies only typed non-auth fields; `auth` supplies
only credentials. Setup cannot replace `endpoint`, select another adaptor, or
inject arbitrary request fields. Endpoint, adaptor, setup, auth, options, and model
capability mismatch fails the entire atomic custom provider.

### 1.4 Custom static headers

`CustomProvider.headers` is non-secret static behavior metadata. Header names
must be nonempty ASCII RFC field-name tokens using letters, digits,
`!#$%&'*+-.^_`, U+0060 (backtick), or `|~`; they are canonicalized to lowercase ASCII for
identity, and are unique case-insensitively. `SafeStaticHeaderValue` is bounded
UTF-8 with no C0/C1 controls, DEL, NUL, CR, or LF. Static headers permit at most
64 entries, 128 bytes per name, 8192 bytes per value, and 65,536 aggregate
name/value bytes.

Static header values never interpolate. Any value containing the literal prefix
`${env:`—whether or not it completes a valid
`${env:[A-Z_][A-Z0-9_]*}` form—is rejected rather than expanded. Static values
are never redacted or treated as secrets: they may appear exactly in safe
snapshots, manifests, and diagnostics, so authors must put every secret in
`auth`.

Forbidden static names are at least `authorization`, `host`, `content-length`,
`transfer-encoding`, `connection`, `proxy-authorization`, `cookie`,
`set-cookie`, `accept`, `content-type`, and `user-agent`. The selected adaptor,
protocol, and auth compiler extend this set with every owned header, including
the selected API-key header and provider version headers such as `x-api-key`,
`x-goog-api-key`, `api-key`, and `anthropic-version` when applicable. Any
case-insensitive collision with transport-, protocol-, or auth-owned headers
fails the entire custom provider.

Canonical custom fingerprints and model blueprints include static headers sorted
by canonical lowercase name and include each exact safe value. Changing a static
header changes behavior/config fingerprints and makes an older custom snapshot
fail exact rehydration. Auth fingerprints include method, safe parameters such as
owned header name, and credential field names, but exclude credential values;
secret rotation does not change behavior identity. Managed models.dev provider
definitions have no `headers` field; an attempted managed static header is an
unknown-field error unless a future recipe/schema explicitly adds one.

## 2. Endpoint and query rules

Every catalog API template, managed `base_url`, and custom `endpoint` must be an
absolute URL no longer than 2048 UTF-8 bytes. Userinfo, fragment, and any query
component are rejected, including a bare trailing `?` or an empty parsed query.
HTTPS is required. HTTP is accepted only when the exact selected recipe or
adaptor policy names loopback hosts `127.0.0.0/8`, `[::1]`, or `localhost` and
the explicit port/path also satisfy that policy. DNS names resolving to loopback
do not qualify.

Managed endpoint precedence is exactly:

```text
same atomic definition base_url
> nested model provider.api
> catalog provider api
> Family registry 1 default
```

Provider-store state never provides an endpoint. Catalog provider/model API
templates are family metadata: nested model metadata overrides provider metadata,
`${VAR}` placeholders derive typed setup fields, and the resolved endpoint is
normalized under the selected family's endpoint policy.

An authored `base_url` requires same-definition `api_key` or `auth_override`
unless the selected recipe explicitly declares no-auth. It also requires every
required non-defaulted setup field in the same definition; recipe defaults may
fill only setup fields explicitly declared defaultable. A base URL can never
inherit provider-store setup or auth state from a replaced user definition.

## 3. Authentication

`auth_override` has exactly `method` and `values`; `values` must contain exactly
the selected method's semantic fields. Unknown, missing, duplicate, or extra
fields fail. Effective `api_key` and `auth_override` are mutually exclusive.

The ergonomic `api_key` field is accepted only when Family registry 1 declares
one default auth method with exactly one required credential field classified
`api_key`. If multiple auth methods are viable, another credential is required,
or the sole credential is not classified as an API key, `api_key` is ambiguous and configuration must use
`auth_override = { method = ..., values = ... }`.

### 3.1 Setup versus auth precedence

Provider setup and auth never share a field. For a managed provider, effective
setup precedence is complete same-definition authored `setup`, then exact
provider-store setup, then code-owned defaults for fields explicitly declared
defaultable, then unavailable. Authored setup never field-merges with store or a
replaced user definition; it must contain every non-defaulted required field.
Missing/extra fields fail. Custom setup is authored-only and validated by the
selected adaptor.

Setup contains endpoint/resource data such as Vertex `project`/`location`,
Bedrock `region`, Azure `resource_name`/`deployment`/`api_version`, or a reviewed
provider `workspace`. Auth contains only credentials such as `api_key`,
`access_token`, `access_key_id`, `secret_access_key`, or `session_token`.

Managed auth precedence is exact:

```text
same-definition api_key
> same-definition auth_override
> exact active provider-store connection only without authored base_url
> reviewed no-auth
> unavailable
```

Store auth is eligible only when neither authored auth nor authored `base_url`
exists. Its scope must exactly match provider ID, recipe default normalized base
URL/endpoint identity, canonical safe setup fingerprint, and an allowed auth
method. A nested npm family may map a compatible source method and semantic
credential field to its effective method (for example API-key to bearer
API-key, or OAuth `access_token` to bearer `api_key`); unrelated methods and
field shapes remain ineligible.
Authored auth remains effective after any durable connect update. Custom
providers use only their authored `auth`; they never use the store.

Store setup is eligible only when authored `setup` and authored `base_url` are
both absent. Removing authored setup, authored auth, and authored `base_url`
allows exact stored setup and auth to become effective on recomposition. An authored
`base_url` blocks both store setup and store auth.

Auth method IDs represent semantics, not wire syntax. Family registry 1 maps
semantic fields to bearer, provider header, explicit access-token, SigV4, or
another reviewed encoder. Catalog does not define provider setup schemas or auth
methods.

## 4. Interpolation, ownership, and secrets

`${env:NAME}` is single-pass and allowed only in managed `base_url`,
string-valued `setup.*`, managed `api_key`, `auth_override.values.*`, custom `endpoint`,
and custom `auth.values.*`. Custom static headers never interpolate. `NAME` matches
`[A-Z_][A-Z0-9_]*`; missing or non-UTF-8
values fail. No other field interpolates. Prefer interpolation or `/connect`
over plaintext credentials.

Both `~/.config/cookie_agent` and exact-cwd `.cookie-agent`, including their
`agents` directories, use ordinary filesystem reads. Owner, mode, symlink, and
hard-link restrictions are not enforced. Wrong object types, oversize data,
unknown fields, and duplicates still fail closed.

Resolved secrets use `SecretString`. Owned source, interpolation, connect,
serialization, and CLI/TUI input buffers are best-effort zeroized immediately
after use and on drop. This is process hygiene; transport/kernel copies are not
marked erasable. Secret values are excluded from all safe outputs and hashes.
All Registry-1 setup values are non-secret and enter safe canonical config/
behavior fingerprints directly where behaviorally relevant. Every secret or
sensitive value must instead be an auth credential value and is excluded from
fingerprints, snapshots, and manifests; credential rotation does not change a
behavior fingerprint. A future non-auth sensitive setup requirement needs a
future schema/recipe version and independent mechanism.

## 5. Catalog candidate and quarantine boundary

The request URL is exactly `https://models.dev/catalog.json`; redirects,
cookies, auth, configurable headers, and queries are forbidden. A validated
cache ETag is the only conditional request value.

The client sends `Accept-Encoding: identity` and accepts only absent or
`identity` `Content-Encoding`. It checks `Content-Length` before reading and
rejects values above 16 MiB. Missing or smaller lengths do not authorize
buffering: bytes are streamed through a hard 16 MiB counter and overflow aborts
before allocation of the complete body, UTF-8 decoding, or JSON parsing.

After the byte cap, JSON limits are depth 32, at most 4096 providers, at most
65,536 provider models per provider, at most 65,536 root canonical models, at
most 1,000,000 total object/array entries, and at most 256 KiB per string before
narrower identity/URL/field limits.

The strict root is exactly:

```text
CatalogRoot { providers: Map<ProviderId, RawProviderRecord>,
              models: Map<CanonicalModelId, RawCanonicalModelRecord> }
```

Both maps are required, bounded, and nonempty. Unknown root fields are rejected.
`providers` is limited to 4096 entries and carries provider-scoped executable
metadata and provider model maps. Root `models` is limited to 65,536 entries and
carries canonical metadata/provenance only. It cannot select endpoint, npm,
protocol, provider setup schema, auth method, Oven adaptor, model inclusion,
defaults, or variants.

A **candidate failure** occurs only for invalid UTF-8/JSON, non-object root,
missing/non-object/empty top-level `providers` or `models`, unknown top-level
fields, duplicate root keys, exceeded whole-document/depth/container/string
limits, or another unrecoverable top-level map shape. Candidate failure advances
to the next source.

After a bounded provider map is recovered, records are isolated:

1. Parse each provider value independently from its bounded raw JSON value.
2. Provider-local duplicate keys, malformed required shape, invalid ID, or a
   normalized-ID collision quarantine that provider and all its models.
3. Within a valid provider, parse each model independently.
4. Model-local duplicate keys, malformed required shape, invalid ID, or a
   normalized-ID collision quarantine only that model and all colliding model
   peers; valid sibling models survive.
5. Structurally valid npm/API/shape metadata is classified through Family
   registry 1. An unknown provider family leaves the provider visible but
   unsupported; an unknown nested model family makes only that model unsupported.

Each provider-map key must equal its provider record `id`; each provider-model
key must equal that model record `id`; and each canonical-model key must equal
its canonical record `id`. Duplicate/ambiguous provider IDs quarantine the
provider; duplicate/ambiguous provider-model IDs quarantine those models;
duplicate/ambiguous canonical IDs quarantine only those canonical records.
Record schemas are strict: an unknown provider field quarantines that provider,
an unknown provider-model field quarantines that provider model, and an unknown
canonical-model field quarantines only that canonical record.

When a provider-model key exactly equals a valid root canonical-model key, the
runtime records an optional provenance cross-reference/digest. The provider
record remains the only executable metadata and may legitimately disagree on name,
limits, modalities, dates, or capabilities. No exact canonical match is required
for execution. A missing or quarantined canonical record removes provenance only
and never invents or invalidates an otherwise valid provider executable record.

Quarantined records never compile or connect. A recoverable uniquely valid
provider ID may produce an unsupported `/connect` row with
`invalid_catalog_provider_record`; records without such an ID contribute only
to safe global quarantine counts.

## 6. Catalog cache schema 2

Unix paths are fixed:

```text
~/.local/share/cookie_agent/catalog/models-dev-v2.json
~/.local/share/cookie_agent/catalog/models-dev-v2.meta.json
~/.local/share/cookie_agent/catalog/models-dev-v2.lock
```

All ancestor/application/catalog directories are current-user-owned `0700`.
Body, metadata, lock, and temporary files are current-user-owned `0600`, regular,
single-link, and opened no-follow. Every write locks/rereads, writes exclusive
same-directory temps, fsyncs, atomically renames, and parent-fsyncs.

```rust
pub struct CatalogCacheMeta {
    pub schema_version: CatalogCacheSchemaVersion, // exactly 2
    pub url: FixedModelsDevUrl,
    pub body_revision: CatalogRevision, // sha256:<lowercase SHA-256 digest of the exact selected body bytes>
    pub etag: Option<String>,
    pub byte_length: u64,
    pub validated_at: Rfc3339,
    pub last_checked_at: Rfc3339,
    pub selected_source: CatalogSource, // network | cache | bootstrap
    pub stale: bool,
    pub provider_quarantine_count: u32,
    pub model_quarantine_count: u32,
    pub quarantine_digest: Sha256Digest,
    pub last_error: Option<CatalogSafeErrorMeta>,
}
```

`CatalogSafeErrorMeta` contains stable code, bounded redacted message, and time.
A `200` validates and atomically writes body/meta before selection. A `304`
revalidates body/meta and records a successful check. Network/HTTP/validation
failure with cache selection sets `stale = true`, `selected_source = cache`, and
the exact safe error metadata. Bootstrap selection sets source `bootstrap`,
`stale = true`, and records why network and cache were unusable. Metadata is
updated even when selected body bytes do not change. If metadata cannot be
written safely, the in-memory snapshot reports `cache_metadata_write_failed`;
unsafe disk state is never trusted.

## 7. Provider visibility and store 3

`/connect` is a TUI projection of runtime snapshot providers. It includes:

- every current catalog provider with a recoverable unique ID, including
  unsupported/quarantined records;
- every authored or store-backed managed provider absent from the current
  catalog, marked `removed`;
- no custom provider.

Provider store 3 is fixed at
`~/.local/share/cookie_agent/providers/store-v3.json` with sibling lock. The
application/providers directories are `0700`; store/lock/temp files are `0600`,
current-user-owned, regular, single-link, descriptor-relative, no-follow, and
atomically replaced with lock/reread/fsync/rename/parent-fsync.

```rust
pub struct ProviderStoreSnapshot {
    pub schema_version: ProviderStoreSchemaVersion, // exactly 3
    pub generation: ProviderStoreGeneration,
    pub store_revision: ProviderStoreRevision,
    pub providers: BTreeMap<ProviderId, StoredManagedConnection>,
    pub connect_receipts: BTreeMap<ClientConnectId, DurableProviderReceipt>,
    pub disconnect_receipts: BTreeMap<ClientRequestId, DurableProviderReceipt>,
}

pub struct StoredManagedConnection {
    pub provider_id: ProviderId,
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub setup_fingerprint: Sha256Digest,
    pub auth_method: AuthMethodId,
    pub auth_values: BTreeMap<AuthFieldName, SecretString>,
    pub connection_generation: ProviderConnectionGeneration,
    pub policy: StoredProviderPolicyProjection,
    pub connected_at: Rfc3339,
}

pub struct StoredProviderPolicyProjection {
    pub catalog_revision: CatalogRevision,
    pub family_id: SafePolicyString,
    pub setup_recipe: ProviderSetupRecipeId,
    pub adapter_id: SafePolicyString,
    pub compiler_version: RecipeCompilerVersion,
    pub default_endpoint_identity: SafeEndpointIdentity,
    pub package_claim: SafePolicyString,
    pub source_record_digest: Sha256Digest,
    pub recipe_fingerprint: Sha256Digest,
    pub model_overrides: BTreeMap<ProviderModelId, StoredModelOverrideProjection>,
}

pub struct DurableConnectionDescriptor {
    pub provider_id: ProviderId,
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub setup_fingerprint: Sha256Digest,
    pub recipe_fingerprint: Sha256Digest,
    pub auth_method: AuthMethodId,
    pub credential_fields: Vec<AuthFieldName>,
    pub connection_generation: ProviderConnectionGeneration,
    pub connected_at: Rfc3339,
}
```

Azure Foundry Anthropic models classified as `@ai-sdk/anthropic` retain the
package's API-key wire behavior: `x-api-key`. Foundry accepts `x-api-key` for
API-key authentication; Entra access tokens instead use Authorization bearer.
Amazon Bedrock Mantle Responses models require a Bedrock API key as bearer
material (`AWS_BEARER_TOKEN_BEDROCK`) for the OpenAI Responses adapter. SigV4
credentials remain applicable to Bedrock Converse models but are not passed to
the OpenAI adapter.

Each connection retains provider ID, recipe-default endpoint identity, safe
setup/config scope fingerprint, normalized non-secret setup values, auth method
and secret credential values, generation, timestamps, family/adapter IDs and
package, exact source-record digest, and independent recipe fingerprint
validated at connect time. `source_record_digest` is immutable provenance over
the exact canonical validated catalog provider record selected at connect time.
`recipe_fingerprint` is the credential/execution compatibility identity over
Registry-1 revision/schema, compiler version, complete selected provider/
protocol/adapter/setup/auth/endpoint recipe, and versioned model-exception
semantics. Provider store 3 stores managed setup and credentials plus non-secret
policy, scope, source projection, generation, and idempotency-receipt metadata.
State revisions, connect payload digests, persisted receipts, and durable runtime
connection descriptors include `recipe_fingerprint`; every non-schema-3 store,
including `store-v2.json`, is rejected without migration. The retained safe source projection allows a
formerly current provider to remain a configured `removed` row. The store
contains no custom provider, catalog body, model inclusion pin, or endpoint
discovered from catalog.

A harmless catalog metadata refresh changes only `source_record_digest` for the
newly compiled runtime/manifest. If `recipe_fingerprint`, provider identity,
endpoint policy, normalized setup, and auth shape are unchanged, the existing
store secret remains eligible. API/environment/package metadata, adapter,
protocol, setup/auth/endpoint policy, compiler/registry, or model-exception drift
changes or quarantines recipe compatibility and blocks reuse. Source provenance
is never rewritten to make credentials match.

## 8. Connect and disconnect RPCs

```rust
pub struct ProviderConnectParams {
    pub provider_id: ProviderId,
    pub expected_catalog_revision: CatalogRevision,
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub auth_method: AuthMethodId,
    pub auth_values: BTreeMap<AuthFieldName, SecretString>,
    pub client_connect_id: ClientConnectId,
}

pub struct ProviderConnectResult {
    pub durable_connection: DurableConnectionDescriptor,
    pub effective_auth_source: EffectiveAuthSource,
    pub runtime: RuntimeSnapshotV1,
    pub replayed: bool,
}

pub struct ProviderDisconnectParams {
    pub provider_id: ProviderId,
    pub expected_runtime_revision: RuntimeRevision,
    pub expected_provider_state_revision: ProviderStateRevision,
    pub expected_connection_generation: Option<ProviderConnectionGeneration>,
    pub client_request_id: ClientRequestId,
}

pub struct ProviderDisconnectResult {
    pub durable_receipt: DurableProviderReceipt,
    pub provider_id: ProviderId,
    pub disconnected: bool, // always true on success
    pub effective_auth_state: EffectiveAuthState,
    pub runtime: RuntimeSnapshotResult,
    pub replayed: bool,
}
```

The expected revision must exactly equal
`sha256:<lowercase SHA-256 digest of the exact selected body bytes>`. For HTTP
`200` these are the exact response body bytes;
for `304`, cache, or bootstrap they are the exact selected cached/bundled bytes. Connect
accepts a current supported managed provider or a configured removed managed
provider whose retained safe source projection exactly matches registry 1. No
prior store record or authored provider definition is required for a current
catalog provider; connect creates/upserts the managed store record.

Before durable write, hold the serialized runtime-mutation lock and store lock;
reread store 3; validate catalog revision/idempotency and exact missing/extra
recipe-typed setup fields and auth credential fields; normalize safe setup
values; assign the final
store revision, connection generation, and receipt; and compile the complete
provider/model/agent/runtime candidate against exactly that proposed state.
Then one store transaction writes normalized setup, credentials, policy metadata,
connection record, and receipt atomically.
After commit, publishing the precompiled `Arc<RuntimeSnapshotV1>` is an
infallible atomic swap with no intervening allocation or validation. Response
serialization failure does not roll back; a retry with the same
`client_connect_id` returns the durable receipt and `replayed = true`.
Reusing that ID with different provider, setup-value, or auth-credential payload returns
`idempotency_conflict`. Secret-bearing equivalence data remains only inside the
private store receipt and is never hashed into or projected through safe state.

Authored auth remains effective after the stored update and is reported as
`authored_api_key` or `authored_override`; otherwise the source is
`provider_store` or `no_auth`.
`durable_connection` includes normalized setup values only where recipe policy
marks them safe, setup fingerprint, auth method/field names, and generation; it
never contains credential values.

The connect descriptor schema comes only from the code-owned provider recipe:
setup descriptors contain field ID, display/help text, required/defaulted status,
bounded type/validation, and safe projection policy; auth descriptors contain
auth method ID and credential field IDs/types. `/connect` renders setup in public
input controls and credentials in separate secret controls. This supports
absent-provider upsert for Vertex project/location/resource, Bedrock region,
Azure resource/deployment/API version, and API-key-only recipes whose setup is
empty or fully defaulted.

Disconnect idempotency is exact. Under the runtime/store locks, a receipt lookup
happens first. Reusing `client_request_id` with the same canonical complete
params returns the stored result with `replayed = true` regardless of later
runtime revisions. Reusing it with different params returns typed
`idempotency_conflict`. A new request must match both expected revisions and, when
a stored connection exists, its exact generation. For an absent record,
`expected_connection_generation` must be `None`; a supplied generation returns
`stale_provider_connection_generation`.

The receipt payload digest is SHA-256 over RFC-8785 JCS of all disconnect params;
the client request ID remains the receipt lookup key.

For a present stored managed provider, build the post-removal store state and
compile the complete candidate before mutation. For a syntactically valid absent
managed provider, disconnection is an idempotent success: compile the unchanged
effective provider candidate with the new durable receipt and return
`disconnected = true`. A configured custom provider still returns
`custom_provider_not_store_backed`.

One transaction atomically removes the stored setup and credentials with the
record when present (or records the absence no-op) and writes the receipt. After
commit, publication of the already
compiled snapshot is infallible and emits `runtime.changed` reason
`ProviderDisconnected`/`provider_disconnected`. First-time absent success also
publishes the receipt-advanced provider-state/runtime revisions; replay does not
publish again.

`effective_auth_state` is computed after store removal and is exactly
`authored_api_key`, `authored_override`, `no_auth`, or `unavailable`. Thus
authored credentials may keep the provider effective. `RuntimeSnapshotResult`
contains the complete coherent `RuntimeSnapshotV1`, not independently loaded
lists. Disconnect never edits config, deletes another provider, or exposes
secret values.

Required disconnect RPC tests cover present removal, already-absent success,
authored-auth remaining effective, same-ID/same-payload replay, conflicting
payload `idempotency_conflict`, stale expected revisions/generation, custom
provider rejection, one durable removal/no-op-plus-receipt transaction, one
`ProviderDisconnected` publication, and no publication on replay.

## 9. Runtime snapshot schema 3

```rust
pub struct RuntimeSnapshotV1 {
    pub snapshot_schema_version: RuntimeSnapshotSchemaVersion, // exactly 3
    pub recipe_registry_revision: RecipeRegistryRevision,
    pub catalog_revision: CatalogRevision,
    pub catalog_source: CatalogSource,
    pub catalog_state: CatalogRuntimeState,
    pub provider_state_revision: ProviderStateRevision,
    pub provider_store_generation: ProviderStoreGeneration,
    pub model_revision: ModelRevision,
    pub agent_revision: AgentRevision,
    pub runtime_revision: RuntimeRevision,
    pub providers: Vec<ProviderDescriptor>,
    pub models: Vec<AvailableModelDescriptor>,
    pub agents: Vec<AgentDescriptor>,
}

pub struct RuntimeSnapshotResult {
    pub snapshot: RuntimeSnapshotV1,
}
```

All revisions are opaque, content-derived, deterministic, and secret-free.
Each store-backed `ProviderDescriptor.durable_connection` carries the same safe
`recipe_fingerprint` used for compatibility projection; it does not expose
`source_record_digest` or any secret value.
`runtime.snapshot.get` is the only discovery RPC. Protocol 9 removes legacy
catalog/provider/model/agent list RPCs and racing refresh sequences. Connect,
disconnect, startup, catalog refresh, and config reload return a complete
snapshot.

Agent materialization preserves authored descriptors and unresolved fallbacks
unchanged. When no authored agent is root-runnable but at least one model is
available, the engine additionally materializes reserved built-in primary agent
`default`. Its sole fallback is the lexicographically first available model with
that model's default variant, or base when no named default exists. It is absent
as soon as any authored agent is root-runnable, and authored documents cannot use
the reserved `default` ID.

Every publication emits:

```rust
runtime.changed {
    previous_revision: Option<RuntimeRevision>,
    snapshot: RuntimeSnapshotV1,
    reasons: NonEmptySortedSet<RuntimeChangeReason>,
}
```

Reasons are exactly `startup`, `catalog_refreshed`, `catalog_fallback`,
`config_reloaded`, `provider_connected`, `provider_disconnected`,
`provider_store_changed`, `provider_store_reloaded`, and `agent_reloaded`.
The corresponding Rust variants include `ProviderConnected`,
`ProviderDisconnected`, `ProviderStoreChanged`, and `ProviderStoreReloaded`.
Local connect publishes `ProviderConnected`; local disconnect publishes
`ProviderDisconnected`. A successfully reconciled external generation change
publishes both store-change variants.

Before `runtime.snapshot.get` or any discovery RPC, session admission, or root
run admission, the process locks and rereads provider-store generation. A
mismatch blocks the operation, recompiles and publishes a coherent runtime with
both store-change reasons, then retries against that runtime. Reload failure
returns `provider_store_reload_failed`; accepted runs continue unchanged.

## 10. Project model-snapshot manifest and rehydration

The fixed project directory is:

```text
<exact-cwd>/.cookie-agent/model-snapshots/
  <64-lowercase-hex>.json
  model-snapshots-v1.lock
```

```rust
pub struct ModelSnapshotManifestV1 {
    pub schema_version: ModelSnapshotManifestSchemaVersion, // exactly 1
    pub revision: ModelSnapshotRevision, // sha256:<payload digest>
    pub payload: ModelSnapshotPayloadV1,
}

pub struct ModelSnapshotPayloadV1 {
    pub catalog_revision: CatalogRevision,
    pub recipe_registry_revision: RecipeRegistryRevision,
    pub provider_state_revision: ProviderStateRevision,
    pub model_revision: ModelRevision,
    pub blueprints: Vec<CompiledSafeModelBlueprint>,
}

pub struct CompiledSafeModelBlueprint {
    pub blueprint_fingerprint: Sha256Digest,
    pub selection: ModelSelection,
    pub source: FrozenProviderSource,
    pub config_override_fingerprint: Sha256Digest,
    pub setup_binding: FrozenSetupBinding,
    pub credential_binding: FrozenCredentialBinding,
    pub endpoint_identity: SafeEndpointIdentity,
    pub provider_recipe: ProviderRecipeId,
    pub protocol_recipe: ProtocolRecipeId,
    pub setup_recipe: ProviderSetupRecipeId,
    pub auth_method: AuthMethodId,
    pub compiler_version: RecipeCompilerVersion,
    pub descriptor: LanguageModelDescriptor,
    pub defaults: ResolvedRequestDefaults,
    pub options: FrozenProviderOptions,
    pub static_headers: BTreeMap<HeaderName, SafeStaticHeaderValue>, // custom only; {} for managed
    pub variants: Vec<FrozenVariantBlueprint>,
    pub behavior_fingerprint: Sha256Digest,
}
```

Canonical bytes are exactly RFC 8785 JSON Canonicalization Scheme (JCS) applied
to the self-contained `payload` object only. The envelope's `schema_version` and
`revision` are excluded, preventing digest self-reference. The filename is
`<lowercase hex SHA-256(JCS(payload))>.json`; `revision` is the same digest as
`sha256:<lowercase hex>`. Envelope and payload deny unknown/duplicate keys.

Payload JSON is UTF-8/I-JSON. Maps follow RFC 8785 property ordering. Before JCS,
semantic collections are projected deterministically: model blueprints sorted by
`ModelKey`, variants by `VariantId`, setup fields by `SetupFieldId`, credential
binding names by `AuthFieldName`, and unordered sets by their strict ID/string
order. Arrays whose order is semantic—fallback suffixes, ordered bindings,
permission-like sequences, and provider option sequences—are preserved exactly.

JSON numbers are integers only, must fit both their domain bounds and the I-JSON
safe range `[-(2^53-1), 2^53-1]`; floating JSON numbers are forbidden. Finite decimal request values are
encoded as their domain type's normalized decimal string. Strings are valid
UTF-8 scalar sequences and are not NFC/NFKC normalized; exact code points are
preserved and escaped/ordered by RFC 8785. IDs apply their stricter grammar
before canonicalization.

Credential binding
contains only auth source/method and credential field names. Setup binding is
separate and contains setup recipe, field IDs, and normalized non-secret values
directly. Custom static header names/values are included in `static_headers` as
safe behavior metadata. Auth credential values, generated auth-owned header
values, environment values, raw catalog records, live handles, and
provider-native private payloads are forbidden.

The directory is current-user-owned `0700`; manifest, exact lock
`model-snapshots-v1.lock`, and temp files are
current-user-owned `0600`, regular, single-link, descriptor-relative/no-follow,
bounded to 4 MiB each and 4096 direct matching files, and atomically written by
lock/reread, exclusive sibling temp, fsync, rename, and parent fsync. A manifest
is durable before `SessionCreated` or `RunStarted` references it.

Startup scans direct `<64-lowercase-hex>.json` files in sorted byte order and validates
strict schema, RFC-8785 reserialization, filename/revision/payload digest equality, bounded unique
blueprints, and safe identities. Unsafe objects or malformed matching files fail
project open with `invalid_model_snapshot_manifest` or
`model_snapshot_digest_mismatch`; no partial acceptance or filename fallback is
allowed. Every protocol-9 session/journal reference must resolve. Referenced
manifests are never garbage-collected; Family registry 1 performs no automatic manifest
GC.

```rust
pub enum FrozenProviderSource {
    Managed {
        provider_recipe: ProviderRecipeId,
        source_record_digest: Sha256Digest,
        recipe_fingerprint: Sha256Digest,
        package_claim: String,
    },
    Custom {
        safe_definition_fingerprint: Sha256Digest,
    },
}

pub struct FrozenModelBinding {
    pub manifest_revision: ModelSnapshotRevision,
    pub blueprint_fingerprint: Sha256Digest,
    pub selection: ModelSelection,
    pub source: FrozenProviderSource,
    pub config_override_fingerprint: Sha256Digest,
    pub credential_source: FrozenCredentialSource,
    pub setup_binding: FrozenSetupBinding,
    pub endpoint_identity: SafeEndpointIdentity,
    pub protocol_recipe: ProtocolRecipeId,
    pub setup_recipe: ProviderSetupRecipeId,
    pub auth_method: AuthMethodId,
    pub compiler_version: RecipeCompilerVersion,
    pub descriptor: LanguageModelDescriptor,
    pub defaults: ResolvedRequestDefaults,
    pub options: FrozenProviderOptions,
    pub behavior_fingerprint: Sha256Digest,
}
```

`config_override_fingerprint` is domain-separated SHA-256 over the canonical
effective authored override shape: provider ID and source kind; authored
endpoint identity when present; setup recipe/field IDs and normalized non-secret
setup values, auth method/credential names
but no credential values; and the complete normalized managed `model_overrides` map. No authored
provider uses the canonical `no-authored-override` marker. The custom
`safe_definition_fingerprint` covers provider ID/source, normalized endpoint,
adaptor, setup recipe/field IDs and normalized non-secret setup values, auth
method, safe auth parameters/owned header names, credential field names, static
headers sorted by canonical lowercase name with each exact safe value, and
complete normalized model definitions, defaults, options, and variants; it
excludes every auth credential value. Credential-only rotation therefore preserves safe
fingerprints while setup binding, source kind, endpoint, auth shape, or behavior
changes do not.

For managed sources, `source_record_digest` and `recipe_fingerprint` are
independent required fields. The former pins the exact catalog provider record
used to compile that blueprint. The latter pins code-owned execution and
credential compatibility. Blueprint, manifest payload, frozen binding, runtime
source projection, persisted event/session identity, and delegation journal
binding descriptions preserve both values. Events and journals carry them only
through the referenced exact `FrozenModelBinding`; neither value contains a
secret.

`FrozenCredentialSource` is exactly `authored_api_key`, `authored_override`,
`provider_store`, or `no_auth`. It never changes during rehydration.

- Authored sources require the current same-ID provider definition, same source
  kind, same safe config-override fingerprint, and the same authored auth shape.
  Missing/changed authored config fails; it never falls to store.
- Store source requires managed source, exact current `recipe_fingerprint`,
  recipe endpoint policy, setup recipe/fingerprint, exact auth method/shape, and
  retained store scope/generation-compatible credential. It never substitutes
  config auth. The durable connection's historical `source_record_digest` need
  not equal a harmless newly refreshed blueprint's source digest.
- Managed source reconstructs from the persisted safe source projection only if
  Family registry 1 still reproduces the exact recipe fingerprint, frozen
  `package_claim`,
  protocol, auth, and compiler. The binding must exactly match its referenced
  blueprint's own source-record digest, but source provenance is not credential
  scope. Current catalog presence is not required.
- Custom source requires a current custom definition with the exact safe
  definition fingerprint. Custom never uses store.
- Recompiled behavior must equal the frozen fingerprint.

A store-backed managed blueprint freezes normalized non-secret setup values and
their canonical setup fingerprint, plus auth method and credential field names/
shape, but never credential values. Config-backed managed and custom blueprint
rules are unchanged; custom remains entirely config-only.

Custom provider compilation, fingerprints, and rehydration are entirely
config-only and never open or depend on provider store 3.

Typed failures are `snapshot_config_mismatch`,
`snapshot_credentials_unavailable`, `unsupported_snapshot_recipe`, and
`snapshot_rehydration_mismatch`. History remains readable; no key/name fallback
or model substitution occurs.

New root runs resolve only from the current coherent runtime and durably
write/reference its current manifest blueprint. A harmless catalog refresh
therefore freezes a new source-record digest while reusing credentials through
the unchanged recipe fingerprint. Previously accepted runs keep their original
manifest/source provenance and may rehydrate the same store secret through the
same recipe/auth/setup identity. Delegated sessions use the invoking parent's
accepted manifest revision and exact frozen suffix even when the current runtime
changed. Once a run is accepted, later catalog, config, provider-store,
manifest-directory, or runtime changes do not reinterpret it.

## 11. Normative startup order

1. **Schema 7 and agents:** securely load recipe registry 1, atomic config 7,
   and agent documents.
2. **Catalog:** securely open cache 2, perform bounded network acquisition, then
   resolve network, validated cache, or bootstrap and quarantine records.
3. **Provider store:** lock/load provider store 3 and its generation.
4. **Effective providers:** resolve authored/stored managed and authored custom
   providers.
5. **Coherent runtime:** compile all providers/models/authored agents, add
   built-in `default` iff models are available and no authored agent is
   root-runnable, and build runtime snapshot 1; an empty model set is valid.
6. **Project manifests:** scan/validate model-snapshot manifests 1 and rehydrate
   referenced safe blueprints.
7. **Engine:** open/reconcile protocol-9 / event-schema-13 sessions/events/delegation against the
   manifest index.
8. **Service:** publish runtime, then open server/TUI and emit startup
   notification.

Nothing serves before step 8. Provider-store generation reconciliation runs
again before discovery, session admission, and root-run admission.

## 12. TUI state machine

The TUI consumes one runtime snapshot and notifications. It implements exactly:

| State | Required behavior |
|---|---|
| `loading` | No snapshot; selectors and ordinary submission disabled. |
| `empty` | Valid coherent snapshot with zero available models after authored providers and global store records are applied. Message model/draft display is exactly `type /connect to continue`; no Model/Variant hit target; ordinary text/run blocked with the same guidance; `/connect` accepted. |
| `ready` | Live or 304-validated catalog; normal actions. |
| `stale` | Cache selected after error; usable rows plus durable global error/time explanation. |
| `bootstrap` | Bootstrap selected; durable global fallback explanation. |
| `unsupported` | Typed row reason; Enter opens details only, never connect. |
| `disconnected` | Supported managed provider lacks complete effective setup and/or auth; Enter opens separate public setup and secret credential forms from recipe descriptors. |
| `connected-reconnect` | Effective stored state exists; Enter opens reconnect/update with stored public setup prefilled and credential controls secret/blank; disconnect removes both stored setup and credentials. |
| `removed` | Configured managed provider or retained session model absent from current catalog; details state available action. |
| `error-retry` | No usable snapshot or operation failed; explicit retry action. |

Global stale/bootstrap/error explanations are durable application state across
navigation, not transient toasts. A newer snapshot explicitly clears or
replaces them. Unsupported Enter is details-only. Active frozen runs are never
changed by catalog, connect, disconnect, or picker actions.

The `/connect` view always renders the exact copy
`Stored setup, connections, and credentials are per-user and shared across workspaces.`
Normalized setup is non-secret and projected only where its recipe descriptor
marks it safe; credential values are always secret/redacted.

`loading` means no runtime snapshot exists. `empty` means a valid zero-model
snapshot exists and cannot form a root draft. `error-retry` means startup or an explicit
operation has no usable result. These states must never reuse one another's
display or actions. Empty state cannot project a persisted, removed, stale, or
placeholder model as the draft.

After `provider.connect` and its coherent runtime publication produce at least
one available model, the TUI initializes the normal structured draft from an
authored root-runnable agent or built-in `default`, removes
`type /connect to continue`, and restores Model and Variant hit regions. No model
is fabricated and authored fallback chains are never rewritten.

Built-in `default` uses source `built_in`, mode `primary`, the standard coding
tools, no delegation targets, and normal ordered agent permission rules:
workspace read allow; outside read, write/edit, bash, and delegate ask; outside
filesystem access is controlled by absolute read/write patterns; secret-file
reads deny for `.env` variants, `store-v3.json`,
`token-v1`, `id_*`, `.netrc`, and
`application_default_credentials.json`, retaining the exact `.env.example`
allow exceptions.

### 12.1 Required actual-buffer and RPC tests

Tests use the real input/message buffer, renderer, hit map, command dispatcher,
and protocol client:

1. Omitted `providers` and explicit empty `[providers]` both parse as schema 7.
2. With empty provider store and no effective custom provider, startup publishes
   snapshot 1 with `models = []` and no root-runnable agents, opens the TUI, and
   never emits `runtime providers must be nonempty`.
3. Empty/omitted TOML plus a valid global stored managed connection materializes
   its models and does not show empty guidance; built-in `default` supplies the
   root draft when no authored agent is runnable.
4. The actual Message model/draft buffer equals the UTF-8 bytes
   `type /connect to continue`; rendered hit maps contain no Model or Variant
   target for it.
5. In the zero-model state, submitting ordinary text does not invoke session-create or run-start RPC and
   returns/displays exactly `type /connect to continue`. Direct run RPC in the
   same state returns typed `no_runnable_model` with that safe guidance.
6. Submitting `/connect` through the actual input buffer remains accepted and
   opens all-provider discovery from the coherent runtime snapshot.
7. A successful connect plus `runtime.changed` carrying any available model
   replaces the guidance with normal structured agent/model selection, using
   synthetic `default` iff no authored agent is root-runnable.
8. Cross-process store-generation change before discovery/session/root admission
   publishes `provider_store_changed` plus `provider_store_reloaded`, or blocks
   with `provider_store_reload_failed`.
9. Connect forms come only from separate recipe setup descriptors and auth
   credential descriptors, render setup as
   public inputs and credentials as secret inputs, and apply explicit defaults.
10. Missing/extra setup or auth fields fail before any durable write or publish.
11. Reconnect atomically replaces stored setup and credentials; another workspace
    observes both after provider-store generation reconciliation.
12. Disconnect removes both stored setup and credentials, while custom providers
    remain absent from all store-backed actions.
13. Engine projection tests cover zero authored agents, authored agents that are
    all unrunnable, authored runnable suppression, lexicographically first model
    plus default variant selection, built-in source/tools/permissions, reserved
    ID rejection, and successful session admission through `default`.

## 13. Current-only protocol and persistence

Protocol handshake, events, session JSONL, metadata, and delegation journal are
exactly version 9. Every run admission records runtime snapshot schema 3 revision
and complete frozen safe bindings. Version 7 input and config schema 6 are
rejected; no migration tool exists. Provider store 1 and unversioned cache/store
files are rejected rather than renamed or imported. Project model-snapshot
manifests are exactly schema 1; unknown versions are rejected.

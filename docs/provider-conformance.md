# Oven model, provider, catalog, and variant conformance

This document defines the reviewed provider/adaptor boundary used by
configuration schema 6. [ARCHITECTURE.md](../ARCHITECTURE.md) is authoritative;
the complete strict provider and variant contract is in
[agent-model-variant-redesign.md](agent-model-variant-redesign.md).

`crates/models` constructs published Oven adapters only from strict
provider-centric definitions and reviewed pinned models.dev recipes. It performs
no model-name inference, network discovery, build-time download, runtime catalog
refresh, or provider probe. Runnable identities are direct
`provider/model-id` keys; model aliases do not exist.

## Published Oven pins and adaptor IDs

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

The accepted explicit adaptor IDs are:

```text
anthropic
openai-chat
openai-responses
openai-compatible
google-gemini
google-vertex-gemini
aws-bedrock-converse
azure-openai-chat
azure-openai-responses
cohere-v2-chat
open-responses
```

MiniMax and Claude Platform on AWS are not exposed. Static Anthropic
construction uses `native_context_discriminator`; Vertex derives
`google_vertex_native_context_scope`; official OpenAI and Azure Responses
require explicit compaction settings whenever native compaction is declared.
No former discriminator/scope field names are accepted.

The adaptor ID selects only a reviewed constructor and wire protocol. It is
never inferred from provider or model text. The arbitrary provider model ID is
sent to that already-selected adaptor and cannot alter construction behavior.

`ScriptedModel` remains the deterministic FIFO test implementation, including
captured requests, streaming delays, cancellation points, native compaction,
exhaustion, and planned errors.

## Pinned models.dev snapshot

The exact upstream `snapshotPayload` from `anomalyco/models.dev` commit
`c3057690bbb8bd41cafdefadcd2a7b958e2a4642` is vendored at
`crates/models/catalog/models-dev.json`:

- size: 3,567,054 bytes;
- SHA-256 and required schema-6 `catalog_revision`:
  `sha256:d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a`;
- no trailing newline;
- MIT attribution: copyright 2025 models.dev.

The authoritative upstream inputs are provider/model TOML, schema, and
generator. Repository-root `models.json` is not the vendored artifact.
`scripts/update_models_dev.py --check --source ...` is offline and requires an
already-prepared pinned checkout. Network cloning/dependency installation is
isolated behind explicit opt-in `--update`. Cargo builds, tests, and runtime
never invoke the updater or access the network.

The parser retains and bounds the complete upstream document, then emits a
stable secret-free projection in provider/model order. Every schema-6
`ModelsDevProvider.catalog_revision` must exactly match the compiled snapshot;
stale or unknown revisions fail before provider construction. Canonical model
IDs are emitted only for exact metadata keys. Wrapper models are not guessed
into families.

Known, constructible, and configured are distinct states. Catalog presence is
not support. A model becomes runnable only when it is explicitly listed and
enabled in one provider's `models` map, has a reviewed recipe, and the complete
provider candidate validates.

`catalog.provider.list` returns only configured models.dev providers whose auth
is `credential_store`, with the pinned catalog revision and safe provider
metadata/credential field names; this is the connectable provider picker.
`catalog.model.list` returns safe pinned catalog model metadata.
`model.list` returns the independently revisioned configured runnable snapshot;
it is not a catalog endpoint. `agent.list` is refreshed only after the model
snapshot publishes, so agent runnability is evaluated against one coherent
model revision.

## Provider construction

A strict schema-6 provider is one of:

- `source = "models_dev"`: exact catalog revision, reviewed recipe, supported
  auth shape, optional recipe-permitted endpoint/adaptor override, strict
  headers, and a nonempty explicit model map;
- `source = "explicit"`: required endpoint/adaptor/auth and a nonempty
  explicit model map with complete honest capabilities.

For both forms, `source`, `auth`, and `models` are required and `models` must be
nonempty. Models.dev additionally requires `catalog_revision`; `endpoint` and
`adaptor` are optional and default to the reviewed source recipe. Explicit
providers require `endpoint` and `adaptor`. `headers` is optional and defaults
to `{}`.

Unknown fields fail at every level. Provider definitions are atomic layer
replacements: a workspace provider with the same `ProviderId` replaces the
entire user provider before validation. Models and variants are not merged
across layers.

Models.dev providers normally use the recipe endpoint and adaptor. An endpoint
override is accepted only when the recipe marks it overridable. An adaptor
override must be one of that recipe's reviewed alternatives. Explicit
providers require HTTPS except adaptor-declared loopback HTTP. Endpoint
userinfo, fragments, and credential-bearing query parameters are rejected.

Authentication is a strict tagged value: `none`, `credential_store`, `bearer`,
`api_key`, `basic`, `aws_sdk`, `google_adc`, or adaptor-schema-validated
`fields`. `credential_store` is models.dev-only and names exactly the recipe
fields. Missing stored values leave the provider unavailable until connect;
construction requires all of them.
Unknown, missing, or extra semantic fields and unsupported auth/adaptor pairs
fail. Static headers reject invalid values, case-insensitive duplicates,
transport-owned headers, and auth headers owned by the selected auth form.
The Phase-1 compiler does not provide ambient SDK credential discovery, so
`basic`, `aws_sdk`, and `google_adc` are currently rejected by every reviewed
adaptor. Vertex fields are exactly `access_token`; Bedrock fields are exactly
`access_key_id` plus `secret_access_key` and optional `session_token`.
Compatible API-key auth emits the configured header (default `x-api-key`).

Only listed enabled models are constructed. Each included model has:

- an exact `ModelKey` and display name;
- explicit/restricted capabilities and limits;
- normalized ordinary authorable `RequestDefaults` plus internal compiled reasoning;
- strict typed adaptor/provider options;
- base behavior, generated and explicit variants, and optional default variant;
- descriptor, selection, behavior, and provider-snapshot fingerprints.

For every model, `enabled` is optional and defaults true; `defaults`, `options`,
and `variants` are optional tables defaulting to `{}`; `default_variant` is an
optional field. A models.dev display name defaults to the pinned source value;
an explicit display name is required.

Models.dev capabilities are derived completely from the pinned record plus
reviewed recipe/compiler; a configured capabilities table is an unknown-field
error. Explicit model capabilities are required and every field must be
present, including false booleans and an empty media map. Parallel tool calls
may be true only with tool calling; seed support is a required explicit
capability; each declared non-text input modality requires exactly one matching
bounded MIME/media entry, while text-only input requires an empty media map.
Unsupported normalized defaults, unknown options, duplicated semantics between
defaults/options, and dishonest capability declarations fail construction.

Authorable ordinary request defaults contain only temperature, top-p, maximum
output tokens, stop strings, seed, and tool choice; their omission values are
None/empty. Reasoning is authorable only through
`VariantDirective.reasoning`. Provider options reject all alternate
reasoning/effort/thinking/budget fields. The compiler alone creates internal
resolved request defaults containing compiled reasoning behavior.

The candidate constructs all adapters and compiles every base/variant before
publication. Publication atomically replaces the complete provider/model
snapshot. Failure leaves the old snapshot intact; no partially refreshed
provider is observable.
Base and every enabled named variant own a separately constructed exact Oven
model handle. Freezing records that selection's descriptor, defaults, options,
behavior fingerprint, and selection fingerprint; rebinding resolves the same
exact executable rather than applying variant defaults to a shared base model.

## Reviewed models.dev recipe allowlist

Initial generated construction support is limited to:

- first-party Anthropic Messages;
- exact hand-reviewed OpenAI model IDs mapped to Responses or Chat;
- first-party Google `generateContent`;
- first-party Cohere v2 Chat;
- OpenRouter's reviewed HTTPS compatible Chat endpoint;
- effective `@ai-sdk/openai-compatible` models whose endpoint is HTTPS and
  whose provider declares exactly one credential field.

Vertex, Azure, Bedrock, standardized Open Responses, MiniMax, Claude Platform
on AWS, ambiguous package reuse, insecure endpoints, deprecated offerings, and
records requiring unreviewed provider body/header injection remain unsupported
models.dev recipes. They may be used only through an honestly declared
supported explicit provider where the selected adaptor can encode them.

Recipe-generated descriptors are conservative. They may expose only pinned
catalog features confirmed by the reviewed adaptor compiler. Pinned attachment
modalities are mapped through reviewed bounded MIME/count/byte baselines and
compiled into Oven media descriptors; they are never inferred from a model
name. Recipes never infer parallel tools, tool-input deltas, top-p, reasoning,
replay, or native compaction from a model name. Cancellation is local unless
the adaptor proves a stronger behavior. Default maximum output is
`min(16_384, catalog_output_limit)` unless a reviewed recipe defines another
safe value.

Option compilation is lossless: Anthropic accepts only its fixed
`2023-06-01` API version and emits `beta` values in the Anthropic request
namespace; Responses accepts only `store = false`; compatible `api_path` must
end in `/chat/completions` and changes the effective Oven endpoint; Gemini API
versions replace the endpoint version segment; Cohere accepts only `v2`.
Vertex project/location, Bedrock region, Azure deployment/API version, OpenAI
organization/project, and Open Responses protocol mode compile into their
corresponding concrete Oven settings. Other option/adaptor combinations fail.

## Reasoning options and variants

The catalog compiler recognizes exactly three reasoning-option forms:
`effort`, `toggle`, and `budget_tokens`.

- Effort accepts only `none`, `minimal`, `low`, `medium`, `high`, `xhigh`,
  `max`, `default`, and `null`. Non-null values produce same-ID variants;
  the actual null token produces `off` and a string `"null"` is invalid.
- Toggle produces `off` and `on`; each must compile to an explicit honest wire
  behavior.
- Pinned `budget_tokens` accepts only optional `min` and `max`. `min = -1`
  generates `budget-auto`; finite `min` generates `budget-min`; present finite
  `max` generates `budget-max`. All other fields are invalid. Missing bounds
  produce no variants, and no other budget
  ID is generated.

Any reviewed recipe metadata that defines base request behavior or a provider
model source default is separate from `reasoning_options.budget_tokens` and
does not create another generated budget ID.

Multiple catalog options form a deterministic union, not combinations.
Generated collisions use `effort` over `toggle` over `budget_tokens` only when
the normalized compiled behavior is identical; otherwise construction fails.
An explicit config directive has highest precedence: `add` requires an absent
ID, `replace` requires an existing ID, and `disable` requires and removes an
existing ID. Base cannot be targeted. The model-level
`default_variant` is precisely `Option<ConfiguredModelDefault>`: omission/None
retains the provider model source default, explicit `base`/Some(Base) selects
exact base, and every other string/Some(Named) selects an enabled final variant.
The models.dev source default is its explicitly pinned source/recipe default or
base; an explicit provider's source default is base. Disabling a selected source
default requires an explicit replacement. Resolution produces exact
`ModelSelection` before freezing.

Every generated/explicit behavior is compiled by the selected adaptor into
internal resolved request defaults and typed provider options. If effort, toggle,
budget, or a combined explicit variant cannot be represented without loss, the
included model/provider fails atomically. The compiler never approximates,
renames, silently drops, or advertises an unencodable variant.

Variant identity and behavior fingerprints participate in frozen bindings,
fallback, replay/native-context scope, persistence, protocol attribution, and
diagnostics. Base and each variant are distinguishable even when request values
happen to be equal.

## Credential persistence and redaction

`provider.connect` values are not configuration fields. On Unix they are stored
at `~/.local/share/cookie_agent/credentials/store-v1.json` with a sibling lock:

- directories are current-user-owned mode 0700;
- store, lock, and temporary files are current-user-owned regular mode 0600;
- traversal is anchored at a current-user-owned, non-group/world-writable home
  or data directory and uses dirfd-relative no-follow opens for every component;
- symlinks, ancestor replacement, hard links, weak modes, and unexpected object
  types are rejected;
- every transaction locks and rereads under the lock;
- writes use same-directory exclusive temp, file fsync, atomic rename, and
  parent-directory fsync;
- malformed, oversized, wrongly owned, or weak-permission state fails closed.

The store contains sorted credentials, connection timestamp, generation UUID,
catalog revision, a random local HMAC key, and durable idempotency receipts.
Receipts contain only HMAC-SHA256 over the canonical secret-bearing request.
Persistent connect remains disabled on platforms without equivalent ownership,
ACL, no-follow, locking, and atomic-replacement guarantees.

Credential values travel only in the inbound connect request and secret
containers. They are excluded from events, results, typed error data, debug
output, generic request/result logging, schema examples, fingerprints, model
snapshots, session JSONL/meta, delegation journals, artifacts, and TypeScript
result projections. Auth shape and header names may enter safe fingerprints;
secret/header values may not.

CLI/TUI owned secret buffers use best-effort zeroization when ownership allows.
The CLI keeps structured connect parameters and pre-dispatch JSON serialization
in drop guards/`Zeroizing` buffers and wipes owned source buffers immediately
after dispatch without cloning credentials. Once a frame is moved into the
WebSocket/TLS/socket/kernel transport, those transport-owned copies cannot be
honestly guaranteed to be wiped. This is process hygiene, not a locked-memory
or forensic-erasure guarantee.

Connect rejects a truly unconfigured provider as `unknown_provider`; an
explicit provider or configured provider without `credential_store` is
`unsupported_provider`. Missing required recipe fields are
`missing_credential` with the exact bounded, sorted
`missing_credential_fields`; extra or otherwise invalid fields remain
`invalid_credential`.

Connect reporting remains phase-specific: connect acceptance, model-list
refresh, agent-list refresh, and optional initial session creation are separate
outcomes. A configuration with no `runnable_as_root = true` agent is a valid
setup state and does not create a session. Every primary requires a nonempty
chain; subagent/all may be empty for delegated inheritance, but any empty-chain
agent is not root-runnable and an all agent is root-selectable only with its own
nonempty chain and at least one available selection. Connecting credentials may make a previously
unavailable enabled nonempty-chain primary/all agent root-runnable; it never
enables an agent whose document says `enabled: false`.

The checked workspace fixture uses three explicit provider declarations and
one environment interpolation variable, `COOKIE_TEST_API_KEY`. Its runnable
keys are `anthropic/kimi-for-coding`, `openai/gpt-5.6-luna`, and
`quantumcookie.gateway/deepseek-v4-flash`; Anthropic and Responses select the
named `high` variant, while compatible chat selects base. These are direct
keys and variants, not aliases or profiles.

## Immutable snapshots and restart resolution

`ModelSetManager` publishes immutable provider/model snapshots through an
atomic swap and serializes refresh/connect. During one daemon lifetime, exact
older fingerprints are retained so already-frozen sessions and runs can resolve
without rebinding to the newly published provider/model behavior. New agent
listing and session creation use only the current atomically loaded snapshot.

The retained map is process-local and is not persisted. Restart builds only the
current snapshot from validated schema-6 config, the exact pinned catalog, and
latest credentials. Persisted frozen bindings remain readable audit data but
execution fails with `obsolete_model_fingerprint` when the exact fingerprint is
absent. Resolution never falls back by provider/model key or variant name.

Secret-only credential rotation can preserve safe behavior fingerprints while
new adapter handles receive current credentials. Revisions include all safe
behavior-affecting endpoint identity, auth shape, header names, capabilities,
defaults, options, recipe data, native-context scope inputs, compaction settings,
and variant behavior.

## Workspace loading authority

Local `cookie` and `cookie daemon` load and validate built-in defaults, user
schema-6 TOML, exact-cwd workspace schema-6 TOML, and user/workspace Markdown
agents once before composition. There is no persisted workspace acceptance and
no upward search. `attach` and `connect` do not inspect cwd configuration.

Config and agent paths use descriptor-relative no-follow loading. Approved
`${env:NAME}` interpolation is restricted to provider endpoints, auth secret
fields, and header values. Environment variables never become arbitrary config
keys. Agent prompts, IDs, capabilities, defaults, options, and variants cannot
interpolate.

The resulting operation authority is still the frozen agent policy, exact
approval/tree grant when required, and prepared descriptor-bound capability.
Provider configuration cannot itself execute a tool operation.

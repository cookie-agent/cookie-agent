# Model Configuration Simplification

Status: Draft. Implementation is not authorized.

This document is the working specification for the model-configuration changes
discussed with the user. Update it as decisions are made. Examples describe the
proposed schema, not configuration accepted by the currently installed binary.

## Goals

- Make variant entries create or selectively override variants without an
  explicit operation.
- Distinguish generation settings from adaptor-specific settings by name.
- Select Chat Completions or Responses explicitly through adaptor settings.
- Locate pricing alongside its model definition or override.
- Keep authored configuration strict, with no compatibility aliases for removed
  fields.

## Agreed Changes

### Generation and adaptor settings

Rename authored `defaults` to `generation_options` and authored `options` to
`adaptor_options` wherever they describe model or variant settings, including
managed-model overrides.

`generation_options` contains the existing generation fields:

- `temperature`
- `top_p`
- `max_output_tokens`
- `stop`
- `seed`
- `tool_choice`

Their meaning and capability validation remain unchanged. `adaptor_options`
contains typed, adaptor-specific settings; it is not an arbitrary JSON body.
Unrelated SDK or protocol fields called `options` are not renamed by implication.

`reasoning` remains separate. `default_variant` also retains its name: it selects
the variant used when no explicit variant is selected, rather than supplying
generation parameters.

### Implicit variant creation and sparse overrides

Remove the required `operation` field and its `add`, `replace`, and `disable`
directives from authored variants.

- If a configured variant ID does not exist, create it.
- If it exists, override only supplied fields and preserve unspecified fields.
- Apply this behavior to both custom and managed providers.
- Managed providers begin with their generated catalog variants; custom
  providers begin without catalog variants.

This replaces whole-variant replacement with sparse updates. Disabling variants
remains an open decision below.

### Base-model inheritance

Variants inherit the model's `generation_options` and `adaptor_options`.
Resolve these settings in this order, with later supplied fields taking precedence:

1. Effective base-model settings.
2. Existing catalog variant settings, when present.
3. User-authored variant overrides.

Custom variants have no catalog layer. A variant that only sets reasoning uses
the model's sampling parameters and request endpoint without repeating them.
For managed models, the effective base includes model-level user overrides, but
an explicitly supplied catalog variant field still takes precedence over that
base; a user variant override has the final say.

Merge nested option fields individually. Omitted fields inherit; explicit false
and empty lists override inherited values. Lists replace rather than append.
Parser or compiler defaults must not become synthetic variant overrides that
mask base settings. Apply remaining fallback defaults after composition, and
validate the resulting settings for the selected adaptor and endpoint.

This intentionally changes today's variant behavior. Inheritance is computed
from the layers, not copied into authored variants, so a base-model edit affects
all variants that do not override that field. It does not introduce cross-file
provider merging or change the existing header-layer contract.

### API selection

Remove `api_path`. It is accepted and stored today but was found not to reach the
executable OpenAI-compatible request configuration. Do not implement the unused
path-override behavior as part of this work.

Replace existing top-level `shape` with:

```toml
adaptor_options = { request_endpoint = "completions" }
```

Accepted values:

| Value | API |
|---|---|
| `completions` | Chat Completions, conventionally `/chat/completions` |
| `responses` | Responses, conventionally `/responses` |

`completions` does not mean the legacy `/completions` API. Selection must choose
the corresponding request encoder, response/stream decoder, and applicable
capability validation, not merely substitute a URL path.

The setting belongs under `adaptor_options` for model definitions, managed-model
overrides, and variants. Unsupported adaptor/endpoint combinations must fail
configuration validation. A complete support matrix must be verified before
implementation; generic compatible Responses support must not be inferred from
official OpenAI Responses support.

### Default tool and structured-output capabilities

Make these authored capability fields optional, with a fallback value of true:

- `tool_calling`
- `parallel_tool_calls`
- `structured_output`

Apply these defaults only when no value is supplied by configuration or inherited
catalog metadata. An explicit false remains false. Managed overrides continue
to inherit catalog declarations rather than replacing them with schema defaults.
Distinguish absent catalog metadata from an explicit false: a hardcoded
projection fallback is not a catalog declaration. In particular, the current
managed projection's unconditional `parallel_tool_calls = false` must not defeat
the new default, and absent `structured_output` metadata now falls back to true.

These defaults are optimistic declarations, not endpoint feature detection.
Custom endpoint users must explicitly set unsupported capabilities to false.
Keep adaptor capability validation; an unsupported effective declaration must
produce an actionable configuration error rather than silently changing its
value. Documentation must state this trade-off and show how to opt out.

Preserve the invariant that parallel tool calls require tool calling. If
`tool_calling = false` while `parallel_tool_calls` resolves to true, report the
conflict and instruct the user to set `parallel_tool_calls = false` too. Do not
silently override either value.

This change does not itself force parallel-tool request parameters onto every
adaptor, change tool execution scheduling, or request JSON output for ordinary
agent responses. Existing uses of these capability declarations remain subject
to their adaptor-specific behavior.

### Request model identity

Add optional `model_id` to model definitions, managed-model overrides, and variant
entries. It sets the model identifier sent in provider requests, independently
of the local configuration key used to select the model.

Resolve the wire identifier in this order, with later supplied values winning:

1. Provider-supplied model identifier (for custom models, the configuration model
   key is the fallback identifier).
2. Model-level configured `model_id`.
3. Variant-level configured `model_id`.

Omission inherits; reject empty or invalid identifiers using the applicable
model-ID validation. Preserve provider-specific model-ID syntax rather than
imposing local alias syntax on the wire value.

```toml
[providers."custom.example".models."coding"]
model_id = "backend-model-v1"

[providers."custom.example".models."coding".variants.fast]
model_id = "backend-model-fast-v1"
```

These fragments illustrate identifiers only; other required custom-model fields
are omitted. Users still select `custom.example/coding` and its `fast` variant.
Changing the wire identifier does not rename the catalog/configuration entry,
change its pricing lookup identity, or infer new capabilities. A managed entry
must still refer to an existing catalog key even if its wire ID is overridden.

Use the resolved wire ID in request construction and freeze it with the selected
variant's executable settings so session reconstruction sends the same ID.
Preserve distinct local and wire identities in internal representations. For
provider-specific deployment/resource routing, use the supported adaptor's
mapping and reject combinations that cannot honor the override; never accept a
`model_id` setting that has no effect.

### Automatic native replay when unspecified

Retain `capabilities.native_replay` and its explicit values (`unsupported`,
`optional`, `required`), but make the authored field optional.

- When no value is supplied by configuration or an inherited catalog
  declaration, derive replay support from the resolved adaptor and request
  endpoint. Enable native replay whenever supported; otherwise resolve to
  unsupported.
- For a managed model, an omitted override retains an existing catalog
  declaration. Absence must remain distinct from an explicit `unsupported` so
  schema defaults do not inadvertently disable automatic replay.
- Explicit values remain authoritative, subject to adaptor capability
  validation. Explicit `unsupported` disables native replay. Unsupported
  explicit combinations fail validation instead of silently degrading.
- Recompute automatic resolution for the effective variant endpoint after
  inheritance and overrides. Do not freeze it against the base endpoint before
  a variant's endpoint selection is known.

Enabled native replay means capturing provider-native assistant response data
and reusing valid, compatible artifacts in subsequent conversation requests.
Always prefer such artifacts over normalized reconstruction when replay is
enabled. This is not retransmission of a prior HTTP request, a retry policy, or
permission to reuse artifacts across incompatible provider scopes.

Classify replay content by its format and portability rather than treating every
native artifact as one uniformly portable blob:

- Standard, non-custom-adaptor-specific blocks: replay by default when the target
  adaptor supports their semantics and encoding. Do not drop supported blocks
  because model/provider IDs, variants, endpoints, headers, or deployment
  fingerprints differ.
- Encrypted reasoning: replay only when the source and target effective wire
  model IDs are equal, and the target supports that encrypted reasoning format.
  This is the resolved `model_id` actually used for the request, not a local
  configuration alias or variant name. Missing source model identity cannot
  establish equality, so encrypted reasoning is not replayed in that case.
- Custom-adaptor-specific blocks: do not assume portability. Replay only through
  a target adaptor with explicit support for their format and semantics.

This supersedes the earlier unconditional cross-model encrypted-state policy.
Model-ID equality is necessary for encrypted reasoning but does not guarantee
acceptance: different providers may use different keys for the same model ID.
Provider, header, and deployment fingerprints are not additional identity gates
for otherwise eligible blocks. This explicitly applies to Azure too. Preserve
metadata needed for provenance, but distinguish it from replay eligibility.

Separate a block's encoding format from its originating adaptor identity.
Do not reject a standard supported block solely because its source adaptor ID
differs. Do not invent a translation between incompatible formats: an Anthropic
signed thinking block is not automatically an OpenAI Responses reasoning item.
Signed and encrypted are not interchangeable classifications; inventory actual
SDK block types and test their eligibility explicitly.

Where artifacts bundle several blocks, evaluate eligibility at block granularity
when the format permits safe separation. Do not drop portable content solely
because a companion encrypted block is ineligible; likewise, do not unwrap an
opaque bundle by guessing its structure. Required continuation semantics still
apply when omitting an ineligible block would make a request invalid.

This deliberately allows eligible native state to reach a different configured
provider. Document that the target receives it and can reject signatures,
encryption, or deployment-specific state. Do not promise cross-provider
portability or improved cache hits. Do not silently retry without reasoning on
provider rejection; use the existing explicit error and retry contracts.

Never fabricate absent native state, bypass artifact integrity/payload validation,
or inject native blocks the target encoder cannot represent. Unsupported format
conversion is distinct from a fingerprint mismatch and must remain visible in
diagnostics. Explicit `native_replay = "unsupported"` still disables replay.
If native data is unavailable, use normalized reconstruction only where the
protocol permits it; if continuation requires native data, return an explicit
error instead of silently dropping required state. Inventory existing SDK
`required` handling before implementation because its current reconstruction
fallback is not a blanket guarantee of required-artifact enforcement.

The intent is to preserve history fidelity and improve prompt-cache reuse where
the provider supports it. Native replay does not itself enable provider caching
or guarantee cache hits; existing cache settings remain separate.

Automatic resolution should select the supported replay behavior, including
protocol-required continuation handling, rather than blindly assigning the
literal capability value `required` to every replay-capable adaptor.

### Derived cancellation capability

Remove `cancellation` from authored model `capabilities`. Users cannot select a
cancellation mode; derive it from the resolved adaptor's implemented execution
behavior instead.

All currently integrated execution paths provide local cancellation only.
Cancelling stops local work or drops the request stream; it does not guarantee
that server-side generation or billing stops. The SDK's `RemoteBestEffort`
representation does not establish an implemented remote cancellation operation.

Keep cancellation information in compiled/runtime capabilities where needed by
existing consumers. Derive `local_only` for current adaptors. A future adaptor
may advertise provider cancellation only when its resolved endpoint/request mode
actually implements an explicit remote cancellation attempt.

Reject the removed authored field rather than silently ignoring it or retaining
an alias. Remove it from authored schemas, custom-model examples, fixtures, and
the user's local configuration during migration. This does not remove interrupt
handling, cancellation tokens, or SDK/runtime cancellation types.

### Model-local pricing

Move authored pricing out of `pricing.models."provider/model"` and into the
provider's model configuration. The requested target is:

```toml
[providers."provider_id".models."model_id".pricing]
input_per_million_usd = "0.10"
output_per_million_usd = "0.40"
```

These example prices are illustrative. Preserve the existing precision and
validation of decimal-string rates, and these optional fields:

- `input_per_million_usd`
- `output_per_million_usd`
- `reasoning_per_million_usd`
- `cache_read_per_million_usd`
- `cache_write_per_million_usd`

Both managed and custom providers use this location under the unified `models`
schema below. Variant-specific pricing is not requested. Pricing fallback
semantics remain an open decision below.

### Unified model definitions and overrides

Use `providers.<provider_id>.models.<model_id>` for both managed and custom
providers. Remove `model_overrides` from authored configuration.

For managed (`models_dev`) providers, the provider's catalog is the starting
model collection:

- Models not mentioned in configuration remain available under existing catalog
  availability rules. The configured `models` map is not an allowlist or a
  replacement for the catalog collection.
- An entry for an existing catalog model is a sparse override. Unspecified
  settings retain their catalog values, including generated variants.
- Merge supplied `generation_options` and `adaptor_options` fields individually
  into the catalog model settings. Preserve explicit false and empty lists;
  lists replace, not append.
- Variant entries use the agreed inheritance and sparse-override rules. An
  omitted or empty variant map does not remove catalog variants.
- Omitted model `enabled` preserves catalog behavior; `enabled = false`
  explicitly disables a model.
- Existing restrictions on what managed overrides may author remain in force;
  using the `models` name does not permit redefining catalog capabilities.
- A configured model ID absent from the catalog remains an error. Defining new
  models remains the role of a custom provider.

For custom providers, entries define models because there is no catalog model
collection to inherit. Existing required custom-model fields remain required;
the shared table name does not make incomplete custom definitions valid.

For a managed variant, the complete generation/adaptor precedence is:

1. Catalog base-model settings.
2. User model-level overrides under `models`.
3. Catalog variant settings.
4. User variant-level overrides.

This is catalog-to-configuration inheritance within a provider definition. It
does not change cross-file layering: a workspace provider definition still
replaces a same-ID user provider definition under the existing loader contract.

```toml
[providers.openai]
source = "models_dev"

[providers.openai.models."model-id"]
generation_options = { max_output_tokens = 8192 }

[providers.openai.models."model-id".variants.high]
generation_options = { max_output_tokens = 16384 }
```

This illustrative override preserves the model's other catalog settings and
the high variant's reasoning behavior, assuming that model and variant exist.

## Proposed Merge Contract

The following details are recommendations awaiting confirmation where noted.
They make sparse updates precise and testable.

| Field | Proposed behavior |
|---|---|
| `display_name` | Omission preserves the existing name; new variants receive an ID-derived name. |
| `model_id` | Omission inherits the provider/model wire ID; a supplied variant value overrides it. |
| `generation_options` | Merge supplied fields individually. |
| `adaptor_options` | Merge supplied fields individually, retaining explicit `false`. |
| Lists | Supplied lists replace, not append; `[]` clears the list. |
| `headers` | Merge by normalized header name using existing deletion semantics. |
| `reasoning` | Omission preserves; a supplied tagged object replaces the whole reasoning behavior. |

Parsing must preserve omission versus explicit empty values. In particular,
`stop` and `beta` currently use vectors whose deserialization defaults can erase
this distinction. Use presence-aware authoring representations, reusing
`PartialRequestDefaults` where appropriate.

Header empty-string deletion markers must survive until all existing header
layers have been combined. Dropping a marker early must not restore an inherited
header. Preserve header validation, ownership, and size limits.

Validate the final merged variant against model capabilities and adaptor
restrictions. Preserved settings that conflict with a newly selected endpoint
must produce an actionable validation error, not be silently discarded.

Preserve positions of existing variants; append newly created IDs in a
deterministic order. Resolve `default_variant` after applying configuration and
reject named defaults that are missing or disabled.

## Open Decisions

### 1. Disabling variants

Recommendation: introduce optional `enabled`, with omission equivalent to true.

```toml
[providers.openai.models."model-id".variants.low]
enabled = false
```

Proposed boundaries:

- `enabled = false` removes an existing variant.
- Disabling an absent variant is a valid no-op, including for custom providers.
- Reject other variant settings alongside `enabled = false` rather than ignore
  them.
- A default pointing to a disabled variant is invalid.

The user has not yet confirmed this replacement for `operation = "disable"`.

### 2. Clearing reasoning

Recommendation: omission preserves existing reasoning, with no new explicit
clear mechanism in this change. TOML has no null. A thinking-off toggle or a
`none` effort value is a concrete behavior, not removal of the setting.

### 3. Endpoint defaults and adaptor support

Recommendation: omitted `request_endpoint` preserves each adaptor's current
native endpoint; custom `openai-compatible` continues to use Chat Completions.
An explicit selection must be supported by that adaptor or rejected.

Before coding, enumerate the supported combinations, including how existing
endpoint-specific adaptor IDs interact with an explicit endpoint choice and
whether generic compatible Responses requires oven-sdk changes. Preserve
provider-specific routing such as Azure deployment paths; the conventional
paths above are not universal URL-construction instructions.

### 4. Pricing fallback and validation

Recommendation: relocate authoring without changing existing rate precedence,
missing-rate behavior, accounting precision, or historical session accounting.
Verify these against `crates/engine/src/usage.rs` and session reconstruction
before defining the final migration tests. Decide how to validate model-local
pricing for disabled or unavailable catalog models. Do not silently introduce
field-by-field catalog price inheritance while moving the schema.

## Proposed Qwen Configuration

This example uses the proposed names and three requested effort variants. It
leaves `default_variant` unspecified. All three variants inherit sampling and
API selection from the model; only reasoning effort differs.

```toml
[providers.cookie-api.models."qwen3.8-flash"]
# Existing display name and capabilities remain unchanged.
generation_options = { temperature = 1.0, top_p = 0.95 }
adaptor_options = { request_endpoint = "completions" }

[providers.cookie-api.models."qwen3.8-flash".variants.low]
reasoning = { type = "effort", value = "low" }

[providers.cookie-api.models."qwen3.8-flash".variants.medium]
reasoning = { type = "effort", value = "medium" }

[providers.cookie-api.models."qwen3.8-flash".variants.xhigh]
reasoning = { type = "effort", value = "xhigh" }
```

This is a partial model configuration, not a standalone valid provider. No
credentials belong in this document. Configuring effort requests it from the
backend; successful parsing or HTTP acceptance does not prove that the deployed
Qwen server implements distinct effort behavior.

## Compatibility and Migration

After implementation, reject removed authored keys instead of accepting aliases:

- `operation` in variants
- `defaults` and `options` in model/variant configuration
- top-level model `shape`
- `api_path` in adaptor configuration
- `cancellation` in authored model capabilities
- top-level `pricing`
- `model_overrides`, replaced by `models` for managed providers

Update relevant schemas, compiler errors, examples, and tests together. Keep
authored-config changes distinct from runtime manifests, protocol contracts,
and best-effort session history. Inventory persisted representations and either
retain their intentional contracts or explicitly plan necessary version changes;
do not mechanically rename shared serialized fields without checking consumers.

Migrate the user's local config only after the new binary is available. Preserve
provider endpoints, credentials, capabilities, unrelated settings, and the
current default-variant selection. Verify exact current contents before editing.
Do not commit the local config or expose credentials in logs.

## Implementation Plan After Approval

1. Resolve open decisions and inventory all authoring and persisted consumers.
2. Implement strict field renames, unified `models` authoring, catalog-model
   sparse overrides, presence-aware capability defaults, and presence-aware
   variant configuration.
3. Implement base-model inheritance, sparse variant merging, and final validation.
4. Implement endpoint selection through the complete adaptor execution path;
   remove the unused path setting and resolve automatic native replay for the
   effective endpoint while preserving explicit declarations. Derive runtime
   cancellation capability instead of accepting it from authored configuration.
   Implement wire model-ID overrides and block-specific replay eligibility,
   including encrypted reasoning's same-wire-model requirement.
5. Relocate pricing with unchanged accounting semantics unless separately
   approved.
6. Update tests, generated schemas/bindings where affected, and user docs.
7. Review, fix findings, run gates, commit/push when authorized, and verify CI
   for the exact final SHA.
8. Install the binary, migrate and validate local Qwen configuration, then run
   the established dev-artifact cleanup while preserving release/caches/sessions.

## Acceptance Tests

- New variant creation and sparse updates to generated catalog variants.
- Managed `models` entries preserve omitted catalog fields and unmentioned
  catalog models; omitted or empty variant maps retain generated variants.
- Model-level generation/adaptor overrides merge sparsely, including explicit
  false and empty lists. Custom model definitions still require their fields.
- Unknown managed model IDs and forbidden capability overrides remain errors;
  explicit model disabling works without replacing the catalog collection.
- Custom reasoning-only variants inherit base generation/adaptor settings.
- Managed variants resolve base, catalog variant, and user variant fields in
  that order; unspecified catalog fields must not mask the base.
- A single-field variant override preserves other inherited fields; base edits
  propagate only to fields not overridden by a later layer.
- Omitted fields preserve values; explicit false and empty lists take effect.
- Reasoning object replacement cannot create an invalid mixed behavior.
- Header normalization and inherited-header deletion remain correct.
- Deterministic ordering and default-variant resolution.
- Approved disabling semantics, including absent IDs and conflicting fields.
- Removed and unknown authored keys fail with clear errors.
- Omitted tool calling, parallel tool calls, and structured output resolve to
  true only in the absence of inherited declarations. Explicit false values
  survive parsing and managed inheritance.
- Missing catalog capability information uses the new true defaults rather than
  legacy projection fallbacks. Conflicting tool/parallel declarations and
  unsupported adaptor capabilities produce actionable errors.
- Authored cancellation is rejected; current custom and managed models receive
  derived local-only cancellation. Existing interruption behavior remains intact.
- Custom and managed generation/adaptor configuration validates consistently.
- Endpoint choices select the correct encoder, route, stream decoder, and
  tool-call handling; invalid adaptor combinations fail before network access.
- Omitted native replay enables valid artifact capture and reuse on supported
  adaptors/endpoints and resolves to unsupported elsewhere; explicit values and
  inherited catalog declarations remain distinguishable from absence.
- Variant endpoint changes recompute automatic replay support; explicit
  incompatible capability declarations fail validation.
- Standard supported blocks survive variant, model, provider, endpoint, header,
  and deployment-fingerprint changes without duplication. Wire tests confirm
  replay, including cross-provider and Azure revision-change/missing-revision
  cases, not merely successful configuration compilation.
- Encrypted reasoning replays for equal effective wire model IDs and is excluded
  for different or unknown source IDs. Test aliases with the same wire ID and
  the same alias with a variant-level wire-ID override. Mixed-content artifacts
  preserve eligible standard blocks when safe to separate.
- Model-ID resolution follows provider -> configured model -> configured variant;
  request dispatch and frozen reconstruction use the resolved ID while local
  selection and pricing identity remain unchanged.
- Adaptor identity differences do not reject artifacts with supported encoding
  formats. Incompatible formats and invalid artifacts are not replayed, and
  protocol-required native state is not silently discarded. Explicit replay
  disabling remains effective and provider rejection is surfaced. Cache
  configuration remains independent.
- Pricing relocation preserves decimal precision, rate precedence, missing-rate
  handling, and historical accounting as defined after the inventory.
- Config layer replacement semantics do not accidentally become deep merges.
- Existing title and compaction internal-agent flows remain functional.
- Qwen low/medium/xhigh parse and compile; tests verify emitted effort values
  without claiming unsupported backend behavior.

## Verification Gates

Follow the repository's current `AGENTS.md`, including:

```sh
cargo build --locked --workspace --all-targets
cargo test --locked --workspace
cargo fmt --all -- --check
cargo +stable clippy --locked --workspace --all-targets -- -D warnings
cargo +1.88.0 clippy --locked --workspace --all-targets -- -D warnings
crates/protocol/scripts/check-bindings.sh --check
RUSTDOCFLAGS='-D warnings' ./scripts/build-docs.sh
cargo deny --locked check advisories licenses sources
```

If oven-sdk changes are required, apply its own review and verification workflow
before updating cookie-agent's pinned dependency revision.

## Source Pointers

- `crates/models/src/model_types.rs`: current defaults, options, reasoning, and
  variant directive types.
- `crates/models/src/authoring/mod.rs`: custom/managed authoring and partial
  defaults.
- `crates/models/src/compiler/variants.rs`: generated variants, directives, and
  default resolution.
- `crates/models/src/compiler/dynamic.rs`: validation and header composition.
- `crates/models/src/compiler/executable.rs`: executable adaptor configuration.
- `crates/models/src/manager/mod.rs`: variant runtime compilation.
- `crates/config/src/runtime.rs`: current pricing authoring types.
- `crates/config/src/loader.rs`: configuration layer behavior.
- `crates/engine/src/usage.rs` and `crates/engine/src/session.rs`: pricing and
  session accounting.

No code changes are authorized by the existence of this draft.

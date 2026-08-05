# Recipe Registry 1 Provider Conformance

**Registry schema:** 1

This document freezes the initial code-owned provider, protocol, non-secret
setup-schema, auth-method, endpoint, and model-family entries. Catalog data
supplies claims only. It never supplies a setup schema, credential schema, wire auth, endpoint authority,
protocol implementation, or Oven constructor.

The claim tables below are covered by both:

- bundled catalog: 3,567,054 bytes, 177 providers, 279 canonical models,
  `sha256:d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a`;
- independently reviewed test-only live fixture captured from
  `https://models.dev/catalog.json` on 2026-08-05 at
  `crates/models/tests/fixtures/models-dev-live-audit-2026-08-05.json`:
  3,801,566 identity bytes, ETag
  `"25dd5dd6eb21b2d78044606eeb806d8c"`, 180 providers, 6,131 provider
  models, 293 canonical models, and
  `sha256:25dd5dd6eb21b2d78044606eeb806d8cdd38640c8deea071122d5591edb88795`.

Those digests are audit evidence only. The reviewed live fixture is test-only;
neither digest is a runtime pin or runtime acceptance criterion. Runtime still
validates the selected live/cache/bootstrap bytes.

## 1. Catalog claims versus code-owned setup

For a recipe entry, catalog matching independently checks:

1. exact provider ID;
2. provider `npm`: `PresentExact(value)`, `PresentOneOf(values)`, or `Absent`;
3. provider `api`: `PresentExact(value)`, `PresentOneOf(values)`, or `Absent`;
4. provider `env`: an exact duplicate-free set; array order is ignored;
5. each model's optional `provider` override object, including exact `npm`,
   `api`, and `shape` presence/absence and values;
6. the model's required structural/capability shape.

Expected absence is a claim: an unexpected `npm`, `api`, `shape`, or model
`provider` object is drift. API comparison uses the recipe's explicit raw
allowed set, then normalizes only for security/equivalence checks. Environment
matching never turns catalog strings into credentials.

The registry separately defines provider setup schemas and auth methods. A setup
schema contains only typed non-secret routing/configuration fields, their
required/defaulted status, validation, and code-owned endpoint construction. An
auth method separately contains secret credential fields and wire auth. Catalog
has neither setup-schema nor auth-method claims.

For an API-key auth method, catalog `env` aliases use **any-of** semantics for that one
semantic `api_key`: any one listed alias may satisfy optional convenience import,
but catalog order does not imply precedence. For provider setup and multi-field
auth, each alias maps only where explicitly stated below. Unmapped expected aliases remain checked
catalog claims and are not ambient credential sources.

If a known provider's npm/API/env claim drifts, quarantine that provider record
and all children. If a model provider-override or model-shape claim drifts,
quarantine only that model. A provider with no registry entry remains visible as
`no_reviewed_provider_recipe`; that is absence of support, not claim drift.

## 2. Initial provider claim patterns

`model.provider = absent` means the model has no provider override object.

| Recipe entry | Exact provider catalog claims | Allowed model provider claims |
|---|---|---|
| `anthropic.messages.v1` | id `anthropic`; npm exact `@ai-sdk/anthropic`; api absent; env exact `{ANTHROPIC_API_KEY}` | `model.provider` absent |
| `openai.responses.v1` / `openai.chat.v1` | id `openai`; npm exact `@ai-sdk/openai`; api absent; env exact `{OPENAI_API_KEY}` | `model.provider` absent; model ID must match section 4 |
| `openrouter.chat.v1` | id `openrouter`; npm exact `@openrouter/ai-sdk-provider`; api exact `https://openrouter.ai/api/v1`; env exact `{OPENROUTER_API_KEY}` | `model.provider` absent |
| `google.gemini.v1` | id `google`; npm exact `@ai-sdk/google`; api absent; env exact `{GOOGLE_API_KEY, GOOGLE_GENERATIVE_AI_API_KEY, GEMINI_API_KEY}` | `model.provider` absent; model ID begins `gemini-` |
| `cohere.chat.v2` | id `cohere`; npm exact `@ai-sdk/cohere`; api absent; env exact `{COHERE_API_KEY}` | absent, except exact `north-mini-code-1-0` override in section 5 |
| `compatible.groq.v1` | id `groq`; npm exact `@ai-sdk/groq`; api absent; env exact `{GROQ_API_KEY}` | `model.provider` absent |
| `compatible.togetherai.v1` | id `togetherai`; npm exact `@ai-sdk/togetherai`; api absent; env exact `{TOGETHER_API_KEY}` | `model.provider` absent |
| `compatible.deepinfra.v1` | id `deepinfra`; npm exact `@ai-sdk/deepinfra`; api absent; env exact `{DEEPINFRA_API_KEY}` | `model.provider` absent |
| `compatible.fireworks.v1` | id `fireworks-ai`; npm exact `@ai-sdk/openai-compatible`; api exact `https://api.fireworks.ai/inference/v1/`; env exact `{FIREWORKS_API_KEY}` | `model.provider` absent |
| `google.vertex.gemini.v1` | id `google-vertex`; npm exact `@ai-sdk/google-vertex`; api absent; env exact `{GOOGLE_VERTEX_PROJECT, GOOGLE_VERTEX_LOCATION, GOOGLE_APPLICATION_CREDENTIALS}` | only `model.provider` absent; every Anthropic or OpenAI-compatible override is quarantined |
| `amazon.bedrock.converse.v1` | id `amazon-bedrock`; npm exact `@ai-sdk/amazon-bedrock`; api absent; env exact `{AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_REGION, AWS_BEARER_TOKEN_BEDROCK}` | only `model.provider` absent; every `@ai-sdk/amazon-bedrock/mantle` override is quarantined |
| `azure.openai.v1` | id `azure`; npm exact `@ai-sdk/azure`; api absent; env exact `{AZURE_RESOURCE_NAME, AZURE_API_KEY}` | only `model.provider` absent and OpenAI model families in section 4; Anthropic/OpenAI-compatible overrides are quarantined |

No initial entry expects provider npm absence. Registry schema 1 nevertheless
supports `npm = Absent`; adding such an entry requires a reviewed code change.
All initial provider records require top-level `shape` absence. Unless an exact
model exception below says otherwise, model-level `provider` and standalone
model `shape` claims are also expected absent.

## 3. Initial code-owned recipes, setup schemas, and auth methods

| Entry | Protocol recipe / Oven `AdapterId` | Code-owned endpoint policy | Separate setup map / auth credentials |
|---|---|---|---|
| `anthropic.messages.v1` | `oven.anthropic.messages` / `anthropic` | default `https://api.anthropic.com/v1`; authored HTTPS override allowed with same-definition auth | setup `{}` / `anthropic-api-key-v1 { api_key }` |
| `openai.responses.v1` | `oven.openai.responses` / `openai-responses` | default `https://api.openai.com/v1`; authored HTTPS override allowed | setup `{}` / `bearer-api-key-v1 { api_key }` |
| `openai.chat.v1` | `oven.openai.chat` / `openai-chat` | same OpenAI default/override policy | setup `{}` / same API-key auth |
| `openrouter.chat.v1` | `oven.openai-compatible.chat` / `openai-compatible` | default `https://openrouter.ai/api/v1`; authored HTTPS override allowed | setup `{}` / `bearer-api-key-v1 { api_key }` |
| `google.gemini.v1` | `oven.google.gemini.generate-content` / `google-gemini` | default `https://generativelanguage.googleapis.com/v1beta`; authored HTTPS override allowed | setup `{}` / `google-api-key-header-v1 { api_key }` |
| `cohere.chat.v2` | `oven.cohere.chat-v2` / `cohere-v2-chat` | default `https://api.cohere.com/v2`; authored HTTPS override allowed | setup `{}` / `bearer-api-key-v1 { api_key }` |
| `compatible.groq.v1` | `oven.openai-compatible.chat` / `openai-compatible` | default `https://api.groq.com/openai/v1`; authored HTTPS override allowed | setup `{}` / `bearer-api-key-v1 { api_key }` |
| `compatible.togetherai.v1` | same compatible recipe/adapter | default `https://api.together.xyz/v1`; authored HTTPS override allowed | setup `{}` / `bearer-api-key-v1 { api_key }` |
| `compatible.deepinfra.v1` | same compatible recipe/adapter | default `https://api.deepinfra.com/v1/openai`; authored HTTPS override allowed | setup `{}` / `bearer-api-key-v1 { api_key }` |
| `compatible.fireworks.v1` | same compatible recipe/adapter | default normalized `https://api.fireworks.ai/inference/v1`; authored HTTPS override allowed | setup `{}` / `bearer-api-key-v1 { api_key }` |
| `google.vertex.gemini.v1` | `oven.google.vertex.generate-content` / `google-vertex-gemini` | code constructs the reviewed Vertex publisher endpoint from setup; authored `base_url` forbidden | setup `{ project, location, resource? = "publishers/google" }` / `oauth-access-token-v1 { access_token }` |
| `amazon.bedrock.converse.v1` | `oven.bedrock.converse` / `aws-bedrock-converse` | code/Oven constructs the regional Bedrock endpoint from setup; authored `base_url` forbidden | setup `{ region }` / `aws-sigv4-credentials-v1 { access_key_id, secret_access_key, session_token? }` |
| `azure.openai.v1` | `oven.azure.openai.chat` / `azure-openai-chat`, or `oven.azure.openai.responses` / `azure-openai-responses` by section 4 model routing | code constructs Azure endpoint from setup; authored `base_url` forbidden | setup `{ resource_name, deployment, api_version }` / `azure-api-key-v1 { api_key }` |

Vertex `GOOGLE_VERTEX_PROJECT` and `GOOGLE_VERTEX_LOCATION` are convenience
aliases for setup fields. Optional setup `resource` is a bounded reviewed
relative resource path, defaults to `publishers/google`, and never belongs to
auth. `GOOGLE_APPLICATION_CREDENTIALS` is an exact
catalog claim but is not consumed: Cookie/Oven registry 1 does not implement ADC
or ambient credential files. `access_token` must be supplied explicitly.

Bedrock `AWS_REGION` maps only to setup `region`; `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY` map only to auth credentials.
`AWS_BEARER_TOKEN_BEDROCK` is an expected catalog claim but is not consumed by
the Converse recipe. `session_token` is optional explicit input. Registry 1 does
not implement ambient AWS SDK/profile/default chains.

Azure aliases map resource name and API key only. Deployment and API version are
explicit setup fields. API-key Azure is current Cookie compiler support. Azure
may use ergonomic `api_key` because setup is separate. Entra/OAuth identity is
future and must not be inferred from ambient identity.

Every Registry-1 setup field is non-secret behavioral routing/configuration
metadata: Vertex project/location/resource, Bedrock region, and Azure resource
name/deployment/API version are included directly in safe canonical behavior and
config fingerprints. Registry 1 declares no sensitive setup field. API keys,
access tokens, AWS secret material, and session tokens are auth-only and excluded
from fingerprints. A future non-auth sensitive setup need requires a future
schema/recipe version and independent mechanism.

## 4. OpenAI and Azure model routing

Case-sensitive family routing is code-owned:

1. Responses: exact ID or `-`-suffixed descendants of `gpt-5`, `o1`, `o3`, or
   `o4` (including `o4-mini`).
2. Chat: exact ID or `-`-suffixed descendants of `gpt-4.1`, `gpt-4o`,
   `gpt-4-turbo`, or `gpt-3.5-turbo`.
3. No match quarantines that model as `unreviewed_openai_model_family`.
4. An ambiguous future match quarantines it as
   `ambiguous_openai_model_family`.

The Azure entry applies the same routing only to models with no model provider
override. Azure Anthropic and generic-compatible model overrides are quarantined
because they are not the Azure OpenAI Chat/Responses recipes.

## 5. Model provider-override exceptions

The bundled and audited live Cohere record `north-mini-code-1-0` has exactly:

```text
provider.npm = "@ai-sdk/openai-compatible"
provider.api = "https://api.cohere.ai/compatibility/v1"
provider.shape = absent
```

Registry 1 maps only that exact override through
`oven.openai-compatible.chat` / `openai-compatible`, using Cohere's existing
`api-key` setup and bearer auth. Any changed npm/API/shape or another Cohere
override quarantines only that model.

For Google Vertex, all current `@ai-sdk/google-vertex/anthropic` and
`@ai-sdk/openai-compatible` model overrides are quarantined; only unoverridden
Gemini records enter `oven.google.vertex.generate-content`.

For an unoverridden Vertex record, Registry 1 applies this exact reviewed Gemini
predicate after structural validation:

1. provider model table key equals embedded `id`;
2. ID contains only lowercase ASCII letters, digits, `.`, `_`, and `-`, starts
   exactly `gemini-`, contains no `/`, and is not `gemini-embedding-001` or any
   future `gemini-embedding-` descendant;
3. `family` is exactly one of `gemini-flash`, `gemini-flash-lite`, or
   `gemini-pro`;
4. `modalities.input` contains `text`, `modalities.output` contains `text`,
   context/output limits are positive, and attachment/reasoning/tool-call and
   all other required Gemini metadata have the strict expected types;
5. the complete record compiles losslessly through
   `oven.google.vertex.generate-content`.

Every unoverridden Vertex record failing any family predicate is quarantined as
`unsupported_vertex_model_family`. In particular,
`openai/gpt-oss-20b-maas` and `openai/gpt-oss-120b-maas` are explicitly
quarantined and never routed as Gemini. Future unoverridden non-Gemini families
also quarantine until Registry 1 is deliberately revised. Provider-level model
npm/API/shape overrides are evaluated separately first and retain their specific
drift quarantine reason.

The exact audited Vertex override claim patterns are:

```text
npm = "@ai-sdk/google-vertex/anthropic", api absent, shape absent
npm = "@ai-sdk/openai-compatible"
api = "https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}/endpoints/openapi"
shape absent
```

For Bedrock, all current `@ai-sdk/amazon-bedrock/mantle` overrides, including
their `shape = "responses"` claims, are quarantined; only unoverridden Converse
records enter `oven.bedrock.converse`.

Their exact audited APIs are
`https://bedrock-mantle.${AWS_REGION}.api.aws/openai/v1` or
`https://bedrock-mantle.${AWS_REGION}.api.aws/v1`, with npm
`@ai-sdk/amazon-bedrock/mantle` and shape exactly `responses`.

Azure audited overrides are quarantined in two exact current forms:

```text
npm = "@ai-sdk/anthropic"
api = "https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1"
shape absent

npm = "@ai-sdk/openai-compatible"
api = "https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/models"
shape = "completions"
```

## 6. Auth family registry

Registry 1 implements these semantic families:

| Auth method | Safe parameters | Secret credential fields | Owned wire headers/behavior |
|---|---|---|---|
| `no-auth-v1` | none | none | emits no auth material; only explicitly allowlisted recipes |
| `bearer-api-key-v1` | none | required `api_key` | owns `authorization` with Bearer encoding |
| `api-key-header-v1` | required `header_name`, restricted to the adaptor recipe's allowlist | required `api_key` | owns that canonical case-insensitive header |
| `anthropic-api-key-v1` | none | required `api_key` | owns `x-api-key`; protocol owns `anthropic-version` and reviewed beta/version headers |
| `google-api-key-header-v1` | none | required `api_key` | owns `x-goog-api-key`; no credential query |
| `oauth-access-token-v1` | none | required `access_token` | owns `authorization` with Bearer encoding |
| `aws-sigv4-credentials-v1` | none | required `access_key_id`, required `secret_access_key`, optional `session_token` | owns `authorization`, `host`, `x-amz-date`, `x-amz-content-sha256`, and `x-amz-security-token` when used |
| `azure-api-key-v1` | none | required `api_key` | owns `api-key`; resource/deployment/API version are setup-only |

All catalog/authored endpoint query components remain rejected. Azure's
code-owned protocol compiler may place the validated `api_version` setup value
in the provider-required request parameter; no user/catalog URL query is parsed,
preserved, or forwarded.

There is no ambient ADC, AWS SDK/profile chain, metadata-service lookup, Entra,
or filesystem credential discovery in current Registry 1.

For custom providers, initial adaptor allowlists are: `openai-compatible`
allows `bearer-api-key-v1`, `api-key-header-v1`, and reviewed `no-auth-v1`, with
API-key header parameters limited to `x-api-key` or `api-key`; `openai-chat` and
`openai-responses` allow `bearer-api-key-v1` and reviewed `no-auth-v1`;
`anthropic` allows `anthropic-api-key-v1`; `google-gemini` allows
`google-api-key-header-v1`; `google-vertex-gemini` allows
`oauth-access-token-v1`; `aws-bedrock-converse` allows
`aws-sigv4-credentials-v1`; Azure adapters allow `azure-api-key-v1`; and
`cohere-v2-chat` allows `bearer-api-key-v1`.
Any custom auth method not present in the selected adaptor's exact allowlist
fails the entire custom provider as `unsupported_auth_method`.

`basic` is not contract-supported by any current Registry-1 Oven adaptor and is
rejected as `unsupported_auth_method`. It may be added only by a future reviewed
auth method that owns `authorization` and proves adapter support.

Custom static headers are never auth. Their names/values are safe behavior
metadata and cannot interpolate. Every adaptor begins with the forbidden static
set `authorization`, `host`, `content-length`, `transfer-encoding`, `connection`,
`proxy-authorization`, `cookie`, `set-cookie`, `accept`, `content-type`, and
`user-agent`, then adds all protocol/auth-owned headers. Header names use RFC
field-name token syntax and lowercase canonical identity; values reject controls,
CR/LF/NUL, and credential interpolation markers. Any case-insensitive duplicate
or collision is invalid.

## 7. Oven package pins

| Oven adapter family | Workspace package/version |
|---|---|
| core | `oven-sdk` 0.4.0 |
| Anthropic | `oven-sdk-anthropic` 0.5.0 |
| OpenAI and compatible | `oven-sdk-openai` 0.4.0 |
| Google Gemini | `oven-sdk-google` 0.4.0 |
| Google Vertex | `oven-sdk-google-vertex` 0.4.0 |
| Bedrock | `oven-sdk-bedrock` 0.3.0 |
| Azure | `oven-sdk-azure` 0.3.0 |
| Cohere | `oven-sdk-cohere` 0.2.0 |

Open Responses (`oven-sdk-open-responses` 0.2.0) remains future registry work.
Package presence alone never enables a recipe.

## 8. Quarantine reasons

Known-provider claim drift quarantines the exact record with one of:

```text
catalog_provider_npm_drift
catalog_provider_api_drift
catalog_provider_env_drift
catalog_provider_shape_drift
catalog_model_provider_npm_drift
catalog_model_provider_api_drift
catalog_model_provider_shape_drift
catalog_model_shape_drift
unreviewed_openai_model_family
ambiguous_openai_model_family
unsupported_model_capabilities
unsupported_protocol_feature
unsupported_vertex_model_family
```

Provider-level drift quarantines the provider and children. Model-level drift
quarantines only that model. Quarantine descriptors remain safe-visible in
`/connect` or diagnostics when a unique valid ID is recoverable, but never
compile or connect.

Unknown providers use `no_reviewed_provider_recipe` without pretending a claim
matched. Removed providers without a retained exact source projection use
`removed_without_retained_recipe_match`.

## 9. Conformance validation

Tests must compare bundled fixtures and live-response fixtures against every
exact claim above, including expected absence, env-set equality and any-of alias
mapping, Cohere/Vertex/Bedrock/Azure model overrides, Groq/Together/DeepInfra npm
claims, trailing-slash Fireworks API, and provider-versus-model quarantine
boundaries. Vertex tests explicitly reject both unoverridden `gpt-oss` MaaS IDs,
embedding/non-Gemini/future families, and override drift. Request golden tests
cover every current provider setup schema and auth method separately and prove
that ADC, ambient AWS chains, Bedrock bearer, and Azure Entra remain disabled.

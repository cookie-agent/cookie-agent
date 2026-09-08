# Providers [providers]

Use `/connect` for a catalog-backed provider, or define a custom endpoint and
model below. Provider configuration lives under `[providers.<id>]` in
[config.toml](configuration.md). The authored map defaults to empty; managed
providers can still be available through the catalog and user provider store.

## Managed providers

`/connect` in the TUI, or `cookie connect` against the daemon, walks through
provider setup and authentication. Connections are stored globally for the
current user, not in the workspace config. Setup is validated locally; the first
model request tests the credentials. Never commit credential-bearing files.

At startup, the engine refreshes `https://models.dev/catalog.json`, falling back
to a validated cache or the bundled catalog. Supported, non-deprecated text-output
models are included automatically. Catalog declarations are not probes of your
endpoint: account access, deployed model features, and billing may differ.

This is a complete `config.toml` alternative to `/connect`, with credentials
supplied by the environment:

```toml
[providers.openai]
source = "models_dev"
api_key = "${env:OPENAI_API_KEY}"
```

Authored `api_key` or `auth_override` takes precedence over an eligible saved
connection, then a supported no-auth recipe, otherwise the provider is
unavailable. An authored `base_url` cannot inherit store credentials or setup.
Endpoint selection uses authored `base_url`, catalog API URL, then family
default; setup-derived families retain their own routing.

### Provider fields

| Key | Type | Default | Description |
|---|---|---|---|
| `source` | string | *(required)* | Must be `"models_dev"`. |
| `base_url` | string | *(none)* | HTTPS endpoint override. Requires same-definition auth (`api_key` or `auth_override`) and never inherits provider-store setup or credentials. Forbidden for families that compute their endpoint from setup (Vertex, Bedrock, Azure). |
| `setup` | map of string values | empty | Setup fields the provider recipe requires (for example `project`, `location`, `region`, `resource_name`). Native Azure Responses compaction also requires `model`, `version`, and `deployment_type`. Interpolates `${env:NAME}`. |
| `api_key` | string | *(none)* | Single-secret default auth. Allowed only for providers whose default method is an unambiguous single API key. Interpolates `${env:NAME}`. |
| `auth_override` | table | *(none)* | Explicit auth method override. Mutually exclusive with `api_key`. |
| `cache` | table | family default | [Prompt-cache policy](#prompt-caching) matching the resolved adaptor family. |
| `headers` | map of strings | empty | Provider request-header overrides. |
| `models` | map | empty | Sparse per-model overrides: `enabled`, `display_name`, `model_id`, `generation_options`, `adaptor_options`, `variants`, `default_variant`, `compaction`, `headers`, `pricing`. Unmentioned catalog models remain available. Cannot invent models or directly change catalog capabilities. |

Within a model override, `compaction` defaults to `"unsupported"`. Set it to
`"openai-responses-compact"` for the OpenAI Responses recipe or
`"azure-responses-compact"` for the Azure Responses recipe. The compiler derives
the native capability from this setting and rejects it for other recipes. The
Azure provider identity must be `azure.openai` to match Oven's native scope.
Frozen manifests store the compiled native/unsupported capability, not this
setting string, so they never contain the retired `"v1"` value. Authored `"v1"`
fails with a migration error naming the adapter-specific replacement.

`auth_override`:

| Key | Type | Description |
|---|---|---|
| `method` | string | One of the current auth method IDs (see below). |
| `values` | map of strings | Credential values keyed by credential field name (for example `api_key`, `access_token`, `access_key_id`, `secret_access_key`, `session_token`). Interpolates `${env:NAME}`. |

### Managed model overrides

Use `models.<model-id>` for a sparse override, not an allowlist. Unmentioned
models and omitted or empty variant maps retain catalog availability and
variants. An unknown catalog model ID is an error; use a custom provider to
define new models. Omitted `enabled` retains catalog behavior; `false` disables
the model. Capabilities cannot be authored on a managed override.

This complete file illustrates an override for the bundled catalog's `gpt-5`
model. Confirm availability in your runtime before making requests:

```toml
[providers.openai]
source = "models_dev"
api_key = "${env:OPENAI_API_KEY}"

[providers.openai.models.gpt-5]
generation_options = { max_output_tokens = 8192 }
default_variant = "base"

[providers.openai.models.gpt-5.variants.high]
generation_options = { max_output_tokens = 4096 }
```

Model overrides accept `enabled`, `display_name`, `model_id`, `generation_options`,
`adaptor_options`, `pricing`, `variants`, `default_variant`, `compaction`, and
`headers`. Except for the explicit compaction policy described above, omitted
fields retain catalog/base values. Display names and generation settings have
the same constraints as custom definitions. Variant and pricing rules are below.

## Custom providers

Custom providers are config-only, never appear in `/connect`, and never use
the provider store. Any valid provider ID is allowed; on collision with a
catalog provider ID the custom definition takes precedence.

### Custom model example

Complete `config.toml` for an illustrative endpoint. Replace the URL, model ID,
limits, and capability declarations with those of your actual service. The
reasoning-only variants inherit sampling and endpoint settings from the base:

```toml
[providers."custom.example"]
source = "custom"
endpoint = "https://api.example.invalid/v1"
adaptor = "openai-compatible"
auth = { method = "bearer-api-key-v1", values = { api_key = "${env:CUSTOM_API_KEY}" } }

[providers."custom.example".models."example-model"]
display_name = "Example Model"
model_id = "backend-model-v1"
generation_options = { temperature = 1.0, top_p = 0.95, max_output_tokens = 4096 }
adaptor_options = { request_endpoint = "completions" }

[providers."custom.example".models."example-model".capabilities]
input = ["text"]
output = ["text"]
context_tokens = 32768
output_tokens = 4096
reasoning = true
temperature = true
top_p = true
seed = false
media = {}

[providers."custom.example".models."example-model".variants.low]
reasoning = { type = "effort", value = "low" }

[providers."custom.example".models."example-model".variants.medium]
reasoning = { type = "effort", value = "medium" }

[providers."custom.example".models."example-model".variants.xhigh]
model_id = "backend-model-high-v1"
reasoning = { type = "effort", value = "xhigh" }
```

The local selection key is `custom.example/example-model`. The base, `low`, and
`medium` variants send `backend-model-v1`; `xhigh` sends `backend-model-high-v1`.
All variants still inherit the base sampling and endpoint settings unless
overridden. No default variant is selected by this example.
Setting an effort requests that behavior; parsing or HTTP acceptance does not
prove that the backend implements distinct effort levels.

For a local service, change `endpoint` to its loopback base URL, such as
`http://127.0.0.1:11434/v1`, and use
`auth = { method = "no-auth-v1", values = {} }`. Compatible Responses supports
tools and native replay with no auth when explicitly declared and supported by
the server; it is not restricted to a tool-less text-only legacy path.

### Provider fields

| Key | Type | Default | Description |
|---|---|---|---|
| `source` | string | *(required)* | Must be `"custom"`. |
| `endpoint` | string | *(required)* | Absolute, query-free URL. Must be `https`, or `http` to `localhost`/a loopback address. Interpolates `${env:NAME}`. |
| `adaptor` | string | *(required)* | Protocol adaptor ID (see below). |
| `setup` | map of string values | empty | Adaptor-required setup fields (Vertex `location`/`project`/`resource`, Bedrock `region`, Azure `api_version`/`deployment`). Interpolates `${env:NAME}`. |
| `auth` | table | *(required)* | Typed auth definition (see below). |
| `headers` | map of strings | empty | Public provider request-header overrides. Supports environment and session templates. |
| `cache` | table | adaptor default | [Prompt-cache policy](#prompt-caching) matching `adaptor`. |
| `models` | map | *(required)* | At least one custom model definition. |

### Authentication and adaptors

Custom `auth` accepts required `method` and `values`, plus optional `parameters`
(empty by default). Each method requires exactly the listed credential fields;
only `api-key-header-v1` accepts parameters.

| Method | Credential fields in `values` | Additional requirement |
|---|---|---|
| `no-auth-v1` | Empty map | No parameters |
| `bearer-api-key-v1` | `api_key` | Bearer header |
| `api-key-header-v1` | `api_key` | `parameters = { header_name = "x-api-key" }` or `"api-key"` |
| `anthropic-api-key-v1` | `api_key` | Anthropic key header |
| `google-api-key-header-v1` | `api_key` | Google key header |
| `oauth-access-token-v1` | `access_token` | Caller-supplied access token |
| `aws-sigv4-credentials-v1` | `access_key_id`, `secret_access_key`, optional `session_token` | SigV4 signing |
| `azure-api-key-v1` | `api_key` | Azure key header |

| Custom adaptor | Allowed auth methods | Required setup |
|---|---|---|
| `openai-compatible` | Bearer, API-key-header, no-auth | None |
| `openai-chat`, `openai-responses` | Bearer, no-auth | None |
| `anthropic`, `anthropic-compatible` | Anthropic API key | None |
| `google-gemini` | Google API key | None |
| `google-vertex-gemini` | OAuth access token | `project`, `location`; `resource` defaults to `publishers/google` |
| `aws-bedrock-converse` | AWS SigV4 | `region` |
| `azure-openai-chat`, `azure-openai-responses` | Azure API key | `deployment`, `api_version` |
| `cohere-v2-chat` | Bearer | None |

The short auth labels above refer to the full method IDs in the preceding table.
Managed providers instead use their catalog family recipes, including their
setup fields and allowed methods; `/connect` presents that exact form.
Vertex, Bedrock, and Azure compute routes from setup and reject arbitrary
managed `base_url` overrides. Custom endpoints must satisfy the selected
adaptor's route constraints too.

### Model fields

Custom model definition under `[providers."custom.x".models."<model-id>"]`:

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Whether the model is compiled into the runtime. |
| `display_name` | string | *(required)* | Nonblank, at most 512 bytes, no control characters. |
| `model_id` | string | inherited | Provider request identifier, independent of the local selection key; see [Request model identity](#request-model-identity). |
| `capabilities` | table | *(required)* | Explicit capability declarations (see below). |
| `generation_options` | table | empty | Generation settings (see below). |
| `adaptor_options` | table | empty | Typed adaptor-specific settings (see below). |
| `pricing` | table | absent | Whole-model rate override, including an explicitly empty rate set. |
| `headers` | map of strings | empty | Model request-header overrides. Variants accept the same header map. |
| `variants` | map | empty | Implicit variant creation or sparse overrides. |
| `default_variant` | string | *(none)* | `"base"` or a named variant that must exist in `variants`. |

### Request model identity

Optional `model_id` is accepted on custom models, managed-model overrides, and
variants. Later supplied values win:

1. Provider-supplied identifier, normally the catalog/model key; custom Azure
   uses its configured `setup.deployment` as the fallback.
2. Model-level `model_id`.
3. Variant-level `model_id`.

Omission inherits. Values must be nonempty, at most 2,048 bytes, without control
characters or surrounding whitespace, and valid for the selected adaptor's
request route. Wire identifiers are not restricted to local alias syntax.
OpenAI-style and Messages requests use the resolved model field; Gemini and
Vertex use it in their model resource, Bedrock in its model path, and Azure in
its deployment/model mapping. Resource restrictions still apply: for example,
Gemini requires a bare model ID rather than a `models/` prefix, slash, colon,
query, fragment, or whitespace. Unsupported route/identifier combinations fail
when constructing the executable model, rather than ignoring the override.

Changing `model_id` does not rename the local model, change its pricing lookup,
modify catalog membership, or infer new capabilities. A managed alias must still
name an existing catalog entry. The selected effective wire ID and endpoint are
frozen with the executable settings, so reconstruction preserves them even when
the current configuration or catalog changes.

### Capabilities and media

Custom fields are required unless a default or omission rule is stated below.

| Key | Type | Description |
|---|---|---|
| `input` | array of strings | Nonempty modalities: `text`, `image`, `audio`, `pdf`, `video`; subject to adaptor support. |
| `output` | array of strings | Currently only `["text"]` is supported. Structured JSON is text, not a separate modality. |
| `context_tokens` | integer | Context window in tokens; must be greater than zero. |
| `output_tokens` | integer | Maximum output tokens; must be greater than zero and at most `context_tokens`. |
| `tool_calling` | boolean | Defaults to `true` when not declared or inherited. |
| `parallel_tool_calls` | boolean | Defaults to `true`; requires `tool_calling = true`. |
| `structured_output` | boolean | Defaults to `true`; declares JSON-structured output support. |
| `reasoning` | boolean | Reasoning support. |
| `temperature` | boolean | Temperature parameter support. |
| `top_p` | boolean | Top-p parameter support. |
| `seed` | boolean | Seed parameter support. |
| `compaction` | string | Defaults to `"unsupported"`, the only accepted custom capability value. Native opt-in is managed-model configuration. |
| `native_replay` | string | Optional `"unsupported"`, `"optional"`, or `"required"`; omission derives supported replay from the resolved adaptor and endpoint. |
| `media` | map | Per-kind (`image`, `audio`, `pdf`, `video`) input limits: nonempty `mime_types` string array, positive `max_bytes` and `max_count`. Exactly the non-text input modalities need entries; text-only models use `{}`. |

The three optimistic `true` defaults are declarations, not endpoint detection.
Set unsupported features to `false`. In particular, disabling tool calling also
requires `parallel_tool_calls = false`. Invalid effective declarations are errors.
For an endpoint without tools or structured output, add this fragment to its
capabilities table:

```toml
tool_calling = false
parallel_tool_calls = false
structured_output = false
```

These flags neither request JSON for ordinary turns nor guarantee parallel
execution or backend support. Managed models retain explicit catalog false
declarations; only missing metadata falls back to true.

Input support also depends on the delivery channel. A modality accepted in a
user turn is not necessarily accepted in a tool result. Counts are enforced per
request; a tool result and its emitted messages share a combined budget.
See [media reads](../reference/tools.md#media-reads) for supported channels,
MIME types, retention, and operational limits. Unsupported combinations produce
tool errors, not generated media output.

### Replay and cancellation

Cancellation is derived as local-only; it does not guarantee server generation
or billing stops, and `capabilities.cancellation` is rejected.

All integrated SDK codecs support replay. Automatic replay is optional except
Anthropic reasoning, which requires native state. Azure ordinary replay does
not require deployment revision metadata; `model`, `version`, and
`deployment_type` remain required for native compaction. Azure Chat supports
native text/tool replay, not provider-authoritative reasoning replay.
Explicit replay declarations remain authoritative and fail when incompatible;
`native_replay = "unsupported"` disables replay. Automatic replay is recomputed
for the effective variant endpoint.

Replay preserves assistant response history, not a prior HTTP request. With
replay enabled, the target prefers valid native blocks it supports:

| Content | Eligibility |
|---|---|
| Standard supported text, tools, and non-encrypted reasoning blocks | Portable across local models, variants, providers, endpoints, headers, and deployment fingerprints when the target supports their format and semantics |
| Encrypted/opaque reasoning: Responses `encrypted_content`, redacted Messages/Converse state, Gemini/Vertex `thoughtSignature` | Target format support and equal, known source/target effective wire model IDs are both required |
| Custom-adaptor-specific or unknown blocks/codecs | Only explicit target support authorizes replay; unknown formats are not guessed or translated |

Signed visible thinking is not encrypted reasoning. A supported Messages signed
thinking block can remain eligible across wire-model changes, but is not
automatically convertible to Responses reasoning. Native payload integrity and
agreement with normalized history are still validated; provider/header/model
fingerprints are not additional eligibility restrictions for standard blocks.
When a validated bundle can be separated safely, portable siblings survive
exclusion of an ineligible encrypted block.

Ordinary tool history can reconstruct from normalized text and calls if native
data is unavailable. Required continuation state cannot silently disappear:
missing, corrupt, or ineligible required reasoning/signatures fail closed, even
when ordinary siblings are portable. Saved evidence that opaque state was
required is distinct from an ordinary integrity marker; a tool call alone does
not imply encrypted state. Legacy history without that evidence is read
best-effort, not repaired by inventing missing native data. Missing source wire
identity never establishes equality for encrypted replay.

Switching providers can send eligible prior native state to the new endpoint.
Equal wire model IDs are necessary for encrypted replay, but a different service
can still reject ciphertext or signatures, even for that ID. A provider HTTP 400
is not automatically recovered by stripping reasoning and resending. Errors
follow the normal [retry and fallback policy](../engine/model_retry.md).
Replay neither enables prompt caching nor guarantees cache hits or live backend
acceptance. Cache policy and [native compaction](compaction.md#native-provider-compaction)
retain their separate scope and validation rules.

## Generation options

Model and variant `generation_options` supply shared generation settings. On a
custom base model, omission leaves optional values unset and `stop` empty; on
managed models and variants, supplied fields override individually. Agent limits
and explicit request settings can further constrain the effective request.

| Key | Type | Description |
|---|---|---|
| `temperature` | float | Finite value; requires `capabilities.temperature`. |
| `top_p` | float | Finite value; requires `capabilities.top_p`. |
| `max_output_tokens` | integer | Must be greater than zero and at most `capabilities.output_tokens`. |
| `stop` | array of strings | Stop sequences. |
| `seed` | integer | Requires `capabilities.seed`. |
| `tool_choice` | string or table | `"auto"`, `"none"`, `"required"`, or `{ named = "tool-name" }`; requires `capabilities.tool_calling`. |

Current integration limitation: authored `stop`, `seed`, and `tool_choice` are
accepted, validated, inherited, and retained in model snapshots, but the shared
request-default application does not forward them to SDK requests. Do not rely
on those authored fields to control a live request. Temperature, top-p,
maximum output tokens, and supported reasoning settings do reach request
construction; final behavior still depends on the selected adaptor and backend.

## Adaptor options

These are typed model/variant settings, not arbitrary JSON body fields. All are
optional; omission inherits before adaptor defaults apply. Explicit `false`
and empty lists remain overrides. Nonempty wrong-adaptor settings fail.

| Key | Type | Description |
|---|---|---|
| `request_endpoint` | string | `"completions"` (Chat Completions) or `"responses"`; selects the complete codec and route. |
| `beta` | array of strings | Anthropic beta header values; rejected for non-Anthropic adaptors. |
| `organization` | string | Official-auth OpenAI organization header; not compatible or unauthenticated Responses. |
| `project` | string | Official-auth OpenAI project header; not compatible or unauthenticated Responses. |
| `store` | boolean | OpenAI/compatible Responses only, not Azure. The stateless encoder supports `false`; `true` is rejected. |

### Request endpoint

| Adaptor | Omitted | `completions` | `responses` |
|---|---|---|---|
| `openai-chat` | Chat Completions | Supported | Supported |
| `openai-responses` | Responses | Supported | Supported |
| `openai-compatible` | Chat Completions | Supported | Compatible Responses |
| `azure-openai-chat` | Azure Chat | Supported | Supported |
| `azure-openai-responses` | Azure Responses | Supported | Supported |
| Anthropic, Gemini, Vertex Gemini, Bedrock Converse, Cohere | Native API | Rejected | Rejected |

Managed omission preserves catalog routing. Selection chooses the encoder,
decoder, streaming protocol, and route, not just a URL suffix. `completions`
means `/chat/completions`, not the legacy `/completions` API. Azure retains its
deployment-specific routing. Backend compatibility must be verified separately;
selecting Responses cannot add it to a server that only implements Chat.

Changing a variant endpoint validates all inherited settings against that
endpoint. For example, an inherited `store = false` conflicts with a Chat
variant; omit it on the base and put it only on the Responses variant instead.

## Variants and inheritance

Variant entries create absent IDs and sparsely update existing catalog variants.
Generation and adaptor fields inherit in order: catalog base, model overrides,
catalog variant, user variant. Omitted fields inherit, explicit `false` survives,
and lists replace (`[]` clears). Reasoning omission preserves the prior value;
a supplied tagged reasoning object replaces it as a whole. Headers merge by
normalized name, preserving empty-string deletion markers through composition.

Variants accept `display_name`, `model_id`, `generation_options`, `adaptor_options`,
`reasoning`, `headers`, and optional `enabled`. `enabled = false` removes an
existing variant or is a no-op for an absent ID; other fields alongside it are
errors. Existing positions are preserved, and new IDs append in sorted order.
An omitted or empty map preserves catalog variants. Named defaults must exist
and be enabled after composition. A new unnamed display label is derived from
its ID. Custom variants have no catalog layer, so editing shared base settings
changes every variant that does not override those fields.

`default_variant` selects a variant; it is not a generation setting. Omission
does not select the first authored variant. `"base"` explicitly selects the base;
named defaults are resolved after creation, overrides, and disabling.

Reasoning is optional and requires the model's reasoning capability. The tagged
object accepts exactly one of:

| Type | Fields | Constraints |
|---|---|---|
| `effort` | `value` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `default`; not Cohere |
| `toggle` | `enabled` boolean | Anthropic, Gemini, Vertex Gemini, Bedrock Converse, Cohere |
| `budget_tokens` | `value` signed integer | Same families as toggle; final adaptor validation determines supported budgets |

A concrete off toggle or `none` effort is not removal of the reasoning object.
There is no null/clear operation. Example fragment disabling a managed variant:

```toml
[providers.openai.models.gpt-5.variants.low]
enabled = false
```

### File layers and headers

A workspace provider replaces the complete same-ID user provider, including
credentials, models, pricing, and variants. The sparse rules above apply only
inside that effective definition. Headers use the separate
[Request Header](../engine/headers.md) composition and ownership rules; their
values are public metadata, not secret-typed storage.

### Breaking configuration migration

Removed keys fail without aliases: `model_overrides` becomes `models`, model and
variant `defaults` becomes `generation_options`, and `options` becomes
`adaptor_options`. Replace `shape` and the ineffective `api_path` with
`adaptor_options.request_endpoint`. Remove authored `capabilities.cancellation`.
Replace variant `operation` directives with sparse entries or `enabled = false`.
Move top-level pricing to the model's [pricing table](#model-pricing).

## Prompt caching

Prompt-cache policy belongs to the provider definition. Omit `cache` to use the
family defaults. A cache table is validated against the provider's resolved
adaptor; wrong-family fields and unknown keys are load errors.

The following are fragments for already-defined providers, not a complete file:

```toml
[providers.anthropic.cache]
system = "1h"
tools = "1h"
rolling = "5m"

[providers."custom.bedrock".cache]
system = "5m"
tools = "5m"
rolling = "5m"

[providers.openai.cache]
mode = "auto"
ttl = "30m"
system = true
rolling = true

[providers."custom.compatible".cache]
prompt_cache_key = "tenant-${session_id}"
```

Here `custom.bedrock` is an existing custom `aws-bedrock-converse` provider.
A managed provider can contain models routed through different adaptor families;
a shared cache table must validate for its resolved models. In particular, do
not apply a Converse-only cache policy to a mixed Bedrock catalog provider that
also routes models through Responses. Use a custom provider for explicit
family-specific policy when the managed definition cannot share it.

Anthropic defaults to `system = "1h"`, `tools = "1h"`, and `rolling = "5m"`.
`system` and `tools` accept `"1h"`, `"5m"`, or `"off"`; `rolling` accepts
`"5m"` or `"off"`. Marker order is tools, system, then rolling history, and a
one-hour marker cannot follow a five-minute marker. Explicit `"1h"` placement
requires the model's authored Anthropic `beta` options to contain
`extended-cache-ttl-2025-04-11`; Cookie agent never inserts an
`anthropic-beta` value on the user's behalf.

Bedrock has the same three fields and ordering rule. All three default to
`"5m"`; setting all three to `"off"` emits no cache points. Rolling always marks
the last non-system message. There is no `enabled` switch and no indexed
`messages` list. System and tool points are omitted from requests without
eligible system text or tools. The three structural placements stay below
Bedrock's four-point request limit.

Anthropic and Bedrock use structural markers. The selected policy is frozen for
the run, prior markers are cleared, and ordinary and compaction requests resolve
the same system, tools, and last-non-system placements. Compaction shares the
parent's cached prefix instead of creating an isolated cache namespace.

OpenAI and Azure OpenAI always send the session's bare UUIDv7 as
`prompt_cache_key`; this has no configuration field or opt-out. Their cache table
accepts `prompt_cache_retention`, `mode`, `ttl`, `system`, and `rolling`.
`prompt_cache_retention` is `"in_memory"` or `"24h"`. Setting `mode` or `ttl`, or
setting `system` or `rolling` to true, enables GPT-5.6 controls. Both placements
default to false; omitted `mode` and `ttl` then default to `"auto"` and `"30m"`.
The only accepted `ttl` is `"30m"`. `mode = "explicit"` disables the
provider-managed breakpoint, so `system = false` and `rolling = false` produce
zero cache writes. There is no tools placement.

Custom `openai-compatible` providers accept only `prompt_cache_key`. Omission
defaults to `${session_id}`, an empty string disables the key, and another value
is sent verbatim after `${session_id}` expansion. The expanded value must be at
most 64 characters. Other template variables are rejected.

Gemini caching is always implicit and has no configuration surface. A `cache`
table on Google Gemini or Vertex Gemini providers is an error. Cohere caching is
server-side automatic and likewise has no authored cache table.

Provider behavior changes independently of Cookie agent. See the official
[OpenAI prompt-caching guide](https://developers.openai.com/api/docs/guides/prompt-caching),
[Gemini context-caching guide](https://ai.google.dev/gemini-api/docs/generate-content/caching),
[Amazon Bedrock prompt-caching guide](https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html),
[Cohere API reference](https://docs.cohere.com/v2/reference/chat), and
[Anthropic prompt-caching guide](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching)
for current model support, thresholds, retention, and billing.

### Breaking changes

1. The global `[prompt_caching]` table and all `[prompt_caching.<family>]` subtables are removed; use `[providers.<id>.cache]`.
2. Anthropic and Bedrock TTLs accept only `"1h"`, `"5m"`, and `"off"`. The former `"one_hour"` and `"five_minutes"` values are removed. Configuration is strict: any other TTL string (for example `"automatic"`, `"short"`, `"long"`, `"standard"`, `"none"`, or `"ephemeral"`) was never a cookie-agent literal and is a hard error, not an alias.
3. Google cache configuration is removed, including `mode` (`"implicit"`, `"explicit"`, or `"off"`) and `cached_content`; Gemini caching is always implicit.
4. Bedrock now uses only `system`, `tools`, and `rolling`. The `enabled` field, `messages` list, and each message entry's `history_index` and `ttl` fields are removed.
5. First-party OpenAI and Azure cache tables no longer accept `prompt_cache_key`; the bare session UUIDv7 is always sent. OpenAI-compatible providers own that field, default to `${session_id}`, and use `prompt_cache_key = ""` to disable it.
6. OpenAI cache `mode = "implicit"` is removed; use `mode = "auto"`. `mode = "explicit"` remains supported.

## Model pricing

Pricing overrides are empty by default. Managed models use prices from the
selected models.dev catalog. Add an entry for a custom model absent from the
catalog, or to override catalog prices with the provider terms that apply to
your account. Catalog context tiers are selected independently for each request
from its reported input-token count. Both managed and custom models use this
model-local table. Rates are validated even for disabled or unavailable models;
managed model IDs must still exist in the catalog.

```toml
[providers."custom.example".models."model-name".pricing]
input_per_million_usd = "1.25"
output_per_million_usd = "5.0"
```

All fields are optional quoted, finite, nonnegative decimal USD rates per
million tokens. Quoting preserves the authored decimal exactly.

Rates use fixed-point precision of 12 decimal places in USD per million tokens,
stored as an unsigned 128-bit scaled integer. Scientific notation is accepted
when exactly representable. Negative values, non-finite values, overflow, and
nonzero precision below that scale are rejected, not rounded. Pricing has no
variant override. The example above is a fragment for an existing model.

| Key | Applies to |
|---|---|
| `input_per_million_usd` | Input tokens not attributed to cache reads or writes |
| `output_per_million_usd` | Plain, non-reasoning output tokens |
| `reasoning_per_million_usd` | Reasoning output tokens; falls back to the output rate when omitted |
| `cache_read_per_million_usd` | Provider-reported cache-read input tokens |
| `cache_write_per_million_usd` | Provider-reported cache-write input tokens |

Precedence is config override, then catalog price, then no estimate. A config
entry replaces the catalog rate set for that model. When a selected rate set
does not distinguish cache reads or writes, those tokens use its plain input
rate; reasoning tokens similarly use its output rate when no reasoning rate is
present. An estimate is returned only when the selected source prices every
nonzero observed category and the provider reports every split needed by a
distinct rate. A session, agent, or global total is unpriced if any
model within it is unpriced. These are estimates from provider-reported token
counts, not invoices.

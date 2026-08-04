# Agent, Provider, Model Variant, and Assistant-Turn Redesign

**Status:** accepted implementation specification
**Implementation policy:** current-only; no compatibility readers, aliases,
migrations, or dual paths
**Required versions:** config schema 6, agent-document schema 1, protocol 7,
event schema 7, session JSONL 7, session metadata 7, delegation-journal schema 7

This document is the detailed implementation contract for the accepted future
architecture described in [ARCHITECTURE.md](../ARCHITECTURE.md). Where this
document is more specific, its strict types, validation rules, ordering, and
failure behavior are mandatory.

## 1. Required user-visible results

1. Workspace configuration is `<cwd>/.cookie-agent/`; `.cookie_agent/` is
   never inspected, migrated, warned about, renamed, or deleted.
2. `.cookie-agent/config.toml` is provider-centric. It contains providers,
   included models, model variants, and non-agent runtime settings. It contains
   no agents, agent prompts, fallback chains, agent tools, permissions, or
   delegation policy.
3. Agents are strict Markdown documents at
   `.cookie-agent/agents/<agent-id>.md`, with YAML frontmatter schema 1 and a
   required nonempty Markdown body.
4. Fallback entries use direct `provider/model-id` keys and one of three
   variant-authoring states: omitted, explicit `base`, or a named variant.
5. Variants are request-behavior presets beneath one model, not separate
   models. They can be generated from pinned models.dev reasoning metadata or
   explicitly added, replaced, disabled, and selected as a model default.
6. The visible assistant header text is exactly
   `Agent(provider/model[variant])`. It identifies the frozen producing agent
   and exact model selection; base is explicit as `[base]`, and a named variant
   is preserved exactly, including `[default]`.
7. The Message panel title uses the same canonical model-selection text. Agent
   and model selection are separate hit regions, but variant is part of the
   model selection and has no separate picker.

In those forms, `Agent` is the exact `AgentId`; `provider/model` is the exact
`ModelKey`; and the bracketed value is the exact variant or `base`. They are
placeholders, not literal words. There is no additional textual `ASSISTANT`
prefix. Rendering never parses identity back from the combined string: each
portion remains structured data.
8. Thinking and tool calls are ordered, collapsible child segments inside
   their owning assistant turn. There are no standalone `REASONING` or `TOOL`
   transcript blocks.
9. Compact tool rows show a safe primary argument when available, for example
   `bash touch README.md`. Running adds `…`; success adds no suffix; failures,
   cancellation, and interruption use concise textual markers. `COMPLETED` is
   never rendered.
10. Agents-panel rows are `agent-id:session-title`, patch immediately from
    title events, and retain the original hierarchy root while a child is
    watched. Only the Sessions picker reroots.
11. Agents-panel text height is exactly
    `clamp(visible_tree_row_count, 1, 3)`. Borders are outside that text-row
    count.
12. Conversation and Message border titles contain no drag-scrollbar or hotkey
    prose. Existing scrolling, multiline-input, approvals, Markdown rendering,
    syntax highlighting, diagnostics, and stable-tree behavior otherwise
    remain.

## 2. Layout, layering, and atomic replacement

```text
<cwd>/.cookie-agent/
  config.toml
  agents/
    primary.md
    worker.md

~/.config/cookie_agent/
  config.toml
  agents/
    <agent-id>.md
```

Precedence is:

```text
built-in runtime defaults < user config.toml < workspace config.toml
user agent document < workspace agent document with the same AgentId
```

There is no upward workspace search: only the process's exact canonical cwd is
anchored and `<cwd>/.cookie-agent/` is considered. `attach` and `connect` do
not inspect cwd configuration.

Provider definitions and agent documents are atomic layer replacements. If a
workspace config defines provider `openai`, it replaces the entire user
provider `openai`; fields, maps, arrays, model tables, and variant tables are
not merged across those two definitions. A workspace agent document replaces
the complete user document with the same ID. Other top-level runtime sections
follow their declared schema-6 replacement semantics; arrays always replace.

## 3. Shared identities and selections

`crates/identity` owns the strict identities used by config, models, protocol,
engine, tools, server, CLI, and TUI:

```rust
pub struct AgentId(String);          // lowercase kebab-case, 1..=64 bytes
pub struct ProviderId(String);       // lowercase [a-z0-9._-], 1..=128 bytes
pub struct ProviderModelId(String);  // visible UTF-8, no '/', 1..=384 bytes
pub struct ModelKey(String);         // ProviderId + '/' + ProviderModelId
pub struct VariantId(String);        // lowercase [a-z0-9._-], 1..=64 bytes

pub struct ModelSelection {
    pub model: ModelKey,
    pub variant: Option<VariantId>, // None is exact base behavior
}

pub enum ConfiguredVariantRef {
    Base,
    Named(VariantId),
}

pub enum ConfiguredModelDefault {
    Base,
    Named(VariantId),
}
```

Agent fallback stores `variant` as `Option<ConfiguredVariantRef>` and has three
document states separate from `ConfiguredModelDefault`:

- `None` means the `variant` field was omitted and selects the provider model's
  already-resolved default selection;
- `Some(Base)` comes from `variant: base` and selects exact base;
- every other valid string, including `default`, is
  `Some(Named(VariantId(string)))`.

Thus a generated variant whose ID is `default` remains addressable and is not
confused with omission. `base` is reserved and cannot be a `VariantId`.
`ModelKey` is at most 512 bytes, splits at the first `/`, and has valid nonempty
segments. All wire identities are strict, bounded, schema-generating newtypes.
Canonical `ModelSelection` formatting is `provider/model[variant]`; `None`
formats as `[base]`, while a named variant formats with its exact ID. Formatting
does not alter the structured `{ model, variant }` serialization or protocol
schema.

`ConfiguredModelDefault` is a separate provider-config type and is always held
as `Option<ConfiguredModelDefault>`:

- `None` means the `default_variant` field was omitted and retains the provider
  model's source default selection;
- `Some(Base)` comes only from explicit `default_variant = "base"` and selects
  exact base behavior;
- `Some(Named(id))` comes from every other valid string, including `default`,
  and selects that named variant.

The provider model source default is established before config directives:
models.dev uses an explicitly declared default from the pinned source/recipe or
base when none exists; an explicit provider's source default is base. After
variant directives, `ConfiguredModelDefault` resolves to exact
`ModelSelection { model, variant: Option<VariantId> }`. No omitted/base/named
marker remains in frozen state. This type and resolution are independent from
the agent fallback `ConfiguredVariantRef` states above.

## 4. Provider-centric config schema 6

The only top-level fields are:

```rust
pub struct RuntimeConfig {
    pub schema_version: ConfigSchemaVersion, // exactly 6
    pub server: ServerConfig,
    pub tool_output: ToolOutputConfig,
    pub approval: ApprovalConfig,
    pub context_compaction: ContextCompactionConfig,
    pub session_title: SessionTitleConfig,
    pub providers: BTreeMap<ProviderId, ProviderDefinition>,
}

pub enum ProviderDefinition {
    ModelsDev(ModelsDevProvider), // source = "models_dev"
    Explicit(ExplicitProvider),   // source = "explicit"
}
```

Top-level `agents`, `models`, `permissions`, `profiles`, and
`internal_agents` are unknown fields and fail parsing.

`schema_version` and `providers` are required; `providers` must be nonempty.
`server`, `tool_output`, `approval`, `context_compaction`, and `session_title`
are optional sections whose omitted values are the documented built-in runtime
defaults. Therefore the schema-6 example below is complete and valid even
though it omits those non-provider sections other than `server`.

### 4.1 Common strict provider fields

```rust
pub struct ModelsDevProvider {
    pub source: ModelsDevTag, // required, exactly "models_dev"
    pub catalog_revision: CatalogRevision, // required
    pub endpoint: Option<EndpointUrl>, // optional, default None/source endpoint
    pub adaptor: Option<AdaptorId>, // optional, default None/source adaptor
    pub auth: AuthDefinition, // required
    pub headers: BTreeMap<HeaderName, SecretString>, // optional, default {}
    pub models: BTreeMap<ProviderModelId, ModelsDevModelConfig>, // required, nonempty
}

pub struct ExplicitProvider {
    pub source: ExplicitTag, // required, exactly "explicit"
    pub endpoint: EndpointUrl, // required
    pub adaptor: AdaptorId, // required
    pub auth: AuthDefinition, // required
    pub headers: BTreeMap<HeaderName, SecretString>, // optional, default {}
    pub models: BTreeMap<ProviderModelId, ExplicitModelConfig>, // required, nonempty
}
```

Every struct and tagged enum uses deny-unknown-fields decoding. Duplicate TOML
keys, duplicate case-insensitive header names, invalid header names/values,
URL userinfo, URL fragments, and endpoint query credentials are rejected.
Endpoints must be HTTPS except explicit loopback HTTP accepted by an adaptor
that declares loopback HTTP support. Transport-controlled headers such as
`host`, `content-length`, `transfer-encoding`, `connection`, and provider auth
headers owned by the selected auth form cannot be supplied in `headers`.

Supported `AdaptorId` values are the reviewed constructors listed in
[provider-conformance.md](provider-conformance.md). An adaptor is a wire
protocol choice, never inferred from a model ID. A models.dev provider normally
uses the pinned recipe adaptor; an explicit `adaptor` is legal only when it is
one of that recipe's reviewed alternatives. An endpoint override is legal only
when the recipe marks the endpoint overridable. Otherwise either field fails
validation.

### 4.2 Authentication

```rust
pub enum AuthDefinition {
    None,
    CredentialStore,
    Bearer { token: SecretString },
    ApiKey { key: SecretString, header: Option<HeaderName> },
    Basic { username: SecretString, password: SecretString },
    AwsSdk,
    GoogleAdc,
    Fields { values: BTreeMap<AuthFieldName, SecretString> },
}
```

The wire `type` values are `none`, `credential_store`, `bearer`, `api_key`,
`basic`, `aws_sdk`, `google_adc`, and `fields`. Unknown fields and empty secret
values fail. `credential_store` is allowed only for models.dev providers and
uses exactly the credential fields named by the pinned recipe. Missing stored
credentials leave that provider's configured models unavailable (and dependent
agents non-runnable) until `provider.connect`; this is the only unresolved-auth
setup state that does not fail configuration loading. Once construction is
attempted, every required field must be present.
`fields` is accepted only when the adaptor declares an exact semantic auth
field schema; unknown, missing, or extra field names fail. `api_key.header`, if
omitted, uses the adaptor's documented header and never a model-name heuristic.
Auth/adaptor combinations not explicitly supported fail startup.

The auth `type` field is always required. `none`, `credential_store`,
`aws_sdk`, and `google_adc` accept no additional fields. Bearer `token`, API-key
`key`, both Basic fields, and the complete adaptor-declared `fields.values` map
are required. Only API-key `header` is optional, with the adaptor header as its
default. No secret field has an empty or implicit value.

### 4.3 Model inclusion, capabilities, defaults, and options

Only model IDs present in a provider's `models` map are included in the
runnable model set. The map must be nonempty. `enabled = false` excludes that
entry without making it selectable. For a models.dev provider, every included
ID must exist in the exact pinned snapshot and have a reviewed construction
recipe. For an explicit provider, each model is constructed only through the
provider's declared adaptor.

```rust
pub struct ModelsDevModelConfig {
    pub enabled: bool, // default true
    pub display_name: Option<String>, // optional, default source display name
    pub defaults: RequestDefaults, // optional table, default {}
    pub options: ProviderOptions, // optional table, default {}
    pub variants: BTreeMap<VariantId, VariantDirective>, // optional, default {}
    pub default_variant: Option<ConfiguredModelDefault>, // optional field; None = omitted
}

pub struct ExplicitModelConfig {
    pub enabled: bool, // default true
    pub display_name: String, // required
    pub capabilities: ModelCapabilities, // required
    pub defaults: RequestDefaults, // optional table, default {}
    pub options: ProviderOptions, // optional table, default {}
    pub variants: BTreeMap<VariantId, VariantDirective>, // optional, default {}
    pub default_variant: Option<ConfiguredModelDefault>, // optional field; None = omitted
}

pub struct ModelCapabilities {
    pub input: BTreeSet<Modality>,       // text | image | audio | pdf
    pub output: BTreeSet<Modality>,      // text | image | audio
    pub context_tokens: u64,
    pub output_tokens: u64,
    pub tool_calling: bool,
    pub parallel_tool_calls: bool,
    pub structured_output: bool,
    pub reasoning: bool,
    pub temperature: bool,
    pub top_p: bool,
    pub seed: bool,
    pub native_replay: ReplayCapability,       // unsupported | optional | required
    pub native_compaction: CompactionCapability, // unsupported | optional | required
    pub cancellation: CancellationCapability,  // local_only | provider
    pub media: BTreeMap<MediaKind, MediaCapability>,
}

pub struct MediaCapability {
    pub mime_types: BTreeSet<MimeType>,
    pub max_bytes: u64,
    pub max_count: u32,
}

pub struct RequestDefaults {
    pub temperature: Option<FiniteF32>,       // 0.0..=2.0
    pub top_p: Option<FiniteF32>,             // 0.0..=1.0
    pub max_output_tokens: Option<u64>,       // 1..=capability output_tokens
    pub stop: Vec<String>,                    // at most 8, each 1..=256 bytes
    pub seed: Option<i64>,
    pub tool_choice: Option<ToolChoice>,      // auto | none | required | named
}

pub enum ReasoningBehavior {
    Effort { value: ReasoningEffort },
    Toggle { enabled: bool },
    BudgetTokens { value: i64 }, // -1 or nonnegative
}

pub struct ResolvedRequestDefaults {
    pub request: RequestDefaults,
    pub reasoning: Option<CompiledReasoningBehavior>,
}

pub enum ProviderOptions {
    Anthropic { api_version: Option<String>, beta: Vec<String> },
    OpenAiChat { organization: Option<String>, project: Option<String> },
    OpenAiResponses { organization: Option<String>, project: Option<String>, store: Option<bool> },
    OpenAiCompatible { api_path: Option<String> },
    GoogleGemini { api_version: Option<String> },
    GoogleVertexGemini { project: String, location: String },
    AwsBedrockConverse { region: String },
    AzureOpenAiChat { deployment: String, api_version: String },
    AzureOpenAiResponses { deployment: String, api_version: String },
    CohereV2Chat { api_version: Option<String> },
    OpenResponses { protocol_mode: OpenResponsesMode },
}
```

`ModelsDevModelConfig` has no authorable `capabilities` field. Its complete
capabilities are derived from the exact pinned model record plus reviewed
recipe/compiler; attempting to add a capabilities table is an unknown-field
error. `ExplicitModelConfig.capabilities` is required and complete: every field
shown in `ModelCapabilities`, including every boolean, limit, capability enum,
and the `media` map, must be written even when false, unsupported, or empty.
There are no implicit capability booleans.

Capability sets must be nonempty, limits positive, and `output_tokens` must not
exceed `context_tokens`. `parallel_tool_calls = true` requires
`tool_calling = true`; when tool calling is false it must be explicitly false.
The required `seed` boolean says whether request seed is supported and is
independent from an authorable default seed. The required `media` map is empty
when input is text-only. Each declared non-text input modality (`image`,
`audio`, or `pdf`) requires exactly one same-kind media entry with a nonempty
MIME set and positive `max_bytes`/`max_count`; undeclared input modalities and
all output modalities must not have media entries. Explicit capabilities must
be honestly implementable by the selected adaptor.

`ReasoningEffort` is exactly `none|minimal|low|medium|high|xhigh|max|default`;
reasoning off is represented by a supported toggle or the catalog null mapping,
not an extra effort value. `ToolChoice::Named` contains one validated tool name.

`ProviderOptions` is selected by and must match the provider's `AdaptorId`.
Every optional string is nonempty and bounded to 512 bytes; `beta` has at most
32 unique entries; project/location/region/deployment/api-version values are
bounded to 256 bytes. `api_path` must be an absolute URL path with no query or
fragment. `OpenResponsesMode` is the adaptor's strict `standard|compact` enum.
There is no arbitrary JSON/TOML body or unknown option map. Unsupported
normalized defaults, duplicate semantic settings across defaults/options, and
capability/default contradictions fail provider construction.

Within an `options` table, Anthropic `api_version`, OpenAI
organization/project, Responses `store`, compatible `api_path`, Gemini/Cohere
`api_version` are optional and default to None/adaptor behavior; Anthropic
`beta` defaults to `[]`. Vertex project/location, Bedrock region, and Azure
deployment/api-version are required when those adaptors are selected.
Open Responses `protocol_mode` defaults to `standard`. Because the whole
`options` table defaults to `{}`, omission is valid only when all options
required by the selected adaptor can be supplied by a reviewed models.dev
recipe; otherwise semantic validation reports the missing required option.

For models.dev entries, pinned recipe defaults/options are the baseline.
Configured `RequestDefaults` replace only fields explicitly present (the stop
array replaces as a whole); configured options may replace only recipe fields
marked configurable. For explicit entries, omitted defaults mean no request
default and every adaptor-required option is mandatory. A variant's final
behavior starts from the model's final base defaults/options and applies its
complete directive payload as an overlay; `add`/`replace` refer to variant-map
identity, not to field-merging with an earlier variant definition.

The authorable `RequestDefaults` fields have these omission defaults:
`temperature = None`, `top_p = None`, `max_output_tokens = None`, `stop = []`,
`seed = None`, and `tool_choice = None`. Setting a field whose capability is
false fails. `tool_choice` requires tool calling; a named choice must name an
exposed tool. The model-level `defaults`, `options`, and `variants` tables and
provider-level `headers` table default to empty when omitted. Empty provider
`models` maps are invalid; model `enabled` defaults true; no other boolean has
an implicit value.

Reasoning has exactly one authorable source: `VariantDirective.reasoning`.
`RequestDefaults` has no reasoning field, and every adaptor-specific
`ProviderOptions` schema rejects reasoning/effort/thinking/budget aliases.
Generated models.dev variants populate the same semantic directive slot.
After compilation only, `ResolvedRequestDefaults` combines ordinary request
defaults with `CompiledReasoningBehavior`; it is internal/frozen output and is
not a config schema. Duplicate or alternate reasoning authoring fails.

`default_variant: Option<ConfiguredModelDefault>` follows section 3 exactly.
`None` retains the provider model source default; `Some(Base)` explicitly
selects exact base; `Some(Named(id))` selects an enabled final variant. A source
or named default removed by directives without an explicit valid replacement
fails. Resolution produces exact `ModelSelection` before model snapshots,
fallback entries, fingerprints, or events are frozen.

### 4.4 Variants directives and atomic publication

```rust
pub enum VariantDirective {
    Add {
        display_name: Option<String>, // optional, default ID-derived name
        defaults: RequestDefaults, // optional table, default {}
        options: ProviderOptions, // optional table, default {}
        reasoning: Option<ReasoningBehavior>, // optional, default None
    },
    Replace {
        display_name: Option<String>, // optional, default ID-derived name
        defaults: RequestDefaults, // optional table, default {}
        options: ProviderOptions, // optional table, default {}
        reasoning: Option<ReasoningBehavior>, // optional, default None
    },
    Disable,
}
```

Wire operations are `add`, `replace`, and `disable`. `add` requires that the ID
is absent after generation and earlier directives. `replace` requires that the
ID already exists and replaces it completely; it never field-merges. `disable`
requires an existing ID, removes it, and permits no other fields. Directives
are applied in lexicographic `VariantId` order after generation. Base behavior
cannot be targeted. A default variant is selected by the model-level
`default_variant`, not a flag repeated inside variant tables.

Every variant table requires `operation`. Add/replace omission defaults are the
comments above; disable accepts only `operation = "disable"`. Any reasoning
field outside `VariantDirective.reasoning`, including under defaults or options,
is rejected as unknown or duplicate semantic authoring.

The loader constructs and validates an entire provider candidate—including
auth shape, endpoint, every enabled model, capabilities, base defaults,
options, generated variants, directives, defaults, concrete adapters, and all
fingerprints—before publication. Refresh uses one atomic provider-snapshot
replacement. A failure leaves the previously published provider/model snapshot
unchanged; partial model publication is forbidden.

### 4.5 Example

```toml
schema_version = 6

[server]
host = "127.0.0.1"
port = 7419

[providers.openai]
source = "models_dev"
catalog_revision = "sha256:d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a"
auth = { type = "credential_store" }

[providers.openai.models."gpt-5.6-sol"]
default_variant = "high"

[providers.openai.models."gpt-5.6-sol".variants.high]
operation = "replace"
reasoning = { type = "effort", value = "high" }

[providers.quantumcookie]
source = "explicit"
endpoint = "https://llm-api.quantumcookie.xyz/v1"
adaptor = "openai-compatible"
auth = { type = "bearer", token = "${env:COOKIE_TEST_API_KEY}" }

[providers.quantumcookie.models."deepseek-v4-flash"]
display_name = "DeepSeek V4 Flash"

[providers.quantumcookie.models."deepseek-v4-flash".capabilities]
input = ["text"]
output = ["text"]
tool_calling = true
parallel_tool_calls = true
structured_output = false
reasoning = true
temperature = true
top_p = true
seed = true
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
context_tokens = 131072
output_tokens = 16384
media = {}
```

## 5. Strict Markdown agent documents

```markdown
---
schema: 1
description: General implementation agent
mode: primary
enabled: true
model_fallback:
  - { model: "anthropic/claude-sonnet-4-6", variant: high }
  - { model: "openai/gpt-5.6-sol" }
  - { model: "quantumcookie/deepseek-v4-flash", variant: base }
tools: [read, grep, glob, write, edit, bash]
permissions:
  - { id: allow-read, action: read, resource: "*", effect: allow }
  - { id: ask-write, action: write, resource: "*", effect: ask }
  - { id: ask-bash, action: bash, resource: "*", effect: ask }
delegation:
  agents: [worker]
  max_depth: 3
---
You are the primary implementation agent.
```

The basename is the `AgentId`; frontmatter has no `id` field.

```rust
pub struct AgentFrontmatter {
    pub schema: AgentSchemaVersion, // exactly 1
    pub description: String,
    pub mode: AgentMode,            // primary | subagent | all
    pub enabled: bool,
    pub model_fallback: Vec<AgentModelFallback>,
    pub tools: Vec<ToolName>,
    pub permissions: Vec<PermissionRule>,
    pub delegation: Option<AgentDelegationConfig>,
}

pub struct AgentModelFallback {
    pub model: ModelKey,
    pub variant: Option<ConfiguredVariantRef>, // None = omitted/provider default
}
```

The normalized Markdown body is required and must contain at least one
non-whitespace Unicode scalar. It replaces the generic agent system prompt;
there is no prepended generic prompt. The engine appends no environment,
repository, cwd, date, tool, or project metadata to the system prompt in this
design. Tool schemas are request tools, and delegated task/context is a user
message, not system-prompt text. Consequently the complete composed prompt is
exactly the normalized body. The complete prompt and its domain-separated
SHA-256 fingerprint are frozen and persisted in `AgentSnapshot` before a run
starts.

Parsing is strict: exact schema 1; unknown and duplicate fields rejected; YAML
tags, aliases, anchors, merge keys, executable references, includes, and all
interpolation rejected. Documents are UTF-8, at most 256 KiB; frontmatter is at
most 128 KiB; body is at most 128 KiB; description is 1..=512 bytes; lists are
at most 256 entries; nesting is at most 16. Newlines normalize to LF and the
body has exactly one final LF for fingerprinting.

Every `primary` agent requires a nonempty fallback chain, including a disabled
primary document. `subagent` and `all` agents may have an empty chain; that
empty chain means inherit only when the agent is invoked through delegation.
Any empty-chain agent has `runnable_as_root = false`. A `subagent` is never
runnable as root. An `all` agent remains root-selectable only when enabled and
its own configured chain is nonempty. A primary is root-selectable only when
enabled and at least one selection in its required chain is available.

Every nonempty fallback chain must contain each `ModelKey` at most once.
Duplicate model keys are a startup error, regardless of differing variant
references, so model-based suffix selection is unambiguous. Unknown or disabled
models, variants, tools, actions, rule effects, and delegation targets fail
startup. A known enabled models.dev model blocked only by missing
credential-store fields remains a resolved-but-unavailable entry and makes the
agent non-runnable until connection; it is not treated as unknown.

## 6. Fallback resolution and freezing

Every authored fallback entry resolves before freezing to exactly one
`ModelSelection`:

- `None`/omitted: use the provider model's resolved default selection, including
  `variant = None` when that source/configured default is base;
- `Some(Base)`: use `variant = None` even if the model has a named default;
- `Some(Named(id))`: require that enabled variant and use
  `variant = Some(id)`.

No unresolved omitted/base/named marker enters a frozen snapshot, event,
fingerprint, or request. A run selection is:

```rust
pub struct RunSelection {
    pub agent: AgentId,
    pub model: ModelSelection,
}

pub struct AvailableVariantDescriptor {
    pub id: VariantId,
    pub display_name: String,
    pub origin: VariantOrigin,
    pub behavior_fingerprint: Sha256Digest,
}

pub struct AvailableModelDescriptor {
    pub key: ModelKey,
    pub display_name: String,
    pub capabilities: ModelCapabilities,
    pub variants: Vec<AvailableVariantDescriptor>,
    pub default_variant: Option<VariantId>,
    pub behavior_fingerprint: Sha256Digest,
}

pub struct AgentDescriptor {
    pub id: AgentId,
    pub description: String,
    pub mode: AgentMode,
    pub enabled: bool,
    pub runnable_as_root: bool,
    pub resolved_fallback: Vec<ModelSelection>,
    pub tools: Vec<ToolName>,
    pub delegation_targets: Vec<AgentId>,
}

pub struct SessionCreateParams {
    pub selection: RunSelection,
}

pub struct RunStartParams {
    pub session_id: SessionId,
    pub client_run_id: ClientRunId,
    pub selection: RunSelection,
    pub input: String,
}
```

Root selection uses one coherent model snapshot for catalog publication,
model/variant validation, authored-entry availability, and plan construction.
The public root catalog contains every configured, enabled, currently
executable model in strict `ModelKey` order. Credential-blocked unavailable
models remain internal resolved entries but are omitted from public model
descriptors.

The exact selected model/variant may be any public catalog entry. When its
`ModelKey` occurs in the resolved authored chain, the root plan contains the
exact requested head followed by the available authored tail after that entry;
the requested variant never falls back to the authored/default head variant.
When the selected key is outside the authored chain, the root plan contains a
synthetic exact head followed by all available authored fallback entries.
Unavailable authored entries are skipped in both cases. Root eligibility is
still based on the authored chain being nonempty and containing at least one
available entry.

Delegated semantics are unchanged: a configured-chain child uses the unique
authored suffix, replacing only its head with the exact delegated selection and
preserving its authored tail; fallback advances and never wraps. An empty-chain
child inherits the parent's active frozen suffix as specified in section 9.

Before `RunStarted`, freeze the complete `AgentSnapshot`: identity, mode,
description, source/document fingerprint, complete composed prompt and prompt
fingerprint, tools, ordered permissions, delegation policy, exact resolved
fallback bindings/defaults/fingerprints, and selected suffix. Retries,
fallback, tool-loop passes, compaction, title work, approval work, and replay
reconstruction use that snapshot. Internal engine work inherits the owning
run's currently active exact frozen suffix unless its documented policy says
otherwise; it never re-resolves live config.

## 7. Models.dev variant generation and compilation

The only recognized models.dev reasoning-option forms are `effort`, `toggle`,
and `budget_tokens`. Unknown forms or fields fail an included model. No variant
is inferred from a model name.

### 7.1 Effort

Effort accepts only the ordered values `none`, `minimal`, `low`, `medium`,
`high`, `xhigh`, `max`, `default`, and `null`. Each non-null value generates a
same-ID variant with that exact semantic effort. `null` generates `off` and
means the actual upstream null token and the adaptor's honest
reasoning-disabled encoding; a string containing `"null"` is rejected.
Duplicate values are rejected.

### 7.2 Toggle

Toggle generates `off` and `on`. `off` must compile to an explicit supported
disabled encoding, not omission when omission means provider default-on. `on`
must compile to the adaptor's supported enabled/default reasoning encoding.

### 7.3 Budget tokens

Pinned models.dev `budget_tokens` has only optional `min` and `max` fields; any
other field is unknown and fails. `min` is `-1` (automatic) or a nonnegative token count; `max`, when
present, is a nonnegative token count. `min = -1` generates only
`budget-auto`; finite `min` generates `budget-min`; present `max` generates
`budget-max`. An absent bound generates nothing, and no other budget ID is
generated. Finite `min > max` fails. Equal finite bounds may produce distinct
`budget-min` and `budget-max` IDs with identical behavior; IDs remain distinct.

Reviewed recipe metadata may separately define base request behavior or a
provider model source default. Such recipe metadata is not a
`reasoning_options.budget_tokens` field and never creates an additional budget
variant ID.

### 7.4 Multiple options and collisions

Multiple options generate a deterministic union, not a Cartesian product.
Source options are normalized in snapshot order, but generated-map ordering is
by `VariantId`. An explicit provider directive has highest precedence and may
replace or disable any generated ID. Among generated options, ID precedence is
`effort` over `toggle` over `budget_tokens`; a lower-precedence collision is
discarded only when its compiled normalized behavior is byte-for-byte equal.
Different behavior for the same generated ID fails the included model. Equal
precedence duplicate IDs also fail. Combined behaviors require one explicit
replacement variant and must be honestly supported by the adaptor.

### 7.5 Honest adapter compiler

Each base model and variant is compiled at provider construction by an
adaptor-specific compiler into internal `ResolvedRequestDefaults` plus strict
typed provider options. Compilation must prove that the adaptor can encode every
requested setting and distinguish it from base behavior. Unsupported effort,
toggle-off, token budget, conflicting settings, or lossy mapping fails the
included model/provider atomically. The loader never silently drops a variant,
renames it, approximates it, or advertises behavior the request encoder cannot
produce.

`ModelVariant` and `ModelEntry` retain exact behavior fingerprints. Variant
identity participates in frozen bindings, selection fingerprints, fallback,
native replay/context scope, persistence, RPCs, diagnostics, and attribution.

## 8. Permissions, guards, exposure, and approvals

```rust
pub struct PermissionRule {
    pub id: RuleId,
    pub action: PermissionAction,
    pub resource: WildcardPattern,
    pub effect: PermissionEffect,
}

pub enum PermissionAction {
    Read, Write, Bash, Grep, Glob, Delegate, ExternalDirectory,
}

pub enum PermissionEffect { Allow, Ask, Deny }
```

Wire values are the lowercase names above. Rules are evaluated in document
order and the last matching rule wins. No match is `Ask`. Rule IDs are unique
within one agent and all fields are strict.

The wildcard grammar is not a filesystem glob: `*` matches zero or more of any
character including `/`; `?` matches exactly one character; there is no escape,
character class, alternation, or globstar syntax. A terminal literal ` *` is
optional, so `git status *` also matches `git status`.

Action mapping is exact: `read→read`, `write→write|edit`, `bash→bash`,
`grep→grep`, `glob→glob`, and `delegate→delegate`. `external_directory` is a
guard action and never a tool. Bash evaluates each parsed subcommand, falling
back to the whole command when parsing is unsafe. Matched resources are:

| Action | Exact resource string |
|---|---|
| `bash` | each parsed subcommand's normalized source, or the whole normalized command when safe parsing fails |
| `read`, `write` | canonical workspace-relative path using `/`; outside-workspace paths use canonical absolute form |
| `grep` | the exact normalized regular-expression string |
| `glob` | the exact normalized pattern string |
| `delegate` | target `AgentId` |
| `external_directory` | canonical absolute directory boundary ending in `/*` |

For a not-yet-existing write target, preparation resolves the nearest existing
ancestor without following a final link and appends validated remaining
components. Resource strings are derived from held prepared objects, not from
later path lookup.

Every tool call follows prepare-once → evaluate → approve if needed → execute
the held capability. Preparation is descriptor/handle based, no-follow, and
produces immutable prepared resources plus a domain-separated operation
fingerprint. Approval authorizes only those prepared resources. Execution never
reopens a path or substitutes a resource; replacement, lost capability, or
restart fails closed. Multi-resource aggregation is Deny, then Ask, then Allow.

Built-in guards run before agent rules and cannot be weakened by omission:

- an outside-workspace filesystem resource first evaluates
  `external_directory`, then its `read` or `write` action;
- `.env` and `.env.*` reads default to Ask, except `*.example`;
- prepared paths must remain within the approved canonical boundary;
- a run that proposes the same operation fingerprint four times without
  intervening user input or a successful different operation is denied on the
  fourth proposal as a doom loop and emits `ApprovalDoomLoopDetected`.

Agent rules may make the first two guards stricter or explicitly allow them;
descriptor binding and the doom-loop guard cannot be bypassed by a rule.

Tool exposure is derived from the frozen agent, not from live config. The
frontmatter `tools` list accepts only `read`, `write`, `edit`, `bash`, `grep`,
and `glob`; `delegate` is engine-owned and listing it is a validation error. A
non-delegate tool is exposed only when listed, registered, and not
made impossible by a final unconditional `resource: "*"` Deny with no later
rule for that action. Ask-by-default tools remain exposed and request approval
at invocation. Section 9 exclusively defines delegate exposure.

An Ask creates a revisioned `ApprovalRequested` for the exact operation
fingerprint. Internal policy may allow, deny, or escalate. Only
`ApprovalEscalated` is user-visible/respondable. Responses are
`approve_once`, `approve_tree`, `reject`, or `cancel`, are idempotent by client
response ID, and must match session, approval ID, revision, and fingerprint.
Tree grants are engine-authored and scoped to one delegation root; process-local
filesystem capabilities do not survive restart. Unattended Ask is Deny.

Each child uses only its own frozen ordered permissions. Parent permissions,
approvals, and rules are never copied into the child. A narrowly matching
engine-authored tree grant may apply because it is runtime consent scoped to the
tree, not inherited configured policy.

## 9. Delegation

```rust
pub struct AgentDelegationConfig {
    pub agents: Vec<AgentId>,
    pub max_depth: u32,
}
```

Absence of `delegation` disables delegation. Only the model-visible `delegate`
tool may create a child; no public session-create, client fan-out, workflow, or
other tool can create one. `agents` is a nonempty unique list. Targets must be
enabled agents whose mode is `subagent` or `all`; `primary` targets are invalid.

The root session has depth 0; each child is exactly parent depth + 1.
`max_depth` is an inclusive maximum child depth relative to that root. A root
with `max_depth: 0` cannot delegate; `max_depth: 1` may create children at depth
1 but no descendants. The root's frozen ceiling is authoritative for the whole
tree. A child with its own delegation block receives effective ceiling
`min(parent_effective_ceiling, child.max_depth)`; without a block it cannot
delegate. The engine derives depth/root from durable provenance and rejects
client/tool-supplied values.

The delegate tool is exposed only when the invoking frozen agent has a
delegation block, current depth is below its effective ceiling, and at least one
listed target remains enabled and mode-eligible in the frozen registry. Its
schema enum and agent-list projection contain only those targets. This is an
exposure convenience, not authority: immediately before child reservation the
engine revalidates target membership, enabled/mode eligibility, parent run,
depth, ceiling, permission result, and idempotency against the same frozen
registry. Failure creates no child.

If the configured child agent has a nonempty fallback chain, it resolves and
uses its own chain. If it has an empty chain, the child inherits the invoking
parent run's currently active frozen fallback suffix at delegate admission,
including the active head's exact selected variant and all remaining entries.
It does not inherit the parent's original full chain, entries already exhausted,
or the parent's authored omitted/default markers. The inherited exact suffix is
frozen into the child `AgentSnapshot` before its first `RunStarted`.

Delegation retains durable invocation reservation, parent `ToolCallLinked`,
child provenance, exactly-once child start, cancellation propagation, bounded
result retention, and restart reconciliation. The delegated task/context is the
child's initial user message and never changes its system prompt.

## 10. Protocol and event schema 7

Protocol handshake and event schema are exactly 7. Config remains exactly 6
and agent documents exactly 1. Protocol, event, session JSONL, metadata, or
delegation-journal version 6 is rejected; there is no decoder or migration.

```rust
pub struct ResolvedModelRef {
    pub selection: ModelSelection,
    pub provider_id: ProviderId,
    pub model_id: ProviderModelId,
    pub adapter_id: AdaptorId,
    pub selection_fingerprint: Sha256Digest,
}

pub struct FrozenModelBinding {
    pub resolved: ResolvedModelRef,
    pub descriptor: LanguageModelDescriptor,
    pub defaults: ResolvedRequestDefaults,
    pub provider_options: ProviderOptions,
    pub behavior_fingerprint: Sha256Digest,
}

pub struct AgentSnapshot {
    pub agent: AgentId,
    pub schema: AgentSchemaVersion,
    pub mode: AgentMode,
    pub description: String,
    pub document_source: AgentDocumentSource,
    pub document_fingerprint: Sha256Digest,
    pub composed_prompt: String,
    pub prompt_fingerprint: Sha256Digest,
    pub tools: Vec<ToolName>,
    pub permissions: Vec<PermissionRule>,
    pub delegation: Option<FrozenDelegationPolicy>,
    pub fallback_chain: Vec<FrozenModelBinding>,
    pub selected_suffix_start: u32,
}

pub struct RunSelection {
    pub agent: AgentId,
    pub model: ModelSelection,
}
```

`AgentSnapshot` contains no auth values, header values, credential-store data,
or live adapter handles. `AgentDocumentSource` is the unit enum
`built_in|user|workspace`; raw filesystem paths are not persisted. The wire
shape is unchanged. For root runs, `fallback_chain` carries the frozen
exact root plan described in section 6 and `selected_suffix_start` is zero. For
delegated configured-chain runs it retains the authored-chain semantics;
empty-chain children carry their inherited exact suffix.
`RunStarted.selected_suffix` remains the authoritative executable suffix used
by attempts and child inheritance.

Model/variant and agent descriptor vectors are deterministically sorted by
their strict IDs. Descriptor projections contain no provider secrets or prompt
body. `AgentDescriptor.runnable_as_root` is true exactly when the agent is
enabled, its mode is `primary` or `all`, its own configured chain is nonempty,
and at least one chain selection is available. Root session creation and the root
Agent selector accept only descriptors with `runnable_as_root = true`.
Delegated creation is not a public protocol method.

### 10.1 Stored envelope and required events

```rust
pub struct StoredEvent {
    pub event_schema_version: EventSchemaVersion, // exactly 7
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub seq: u64,             // per-session, starts at 1
    pub timestamp: Rfc3339,
    pub payload: EventPayload,
}
```

The envelope contains ordering/version metadata only. Frozen policy is not
duplicated in every event: creation policy is in `SessionCreated`, and each
run's authoritative complete policy is in `RunStarted`.

`SessionMeta` is not an `EventPayload`. It is the strict rebuildable
`meta.json` cache schema:

```text
SessionMeta {
  meta_schema_version: 7, session_id, origin, cwd_identity,
  creation_selection, title: Option<SessionTitle>, title_updated_seq,
  last_event_seq, status
}
```

Required `EventPayload` fields are:

```text
SessionCreated {
  origin, cwd_identity, creation_selection: RunSelection,
  creation_agent: AgentSnapshot, model_snapshot_fingerprint
}
RunStarted {
  client_run_id, selection: RunSelection, agent: AgentSnapshot,
  selected_suffix: [FrozenModelBinding], input_through_seq
}
ModelAttemptStarted {
  attempt_id, attempt_ordinal, fallback_index, retry_ordinal,
  resolved_model: ResolvedModelRef, prompt_fingerprint
}
ModelReplayEvaluated {
  attempt_id, resolved_model, ordered_decisions: [ReplayDecision]
}
ModelTurnCommitted {
  attempt_id, model_turn_seq, resolved_model,
  input_through_seq, turn: PersistedModelTurn, warnings
}
ModelFallback {
  from: ResolvedModelRef, to: ResolvedModelRef,
  from_fallback_index, to_fallback_index, attempts_on_from, error
}
ToolCallStarted {
  tool_call_id, owner: AssistantToolCallRef,
  presentation: ToolCallPresentation, operation_fingerprint
}
ToolCallTerminated {
  tool_call_id, owner: AssistantToolCallRef,
  outcome: completed | failed | cancelled | interrupted,
  result: Option<PersistedToolResult>, error: Option<SafeToolError>
}
SessionTitleCommitted {
  change: SessionTitleChange, input_through_seq
}
```

`SessionOrigin` is exactly `root` or
`delegated { root_session_id, parent_session_id, parent_run_id,
parent_tool_call_id, invocation_id, depth }`; every delegated field is derived
or verified by the engine. `SessionMeta.status` is
`idle|running|completed|failed|cancelled|interrupted`. `SessionCreated` is
sequence 1 with envelope `run_id = None`. Every run-scoped payload has the same
non-null envelope `run_id` as its referenced run; `RunStarted` is that run's
first run-scoped event.

`ModelAttemptStarted` is persisted before any text/reasoning delta for that
attempt, so streaming attribution is exact even if the attempt never commits.
Every delta carries `attempt_id`. `model_turn_seq` is a stable per-session turn
sequence, distinct from event `seq`.

`AssistantToolCallRef` contains `model_turn_seq`, `content_index`,
`model_call_id`, and optional `provider_item_id`. A tool call begins only after
the owning `ModelTurnCommitted` is durable. Validation checks session, run,
content index, tool-call content, IDs, and uniqueness. `ToolCallTerminated`
must repeat the exact owner and is the sole terminal tool event; progress and
ephemeral output remain nonterminal. `ToolCallStarted` does not duplicate tool
name or raw arguments; those remain in the referenced committed turn. For a
completed termination, `result` is present and `error` absent. For failed,
cancelled, or interrupted termination, `error` is present and any `result` is a
strictly bounded safe partial result.

Replay dispositions include replayed, no artifact, discarded foreign adapter,
discarded foreign model selection (including
`DiscardedForeignVariant`), discarded invalid payload, and reconstructed
normalized history. Native state is never reused across selection fingerprints.

The title sequence is the `StoredEvent.seq` of the latest valid
`SessionTitleCommitted`. `SessionMeta.title_updated_seq` equals it (or 0 when
none). Tree/list patches apply only a strictly newer sequence, so stale tree
responses cannot overwrite a title event.

`SessionTitleChange` is a strict tagged enum:

```text
user_set { title, client_rename_id }
user_clear { client_rename_id }
user_reset { client_rename_id }
internal_agent_set { title, invocation_id }
fallback_set { title }
```

Set titles are nonblank, control-free UTF-8 of at most 512 bytes. A
`client_rename_id` is required only for user changes, is nonempty and at most
256 bytes, and is replay-indexed for idempotency. Reusing it with a different
change is `idempotency_conflict`. Clear keeps an intentional untitled override;
reset removes that override so automatic generation may run again.

### 10.2 Persistence and restart

Each `events.jsonl` line is one strict `StoredEvent` v7. `meta.json` is strict
metadata schema 7 and a rebuildable cache. The delegation journal is strict
schema 7. Unknown fields, invalid sequence continuity, wrong session IDs,
wrong first event, invalid ownership references, and non-tail corruption fail
closed. A partial final JSONL line may be truncated to the last complete newline.

Restart reconstructs `SessionMeta`, run state, title sequence, attempt/turn
attribution, tool ownership, approvals, delegation edges, and replay decisions
only from valid v7 events/journal. Nonterminal runs become interrupted; prepared
OS capabilities are never reconstructed or re-executed. The cache is rewritten
atomically after reconstruction.

Frozen bindings resolve only through an in-process model snapshot with the
exact selection and behavior fingerprints. If a persisted fingerprint is no
longer retained after restart, the session stays readable but execution/resume
fails with typed `obsolete_model_fingerprint`; it never falls back by model key
or variant name.

Assistant attribution on live streaming and replay is derived from the frozen
`RunStarted` plus `ModelAttemptStarted`/`ModelTurnCommitted`, never from the
current picker, live agent files, current provider config, or an inferred model
name. The visible header projects the exact canonical
`Agent(provider/model[variant])` selection, including `[base]`.

## 11. Inline assistant children and selectors

```rust
pub enum TranscriptItem {
    User { /* ... */ },
    Assistant {
        attribution: FrozenAssistantAttribution,
        committed_turn_seq: Option<u64>,
        children: Vec<AssistantChild>,
    },
    Event { /* diagnostic */ },
}

pub enum AssistantChild {
    Text(TextSegment),
    Thinking(ThinkingSegment),
    Tool(ToolSegment),
}
```

Each attempt/committed model turn has one visible header:
`<agent-id>(<provider>/<model-id>[<variant>])`. Text,
thinking, and tools remain in model-content order beneath it. Thinking has one
`▸`/`▾` chevron, plain wrapped text, independent expansion/cache state, and no
standalone header. Tools are ordered by owning committed model content, not
completion timing, and retain arguments, output, truncation, attachments, and
artifact references when expanded. Expanded read output keeps syntax
highlighting.

`ToolCallPresentation.primary_argument` is persisted only after control
sanitization, secret redaction, and bounding. Known primary arguments are bash
command; read/write/edit path; grep/glob pattern; and delegate target plus task
excerpt.

Message title uses `Agent(provider/model[variant])` and represents the draft
selection. Changing it does not mutate an active run. The root Agent selector
lists only agents with `runnable_as_root = true`; therefore an empty-chain `all`
agent is not selectable as a root. The root model-selection picker uses the
complete current public model catalog with exactly one row per globally
available model. Each row shows the model's canonical resolved default as
`provider/model[base]` or `provider/model[named]` plus its display name, and
changing to that model initializes that default; reselecting the current model
retains its current exact variant. Exact variant changes occur only by clicking
the dedicated `[variant]` region in the Message title, which cycles base then
named variants lexicographically. There are no variant rows and no Variant
modal.

The Agents tree has a stable root, rows exactly
`agent-id:session-title`, immediate monotonic title patching, and text height
`clamp(visible_tree_row_count, 1, 3)` with borders outside the count.

## 12. Security and bounded loading

User and workspace config roots are opened from trusted descriptors: the user
root is anchored below the current user's config home; the workspace root is
anchored at the exact cwd descriptor. Every component and final open is
descriptor-relative and no-follow. Symlinks, magic links, FIFOs, sockets,
devices, directories where files are expected, files where directories are
expected, and final files with link count other than one are rejected.

User config directories and agent directories must be current-user-owned and
mode 0700; user TOML/Markdown files must be current-user-owned regular files,
single-link, and mode 0600. Workspace directories/files may use repository
modes and ownership but must not be symlinks, must have expected type, and
agent/config files must be single-link regular files. Files are read from the
validated held descriptor, never reopened by pathname after validation.

Agent enumeration reads only direct regular `*.md` children of the exact
`agents` directory, in sorted filename-byte order; it does not recurse. Invalid
filenames fail. Two files in one layer that normalize to the same `AgentId`
fail. User/workspace duplicate IDs are the intentional atomic replacement from
section 2. Unexpected entries are ignored only when they are regular files
whose names do not end in `.md`; unexpected directories or links fail so they
cannot hide agent documents.

Config TOML is UTF-8 and at most 1 MiB, with nesting at most 32, at most 4096
table/array entries per container, and strings at most 256 KiB unless a smaller
field bound applies. Duplicate keys, unknown fields, nonfinite numbers, and
unsupported TOML date/time values fail. Agent YAML/body bounds are in section
5. Error messages identify logical path and field but never include file
contents or resolved secret values.

Interpolation is single-pass and allowed only in provider endpoint strings,
approved auth secret fields, and provider header values. The only form is
`${env:NAME}` with `NAME` matching `[A-Z_][A-Z0-9_]*`; `$$` is a literal dollar.
Missing/non-UTF-8 variables fail. Interpolation is forbidden in model IDs,
adaptor IDs, capabilities, defaults, options, variant IDs/behavior, agent
documents, prompts, tools, permissions, and delegation.

Resolved auth values, credential-store values, header values, HMAC keys, and
other values held as `SecretString` are excluded from events, errors, debug
output, logs, generated schemas/examples, fingerprints, model snapshots,
session JSONL/meta, delegation journals, artifacts, and TUI projections.
Fingerprints retain only non-secret auth shape and header names. Secret-bearing
provider requests are redacted at generic transport/logging boundaries.

## 13. Migration, phases, and ownership

Implementation deletes `.cookie_agent/config.toml` and adds the current-only
fixture:

```text
.cookie-agent/config.toml
.cookie-agent/agents/primary.md
.cookie-agent/agents/worker.md
.cookie-agent/agents/anthropic.md
.cookie-agent/agents/responses.md
.cookie-agent/agents/chat.md
```

No old path, `[agents]`, `[models]`, aliases, profile terminology, old agent
TOML, protocol/event/storage v6, or dual parser remains.

| Phase | Owner | Required owned surface |
|---|---|---|
| 1 identities/config/models | Config-model owner | `crates/identity/**`, `crates/config/**`, `crates/models/**`, models.dev updater/catalog compiler |
| 2 protocol/event v7 | Protocol owner | `crates/protocol/**`, JSON Schemas, TypeScript bindings, protocol/event golden snapshots |
| 3 engine/tools | Runtime owner | `crates/engine/**`, `crates/tools/**`, prompt/policy freezing, fallback, permissions, delegation, ownership, restart |
| 4 server/CLI | Service owner | `crates/server/**`, `crates/cookie_agent/**`, routing, composition, list/select/connect methods |
| 5 TUI | TUI owner | `crates/tui/**`, selectors, titles, stable tree, exact panel height, attribution, inline thinking/tools |
| Integration | Integration owner | root `Cargo.toml`, root `Cargo.lock`, workspace manifests, `ARCHITECTURE.md`, `README.md`, `docs/**`, checked `.cookie-agent/**` fixtures, cross-crate integration fixtures, generated schema/binding/snapshot aggregation, final current-only and stale-claim review |

Generated artifacts are committed only by their assigned owner and reviewed by
the integration owner. The integration owner resolves cross-phase type changes
and is accountable for one coherent locked workspace; individual phases do not
silently edit another owner's generated outputs.

## 14. Required validation

Each phase adds focused unit, strict-decoding, security, and snapshot tests.
Final validation includes formatting, locked workspace build/check/clippy/test,
rustdoc, Rust 1.88 equivalents, audit/deny, protocol/schema/binding regeneration
checks, functional E2E, adversarial config/filesystem tests, restart/replay, and
TUI render/hit-region snapshots.

The final stale-claim review must find no accepted text describing
`.cookie_agent`, schema-v5 config, protocol/event/storage v6 as runnable,
TOML-agent profiles, model aliases, global permissions, a standalone reasoning
or tool block, a hidden assistant variant, a separate variant picker, inherited
parent permissions, inheritance of the parent's original full fallback chain,
unversioned variant behavior, stale title refresh, or a compatibility decoder.

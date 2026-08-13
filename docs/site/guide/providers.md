# Providers

cookie agent connects to language models through a dynamic provider pipeline: a
fixed models.dev catalog, a code-owned family recipe registry, a per-user
provider store, and Oven SDK adapters for each protocol family. Two kinds of
provider definitions are supported: **managed** catalog providers and
**custom** providers.

## How providers are resolved

At startup the daemon refreshes the models.dev catalog
(`https://models.dev/catalog.json`), with a validated ETag cache and a bundled
integrity-checked bootstrap as fallback. Catalog providers are classified by
their npm package name against the code-owned family registry (schema 1). Only
providers with a known family recipe compile; non-deprecated text-output models
are included automatically. Providers removed from the catalog remain visible
when a retained store connection matches a known family.

## Managed providers

The simplest setup is `/connect` (or `cookie connect`). It lists the current
catalog providers plus authored or store-backed managed providers removed from
the current catalog, and walks you through the provider's recipe-defined setup
fields and authentication credentials. The flow stores normalized setup and
credentials **globally for the current user** in the provider store and does not
test them until the first model request.

Setup values are validated as you enter them against the recipe's field
descriptor: string and code fields enforce their declared minimum and maximum
lengths, integer fields enforce their allowed range, and booleans accept
`true`/`yes`/`y`/`1` and `false`/`no`/`n`/`0` case-insensitively. The same shared
parser is used by `/connect`, `cookie connect`, and the TUI's connect flow.

Managed providers may also be authored in configuration:

```toml
schema_version = 10

[providers.openai]
source = "models_dev"
api_key = "${env:OPENAI_API_KEY}"
```

Available fields for a managed provider are `base_url`, `shape`, `setup`,
`api_key`, `auth_override`, and sparse `model_overrides`. `api_key` is allowed
only for providers whose recipe's default method is an unambiguous single-secret
method. Other methods use `auth_override`:

```toml
[providers.google-vertex]
source = "models_dev"
setup = { project = "${env:GOOGLE_VERTEX_PROJECT}", location = "${env:GOOGLE_VERTEX_LOCATION}" }
auth_override = { method = "oauth-access-token-v1", values = { access_token = "${env:ACCESS_TOKEN}" } }
```

`api_key` and `auth_override` are mutually exclusive. An authored `base_url`
requires auth and all non-defaulted setup fields in the same provider definition;
it never inherits provider-store setup or credentials, and it must be HTTPS.

**Credential precedence** for a managed provider: authored `api_key`, authored
`auth_override`, an eligible provider-store connection (when no authored
`base_url` exists), reviewed no-auth, then unavailable.

**Endpoint precedence**: authored `base_url`, the catalog's declared API
endpoint, then the family default endpoint.

### Family registry

The registry maps catalog npm packages to protocol families, default endpoints,
and authentication methods:

| Family | npm package (catalog rows) | Default endpoint | Default auth | Allowed auth |
|---|---|---|---|---|
| OpenAI-compatible chat | `@ai-sdk/openai-compatible` | *(none)* | `bearer-api-key-v1` | `bearer-api-key-v1` |
| OpenAI-compatible chat | `@ai-sdk/groq` | `https://api.groq.com/openai/v1` | `bearer-api-key-v1` | `bearer-api-key-v1` |
| OpenAI-compatible chat | `@ai-sdk/mistral` | `https://api.mistral.ai/v1` | `bearer-api-key-v1` | `bearer-api-key-v1` |
| OpenAI-compatible chat | `@ai-sdk/xai` | `https://api.x.ai/v1` | `bearer-api-key-v1` | `bearer-api-key-v1` |
| OpenAI-compatible chat | `@ai-sdk/cerebras` | `https://api.cerebras.ai/v1` | `bearer-api-key-v1` | `bearer-api-key-v1` |
| OpenAI-compatible chat | `@ai-sdk/togetherai` | `https://api.together.xyz/v1` | `bearer-api-key-v1` | `bearer-api-key-v1` |
| OpenAI-compatible chat | `@ai-sdk/deepinfra` | `https://api.deepinfra.com/v1/openai` | `bearer-api-key-v1` | `bearer-api-key-v1` |
| OpenAI-compatible chat | `@ai-sdk/perplexity` | `https://api.perplexity.ai` | `bearer-api-key-v1` | `bearer-api-key-v1` |
| OpenAI-compatible chat | `venice-ai-sdk-provider` | `https://api.venice.ai/api/v1` | `bearer-api-key-v1` | `bearer-api-key-v1` |
| OpenAI-compatible chat | `@openrouter/ai-sdk-provider` | `https://openrouter.ai/api/v1` | `bearer-api-key-v1` | `bearer-api-key-v1` |
| OpenAI-compatible chat | `@qvac/ai-sdk-provider` | *(none)* | `bearer-api-key-v1` | `bearer-api-key-v1` |
| Anthropic | `@ai-sdk/anthropic` | `https://api.anthropic.com/v1` | `anthropic-api-key-v1` | `anthropic-api-key-v1`, `bearer-api-key-v1` |
| OpenAI | `@ai-sdk/openai` | `https://api.openai.com/v1` | `bearer-api-key-v1` | `bearer-api-key-v1` |
| Google | `@ai-sdk/google` | `https://generativelanguage.googleapis.com/v1beta` | `google-api-key-header-v1` | `google-api-key-header-v1` |
| Vertex | `@ai-sdk/google-vertex` | *(computed)* | `oauth-access-token-v1` | `oauth-access-token-v1` |
| Vertex Anthropic | `@ai-sdk/google-vertex/anthropic` | *(computed)* | `oauth-access-token-v1` | `oauth-access-token-v1` |
| Bedrock | `@ai-sdk/amazon-bedrock` | `https://bedrock-runtime.${AWS_REGION}.amazonaws.com` | `aws-sigv4-credentials-v1` | `aws-sigv4-credentials-v1`, `bearer-api-key-v1` |
| Bedrock | `@ai-sdk/amazon-bedrock/mantle` | *(none)* | `bearer-api-key-v1` | `bearer-api-key-v1` |
| Azure | `@ai-sdk/azure` | `https://${AZURE_RESOURCE_NAME}.openai.azure.com` | `azure-api-key-v1` | `azure-api-key-v1`, `bearer-api-key-v1` |
| Cohere | `@ai-sdk/cohere` | `https://api.cohere.com/v2/chat` | `bearer-api-key-v1` | `bearer-api-key-v1` |

Families that build the endpoint from setup fields (**Vertex**, **Bedrock**,
**Azure**) forbid authored `base_url` overrides.

### Common managed examples

```toml
# OpenAI
[providers.openai]
source = "models_dev"
api_key = "${env:OPENAI_API_KEY}"

# Anthropic
[providers.anthropic]
source = "models_dev"
api_key = "${env:ANTHROPIC_API_KEY}"

# Google Gemini
[providers.google]
source = "models_dev"
api_key = "${env:GOOGLE_API_KEY}"

# Groq (OpenAI-compatible chat)
[providers.groq]
source = "models_dev"
api_key = "${env:GROQ_API_KEY}"

# Vertex (endpoint computed from project/location)
[providers.google-vertex]
source = "models_dev"
setup = { project = "${env:GOOGLE_VERTEX_PROJECT}", location = "${env:GOOGLE_VERTEX_LOCATION}" }
auth_override = { method = "oauth-access-token-v1", values = { access_token = "${env:GOOGLE_VERTEX_ACCESS_TOKEN}" } }

# Bedrock (endpoint computed from region)
[providers.amazon-bedrock]
source = "models_dev"
setup = { region = "${env:AWS_REGION}" }
auth_override = {
  method = "aws-sigv4-credentials-v1",
  values = {
    access_key_id = "${env:AWS_ACCESS_KEY_ID}",
    secret_access_key = "${env:AWS_SECRET_ACCESS_KEY}",
    # session_token = "${env:AWS_SESSION_TOKEN}"  # optional
  }
}

# Azure (endpoint computed from resource_name)
[providers.azure]
source = "models_dev"
setup = { resource_name = "${env:AZURE_RESOURCE_NAME}" }
api_key = "${env:AZURE_API_KEY}"

# Cohere
[providers.cohere]
source = "models_dev"
api_key = "${env:COHERE_API_KEY}"
```

### Environment variable conventions

The provider store and `/connect` use the environment aliases declared by each
catalog row. The first declared variable aliases the `api_key` (or `access_token`)
credential; AWS static credentials map to `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, and `AWS_SESSION_TOKEN`. For example, `openai` declares
`OPENAI_API_KEY`, `anthropic` declares `ANTHROPIC_API_KEY`, `google` declares
`GOOGLE_API_KEY`, `azure` declares `AZURE_API_KEY`, and `cohere` declares
`COHERE_API_KEY`. These are suggestions for `/connect`; you are never required to
use the exact variable name in authored configuration.

## Authentication methods

| Method | Credentials | Wire form |
|---|---|---|
| `no-auth-v1` | — | no auth |
| `bearer-api-key-v1` | `api_key` | `Authorization: Bearer <key>` |
| `api-key-header-v1` | `api_key` + `header_name` parameter (`x-api-key` or `api-key`) | custom header |
| `anthropic-api-key-v1` | `api_key` | `x-api-key` |
| `google-api-key-header-v1` | `api_key` | `x-goog-api-key` |
| `oauth-access-token-v1` | `access_token` | `Authorization: Bearer <token>` |
| `aws-sigv4-credentials-v1` | `access_key_id`, `secret_access_key`, optional `session_token` | AWS SigV4 |
| `azure-api-key-v1` | `api_key` | `api-key` |

## Custom providers

Custom provider IDs begin with `custom.` (for example `custom.example`). They
are config-only: they never appear in `/connect` and never use the provider
store. A custom definition requires an HTTPS endpoint (or loopback `http`), an
adaptor, a typed auth definition, and complete explicit model capabilities.

### Adaptors

| Adaptor ID | Allowed auth methods | Notes |
|---|---|---|
| `openai-compatible` | `bearer-api-key-v1`, `api-key-header-v1`, `no-auth-v1` | Chat Completions against any OpenAI-compatible endpoint |
| `openai-chat` | `bearer-api-key-v1`, `no-auth-v1` | Official OpenAI Chat |
| `openai-responses` | `bearer-api-key-v1`, `no-auth-v1` | OpenAI Responses API |
| `anthropic` | `anthropic-api-key-v1` | Anthropic Messages |
| `anthropic-compatible` | `anthropic-api-key-v1` | Anthropic Messages against a compatible endpoint |
| `google-gemini` | `google-api-key-header-v1` | Gemini `generateContent` |
| `google-vertex-gemini` | `oauth-access-token-v1` | Vertex publisher; requires `setup` = `project`, `location`, `resource` |
| `aws-bedrock-converse` | `aws-sigv4-credentials-v1` | Bedrock Converse; requires `setup` = `region` |
| `azure-openai-chat` / `azure-openai-responses` | `azure-api-key-v1` | Azure OpenAI; requires `setup` = `api_version`, `deployment` |
| `cohere-v2-chat` | `bearer-api-key-v1` | Cohere Chat v2 |

Custom model IDs are `provider/model-id` strings; group into one provider under
the `models` map.

### OpenAI-compatible example

```toml
[providers."custom.acme"]
source = "custom"
endpoint = "https://api.acme.example.invalid/v1"
adaptor = "openai-compatible"
setup = {}
auth = { method = "bearer-api-key-v1", values = { api_key = "${env:ACME_API_KEY}" } }
headers = { "x-acme-feature" = "enabled" }

[providers."custom.acme".models."acme/turbo"]
display_name = "Acme Turbo"
defaults = { max_output_tokens = 4096 }

[providers."custom.acme".models."acme/turbo".capabilities]
input = ["text"]
output = ["text"]
context_tokens = 32768
output_tokens = 4096
tool_calling = true
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = false
native_replay = "unsupported"
cancellation = "local_only"
media = {}
```

### Anthropic-compatible example

```toml
[providers."custom.acme-anthropic"]
source = "custom"
endpoint = "https://api.acme.example.invalid/v1"
adaptor = "anthropic-compatible"
auth = { method = "anthropic-api-key-v1", values = { api_key = "${env:ACME_API_KEY}" } }

[providers."custom.acme-anthropic".models."acme/claude-compatible"]
display_name = "Acme Claude Compatible"
capabilities = { input = ["text"], output = ["text"], context_tokens = 32768, output_tokens = 4096,
  tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false,
  temperature = true, top_p = true, seed = false, native_replay = "unsupported",
  cancellation = "local_only", media = {} }
```

An `api-key-header-v1` auth for a vendor that wants the key in a specific header
(rather than `Authorization: Bearer`):

```toml
[providers."custom.acme"]
source = "custom"
endpoint = "https://api.acme.example.invalid/v1"
adaptor = "openai-compatible"
auth = {
  method = "api-key-header-v1",
  parameters = { header_name = "x-api-key" },
  values = { api_key = "${env:ACME_API_KEY}" },
}
```

Custom static headers are public behavior metadata: they cannot interpolate,
carry credentials, or collide with transport/protocol/auth-owned headers
(`authorization`, `host`, `content-length`, `user-agent`, `content-type`,
`x-api-key`, `x-goog-api-key`, `anthropic-version`, and others). Use a typed auth
method such as `api-key-header-v1` for secret header values.

### Local OpenAI-compatible endpoint

Loopback `http` endpoints are allowed for local servers (for example LM Studio or
a local vLLM). The URL must be `http://localhost:<port>/v1` or
`http://127.0.0.1:<port>/v1` (or `::1`) with the exact adaptor path:

```toml
[providers."custom.local"]
source = "custom"
endpoint = "http://127.0.0.1:11434/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }
```

`no-auth-v1` is allowed only for `openai-compatible`, `openai-chat`, and
`openai-responses`. A no-auth OpenAI Responses model additionally requires a
text-only, tool-less capability profile with native replay unsupported.

### Vertex, Bedrock, and Azure custom examples

```toml
# Vertex custom — endpoint is the publisher path derived from setup
[providers."custom.vertex"]
source = "custom"
endpoint = "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google"
adaptor = "google-vertex-gemini"
setup = { project = "my-project", location = "us-central1", resource = "publishers/google" }
auth = { method = "oauth-access-token-v1", values = { access_token = "${env:VERTEX_TOKEN}" } }

[providers."custom.vertex".models."gemini-x"]
display_name = "Gemini X"
capabilities = { input = ["text"], output = ["text"], context_tokens = 131072, output_tokens = 8192,
  tool_calling = true, parallel_tool_calls = true, structured_output = true, reasoning = true,
  temperature = true, top_p = true, seed = false, native_replay = "unsupported",
  cancellation = "local_only", media = {} }
```

```toml
# Azure custom — deployment + api_version in setup, no base_url override
[providers."custom.azure"]
source = "custom"
endpoint = "https://my-resource.openai.azure.com"
adaptor = "azure-openai-chat"
setup = { deployment = "gpt-x", api_version = "2024-06-01" }
auth = { method = "azure-api-key-v1", values = { api_key = "${env:AZURE_OPENAI_API_KEY}" } }
```

## Provider options

Custom models accept adaptor-specific `options`. The current options are:

| Option | Applies to | Meaning |
|---|---|---|
| `api_version` | *(reserved)* | Rejected in custom model options; set at provider level. |
| `beta` | `anthropic`, `anthropic-compatible` | Anthropic beta header values. |
| `organization` | `openai-chat`, `openai-responses`, `openai-compatible` | `OpenAI-Organization` header. |
| `project` | `openai-chat`, `openai-responses` | `OpenAI-Project` header. |
| `store` | `openai-responses` | Persist the response with the provider. |
| `api_path` | `openai-compatible` | Alternative API path on the endpoint. |
| `location`, `region`, `deployment` | *(reserved)* | Provider setup fields; rejected in custom model options. |

Options are validated per adaptor: for example `beta` is rejected for every
non-Anthropic adaptor, and `api_path` is rejected unless the adaptor is
`openai-compatible`.

## Model overrides

Managed providers accept sparse `model_overrides` to adjust a catalog model
without inventing new behavior:

```toml
[providers.openai]
source = "models_dev"
api_key = "${env:OPENAI_API_KEY}"

[providers.openai.model_overrides."gpt-5"]
display_name = "My GPT-5"
enabled = true
defaults = { max_output_tokens = 8192, temperature = 0.2 }
shape = "chat"
default_variant = "base"
```

A model override cannot add a model that is absent from the catalog, change
capabilities, or point a default variant at a variant that does not exist.
Variants use `VariantDirective` operations: `add`, `replace`, or `disable`.

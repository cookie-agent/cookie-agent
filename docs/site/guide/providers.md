# Providers

cookie agent obtains a fixed models.dev catalog at startup, then resolves
managed providers through code-owned protocol, setup, and authentication
recipes. The selected catalog source is network, a validated ETag cache, or the
bundled bootstrap.

## Managed providers

The simplest setup is `/connect`. It lists current catalog providers plus
authored or store-backed managed providers removed from the current catalog.
Unsupported rows remain visible with a reason. The flow stores normalized setup
and credentials globally for the current user and does not test them until first
use.

Managed providers may also be authored:

```toml
[providers.openai]
source = "models_dev"
api_key = "${env:OPENAI_API_KEY}"
```

Available fields are `base_url`, `shape`, `setup`, `api_key`, `auth_override`,
and sparse `model_overrides`. `api_key` is allowed only for an unambiguous
single-secret default method. Other methods use:

```toml
auth_override = { method = "oauth-access-token-v1", values = { access_token = "${env:ACCESS_TOKEN}" } }
```

`api_key` and `auth_override` are mutually exclusive. An authored `base_url`
requires auth and all non-defaulted setup fields in the same provider
definition; it never inherits provider-store setup or credentials.

Managed auth precedence is authored `api_key`, authored `auth_override`, an
eligible provider-store connection when no authored `base_url` exists,
reviewed no-auth, then unavailable. Endpoint precedence is authored `base_url`,
catalog API, then the family default.

Supported, non-deprecated text-output models are included automatically. Sparse
`model_overrides` can disable a model or adjust recipe-approved display,
defaults, variants, default variant, and shape; it cannot invent an absent
managed model or capabilities.

## Custom providers

Custom provider IDs begin with `custom.`. They are config-only, never appear in
`/connect`, and never use the provider store. A custom definition requires an
endpoint, adaptor, typed auth, and complete explicit model capabilities:

```toml
[providers."custom.example"]
source = "custom"
endpoint = "https://api.example.invalid/v1"
adaptor = "openai-compatible"
setup = {}
auth = { method = "bearer-api-key-v1", values = { api_key = "${env:CUSTOM_API_KEY}" } }
headers = { "x-example-feature" = "enabled" }

[providers."custom.example".models."example-org/model"]
display_name = "Example Model"
defaults = { max_output_tokens = 4096 }

[providers."custom.example".models."example-org/model".capabilities]
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

Custom static headers are public behavior metadata: they cannot interpolate,
carry credentials, or collide with transport/protocol/auth-owned headers. Use a
typed auth method such as `api-key-header-v1` for secret header values.

See the [provider conformance design note](../design/index.md) for the current
npm-family registry.

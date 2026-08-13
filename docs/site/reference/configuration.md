# Configuration Reference

This page documents every configurable item accepted by the current configuration
schemas. Everything here is verified against the `config` and `models` crates;
there are no undocumented aliases, hidden keys, or migration paths.

Two independent configuration surfaces exist:

| Surface | Location | Schema |
|---|---|---|
| Runtime and providers | `~/.config/cookie_agent/config.toml` and `<cwd>/.cookie-agent/config.toml` | 10 |
| Agents | `~/.config/cookie_agent/agents/<agent-id>.md` and `<cwd>/.cookie-agent/agents/<agent-id>.md` | 4 |
| TUI | `$XDG_CONFIG_HOME/cookie_agent/tui.toml` or `~/.config/cookie_agent/tui.toml` | 1 |

## Layering and strictness

The user layer and the exact working directory's `.cookie-agent` layer are both
optional. Configuration is loaded from the exact working directory only; there
is no upward search. Within a layer, `config.toml` and the `agents/` directory
are optional. A workspace layer replaces the corresponding user settings and
providers/agents wholesale by ID; nested fields never merge.

Unknown keys, wrong types, and wrong schema versions are rejected with an
actionable path or key error. Decoded values that hold secrets are zeroized when
the load completes.

TOML-level limits (enforced before deserialization):

- Configuration file at most 1 MiB; agent document at most 256 KiB.
- Maximum TOML nesting depth 32; at most 4096 entries per table or array.
- String values at most 256 KiB; TOML datetimes rejected; floats must be finite.

## Top-level keys

Only these keys are allowed at the top of `config.toml`. Everything except
`schema_version` is optional.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `schema_version` | integer | *(required)* | Must be exactly `10` |
| `server` | table | defaults below | Daemon bind address |
| `tool_output` | table | defaults below | Tool output truncation limits |
| `approval` | table | defaults below | Approval expiry |
| `context_compaction` | table | defaults below | Automatic context compaction |
| `session_title` | table | defaults below | Automatic session titles |
| `delegation` | table | defaults below | Delegation depth and concurrency |
| `providers` | table | empty | Provider definitions |

Minimal valid file:

```toml
schema_version = 10

[providers]
```

## `[server]`

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | `"127.0.0.1"` | Interface the daemon listens on. Must be non-empty and at most 255 characters. The `cookie` binary additionally requires exactly `"127.0.0.1"` at startup. |
| `port` | integer | `7419` | TCP port for the WebSocket daemon. |

## `[tool_output]`

Controls how much tool output is retained inline in a session before it is
truncated or replaced with artifact references.

| Key | Type | Default | Description |
|---|---|---|---|
| `max_lines` | integer | `2000` | Maximum lines of tool output retained. Must be greater than zero. |
| `max_bytes` | integer | `51200` (`50 * 1024`) | Maximum bytes of tool output retained. Must be greater than zero. |

## `[approval]`

| Key | Type | Default | Description |
|---|---|---|---|
| `timeout_ms` | integer | `30000` | How long a user approval stays pending before it expires unattended. Must be greater than zero. |

## `[context_compaction]`

Controls the automatic context-limit behavior documented in
[Compaction](../guide/compaction.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `auto` | boolean | `true` | Enable automatic compaction signals (post-check usage and predictive pre-send estimation). Manual `/compact` and context-overflow recovery compaction remain available when `false`. |
| `buffer_tokens` | integer | `33000` | Headroom subtracted from the model context limit. The trigger threshold is `context_limit - buffer_tokens`; compaction runs when actual or estimated usage reaches it. Must be greater than zero. |
| `max_summary_bytes` | integer | `262144` (`256 * 1024`) | Hard byte limit for a compaction summary produced by the internal `compaction` agent. Must be greater than zero and at most `2 * 1024 * 1024` (2 MiB). |

## `[session_title]`

Controls automatic session titles generated from the first user message.

| Key | Type | Default | Description |
|---|---|---|---|
| `max_chars` | integer | `80` | Maximum title length in characters. Must be greater than zero. |
| `max_input_messages` | integer | `4` | Maximum number of opening user messages included in the title-agent prompt. The engine uses the first N messages so the title remains anchored to the session's original topic. Must be greater than zero. |
| `generate_on_first_turn` | boolean | `true` | Generate a title automatically after the first user message. When `false`, no automatic title is produced (user-set titles still work). |
| `fallback_to_input_excerpt` | boolean | `true` | When the internal title agent fails or returns an unusable title, fall back to an excerpt of the first user message instead of leaving the session untitled. |

## `[delegation]`

| Key | Type | Default | Description |
|---|---|---|---|
| `max_depth` | integer | `3` | Maximum delegation depth below a root session. Must be greater than zero. |
| `max_concurrency` | integer | *(omitted)* | Maximum concurrent delegation invocations. Omitted means unlimited. A value of `0` is rejected. |

## `[providers]`

Provider definitions are keyed by provider ID under `[providers.<id>]`. Each
value is one of two tagged sources. See the [Providers guide](../guide/providers.md)
for complete examples.

### Managed providers (`source = "models_dev"`)

```toml
[providers.openai]
source = "models_dev"
api_key = "${env:OPENAI_API_KEY}"
```

| Key | Type | Default | Description |
|---|---|---|---|
| `source` | string | *(required)* | Must be `"models_dev"`. |
| `base_url` | string | *(none)* | HTTPS endpoint override. Requires same-definition auth (`api_key` or `auth_override`) and never inherits provider-store setup or credentials. Forbidden for families that compute their endpoint from setup (Vertex, Bedrock, Azure). |
| `setup` | map of string values | empty | Setup fields the provider recipe requires (for example `project`, `location`, `region`, `resource_name`). Interpolates `${env:NAME}`. |
| `api_key` | string | *(none)* | Single-secret default auth. Allowed only for providers whose default method is an unambiguous single API key. Interpolates `${env:NAME}`. |
| `auth_override` | table | *(none)* | Explicit auth method override. Mutually exclusive with `api_key`. |
| `shape` | string | catalog shape | `"chat"` or `"responses"` model shape override. |
| `model_overrides` | map | empty | Sparse per-model overrides: `enabled`, `display_name`, `defaults`, `variants`, `default_variant`, `shape`. Cannot invent a model absent from the catalog or change capabilities. |

`auth_override`:

| Key | Type | Description |
|---|---|---|
| `method` | string | One of the current auth method IDs (see below). |
| `values` | map of strings | Credential values keyed by credential field name (for example `api_key`, `access_token`, `access_key_id`, `secret_access_key`, `session_token`). Interpolates `${env:NAME}`. |

### Custom providers (`source = "custom"`)

Provider IDs under `custom.` are config-only, never appear in `/connect`, and
never use the provider store.

```toml
[providers."custom.example"]
source = "custom"
endpoint = "https://api.example.invalid/v1"
adaptor = "openai-compatible"
setup = {}
auth = { method = "bearer-api-key-v1", values = { api_key = "${env:CUSTOM_API_KEY}" } }
headers = {}
```

| Key | Type | Default | Description |
|---|---|---|---|
| `source` | string | *(required)* | Must be `"custom"`. |
| `endpoint` | string | *(required)* | Absolute, query-free URL. Must be `https`, or `http` to `localhost`/a loopback address. Interpolates `${env:NAME}`. |
| `adaptor` | string | *(required)* | Protocol adaptor ID (see below). |
| `setup` | map of string values | empty | Adaptor-required setup fields (Vertex `location`/`project`/`resource`, Bedrock `region`, Azure `api_version`/`deployment`). Interpolates `${env:NAME}`. |
| `auth` | table | *(required)* | Typed auth definition (see below). |
| `headers` | map of strings | empty | Public static headers. No interpolation; may not collide with transport/protocol/auth-owned headers. |
| `models` | map | *(required)* | At least one custom model definition. |

Supported adaptor IDs are `openai-compatible`, `openai-chat`,
`openai-responses`, `anthropic`, `anthropic-compatible`, `google-gemini`,
`google-vertex-gemini`, `aws-bedrock-converse`, `azure-openai-chat`,
`azure-openai-responses`, and `cohere-v2-chat`.

Custom model definition under `[providers."custom.x".models."<model-id>"]`:

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Whether the model is compiled into the runtime. |
| `display_name` | string | *(required)* | Nonempty, at most 512 characters, no control characters. |
| `capabilities` | table | *(required)* | Explicit capability declarations (see below). |
| `defaults` | table | empty | Request defaults (see below). |
| `options` | table | empty | Adaptor-specific options (see below). |
| `variants` | map | empty | Named variant directives. |
| `default_variant` | string | *(none)* | `"base"` or a named variant that must exist in `variants`. |

`capabilities`:

| Key | Type | Description |
|---|---|---|
| `input` | array of strings | Nonempty modalities: `text`, `image`, `audio`, `pdf`. |
| `output` | array of strings | Nonempty modalities; may not include a modality that is also declared in `media` output constraints. |
| `context_tokens` | integer | Context window in tokens; must be greater than zero. |
| `output_tokens` | integer | Maximum output tokens; must be greater than zero and at most `context_tokens`. |
| `tool_calling` | boolean | Whether the model supports tool calling. |
| `parallel_tool_calls` | boolean | Implies `tool_calling` when `true`. |
| `structured_output` | boolean | JSON-structured output support. |
| `reasoning` | boolean | Reasoning support. |
| `temperature` | boolean | Temperature parameter support. |
| `top_p` | boolean | Top-p parameter support. |
| `seed` | boolean | Seed parameter support. |
| `native_replay` | string | `"unsupported"`, `"optional"`, or `"required"`. |
| `cancellation` | string | `"local_only"` or `"provider"`. |
| `media` | map | Per-kind (`image`, `audio`, `pdf`) media capability tables with `mime_types`, `max_bytes`, `max_count`. Must match declared input modalities. |

`defaults` (request defaults):

| Key | Type | Description |
|---|---|---|
| `temperature` | float | Finite value; requires `capabilities.temperature`. |
| `top_p` | float | Finite value; requires `capabilities.top_p`. |
| `max_output_tokens` | integer | Must be greater than zero and at most `capabilities.output_tokens`. |
| `stop` | array of strings | Stop sequences. |
| `seed` | integer | Requires `capabilities.seed`. |
| `tool_choice` | string or object | `"auto"`, `"none"`, `"required"`, or `{ "name": "..." }`; requires `capabilities.tool_calling`. |

`options` (adaptor-specific; see [Provider options](../guide/providers.md#provider-options)):

| Key | Type | Description |
|---|---|---|
| `api_version` | string | Reserved for provider-level setup; rejected in custom model options. |
| `beta` | array of strings | Anthropic beta header values; rejected for non-Anthropic adaptors. |
| `organization` | string | OpenAI organization header; rejected for non-OpenAI adaptors. |
| `project` | string | OpenAI project header; rejected for non-OpenAI adaptors. |
| `store` | boolean | OpenAI Responses store flag; rejected for non-OpenAI adaptors. |
| `api_path` | string | OpenAI-compatible path override; rejected for non-compatible adaptors. |
| `location`, `region`, `deployment` | string | Provider setup fields; rejected in custom model options. |

## Environment interpolation

`${env:NAME}` is a single-pass, `$$`-escaped interpolation performed on provider
values during config load. It is allowed only in these paths:

- `providers.<id>.endpoint`
- `providers.<id>.base_url`
- `providers.<id>.setup.<field>`
- `providers.<id>.api_key`
- `providers.<id>.auth_override.values.<field>`
- `providers.<id>.auth.values.<field>`

`NAME` must be `[A-Z_][A-Z0-9_]*` (uppercase letters, digits, underscore, starting
with a letter or underscore). A missing variable, a non-UTF-8 value, or an
interpolation used anywhere else is a load error. Interpolation is not available
in permission patterns, agent documents, or custom static headers.

## Agent documents (schema 4)

Agent files are named `<agent-id>.md`; the filename is the agent ID
(`^[a-z0-9](?:[a-z0-9]|-(?=[a-z0-9]))*$`, at most 64 characters). Each file has
YAML frontmatter between `---` fences and a nonempty Markdown body used as the
system prompt. Frontmatter at most 128 KiB, body at most 128 KiB; nested list or
map size is capped at 256 entries and depth at 16. YAML anchors, aliases, tags,
and merge keys are rejected, as is any `${env:` text.

See [Agents](../guide/agents.md) for the full frontmatter reference and reserved
IDs.

## TUI configuration (`tui.toml`, schema 1)

The TUI config is independent of the runtime config: no workspace layer, no
environment-variable override, no engine involvement. A missing file uses
defaults; unknown keys and malformed values are rejected naming the path and key.

| Key | Type | Default | Description |
|---|---|---|---|
| `minimum_event_level` | string | `"warning"` | Minimum diagnostic event level rendered in the conversation pane. One of `"debug"`, `"info"`, `"warning"`, `"error"`. Rows below the threshold stay in the session projection and reappear when the level is lowered with `/events` at runtime (view-only; the file is never rewritten). |
| `theme` | string | *(env detection)* | `"default"`, `"mono"`, or `"high-contrast"`. Precedence: this key, then `COOKIE_THEME`, then terminal detection. `NO_COLOR` and `TERM=dumb` always force monochrome regardless of this key. |

See `docs/tui.toml.example` in the repository for a fully commented example.

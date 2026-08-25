# Configuration Reference

This page documents every configurable item accepted by the current parsers.
Everything here is verified against the `config` and `models` crates;
there are no undocumented aliases, hidden keys, or migration paths.

Three independent configuration surfaces exist:

| Surface | Location |
|---|---|
| Runtime, providers, MCP servers, and plugins | `~/.cookie_agent/config.toml` and `<cwd>/.cookie-agent/config.toml` |
| Agents | `~/.cookie_agent/agents/<agent-id>.md` and `<cwd>/.cookie-agent/agents/<agent-id>.md` |
| Skills | `~/.cookie_agent/skills/<name>/SKILL.md` and `.cookie-agent/skills/<name>/SKILL.md` from cwd to worktree root |
| TUI | `~/.cookie_agent/tui.toml` |

## Layering and strictness

The user layer and the exact working directory's `.cookie-agent` layer are both
optional. Configuration is loaded from the exact working directory only; there
is no upward search. Within a layer, `config.toml` and the `agents/` directory
are optional. A workspace layer replaces the corresponding user settings and
providers, MCP servers, plugins, and agents wholesale by ID; nested fields
never merge.

Every authored file is parsed strictly. Unknown keys, leftover `schema` or
`schema_version` keys, wrong types, and malformed content are hard errors with an
actionable path, key, and line where available. No authored-file migrations or
unknown-field ignores exist. Decoded values that hold secrets are zeroized when
the load completes.

The running engine adds a mutable `Runtime` layer for MCP server entries. It
wins over the workspace and user file layers and lasts for the daemon lifetime.
Runtime removals are tombstones: the named server is absent even when a file
layer defines it. Runtime, user-file, and workspace-file entries use the same
connection lifecycle. Nothing is written automatically.

Explicit MCP write-back replaces only `[mcp.servers.<name>]` in the selected
user or project file using a format-preserving TOML document edit. Unrelated
keys, tables, and comments are retained. The complete candidate is passed
through the strict configuration loader before an atomic replacement; an
existing unknown field, type conflict, or malformed table fails without
changing the file. The replacement syncs the candidate before rename and syncs
the containing directory afterward. The newly staged replacement is owner-only;
the existing source path is not validated before replacement. The source file is re-read and
compared immediately before replacement, and a mismatch fails with a conflict
instead of overwriting the external edit. A modification landing between that
comparison and the rename itself cannot be detected — a narrow residual race
that only matters if another writer edits the file in the same instant; avoid
concurrent external edits while applying changes. The runtime entry remains the
effective layer after a successful write, so normal file-layer provenance
resumes on restart.

TOML-level limits (enforced before deserialization):

- Configuration file at most 1 MiB; agent document at most 256 KiB.
- Maximum TOML nesting depth 32; at most 4096 entries per table or array.
- String values at most 256 KiB; TOML datetimes rejected; floats must be finite.

## Top-level keys

Only these optional keys are allowed at the top of `config.toml`.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `server` | table | defaults below | Daemon bind address |
| `tool_output` | table | defaults below | Tool output truncation limits |
| `project_context` | table | defaults below | Automatic `AGENTS.md` context |
| `approval` | table | defaults below | Approval expiry |
| `context_compaction` | table | defaults below | Automatic context compaction |
| `prompt_caching` | table | defaults below | Anthropic prompt-cache breakpoints |
| `session_title` | table | defaults below | Automatic session titles |
| `delegation` | table | defaults below | Delegation depth and concurrency |
| `pricing` | table | empty | Optional model-rate overrides for cost estimates |
| `providers` | table | empty | Provider definitions |
| `mcp` | table | empty | MCP tool server definitions |
| `plugins` | table | empty | Executable plugin definitions |

Minimal valid file:

```toml

[providers]
```

Skill access is configured in agent document permissions like other scoped
capabilities:

```yaml
permissions:
  skill:
    release-check: allow
    internal-only: deny
```

`skill` resources are skill names. A deny hides the skill from model discovery;
an allow or ask can publish it and the conditional `skill` tool. Skill
`allowed-tools` grants never override a governing deny.

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

## `[project_context]`

Controls root-run `AGENTS.md` discovery documented in
[Agents](../guide/agents.md#project-context-from-agentsmd).

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Load project-context files at each root run start. Delegated and internal agents remain excluded. |
| `max_bytes` | integer | `32768` (`32 * 1024`) | Maximum UTF-8 bytes retained from each discovered file. Longer content is truncated on a UTF-8 boundary and records its original size. Must be from 1 through `2097152` (2 MiB). |

## `[context_compaction]`

Controls the automatic context-limit behavior documented in
[Compaction](../guide/compaction.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `auto` | boolean | `true` | Enable automatic compaction signals (post-check usage and predictive pre-send estimation). Manual `/compact` and context-overflow recovery compaction remain available when `false`. |
| `trigger` | inline table | `{ percent = 70 }` | Trigger threshold selection. `{ percent = N }` uses `N%` of the model context limit, where `N` must be from 1 through 99. `{ buffer_tokens = N }` subtracts positive `N` from the context limit, saturating at zero. |
| `buffer_tokens` | integer | unset | Legacy alias for `trigger = { buffer_tokens = N }`. Must be greater than zero and cannot be set together with `trigger`. |
| `max_summary_bytes` | integer | `262144` (`256 * 1024`) | Hard byte limit for a compaction summary produced by the internal `compaction` agent. Must be greater than zero and at most `2 * 1024 * 1024` (2 MiB). |

## `[prompt_caching]`

Controls declarative Anthropic prompt-cache breakpoints. The strategy is applied
only when the selected adaptor declares prompt-caching capability. Other
adaptors are unchanged. Defaults opt managed Anthropic models in:

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Enables cache marker normalization. Set to `false` to emit no strategy markers. |
| `system_ttl` | string | `"one_hour"` | TTL for `history[0]` when it is a non-empty system turn. |
| `tools_ttl` | string | `"one_hour"` | TTL for the final emitted tool definition. |
| `rolling_ttl` | string | `"five_minutes"` | TTL for the last eligible non-empty history turn, walking backward over empty and tool-result-only turns. |

TTL values are `"one_hour"` and `"five_minutes"`. Configuration is rejected if
a one-hour marker would follow a five-minute marker in Oven's actual marker
order: tools, system blocks, non-system history, then the optional request-level
marker. The rolling marker is recomputed on every request.

The lower-level Anthropic adaptor option `cache_ttl` remains supported. It maps
to Oven's request-level `cache_control`, which is a final, optional fourth
breakpoint; `cache_strategy` maps to the three normalized tool/message
breakpoints above.

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
| `max_concurrency` | integer | `4` | Maximum concurrently running root-level background delegations. Excess calls queue FIFO, up to `4 × max_concurrency`; a full queue rejects admission. Foreground and nested delegations bypass this queue. A value of `0` is rejected. |
| `max_resident_subagents` | integer | `20` | Soft trigger for resident delegated sessions. Above this count, the janitor evicts eligible idle children oldest-first until the count reaches the trigger or no eligible child remains. Recently active children may keep residency above the trigger. |
| `idle_eviction_after` | duration string | `"1h"` | Minimum time since a delegated session's last run ended before it can be evicted. Compact `ms`, `s`, `m`, `h`, and `d` suffixes are accepted. |

## `[pricing.models."<provider/model>"]`

Pricing overrides are empty by default. Managed models use prices from the
selected models.dev catalog. Add an entry for a custom model absent from the
catalog, or to override catalog prices with the provider terms that apply to
your account. Catalog context tiers are selected independently for each request
from its reported input-token count. Keys are canonical model identities,
including the provider ID.

```toml
[pricing.models."custom.example/model-name"]
input_per_million_usd = "1.25"
output_per_million_usd = "5.0"
```

All fields are optional quoted, finite, nonnegative decimal USD rates per
million tokens. Quoting preserves the authored decimal exactly:

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

## `[mcp.servers.<name>]`

MCP server definitions are layered by server name. A workspace definition
replaces the same user-level server. See the [MCP guide](../guide/mcp.md) for
permission and naming behavior.

| Key | Type | Default | Description |
|---|---|---|---|
| `command` | string | *(none)* | Stdio server executable. Exactly one of `command` or `url` is required. |
| `args` | array of strings | empty | Command arguments; valid only with `command`. |
| `env` | map of strings | empty | Command environment additions; valid only with `command`. |
| `cwd` | string | *(none)* | Child working directory; valid only with `command`. |
| `url` | string | *(none)* | Absolute HTTP or HTTPS Streamable HTTP endpoint. Exactly one of `url` or `command` is required. |
| `headers` | map of strings | empty | Static request headers; valid only with `url`. |
| `oauth` | boolean or table | auto | Streamable HTTP OAuth. Omit, set `true`, or use `{}` for reactive OAuth; set `false` to disable it. A table accepts optional `client_id`, `client_secret`, `client_metadata_url`, and `scopes`. A static `Authorization` header takes precedence. |
| `enabled` | boolean | `true` | Whether the server may connect. |
| `lazy` | boolean | `false` | Defer connection and tool listing until first named use. |
| `timeout_ms` | integer | `30000` | Positive timeout for connect, list, and call operations. |

## `[plugins.<name>]`

Plugin definitions are layered by plugin name. A workspace definition replaces
the complete same-name user definition without merging nested fields. The
replacement keeps that user's position in authored order, which is also the
interception order; workspace-only plugins append in workspace-authored order.
See the [Plugins guide](../guide/plugins.md) for protocol, permission, and tool
precedence behavior.

| Key | Type | Default | Description |
|---|---|---|---|
| `command` | string | *(required)* | Plugin executable. |
| `args` | array of strings | empty | Command arguments. |
| `env` | map of strings | empty | Complete child environment; inherited variables are cleared. |
| `cwd` | string | *(none)* | Child working directory. |
| `enabled` | boolean | `true` | Whether the plugin starts with the engine. |
| `interception_timeout_ms` | integer | `2000` | Positive timeout for interception requests. |
| `startup_timeout_ms` | integer | `10000` | Positive timeout for initialization. |
| `shutdown_grace_ms` | integer | `3000` | Positive graceful shutdown period before termination. |
| `tool_timeout_ms` | integer | `30000` | Positive timeout for each plugin tool call. |

Plugin entries reject unknown fields. `command` must be present and nonempty,
`cwd` must be nonempty when set, and every timeout must be greater than zero.
These rules apply even when `enabled = false`; disabling an entry prevents
startup but does not bypass configuration validation.

The child receives only the variables in `env`. It does not inherit the
engine's environment, including `PATH`; use an absolute `command` and configure
`PATH` explicitly when the executable or its children need it.

Complete example:

```toml
[plugins.issue_tracker]
command = "/opt/cookie-plugins/issue-tracker" # Required executable path.
args = ["--stdio"]                            # Optional arguments.
env = { PATH = "/usr/bin:/bin", MODE = "local" } # Complete child environment.
cwd = "/workspace"                           # Optional child working directory.
enabled = true                                # Start when the engine opens.
interception_timeout_ms = 2000                # Per interception hook.
startup_timeout_ms = 10000                    # Initialization handshake.
shutdown_grace_ms = 3000                      # Grace before termination.
tool_timeout_ms = 30000                       # Per plugin tool call.
```

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
| `setup` | map of string values | empty | Setup fields the provider recipe requires (for example `project`, `location`, `region`, `resource_name`). Native Azure Responses compaction also requires `model`, `version`, and `deployment_type`. Interpolates `${env:NAME}`. |
| `api_key` | string | *(none)* | Single-secret default auth. Allowed only for providers whose default method is an unambiguous single API key. Interpolates `${env:NAME}`. |
| `auth_override` | table | *(none)* | Explicit auth method override. Mutually exclusive with `api_key`. |
| `shape` | string | catalog shape | `"chat"` or `"responses"` model shape override. |
| `model_overrides` | map | empty | Sparse per-model overrides: `enabled`, `display_name`, `defaults`, `variants`, `default_variant`, `shape`, `compaction`. Cannot invent a model absent from the catalog or directly change capabilities. |

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

## Agent documents

Agent files are named `<agent-id>.md`; the filename is the agent ID
(`^[a-z0-9](?:[a-z0-9]|-(?=[a-z0-9]))*$`, at most 64 characters). Each file has
YAML frontmatter between `---` fences and a nonempty Markdown body used as the
system prompt. Frontmatter at most 128 KiB, body at most 128 KiB; nested list or
map size is capped at 256 entries and depth at 16. YAML anchors, aliases, tags,
and merge keys are rejected, as is any `${env:` text.

| Frontmatter key | Type | Default | Description |
|---|---|---|---|
| `description` | string | *(required)* | Nonempty display description, at most 512 bytes and without control characters. |
| `mode` | string | *(required)* | `primary`, `subagent`, `all`, or `internal`. |
| `enabled` | boolean | *(required)* | Controls root, delegation-target, or internal-backend eligibility. |
| `models` | array | *(required for `primary`)* | Ordered `{ model, variant? }` model chain. `${parent_model}` is internal-only. |
| `limits` | table | mode-specific | Supports `max_output_tokens` in every mode and `timeout_ms` for internal agents only. |
| `permissions` | table | `{}` | Ordered action permission map with at most 256 rules. |

For `primary`, `subagent`, and `all`, omitted or zero `max_output_tokens` adds no
document cap. A nonzero value is combined with the model's output limit by taking
the smaller value. Internal agents default to 2,048 output tokens; explicit zero
removes that document cap. A nonzero `timeout_ms` is a hard error outside
`internal` mode. Internal agents default to 30,000 ms when it is omitted or zero.

The former `model_fallback` key is a targeted hard error; use `models`. The former
`limits.max_input_tokens` key is removed. Internal input budgets are derived per
resolved model from its context limit minus its effective output reserve, with a
16,384-token fallback when context is unknown; an undersized model is skipped in
favor of the next candidate.

For durable wire-schema compatibility, the event emitted when advancing through
the chain remains named `model_fallback`; only agent frontmatter uses `models`.

See [Agents](../guide/agents.md) for the full frontmatter reference and reserved
IDs.

## TUI configuration (`tui.toml`)

The TUI config is independent of the runtime config: no workspace layer, no
environment-variable override, no engine involvement. A missing file uses
defaults; unknown keys and malformed values are rejected naming the path and key.

| Key | Type | Default | Description |
|---|---|---|---|
| `minimum_event_level` | string | `"warning"` | Minimum diagnostic event level rendered in the conversation pane. One of `"debug"`, `"info"`, `"warning"`, `"error"`. Rows below the threshold stay in the session projection and reappear when the level is lowered with `/events` at runtime (view-only; the file is never rewritten). |
| `theme` | string | `"auto"` | `"auto"`, `"default"`, `"dark"` (the bakery palette's dark roast), `"mono"`, or `"high-contrast"`. Unset and `"auto"` query the terminal background with OSC 11, then check the last `COLORFGBG` field, then fall back to `"default"`. Dark is curated; HighContrast uses terminal-driven bright ANSI colors. Precedence: this key, then `COOKIE_THEME`, then automatic detection. `NO_COLOR` and `TERM=dumb` always force monochrome after selection. |

See `docs/tui.toml.example` in the repository for a fully commented example.

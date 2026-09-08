# config.toml

The engine reads `~/.cookie-agent/config.toml`, then
`<exact-cwd>/.cookie-agent/config.toml`, over built-in defaults. Neither file is
required; an empty file is valid. This complete example changes one setting:

```toml
[model_retry]
standard_retries = 1
```

## Locations and inheritance

The user layer and the exact working directory's `.cookie-agent` layer are both
optional. Configuration is loaded from the exact working directory only; there
is no upward search. Within a layer, `config.toml` and the `agents/` directory
are optional. A same-ID workspace provider, MCP server, plugin, or agent replaces
the complete user entry; nested fields never merge.

An authored settings table replaces the lower layer's whole table. Omitted
fields in that replacement use defaults, not the lower file's values. For
example, the snippet above also resets `overload_retries` and
`backoff_ceiling_ms` to their defaults. An omitted table inherits unchanged.
Global [headers](../engine/headers.md) are the exception: names merge and empty
values delete inherited headers. Model and variant sparse inheritance happens
within one provider definition, not between files.

Agents are separate [Markdown documents](agents.md); [skills](skills.md) have
their own discovery rules. The client loads [tui.toml](../tui/configuration.md)
independently and has no workspace layer.

## Accepted top-level keys

Every key is optional. Unknown keys, including removed top-level `pricing`, fail.

| Key | Canonical settings page |
|---|---|
| `server` | [Server](../engine/server.md) |
| `tool_output` | [Tool Output](../engine/tool_output.md) |
| `agent_md` | [AGENTS.md Context](../engine/agent_md.md) |
| `approval` | [Approval](../engine/approval.md) |
| `model_retry` | [Model Retry](../engine/model_retry.md) |
| `context_compaction` | [Context Compaction](../engine/context_compaction.md) |
| `session_title` | [Session Title](../engine/session_title.md) |
| `delegation` | [Delegation](../engine/delegation.md) |
| `headers` | [Request Header](../engine/headers.md) |
| `providers` | [Providers](providers.md) |
| `mcp` | [MCP](mcp.md) |
| `plugins` | [Plugins](plugins.md) |

## Strictness and limits

Every authored file is parsed strictly. Unknown keys, leftover `schema` or
`schema_version` keys, wrong types, and malformed content are hard errors with an
actionable path, key, and line where available. No authored-file migrations or
unknown-field ignores exist. Decoded values that hold secrets are zeroized when
the load completes.

TOML-level limits (enforced before deserialization):

- Configuration file at most 1 MiB; agent document at most 256 KiB.
- Maximum TOML nesting depth 32; at most 4096 entries per table or array.
- String values at most 256 KiB; TOML datetimes rejected; floats must be finite.

## Environment interpolation

`${env:NAME}` is single-pass interpolation. `${env:NAME:-fallback}` uses the
fallback when the variable is unset; the fallback may be empty and is split on
the first `:-`. `$$` emits a literal `$`. Interpolation is allowed only in these
paths:

- `providers.<id>.endpoint`
- `providers.<id>.base_url`
- `providers.<id>.setup.<field>`
- `providers.<id>.api_key`
- `providers.<id>.auth_override.values.<field>`
- `providers.<id>.auth.values.<field>`
- `headers.<name>`
- `providers.<id>.headers.<name>`
- `providers.<id>.models.<model>.headers.<name>`
- model and model-override `variants.<variant>.headers.<name>`

`NAME` must be `[A-Z_][A-Z0-9_]*` (uppercase letters, digits, underscore, starting
with a letter or underscore). A missing variable, a non-UTF-8 value, or an
interpolation used anywhere else is a load error. A missing variable without a
default remains an error. Header templates are validated at load time but kept
unresolved for request-time expansion and stable manifests. Interpolation is not
available in permission patterns or agent documents.

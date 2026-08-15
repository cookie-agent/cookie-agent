# Configuration

cookie agent reads three independent configuration surfaces:

| Surface | Location | Schema |
|---|---|---|
| Runtime, providers, and MCP servers | `~/.config/cookie_agent/config.toml` and `<cwd>/.cookie-agent/config.toml` | 10 |
| Agents | `~/.config/cookie_agent/agents/<agent-id>.md` and `<cwd>/.cookie-agent/agents/<agent-id>.md` | 4 |
| TUI | `$XDG_CONFIG_HOME/cookie_agent/tui.toml` or `~/.config/cookie_agent/tui.toml` | 1 |

This page covers where configuration lives and how it behaves. For the complete
key-by-key reference, see [Configuration Reference](../reference/configuration.md).

## Locations and layering

```text
~/.config/cookie_agent/config.toml
~/.config/cookie_agent/agents/<agent-id>.md
<exact-cwd>/.cookie-agent/config.toml
<exact-cwd>/.cookie-agent/agents/<agent-id>.md
```

There is no upward workspace search: configuration is loaded from the exact
working directory the daemon started in. The user layer and the workspace layer
are both optional. Workspace settings replace the corresponding user settings. A
same-ID workspace provider, MCP server, or agent replaces the complete user definition;
nested fields never merge. Unknown fields, wrong schema versions, and malformed
values are rejected.

## Minimum runtime configuration

`schema_version = 10` is required; every other top-level section is optional.
An empty provider map is valid.

```toml
schema_version = 10

[providers]
```

If the global provider store is also empty, the TUI starts in setup mode and
keeps `/connect` available.

## Runtime settings

All runtime sections default to safe values; you only need to write the ones you
want to change:

```toml
schema_version = 10

[server]
host = "127.0.0.1"
port = 7419

[tool_output]
max_lines = 2000
max_bytes = 51200

[approval]
timeout_ms = 30000

[context_compaction]
auto = true
trigger = { percent = 70 }
max_summary_bytes = 262144

[session_title]
max_chars = 80
max_input_messages = 4
generate_on_first_turn = true
fallback_to_input_excerpt = true

[delegation]
max_depth = 3
max_concurrency = 4

[providers]
```

Validation rules that apply regardless of what you set:

- The `cookie` binary requires `server.host` to be exactly `127.0.0.1`.
- Positive limits are required everywhere; compaction trigger percentages must
  be from 1 through 99, and `context_compaction.max_summary_bytes` may not exceed
  2 MiB.
- `delegation.max_concurrency` defaults to `4`; `0` is rejected. Excess
  root-level background delegations queue up to four times the concurrency
  limit. Foreground and nested delegations bypass the queue.
- Provider definitions are validated per provider ID (see
  [Providers](providers.md)).
- MCP servers require exactly one stdio command or Streamable HTTP URL (see
  [MCP servers](mcp.md)).

## Environment interpolation

`${env:NAME}` is a single-pass interpolation available only in approved provider
secret values (`api_key`, `auth`/`auth_override` credential values, `setup`
fields) and authored endpoint strings (`endpoint`, `base_url`). It is not
available in permission patterns, agent documents, or custom static headers.
Interpolation is applied at load time; a missing variable fails startup with the
offending path.

```toml
[providers.openai]
source = "models_dev"
api_key = "${env:OPENAI_API_KEY}"
```

Prefer interpolation or `/connect` over plaintext credentials, and never commit
`.env` or a credential-bearing config.

## Agent documents

Agents are Markdown files whose filename is the agent ID. Each file has schema-4
YAML frontmatter and a nonempty Markdown body used as the system prompt:

```markdown
---
schema: 4
description: Reviews changes for correctness
mode: subagent
enabled: true
model_fallback:
  - { model: "openai/gpt-5", variant: null }
limits:
  timeout_ms: 30000
  max_input_tokens: 16384
  max_output_tokens: 2048
permissions:
  read: allow
  write: deny
  bash:
    "git diff*": allow
    "*": ask
  delegate: deny
---
Review the requested change and report concrete findings.
```

Modes are `primary`, `subagent`, `all`, and `internal`. Internal agents are
engine-only and cannot be selected as roots or delegation targets. The reserved
built-ins are `approval`, `compaction`, and `title`; same-ID authored internal
documents replace them through normal layering, and only internal agents may use
`${parent_model}` in a model fallback.

See [Agents](agents.md) for the full frontmatter reference and the built-in
internal agents.

## TUI configuration

The TUI reads `tui.toml` from `$XDG_CONFIG_HOME/cookie_agent/` when XDG is set,
otherwise `~/.config/cookie_agent/`. It is independent of the runtime config:
there is no workspace layer and no environment-variable override. A missing file
uses defaults; unknown keys or malformed values are rejected naming the path and
key.

```toml
minimum_event_level = "warning"   # debug | info | warning | error
theme = "default"                 # default | mono | high-contrast
```

`theme` takes precedence over `COOKIE_THEME` and terminal detection, but
`NO_COLOR` and `TERM=dumb` always force monochrome. See
`docs/tui.toml.example` for the fully commented example.

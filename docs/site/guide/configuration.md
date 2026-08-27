# Configuration

cookie agent reads four independent configuration surfaces:

| Surface | Location |
|---|---|
| Runtime, providers, MCP servers, and plugins | `~/.cookie-agent/config.toml` and `<cwd>/.cookie-agent/config.toml` |
| Agents | `~/.cookie-agent/agents/<agent-id>.md` and `<cwd>/.cookie-agent/agents/<agent-id>.md` |
| Skills | `~/.cookie-agent/skills/<name>/SKILL.md` and `.cookie-agent/skills/<name>/SKILL.md` from cwd to worktree root |
| TUI | `~/.cookie-agent/tui.toml` |

This page covers where configuration lives and how it behaves. For the complete
key-by-key reference, see [Configuration Reference](../reference/configuration.md).
Examples on this page target the user layer unless workspace behavior is named.
The workspace file accepts the same syntax.

## Locations and layering

```text
~/.cookie-agent/config.toml
~/.cookie-agent/agents/<agent-id>.md
<exact-cwd>/.cookie-agent/config.toml
<exact-cwd>/.cookie-agent/agents/<agent-id>.md
```

There is no upward workspace search: configuration is loaded from the exact
working directory the daemon started in. The user layer and the workspace layer
are both optional. Workspace settings replace the corresponding user settings. A
same-ID workspace provider, MCP server, plugin, or agent replaces the complete user
definition; nested fields never merge. Every authored file is parsed strictly.
Unknown fields, leftover schema/version fields, wrong types, and malformed content
are hard errors; there are no migrations or silently ignored fields.

## Optional runtime configuration

No configuration file is required. For common per-user settings, create
`~/.cookie-agent/config.toml`. Every top-level section is optional, and an empty
file or provider map is valid. Use `<cwd>/.cookie-agent/config.toml` only for
workspace-specific overrides.

```toml

[providers]
```

If the global provider store is also empty, the TUI starts in setup mode and
keeps `/connect` available.

## Runtime settings

All runtime sections default to safe values; you only need to write the ones you
want to change:

```toml

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

## Plugins

Plugins use the same user and exact-workspace `config.toml` layers as runtime
settings. A minimal executable plugin entry is:

```toml
[plugins.issue_tracker]
command = "/opt/cookie-plugins/issue-tracker"
args = ["--stdio"]
```

A same-name workspace entry replaces the complete user entry. Set `enabled =
false` to keep a plugin from starting; its command, timeouts, and other fields
are still validated. See [Plugins](plugins.md) for installation, permissions,
and runtime behavior, or [Plugin development](../development/plugins.md) for
the extension protocol and capabilities.

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

## Prompt caching

Runtime prompt-cache policy is provider-specific. This GPT-5.6+ example keeps
OpenAI's implicit breakpoint and adds stable system and latest-user boundaries:

```toml
[prompt_caching.openai]
prompt_cache_key = "workspace-${session_id}"
mode = "implicit"
ttl = "30m"
system = true
rolling = true
```

Use `mode = "explicit"` when only the selected `system` and `rolling` boundaries
should write. OpenAI and Azure OpenAI do not support a Cookie agent `tools`
placement because provider breakpoints attach to content parts, not tool
definitions. See the [configuration reference](../reference/configuration.md#prompt_caching)
for legacy retention, placement, and provider-specific rules.

## Agent documents

Agents are Markdown files whose filename is the agent ID. Each file has strict
YAML frontmatter and a nonempty Markdown body used as the system prompt:

```markdown
---
description: Reviews changes for correctness
mode: subagent
enabled: true
models:
  - model: "openai/gpt-5.6"
    variant: null
    cache:
      openai:
        mode: explicit
        ttl: 30m
        system: true
        rolling: true
limits:
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
`${parent_model}` in the `models` list.

See [Agents](agents.md) for the full frontmatter reference and the built-in
internal agents.

## TUI configuration

The TUI reads `~/.cookie-agent/tui.toml`. It is independent of the runtime
config: there is no workspace layer and no environment-variable override. A
missing file uses defaults; unknown keys or malformed values are rejected naming
the path and key.

```toml
minimum_event_level = "warning"   # debug | info | warning | error
theme = "auto"                    # auto | default | dark | mono | high-contrast
```

`theme` takes precedence over `COOKIE_THEME` and terminal detection, but
leaving it unset is equivalent to `"auto"`. Automatic selection queries the
terminal background with OSC 11, then checks `COLORFGBG`, then falls back to the
light `default` theme. `NO_COLOR` and `TERM=dumb` always force monochrome. See
`docs/tui.toml.example` for the fully commented example.

# Configuration

## Locations and layering

cookie agent reads two optional authored layers:

```text
~/.config/cookie_agent/config.toml
~/.config/cookie_agent/agents/<agent-id>.md
<exact-cwd>/.cookie-agent/config.toml
<exact-cwd>/.cookie-agent/agents/<agent-id>.md
```

There is no upward workspace search. Workspace settings replace corresponding
user settings. A same-ID workspace provider or agent replaces the complete user
definition; nested fields do not merge. Unknown fields and wrong schema versions
are rejected.

## Runtime settings

`schema_version = 10` is required. All other top-level sections are optional.

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
buffer_tokens = 33000
max_summary_bytes = 262144

[session_title]
max_chars = 80
max_input_messages = 4
generate_on_first_turn = true
fallback_to_input_excerpt = true

[delegation]
max_depth = 3
# max_concurrency = 4 # omitted means unlimited

[providers]
```

The daemon requires `server.host` to be exactly `127.0.0.1`. Positive limits are
required; `max_summary_bytes` may not exceed 2 MiB. Provider definitions are
covered in [Providers](providers.md).

## Environment interpolation

`${env:NAME}` is a single-pass interpolation available only in approved secret
values and authored endpoint strings. It is not available in permission
patterns, agent documents, or custom static headers. Prefer interpolation or
`/connect` over plaintext credentials.

## Agent documents

The filename is the agent ID. Each Markdown file has schema-4 YAML frontmatter
and a nonempty Markdown body used as the system prompt:

```markdown
---
schema: 4
description: Reviews changes for correctness
mode: subagent
enabled: true
model_fallback:
  - model: openai/gpt-5
    variant: null
limits:
  timeout_ms: 30000
  max_input_tokens: 16384
  max_output_tokens: 2048
tools: [read, bash]
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
documents replace them through normal layering. Only internal agents may use
`${parent_model}` in a model fallback.

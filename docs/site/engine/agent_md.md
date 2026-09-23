# AGENTS.md Context [agent_md]

Complete `config.toml` to disable automatic repository context:

```toml
[agent_md]
enabled = false
```

An omitted table inherits; an authored table replaces the lower table and uses
defaults for omitted fields. Unknown fields fail. See
[config.toml](../guide/configuration.md).

Repository-authored context is untrusted input. Review it before running in an
unfamiliar workspace; admission is independent of tool read permissions. See
[security boundaries](../guide/security.md#agentsmd-context-files).

Controls root-run `AGENTS.md` discovery documented in
[Agents](../guide/agents.md#agentsmd-context). This is the global default; an
agent document may set `agent_md: true` or `agent_md: false` to override it for
that agent, for example to keep a lean agent's context free of repository
instructions:

```markdown
---
description: Minimal-context agent
mode: primary
enabled: true
models:
  - { model: "openai/gpt-5", variant: null }
agent_md: false
---
Answer using only what the user provides.
```

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Load `AGENTS.md` context files at each root run start. Delegated and internal agents remain excluded. An agent document's `agent_md` field, when present, overrides this for that agent's root runs. |

Files larger than 2 MiB (`2097152` bytes) are skipped entirely and surfaced as
an `AgentMdSkipped` warning event; they are never partially loaded or
truncated. Loaded entries are rendered into a single user context turn wrapped
in a `<system-reminder>` block, with each file delimited as
`<contents from="/abs/path/AGENTS.md">…</contents>`.

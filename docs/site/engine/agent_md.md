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
[Agents](../guide/agents.md#agentsmd-context).

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Load `AGENTS.md` context files at each root run start. Delegated and internal agents remain excluded. |

Files larger than 2 MiB (`2097152` bytes) are skipped entirely and surfaced as
an `AgentMdSkipped` warning event; they are never partially loaded or
truncated. Loaded entries are rendered into a single user context turn wrapped
in a `<system-reminder>` block, with each file delimited as
`<contents from="/abs/path/AGENTS.md">…</contents>`.

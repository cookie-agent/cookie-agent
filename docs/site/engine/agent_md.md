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
| `max_bytes` | integer | `32768` (`32 * 1024`) | Maximum UTF-8 bytes retained from each discovered file. Longer content is truncated on a UTF-8 boundary and records its original size. Must be from 1 through `2097152` (2 MiB). |

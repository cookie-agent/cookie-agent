# ACP Agents [acp]

Control how the daemon runs ACP agents, the external coding agents such as
Claude Code, Codex, and Gemini CLI that runtimes host and that take a
session's turns over the Agent Client Protocol (see the
[ACP agents guide](../extend/acp-agents.md)). The agents themselves are
declared on their runtime, under
[`[runtimes.<id>.acp.agents.<agent>]`](../runtimes/tools.md#acp-agents).
Complete `config.toml`:

```toml
[acp]
idle_shutdown_ms = 300000
```

An omitted table inherits; an authored table replaces the lower table and
uses defaults for omitted fields. Unknown fields fail; see
[config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `idle_shutdown_ms` | integer | `300000` | How long an agent's process stays up with no turn in progress before the daemon stops it. Each agent runs one process per runtime, shared by its sessions; the next turn starts it again and resumes the session's ACP session. Must be at least 1. |

The table applies at once when the configuration changes: the next idle
period reads it.

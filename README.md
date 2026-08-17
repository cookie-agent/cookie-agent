# cookie agent

<p align="center"><img src="assets/logo.png" alt="cookie agent logo" width="256"></p>

Simple and mighty coding agent.

[![Documentation](https://img.shields.io/badge/docs-cookie--agent.github.io-blue)](https://cookie-agent.github.io/cookie-agent/)

cookie agent is a Rust-powered terminal coding agent. The surface stays
minimalistic — a daemon, a TUI, a headless runner — while the batteries are
included underneath: providers, permissions, delegation, sessions, plugins,
and observability are all built in.

## Highlights

- **Subagent delegation** — agents delegate to other agents, with background
  tasks that keep running while you keep working.
- **MCP servers** — attach local or remote MCP tool servers, including
  streamable HTTP servers with OAuth.
- **Plugins** — run out-of-process JSON-RPC extensions that add tools, observe
  events, and intercept tool calls, agent starts, and compaction.
- **Permissions** — every capability is opt-in. Agents see only the tools
  their permission map allows, with resource-pattern rules and ask/allow/deny
  effects.
- **Agent documents** — Markdown files with YAML frontmatter define personas,
  models, and permissions. Author your own or use the built-in `default`.
- **Skill loading** — strict user/project `SKILL.md` discovery, permission-aware
  model and user invocation, turn-scoped tool grants, and forked skill context.
- **Observability** — per-session token usage, prompt-cache hit rates, and
  estimated cost rollups.
- **Headless runs** — `cookie run` executes a prompt without the TUI, suitable
  for CI and scripting.
- **Durable sessions** — versionless persisted history with best-effort reading
  and automatic context compaction.

## Quick start

Requires Rust 1.88 or newer. Build from source:

```sh
cargo build --locked -p cookie_agent
```

Running the binary starts a local daemon and opens the TUI. In the TUI, type
`/connect` to store a provider connection, then start a session.

```sh
./target/debug/cookie
```

For headless use:

```sh
cookie run "Review this workspace"
```

## Documentation

Setup, configuration, and task-oriented guides are at
[cookie-agent.github.io/cookie-agent](https://cookie-agent.github.io/cookie-agent/),
including [Getting Started](https://cookie-agent.github.io/cookie-agent/getting-started/),
[Providers](https://cookie-agent.github.io/cookie-agent/guide/providers/),
[Plugins](https://cookie-agent.github.io/cookie-agent/guide/plugins/), and
[Permissions](https://cookie-agent.github.io/cookie-agent/guide/permissions/).

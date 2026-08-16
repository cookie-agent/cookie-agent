# cookie agent

cookie agent is a subagent-first coding harness. A local daemon owns provider
connections, sessions, model execution, permissions, and persistence; the
terminal UI communicates with it over the versioned JSON-RPC protocol.

Writers use current schemas. Readers reopen event schemas 15-17 and delegation
journal schemas 11-14; other persisted and wire surfaces remain current-only.
[Architecture](architecture.md) describes how the pieces fit together; this site
turns that implementation into task-oriented guides and reference material.

## Quickstart

You need Rust 1.88 or newer and Python 3 for the documentation tooling.

```sh
git clone https://github.com/cookie-agent/cookie-agent.git
cd cookie-agent
mkdir -p .cookie-agent
printf '[providers]\n' > .cookie-agent/config.toml
cargo run --locked -p cookie_agent -- daemon
```

In another terminal, open the TUI:

```sh
cargo run --locked -p cookie_agent -- attach
```

Type `/connect` to store a managed provider connection. Provider setup and
credentials are per-user and shared across workspaces. Credentials are checked
when the provider is first used, not during the connect flow.

See [Getting Started](getting-started.md) for configuration and first-run
details, [Providers](guide/providers.md) for managed and custom provider
options, and [Configuration](reference/configuration.md) for every configurable
item.

!!! warning "Keep credentials out of Git"
    Prefer `/connect` or `${env:NAME}` interpolation. Do not commit `.env`, a
    credential-bearing config, or provider-store data.

## Workspace crates

The Rust workspace contains nine crates: `config`, `cookie_agent`, `engine`,
`identity`, `models`, `protocol`, `server`, `tools`, and `tui`. Browse their
public interfaces in the [Rust API documentation](reference/api.md).

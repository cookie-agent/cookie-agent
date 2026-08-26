# cookie agent

cookie agent is a subagent-first coding harness. A local daemon owns provider
connections, sessions, model execution, permissions, and persistence; the
terminal UI communicates with it over the versioned JSON-RPC protocol.

Session history uses versionless events with best-effort reading; other
persisted and wire surfaces remain current-only.
[Architecture](architecture.md) describes how the pieces fit together; this site
turns that implementation into task-oriented guides and reference material.

## Quickstart

First [install cookie agent](install.md), then run it from the workspace where it
should operate:

```sh
mkdir -p .cookie-agent
printf '[providers]\n' > .cookie-agent/config.toml
cookie
```

Type `/connect` to store a managed provider connection. Provider setup and
credentials are per-user and shared across workspaces. Credentials are checked
when the provider is first used, not during the connect flow.

The [installation guide](install.md#quick-start) continues through configuration
and the first run. See [Providers](guide/providers.md) for managed and custom
provider options, [Plugins](guide/plugins.md) for executable extensions, and
[Configuration](reference/configuration.md) for every configurable item.

!!! warning "Keep credentials out of Git"
    Prefer `/connect` or `${env:NAME}` interpolation. Do not commit `.env`, a
    credential-bearing config, or provider-store data.

## Development

See [Building and testing](development/building.md) for the Rust toolchain and
required gates, [Plugin development](development/plugins.md) for extension
authoring, and the [Rust API documentation](reference/api.md) for public
workspace interfaces.

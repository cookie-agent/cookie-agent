# cookie agent

cookie agent is a subagent-first coding harness. A daemon owns provider
connections, sessions, agents, model execution, permissions, memory, and
persistence; runtimes provide its tools, and clients (the terminal UI, the web
client, and the command line) communicate with it over the versioned JSON-RPC
client protocol.

The client, plugin, and runtime protocols and session history are frozen
semver surfaces; see [Compatibility](reference/compatibility.md).
[Architecture](architecture.md) describes how the pieces fit together; this site
turns that implementation into task-oriented guides and reference material.

## Quickstart

First [install cookie agent](install.md), then run it from the workspace where it
should operate:

```sh
cookie
```

No configuration file is needed to start. If no provider is available, the TUI
opens in setup mode; type `/connect` to store a managed provider connection.
Provider setup and credentials are per-user and shared across workspaces.
Credentials are checked when the provider is first used, not during the connect
flow.

The [installation guide](install.md#connect-a-model) ends with provider setup;
[Run](clients/tui.md) covers interactive and headless operation. See [Providers](extend/providers.md) for managed and custom
provider options, [Plugins](extend/plugins.md) for executable extensions, and
[Configuration](engine/configuration.md) for every configurable item.

!!! warning "Keep credentials out of Git"
    Prefer `/connect` or `${env:NAME}` interpolation. Do not commit `.env`, a
    credential-bearing config, or provider-store data.

## Development

See [Building and testing](development/building.md) for the Rust toolchain and
required gates, [Plugin development](development/plugins.md) for extension
authoring, and the [Rust API documentation](reference/api.md) for public
workspace interfaces.

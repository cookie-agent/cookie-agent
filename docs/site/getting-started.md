# Getting Started

## Install

The workspace requires Rust 1.88 or newer. Build the `cookie` binary from the
repository:

```sh
cargo build --locked -p cookie_agent
```

The binary is `target/debug/cookie`. Running it without a subcommand starts a
local daemon and opens the TUI. `cookie daemon` runs only the daemon, and
`cookie attach` attaches a TUI to an existing daemon.

## Create a workspace configuration

Configuration is loaded from the exact working directory; there is no upward
search. Create `.cookie-agent/config.toml`:

```toml
schema_version = 10

[providers]
```

An empty provider map is valid. If the global provider store is also empty, the
TUI starts in setup mode and keeps `/connect` available.

## Configure a provider

Start the daemon from the workspace:

```sh
cargo run --locked -p cookie_agent -- daemon
```

In another terminal, attach the TUI, then type `/connect`, select a managed
provider, and fill in the provider's recipe-defined setup and credential fields.
The durable store is global to the user, so other workspaces can use the same
compatible connection. The form does not contact the provider; the first model
request verifies the credentials.

For environment-backed authored configuration instead, use a managed provider
entry:

```toml
schema_version = 10

[providers.openai]
source = "models_dev"
api_key = "${env:OPENAI_API_KEY}"
```

Export the variable before launching:

```sh
export OPENAI_API_KEY='your-key'
cargo run --locked -p cookie_agent
```

See [Providers](guide/providers.md) for precedence rules and custom providers.

## First run

When at least one model is available, select an agent and model if needed, type
a request in the composer, and press Enter. If no authored root agent is
runnable, the engine supplies the built-in `default` coding agent.

Useful first commands are `/help`, `/sessions`, `/new`, `/compact`, and
`/cancel`. The [TUI guide](guide/tui.md) covers editing, steering, approvals,
selection, and message actions. [Agents](guide/agents.md) explains the built-in
internal agents and how to author your own.

## Separate daemon and TUI

The daemon binds to `127.0.0.1:7419` by default:

```sh
cargo run --locked -p cookie_agent -- daemon
```

Attach from another terminal:

```sh
cargo run --locked -p cookie_agent -- attach
```

The attach URL defaults to `ws://127.0.0.1:7419/ws` and may be changed with
`--url`. Only loopback WebSocket URLs with the exact `/ws` path are accepted.

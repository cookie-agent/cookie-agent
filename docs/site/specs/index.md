# Specification Status

User-facing configuration pages describe the current source tree. Specifications
also preserve proposals and design decisions; a draft is not an installed-binary
configuration reference.

## Implemented

| Specification | Status |
|---|---|
| [Web client and local daemon use](web-client.md) | Implemented (2026-10-20); Leptos/WebAssembly client styled with Tailwind, sharing the protocol client, reducer, and panel rules with the TUI, served by `cookie daemon` or any static host, full TUI parity except MCP OAuth and offering this machine as a runtime, phone layout and home-screen manifest, headless-Firefox suites in CI; plain `cookie` connects to a running local daemon (phase L) |
| [Memory](memory.md) | Implemented; `user`, `agent`, `machine`, and `project` memory layers as Markdown files in the `global` runtime, machines as a daemon concept, the memory tools and snapshot, `memory.*` (client protocol 1.4.0), TUI `/memory`, and `cookie memory` |
| [Background tool calls](background-tools.md) | Implemented (2026-10-16); host-owned background calls and per-turn subagent reports |
| [Runtimes](runtimes.md) | Implemented; daemon as gateway with global and coding runtimes, attachments, the runtime protocol, remote runtimes from `[runtimes.<id>]` templates, and template MCP servers; manual labpc check (ssh template and dial-in) passed 2026-10-20 |
| [Persistent daemon, dial-in runtimes, and runtime capabilities](runtime-capabilities.md) | Implemented; capability attachments, dial-in runtimes (the TUI embeds one when remote), listening daemon with TLS, directory-independent daemon; landed in the pre-1.0 client protocol 45 and runtime protocol 5; manual labpc check (ssh template and dial-in) passed 2026-10-20 |
| [Live engine configuration](engine-configuration.md) | Implemented; `config.get`/`config.apply`/`config.reload` (pre-1.0 client protocol 48), live reload of settings, agents, providers, headers, and MCP servers, TUI `/config` and `cookie config`, plus provider-connect fixes |
| [Compatibility v1](compatibility-v1.md) | Implemented; client, plugin, and runtime protocols frozen at 1.0.0 with an enforced compatibility checker, tolerant session history (format 1) with conversion, golden logs and frozen corpora, user media input, and structured tool display |
| [Plugins: tools, hooks, events, and producers](plugins-v1.md) | Implemented; plugin tools over the runtime tool messages, plugin protocol 1 on the runtime stack, `plugins.list`/`plugins.restart`, landed in the pre-1.0 client protocol 44 |
| [External producers](external-producers.md) | Implemented; named producers for plugins and runtimes under one `external { source, name }` owner |
| [Goal mode and producers](goal-mode.md) | Implemented, approved product contract |
| [Subagent handles](subagent-handles.md) | Implemented (2026-09-17); short tree-unique handles for model-facing session references, with the later tool changes the status line lists |
| [Agent messaging](agent-messaging.md) | Implemented; governing contract for `send_message` agent-to-agent messaging |
| [Tree-scoped session ownership lock](tree-ownership-lock.md) | Implemented; one `owner.lock` per root session tree, no lock files for delegated children |
| [Tree-local sessions](tree-local-sessions.md) | Implemented; tree isolation for all session state, metadata-only listing, no cross-session usage rollups |
| [Store lock retention](store-lock-retention.md) | Implemented; bounded acquisition, commit-time CAS, lock-free OAuth reads, and per-run daemon token for `~/.cookie-agent` secure stores |
| [Tool output streams and artifact reads](tool-output-streams.md) | Implemented; correctness and performance/code-quality reviews passed |

## Drafts and records

Drafts and preserved proposals, and records of finished refactors.

| Specification | Status |
|---|---|
| [Model configuration](model-configuration.md) | Current root draft preserved; implemented in the current source tree and independently reviewed |
| [Documentation overhaul](documentation-overhaul.md) | Preserved draft; this documentation reorganization was separately authorized |
| [Refactor phase 1: test layout and build hygiene](refactor-phase-1-test-layout.md) | Implemented; split test monoliths, collapse engine test hooks, dev-profile debuginfo |
| [Refactor phase 2: dependencies, bindings, CI, syntect](refactor-phase-2-dependencies-ci.md) | Implemented; single reqwest via oven-sdk, dropped TypeScript bindings, CI diet, crates.io syntect |
| [Refactor phase 3: module splits](refactor-phase-3-module-splits.md) | Implemented; split `Inner`, `App`, transcript renderer, `SessionStore`, TUI reducer |

The two draft pages include the current repository source at every build, not
copied snapshots. Their original
status text is preserved, not silently promoted to an approved specification.
Consult [Providers](../extend/providers.md) for the implemented model schema.

## Reference pages

These implemented contracts are references, published under Reference and Run a daemon:

| Reference | Status |
|---|---|
| [Client protocol](../reference/protocol.md) | Implemented wire contract |
| [Runtime protocol](../reference/runtime-protocol.md) | Implemented wire contract between the daemon and its runtimes, version 1 |
| [Events](../reference/events.md) | Implemented persistence and subscription contract |
| [Schemas](../reference/schemas.md) | Current compatibility contract |
| [Tools](../reference/tools.md) | Implemented tool arguments, output, retention, and limits |
| [System prompt](../reference/system-prompt.md) | Implemented composition contract |
| [Security boundaries](../daemon/security.md) | Current platform and local-execution guarantees and limitations |

# Specification Status

User-facing configuration pages describe the current source tree. Specifications
also preserve proposals and design decisions; a draft is not an installed-binary
configuration reference.

## Implemented

| Specification | Status |
|---|---|
| [Push notifications](push-notifications.md) | Implemented (2026-10-25); Web Push from the daemon (VAPID and RFC 8291 on `aws-lc-rs`, keys and subscriptions in `<data_root>/push/`) when a root session settles (its run ended and nothing but recurring cron jobs can wake it) and when an approval or an ACP agent's question waits; `notifications.*` and `client.presence` (client protocol 1.30.0), `[notifications]`, the web client's Settings › This browser › Notifications and `#session=` links, presence from the web client and the TUI; approvals wait until answered by default (`[approval] timeout_ms` optional, `<= 0` never expires) |
| [Native compaction: parallel summaries and default on](native-compaction.md) | Implemented (2026-10-24); native compaction runs beside the internal summary and the checkpoint keeps the window beside the summary (client protocol 1.29.0, plugin protocol 1.17.0), the window serves only its own provider and model and the summary everything else, native compaction is on by default for every Responses-API provider (compatible gateways included, oven-sdk 342a029), and providers whose compact call is unsupported or that reject windows are remembered in `<data_root>/native-compaction.json` and listed in the configuration status; fixes the corrupt-history failure after switching models |
| [Approval reviewer: context, caching, and verdicts](approval-reviewer.md) | Implemented (2026-10-24); one cached, append-only reviewer conversation per session rendered from the event log (every labelled user message, the agent's tool calls with outcomes, AGENTS.md, earlier verdicts), bounded by budgets and epochs, a rolling cache marker with `[approval] cache_ttl`, verdicts with a reason shown in both clients (client protocol 1.28.0, plugin protocol 1.16.0); fixes `auto_approve_y` approving a failed review, the reviewer's latency eating the approval window, unescaped tags, reviewer denials reading as the user's, and the permission mode resetting on restart |
| [Default models, the starter kit, and onboarding](default-models-and-onboarding.md) | Implemented (2026-10-23); `default_model`/`small_model` and their variables, plain-string model entries, the starter kit (`config.toml` and agents) as a release asset, `cookie onboard`, the TUI and web setup panels, and the onboarding RPCs (client protocol 1.22.0, plugin protocol 1.10.0, runtime protocol 1.8.0); official skills kept up to date from `cookie-agent/skills` (section 13) |
| [ACP agents](acp-agents.md) | Implemented (2026-10-23); Claude Code, Codex, and Gemini CLI offered by runtimes as cookie agents over the Agent Client Protocol (client protocol 1.17.0, plugin protocol 1.8.0, runtime protocol 1.7.0), with cookie's tools offered to them over a runtime-relayed MCP bridge; revised 2026-10-23: no `acp` permission action (cookie's own bridged tools allowed at once, the agent's MCP tools as `mcp`, the rest to the approval flow) and delegation targets no runtime has reported yet as configuration diagnostics (client protocol 1.18.0); runtime agents, agent commands and options, the plan, and ACP tool calls in the TUI and web client; display round 2026-10-24: the agent's context use, boolean options, mode updates, command output, and images in replies (client protocol 1.23.0, plugin protocol 1.11.0); elicitation (2026-10-24): the agent's form and URL questions answered in the TUI and web client (client protocol 1.25.0, plugin protocol 1.13.0); input and pickers 2026-10-24: the agent's model and effort as the model and variant, drafts included, text and audio files, additional workspace directories, and the skill listing (client protocol 1.26.0, plugin protocol 1.14.0); the [ACP agents guide](../extend/acp-agents.md) |
| [MCP tool filters](mcp-tool-filters.md) | Implemented (2026-10-23); per-server `include_tools`/`exclude_tools` with `*`/`?` patterns over the server's own tool names, a `filtered_tools` count (client protocol 1.16.0, runtime protocol 1.6.0), the TUI and web editor fields, and MCP tools offered by the merged `mcp` rules for their names |
| [Permission consolidation](permission-consolidation.md) | Implemented (2026-10-23); defaults, the agent document, and session overrides merged into one rule list per run (session rules replace by action, runtime, and pattern), used for offered tools, every call, memory, and `/permissions`; a broad session rule no longer beats a narrower document rule, and a session `deny` can hide a default-allowed tool |
| [Cron jobs](cron.md) | Implemented (2026-10-23); scheduled prompts by cron expression or `in <delay>`, into a session (`steer` or `queue`) or a new session per run, the `cron` permission by target kind, `list_agents`, `crons.*` (client protocol 1.15.0), TUI `/cron`, the web's Scheduled prompts, and `cookie cron` |
| [Web client and local daemon use](web-client.md) | Implemented (2026-10-20); Leptos/WebAssembly client styled with Tailwind, sharing the protocol client, reducer, and panel rules with the TUI, served by `cookie daemon` or any static host, full TUI parity except MCP OAuth (closed 2026-10-21 by web MCP OAuth) and offering this machine as a runtime, phone layout and home-screen manifest (installable as an app with a network-only service worker since 2026-10-21, reversing "no service worker"), headless-Firefox suites in CI; plain `cookie` connects to a running local daemon (phase L). Addendum of 2026-10-21 implemented: web layout v2 (rail, drawer, nested subagents, the approved phone layout) and session deletion (`session.delete`, client protocol 1.5.0) from the TUI and the web client; web commands moved from `/` to the ⋮ menu, subagent tree collapsed to running agents, and a permission mode picker (2026-10-21). Addendum of 2026-10-21 on paths: canonical paths shown as `~/…` under the runtime's reported home, `cwd_identity` from the first workspace attachment, `~/` permission rules (runtime protocol 1.3.0, client protocol 1.7.0); protocol, daemon, and TUI implemented, web display next |
| [MCP OAuth from the web client](web-mcp-oauth.md) | Implemented (2026-10-21); the page itself as the OAuth redirect with `?oauth=callback`, `https` and `localhost` redirects accepted by the daemon (client protocol 1.11.0), new-tab handoff over a `BroadcastChannel`, paste fallback |
| [Delegating to runtimes](runtime-delegation.md) | Implemented (2026-10-21); a root names the runtime capabilities its subagents work through (`capabilities`, checked as `delegate@<runtime>`), `list_runtimes` in `global`, delegated attachments in the root's badges (client protocol 1.10.0), and a web New session that starts global-only; the machine registry remembers where runtimes ran (memory spec section 16.8). Addendum of 2026-10-22 implemented: offline runtimes are refused at once, open instances are handed out only over a live connection, and runtime connections are pinged and ended when silent (runtime protocol 1.4.0) |
| [Projects](projects.md) | Implemented (2026-10-21); sessions belong to projects with default attachments, an `AGENTS.md`, and project memory in the snapshot; session rename, badges, project sections, the project editor, and the TUI's project picker in the clients |
| [Memory](memory.md) | Implemented; `user`, `agent`, `machine`, and `project` memory layers as Markdown files in the `global` runtime, machines as a daemon concept, the memory tools and snapshot, `memory.*` (client protocol 1.4.0), TUI `/memory`, and `cookie memory` |
| [Background tool calls](background-tools.md) | Implemented (2026-10-16); host-owned background calls and per-turn subagent reports. Addendum of 2026-10-22 implemented: a parent waits on a child that can still wake (its own background calls, producers, or subagents), and a producer wait alone no longer keeps a session in memory |
| [Runtimes](runtimes.md) | Implemented; daemon as gateway with global and coding runtimes, attachments, the runtime protocol, remote runtimes from `[runtimes.<id>]` templates, and template MCP servers; manual labpc check (ssh template and dial-in) passed 2026-10-20; addendum of 2026-10-22 implemented: agents with no `mcp` key get the mode default `"*": ask` (section 7.1) |
| [Persistent daemon, dial-in runtimes, and runtime capabilities](runtime-capabilities.md) | Implemented; capability attachments, dial-in runtimes (the TUI embeds one when remote), listening daemon with TLS, directory-independent daemon; landed in the pre-1.0 client protocol 45 and runtime protocol 5; manual labpc check (ssh template and dial-in) passed 2026-10-20 |
| [Live engine configuration](engine-configuration.md) | Implemented; `config.get`/`config.apply`/`config.reload` (pre-1.0 client protocol 48), live reload of settings, agents, providers, headers, and MCP servers, TUI `/config` and `cookie config`, plus provider-connect fixes; addendum (2026-10-23) implemented: clients restart the daemon (`daemon.restart`, client protocol 1.19.0) |
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

These implemented contracts are references, published under Reference and Daemon:

| Reference | Status |
|---|---|
| [Client protocol](../reference/protocol.md) | Implemented wire contract |
| [Runtime protocol](../reference/runtime-protocol.md) | Implemented wire contract between the daemon and its runtimes, version 1 |
| [Events](../reference/events.md) | Implemented persistence and subscription contract |
| [Schemas](../reference/schemas.md) | Current compatibility contract |
| [Tools](../reference/tools.md) | Implemented tool arguments, output, retention, and limits |
| [System prompt](../reference/system-prompt.md) | Implemented composition contract |
| [Security boundaries](../daemon/security.md) | Current platform and local-execution guarantees and limitations |

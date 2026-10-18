# Plugins: Tools, Hooks, Events, and Producers

Status: **implemented** (decisions of 2026-10-16). Section
[Implementation notes](#9-implementation-notes) records where the code
settled details differently. This document is the governing
specification for cleaning up the plugin protocol, host, SDK, and
configuration: plugin tools speak the runtime protocol's tool messages, the
host runs on the runtime stack, and dead or half-built parts go.
This spec was written while there were no plugin implementations outside
this repository, so every part of the plugin protocol could change. The
[compatibility spec](compatibility-v1.md) now governs how the plugin protocol
changes: it is versioned by semver, and plugins written for one major keep
working with every daemon of that major. Section [Decisions](#8-decisions)
records what review settled.

## 1. Goal and scope

A plugin does four things: it declares **tools**, **intercepts** engine
hooks, **streams** session events and a bus, and **produces** messages.
Runtimes ([runtimes spec](runtimes.md)) do tools far better than plugins do
today: a prepare step with the tool's own approval identity, progress,
background, cancellation, media, collision and concurrency choices,
`tools_changed`, and prompt sections, where a plugin tool is one
`plugin/tools/call` under a shared `plugin:<label>` permission. The plugin
host also carries its own process supervisor, JSON-RPC loop, framing, and
SDK, none of which shares the runtime stack's stderr capture, restart, or
status reporting.

After this work a plugin is a **trusted, local, daemon-wide extension** that
declares tools the way a runtime does, intercepts hooks, watches events,
publishes to the bus, and produces messages to any session, over the
runtime stack's transport and process handling.

**Out of scope:** remote plugins, hooks in runtimes, event replay and
filters, plugin resources, and a runtime SDK.

## 2. Plugin tools speak the runtime tool messages

A plugin declares and runs tools with the runtime protocol's tool messages,
unchanged, over its own connection
([runtime protocol](../reference/runtime-protocol.md)):

| Message | Direction | As in the runtime protocol |
|---|---|---|
| `tools` in the `plugin/initialize` result | plugin → daemon | `tools` and `prompt_sections` of `RuntimeTools`, each tool a `ToolDeclaration` |
| `runtime/tools_changed` | plugin → daemon | the plugin's tool list changed |
| `tool/prepare`, `tool/revalidate`, `tool/release` | daemon → plugin | the plugin prepares calls and reports their approval identity, normalized arguments, and presentation |
| `tool/execute`, `tool/progress` | both | execution with streamed output and media |
| `tool/cancel` | daemon → plugin | cancellation reaches the plugin |
| `tool/detachable` | plugin → daemon | background calls and `reports_itself` |

- **Declarations** are the runtime's: an optional, free-form permission
  action (a plugin tool may need none, or name its own action such as
  `browse`), `collision`, `concurrency`, `result_truncation`, `output`,
  `background`, and `background_default`. `primary_resource_param` goes: the
  plugin computes its permission resource in `tool/prepare`.
- **The daemon reuses its runtime tool client** for a plugin's tools, so
  approval, progress, background, cancellation, media retention, and
  `tools_changed` behave exactly as for a runtime's.
- **Publication.** Plugin tools stay in the `global` runtime, but each plugin
  is its own source when names are composed, so its prefix is the plugin's
  name rather than `global`. When a plugin tool's name is taken by another
  tool (another plugin's, an MCP server's, `global`'s own, or any attached
  runtime's), its declared `collision` decides:
  - `prefix` (the default): it is published as `<plugin>_<tool>`, the other
    tool keeps its name, as runtime tools are prefixed with their attachment
    name today;
  - `override`, a new collision value: it takes the bare name, and the tool
    it overrides is published under its own source's prefix (or hidden, if
    that tool is `fallback`). Two `override` tools with one name are both
    prefixed, with a diagnostic.
  Base-layer names stay reserved: no plugin or runtime tool takes them.
  `override` is part of the shared `ToolCollision`, so a runtime tool may
  declare it too. The MCP crate no longer knows about plugins.
- **Removed:** `plugin/tools/call`, `ExtensionToolDeclaration`,
  `ExtensionToolCallParams`, `ExtensionToolCallResult`, the `tools`
  capability (declaring tools is the capability), `[plugins.<name>]
  tool_timeout_ms` (a runtime call has no per-call timeout; a tool bounds its
  own work), the `plugin` permission action and every `plugin:` special case
  (permission-name mapping and rule matching, the CLI's `--allowed-tools
  plugin` and `plugin:<name>`, the TUI's "plugin tools" row), and the MCP
  crate's plugin claims (`claim_plugin_tools`, `plugin_owns_tool`,
  `release_plugin_tools`, the collision handler, and MCP preempting a
  plugin). A stored rule naming `plugin` stays a valid free-form label.
- **Shared types.** The runtime protocol's tool wire types (`ToolDeclaration`,
  the prepare, execute, progress, and detach params and results, and the
  JSON-RPC peer and line transport) move from `runtime_api` into the
  Apache-2.0 protocol crate, so the Apache-2.0 plugin SDK can use them;
  `runtime_api` re-exports them.
- **SDK.** The plugin SDK's tool builder takes the runtime declaration and
  gives an execute closure progress, detach, cancellation, and producers;
  the echo example is updated.

Hooks, events, and producers stay daemon-wide: a session that detaches
`global` loses plugin tools, as it loses every global tool, and keeps its
hooks, events, and producers.

## 3. Protocol cleanup

The plugin protocol keeps its own methods but takes the runtime protocol's
shape where they meet.

**Version.** An integer, starting at `1` (today's string `"0.0.8"` goes),
checked as the runtime protocol checks its version.

**Removed as dead:**

| Item | Why |
|---|---|
| `resources` and `publish_session_events` capabilities | Never implemented; publishing is always rejected. |
| `ExtensionEmitResultParams.durable` | Always `rejected`; `emit_result` reports the bus outcome only. |
| The engine capability flags other than `producer_messaging` | Always true. |
| `ExtensionToolAfterResultResult.note` | Never read. |
| `ExtensionAgentBeforeStartResult.addendum` | Duplicates `append_to_system_prompt`. |
| `ExtensionMessageEndParams.role` | Always `assistant`. |
| `plugin/ping` | Used only by tests; liveness is the connection itself. |
| `provider_before_headers` hook | The model adapters do not expose request headers, so it can neither see nor change them. |
| `headers` of `provider_after_response` | Always empty; the hook keeps `status`. |

**Renamed:** `ExtensionModelSelectSource::FallbackRestore` becomes
`fallback`, which is when the engine sends it.

**Shared with runtimes.** The producer params and results
(`ExtensionProducer*`, `ExtensionRecovery*`) move to neutral names
(`ProducerRegisterParams`, …, `RecoveryCompleteParams`) in the protocol
crate, used by both protocols. The plugin methods become `producer/register`,
`producer/send`, `producer/unregister`, `producer/discard`,
`producer/recovery/start`, and `producer/recovery/complete`, the runtime
protocol's names.

**Hook payloads.** `tool_before_call` and `tool_after_result` gain the
call's `origin`: `{ "kind": "runtime", "attachment": "<name>", "runtime_id":
"<id>" }` for a runtime's or plugin's tool, `{ "kind": "base" }` for a daemon
base tool, so a hook can tell `local`'s `bash` from a remote runtime's.

## 4. Host fixes

- **Per-hook contexts.** Each hook invocation gets its own context ID.
  Today one ID is shared by the four hooks of a model attempt, so a plugin's
  `plugin/emit` from the second hook on is rejected as spent.
- **Typed hook failures.** A hook failure is classified once, by kind
  (`timeout`, `crashed`, `disconnected`, `rejected`, `malformed`), instead of
  by matching error strings in five places, so a malformed reply is no
  longer recorded as a timeout.
- **Engine diagnostics are not plugin diagnostics.** Automatic compaction's
  "disabled after 3 failures" notice gets its own runless event instead of a
  `plugin_diagnostic` under the made-up plugin name `engine:auto-compact`.
- **Dead code.** `PluginConfig::validate` and `ConfigError::Plugin`, the
  plugin-origin self-exclusion of streamed events, the `EventParamsRef` and
  `BusEventParamsRef` duplicates of the protocol structs, and
  `plugin_statuses` outside the new status RPC.
- **Old logs.** `plugin_event_added` and the `recovery_failed` and
  `recovery_disabled` kinds of `plugin_diagnostic` are never written; they
  stay decodable and rendered for old session logs.

## 5. Host on the runtime stack

The plugin host drops its own process and transport code and uses the
runtime stack's:

- **Process.** Started like a runtime process: an argv, `env`, `cwd`, its own
  process group (a job object on Windows), and **stderr captured** into a
  bounded diagnostic tail instead of inherited, which today can corrupt the
  TUI's screen.
- **Transport.** The runtime protocol's JSON-RPC peer and line transport,
  with its 48 MiB frame bound, replacing the hand-rolled reader, writer,
  pending-request table, and 4 MiB bound.
- **Restart.** A plugin that exits or drops its connection is restarted
  with the runtime reconnect policy (1 s, 2 s, 4 s, then failed until the
  user retries). A restarted plugin gets a new connection epoch; its
  producers register again and recover as on first start.
- **Status.** Client RPCs `plugins.list` (name, state, capabilities and
  hooks, last failure with the stderr tail) and `plugins.restart`, a
  `plugins.changed` notification, and a TUI plugins view (`/plugins`) next
  to the runtimes view.

## 6. Configuration

```toml
[plugins.audit]
command = ["python3", "audit.py"]
env = { AUDIT_LOG = "~/audit.log" }
cwd = "~/plugins"
enabled = true
producer_messaging = false
interception_timeout_ms = 2000
startup_timeout_ms = 10000
shutdown_grace_ms = 3000
```

- `command` becomes an argv array, as in `[runtimes.<id>]`; `args` goes.
- `tool_timeout_ms` goes with plugin tools.
- Plugins still run hooks in the order the file lists them.

## 7. Phases

Each phase lands as its own commits with every gate green, and is pushed.

1. **Clean the protocol.** Section 3 and 4: dead fields, capabilities, and
   hooks removed, shared producer types and `producer/*` method names,
   integer version 1, origins in tool hooks, and the host fixes; the SDK and
   fixtures follow.
2. **Runtime stack and status.** Section 5 and 6: process, transport,
   restart, stderr capture, `plugins.list` / `plugins.restart` /
   `plugins.changed`, the TUI view, the `command` array.
3. **Plugin tools.** Section 2: the wire types moved to the protocol crate,
   plugin tools over the runtime tool messages and the daemon's runtime tool
   client, each plugin its own composition source with `prefix` and the new
   `override` collision, the `plugin` action and the MCP crate's plugin
   claims removed, the SDK's tool builder.
4. **Documentation.** The plugin guide and development pages rewritten
   (dropping the crate-internal sections and stale versions), goal-mode and
   runtime references; this spec marked implemented.

## 8. Decisions

Decided 2026-10-16:

- Plugins keep tools, hooks, events, the bus, and producers.
- Plugin tools speak the runtime protocol's tool messages, and the daemon
  runs them with its runtime tool client.
- Plugins may produce to any session.

Confirmed in review 2026-10-16:

- Remove `provider_before_headers` rather than wire headers through the
  model adapters.
- Remove `plugin/ping`.
- Restart crashed plugins with the runtime reconnect policy.
- Rename the producer methods to `producer/*` and share their types.
- Move the runtime protocol's tool wire types and JSON-RPC peer into the
  Apache-2.0 protocol crate for the plugin SDK.
- Drop `tool_timeout_ms`; a plugin bounds its own tool work, as a runtime
  does.
- A colliding plugin tool is prefixed with its plugin's name, or, when it
  declares `collision: override`, takes the bare name and pushes the other
  tool to its prefix.

## 9. Implementation notes

Landed 2026-10-16, with phases 1 to 3 in one change because the host's tests
would otherwise have been rewritten twice.

- The status RPCs are `plugins.list` and `plugins.restart`, matching
  `runtimes.list` and `runtimes.reconnect`. They, the `engine_diagnostic`
  event, and the removed `plugin` action are client protocol 44.
- Hook failures are `timeout`, `disconnected` (not connected, or the
  connection ended during the hook, recorded as `interception_crash`),
  `rejected` (an error answer), and `malformed`; the last two record the new
  `interception_rejected` diagnostic. `unsupported_capability` is kept only
  for old logs.
- `EventParamsRef` and `BusEventParamsRef` stay: they borrow the event so it
  is serialized once for every subscribed plugin, and a test pins them to
  the protocol structs' wire form.
- A plugin's first failed start is `reconnecting` like a later drop, and is
  `failed` once the reconnect policy is exhausted.
- Plugin request IDs are integers, as the shared peer requires.
- Every plugin's tool provider shares the reserved provenance ID `plugin`.

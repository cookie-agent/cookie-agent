# Specification Status

User-facing configuration pages describe the current source tree. Specifications
also preserve proposals and design decisions; a draft is not an installed-binary
configuration reference.

| Specification | Status |
|---|---|
| [Agent messaging](agent-messaging.md) | Implemented; governing contract for `send_message` agent-to-agent messaging |
| [Subagent handles](subagent-handles.md) | Draft proposal; short tree-unique handles for model-facing session references |
| [Tree-scoped session ownership lock](tree-ownership-lock.md) | Implemented; one `owner.lock` per root session tree, no lock files for delegated children |
| [Store lock retention](store-lock-retention.md) | Implemented; bounded acquisition, commit-time CAS, lock-free OAuth reads, and per-run daemon token for `~/.cookie-agent` secure stores |
| [Tool output streams and artifact reads](tool-output-streams.md) | Implemented; correctness and performance/code-quality reviews passed |
| [Model configuration](model-configuration.md) | Current root draft preserved; implemented in the current source tree and independently reviewed |
| [Documentation overhaul](documentation-overhaul.md) | Preserved draft; this documentation reorganization was separately authorized |
| [Refactor phase 1: test layout and build hygiene](refactor-phase-1-test-layout.md) | Implemented; split test monoliths, collapse engine test hooks, dev-profile debuginfo |
| [Refactor phase 2: dependencies, bindings, CI, syntect](refactor-phase-2-dependencies-ci.md) | Implemented; single reqwest via oven-sdk, dropped TypeScript bindings, CI diet, crates.io syntect |
| [Refactor phase 3: module splits](refactor-phase-3-module-splits.md) | Implemented; split `Inner`, `App`, transcript renderer, `SessionStore`, TUI reducer |
| [Goal mode and producers](../development/goal-mode.md) | Implemented, approved product contract |
| [Protocol](../reference/protocol.md) | Implemented wire contract |
| [Events](../reference/events.md) | Implemented persistence and subscription contract |
| [Schemas](../reference/schemas.md) | Current compatibility contract |
| [Tools](../reference/tools.md) | Implemented tool arguments, output, retention, and limits |
| [System prompt](../reference/system-prompt.md) | Implemented composition contract |
| [Security boundaries](../guide/security.md) | Current platform and local-execution guarantees and limitations |

The two draft pages include the current repository source at every build, not
copied snapshots. Their original
status text is preserved, not silently promoted to an approved specification.
Consult [Providers](../guide/providers.md) for the implemented model schema.

# Specification Status

User-facing configuration pages describe the current source tree. Specifications
also preserve proposals and design decisions; a draft is not an installed-binary
configuration reference.

| Specification | Status |
|---|---|
| [Model configuration](model-configuration.md) | Current root draft preserved; implemented in the current source tree and independently reviewed |
| [Documentation overhaul](documentation-overhaul.md) | Preserved draft; this documentation reorganization was separately authorized |
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

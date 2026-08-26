# Bundled models.dev bootstrap catalog

The artifact in this directory is fallback bootstrap input for catalog cache
schema 2. It is not a configured revision pin and is selected only after the
fixed network request and validated per-user cache are unusable.

Bundled artifact facts are 3,567,054 bytes and
`sha256:d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a`.
The independently reviewed test-only live fixture captured on 2026-08-05 is
`crates/models/tests/fixtures/models-dev-live-audit-2026-08-05.json`: 3,801,566
identity bytes, ETag `"25dd5dd6eb21b2d78044606eeb806d8c"`, 180 providers,
6,131 provider models, 293 canonical models, and
`sha256:25dd5dd6eb21b2d78044606eeb806d8cdd38640c8deea071122d5591edb88795`.
The fixture and digest are audit evidence only, not a runtime pin or runtime
acceptance criterion.

Normative source order is:

1. `https://models.dev/catalog.json` response or ETag `304` cache validation;
2. independently validated cache schema 2;
3. independently validated bundled bootstrap.

The network client sends `Accept-Encoding: identity`, rejects compressed
responses, rejects `Content-Length` above 16 MiB before reading, and enforces a
streamed 16 MiB hard cap before buffering/decoding. Parsed JSON is bounded to
depth 32, 4096 providers, 65,536 provider models per provider, 65,536 root
canonical models, 1,000,000 aggregate container entries, and 256 KiB strings
before narrower field limits.

On Unix, cache files are fixed at:

```text
~/.cookie-agent/catalog/models-dev-v2.json
~/.cookie-agent/catalog/models-dev-v2.meta.json
~/.cookie-agent/catalog/models-dev-v2.lock
```

New directories are created mode `0700`; new body, metadata, lock, and temporary
files are created mode `0600`. Writes use lock/reread, exclusive sibling temp,
fsync, atomic rename, and parent fsync. Existing cache paths are used as-is
without ownership, mode, type, link, or symlink checks.

Metadata schema 2 records
`sha256:<lowercase SHA-256 digest of the exact selected body bytes>`, ETag, size,
validation/check times, source, stale flag, structural-record diagnostics, and safe
last-error code/message/time. Cache or bootstrap fallback explicitly persists
stale/error metadata when safe atomic writing is available.

Invalid candidate structure rejects that source. Once a bounded root provider
map is recovered, malformed/ambiguous provider records are quarantined with all
children and malformed/ambiguous model records are quarantined individually;
valid siblings survive. Executable behavior is classified directly: provider and
nested-model npm values select a protocol family, while catalog API, shape,
capability, modality, and limit values are authoritative.

The strict catalog root has exactly required nonempty `providers` and `models`
maps. `providers` carries provider-scoped executable metadata. Root `models`
carries canonical metadata/provenance only and never defines transport, setup,
auth, adaptor, or executable inclusion. Exact same-key links are optional
provenance references; provider records remain executable authority. Invalid
canonical records quarantine only their provenance entries.

Catalog values define managed endpoints and model capabilities. Family registry
schema 1 owns constructors, auth methods, and settings derivation. Cargo builds
do not fetch the runtime catalog; daemon startup does.

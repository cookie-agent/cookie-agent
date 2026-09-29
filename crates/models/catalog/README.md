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

The network client sends `Accept-Encoding: gzip` and accepts gzip or identity
responses; any other content coding is rejected. It rejects `Content-Length`
above 64 MiB before reading, enforces a streamed 64 MiB cap on the body as
sent, and decodes gzip under the same 64 MiB cap on the decoded bytes, so a
small compressed body cannot expand past it. Parsed JSON is bounded to
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

Installing a new body takes the cache lock, atomically replaces the body, then
atomically replaces the metadata naming the body's revision and length. There
is no journal: a crash between the two replaces leaves metadata naming the
previous body, loading rejects that mismatched pair, startup serves the
bootstrap, and the next refresh (sent without an ETag) installs a consistent
pair. A `304` or a failed refresh rewrites only the metadata, after checking it
still names the installed body.

A fetched catalog is refused when it has fewer than half the usable provider
models (rows that parsed rather than quarantined) of a cache validated within
the last seven days. models.dev only grows in normal operation, so such a drop
means a broken upstream deploy or a schema change that quarantines most rows.
The cache stays in use, marked stale with the `catalog_model_loss_rejected`
error. A refused refresh does not revalidate the cache, so if upstream keeps
serving the smaller catalog for a week, it is accepted.

Invalid candidate structure rejects that source. Once a bounded root provider
map is recovered, malformed/ambiguous provider records are quarantined with all
children and malformed/ambiguous model records are quarantined individually;
valid siblings survive. Executable behavior is classified directly: provider and
nested-model npm values select a protocol family, while catalog API, shape,
capability, modality, and limit values are authoritative.

The catalog is third-party data and is parsed for forward compatibility, unlike
authored configuration. The root requires nonempty `providers` and `models`
maps; each record requires its core fields, and the fields cookie reads keep
their type checks. Unknown fields at every level (root, provider, model, and
nested objects) are ignored, as is metadata cookie never reads (for example
`canonical_model_id`, `knowledge`, `experimental`, benchmarks, and links),
whatever its shape. Display text is trimmed of surrounding whitespace, modality
arrays may be empty, and prices (JSON floats upstream) round to the nearest
pico-USD per million tokens, ties up. An unknown model `status` label is treated
as active, and unknown `reasoning_options` types and effort labels are skipped
while the model keeps its known reasoning controls. `providers` carries provider-scoped executable metadata. Root `models`
carries canonical metadata/provenance only and never defines transport, setup,
auth, adaptor, or executable inclusion. Exact same-key links are optional
provenance references; provider records remain executable authority. Invalid
canonical records quarantine only their provenance entries.

Catalog values define managed endpoints and model capabilities. Family registry
schema 1 owns constructors, auth methods, and settings derivation. Cargo builds
do not fetch the runtime catalog; daemon startup does.

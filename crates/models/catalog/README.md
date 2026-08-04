# Vendored models.dev catalog

`models-dev.json` is the exact deterministic `snapshotPayload` generated from
[`anomalyco/models.dev`](https://github.com/anomalyco/models.dev) commit
`c3057690bbb8bd41cafdefadcd2a7b958e2a4642`.

- Upstream generator: `packages/sdk/script/generate.ts`
- Authoritative inputs: the upstream `models/` and `providers/` TOMLs plus
  `packages/core/src/schema.ts` and `packages/core/src/generate.ts`
- Explicit opt-in update command (may clone and run `bun install`):
  `python3 scripts/update_models_dev.py --update`
- Strictly offline check against an existing pinned checkout with dependencies
  already installed:
  `python3 scripts/update_models_dev.py --check --source /path/to/models.dev`
- Size: `3,567,054` bytes
- SHA-256: `d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a`
- Encoding: canonical compact JSON with sorted provider, provider-model, and
  model-metadata keys and no trailing newline

The repository-root upstream `models.json` is not this artifact and must not be
used as a substitute. Offline check mode never clones, installs, or accesses
the network; it fails with instructions when source dependencies are absent.
Only explicit `--update` permits clone/install activity. The updater is
developer-only. Cargo builds, tests, and runtime catalog loading perform no
network access.

The vendored artifact is distributed under the upstream MIT license in
`LICENSE.models.dev`.

The runtime compiler accepts only the pinned `reasoning_options` forms
`effort`, `toggle`, and `budget_tokens`. Effort values, JSON `null`, budget
bounds, duplicate values, unknown fields, deterministic union ordering, and
collision precedence are validated before any configured provider snapshot is
published. The updater preserves the exact upstream bytes; it does not invent
or rewrite variant metadata.

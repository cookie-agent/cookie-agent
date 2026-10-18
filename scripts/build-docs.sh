#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo doc --locked --workspace --no-deps

rm -rf docs/site/api site
mkdir -p docs/site/api
cp -R target/doc/. docs/site/api/
cp scripts/rustdoc-index.html docs/site/api/index.html

# The plugin and runtime protocols' schemas, published as committed.
rm -rf docs/site/schemas
mkdir -p docs/site/schemas
for surface in plugin runtime; do
  cp -R "crates/protocol/compat/${surface}" docs/site/schemas/
done

mkdocs build --strict

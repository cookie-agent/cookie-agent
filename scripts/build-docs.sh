#!/usr/bin/env bash
# Builds the documentation site into site/: workspace rustdoc under api/,
# the protocols' schemas under schemas/, and MkDocs in strict mode.
#
# Usage: scripts/build-docs.sh [--no-rustdoc]
#
# --no-rustdoc skips rustdoc, for changes to the prose only: api/ then holds
# just its index page, whose links lead nowhere. CI and the Documentation
# workflow build everything.
set -euo pipefail

rustdoc=true
case "${1:-}" in
  "") ;;
  --no-rustdoc) rustdoc=false ;;
  *)
    echo "usage: $0 [--no-rustdoc]" >&2
    exit 2
    ;;
esac

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if $rustdoc; then
  cargo doc --locked --workspace --no-deps
fi

rm -rf docs/site/api site
mkdir -p docs/site/api
if $rustdoc; then
  cp -R "${CARGO_TARGET_DIR:-target}/doc/." docs/site/api/
fi
cp scripts/rustdoc-index.html docs/site/api/index.html

# The client, plugin, and runtime protocols' schemas, published as committed.
rm -rf docs/site/schemas
mkdir -p docs/site/schemas
for surface in client plugin runtime; do
  cp -R "crates/protocol/compat/${surface}" docs/site/schemas/
done

mkdocs build --strict

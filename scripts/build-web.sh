#!/usr/bin/env bash
# Builds the web client (crates/web) with Trunk and puts the bundle where
# crates/web_assets embeds it, so the next `cookie` build serves the real
# page. Prints the bundle's size.
#
# Needs the wasm32-unknown-unknown target and trunk of the version below
# (`cargo install trunk --version <it> --locked`). Trunk downloads the
# wasm-bindgen CLI, wasm-opt, and the Tailwind CLI of the versions
# crates/web/Trunk.toml pins on first use, into its cache directory.
set -euo pipefail

TRUNK_VERSION="0.21.14"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if ! installed="$(trunk --version 2>/dev/null)"; then
    echo "trunk is missing: cargo install trunk --version $TRUNK_VERSION --locked" >&2
    exit 1
fi
if [[ "$installed" != "trunk $TRUNK_VERSION" ]]; then
    echo "$installed is installed; this build pins trunk $TRUNK_VERSION" >&2
    exit 1
fi
locked="$(sed -n '/^name = "wasm-bindgen"$/{n;s/^version = "\(.*\)"$/\1/p;q}' Cargo.lock)"
pinned="$(sed -n 's/^wasm_bindgen = "\(.*\)"$/\1/p' crates/web/Trunk.toml)"
if [[ "$locked" != "$pinned" ]]; then
    echo "crates/web/Trunk.toml pins wasm-bindgen $pinned; Cargo.lock has $locked" >&2
    exit 1
fi

(cd crates/web && trunk build --release --locked)

bundle="crates/web_assets/bundle"
rm -rf "$bundle"
mkdir -p "$bundle"
cp -R crates/web/dist/. "$bundle/"
rm -rf "$bundle/.stage"

echo "web bundle in $bundle:"
total=0
compressed=0
while IFS= read -r -d '' file; do
    bytes="$(wc -c <"$file")"
    gzipped="$(gzip -9 -c "$file" | wc -c)"
    total=$((total + bytes))
    compressed=$((compressed + gzipped))
    printf '  %10d bytes  %10d gzipped  %s\n' "$bytes" "$gzipped" "${file#"$bundle"/}"
done < <(find "$bundle" -type f -print0 | sort -z)
printf '  %10d bytes  %10d gzipped  total\n' "$total" "$compressed"

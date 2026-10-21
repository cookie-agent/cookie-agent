#!/usr/bin/env bash
# Builds the web client (crates/web) with Trunk and puts the bundle where
# crates/web_assets embeds it, so the next `cookie` build serves the real
# page. Prints the bundle's size and fails when it is over its budget.
#
# Usage: scripts/build-web.sh [--fast]
#
# The bundle is built with the `web` Cargo profile and wasm-opt, as CI and
# releases ship it. --fast builds it with `web-dev` and without wasm-opt, in
# a fraction of the time, for local browser runs; it is larger and slower,
# so never ship it.
#
# Needs the wasm32-unknown-unknown target and trunk of the version below
# (`cargo install trunk --version <it> --locked`). Trunk downloads the
# wasm-bindgen CLI, wasm-opt, and the Tailwind CLI of the versions
# crates/web/Trunk.toml pins on first use, into its cache directory.
set -euo pipefail

# Trunk runs wasm-opt for release builds only.
trunk_args=(--release --cargo-profile web)
case "${1:-}" in
    "") ;;
    --fast) trunk_args=(--cargo-profile web-dev) ;;
    *)
        echo "usage: $0 [--fast]" >&2
        exit 2
        ;;
esac

TRUNK_VERSION="0.21.14"
# What a browser may download for the page, gzipped, in bytes: a backstop
# against runaway growth, not a target. The release workflow builds the page
# with Rust 1.88.0, whose wasm is larger than stable's (1.75 MB against
# 1.69 MB after the 2026-10 UI polish), and the page is fetched once from
# the daemon, so the budget leaves ample room (raised from 1.7 MB,
# 2026-10-21).
BUDGET_GZIPPED_BYTES=10000000

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

# From a clean dist/: trunk can leave an index.html naming an earlier
# build's stylesheet, which the page then cannot load.
rm -rf crates/web/dist
(cd crates/web && trunk build --locked "${trunk_args[@]}")

bundle="crates/web_assets/bundle"
rm -rf "$bundle"
mkdir -p "$bundle"
cp -R crates/web/dist/. "$bundle/"
rm -rf "$bundle/.stage"
# Stamp the service worker with the build: index.html names the build's
# hashed files, so a new build changes sw.js too and browsers replace the
# installed one. The same build gets the same stamp.
build="$({ sha256sum 2>/dev/null || shasum -a 256; } <"$bundle/index.html" | cut -c1-16)"
sed "s|^// build: source\$|// build: $build|" "$bundle/sw.js" >"$bundle/sw.js.stamped"
mv "$bundle/sw.js.stamped" "$bundle/sw.js"
grep -q "^// build: $build\$" "$bundle/sw.js" || {
    echo "could not stamp $bundle/sw.js with the build" >&2
    exit 1
}

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
if ((compressed > BUDGET_GZIPPED_BYTES)); then
    echo "the bundle is $compressed bytes gzipped, over its budget of $BUDGET_GZIPPED_BYTES" >&2
    exit 1
fi
echo "  within the budget of $BUDGET_GZIPPED_BYTES bytes gzipped"

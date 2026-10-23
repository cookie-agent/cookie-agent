#!/usr/bin/env bash
# Packs the starter kit, defaults/, as the release asset
# cookie-agent-defaults.tar.gz: the files, and manifest.json with the kit
# version, each file's SHA-256, and the cookie version it was built for.
#
#   scripts/pack-defaults.sh <cookie version or tag> <output.tar.gz>
#   scripts/pack-defaults.sh --update-manifest
#
# --update-manifest rewrites defaults/manifest.json's hashes after a kit file
# changed (every file below defaults/ but the manifest is listed); packing
# refuses a manifest whose hashes do not match.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "${1:-}" == "--update-manifest" ]]; then
    python3 -I - "$root/defaults" <<'PY'
import hashlib, json, os, sys
root = sys.argv[1]
path = os.path.join(root, "manifest.json")
manifest = json.load(open(path))
files = {}
for directory, _, names in os.walk(root):
    for name in names:
        full = os.path.join(directory, name)
        relative = os.path.relpath(full, root).replace(os.sep, "/")
        if relative != "manifest.json":
            files[relative] = hashlib.sha256(open(full, "rb").read()).hexdigest()
manifest["files"] = dict(sorted(files.items()))
with open(path, "w") as out:
    json.dump(manifest, out, indent=2)
    out.write("\n")
PY
    exit 0
fi

version="${1:?usage: pack-defaults.sh <cookie version> <output.tar.gz> | --update-manifest}"
output="${2:?usage: pack-defaults.sh <cookie version> <output.tar.gz> | --update-manifest}"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
cp -R "$root/defaults/." "$staging/"
python3 -I - "$staging" "$version" <<'PY'
import hashlib, json, os, sys
root, version = sys.argv[1], sys.argv[2]
path = os.path.join(root, "manifest.json")
manifest = json.load(open(path))
for name, digest in manifest["files"].items():
    actual = hashlib.sha256(open(os.path.join(root, name), "rb").read()).hexdigest()
    if actual != digest:
        sys.exit(f"defaults/{name}: SHA-256 {actual}, the manifest says {digest}; run scripts/pack-defaults.sh --update-manifest")
manifest["cookie_version"] = version[1:] if version.startswith("v") else version
with open(path, "w") as out:
    json.dump(manifest, out, indent=2)
    out.write("\n")
PY
tar -C "$staging" --owner=0 --group=0 --numeric-owner -czf "$output" .

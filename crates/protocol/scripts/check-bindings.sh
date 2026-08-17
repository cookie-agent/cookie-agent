#!/usr/bin/env bash
set -euo pipefail

if (($# > 1)) || (($# == 1)) && [[ "$1" != "--check" ]]; then
  echo "usage: $0 [--check]" >&2
  exit 2
fi

protocol_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd "${protocol_root}/../.." && pwd)"
temporary_root="$(mktemp -d)"
trap 'rm -rf "${temporary_root}"' EXIT

mkdir "${temporary_root}/typescript"
cp "${protocol_root}/typescript/package.json" "${protocol_root}/typescript/package-lock.json" \
  "${temporary_root}/typescript/"
npm ci --prefix "${temporary_root}/typescript" --ignore-scripts --no-audit --no-fund
cargo run --locked --manifest-path "${workspace_root}/Cargo.toml" -p cookie_agent_protocol --example generate -- --output "${temporary_root}/generated"

python3 - "${protocol_root}/generated" "${temporary_root}/generated" <<'PY'
import pathlib
import sys

checked = pathlib.Path(sys.argv[1])
fresh = pathlib.Path(sys.argv[2])

def files(root: pathlib.Path) -> dict[pathlib.Path, bytes]:
    return {
        path.relative_to(root): path.read_bytes()
        for path in root.rglob("*")
        if path.is_file()
    }

checked_files = files(checked)
fresh_files = files(fresh)
if checked_files != fresh_files:
    missing = sorted(set(checked_files) - set(fresh_files))
    extra = sorted(set(fresh_files) - set(checked_files))
    changed = sorted(
        path
        for path in set(checked_files) & set(fresh_files)
        if checked_files[path] != fresh_files[path]
    )
    raise SystemExit(
        f"generated bindings drifted; missing={missing}, extra={extra}, changed={changed}"
    )
PY

python3 "${protocol_root}/scripts/check-event-payload-additive.py" \
  "${protocol_root}/event-payload-baseline.json" \
  "${temporary_root}/generated/json-schema/EventPayload.schema.json"
bash "${protocol_root}/scripts/test-event-payload-additive.sh" \
  "${temporary_root}/generated/json-schema/EventPayload.schema.json"
python3 "${protocol_root}/scripts/check-extension-protocol-additive.py" \
  "${protocol_root}/extension-protocol-baseline.json" \
  "${temporary_root}/generated/json-schema" \
  "${protocol_root}/src/extension.rs"
bash "${protocol_root}/scripts/test-extension-protocol-additive.sh" \
  "${temporary_root}/generated/json-schema"

"${temporary_root}/typescript/node_modules/.bin/tsc" \
  --project "${temporary_root}/generated/typescript/tsconfig.json" \
  --noEmit

#!/usr/bin/env bash
# Regenerates the compatibility baselines in crates/protocol/compat/ from the
# current code. From major version 1 it refuses when a surface's version
# constant lacks the bump its changes require.
set -euo pipefail

if (($# > 0)); then
  echo "usage: $0" >&2
  exit 2
fi

protocol_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd "${protocol_root}/../.." && pwd)"
temporary_root="$(mktemp -d)"
trap 'rm -rf "${temporary_root}"' EXIT

cargo run --locked --manifest-path "${workspace_root}/Cargo.toml" -p cookie_agent_protocol \
  --example generate -- --output "${temporary_root}/generated"

mkdir -p "${protocol_root}/compat"
python3 "${protocol_root}/scripts/check-compat.py" --update \
  --baseline "${protocol_root}/compat" \
  --generated "${temporary_root}/generated"

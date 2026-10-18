#!/usr/bin/env bash
# Checks the protocol surfaces against their compatibility baselines in
# crates/protocol/compat/ (docs/site/specs/compatibility-v1.md section 7),
# then runs the checker's mutation test.
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

python3 "${protocol_root}/scripts/check-compat.py" \
  --baseline "${protocol_root}/compat" \
  --generated "${temporary_root}/generated"
python3 "${protocol_root}/scripts/test-check-compat.py" "${temporary_root}/generated"

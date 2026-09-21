#!/usr/bin/env bash
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

#!/usr/bin/env bash
set -euo pipefail

protocol_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
checker="${protocol_root}/scripts/check-event-payload-additive.py"
baseline="${protocol_root}/event-payload-baseline.json"
schema="${1:-${protocol_root}/generated/json-schema/EventPayload.schema.json}"
temporary_root="$(mktemp -d)"
trap 'rm -rf "${temporary_root}"' EXIT

for mutation in removed-tag removed-field shrunk-required newly-required; do
  candidate="${temporary_root}/${mutation}.json"
  python3 - "${schema}" "${candidate}" "${mutation}" <<'PY'
import json
import pathlib
import sys

schema = json.loads(pathlib.Path(sys.argv[1]).read_text())
variant = schema["oneOf"][0]
mutation = sys.argv[3]
if mutation == "removed-tag":
    schema["oneOf"].pop(0)
elif mutation == "removed-field":
    field = next(field for field in variant["required"] if field != "type")
    variant["properties"].pop(field)
elif mutation == "shrunk-required":
    field = next(field for field in variant["required"] if field != "type")
    variant["required"].remove(field)
elif mutation == "newly-required":
    variant["properties"]["future_required"] = {"type": "string"}
    variant["required"].append("future_required")
pathlib.Path(sys.argv[2]).write_text(json.dumps(schema))
PY
  if python3 "${checker}" "${baseline}" "${candidate}" >/dev/null 2>&1; then
    echo "additive checker unexpectedly accepted ${mutation}" >&2
    exit 1
  fi
done

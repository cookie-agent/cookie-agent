#!/usr/bin/env bash
set -euo pipefail

protocol_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
checker="${protocol_root}/scripts/check-extension-protocol-additive.py"
baseline="${protocol_root}/extension-protocol-baseline.json"
schema_root="${1:-${protocol_root}/generated/json-schema}"
source="${protocol_root}/src/extension.rs"
temporary_root="$(mktemp -d)"
trap 'rm -rf "${temporary_root}"' EXIT

for mutation in actual-initialize-params removed-field shrunk-required newly-required nested-field removed-definition method-name; do
  candidate_root="${temporary_root}/${mutation}"
  candidate_source="${temporary_root}/${mutation}.rs"
  mkdir "${candidate_root}"
  cp "${schema_root}"/ExtensionInitializeParams.schema.json \
    "${schema_root}"/ExtensionInitializeResult.schema.json \
    "${schema_root}"/ExtensionPingParams.schema.json \
    "${schema_root}"/ExtensionPingResult.schema.json \
    "${schema_root}"/ExtensionShutdownParams.schema.json \
    "${candidate_root}/"
  cp "${source}" "${candidate_source}"
  python3 - "${candidate_root}" "${mutation}" "${candidate_source}" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
mutation = sys.argv[2]
source = pathlib.Path(sys.argv[3])
params_path = root / "ExtensionInitializeParams.schema.json"
result_path = root / "ExtensionInitializeResult.schema.json"
params = json.loads(params_path.read_text())
result = json.loads(result_path.read_text())
if mutation == "actual-initialize-params":
    params["properties"]["engine_version"] = {"type": "integer"}
elif mutation == "removed-field":
    params["properties"].pop("engine_version")
elif mutation == "shrunk-required":
    params["required"].remove("engine_version")
elif mutation == "newly-required":
    result["properties"]["future_required"] = {"type": "string"}
    result["required"].append("future_required")
elif mutation == "nested-field":
    result["$defs"]["ExtensionToolDeclaration"]["properties"]["permission_name"] = {"type": "integer"}
elif mutation == "removed-definition":
    result["$defs"].pop("ExtensionPluginCapabilities")
elif mutation == "method-name":
    source.write_text(source.read_text().replace('"plugin/ping"', '"plugin/pong"'))
params_path.write_text(json.dumps(params))
result_path.write_text(json.dumps(result))
PY
  if python3 "${checker}" "${baseline}" "${candidate_root}" "${candidate_source}" >/dev/null 2>&1; then
    echo "extension additive checker unexpectedly accepted ${mutation}" >&2
    exit 1
  fi
done

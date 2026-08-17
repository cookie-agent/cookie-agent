#!/usr/bin/env bash
set -euo pipefail

protocol_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
checker="${protocol_root}/scripts/check-extension-protocol-additive.py"
baseline="${protocol_root}/extension-protocol-baseline.json"
schema_root="${1:-${protocol_root}/generated/json-schema}"
source="${protocol_root}/src/extension.rs"
temporary_root="$(mktemp -d)"
trap 'rm -rf "${temporary_root}"' EXIT

for mutation in actual-initialize-params removed-field shrunk-required newly-required nested-field removed-definition method-name tool-call-field tool-call-method event-field emit-context emit-status intercept-action intercept-method compact-additions; do
  candidate_root="${temporary_root}/${mutation}"
  candidate_source="${temporary_root}/${mutation}.rs"
  mkdir "${candidate_root}"
  python3 - "${baseline}" "${schema_root}" "${candidate_root}" <<'PY'
import json
import pathlib
import shutil
import sys

baseline = json.loads(pathlib.Path(sys.argv[1]).read_text())
schema_root = pathlib.Path(sys.argv[2])
candidate_root = pathlib.Path(sys.argv[3])
for filename in baseline["schemas"]:
    shutil.copy2(schema_root / filename, candidate_root / filename)
PY
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
call_path = root / "ExtensionToolCallParams.schema.json"
event_path = root / "ExtensionEventParams.schema.json"
emit_result_path = root / "ExtensionEmitResultParams.schema.json"
emit_path = root / "ExtensionEmitParams.schema.json"
before_result_path = root / "ExtensionToolBeforeCallResult.schema.json"
compact_path = root / "ExtensionSessionBeforeCompactParams.schema.json"
params = json.loads(params_path.read_text())
result = json.loads(result_path.read_text())
call = json.loads(call_path.read_text())
event = json.loads(event_path.read_text())
emit_result = json.loads(emit_result_path.read_text())
emit = json.loads(emit_path.read_text())
before_result = json.loads(before_result_path.read_text())
compact = json.loads(compact_path.read_text())
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
elif mutation == "tool-call-field":
    call["properties"]["arguments"] = {"type": "string"}
elif mutation == "tool-call-method":
    source.write_text(source.read_text().replace('"plugin/tools/call"', '"plugin/tools/run"'))
elif mutation == "event-field":
    event["properties"].pop("seq")
elif mutation == "emit-context":
    emit["properties"].pop("context_id")
elif mutation == "emit-status":
    emit_result["$defs"]["ExtensionEmitStatus"]["enum"].remove("rejected")
elif mutation == "intercept-action":
    before_result["$defs"]["ExtensionToolBeforeCallAction"]["enum"].remove("block")
elif mutation == "intercept-method":
    source.write_text(source.read_text().replace('"plugin/intercept/tool_after_result"', '"plugin/intercept/tool_result"'))
elif mutation == "compact-additions":
    compact["properties"].pop("additions")
params_path.write_text(json.dumps(params))
result_path.write_text(json.dumps(result))
call_path.write_text(json.dumps(call))
event_path.write_text(json.dumps(event))
emit_result_path.write_text(json.dumps(emit_result))
emit_path.write_text(json.dumps(emit))
before_result_path.write_text(json.dumps(before_result))
compact_path.write_text(json.dumps(compact))
PY
  if python3 "${checker}" "${baseline}" "${candidate_root}" "${candidate_source}" >/dev/null 2>&1; then
    echo "extension additive checker unexpectedly accepted ${mutation}" >&2
    exit 1
  fi
done

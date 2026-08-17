#!/usr/bin/env python3
import json
import pathlib
import sys

if len(sys.argv) != 3:
    raise SystemExit(f"usage: {sys.argv[0]} BASELINE EVENT_PAYLOAD_SCHEMA")

baseline = json.loads(pathlib.Path(sys.argv[1]).read_text())
schema = json.loads(pathlib.Path(sys.argv[2]).read_text())
variants = {}
for variant in schema.get("oneOf", []):
    properties = variant.get("properties", {})
    tag = properties.get("type", {}).get("const")
    if tag:
        variants[tag] = (set(properties), set(variant.get("required", [])))

errors = []
for tag, historical_fields_list in baseline.items():
    if tag not in variants:
        errors.append(f"removed or renamed event tag {tag!r}")
        continue
    historical_fields = set(historical_fields_list)
    fields, required = variants[tag]
    missing_fields = sorted(historical_fields - fields)
    missing_required = sorted(historical_fields - required)
    newly_required = sorted(required - historical_fields)
    if missing_fields:
        errors.append(f"{tag}: removed or renamed fields {missing_fields}")
    if missing_required:
        errors.append(f"{tag}: required array shrank by {missing_required}")
    if newly_required:
        errors.append(f"{tag}: new fields became required {newly_required}")
if errors:
    raise SystemExit("event payload is not additive-only:\n" + "\n".join(errors))

#!/usr/bin/env python3
import json
import pathlib
import re
import sys

if len(sys.argv) != 4:
    raise SystemExit(
        f"usage: {sys.argv[0]} BASELINE GENERATED_SCHEMA_DIRECTORY EXTENSION_SOURCE"
    )

baseline = json.loads(pathlib.Path(sys.argv[1]).read_text())
schema_root = pathlib.Path(sys.argv[2])
source = pathlib.Path(sys.argv[3]).read_text()
methods = dict(
    re.findall(
        r'pub const (PLUGIN_[A-Z_]+_METHOD): &str = "([^"]+)";',
        source,
    )
)
errors = []

for name, historical_value in baseline["methods"].items():
    current = methods.get(name)
    if current != historical_value:
        errors.append(
            f"method {name} changed from {historical_value!r} to {current!r}"
        )


def compare_node(path, historical, current):
    if isinstance(historical, dict):
        if not isinstance(current, dict):
            errors.append(f"{path}: schema kind changed")
            return
        special = {"description", "properties", "required", "$defs"}
        historical_keys = set(historical) - special
        current_keys = set(current) - special
        if historical_keys != current_keys:
            errors.append(
                f"{path}: schema keywords changed; removed={sorted(historical_keys-current_keys)}, added={sorted(current_keys-historical_keys)}"
            )
        for key in sorted(historical_keys & current_keys):
            compare_node(f"{path}.{key}", historical[key], current[key])

        historical_properties = historical.get("properties")
        if historical_properties is not None:
            current_properties = current.get("properties")
            if not isinstance(current_properties, dict):
                errors.append(f"{path}: properties were removed or changed kind")
            else:
                for name, value in historical_properties.items():
                    if name not in current_properties:
                        errors.append(f"{path}: property {name!r} was removed or renamed")
                    else:
                        compare_node(
                            f"{path}.properties.{name}", value, current_properties[name]
                        )

        historical_required = set(historical.get("required", []))
        current_required = set(current.get("required", []))
        if historical_required - current_required:
            errors.append(
                f"{path}: required array shrank by {sorted(historical_required-current_required)}"
            )
        if current_required - historical_required:
            errors.append(
                f"{path}: new fields became required {sorted(current_required-historical_required)}"
            )

        current_definitions = current.get("$defs", {})
        for name, definition in historical.get("$defs", {}).items():
            if name not in current_definitions:
                errors.append(f"{path}: referenced definition {name!r} was removed")
            else:
                compare_node(
                    f"{path}.$defs.{name}", definition, current_definitions[name]
                )
    elif isinstance(historical, list):
        if historical != current:
            errors.append(f"{path}: schema array changed")
    elif historical != current:
        errors.append(f"{path}: changed from {historical!r} to {current!r}")


for filename, historical_schema in baseline["schemas"].items():
    path = schema_root / filename
    if not path.is_file():
        errors.append(f"wire schema {filename!r} was removed or renamed")
        continue
    current_schema = json.loads(path.read_text())
    compare_node(filename, historical_schema, current_schema)

if errors:
    raise SystemExit("extension protocol is not additive-only:\n" + "\n".join(errors))

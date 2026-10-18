#!/usr/bin/env python3
"""Compatibility checker of the protocol surfaces.

Compares a generated compatibility set (`cargo run -p cookie_agent_protocol
--example generate -- --output DIR`) with the committed baselines in
`crates/protocol/compat/`, as specified in
`docs/site/specs/compatibility-v1.md` section 7.1.

Each surface directory holds `version`, `methods.json`, and
`schemas/<Root>.schema.json`. A type's role comes from `methods.json`:
each method's `params_role` (by default, params sent to the daemon are
commands and params the daemon sends are information; a peer's declaration,
such as its tool list, is information though it goes to the daemon), and
results and the listed information roots are information. A type reached in
both roles is checked under both, so the stricter rule wins.

Pre-freeze mode: while a surface's version major is 0 (in the baseline and
the generated set), the baseline must
equal the generated set exactly (regenerate it with
`update-compat-baseline.sh`), and rule violations are printed but do not
fail. From major 1 every rule is enforced: the version must carry at least
the bump the changes require, and `additionalProperties: false` on an
information type fails outright.

Usage:
  check-compat.py --baseline DIR --generated DIR [--update] [--verbose]

`--update` replaces the baseline with the generated set after the checks,
refusing an insufficient bump from major 1.
"""

import argparse
import copy
import json
import pathlib
import re
import shutil
import sys

SURFACES = ("client", "plugin", "runtime", "events")

NONE, PATCH, MINOR, MAJOR = 0, 1, 2, 3
LEVEL_NAMES = {NONE: "none", PATCH: "patch", MINOR: "minor", MAJOR: "major"}

COMMAND = "command"
INFORMATION = "information"

# Keywords that change nothing a reader or writer checks.
IGNORED = {"$schema", "title", "description", "default", "examples", "$comment"}
# Keywords bounding a value. `max*` bounds tighten as they shrink, `min*`
# bounds as they grow; the others have no direction.
UPPER_BOUNDS = {
    "maxLength",
    "maxItems",
    "maxProperties",
    "maximum",
    "exclusiveMaximum",
    "x-cookie-agent-maxUtf8Bytes",
    "x-max-serialized-bytes",
}
LOWER_BOUNDS = {"minLength", "minItems", "minProperties", "minimum", "exclusiveMinimum"}
OTHER_BOUNDS = {"pattern", "format", "uniqueItems", "multipleOf"}
OPEN = "x-open"

VERSION = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


class Finding:
    def __init__(self, level, rule, where, rejected=False):
        self.level = level
        self.rule = rule
        self.where = where
        self.rejected = rejected

    def key(self):
        return (self.rejected, self.level, self.rule, self.where)

    def __str__(self):
        label = "rejected" if self.rejected else LEVEL_NAMES[self.level]
        return f"[{label}] {self.where}: {self.rule}"


def parse_version(text, where):
    match = VERSION.match(text.strip())
    if not match:
        raise SystemExit(f"{where}: {text.strip()!r} is not MAJOR.MINOR.PATCH")
    return tuple(int(part) for part in match.groups())


def load_surface(root, surface):
    directory = pathlib.Path(root) / surface
    if not directory.is_dir():
        return None
    schemas = {}
    schema_root = directory / "schemas"
    if schema_root.is_dir():
        for path in sorted(schema_root.glob("*.schema.json")):
            schemas[path.name[: -len(".schema.json")]] = json.loads(path.read_text())
    return {
        "version": parse_version((directory / "version").read_text(), directory / "version"),
        "methods": json.loads((directory / "methods.json").read_text()),
        "schemas": schemas,
    }


def roles(methods):
    """The roles of each root a surface's methods.json names."""
    found = {}
    for method in methods.get("methods", []):
        default = COMMAND if method["direction"] == "to_daemon" else INFORMATION
        params_role = method.get("params_role", default)
        found.setdefault(method["params"], set()).add(params_role)
        if method.get("result"):
            found.setdefault(method["result"], set()).add(INFORMATION)
    for name in methods.get("information", []):
        found.setdefault(name, set()).add(INFORMATION)
    return found


class Comparison:
    """Compares one root's schemas under one role."""

    def __init__(self, old_root, new_root, role, findings):
        self.old_defs = old_root.get("$defs", {})
        self.new_defs = new_root.get("$defs", {})
        self.role = role
        self.findings = findings
        self.seen = set()

    def add(self, level, rule, where, rejected=False):
        self.findings.append(Finding(level, rule, where, rejected))

    def by_role(self, information, command, rule, where):
        self.add(information if self.role == INFORMATION else command, rule, where)

    def resolve(self, node, defs):
        """`node` with its `$ref` merged in, and the referenced name."""
        if not isinstance(node, dict) or "$ref" not in node:
            return node, None
        name = node["$ref"].rsplit("/", 1)[-1]
        target = defs.get(name)
        if target is None:
            return {key: value for key, value in node.items() if key != "$ref"}, name
        merged = copy.deepcopy(target)
        for key, value in node.items():
            if key == "$ref":
                continue
            if key == "properties":
                merged.setdefault("properties", {}).update(value)
            elif key == "required":
                merged["required"] = sorted(set(merged.get("required", [])) | set(value))
            else:
                merged[key] = value
        return merged, name

    def compare(self, old, new, where):
        old, old_ref = self.resolve(old, self.old_defs)
        new, new_ref = self.resolve(new, self.new_defs)
        if old_ref or new_ref:
            key = (old_ref, new_ref)
            if key in self.seen:
                return
            self.seen.add(key)
            where = new_ref or old_ref
        if isinstance(old, bool) or isinstance(new, bool):
            if old != new:
                self.add(MAJOR, "type changed", where)
            return
        if self.compare_enum(old, new, where):
            return
        if self.compare_union(old, new, where):
            return
        if normalized_type(old) != normalized_type(new):
            self.add(MAJOR, f"type changed from {old.get('type')} to {new.get('type')}", where)
            return
        if old.get("const", None) != new.get("const", None) or ("const" in old) != ("const" in new):
            self.add(MAJOR, "const changed", where)
        self.compare_bounds(old, new, where)
        self.compare_object(old, new, where)
        if isinstance(old.get("items"), (dict, bool)) or isinstance(new.get("items"), (dict, bool)):
            self.compare(old.get("items", True), new.get("items", True), f"{where}[]")

    def compare_enum(self, old, new, where):
        old_values = enum_values(old)
        new_values = enum_values(new)
        if old_values is None and new_values is None:
            return False
        if old_values is None or new_values is None:
            self.add(MAJOR, "an enum became another kind of type", where)
            return True
        self.compare_openness(old, new, where)
        for value in sorted(old_values - new_values, key=str):
            self.add(MAJOR, f"enum value {value!r} removed", where)
        for value in sorted(new_values - old_values, key=str):
            if new.get(OPEN) is True:
                self.add(MINOR, f"value {value!r} added to an open enum", where)
            else:
                self.add(MAJOR, f"value {value!r} added to a closed enum", where)
        if normalized_type(old) != normalized_type(new):
            self.add(MAJOR, "enum type changed", where)
        return True

    def compare_openness(self, old, new, where):
        if old.get(OPEN) is True and new.get(OPEN) is not True:
            self.add(MAJOR, "an open enum became closed", where)
        elif old.get(OPEN) is not True and new.get(OPEN) is True:
            self.add(MINOR, "a closed enum became open", where)

    def compare_union(self, old, new, where):
        keyword = next((key for key in ("oneOf", "anyOf", "allOf") if key in old or key in new), None)
        if keyword is None:
            return False
        old_variants = old.get(keyword)
        new_variants = new.get(keyword)
        if old_variants is None or new_variants is None:
            self.add(MAJOR, f"{keyword} added or removed", where)
            return True
        old_resolved = [self.resolve(variant, self.old_defs)[0] for variant in old_variants]
        new_resolved = [self.resolve(variant, self.new_defs)[0] for variant in new_variants]
        tag = union_tag(old_resolved) if keyword != "allOf" else None
        if tag is not None and union_tag(new_resolved) == tag:
            self.compare_openness(old, new, where)
            old_tagged = {variant["properties"][tag]["const"]: variant for variant in old_resolved}
            new_tagged = {variant["properties"][tag]["const"]: variant for variant in new_resolved}
            for name in sorted(old_tagged.keys() - new_tagged.keys()):
                self.add(MAJOR, f"variant {name!r} removed", where)
            for name in sorted(new_tagged.keys() - old_tagged.keys()):
                if new.get(OPEN) is True:
                    self.add(MINOR, f"variant {name!r} added to an open enum", where)
                else:
                    self.add(MAJOR, f"variant {name!r} added to a closed enum", where)
            for name in sorted(old_tagged.keys() & new_tagged.keys()):
                self.compare(old_tagged[name], new_tagged[name], f"{where}.{name}")
        elif len(old_variants) != len(new_variants):
            self.add(MAJOR, f"{keyword} changed shape", where)
        else:
            for index, (old_variant, new_variant) in enumerate(zip(old_variants, new_variants)):
                self.compare(old_variant, new_variant, f"{where}.{keyword}[{index}]")
        rest_old = {key: value for key, value in old.items() if key != keyword}
        rest_new = {key: value for key, value in new.items() if key != keyword}
        if rest_old.get("properties") or rest_new.get("properties"):
            self.compare_object(rest_old, rest_new, where)
        return True

    def compare_bounds(self, old, new, where):
        for keyword in sorted(UPPER_BOUNDS | LOWER_BOUNDS | OTHER_BOUNDS):
            if old.get(keyword) == new.get(keyword):
                continue
            if keyword in UPPER_BOUNDS:
                tightened = keyword not in old or (
                    keyword in new and new[keyword] < old[keyword]
                )
            elif keyword in LOWER_BOUNDS:
                tightened = keyword not in old or (
                    keyword in new and new[keyword] > old[keyword]
                )
            else:
                tightened = keyword in new
            direction = "tightened" if tightened else "loosened"
            self.by_role(MAJOR, MINOR, f"bound {keyword} {direction}", where)

    def compare_object(self, old, new, where):
        old_properties = old.get("properties", {})
        new_properties = new.get("properties", {})
        old_required = set(old.get("required", []))
        new_required = set(new.get("required", []))
        for name in sorted(old_properties.keys() - new_properties.keys()):
            self.add(MAJOR, f"field {name!r} removed or renamed", where)
        for name in sorted(new_properties.keys() - old_properties.keys()):
            if name in new_required:
                self.by_role(MINOR, MAJOR, f"required field {name!r} added", where)
            else:
                self.add(MINOR, f"optional field {name!r} added", where)
        for name in sorted(old_properties.keys() & new_properties.keys()):
            if name not in old_required and name in new_required:
                self.by_role(MINOR, MAJOR, f"field {name!r} became required", where)
            elif name in old_required and name not in new_required:
                self.by_role(MAJOR, MINOR, f"field {name!r} became optional", where)
            self.compare(old_properties[name], new_properties[name], f"{where}.{name}")
        old_extra = old.get("additionalProperties")
        new_extra = new.get("additionalProperties")
        if isinstance(old_extra, dict) or isinstance(new_extra, dict):
            self.compare(
                old_extra if old_extra is not None else True,
                new_extra if new_extra is not None else True,
                f"{where}{{}}",
            )
        elif old_extra is False and new_extra is not False:
            self.by_role(MAJOR, MINOR, "unknown fields became allowed", where)
        elif old_extra is not False and new_extra is False and self.role == COMMAND:
            self.add(MAJOR, "unknown fields became rejected", where)
        old_patterns = old.get("patternProperties", {})
        new_patterns = new.get("patternProperties", {})
        for pattern in sorted(old_patterns.keys() ^ new_patterns.keys()):
            self.add(MAJOR, f"pattern properties {pattern!r} changed", where)
        for pattern in sorted(old_patterns.keys() & new_patterns.keys()):
            self.compare(old_patterns[pattern], new_patterns[pattern], f"{where}{{{pattern}}}")
        if "propertyNames" in old or "propertyNames" in new:
            self.compare(old.get("propertyNames", True), new.get("propertyNames", True), f"{where}{{names}}")


def normalized_type(node):
    kind = node.get("type")
    if isinstance(kind, list):
        return tuple(sorted(kind))
    return (kind,) if kind is not None else ()


def enum_values(node):
    """The values of an enum schema: `enum`, or a `oneOf` of `const`s."""
    if "enum" in node:
        return {json.dumps(value, sort_keys=True) for value in node["enum"]}
    variants = node.get("oneOf")
    if variants and all(isinstance(variant, dict) and "const" in variant for variant in variants):
        return {json.dumps(variant["const"], sort_keys=True) for variant in variants}
    return None


def union_tag(variants):
    """The property every variant fixes with a string `const`, if any."""
    candidates = None
    for variant in variants:
        properties = variant.get("properties", {}) if isinstance(variant, dict) else {}
        tags = {
            name
            for name, schema in properties.items()
            if isinstance(schema, dict) and isinstance(schema.get("const"), str)
        }
        candidates = tags if candidates is None else candidates & tags
    if not candidates:
        return None
    return "type" if "type" in candidates else sorted(candidates)[0]


def closed_information_types(name, surface):
    """A rejection for every type reached as information that has
    `additionalProperties: false`, wherever it is in the schemas."""
    findings = []
    for root, root_roles in sorted(roles(surface["methods"]).items()):
        schema = surface["schemas"].get(root)
        if INFORMATION not in root_roles or schema is None:
            continue
        defs = schema.get("$defs", {})
        seen = set()
        pending = [(schema, root)]
        while pending:
            node, where = pending.pop()
            if isinstance(node, list):
                pending.extend((item, where) for item in node)
                continue
            if not isinstance(node, dict):
                continue
            if "$ref" in node:
                target = node["$ref"].rsplit("/", 1)[-1]
                if target not in seen and target in defs:
                    seen.add(target)
                    pending.append((defs[target], target))
            # A map with typed keys (`patternProperties`) states its key
            # grammar with `additionalProperties: false`; it has no fields.
            if node.get("additionalProperties") is False and "patternProperties" not in node:
                findings.append(
                    Finding(
                        MAJOR,
                        "additionalProperties: false on an information type",
                        f"{name}:{where}",
                        rejected=True,
                    )
                )
            for keyword in ("items", "oneOf", "anyOf", "allOf", "additionalProperties", "propertyNames"):
                if keyword in node:
                    pending.append((node[keyword], where))
            for keyword in ("properties", "patternProperties"):
                pending.extend((child, where) for child in node.get(keyword, {}).values())
    return findings


def compare_surface(name, old, new):
    """The findings of one surface's change from `old` to `new`."""
    findings = []

    def add(level, rule, where):
        findings.append(Finding(level, rule, where))

    old_methods = {(method["method"], method["direction"]): method for method in old["methods"].get("methods", [])}
    new_methods = {(method["method"], method["direction"]): method for method in new["methods"].get("methods", [])}
    for key in sorted(old_methods.keys() - new_methods.keys()):
        add(MAJOR, "method or notification removed", f"{name}:{key[0]} ({key[1]})")
    for key in sorted(new_methods.keys() - old_methods.keys()):
        add(MINOR, "method or notification added", f"{name}:{key[0]} ({key[1]})")
    for key in sorted(old_methods.keys() & new_methods.keys()):
        old_method, new_method = old_methods[key], new_methods[key]
        for member in ("kind", "params", "result"):
            if old_method.get(member) != new_method.get(member):
                add(
                    MAJOR,
                    f"{member} changed from {old_method.get(member)!r} to {new_method.get(member)!r}",
                    f"{name}:{key[0]}",
                )
    old_information = set(old["methods"].get("information", []))
    new_information = set(new["methods"].get("information", []))
    for root in sorted(old_information - new_information):
        add(MAJOR, "information root removed", f"{name}:{root}")
    for root in sorted(new_information - old_information):
        add(MINOR, "information root added", f"{name}:{root}")

    old_roles = roles(old["methods"])
    new_roles = roles(new["methods"])
    for root in sorted(set(old_roles) | set(new_roles)):
        if root not in old["schemas"] or root not in new["schemas"]:
            if root in old_roles and root in new_roles:
                add(MAJOR, "schema file missing", f"{name}:{root}")
            continue
        for role in sorted(old_roles.get(root, set()) | new_roles.get(root, set())):
            comparison = Comparison(old["schemas"][root], new["schemas"][root], role, findings)
            comparison.compare(old["schemas"][root], new["schemas"][root], root)
    findings.extend(closed_information_types(name, new))
    unique = {}
    for finding in findings:
        unique.setdefault(finding.key(), finding)
    return sorted(unique.values(), key=Finding.key, reverse=True)


def required_level(findings):
    return max((finding.level for finding in findings), default=NONE)


def bump_level(old, new):
    if new < old:
        return None
    if new[0] != old[0]:
        return MAJOR
    if new[1] != old[1]:
        return MINOR
    if new[2] != old[2]:
        return PATCH
    return NONE


def version_text(version):
    return ".".join(str(part) for part in version)


def identical(old, new):
    return (
        old["version"] == new["version"]
        and old["methods"] == new["methods"]
        and old["schemas"] == new["schemas"]
    )


def differences(old, new):
    lines = []
    if old["version"] != new["version"]:
        lines.append(f"version {version_text(old['version'])} -> {version_text(new['version'])}")
    if old["methods"] != new["methods"]:
        lines.append("methods.json differs")
    for root in sorted(old["schemas"].keys() | new["schemas"].keys()):
        if old["schemas"].get(root) != new["schemas"].get(root):
            state = (
                "added"
                if root not in old["schemas"]
                else "removed" if root not in new["schemas"] else "changed"
            )
            lines.append(f"schemas/{root}.schema.json {state}")
    return lines


def evaluate(name, old, new):
    """The outcome of one surface's check: `errors` fail it; `stale` (a
    pre-freeze baseline that differs) fails it unless it is being updated."""
    outcome = {"errors": [], "stale": [], "notes": [], "findings": compare_surface(name, old, new)}
    findings = outcome["findings"]
    if new["version"][0] == 0 and old["version"][0] == 0:
        if not identical(old, new):
            outcome["stale"].append(
                f"{name}: the baseline is not current; run crates/protocol/scripts/update-compat-baseline.sh"
            )
            outcome["stale"].extend(f"  {line}" for line in differences(old, new))
        if findings:
            rejected = sum(1 for finding in findings if finding.rejected)
            outcome["notes"].append(
                f"{name}: pre-freeze ({version_text(new['version'])}), rules not enforced: "
                f"{rejected} information types reject unknown fields, "
                f"{len(findings) - rejected} other findings"
            )
        return outcome
    errors = outcome["errors"]
    errors.extend(f"{name}: {finding}" for finding in findings if finding.rejected)
    level = required_level(findings)
    bump = bump_level(old["version"], new["version"])
    if bump is None:
        errors.append(
            f"{name}: version went down from {version_text(old['version'])} to {version_text(new['version'])}"
        )
    elif bump < level:
        errors.append(
            f"{name}: the changes need a {LEVEL_NAMES[level]} bump from "
            f"{version_text(old['version'])}, but the version is {version_text(new['version'])}"
        )
        errors.extend(f"  {finding}" for finding in findings if finding.level > bump and not finding.rejected)
    if name == "events":
        missing = sorted(event_tags(new) - event_tags(old))
        if missing:
            outcome["stale"].append(
                f"events: tags {missing} are not in the baseline; record them with update-compat-baseline.sh"
            )
    return outcome


def event_tags(surface):
    schema = surface["schemas"].get("EventPayload", {})
    return {
        variant.get("properties", {}).get("type", {}).get("const")
        for variant in schema.get("oneOf", [])
        if isinstance(variant, dict)
    } - {None}


def run(baseline_root, generated_root, update=False, verbose=False, out=sys.stdout):
    """Checks every surface; returns the process exit code."""
    failed = False
    results = {}
    for name in SURFACES:
        new = load_surface(generated_root, name)
        if new is None:
            print(f"{name}: the generated set has no {name} surface", file=out)
            failed = True
            continue
        old = load_surface(baseline_root, name)
        if old is None:
            if update:
                results[name] = new
                continue
            print(f"{name}: no baseline; run crates/protocol/scripts/update-compat-baseline.sh", file=out)
            failed = True
            continue
        outcome = evaluate(name, old, new)
        # The update itself makes a stale baseline current.
        errors = outcome["errors"] + ([] if update else outcome["stale"])
        for line in outcome["notes"]:
            print(line, file=out)
        if verbose:
            for finding in outcome["findings"]:
                print(f"  {finding}", file=out)
        for line in errors:
            print(line, file=out)
        failed = failed or bool(errors)
        results[name] = new
    if failed:
        if update:
            print("refusing to update the baseline", file=out)
        return 1
    if update:
        for name in SURFACES:
            target = pathlib.Path(baseline_root) / name
            if target.exists():
                shutil.rmtree(target)
            shutil.copytree(pathlib.Path(generated_root) / name, target)
        print(f"updated the baselines in {baseline_root}", file=out)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--generated", required=True)
    parser.add_argument("--update", action="store_true")
    parser.add_argument("--verbose", action="store_true")
    arguments = parser.parse_args()
    sys.exit(run(arguments.baseline, arguments.generated, arguments.update, arguments.verbose))


if __name__ == "__main__":
    main()

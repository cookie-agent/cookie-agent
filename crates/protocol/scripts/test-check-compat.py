#!/usr/bin/env python3
"""Mutation test of check-compat.py: one case per row of the rules table in
`docs/site/specs/compatibility-v1.md` section 7.1.

Usage: test-check-compat.py GENERATED_DIR

Each case mutates a copy of the generated compatibility set, stamped `1.0.0`.
`additionalProperties: false` is stripped from both sides first, since
command types reject unknown fields and the cases close types themselves. A
case passes when the checker requires exactly its level, rejects one level
less, and accepts it.
"""

import copy
import importlib.util
import io
import pathlib
import sys
import tempfile

sys.dont_write_bytecode = True

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check_compat", HERE / "check-compat.py")
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)

NONE, PATCH, MINOR, MAJOR = checker.NONE, checker.PATCH, checker.MINOR, checker.MAJOR
REJECTED = "rejected"


def strip_closed(node):
    if isinstance(node, dict):
        if node.get("additionalProperties") is False:
            del node["additionalProperties"]
        for value in node.values():
            strip_closed(value)
    elif isinstance(node, list):
        for value in node:
            strip_closed(value)


def load(root):
    surfaces = {name: checker.load_surface(root, name) for name in checker.SURFACES}
    for surface in surfaces.values():
        surface["version"] = (1, 0, 0)
        strip_closed(surface["schemas"])
    return surfaces


def bumped(version, level):
    major, minor, patch = version
    if level == MAJOR:
        return (major + 1, 0, 0)
    if level == MINOR:
        return (major, minor + 1, 0)
    if level == PATCH:
        return (major, minor, patch + 1)
    return version


def schema(surfaces, surface, root):
    try:
        return surfaces[surface]["schemas"][root]
    except KeyError:
        raise SystemExit(f"test fixture missing: {surface}/{root}; update test-check-compat.py")


def definition(surfaces, surface, root, name):
    defs = schema(surfaces, surface, root).get("$defs", {})
    if name not in defs:
        raise SystemExit(f"test fixture missing: {surface}/{root} $defs {name}")
    return defs[name]


def add_field(surface, root, required):
    def mutate(surfaces):
        node = schema(surfaces, surface, root)
        node["properties"]["future_field"] = {"type": "string"}
        if required:
            node["required"].append("future_field")

    return mutate


def remove_field(surface, root, field):
    def mutate(surfaces):
        node = schema(surfaces, surface, root)
        del node["properties"][field]
        if field in node.get("required", []):
            node["required"].remove(field)

    return mutate


def make_required(surface, root, field):
    def mutate(surfaces):
        node = schema(surfaces, surface, root)
        assert field not in node["required"], (root, field)
        node["required"].append(field)

    return mutate


def set_bound(surface, root, field, keyword, value):
    def mutate(surfaces):
        node = schema(surfaces, surface, root)["properties"][field]
        assert keyword in node, (root, field, keyword)
        node[keyword] = value

    return mutate


def hook_action(surfaces):
    return definition(surfaces, "plugin", "ExtensionToolBeforeCallResult", "ExtensionToolBeforeCallAction")


def open_hook_action(surfaces):
    hook_action(surfaces)["x-open"] = True


def add_hook_action(surfaces):
    hook_action(surfaces)["enum"].append("ask")


def remove_hook_action(surfaces):
    hook_action(surfaces)["enum"].remove("block")


def change_const(surfaces):
    origin = schema(surfaces, "plugin", "ExtensionToolBeforeCallParams")["$defs"]["ExtensionToolOrigin"]
    node = origin["oneOf"][0]["properties"]["kind"]
    node["const"] = node["const"] + "_layer"


def close_information(surfaces):
    schema(surfaces, "runtime", "ContextResult")["additionalProperties"] = False


def close_command(surfaces):
    schema(surfaces, "runtime", "ToolProgressParams")["additionalProperties"] = False


def change_type(surface, root, field):
    def mutate(surfaces):
        schema(surfaces, surface, root)["properties"][field] = {"type": "boolean"}

    return mutate


def nested_field_removed(surfaces):
    error = definition(surfaces, "runtime", "ExecuteResult", "WireToolError")
    del error["properties"]["message"]
    error["required"].remove("message")


def add_method(surfaces):
    surfaces["runtime"]["methods"]["methods"].append(
        {
            "direction": "from_daemon",
            "kind": "request",
            "method": "runtime/example",
            "params": "Empty",
            "result": "Empty",
        }
    )


def remove_method(surfaces):
    methods = surfaces["runtime"]["methods"]["methods"]
    before = len(methods)
    methods[:] = [method for method in methods if method["method"] != "tool/cancel"]
    assert len(methods) == before - 1


def event_payload(surfaces):
    return schema(surfaces, "events", "EventPayload")


def open_events(surfaces):
    event_payload(surfaces)["x-open"] = True


def close_events(surfaces):
    event_payload(surfaces).pop("x-open", None)


def add_event(surfaces):
    event_payload(surfaces)["oneOf"].append(
        {
            "type": "object",
            "properties": {"type": {"type": "string", "const": "future_event"}},
            "required": ["type"],
        }
    )


def remove_event(surfaces):
    variants = event_payload(surfaces)["oneOf"]
    variants[:] = [
        variant for variant in variants if variant["properties"]["type"]["const"] != "run_finished"
    ]


def describe(surfaces):
    schema(surfaces, "runtime", "ContextResult")["description"] = "reworded"


# (name, surface whose version the case bumps, mutation, expected level,
#  mutation applied to the baseline as well)
CASES = [
    ("information optional field added", "runtime", add_field("runtime", "ContextResult", False), MINOR, None),
    ("command optional field added", "runtime", add_field("runtime", "ToolProgressParams", False), MINOR, None),
    ("information required field added", "runtime", add_field("runtime", "ContextResult", True), MINOR, None),
    ("command required field added", "runtime", add_field("runtime", "ToolProgressParams", True), MAJOR, None),
    ("information field removed", "runtime", remove_field("runtime", "ContextResult", "skipped"), MAJOR, None),
    ("command field removed", "runtime", remove_field("runtime", "ToolProgressParams", "prepared_id"), MAJOR, None),
    ("information field type changed", "runtime", change_type("runtime", "ContextResult", "skipped"), MAJOR, None),
    ("command field type changed", "runtime", change_type("runtime", "ToolProgressParams", "prepared_id"), MAJOR, None),
    ("information optional field became required", "plugin", make_required("plugin", "ExtensionEmitResultParams", "reason"), MINOR, None),
    ("command optional field became required", "runtime", make_required("runtime", "DetachableParams", "note"), MAJOR, None),
    ("information bound tightened", "plugin", set_bound("plugin", "ExtensionEventParams", "seq", "minimum", 2), MAJOR, None),
    ("command bound tightened", "plugin", set_bound("plugin", "ExtensionEmitParams", "name", "maxLength", 64), MINOR, None),
    ("information bound loosened", "plugin", set_bound("plugin", "ExtensionEventParams", "seq", "minimum", 0), MAJOR, None),
    ("command bound loosened", "plugin", set_bound("plugin", "ExtensionEmitParams", "name", "maxLength", 256), MINOR, None),
    ("value added to an open enum", "plugin", add_hook_action, MINOR, open_hook_action),
    ("value added to a closed enum", "plugin", add_hook_action, MAJOR, None),
    ("value removed from an open enum", "plugin", remove_hook_action, MAJOR, open_hook_action),
    ("value removed from a closed enum", "plugin", remove_hook_action, MAJOR, None),
    ("variant added to an open tagged enum", "events", add_event, MINOR, open_events),
    ("variant added to a closed tagged enum", "events", add_event, MAJOR, close_events),
    ("variant removed", "events", remove_event, MAJOR, None),
    ("const changed", "plugin", change_const, MAJOR, None),
    ("field removed in a nested definition", "runtime", nested_field_removed, MAJOR, None),
    ("additionalProperties false on an information type", "runtime", close_information, REJECTED, None),
    ("additionalProperties false on a command", "runtime", close_command, MAJOR, None),
    ("method added", "runtime", add_method, MINOR, None),
    ("method removed", "runtime", remove_method, MAJOR, None),
    ("no schema change", "runtime", describe, NONE, None),
]


def outcome_errors(name, old, new):
    """The errors of a check, without the events surface's reminder to
    record new tags, and the findings."""
    outcome = checker.evaluate(name, old, new)
    stale = [error for error in outcome["stale"] if "not in the baseline" not in error]
    return outcome["errors"] + stale, outcome["findings"]


def records_new_tags(old, new):
    outcome = checker.evaluate("events", old, new)
    return any("not in the baseline" in error for error in outcome["stale"])


def run_case(base, case):
    name, surface, mutate, expected, prepare = case
    baseline = copy.deepcopy(base)
    if prepare:
        prepare(baseline)
    candidate = copy.deepcopy(baseline)
    mutate(candidate)
    failures = []
    old, new = baseline[surface], copy.deepcopy(candidate[surface])
    _, findings = outcome_errors(surface, old, new)
    if expected == REJECTED:
        new["version"] = bumped(old["version"], MAJOR)
        errors, _ = outcome_errors(surface, old, new)
        if not any("rejected" in error for error in errors):
            failures.append(f"{name}: accepted with a major bump: {errors}")
        return failures
    level = checker.required_level(findings)
    if level != expected:
        failures.append(
            f"{name}: requires {checker.LEVEL_NAMES[level]}, expected {checker.LEVEL_NAMES[expected]}: "
            + "; ".join(str(finding) for finding in findings)
        )
    new["version"] = bumped(old["version"], expected)
    errors, _ = outcome_errors(surface, old, new)
    if errors:
        failures.append(f"{name}: rejected with a {checker.LEVEL_NAMES[expected]} bump: {errors}")
    # A new event tag passes the bump check and still waits for the update.
    if mutate is add_event and not records_new_tags(old, new):
        failures.append(f"{name}: a new event tag did not ask for the update script")
    if expected > NONE:
        new["version"] = bumped(old["version"], expected - 1)
        errors, _ = outcome_errors(surface, old, new)
        if not errors:
            failures.append(f"{name}: accepted with a {checker.LEVEL_NAMES[expected - 1]} bump")
    return failures


def write(root, surfaces):
    for name, surface in surfaces.items():
        directory = pathlib.Path(root) / name
        (directory / "schemas").mkdir(parents=True)
        (directory / "version").write_text(checker.version_text(surface["version"]) + "\n")
        (directory / "methods.json").write_text(checker.json.dumps(surface["methods"]))
        for root_name, document in surface["schemas"].items():
            (directory / "schemas" / f"{root_name}.schema.json").write_text(checker.json.dumps(document))


CORPUS_FILE = pathlib.Path("corpus") / "client" / "1.0.0" / "session.jsonl"


def run_cli(baseline, candidate, update=False):
    """The checker's exit code, the baselines after it ran, and whether the
    frozen corpus beside them is untouched."""
    with tempfile.TemporaryDirectory() as temporary:
        base_root = pathlib.Path(temporary) / "baseline"
        new_root = pathlib.Path(temporary) / "generated"
        write(base_root, baseline)
        write(new_root, candidate)
        corpus = base_root / CORPUS_FILE
        corpus.parent.mkdir(parents=True)
        corpus.write_text("recorded\n")
        code = checker.run(base_root, new_root, update=update, out=io.StringIO())
        after = {name: checker.load_surface(base_root, name) for name in checker.SURFACES}
        kept = corpus.read_text() == "recorded\n"
    return code, after, kept


def version_cases(base):
    failures = []
    candidate = copy.deepcopy(base)
    # A build from before the freeze is lower than a frozen baseline.
    candidate["client"]["version"] = (0, 52, 0)
    errors, _ = outcome_errors("client", base["client"], candidate["client"])
    if not any("went down" in error for error in errors):
        failures.append(f"a lower version was accepted: {errors}")

    # The CLI: the update refuses an insufficient bump and leaves the
    # baseline alone; a sufficient one replaces it.
    insufficient = copy.deepcopy(base)
    remove_method(insufficient)
    insufficient["runtime"]["version"] = bumped(base["runtime"]["version"], MINOR)
    code, after, _ = run_cli(base, insufficient, update=True)
    if code == 0 or after["runtime"]["methods"] != base["runtime"]["methods"]:
        failures.append("update accepted an insufficient bump or changed the baseline")
    sufficient = copy.deepcopy(insufficient)
    sufficient["runtime"]["version"] = bumped(base["runtime"]["version"], MAJOR)
    code, after, kept = run_cli(base, sufficient, update=True)
    if code != 0 or after["runtime"]["methods"] != sufficient["runtime"]["methods"]:
        failures.append("update refused a sufficient bump or did not record it")
    if not kept:
        failures.append("update changed the frozen corpus")
    code, _, _ = run_cli(base, copy.deepcopy(base))
    if code != 0:
        failures.append("an unchanged set failed the check")
    return failures


def main():
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} GENERATED_DIR")
    base = load(sys.argv[1])
    failures = []
    for case in CASES:
        failures.extend(run_case(base, case))
    failures.extend(version_cases(base))
    if failures:
        raise SystemExit("check-compat.py mutation test failed:\n" + "\n".join(failures))
    print(f"check-compat.py mutation test: {len(CASES)} rule cases and the version cases passed")


if __name__ == "__main__":
    main()

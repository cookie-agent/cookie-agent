"""Checks the reference plugin's messages against the plugin protocol's
published schemas, with no dependencies.

It plays the daemon: it starts reference_plugin.py, sends it a scripted
conversation (the handshake, producer recovery, a tool call with a hook and
progress, events, and shutdown), and validates every message either side
sends against `crates/protocol/compat/plugin` (methods.json and
schemas/<Root>.schema.json). The validator covers the subset of JSON Schema
the generated schemas use: types, properties, required, additionalProperties,
$ref, oneOf, anyOf, allOf, enum, const, items, item and length bounds,
numeric bounds, patterns (with the Unicode classes `\\p{Cc}` and `\\p{Cf}`),
and `x-open` enums, which also accept values they do not list.

With `--record ROOT` it also writes the conversation, both directions in
order, to ROOT/plugin/<version>/reference.jsonl as the frozen plugin corpus
(`crates/protocol/compat/corpus`), and refuses when that directory exists: a
recorded corpus is never rewritten.

    python3 examples/plugins/python/check_reference_plugin.py [--schemas DIR] [--record ROOT]
"""

import argparse
import functools
import json
import os
import re
import subprocess
import sys
import unicodedata
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_SCHEMAS = os.path.join(HERE, "..", "..", "..", "crates", "protocol", "compat", "plugin")


# --- the validator -----------------------------------------------------------

@functools.lru_cache(maxsize=None)
def _category_class(category):
    """A regular-expression class body of every code point of a Unicode
    general category."""
    ranges, start, previous = [], None, None
    for code in range(sys.maxunicode + 1):
        if unicodedata.category(chr(code)) == category:
            if start is None:
                start = code
            previous = code
        elif start is not None:
            ranges.append((start, previous))
            start = None
    if start is not None:
        ranges.append((start, previous))
    return "".join(
        "\\U%08x" % low if low == high else "\\U%08x-\\U%08x" % (low, high)
        for low, high in ranges
    )


@functools.lru_cache(maxsize=None)
def _pattern(pattern):
    for category in ("Cc", "Cf"):
        pattern = pattern.replace("\\p{%s}" % category, _category_class(category))
    return re.compile(pattern)


def _type_matches(name, value):
    if name == "null":
        return value is None
    if name == "boolean":
        return isinstance(value, bool)
    if name == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if name == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if name == "string":
        return isinstance(value, str)
    if name == "array":
        return isinstance(value, list)
    if name == "object":
        return isinstance(value, dict)
    raise ValueError("unknown schema type %r" % name)


class Validator:
    def __init__(self, root):
        self.root = root

    def resolve(self, reference):
        if not reference.startswith("#/"):
            raise ValueError("unsupported $ref %r" % reference)
        node = self.root
        for part in reference[2:].split("/"):
            node = node[part.replace("~1", "/").replace("~0", "~")]
        return node

    def errors(self, schema, value, path="$"):
        """Every way `value` fails `schema`, as (path, reason) pairs."""
        if schema is True or schema == {}:
            return []
        if schema is False:
            return [(path, "no value is allowed here")]
        found = []
        if "$ref" in schema:
            found += self.errors(self.resolve(schema["$ref"]), value, path)
        types = schema.get("type")
        if types is not None:
            names = types if isinstance(types, list) else [types]
            if not any(_type_matches(name, value) for name in names):
                return found + [(path, "expected %s, got %s" % ("/".join(names), json.dumps(value)[:80]))]
        if "const" in schema and value != schema["const"]:
            found.append((path, "expected %s" % json.dumps(schema["const"])))
        if "enum" in schema and value not in schema["enum"]:
            if not (schema.get("x-open") and isinstance(value, str)):
                found.append((path, "%s is not one of %s" % (json.dumps(value), schema["enum"])))
        for keyword in ("allOf",):
            for branch in schema.get(keyword, []):
                found += self.errors(branch, value, path)
        if "anyOf" in schema:
            if not any(not self.errors(branch, value, path) for branch in schema["anyOf"]):
                found += self._choice_errors(schema, "anyOf", value, path)
        if "oneOf" in schema:
            passing = [branch for branch in schema["oneOf"] if not self.errors(branch, value, path)]
            if len(passing) > 1:
                found.append((path, "matches %d branches of oneOf" % len(passing)))
            elif not passing:
                found += self._choice_errors(schema, "oneOf", value, path)
        if isinstance(value, str):
            if "minLength" in schema and len(value) < schema["minLength"]:
                found.append((path, "shorter than %d" % schema["minLength"]))
            if "maxLength" in schema and len(value) > schema["maxLength"]:
                found.append((path, "longer than %d" % schema["maxLength"]))
            if "pattern" in schema and not _pattern(schema["pattern"]).search(value):
                found.append((path, "does not match %s" % schema["pattern"]))
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            if "minimum" in schema and value < schema["minimum"]:
                found.append((path, "below %s" % schema["minimum"]))
            if "maximum" in schema and value > schema["maximum"]:
                found.append((path, "above %s" % schema["maximum"]))
        if isinstance(value, list):
            if "minItems" in schema and len(value) < schema["minItems"]:
                found.append((path, "fewer than %d items" % schema["minItems"]))
            if "maxItems" in schema and len(value) > schema["maxItems"]:
                found.append((path, "more than %d items" % schema["maxItems"]))
            if schema.get("uniqueItems"):
                encoded = [json.dumps(item, sort_keys=True) for item in value]
                if len(set(encoded)) != len(encoded):
                    found.append((path, "items are not unique"))
            if "items" in schema:
                for index, item in enumerate(value):
                    found += self.errors(schema["items"], item, "%s[%d]" % (path, index))
        if isinstance(value, dict):
            properties = schema.get("properties", {})
            for name in schema.get("required", []):
                if name not in value:
                    found.append((path, "missing required member %r" % name))
            for name, member in value.items():
                member_path = "%s.%s" % (path, name)
                if name in properties:
                    found += self.errors(properties[name], member, member_path)
                elif "additionalProperties" in schema:
                    found += self.errors(schema["additionalProperties"], member, member_path)
        return found

    def _choice_errors(self, schema, keyword, value, path):
        branches = schema[keyword]
        tag = self._tag(branches)
        if tag is not None and isinstance(value, dict):
            known = {self._branch(branch)["properties"][tag]["const"] for branch in branches
                     if tag in self._branch(branch).get("properties", {})}
            if value.get(tag) not in known:
                if schema.get("x-open") and isinstance(value.get(tag), str):
                    return []
                return [(path, "%s %s is not one of %s" % (tag, json.dumps(value.get(tag)), sorted(known)))]
            for branch in branches:
                if self._branch(branch).get("properties", {}).get(tag, {}).get("const") == value.get(tag):
                    return self.errors(branch, value, path)
        if schema.get("x-open") and isinstance(value, str):
            return []
        reasons = [self.errors(branch, value, path) for branch in branches]
        return min(reasons, key=len) if reasons else [(path, "matches no branch of %s" % keyword)]

    def _branch(self, branch):
        return self.resolve(branch["$ref"]) if set(branch) == {"$ref"} else branch

    def _tag(self, branches):
        """The member every branch fixes with a `const`, if any."""
        candidates = None
        for branch in branches:
            branch = self._branch(branch)
            fixed = {name for name, member in branch.get("properties", {}).items()
                     if isinstance(member, dict) and "const" in member}
            candidates = fixed if candidates is None else candidates & fixed
        return next(iter(sorted(candidates))) if candidates else None


class Schemas:
    def __init__(self, directory):
        self.directory = directory
        with open(os.path.join(directory, "methods.json"), encoding="utf-8") as source:
            self.methods = {entry["method"]: entry for entry in json.load(source)["methods"]}
        self._roots = {}

    def root(self, name):
        if name not in self._roots:
            path = os.path.join(self.directory, "schemas", name + ".schema.json")
            with open(path, encoding="utf-8") as source:
                self._roots[name] = json.load(source)
        return self._roots[name]

    def check(self, root, value, what):
        schema = self.root(root)
        errors = Validator(schema).errors(schema, value)
        if errors:
            details = "\n".join("  %s: %s" % error for error in errors[:20])
            raise AssertionError("%s does not match %s:\n%s\n%s" % (
                what, root, details, json.dumps(value, indent=1)[:4000]))


# --- the conversation -------------------------------------------------------

class Daemon:
    """The daemon's end of a connection to the plugin."""

    def __init__(self, schemas, plugin):
        self.schemas = schemas
        self.process = subprocess.Popen(
            [sys.executable, plugin],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8",
        )
        self.next_id = 1
        self.checked = 0
        self.recorded = []

    def method(self, method):
        entry = self.schemas.methods.get(method)
        if entry is None:
            raise AssertionError("%s is not a plugin protocol method" % method)
        return entry

    def send(self, message):
        self.recorded.append({"from": "daemon", "message": message})
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def receive(self):
        line = self.process.stdout.readline()
        if not line:
            raise AssertionError("the plugin closed its output")
        message = json.loads(line)
        if message.get("jsonrpc") != "2.0":
            raise AssertionError("not a JSON-RPC 2.0 message: %s" % line)
        self.recorded.append({"from": "plugin", "message": message})
        return message

    def notify(self, method, params):
        entry = self.method(method)
        assert entry["direction"] == "from_daemon" and entry["kind"] == "notification", entry
        self.schemas.check(entry["params"], params, "the daemon's %s params" % method)
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method, params, answer):
        """Sends a request and answers the plugin's requests until its reply
        arrives; `answer(method, params)` returns each result."""
        entry = self.method(method)
        assert entry["direction"] == "from_daemon" and entry["kind"] == "request", entry
        self.schemas.check(entry["params"], params, "the daemon's %s params" % method)
        request_id = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            message = self.receive()
            if "method" in message:
                self.serve(message, answer)
                continue
            assert message.get("id") == request_id, message
            if "error" in message:
                raise AssertionError("%s failed: %s" % (method, message["error"]))
            self.schemas.check(entry["result"], message["result"], "the plugin's %s result" % method)
            self.checked += 1
            return message["result"]

    def serve(self, message, answer):
        """Checks one message the plugin sent and answers it if it is a
        request."""
        method = message["method"]
        entry = self.method(method)
        assert entry["direction"] == "to_daemon", "the plugin may not send %s" % method
        self.schemas.check(entry["params"], message.get("params"), "the plugin's %s params" % method)
        self.checked += 1
        if "id" in message:
            assert entry["kind"] == "request", "%s is a notification" % method
            result = answer(method, message.get("params"))
            self.schemas.check(entry["result"], result, "the daemon's %s result" % method)
            self.send({"jsonrpc": "2.0", "id": message["id"], "result": result})
        return method


def converse(schemas, plugin):
    daemon = Daemon(schemas, plugin)
    seen = []
    producer_id, message_id = str(uuid.uuid4()), str(uuid.uuid4())

    def answer(method, params):
        seen.append((method, params))
        if method == "tool/progress":
            return {}
        if method == "producer/recovery/complete":
            return {}
        if method == "producer/register":
            return {"producer_id": producer_id}
        if method == "producer/send":
            return {"message_id": message_id}
        raise AssertionError("unexpected request %s" % method)

    with open(os.path.join(schemas.directory, "version"), encoding="utf-8") as source:
        version = source.read().strip()
    initialized = daemon.request("plugin/initialize", {
        "protocol_version": version,
        "engine_version": "0.0.0-check",
        "features": ["producer_messaging"],
        "interception_timeout_ms": 2000,
    }, answer)
    assert initialized["name"] == "reference", initialized
    assert initialized["capabilities"]["intercept"] == [
        {"hook": "tool_before_call", "on_error": "block"}], initialized
    assert initialized["capabilities"]["subscribe_events"] == ["run_*", "tool_call_*"]

    daemon.notify("producer/recovery/start", {})
    message = daemon.receive()
    assert daemon.serve(message, answer) == "producer/recovery/complete", message

    session, run = str(uuid.uuid4()), str(uuid.uuid4())
    call_id = str(uuid.uuid4())
    turn = {"agent": "primary", "model": {
        "model": "example/model", "adapter": "openai-responses",
        "input_media": [{"kind": "image", "mime_types": ["image/png"],
                         "max_bytes": 1048576, "delivery": "tool_result"}],
    }}
    arguments = {"text": "one two\nthree", "label": "notes.txt"}
    prepared = daemon.request("tool/prepare", {
        "session": session, "run": run, "turn": turn,
        "calls": [
            {"id": call_id, "name": "word_count", "arguments": arguments},
            {"id": str(uuid.uuid4()), "name": "missing", "arguments": {}},
        ],
    }, answer)
    outcome, missing = prepared["results"]
    assert outcome["status"] == "prepared", outcome
    assert outcome["permission"] == "text_read" and outcome["permission_resource"] == "notes.txt"
    assert outcome["presentation"]["hint"] == "search", outcome
    assert missing["status"] == "failed", missing

    allowed = daemon.request("plugin/intercept/tool_before_call", {
        "session_id": session, "context_id": "check-1", "tool": "word_count",
        "origin": {"kind": "runtime", "attachment": "global", "runtime_id": "global"},
        "arguments": arguments, "permission_name": "text_read", "resource": "notes.txt",
    }, answer)
    assert allowed["action"] == "allow", allowed
    blocked = daemon.request("plugin/intercept/tool_before_call", {
        "session_id": session, "context_id": "check-2", "tool": "word_count",
        "origin": {"kind": "runtime", "attachment": "global", "runtime_id": "global"},
        "arguments": {"text": "forbidden"}, "permission_name": "text_read", "resource": "text",
    }, answer)
    assert blocked["action"] == "block", blocked

    assert daemon.request("tool/revalidate", {"prepared_id": outcome["prepared_id"]}, answer) == {}
    executed = daemon.request("tool/execute", {
        "prepared_id": outcome["prepared_id"], "session": session, "run": run, "turn": turn,
    }, answer)
    result = executed["outcome"]
    assert result["status"] == "completed", result
    assert result["metadata"] == {"words": 3, "lines": 2}, result
    progress = [params for method, params in seen if method == "tool/progress"]
    assert [entry["progress"]["display"]["type"] for entry in progress] == ["replace", "append"]

    terminated = {
        "type": "tool_call_terminated", "tool_call_id": call_id,
        "owner": {"model_turn_seq": 3, "content_index": 0, "model_call_id": "call-1",
                  "provider_item_id": None},
        "outcome": "completed", "error": None,
        "result": {
            "title": result["title"], "output": result["output"]["text"],
            "display": result["display"], "metadata": result["metadata"], "attachments": [],
        },
    }
    for event in (terminated, {"type": "run_finished", "outcome": {"type": "completed"}}):
        daemon.notify("plugin/event", {
            "session_id": session, "context_id": "check-event", "seq": 7,
            "event": event, "timestamp": "2026-10-18T12:00:00Z",
        })
    # The finished call's producer message: registration, then the send.
    for expected in ("producer/register", "producer/send"):
        message = daemon.receive()
        assert daemon.serve(message, answer) == expected, message

    daemon.notify("plugin/shutdown", {})
    daemon.process.stdin.close()
    if daemon.process.wait(timeout=10) != 0:
        raise AssertionError("the plugin exited with %s" % daemon.process.returncode)
    return daemon.checked, daemon.recorded


def corpus_directory(root, schemas):
    """ROOT/plugin/<version> for the version of `schemas`; it must not exist
    yet."""
    with open(os.path.join(schemas.directory, "version"), encoding="utf-8") as source:
        directory = os.path.join(root, "plugin", source.read().strip())
    if os.path.exists(directory):
        raise SystemExit("%s exists: a recorded corpus is never rewritten" % directory)
    return directory


def record(directory, recorded):
    """Writes the conversation as DIRECTORY/reference.jsonl."""
    os.makedirs(directory)
    path = os.path.join(directory, "reference.jsonl")
    with open(path, "w", encoding="utf-8") as target:
        for entry in recorded:
            target.write(json.dumps(entry, separators=(",", ":"), sort_keys=True,
                                    ensure_ascii=False) + "\n")
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--schemas", default=DEFAULT_SCHEMAS,
                        help="the plugin surface's compat directory")
    parser.add_argument("--plugin", default=os.path.join(HERE, "reference_plugin.py"))
    parser.add_argument("--record", metavar="ROOT",
                        help="also write the conversation to ROOT/plugin/<version>/")
    arguments = parser.parse_args()
    schemas = Schemas(os.path.abspath(arguments.schemas))
    directory = arguments.record and corpus_directory(arguments.record, schemas)
    checked, recorded = converse(schemas, arguments.plugin)
    print("reference plugin: %d messages match the plugin protocol schemas" % checked)
    if directory:
        path = record(directory, recorded)
        print("recorded %d messages in %s" % (len(recorded), path))


if __name__ == "__main__":
    main()

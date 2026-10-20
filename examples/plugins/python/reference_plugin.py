"""The reference cookie-agent plugin: every part of the plugin protocol a
plugin is likely to use, in plain Python with no dependencies.

- A tool, `word_count`, under its own permission action `text_read`. It
  presents its calls with a title, a hint, its arguments, and a preview,
  reports progress as a live display, and returns display blocks and
  metadata.
- A hook, `tool_before_call`, declared with `on_error = "block"`: it refuses
  calls whose arguments mention "forbidden", and if it fails, calls are
  blocked rather than let through.
- An event subscription by pattern (`run_*` and `tool_call_*`).
- A producer: once a `word_count` call has finished, it queues a message for
  the session's next run.

Configure it as a plugin named `reference` (docs/site/extend/plugins.md):

    [plugins.reference]
    command = ["python3", "examples/plugins/python/reference_plugin.py"]
    producer_messaging = true

With `REFERENCE_PLUGIN_LOG` set to a file, it appends one JSON line per event
and hook call it receives.
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import cookie_plugin  # noqa: E402

plugin = cookie_plugin.Plugin("reference", "1.0.0")

# The calls this plugin ran, by tool call ID, with their word counts.
counted = {}


def log(entry):
    path = os.environ.get("REFERENCE_PLUGIN_LOG")
    if path:
        with open(path, "a", encoding="utf-8") as output:
            output.write(json.dumps(entry, separators=(",", ":")) + "\n")


# --- the tool ---------------------------------------------------------------

WORD_COUNT = {
    "name": "word_count",
    "description": "Count the words and lines of a text.",
    "permission": "text_read",
    "parameters": {
        "type": "object",
        "properties": {
            "text": {"type": "string", "description": "The text to count."},
            "label": {"type": "string", "description": "What the text is, such as a file name."},
        },
        "required": ["text"],
        "additionalProperties": False,
    },
}


def word_count_resource(arguments):
    """The permission resource a call is approved under: its label."""
    return arguments.get("label") or "text"


def word_count_presentation(name, arguments, resource):
    text = arguments.get("text", "")
    return {
        "title": "Count words",
        "hint": "search",
        "primary_argument": resource,
        "arguments": [{"type": "code", "text": text[:4000], "language": "text"}],
        "preview": [{"type": "text", "text": "Counts the words of %d characters." % len(text)}],
    }


def word_count(call, arguments):
    text = arguments["text"]
    # A live display: one text block, replaced and then appended to.
    call.progress("counting", display={"type": "replace", "blocks": [
        {"type": "text", "text": "Counting words…"},
    ]})
    words = len(text.split())
    lines = len(text.splitlines())
    call.progress("counted", display={"type": "append", "index": 0, "text": " done."})
    counted[call.tool_call_id] = {"session_id": call.session_id, "words": words}
    return cookie_plugin.tool_result(
        "%d words, %d lines" % (words, lines),
        title="Counted %d words" % words,
        display=[{"type": "key_value", "entries": [
            {"key": "words", "value": str(words)},
            {"key": "lines", "value": str(lines)},
        ]}],
        metadata={"words": words, "lines": lines},
    )


plugin.tool(WORD_COUNT, word_count, resource=word_count_resource,
            presentation=word_count_presentation)


# --- the hook ----------------------------------------------------------------

def before_tool_call(params):
    log({"hook": "tool_before_call", "tool": params.get("tool")})
    if "forbidden" in json.dumps(params.get("arguments")):
        return {"action": "block", "reason": "the reference plugin refuses forbidden text"}
    return {"action": "allow"}


plugin.hook("tool_before_call", before_tool_call, on_error="block")


# --- events and the producer -------------------------------------------------

def on_event(params):
    event = params.get("event", {})
    log({"event": event.get("type"), "session_id": params.get("session_id")})
    if event.get("type") != "tool_call_terminated":
        return
    finished = counted.pop(event.get("tool_call_id"), None)
    if finished is None:
        return
    # Queued for the session's next run; the key makes a retry, such as one
    # after a restart, deliver it once.
    plugin.send_message(
        finished["session_id"],
        "The word count you asked for found %d words." % finished["words"],
        "Word count finished",
        "word-count-%s" % event["tool_call_id"],
        mode="queue",
    )


plugin.on_event(["run_*", "tool_call_*"], on_event)
plugin.enable_producers()

if __name__ == "__main__":
    plugin.run()

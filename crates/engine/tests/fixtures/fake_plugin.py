import json
import os
import sys
import time


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


stderr_line = os.environ.get("FIXTURE_STDERR")
if stderr_line:
    sys.stderr.write(stderr_line + "\n")
    sys.stderr.flush()

pid_file = os.environ.get("FIXTURE_PID_FILE")
if pid_file:
    # Published by rename: a reader sees no file or a whole pid, never the
    # empty file a kill between open and write would leave behind.
    with open(pid_file + ".tmp", "w", encoding="utf-8") as output:
        output.write(str(os.getpid()))
    os.replace(pid_file + ".tmp", pid_file)

env_file = os.environ.get("FIXTURE_ENV_FILE")
if env_file:
    with open(env_file, "w", encoding="utf-8") as output:
        json.dump({
            "parent": os.environ.get("HOME"),
            "configured": os.environ.get("FIXTURE_CONFIGURED_SENTINEL"),
        }, output)

pending_emit_context = None


def emit_from_context(context):
    configured = os.environ.get("FIXTURE_EMIT_ON_EVENT")
    if not configured:
        return
    emit = json.loads(configured)
    emit["session_id"] = os.environ.get(
        "FIXTURE_EMIT_SESSION_ID", context.get("session_id")
    )
    emit["context_id"] = context.get("context_id")
    for _ in range(int(os.environ.get("FIXTURE_EMIT_COUNT", "1"))):
        send({"jsonrpc": "2.0", "method": "plugin/emit", "params": emit})

prepared = {}
next_prepared = 1


def declarations():
    return json.loads(os.environ.get("FIXTURE_TOOLS", "[]"))


def prepare(call):
    """A prepared call as the runtime protocol's `tool/prepare` reports it."""
    global next_prepared
    declaration = next((tool for tool in declarations() if tool["name"] == call["name"]), None)
    if declaration is None:
        return {
            "status": "failed",
            "error": {"kind": "failed", "message": "unknown fixture tool"},
            "presentation": {"title": call["name"]},
        }
    permission = declaration.get("permission")
    arguments = call["arguments"]
    resource = None
    primary = os.environ.get("FIXTURE_TOOL_RESOURCE_PARAM")
    if primary and isinstance(arguments, dict) and isinstance(arguments.get(primary), str):
        resource = arguments[primary]
    prepared_id = next_prepared
    next_prepared += 1
    prepared[prepared_id] = {"call": call}
    # The daemon builds the call's approval identity from its permission,
    # resource, and normalized arguments.
    result = {
        "status": "prepared",
        "prepared_id": prepared_id,
        "normalized_arguments": arguments,
        "presentation": {"title": call["name"]},
    }
    if permission:
        result["permission"] = permission
        if resource is not None:
            result["permission_resource"] = resource
    if resource is not None:
        result["presentation"]["primary_argument"] = resource
    result.update(json.loads(os.environ.get("FIXTURE_PREPARED_EXTRA", "{}")))
    return result


def protocol_version(params):
    # Until the gate file exists, the plugin speaks an unknown version.
    gate = os.environ.get("FIXTURE_VERSION_GATE_FILE")
    if gate and not os.path.exists(gate):
        return "999.0.0"
    configured = os.environ.get("FIXTURE_PROTOCOL_VERSION")
    if configured is not None:
        return json.loads(configured)
    # By default the fixture speaks the daemon's version.
    return params.get("protocol_version")


for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method is None:
        # A reply to a request the fixture sent.
        reply_file = os.environ.get("FIXTURE_REPLY_FILE")
        if reply_file:
            with open(reply_file, "a", encoding="utf-8") as output:
                output.write(json.dumps(message, separators=(",", ":")) + "\n")
        continue
    if method == "plugin/initialize":
        time.sleep(int(os.environ.get("FIXTURE_DELAY_MS", "0")) / 1000)
        if os.environ.get("FIXTURE_MALFORMED") == "1":
            sys.stdout.write("{malformed\n")
            sys.stdout.flush()
            continue
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "protocol_version": protocol_version(message.get("params", {})),
                "name": os.environ.get("FIXTURE_NAME", "fixture"),
                "version": "1.0.0",
                "features": json.loads(os.environ.get("FIXTURE_FEATURES", "[]")),
                "capabilities": json.loads(os.environ.get(
                    "FIXTURE_CAPABILITIES",
                    '{"producer_messaging":false,"subscribe_events":[],"subscribe_bus":false,"publish_bus":false,"intercept":[]}',
                )),
                "tools": {"tools": declarations(), "prompt_sections": []},
            },
        })
        # Frames to send once connected, such as messages a newer or a
        # misbehaving plugin would send.
        for frame in json.loads(os.environ.get("FIXTURE_AFTER_INITIALIZE", "[]")):
            send(frame)
        if os.environ.get("FIXTURE_CRASH_AFTER_INITIALIZE") == "1":
            time.sleep(0.05)
            os._exit(17)
        if os.environ.get("FIXTURE_OVERSIZED_AFTER_INITIALIZE") == "1":
            sys.stdout.write("x" * (4 * 1024 * 1024 + 1))
            sys.stdout.flush()
        if os.environ.get("FIXTURE_INVALID_UTF8_AFTER_INITIALIZE") == "1":
            sys.stdout.buffer.write(b"\xff\n")
            sys.stdout.buffer.flush()
    elif method == "tool/prepare":
        params = message.get("params", {})
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"results": [prepare(call) for call in params.get("calls", [])]},
        })
    elif method == "tool/revalidate":
        send({"jsonrpc": "2.0", "id": request_id, "result": {}})
    elif method == "tool/execute":
        params = message.get("params", {})
        call = prepared.pop(params.get("prepared_id"), {}).get("call", {})
        call_file = os.environ.get("FIXTURE_TOOL_CALL_FILE")
        if call_file:
            with open(call_file, "w", encoding="utf-8") as output:
                json.dump({"execute": params, "call": call}, output)
        time.sleep(int(os.environ.get("FIXTURE_TOOL_DELAY_MS", "0")) / 1000)
        if os.environ.get("FIXTURE_CRASH_DURING_TOOL") == "1":
            os._exit(18)
        if os.environ.get("FIXTURE_TOOL_RPC_ERROR") == "1":
            send({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {
                    "code": -32000,
                    "message": "fixture tool RPC error",
                    "data": {"code": "fixture_failure"},
                },
            })
            continue
        progress = os.environ.get("FIXTURE_TOOL_PROGRESS")
        if progress:
            # Reports progress and waits for its answer before replying.
            progress = dict(json.loads(progress), tool_call_id=call.get("id"))
            send({
                "jsonrpc": "2.0",
                "id": "fixture-progress",
                "method": "tool/progress",
                "params": {"prepared_id": params.get("prepared_id"), "progress": progress},
            })
            for reply in sys.stdin:
                if json.loads(reply).get("id") == "fixture-progress":
                    break
        arguments = call.get("arguments", {})
        content = arguments.get("text", json.dumps(arguments, separators=(",", ":")))
        output = json.loads(os.environ.get(
            "FIXTURE_TOOL_OUTPUT", json.dumps({"kind": "single", "text": content})
        ))
        display = json.loads(os.environ.get("FIXTURE_TOOL_DISPLAY_BLOCKS", "null"))
        if display is None:
            display = [{
                "type": "text",
                "text": os.environ.get("FIXTURE_TOOL_DISPLAY", content[:60000]),
            }]
        outcome = {
            "status": "completed",
            "output": output,
            "title": call.get("name", "tool"),
            "display": display,
        }
        if os.environ.get("FIXTURE_TOOL_ERROR") == "1":
            outcome["failed"] = True
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"outcome": outcome, "media": []},
        })
    elif method in ("tool/release", "tool/cancel"):
        pass
    elif method == "plugin/event":
        context = message.get("params", {})
        event_file = os.environ.get("FIXTURE_EVENT_FILE")
        if event_file:
            with open(event_file, "a", encoding="utf-8") as output:
                output.write(json.dumps(message.get("params", {}), separators=(",", ":")) + "\n")
        if os.environ.get("FIXTURE_EMIT_FIRST_AFTER_SECOND") == "1":
            if pending_emit_context is None:
                pending_emit_context = context
            else:
                emit_from_context(pending_emit_context)
                pending_emit_context = None
        else:
            emit_from_context(context)
        time.sleep(int(os.environ.get("FIXTURE_EVENT_DELAY_MS", "0")) / 1000)
    elif method == "plugin/bus_event":
        bus_file = os.environ.get("FIXTURE_BUS_EVENT_FILE")
        if bus_file:
            with open(bus_file, "a", encoding="utf-8") as output:
                output.write(json.dumps(message.get("params", {}), separators=(",", ":")) + "\n")
    elif method == "plugin/emit_result":
        result_file = os.environ.get("FIXTURE_EMIT_RESULT_FILE")
        if result_file:
            with open(result_file, "a", encoding="utf-8") as output:
                output.write(json.dumps(message.get("params", {}), separators=(",", ":")) + "\n")
    elif method and method.startswith("plugin/intercept/"):
        intercept_file = os.environ.get("FIXTURE_INTERCEPT_FILE")
        if intercept_file:
            with open(intercept_file, "a", encoding="utf-8") as output:
                output.write(json.dumps({
                    "method": method,
                    "params": message.get("params", {}),
                }, separators=(",", ":")) + "\n")
        # The delay, crash, error, and configured result apply only to hook
        # calls whose params have these members, when set; other calls get
        # the hook's default answer.
        fail_when = json.loads(os.environ.get("FIXTURE_FAIL_WHEN", "null"))
        params = message.get("params", {})
        failing = fail_when is None or all(params.get(key) == value for key, value in fail_when.items())
        if failing:
            time.sleep(int(os.environ.get("FIXTURE_INTERCEPT_DELAY_MS", "0")) / 1000)
        release_file = os.environ.get("FIXTURE_INTERCEPT_RELEASE_FILE")
        gate_tool = os.environ.get("FIXTURE_INTERCEPT_GATE_TOOL")
        if release_file and (not gate_tool or message.get("params", {}).get("tool") == gate_tool):
            while not os.path.exists(release_file):
                time.sleep(0.01)
        if failing and os.environ.get("FIXTURE_CRASH_DURING_INTERCEPT") == "1":
            os._exit(19)
        intercept_error = os.environ.get("FIXTURE_INTERCEPT_ERROR")
        if failing and intercept_error:
            send({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {
                    "code": -32000,
                    "message": intercept_error,
                    "data": {"code": "fixture_failure"},
                },
            })
            continue
        result_env = {
            "plugin/intercept/tool_before_call": "FIXTURE_TOOL_BEFORE_RESULT",
            "plugin/intercept/tool_after_result": "FIXTURE_TOOL_AFTER_RESULT",
            "plugin/intercept/agent_before_start": "FIXTURE_AGENT_BEFORE_RESULT",
            "plugin/intercept/session_before_compact": "FIXTURE_COMPACT_BEFORE_RESULT",
            "plugin/intercept/user_before_input": "FIXTURE_USER_BEFORE_INPUT_RESULT",
            "plugin/intercept/model_before_request": "FIXTURE_MODEL_BEFORE_REQUEST_RESULT",
            "plugin/intercept/message_end": "FIXTURE_MESSAGE_END_RESULT",
            "plugin/intercept/model_before_select": "FIXTURE_MODEL_BEFORE_SELECT_RESULT",
            "plugin/intercept/session_before_fork": "FIXTURE_SESSION_BEFORE_FORK_RESULT",
            "plugin/intercept/session_before_revert": "FIXTURE_SESSION_BEFORE_REVERT_RESULT",
        }[method]
        defaults = {
            "plugin/intercept/tool_before_call": {"action": "allow"},
            "plugin/intercept/tool_after_result": {"action": "keep"},
            "plugin/intercept/agent_before_start": {},
            "plugin/intercept/session_before_compact": {},
            "plugin/intercept/user_before_input": {"action": "allow"},
            "plugin/intercept/model_before_request": {"action": "keep"},
            "plugin/intercept/message_end": {"action": "keep"},
            "plugin/intercept/model_before_select": {"action": "allow"},
            "plugin/intercept/session_before_fork": {"action": "allow"},
            "plugin/intercept/session_before_revert": {"action": "allow"},
        }
        result = defaults[method]
        if failing:
            result = json.loads(os.environ.get(result_env, json.dumps(result)))
        # Hands back what the hook was sent, as an unchanged replacement.
        if failing and os.environ.get("FIXTURE_ECHO_REPLACEMENT") == "1":
            if method == "plugin/intercept/model_before_request":
                result = {"action": "replace", "messages": params.get("messages")}
            elif method == "plugin/intercept/message_end":
                result = {"action": "replace", "content": params.get("content")}
        if method == "plugin/intercept/user_before_input":
            transform_from = os.environ.get("FIXTURE_USER_TRANSFORM_FROM")
            if transform_from is not None:
                if message.get("params", {}).get("content") == [{"type": "text", "text": transform_from}]:
                    result = {
                        "action": "transform",
                        "content": [{
                            "type": "text",
                            "text": os.environ.get("FIXTURE_USER_TRANSFORM_TO", ""),
                        }],
                    }
                else:
                    result = {"action": "allow"}
        send({"jsonrpc": "2.0", "id": request_id, "result": result})
    elif method == "plugin/shutdown":
        marker = os.environ.get("FIXTURE_SHUTDOWN_FILE")
        if marker:
            with open(marker, "w", encoding="utf-8") as output:
                output.write("shutdown")
        break
    elif request_id is not None:
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {
                "code": -32601,
                "message": "The other side does not know the method " + method + ".",
                "data": {"code": "method_not_found", "method": method},
            },
        })

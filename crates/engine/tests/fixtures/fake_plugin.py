import json
import os
import sys
import time


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


pid_file = os.environ.get("FIXTURE_PID_FILE")
if pid_file:
    with open(pid_file, "w", encoding="utf-8") as output:
        output.write(str(os.getpid()))
        output.flush()

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

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
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
                "protocol_version": os.environ.get("FIXTURE_PROTOCOL_VERSION", "0.0.4"),
                "name": os.environ.get("FIXTURE_NAME", "fixture"),
                "version": "1.0.0",
                "capabilities": json.loads(os.environ.get(
                    "FIXTURE_CAPABILITIES",
                    '{"tools":true,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":[]}',
                )),
                "tools": json.loads(os.environ.get("FIXTURE_TOOLS", "[]")),
            },
        })
        if os.environ.get("FIXTURE_CRASH_AFTER_INITIALIZE") == "1":
            time.sleep(0.05)
            os._exit(17)
        if os.environ.get("FIXTURE_OVERSIZED_AFTER_INITIALIZE") == "1":
            sys.stdout.write("x" * (4 * 1024 * 1024 + 1))
            sys.stdout.flush()
        if os.environ.get("FIXTURE_INVALID_UTF8_AFTER_INITIALIZE") == "1":
            sys.stdout.buffer.write(b"\xff\n")
            sys.stdout.buffer.flush()
    elif method == "plugin/tools/call":
        params = message.get("params", {})
        call_file = os.environ.get("FIXTURE_TOOL_CALL_FILE")
        if call_file:
            with open(call_file, "w", encoding="utf-8") as output:
                json.dump(params, output)
        time.sleep(int(os.environ.get("FIXTURE_TOOL_DELAY_MS", "0")) / 1000)
        if os.environ.get("FIXTURE_CRASH_DURING_TOOL") == "1":
            os._exit(18)
        if os.environ.get("FIXTURE_TOOL_RPC_ERROR") == "1":
            send({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32000, "message": "fixture tool RPC error"},
            })
            continue
        arguments = params.get("arguments", {})
        content = arguments.get("text", json.dumps(arguments, separators=(",", ":")))
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "content": content,
                "is_error": os.environ.get("FIXTURE_TOOL_ERROR") == "1",
            },
        })
    elif method == "plugin/ping":
        if os.environ.get("FIXTURE_INTERLEAVE_PING") == "1":
            send({"jsonrpc": "2.0", "method": "plugin/future_notice", "params": {}})
            send({"jsonrpc": "2.0", "id": 900, "method": "plugin/future_request", "params": {}})
        send({"jsonrpc": "2.0", "id": request_id, "result": {}})
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
        time.sleep(int(os.environ.get("FIXTURE_INTERCEPT_DELAY_MS", "0")) / 1000)
        if os.environ.get("FIXTURE_CRASH_DURING_INTERCEPT") == "1":
            os._exit(19)
        result_env = {
            "plugin/intercept/tool_before_call": "FIXTURE_TOOL_BEFORE_RESULT",
            "plugin/intercept/tool_after_result": "FIXTURE_TOOL_AFTER_RESULT",
            "plugin/intercept/agent_before_start": "FIXTURE_AGENT_BEFORE_RESULT",
            "plugin/intercept/session_before_compact": "FIXTURE_COMPACT_BEFORE_RESULT",
            "plugin/intercept/user_before_input": "FIXTURE_USER_BEFORE_INPUT_RESULT",
            "plugin/intercept/model_before_request": "FIXTURE_MODEL_BEFORE_REQUEST_RESULT",
            "plugin/intercept/provider_before_headers": "FIXTURE_PROVIDER_BEFORE_HEADERS_RESULT",
            "plugin/intercept/provider_before_request": "FIXTURE_PROVIDER_BEFORE_REQUEST_RESULT",
            "plugin/intercept/provider_after_response": "FIXTURE_PROVIDER_AFTER_RESPONSE_RESULT",
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
            "plugin/intercept/provider_before_headers": {"set": {}, "delete": []},
            "plugin/intercept/provider_before_request": {"action": "keep"},
            "plugin/intercept/provider_after_response": {},
            "plugin/intercept/message_end": {"action": "keep"},
            "plugin/intercept/model_before_select": {"action": "allow"},
            "plugin/intercept/session_before_fork": {"action": "allow"},
            "plugin/intercept/session_before_revert": {"action": "allow"},
        }
        result = json.loads(os.environ.get(result_env, json.dumps(defaults[method])))
        if method == "plugin/intercept/user_before_input":
            transform_from = os.environ.get("FIXTURE_USER_TRANSFORM_FROM")
            if transform_from is not None:
                if message.get("params", {}).get("text") == transform_from:
                    result = {
                        "action": "transform",
                        "new_text": os.environ.get("FIXTURE_USER_TRANSFORM_TO", ""),
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
    elif request_id == 900 and "error" in message:
        marker = os.environ.get("FIXTURE_REQUEST_REJECTED_FILE")
        if marker:
            with open(marker, "w", encoding="utf-8") as output:
                output.write(str(message["error"].get("code")))
    elif request_id is not None:
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": "unsupported fixture method"},
        })

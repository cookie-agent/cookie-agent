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
                "protocol_version": os.environ.get("FIXTURE_PROTOCOL_VERSION", "0.0.1"),
                "name": os.environ.get("FIXTURE_NAME", "fixture"),
                "version": "1.0.0",
                "capabilities": {"tools": True, "resources": False},
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
    elif method == "plugin/ping":
        if os.environ.get("FIXTURE_INTERLEAVE_PING") == "1":
            send({"jsonrpc": "2.0", "method": "plugin/future_notice", "params": {}})
            send({"jsonrpc": "2.0", "id": 900, "method": "plugin/future_request", "params": {}})
        send({"jsonrpc": "2.0", "id": request_id, "result": {}})
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

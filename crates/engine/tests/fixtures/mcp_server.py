import json
import os
import sys
import time


if os.environ.get("MCP_FIXTURE_PID_FILE"):
    with open(os.environ["MCP_FIXTURE_PID_FILE"], "w", encoding="utf-8") as pid_file:
        pid_file.write(str(os.getpid()))
        pid_file.flush()


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def response(request_id, result):
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


refreshed = False

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "server/discover":
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": "legacy fixture"},
        })
    elif method == "initialize":
        response(request_id, {
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {"listChanged": True}},
            "serverInfo": {"name": "cookie-test-mcp", "version": "1.0.0"},
        })
    elif method == "tools/list":
        time.sleep(int(os.environ.get("MCP_FIXTURE_LIST_DELAY_MS", "0")) / 1000)
        tools = [
                {
                    "name": "echo/text",
                    "description": "Echo supplied text.",
                    "inputSchema": {
                        "type": "object",
                        "$defs": {"Text": {"type": "string"}},
                        "properties": {"text": {"$ref": "#/$defs/Text"}},
                        "required": ["text"],
                    },
                },
                {
                    "name": "fail",
                    "description": "Return a tool error.",
                    "inputSchema": {"properties": {}},
                },
            ]
        if refreshed:
            tools.append({
                "name": "new tool",
                "description": "Added after a list-changed notification.",
                "inputSchema": {"type": "object", "properties": {}},
            })
        response(request_id, {"tools": tools})
    elif method == "tools/call":
        params = message["params"]
        if params["name"] == "fail":
            response(request_id, {
                "content": [{"type": "text", "text": "fixture failure"}],
                "isError": True,
            })
        else:
            response(request_id, {
                "content": [{"type": "text", "text": params["arguments"]["text"]}],
                "isError": False,
            })
            if params["arguments"]["text"] == "refresh":
                refreshed = True
                send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
    elif request_id is not None:
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": "unsupported fixture method"},
        })

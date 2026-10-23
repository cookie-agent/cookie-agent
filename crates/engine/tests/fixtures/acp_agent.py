"""A scripted ACP agent for cookie's tests: ACP v1 over stdio, one JSON-RPC
message per line.

Each prompt's last text block is a command:

  say <text>             answers <text>
  think <text>           a thought, then <text>
  echo                   answers every text block of the prompt joined by " | ",
                         as it does any prompt that is not a command
  work                   a thought, an edit tool call with a diff, a plan, usage
  stop <reason>          answers with stop reason <reason>
  permission <kind> <r>  asks permission for a call of ACP kind <kind> on <r>
                         (a path, a command, or a URL) and says the outcome
  wait                   says "waiting" and waits for session/cancel
  ask-wait <kind> <r>    asks permission and waits for session/cancel
  exit                   exits with status 3 after writing to stderr
  error                  fails the prompt with an authentication error
  title <text>           reports session title <text>
  mcp list               lists cookie's tools through the bridge
  mcp call <tool> <json> calls one of cookie's tools through the bridge

Environment:

  ACP_FIXTURE_LOG        a file every message received is appended to
  ACP_FIXTURE_RESUME     "1": offers session/resume
  ACP_FIXTURE_LOAD       "1": offers session/load (and replays on load)
  ACP_FIXTURE_LOST       "1": refuses to resume or load any session
  ACP_FIXTURE_MODES      "1": offers modes instead of a mode config option
  ACP_FIXTURE_NO_AUTO    "1": offers no `auto` mode
  ACP_FIXTURE_PID_FILE   a file the process ID is written to
  ACP_FIXTURE_EAGER_MCP  "1": connects cookie's MCP server and lists its tools
                         before answering session/new, as Claude Code does
"""

import json
import os
import socket
import sys
import threading

write_lock = threading.Lock()
state_lock = threading.Condition()
replies = {}
next_id = [1000]
sessions = {}
cancelled = {}


def log(message):
    path = os.environ.get("ACP_FIXTURE_LOG")
    if path:
        with open(path, "a", encoding="utf-8") as file:
            file.write(json.dumps(message) + "\n")


def send(message):
    message["jsonrpc"] = "2.0"
    with write_lock:
        sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def respond(request_id, result):
    send({"id": request_id, "result": result})


def fail(request_id, code, message):
    send({"id": request_id, "error": {"code": code, "message": message}})


def request(method, params):
    with state_lock:
        request_id = next_id[0]
        next_id[0] += 1
    send({"id": request_id, "method": method, "params": params})
    with state_lock:
        while request_id not in replies:
            state_lock.wait()
        return replies.pop(request_id)


def update(session_id, body):
    send({"method": "session/update", "params": {"sessionId": session_id, "update": body}})


def say(session_id, text):
    update(session_id, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})


def env(name):
    return os.environ.get(name) == "1"


def options(session):
    result = []
    if not env("ACP_FIXTURE_MODES"):
        modes = [{"value": "default", "name": "Default"}, {"value": "plan", "name": "Plan"}]
        if not env("ACP_FIXTURE_NO_AUTO"):
            modes.append({"value": "auto", "name": "Auto"})
        result.append({
            "id": "mode", "name": "Mode", "category": "mode", "type": "select",
            "currentValue": session["mode"], "options": modes,
        })
    result.append({
        "id": "model", "name": "Model", "category": "model", "type": "select",
        "currentValue": session["model"],
        "options": [{"value": "small", "name": "Small"}, {"value": "large", "name": "Large"}],
    })
    return result


def session_result(session_id, include_id):
    session = sessions[session_id]
    result = {"configOptions": options(session)}
    if include_id:
        result["sessionId"] = session_id
    if env("ACP_FIXTURE_MODES"):
        modes = [{"id": "default", "name": "Default"}, {"id": "plan", "name": "Plan"}]
        if not env("ACP_FIXTURE_NO_AUTO"):
            modes.append({"id": "auto", "name": "Auto"})
        result["modes"] = {"currentModeId": session["mode"], "availableModes": modes}
    return result


def announce_commands(session_id):
    update(session_id, {
        "sessionUpdate": "available_commands_update",
        "availableCommands": [
            {"name": "review", "description": "Review the changes", "input": {"hint": "what to review"}},
            {"name": "compact", "description": "Compact the conversation"},
        ],
    })


def bridge(session):
    servers = session.get("mcpServers") or []
    if not servers:
        return None
    args = servers[0].get("args", [])
    path = args[args.index("--socket") + 1]
    if path.startswith("\\\\.\\pipe\\"):
        handle = open(path, "r+b", buffering=0)
        return handle, handle
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.connect(path)
    reader = connection.makefile("rb")
    writer = connection.makefile("wb")
    return reader, writer


def mcp(session, method, params, request_id):
    reader, writer = session["bridge"]
    writer.write((json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + "\n").encode())
    writer.flush()
    while True:
        line = reader.readline()
        if not line:
            raise RuntimeError("the bridge closed")
        message = json.loads(line)
        if message.get("id") == request_id:
            return message


def mcp_open(session):
    if session.get("bridge") is None:
        session["bridge"] = bridge(session)
        if session["bridge"] is None:
            return False
        mcp(session, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "fixture", "version": "1"}}, 1)
        reader, writer = session["bridge"]
        writer.write((json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n").encode())
        writer.flush()
    return True


def permission(session_id, call_id, kind, resource):
    raw = {"execute": {"command": resource}, "fetch": {"url": resource}}.get(kind, {"path": resource})
    locations = [{"path": resource}] if kind in ("read", "edit", "delete", "move", "search") else []
    update(session_id, {
        "sessionUpdate": "tool_call", "toolCallId": call_id, "title": f"{kind} {resource}",
        "kind": kind, "status": "pending", "rawInput": raw, "locations": locations,
    })
    answer = request("session/request_permission", {
        "sessionId": session_id,
        "toolCall": {"toolCallId": call_id},
        "options": [
            {"optionId": "yes-always", "name": "Always allow", "kind": "allow_always"},
            {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
            {"optionId": "no", "name": "Reject", "kind": "reject_once"},
            {"optionId": "no-always", "name": "Always reject", "kind": "reject_always"},
        ],
    })
    outcome = answer.get("result", {}).get("outcome", {})
    if outcome.get("outcome") == "selected":
        return outcome.get("optionId")
    return "cancelled"


def prompt(request_id, params):
    session_id = params["sessionId"]
    session = sessions.setdefault(session_id, {"mode": "default", "model": "small", "turn": 0})
    session["turn"] = session.get("turn", 0) + 1
    texts = [block.get("text", "") for block in params.get("prompt", []) if block.get("type") == "text"]
    kinds = [block.get("type") for block in params.get("prompt", [])]
    command = texts[-1] if texts else ""
    word, _, rest = command.partition(" ")
    with state_lock:
        cancelled[session_id] = False
    if word == "say":
        say(session_id, rest)
    elif word == "think":
        update(session_id, {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "pondering"}})
        say(session_id, rest)
    elif word == "echo" or not word:
        say(session_id, " | ".join(texts + [k for k in kinds if k != "text"]))
    elif word == "work":
        update(session_id, {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "planning"}})
        say(session_id, "Editing.")
        update(session_id, {"sessionUpdate": "plan", "entries": [
            {"content": "Read the file", "priority": "high", "status": "completed"},
            {"content": "Edit the file", "priority": "medium", "status": "in_progress"},
        ]})
        update(session_id, {
            "sessionUpdate": "tool_call", "toolCallId": "call-edit", "title": "Edit notes.txt",
            "kind": "edit", "status": "in_progress", "locations": [{"path": "/work/notes.txt", "line": 2}],
            "rawInput": {"path": "/work/notes.txt"},
        })
        update(session_id, {
            "sessionUpdate": "tool_call_update", "toolCallId": "call-edit", "status": "completed",
            "content": [{"type": "diff", "path": "/work/notes.txt", "oldText": "one\ntwo\n", "newText": "one\n2\n"}],
            "rawOutput": {"ok": True},
        })
        update(session_id, {"sessionUpdate": "usage_update", "used": 1234, "size": 200000, "cost": {"amount": 0.5, "currency": "USD"}})
        say(session_id, "Done.")
    elif word == "stop":
        say(session_id, "stopping")
        respond(request_id, {"stopReason": rest})
        return
    elif word == "permission":
        kind, _, resource = rest.partition(" ")
        outcome = permission(session_id, f"call-{session['turn']}", kind, resource)
        update(session_id, {
            "sessionUpdate": "tool_call_update", "toolCallId": f"call-{session['turn']}",
            "status": "completed" if outcome == "yes" else "failed",
        })
        say(session_id, f"permission: {outcome}")
    elif word in ("wait", "ask-wait"):
        say(session_id, "waiting")
        if word == "ask-wait":
            kind, _, resource = rest.partition(" ")
            outcome = permission(session_id, f"call-{session['turn']}", kind, resource)
            log({"fixture": "permission outcome", "outcome": outcome})
        with state_lock:
            while not cancelled[session_id]:
                state_lock.wait()
        respond(request_id, {"stopReason": "cancelled"})
        return
    elif word == "exit":
        sys.stderr.write("fixture crashed on purpose\n")
        sys.stderr.flush()
        os._exit(3)
    elif word == "error":
        fail(request_id, -32000, "Authentication required")
        return
    elif word == "title":
        update(session_id, {"sessionUpdate": "session_info_update", "title": rest})
        say(session_id, "titled")
    elif word == "mcp":
        action, _, more = rest.partition(" ")
        if not mcp_open(session):
            say(session_id, "no bridge")
        elif action == "eager":
            say(session_id, f"eager: {session.get('eager_tools')}")
        elif action == "list":
            reply = mcp(session, "tools/list", {}, 2)
            names = sorted(tool["name"] for tool in reply["result"]["tools"])
            say(session_id, "tools: " + ",".join(names))
        else:
            tool, _, arguments = more.partition(" ")
            reply = mcp(session, "tools/call", {"name": tool, "arguments": json.loads(arguments or "{}")}, 3)
            result = reply.get("result", {})
            text = " ".join(item.get("text", "") for item in result.get("content", []))
            say(session_id, f"result: {text} error: {result.get('isError')}")
    else:
        say(session_id, " | ".join(texts))
    respond(request_id, {"stopReason": "end_turn", "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}})


def handle(message):
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if method is None:
        with state_lock:
            replies[request_id] = message
            state_lock.notify_all()
        return
    if method == "initialize":
        capabilities = {
            "loadSession": env("ACP_FIXTURE_LOAD"),
            "promptCapabilities": {"image": True, "audio": False, "embeddedContext": False},
            "sessionCapabilities": {"close": {}},
        }
        if env("ACP_FIXTURE_RESUME"):
            capabilities["sessionCapabilities"]["resume"] = {}
        respond(request_id, {
            "protocolVersion": 1,
            "agentCapabilities": capabilities,
            "authMethods": [],
            "agentInfo": {"name": "fixture-agent", "title": "Fixture Agent", "version": "1.0.0"},
        })
    elif method == "session/new":
        session_id = f"sess-{os.getpid()}-{len(sessions) + 1}"
        sessions[session_id] = {"mode": "default", "model": "small", "mcpServers": params.get("mcpServers"), "cwd": params.get("cwd")}
        if env("ACP_FIXTURE_EAGER_MCP") and mcp_open(sessions[session_id]):
            reply = mcp(sessions[session_id], "tools/list", {}, 2)
            sessions[session_id]["eager_tools"] = "error" not in reply
        respond(request_id, session_result(session_id, True))
        announce_commands(session_id)
    elif method in ("session/resume", "session/load"):
        session_id = params["sessionId"]
        if env("ACP_FIXTURE_LOST"):
            fail(request_id, -32002, "Resource not found: no such session")
            return
        sessions.setdefault(session_id, {"mode": "default", "model": "small"})
        sessions[session_id]["mcpServers"] = params.get("mcpServers")
        sessions[session_id]["bridge"] = None
        if method == "session/load":
            update(session_id, {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "REPLAYED QUESTION"}})
            say(session_id, "REPLAYED ANSWER")
        respond(request_id, session_result(session_id, False))
    elif method == "session/set_config_option":
        session = sessions[params["sessionId"]]
        session[params["configId"]] = params["value"]
        respond(request_id, {"configOptions": options(session)})
    elif method == "session/set_mode":
        sessions[params["sessionId"]]["mode"] = params["modeId"]
        respond(request_id, {})
    elif method == "session/prompt":
        threading.Thread(target=prompt, args=(request_id, params), daemon=True).start()
    elif method == "session/cancel":
        with state_lock:
            cancelled[params["sessionId"]] = True
            state_lock.notify_all()
    elif method == "session/close":
        sessions.pop(params["sessionId"], None)
        respond(request_id, {})
    elif request_id is not None:
        fail(request_id, -32601, f"unsupported fixture method {method}")


def main():
    pid_file = os.environ.get("ACP_FIXTURE_PID_FILE")
    if pid_file:
        with open(pid_file + ".tmp", "w", encoding="utf-8") as file:
            file.write(str(os.getpid()))
        os.replace(pid_file + ".tmp", pid_file)
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        message = json.loads(line)
        log(message)
        handle(message)


main()

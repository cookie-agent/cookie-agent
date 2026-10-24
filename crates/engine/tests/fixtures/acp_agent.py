"""A scripted ACP agent for cookie's tests: ACP v1 over stdio, one JSON-RPC
message per line.

Each prompt's last text block is a command:

  say <text>             answers <text>
  think <text>           a thought, then <text>
  echo                   answers every text block of the prompt joined by " | ",
                         as it does any prompt that is not a command
  work                   a thought, an edit tool call with a diff, a plan, usage
                         (the same usage_update twice)
  stop <reason>          answers with stop reason <reason> and usage (40 input
                         and 2 output tokens)
  permission <kind> <r>  asks permission for a call of ACP kind <kind> on <r>
                         (a path, a command, or a URL) and says the outcome
  permission-tool <kind> <title>
                         asks permission for a call of ACP kind <kind> titled
                         <title>, with no locations or raw input
  permission-mcp <server> <tool>
                         asks permission as Codex does for an MCP tool: kind
                         other, titled "Tool: <server>/<tool>", raw input
                         {server, tool, arguments}
  wait                   says "waiting", reports usage (555 tokens used), and
                         waits for session/cancel
  ask-wait <kind> <r>    asks permission and waits for session/cancel
  exit                   exits with status 3 after writing to stderr
  error                  fails the prompt with an authentication error
  title <text>           reports session title <text>
  terminal               two execute calls with terminal output when the client
                         advertised `_meta.terminal_output` (else the output as
                         text content): one as Codex sends it (chunks, then
                         exit 3, failed), one as Claude Code does (the whole
                         output once, then exit 0, completed)
  picture                answers text, an image, and more text
  mode-switch <mode>     switches its own mode to <mode> (current_mode_update),
                         as Claude Code does when it leaves plan mode
  late-input             a tool call whose title, kind, and raw input arrive in a
                         later update (as Claude Code's adapter sends them), with
                         text, image, and resource-link content and raw output
  notices                a warning notice, then a compaction with a streamed
                         summary
  steer-wait [<s>]       says "waiting", then waits (up to <s>, else 10 s) for a message
                         steered into the turn and answers "steered: <texts>",
                         or "no steer"; session/cancel ends it as cancelled
  recall                 answers "remembers: " and the commands of the session's
                         earlier prompts, joined by ", " (a forked session
                         remembers its source's up to the fork point)
  elicit <kind>          asks the user a question and says "elicitation: <action>
                         <content as sorted JSON>": kind `form` (an AskUserQuestion
                         form tied to a tool call), `url` (a URL question), or
                         `url-complete` (a URL question the agent reports complete
                         after a second); `elicit-wait <kind>` then waits for
                         session/cancel
  mcp list               lists cookie's tools through the bridge
  mcp call <tool> <json> calls one of cookie's tools through the bridge

Its config options are `mode` (unless ACP_FIXTURE_MODES), `model`, and `fast`, a
boolean when the client takes boolean options (else an on/off select).

Environment:

  ACP_FIXTURE_LOG        a file every message received is appended to
  ACP_FIXTURE_RESUME     "1": offers session/resume
  ACP_FIXTURE_LOAD       "1": offers session/load (and replays on load)
  ACP_FIXTURE_LOST       "1": refuses to resume or load any session
  ACP_FIXTURE_MODES      "1": offers modes instead of a mode config option
  ACP_FIXTURE_NO_AUTO    "1": offers no `auto` mode
  ACP_FIXTURE_PID_FILE   a file the process ID is written to
  ACP_FIXTURE_CHILD_PID_FILE
                         starts a child process that sleeps, and writes its
                         process ID to this file
  ACP_FIXTURE_EAGER_MCP  "1": connects cookie's MCP server and lists its tools
                         before answering session/new, as Claude Code does
  ACP_FIXTURE_FORK       "1": offers session/fork, which copies the source's
                         history, up to and including the agent message the
                         JetBrains AIR fork point (`_meta.jetbrains.air.fork`)
                         names when there is one (an unknown message is
                         invalid params, -32602)
  ACP_FIXTURE_FORK_WHOLE "1" with ACP_FIXTURE_FORK: session/fork ignores the
                         fork point and copies the whole history
  ACP_FIXTURE_FORK_DELAY seconds session/fork takes to answer
  ACP_FIXTURE_FORK_FAIL  "1": session/fork fails, except cookie's probe (whose
                         fork point is `cookie-fork-probe-...`)
  ACP_FIXTURE_DELETE     "1": offers session/delete
  ACP_FIXTURE_NO_COMPACT "1": announces no `compact` command
  ACP_FIXTURE_STEERING   "1": advertises the steering extension
                         (`_meta.steering.supported`) and serves
                         `_session/steering` as Claude Code's adapter does
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
running = {}
steered = {}
# What the client advertised in `initialize`.
client_capabilities = {}
session_count = [0]
message_count = [0]


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
    with state_lock:
        message_count[0] += 1
        message_id = f"msg-{message_count[0]}"
    session = sessions.get(session_id)
    if session is not None:
        session.setdefault("history", []).append({"role": "agent", "text": text, "messageId": message_id})
    update(session_id, {
        "sessionUpdate": "agent_message_chunk", "messageId": message_id,
        "content": {"type": "text", "text": text},
    })


def new_session_id():
    with state_lock:
        session_count[0] += 1
        return f"sess-{os.getpid()}-{session_count[0]}"


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
    # As Claude Code's fast mode: a boolean for a client that takes them,
    # else an on/off select.
    fast = {"id": "fast", "name": "Fast mode", "category": "model_config"}
    if client_capabilities.get("session", {}).get("configOptions", {}).get("boolean") is not None:
        fast.update({"type": "boolean", "currentValue": session.get("fast", False)})
    else:
        fast.update({
            "type": "select", "currentValue": "on" if session.get("fast") else "off",
            "options": [{"value": "on", "name": "On"}, {"value": "off", "name": "Off"}],
        })
    result.append(fast)
    result.append({
        "id": "reasoning_effort", "name": "Reasoning", "category": "thought_level", "type": "select",
        "currentValue": session.get("reasoning_effort", "low"),
        "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}],
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
    commands = [{"name": "review", "description": "Review the changes", "input": {"hint": "what to review"}}]
    if not env("ACP_FIXTURE_NO_COMPACT"):
        commands.append({"name": "compact", "description": "Compact the conversation"})
    update(session_id, {"sessionUpdate": "available_commands_update", "availableCommands": commands})


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


def permission(session_id, call_id, kind, resource, title=None, raw=None):
    if title is None:
        raw = {"execute": {"command": resource}, "fetch": {"url": resource}}.get(kind, {"path": resource})
        title = f"{kind} {resource}"
    locations = [{"path": resource}] if resource and kind in ("read", "edit", "delete", "move", "search") else []
    call = {
        "sessionUpdate": "tool_call", "toolCallId": call_id, "title": title,
        "kind": kind, "status": "pending", "locations": locations,
    }
    if raw is not None:
        call["rawInput"] = raw
    update(session_id, call)
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


def terminal_calls(session_id):
    streams = client_capabilities.get("_meta", {}).get("terminal_output") is True
    # As Codex: the terminal named from the start, output in chunks.
    meta = {"terminal_info": {"terminal_id": "term-codex", "cwd": "/work"}} if streams else None
    call = {
        "sessionUpdate": "tool_call", "toolCallId": "call-codex", "title": "make test",
        "kind": "execute", "status": "in_progress", "rawInput": {"command": "make test"},
        "content": [{"type": "terminal", "terminalId": "term-codex"}] if streams else [],
    }
    if meta:
        call["_meta"] = meta
    update(session_id, call)
    if streams:
        for chunk in ["building\n", "\u001b[31mFAILED\u001b[0m 1 test\n"]:
            update(session_id, {
                "sessionUpdate": "tool_call_update", "toolCallId": "call-codex",
                "_meta": {"terminal_output": {"terminal_id": "term-codex", "data": chunk}},
            })
        update(session_id, {
            "sessionUpdate": "tool_call_update", "toolCallId": "call-codex", "status": "failed",
            "_meta": {"terminal_exit": {"terminal_id": "term-codex", "exit_code": 3, "signal": None}},
        })
    else:
        update(session_id, {
            "sessionUpdate": "tool_call_update", "toolCallId": "call-codex", "status": "failed",
            "content": [{"type": "content", "content": {"type": "text", "text": "building\nFAILED 1 test\n"}}],
        })
    # As Claude Code: the description first, the whole output once with the
    # result, then the exit with the status.
    call = {
        "sessionUpdate": "tool_call", "toolCallId": "call-claude", "title": "ls",
        "kind": "execute", "status": "pending", "rawInput": {"command": "ls"},
        "content": [{"type": "content", "content": {"type": "text", "text": "List the files"}}],
    }
    if streams:
        call["_meta"] = {"terminal_info": {"terminal_id": "call-claude"}}
    update(session_id, call)
    if streams:
        update(session_id, {
            "sessionUpdate": "tool_call_update", "toolCallId": "call-claude",
            "_meta": {"terminal_output": {"terminal_id": "call-claude", "data": "notes.txt\n"}},
        })
    update(session_id, {
        "sessionUpdate": "tool_call_update", "toolCallId": "call-claude", "status": "completed",
        "content": [{"type": "terminal", "terminalId": "call-claude"}] if streams
        else [{"type": "content", "content": {"type": "text", "text": "notes.txt"}}],
        "_meta": {"terminal_info": {"terminal_id": "call-claude"},
                  "terminal_exit": {"terminal_id": "call-claude", "exit_code": 0, "signal": None}} if streams else {},
    })

FORM_SCHEMA = {
    "type": "object",
    "properties": {
        "question_0": {"type": "string", "title": "Plan", "oneOf": [
            {"const": "safe", "title": "Safe", "description": "Small steps"},
            {"const": "fast", "title": "Fast"},
        ]},
        "question_0_custom": {"type": "string", "title": "Other", "description": "Your own answer"},
        "tags": {"type": "array", "title": "Tags", "items": {"anyOf": [
            {"const": "x", "title": "X"}, {"const": "y", "title": "Y"},
        ]}},
        "count": {"type": "integer", "minimum": 1, "default": 2},
        "ok": {"type": "boolean"},
    },
    "required": ["question_0"],
}


def elicit(session_id, session, kind):
    if kind == "form":
        call_id = f"call-ask-{session['turn']}"
        update(session_id, {
            "sessionUpdate": "tool_call", "toolCallId": call_id, "title": "AskUserQuestion",
            "kind": "other", "status": "pending",
        })
        params = {"sessionId": session_id, "toolCallId": call_id, "mode": "form",
                  "message": "How should I go?", "requestedSchema": FORM_SCHEMA}
    else:
        elicitation_id = f"url-{session['turn']}"
        params = {"sessionId": session_id, "mode": "url", "elicitationId": elicitation_id,
                  "url": "https://example.com/connect?id=" + elicitation_id,
                  "message": "Sign in to Example."}
        if kind == "url-complete":
            def complete():
                import time
                time.sleep(1)
                send({"method": "elicitation/complete", "params": {"elicitationId": "unknown"}})
                send({"method": "elicitation/complete", "params": {"elicitationId": elicitation_id}})
            threading.Thread(target=complete, daemon=True).start()
    answer = request("elicitation/create", params)
    if "error" in answer:
        return f"error {answer['error'].get('code')}"
    result = answer.get("result", {})
    content = json.dumps(result.get("content"), sort_keys=True, separators=(",", ":"))
    return f"{result.get('action')} {content}"


def prompt(request_id, params):
    session_id = params["sessionId"]
    session = sessions.setdefault(session_id, {"mode": "default", "model": "small", "turn": 0})
    session["turn"] = session.get("turn", 0) + 1
    texts = [block.get("text", "") for block in params.get("prompt", []) if block.get("type") == "text"]
    kinds = [block.get("type") for block in params.get("prompt", [])]
    command = texts[-1] if texts else ""
    word, _, rest = command.partition(" ")
    session.setdefault("history", []).append({"role": "user", "text": command})
    with state_lock:
        cancelled[session_id] = False
        running[session_id] = True
        steered[session_id] = []
    try:
        run_command(request_id, session_id, session, word, rest, texts, kinds)
    finally:
        with state_lock:
            running[session_id] = False


def run_command(request_id, session_id, session, word, rest, texts, kinds):
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
        # The same context use again: the session records it once.
        update(session_id, {"sessionUpdate": "usage_update", "used": 1234, "size": 200000, "cost": {"amount": 0.5, "currency": "USD"}})
        say(session_id, "Done.")
    elif word == "late-input":
        update(session_id, {
            "sessionUpdate": "tool_call", "toolCallId": "call-late", "title": "Tool",
            "kind": "other", "status": "pending", "_meta": {"claudeCode": {"toolName": "Read"}},
        })
        update(session_id, {
            "sessionUpdate": "tool_call_update", "toolCallId": "call-late", "title": "Read notes.txt",
            "kind": "read", "rawInput": {"file_path": "/work/notes.txt"},
            "_meta": {"claudeCode": {"toolName": "Read"}},
        })
        update(session_id, {
            "sessionUpdate": "tool_call_update", "toolCallId": "call-late", "status": "completed",
            "content": [
                {"type": "content", "content": {"type": "text", "text": "```\none\n```"}},
                {"type": "content", "content": {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="}},
                {"type": "content", "content": {"type": "resource_link", "uri": "file:///work/notes.txt", "name": "notes.txt"}},
            ],
            "rawOutput": "one",
        })
        say(session_id, "read")
    elif word == "notices":
        update(session_id, {"sessionUpdate": "notice", "severity": "warning", "title": "Rate limited", "description": "Retrying in 1 s."})
        update(session_id, {"sessionUpdate": "compaction_update", "compactionId": "c1", "status": "in_progress"})
        update(session_id, {"sessionUpdate": "compaction_summary_chunk", "compactionId": "c1", "content": {"type": "text", "text": "We read notes.txt."}})
        update(session_id, {"sessionUpdate": "compaction_update", "compactionId": "c1", "status": "completed"})
        say(session_id, "noticed")
    elif word == "steer-wait":
        say(session_id, "waiting")
        with state_lock:
            state_lock.wait_for(
                lambda: steered[session_id] or cancelled[session_id],
                timeout=float(rest or "10"),
            )
            got = list(steered[session_id])
            stopped = cancelled[session_id]
        if stopped:
            respond(request_id, {"stopReason": "cancelled"})
            return
        say(session_id, "steered: " + " | ".join(got) if got else "no steer")
    elif word == "stop":
        say(session_id, "stopping")
        respond(request_id, {"stopReason": rest, "usage": {"inputTokens": 40, "outputTokens": 2, "totalTokens": 42}})
        return
    elif word == "permission":
        kind, _, resource = rest.partition(" ")
        outcome = permission(session_id, f"call-{session['turn']}", kind, resource)
        update(session_id, {
            "sessionUpdate": "tool_call_update", "toolCallId": f"call-{session['turn']}",
            "status": "completed" if outcome == "yes" else "failed",
        })
        say(session_id, f"permission: {outcome}")
    elif word in ("permission-tool", "permission-mcp"):
        first, _, second = rest.partition(" ")
        call_id = f"call-{session['turn']}"
        if word == "permission-tool":
            outcome = permission(session_id, call_id, first, "", title=second)
        else:
            outcome = permission(
                session_id, call_id, "other", "", title=f"Tool: {first}/{second}",
                raw={"server": first, "tool": second, "arguments": {}},
            )
        update(session_id, {
            "sessionUpdate": "tool_call_update", "toolCallId": call_id,
            "status": "completed" if outcome == "yes" else "failed",
        })
        say(session_id, f"permission: {outcome}")
    elif word in ("wait", "ask-wait"):
        say(session_id, "waiting")
        if word == "wait":
            update(session_id, {"sessionUpdate": "usage_update", "used": 555, "size": 200000})
        if word == "ask-wait":
            kind, _, resource = rest.partition(" ")
            outcome = permission(session_id, f"call-{session['turn']}", kind, resource)
            log({"fixture": "permission outcome", "outcome": outcome})
        with state_lock:
            while not cancelled[session_id]:
                state_lock.wait()
        respond(request_id, {"stopReason": "cancelled"})
        return
    elif word == "elicit":
        say(session_id, "elicitation: " + elicit(session_id, session, rest))
    elif word == "elicit-wait":
        outcome = elicit(session_id, session, rest)
        log({"fixture": "elicitation outcome", "outcome": outcome})
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
    elif word == "terminal":
        terminal_calls(session_id)
        say(session_id, "ran")
    elif word == "picture":
        say(session_id, "Here it is:")
        update(session_id, {"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="}})
        say(session_id, "A picture.")
    elif word == "mode-switch":
        session["mode"] = rest
        update(session_id, {"sessionUpdate": "current_mode_update", "currentModeId": rest})
        say(session_id, f"mode: {rest}")
    elif word == "recall":
        earlier = [entry["text"] for entry in session.get("history", [])[:-1] if entry["role"] == "user"]
        say(session_id, "remembers: " + ", ".join(earlier))
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


def fork(request_id, params):
    delay = os.environ.get("ACP_FIXTURE_FORK_DELAY")
    if delay:
        import time
        time.sleep(float(delay))
    source = sessions.get(params["sessionId"])
    if source is None:
        fail(request_id, -32002, "Resource not found: no such session")
        return
    history = list(source.get("history", []))
    point = (((params.get("_meta") or {}).get("jetbrains") or {}).get("air") or {}).get("fork")
    if env("ACP_FIXTURE_FORK_FAIL") and not str((point or {}).get("messageId", "")).startswith("cookie-fork-probe-"):
        fail(request_id, -32603, "Internal error: the fork failed")
        return
    if point and point.get("version") == 1 and not env("ACP_FIXTURE_FORK_WHOLE"):
        found = [index for index, entry in enumerate(history) if entry.get("messageId") == point.get("messageId")]
        if not found:
            fail(request_id, -32602, f"Fork point message {point.get('messageId')} was not found")
            return
        history = history[:found[-1] + 1]
    session_id = new_session_id()
    sessions[session_id] = {
        "mode": source.get("mode", "default"), "model": source.get("model", "small"),
        "mcpServers": params.get("mcpServers"), "cwd": params.get("cwd"), "history": history,
    }
    respond(request_id, session_result(session_id, True))


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
        client_capabilities.update(params.get("clientCapabilities") or {})
        capabilities = {
            "loadSession": env("ACP_FIXTURE_LOAD"),
            "promptCapabilities": {"image": True, "audio": False, "embeddedContext": False},
            "sessionCapabilities": {"close": {}},
        }
        if env("ACP_FIXTURE_RESUME"):
            capabilities["sessionCapabilities"]["resume"] = {}
        if env("ACP_FIXTURE_FORK"):
            capabilities["sessionCapabilities"]["fork"] = {}
        if env("ACP_FIXTURE_DELETE"):
            capabilities["sessionCapabilities"]["delete"] = {}
        result = {
            "protocolVersion": 1,
            "agentCapabilities": capabilities,
            "authMethods": [],
            "agentInfo": {"name": "fixture-agent", "title": "Fixture Agent", "version": "1.0.0"},
        }
        if env("ACP_FIXTURE_STEERING"):
            result["_meta"] = {"steering": {"supported": True}}
        respond(request_id, result)
    elif method == "session/new":
        session_id = new_session_id()
        sessions[session_id] = {"mode": "default", "model": "small", "mcpServers": params.get("mcpServers"), "cwd": params.get("cwd"), "history": []}
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
        value = params["value"]
        if params["configId"] == "fast":
            if params.get("type") == "boolean" and isinstance(value, bool):
                pass
            elif value in ("on", "off"):
                value = value == "on"
            else:
                fail(request_id, -32602, f"invalid value for fast: {value!r}")
                return
        session[params["configId"]] = value
        respond(request_id, {"configOptions": options(session)})
    elif method == "session/set_mode":
        sessions[params["sessionId"]]["mode"] = params["modeId"]
        respond(request_id, {})
    elif method == "session/prompt":
        threading.Thread(target=prompt, args=(request_id, params), daemon=True).start()
    elif method == "_session/steering" and env("ACP_FIXTURE_STEERING"):
        session_id = params["sessionId"]
        texts = [block.get("text", "") for block in params.get("prompt", []) if block.get("type") == "text"]
        with state_lock:
            if running.get(session_id):
                steered[session_id].extend(texts)
                state_lock.notify_all()
                outcome = {"outcome": "injected"}
            elif (params.get("_meta") or {}).get("steering", {}).get("idleBehavior") == "promptRequired":
                outcome = {"outcome": "promptRequired", "reason": "noRunningTurn"}
            else:
                outcome = {"outcome": "startedNewTurn"}
        respond(request_id, outcome)
    elif method == "session/cancel":
        with state_lock:
            cancelled[params["sessionId"]] = True
            state_lock.notify_all()
    elif method == "session/close":
        session = sessions.get(params["sessionId"])
        if session is not None:
            session["closed"] = True
        respond(request_id, {})
    elif method == "session/delete" and env("ACP_FIXTURE_DELETE"):
        sessions.pop(params["sessionId"], None)
        respond(request_id, {})
    elif method == "session/fork" and env("ACP_FIXTURE_FORK"):
        threading.Thread(target=fork, args=(request_id, params), daemon=True).start()
    elif request_id is not None:
        fail(request_id, -32601, f"unsupported fixture method {method}")


def main():
    child_pid_file = os.environ.get("ACP_FIXTURE_CHILD_PID_FILE")
    if child_pid_file:
        import subprocess
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(600)"])
        with open(child_pid_file + ".tmp", "w", encoding="utf-8") as file:
            file.write(str(child.pid))
        os.replace(child_pid_file + ".tmp", child_pid_file)
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

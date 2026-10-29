"""A scripted ACP agent for cookie's tests: ACP v2 over stdio, one JSON-RPC
message per line.

A prompt is answered once it is inserted (its `user_message` echo, then the
`messageId`); its work is reported between `state_update` `running` and
`idle`, which carries the stop reason and usage. A prompt sent while a prompt's
work runs is a steer: it is inserted into that work.

Each prompt's last text block is a command:

  say <text>             answers <text>
  think <text>           a thought, then <text>
  think-blank <text>     a thought that opens with blank lines, as Codex's
                         summaries do, then <text>
  two-messages           answers "first" and then "second", two messages
  echo                   answers every text block of the prompt joined by " | ",
                         then the type of every other block, as it does any
                         prompt that is not a command
  work                   a thought, an edit tool call with a diff, a plan, usage
                         (the same usage_update twice)
  stop <reason>          ends its work with stop reason <reason> and usage (40
                         input and 2 output tokens)
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
  error                  ends its work with stop reason `error` and an
                         authentication error
  title <text>           reports session title <text>
  title-first            reports the first text of its first prompt as the
                         session title, as Codex names a thread
  terminal               two execute calls with agent terminals: one with its
                         output in chunks, then exit 3, failed; one with the
                         whole output as a snapshot with exit 0, completed
  picture                answers text, an image, and more text
  mode-switch <mode>     switches its own mode option to <mode>
                         (config_option_update), as Claude Code does when it
                         leaves plan mode
  late-input             a tool call whose title, kind, and raw input arrive in a
                         later update (as Claude Code's adapter sends them), with
                         text, image, and resource-link content and raw output
  notices                a warning notice, then a compaction with a streamed
                         summary
  steer-wait [<s>]       says "waiting", then waits (up to <s>, else 10 s) for a message
                         steered into its work and answers "steered: <texts>",
                         or "no steer"; session/cancel ends it as cancelled
  steer-late             says "waiting", waits (up to 10 s) for a steer, says
                         "before", and only then takes the steer in, as Codex
                         does at its next step, and answers "steered: <texts>"
                         (with ACP_FIXTURE_LATE_STEERS)
  decline-wait           says "waiting", then waits (up to 30 s) for a steer it
                         declines (ACP_FIXTURE_DECLINE_STEERS), and answers "no
                         steer" once it has; a steer taken or a cancel ends it as
                         steer-wait does
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
  subagent <title> <task>
                         announces a subagent session (subagent_update), gives it
                         <task> (session_message), and the subagent works on its
                         own stream (running, a read call, "subagent says:
                         <task>", idle) while this session answers "subagent
                         done"
  subagent-wait <title>  the same, but the subagent says "waiting" and waits (up to
                         10 s) for session/cancel, while this session answers
                         "subagent started" at once
  subagent-ask <kind> <r>
                         the same, titled "asker", but the subagent asks
                         permission for a call of ACP kind <kind> on <r> and
                         says "subagent asked: <outcome>"
  background <s>         runs `sleep <s>` in the background: an execute call whose
                         terminal stays open after the call completes, and exits
                         <s> seconds later, between runs
  background-wake <s> <w>
                         the same, and <w> seconds after the command exits starts
                         work of its own that says "woke", as Claude Code follows
                         up on its backgrounded commands
  wake-later <s> <text>  answers "later", then <s> seconds after its work ended
                         starts work of its own (running, <text>, idle), as
                         Claude Code does when a background command ends
  wake-steer <s>         the same, but its own work says "waiting" and waits (up to
                         10 s) for a message steered into it, then answers
                         "steered: <texts>"; session/cancel ends it as cancelled
  wake-ask <s> <kind> <r>
                         the same, but its own work asks permission for a call
                         of ACP kind <kind> on <r> and says the outcome
  mcp echo <tool> <json> the same, reported as its own tool call first, as
                         Claude Code reports its MCP calls (`mcp__cookie__<tool>`)
  mcp miss <tool>        reports a call of one of cookie's tools that fails
                         before it reaches the bridge

Its config options are `mode`, `model`, `fast` (a boolean), and
`reasoning_effort`.

Environment:

  ACP_FIXTURE_LOG        a file every message received is appended to
  ACP_FIXTURE_V1         "1": answers `initialize` with protocol version 1
  ACP_FIXTURE_LOST       "1": refuses to resume any session
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
  ACP_FIXTURE_SYSTEM_PROMPT
                         "1": offers a system prompt (`systemPrompt`)
  ACP_FIXTURE_NO_COMPACT "1": announces no `compact` command
  ACP_FIXTURE_IMAGE_ONLY "1": takes images but no audio or embedded context in a
                         prompt, and no additional directories (by default it
                         advertises all of them)
  ACP_FIXTURE_LATE_STEERS
                         "1": answers a prompt sent while its work runs only
                         when steer-late takes it in
  ACP_FIXTURE_DECLINE_STEERS
                         "1": refuses a prompt sent while its work runs
"""

import base64
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
# Whether the work running in each session declined a steer.
declined = {}
# Steers not yet taken in, with ACP_FIXTURE_LATE_STEERS: (request ID, params).
late = {}
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


def next_message_id(prefix="msg"):
    with state_lock:
        message_count[0] += 1
        return f"{prefix}-{message_count[0]}"


def say(session_id, text):
    message_id = next_message_id()
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
    modes = [{"value": "default", "name": "Default"}, {"value": "plan", "name": "Plan"}]
    if not env("ACP_FIXTURE_NO_AUTO"):
        modes.append({"value": "auto", "name": "Auto"})
    result = [{
        "configId": "mode", "name": "Mode", "category": "mode", "type": "select",
        "currentValue": session["mode"], "options": modes,
    }]
    result.append({
        "configId": "model", "name": "Model", "category": "model", "type": "select",
        "currentValue": session["model"],
        "options": [{"value": "small", "name": "Small"}, {"value": "large", "name": "Large"}],
    })
    # As Claude Code's fast mode: a boolean, which every v2 client takes.
    result.append({
        "configId": "fast", "name": "Fast mode", "category": "model_config",
        "type": "boolean", "currentValue": session.get("fast", False),
    })
    result.append({
        "configId": "reasoning_effort", "name": "Reasoning", "category": "thought_level", "type": "select",
        "currentValue": session.get("reasoning_effort", "low"),
        "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}],
    })
    return result


def commands():
    listed = [{"name": "review", "description": "Review the changes",
               "input": {"type": "text", "hint": "what to review"}}]
    if not env("ACP_FIXTURE_NO_COMPACT"):
        listed.append({"name": "compact", "description": "Compact the conversation"})
    return listed


def session_result(session_id, include_id):
    # v2 gives the session's commands with it, as Claude Code's adapter does.
    session = sessions[session_id]
    result = {"configOptions": options(session), "availableCommands": commands()}
    if include_id:
        result["sessionId"] = session_id
    return result


def state(session_id, name, **fields):
    update(session_id, {"sessionUpdate": "state_update", "state": name, **fields})


def finish(session_id, reason, usage=None, error=None):
    """Ends the session's work: `idle` with the stop reason. A prompt after
    it starts work of its own rather than steering this."""
    with state_lock:
        running[session_id] = False
    fields = {"stopReason": reason}
    if usage is not None:
        fields["usage"] = usage
    if error is not None:
        fields["error"] = error
    state(session_id, "idle", **fields)


def tool_call(session_id, call):
    update(session_id, {"sessionUpdate": "tool_call_update", **call})


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
    call = {"toolCallId": call_id, "title": title, "kind": kind, "status": "pending", "locations": locations}
    if raw is not None:
        call["rawInput"] = raw
    tool_call(session_id, call)
    answer = request("session/request_permission", {
        "sessionId": session_id,
        "title": f"Allow {title}?",
        "subject": {"type": "tool_call", "toolCall": {"toolCallId": call_id}},
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


def b64(text):
    return base64.b64encode(text.encode()).decode()


def terminal_calls(session_id):
    # Output in chunks, as Codex streams it, then the exit and a failure.
    tool_call(session_id, {
        "toolCallId": "call-codex", "title": "make test", "kind": "execute", "status": "in_progress",
        "rawInput": {"command": "make test"},
        "content": [{"type": "terminal", "terminalId": "term-codex"}],
    })
    update(session_id, {"sessionUpdate": "terminal_update", "terminalId": "term-codex",
                        "command": "make test", "cwd": "/work"})
    for chunk in ["building\n", "\u001b[31mFAILED\u001b[0m 1 test\n"]:
        update(session_id, {"sessionUpdate": "terminal_output_chunk", "terminalId": "term-codex", "data": b64(chunk)})
    update(session_id, {"sessionUpdate": "terminal_update", "terminalId": "term-codex",
                        "exitStatus": {"exitCode": 3}})
    tool_call(session_id, {"toolCallId": "call-codex", "status": "failed"})
    # The whole output as a snapshot, which comes before the call names the
    # terminal, then the exit with the status.
    tool_call(session_id, {
        "toolCallId": "call-claude", "title": "ls", "kind": "execute", "status": "pending",
        "rawInput": {"command": "ls"},
        "content": [{"type": "content", "content": {"type": "text", "text": "List the files"}}],
    })
    update(session_id, {"sessionUpdate": "terminal_update", "terminalId": "term-claude",
                        "command": "ls", "output": {"data": b64("notes.txt\n")}})
    update(session_id, {"sessionUpdate": "terminal_update", "terminalId": "term-claude",
                        "exitStatus": {"exitCode": 0}})
    tool_call(session_id, {
        "toolCallId": "call-claude", "status": "completed",
        "content": [{"type": "terminal", "terminalId": "term-claude"}],
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
        tool_call(session_id, {"toolCallId": call_id, "title": "AskUserQuestion", "kind": "other", "status": "pending"})
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


USAGE = {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}


def insert(request_id, session_id, params):
    """Takes a prompt in: its `user_message` echo, then its answer."""
    message_id = next_message_id("user")
    update(session_id, {"sessionUpdate": "user_message", "messageId": message_id,
                        "content": params.get("prompt", [])})
    respond(request_id, {"messageId": message_id})


def prompt(request_id, params):
    session_id = params["sessionId"]
    session = sessions.setdefault(session_id, {"mode": "default", "model": "small", "turn": 0})
    session["turn"] = session.get("turn", 0) + 1
    texts = [block.get("text", "") for block in params.get("prompt", []) if block.get("type") == "text"]
    if texts:
        session.setdefault("first_text", texts[0])
    kinds = [block.get("type") for block in params.get("prompt", [])]
    command = texts[-1] if texts else ""
    word, _, rest = command.partition(" ")
    session.setdefault("history", []).append({"role": "user", "text": command})
    insert(request_id, session_id, params)
    state(session_id, "running")
    try:
        run_command(request_id, session_id, session, word, rest, texts, kinds)
    finally:
        with state_lock:
            running[session_id] = False


def wait_for_steer(session_id, timeout):
    """Waits for a message steered into the work, or a cancel: the texts,
    or None for a cancel."""
    with state_lock:
        state_lock.wait_for(lambda: steered[session_id] or cancelled[session_id], timeout=timeout)
        if cancelled[session_id]:
            return None
        return list(steered[session_id])


def wake_later(session_id, delay, work):
    """Starts work of the agent's own `delay` seconds after now, unless a
    prompt's work runs then."""
    def later():
        import time
        time.sleep(delay)
        with state_lock:
            if running.get(session_id):
                return
            running[session_id] = True
            cancelled[session_id] = False
            steered[session_id] = []
        state(session_id, "running")
        if work() == "cancelled":
            finish(session_id, "cancelled")
            return
        finish(session_id, "end_turn", USAGE)
    threading.Thread(target=later, daemon=True).start()


def subagent(parent, title, task, work):
    """A subagent session of the agent's own: announced in `parent`, given
    `task`, and doing `work` on its own stream."""
    child = new_session_id() + "-sub"
    sessions[child] = {"mode": "default", "model": "small", "history": [], "parent": parent}
    update(parent, {"sessionUpdate": "subagent_update", "sessionId": child, "title": title,
                    "description": "a fixture subagent", "capabilities": {"cancel": {}},
                    "state": {"state": "running"}})
    # As Claude Code's adapter: the child runs, then the task arrives, its
    # parent's view of it first.
    with state_lock:
        running[child] = True
        cancelled[child] = False
        steered[child] = []
    state(child, "running")
    # The task trails the child's `running`, as a busy adapter's may: the
    # session waits for it before its first run starts.
    import time
    time.sleep(0.2)
    message = {"sessionUpdate": "session_message", "messageId": next_message_id("task"),
               "senderSessionId": parent, "recipientSessionId": child,
               "content": [{"type": "text", "text": task}]}
    update(parent, message)
    update(child, message)
    if work(child) == "cancelled":
        finish(child, "cancelled")
        return
    finish(child, "end_turn", USAGE)
    update(parent, {"sessionUpdate": "subagent_update", "sessionId": child,
                    "state": {"state": "idle", "stopReason": "end_turn"}})


def run_command(request_id, session_id, session, word, rest, texts, kinds):
    if word == "subagent":
        title, _, task = rest.partition(" ")
        def work(child):
            tool_call(child, {"toolCallId": f"{child}-read", "title": "Read notes.txt", "kind": "read",
                              "status": "completed", "rawInput": {"file_path": "/work/notes.txt"},
                              "content": [{"type": "content", "content": {"type": "text", "text": "one"}}]})
            say(child, "subagent says: " + task)
        subagent(session_id, title, task, work)
        say(session_id, "subagent done")
    elif word == "subagent-wait":
        def work(child):
            say(child, "waiting")
            if wait_for_steer(child, 10) is None:
                return "cancelled"
            say(child, "not stopped")
        threading.Thread(target=subagent, args=(session_id, rest or "waiter", "wait", work), daemon=True).start()
        say(session_id, "subagent started")
    elif word == "subagent-ask":
        kind, _, resource = rest.partition(" ")
        def work(child):
            outcome = permission(child, f"{child}-ask", kind, resource)
            say(child, f"subagent asked: {outcome}")
        subagent(session_id, "asker", "ask something", work)
        say(session_id, "subagent done")
    elif word in ("background", "background-wake"):
        seconds, _, wake = rest.partition(" ")
        delay = float(seconds or "1")
        tool_call(session_id, {"toolCallId": "call-bg", "name": "Bash", "title": "sleep", "kind": "execute",
                               "status": "in_progress", "rawInput": {"command": f"sleep {seconds}"},
                               "content": [{"type": "terminal", "terminalId": "term-bg"}]})
        update(session_id, {"sessionUpdate": "terminal_update", "terminalId": "term-bg", "command": f"sleep {seconds}"})
        tool_call(session_id, {"toolCallId": "call-bg", "status": "completed"})
        def later():
            import time
            time.sleep(delay)
            update(session_id, {"sessionUpdate": "terminal_update", "terminalId": "term-bg",
                                "exitStatus": {"exitCode": 0}})
            if word == "background-wake":
                wake_later(session_id, float(wake or "1"), lambda: say(session_id, "woke"))
        threading.Thread(target=later, daemon=True).start()
        say(session_id, "started")
    elif word == "wake-later":
        delay, _, text = rest.partition(" ")
        wake_later(session_id, float(delay), lambda: say(session_id, text or "woke"))
        say(session_id, "later")
    elif word == "wake-steer":
        def wait_for_message():
            say(session_id, "waiting")
            got = wait_for_steer(session_id, 10)
            if got is None:
                return "cancelled"
            say(session_id, "steered: " + " | ".join(got))
        wake_later(session_id, float(rest), wait_for_message)
        say(session_id, "later")
    elif word == "wake-ask":
        delay, _, ask = rest.partition(" ")
        kind, _, resource = ask.partition(" ")
        def ask_permission():
            outcome = permission(session_id, f"call-wake-{session['turn']}", kind, resource)
            say(session_id, f"woke and asked: {outcome}")
        wake_later(session_id, float(delay), ask_permission)
        say(session_id, "later")
    elif word == "say":
        say(session_id, rest)
    elif word == "think-blank":
        update(session_id, {"sessionUpdate": "agent_thought_chunk", "messageId": next_message_id("thought"),
                            "content": {"type": "text", "text": "\n\n"}})
        update(session_id, {"sessionUpdate": "agent_thought_chunk", "messageId": next_message_id("thought"),
                            "content": {"type": "text", "text": "**Pondering**"}})
        say(session_id, rest)
    elif word == "two-messages":
        say(session_id, "first")
        say(session_id, "second")
    elif word == "steer-late":
        say(session_id, "waiting")
        with state_lock:
            state_lock.wait_for(lambda: steered[session_id] or cancelled[session_id], timeout=10)
            got = list(steered[session_id])
            taken = late.pop(session_id, [])
        say(session_id, "before")
        for steer_request, steer_params in taken:
            insert(steer_request, session_id, steer_params)
        say(session_id, "steered: " + " | ".join(got) if got else "no steer")
    elif word == "think":
        update(session_id, {"sessionUpdate": "agent_thought_chunk", "messageId": next_message_id("thought"),
                            "content": {"type": "text", "text": "pondering"}})
        say(session_id, rest)
    elif word == "echo" or not word:
        say(session_id, " | ".join(texts + [k for k in kinds if k != "text"]))
    elif word == "work":
        update(session_id, {"sessionUpdate": "agent_thought_chunk", "messageId": next_message_id("thought"),
                            "content": {"type": "text", "text": "planning"}})
        say(session_id, "Editing.")
        update(session_id, {"sessionUpdate": "plan_update", "plan": {"type": "items", "planId": "plan", "entries": [
            {"content": "Read the file", "priority": "high", "status": "completed"},
            {"content": "Edit the file", "priority": "medium", "status": "in_progress"},
        ]}})
        tool_call(session_id, {
            "toolCallId": "call-edit", "title": "Edit notes.txt",
            "kind": "edit", "status": "in_progress", "locations": [{"path": "/work/notes.txt", "line": 2}],
            "rawInput": {"path": "/work/notes.txt"},
        })
        patch = "diff --git a/notes.txt b/notes.txt\n--- a/notes.txt\n+++ b/notes.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+2\n"
        tool_call(session_id, {
            "toolCallId": "call-edit", "status": "completed",
            "content": [{"type": "diff", "changes": [{"operation": "modify", "path": "/work/notes.txt"}],
                         "patch": {"format": "git_patch", "text": patch}}],
            "rawOutput": {"ok": True},
        })
        update(session_id, {"sessionUpdate": "usage_update", "used": 1234, "size": 200000, "cost": {"amount": 0.5, "currency": "USD"}})
        # The same context use again: the session records it once.
        update(session_id, {"sessionUpdate": "usage_update", "used": 1234, "size": 200000, "cost": {"amount": 0.5, "currency": "USD"}})
        say(session_id, "Done.")
    elif word == "late-input":
        tool_call(session_id, {
            "toolCallId": "call-late", "name": "Read", "title": "Tool", "kind": "other", "status": "pending",
        })
        tool_call(session_id, {
            "toolCallId": "call-late", "title": "Read notes.txt",
            "kind": "read", "rawInput": {"file_path": "/work/notes.txt"},
        })
        tool_call(session_id, {
            "toolCallId": "call-late", "status": "completed",
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
        # As Claude Code's adapter: the end again, with the token counts.
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
            finish(session_id, "cancelled")
            return
        say(session_id, "steered: " + " | ".join(got) if got else "no steer")
    elif word == "decline-wait":
        say(session_id, "waiting")
        with state_lock:
            state_lock.wait_for(
                lambda: steered[session_id] or cancelled[session_id] or declined.get(session_id),
                timeout=30,
            )
            got = list(steered[session_id])
            stopped = cancelled[session_id]
        if stopped:
            finish(session_id, "cancelled")
            return
        say(session_id, "steered: " + " | ".join(got) if got else "no steer")
    elif word == "stop":
        say(session_id, "stopping")
        finish(session_id, rest, {"inputTokens": 40, "outputTokens": 2, "totalTokens": 42})
        return
    elif word == "permission":
        kind, _, resource = rest.partition(" ")
        outcome = permission(session_id, f"call-{session['turn']}", kind, resource)
        tool_call(session_id, {"toolCallId": f"call-{session['turn']}",
                               "status": "completed" if outcome == "yes" else "failed"})
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
        tool_call(session_id, {"toolCallId": call_id, "status": "completed" if outcome == "yes" else "failed"})
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
        finish(session_id, "cancelled")
        return
    elif word == "elicit":
        say(session_id, "elicitation: " + elicit(session_id, session, rest))
    elif word == "elicit-wait":
        outcome = elicit(session_id, session, rest)
        log({"fixture": "elicitation outcome", "outcome": outcome})
        with state_lock:
            while not cancelled[session_id]:
                state_lock.wait()
        finish(session_id, "cancelled")
        return
    elif word == "exit":
        sys.stderr.write("fixture crashed on purpose\n")
        sys.stderr.flush()
        os._exit(3)
    elif word == "error":
        finish(session_id, "error", error={"code": -32000, "message": "Authentication required"})
        return
    elif word == "terminal":
        terminal_calls(session_id)
        say(session_id, "ran")
    elif word == "picture":
        say(session_id, "Here it is:")
        update(session_id, {"sessionUpdate": "agent_message_chunk", "messageId": next_message_id(),
                            "content": {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="}})
        say(session_id, "A picture.")
    elif word == "mode-switch":
        session["mode"] = rest
        update(session_id, {"sessionUpdate": "config_option_update", "configOptions": options(session)})
        say(session_id, f"mode: {rest}")
    elif word == "recall":
        earlier = [entry["text"] for entry in session.get("history", [])[:-1] if entry["role"] == "user"]
        say(session_id, "remembers: " + ", ".join(earlier))
    elif word == "title":
        update(session_id, {"sessionUpdate": "session_info_update", "title": rest})
        say(session_id, "titled")
    elif word == "title-first":
        update(session_id, {"sessionUpdate": "session_info_update", "title": session["first_text"]})
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
            arguments = json.loads(arguments or "{}")
            name = f"mcp__cookie__{tool}"
            reported = {"toolCallId": f"call-{action}-{tool}", "name": name, "title": name, "kind": "other"}
            if action in ("echo", "miss"):
                tool_call(session_id, {**reported, "status": "pending", "rawInput": arguments})
            if action == "miss":
                tool_call(session_id, {"toolCallId": reported["toolCallId"], "status": "failed",
                                       "content": [{"type": "content", "content": {"type": "text", "text": "MCP error: not connected"}}]})
                say(session_id, "missed")
                finish(session_id, "end_turn", USAGE)
                return
            reply = mcp(session, "tools/call", {"name": tool, "arguments": arguments}, 3)
            result = reply.get("result", {})
            text = " ".join(item.get("text", "") for item in result.get("content", []))
            if action == "echo":
                tool_call(session_id, {"toolCallId": reported["toolCallId"],
                                       "status": "failed" if result.get("isError") else "completed"})
            say(session_id, f"result: {text} error: {result.get('isError')}")
    else:
        say(session_id, " | ".join(texts))
    finish(session_id, "end_turn", USAGE)


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
        client_capabilities.update(params.get("capabilities") or {})
        rich = not env("ACP_FIXTURE_IMAGE_ONLY")
        session_caps = {
            "prompt": {"image": {}, **({"audio": {}, "embeddedContext": {}} if rich else {})},
            "mcp": {"stdio": {}},
        }
        if rich:
            session_caps["additionalDirectories"] = {}
        if env("ACP_FIXTURE_FORK"):
            session_caps["fork"] = {}
        if env("ACP_FIXTURE_DELETE"):
            session_caps["delete"] = {}
        if env("ACP_FIXTURE_SYSTEM_PROMPT"):
            session_caps["systemPrompt"] = {}
        respond(request_id, {
            "protocolVersion": 1 if env("ACP_FIXTURE_V1") else 2,
            "info": {"name": "fixture-agent", "title": "Fixture Agent", "version": "1.0.0"},
            "capabilities": {"session": session_caps},
            "authMethods": [],
        })
    elif method == "session/new":
        session_id = new_session_id()
        sessions[session_id] = {"mode": "default", "model": "small", "mcpServers": params.get("mcpServers"), "cwd": params.get("cwd"), "history": []}
        if env("ACP_FIXTURE_EAGER_MCP") and mcp_open(sessions[session_id]):
            reply = mcp(sessions[session_id], "tools/list", {}, 2)
            sessions[session_id]["eager_tools"] = "error" not in reply
        respond(request_id, session_result(session_id, True))
    elif method == "session/resume":
        session_id = params["sessionId"]
        if env("ACP_FIXTURE_LOST"):
            fail(request_id, -32002, "Resource not found: no such session")
            return
        sessions.setdefault(session_id, {"mode": "default", "model": "small"})
        sessions[session_id]["mcpServers"] = params.get("mcpServers")
        sessions[session_id]["bridge"] = None
        # v2 snapshots the foreground state before the answer.
        state(session_id, "idle")
        respond(request_id, session_result(session_id, False))
    elif method == "session/set_config_option":
        session = sessions[params["sessionId"]]
        value = params["value"]
        kind = params.get("type")
        if params["configId"] == "fast":
            if kind != "boolean" or not isinstance(value, bool):
                fail(request_id, -32602, f"invalid value for fast: {value!r}")
                return
        elif kind != "id":
            fail(request_id, -32602, f"a select takes `type: id`, not {kind!r}")
            return
        session[params["configId"]] = value
        respond(request_id, {"configOptions": options(session)})
    elif method == "session/prompt":
        session_id = params["sessionId"]
        texts = [block.get("text", "") for block in params.get("prompt", []) if block.get("type") == "text"]
        with state_lock:
            busy = running.get(session_id, False)
            if not busy:
                running[session_id] = True
                cancelled[session_id] = False
                steered[session_id] = []
                declined[session_id] = False
        if busy:
            # A prompt during the work: a steer, inserted into it.
            if env("ACP_FIXTURE_DECLINE_STEERS"):
                fail(request_id, -32603, "Internal error: the steer was not taken")
                # Answered first: the work it was meant for may end now.
                with state_lock:
                    declined[session_id] = True
                    state_lock.notify_all()
                return
            if env("ACP_FIXTURE_LATE_STEERS"):
                with state_lock:
                    late.setdefault(session_id, []).append((request_id, params))
                    steered[session_id].extend(texts)
                    state_lock.notify_all()
                return
            insert(request_id, session_id, params)
            with state_lock:
                steered[session_id].extend(texts)
                state_lock.notify_all()
            return
        threading.Thread(target=prompt, args=(request_id, params), daemon=True).start()
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

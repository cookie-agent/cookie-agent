"""A scripted OpenAI-compatible model for scripts/web-screenshots.sh.

It plays one short, tidy coding exchange so the web client can be
photographed: asked to fix `greet.py`, it edits the file, runs the check with
`bash`, and answers in markdown. Asked to take its time, it streams a long
answer slowly, so the page can be photographed while a run is going. Which step it plays follows from how many
tool results the request carries after the last user message; a request
without tools, the session title agent's, gets the title. A message
"Note: <title>." is answered "Noted.", and its session is titled <title>,
for the sessions that show the sessions list's badges.

Run: python3 -I showcase-model.py <port-file>; it writes the port it listens on.
"""

import json
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

USAGE = {"prompt_tokens": 1840, "completion_tokens": 96, "total_tokens": 1936}


def chunk(delta, finish=None, usage=None):
    body = {"choices": [{"delta": delta, "finish_reason": finish}]}
    if usage:
        body["usage"] = usage
    return "data: " + json.dumps(body) + "\n\n"


def text(parts, pause=0.05):
    for part in parts:
        yield chunk({"content": part})
        time.sleep(pause)
    yield chunk({}, "stop", USAGE)


def tool(preface, name, arguments):
    if preface:
        yield chunk({"content": preface})
    call = {
        "index": 0,
        "id": "call-" + str(time.time_ns()),
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }
    yield chunk({"tool_calls": [call]})
    yield chunk({}, "tool_calls", USAGE)


def reply(request):
    messages = request.get("messages", [])
    note = re.search(r"Note: ([^.\"]+)\.", json.dumps(messages))
    if not request.get("tools"):
        # The session title agent.
        return text([note.group(1) if note else "Fix the greeting in greet.py"])
    last_user = max(
        (index for index, message in enumerate(messages) if message.get("role") == "user"),
        default=-1,
    )
    results = sum(1 for message in messages[last_user + 1 :] if message.get("role") == "tool")
    asked = messages[last_user].get("content") if last_user >= 0 else ""
    if isinstance(asked, str) and asked.startswith("Note:"):
        return text(["Noted."])
    if isinstance(asked, str) and "take your time" in asked:
        return text(["Looking at it slowly. "] * 160, pause=0.25)
    if results == 0:
        return tool(
            "The greeting is missing its comma and exclamation mark.",
            "edit",
            {
                "filePath": "greet.py",
                "oldString": '    return "Hello " + name',
                "newString": '    return f"Hello, {name}!"',
            },
        )
    if results == 1:
        return tool("", "bash", {"command": "python3 test_greet.py"})
    return text(
        [
            "Fixed. `greet()` now returns **Hello, Ada!**\n\n",
            "- `greet.py`: an f-string adds the comma and the exclamation mark.\n",
            "- `python3 test_greet.py` passes.\n",
        ]
    )


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length) or b"{}")
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("connection", "close")
        self.end_headers()
        try:
            for part in reply(request):
                self.wfile.write(part.encode())
                self.wfile.flush()
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True

    def log_message(self, *_):
        pass


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    with open(sys.argv[1], "w") as port_file:
        port_file.write(str(server.server_address[1]))
    server.serve_forever()


main()

"""A scripted OpenAI-compatible model for the web client's browser scenarios.

It answers `POST /v1/chat/completions` with a streamed reply chosen by the
last message: a tool result gets a short final answer; otherwise the last
user text decides.

- containing "markdown": a streamed markdown answer with a code block;
- containing "tool": a `bash` call to `printf`;
- containing "slow": a long answer streamed slowly, to steer, queue, and
  cancel while it runs;
- anything else: "ok".

Run: python3 -I mock-model.py <port-file>; it writes the port it listens on.
"""

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

USAGE = {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}


def chunk(delta, finish=None, usage=None):
    body = {"choices": [{"delta": delta, "finish_reason": finish}]}
    if usage:
        body["usage"] = usage
    return "data: " + json.dumps(body) + "\n\n"


def text_reply(parts, pause=0.0):
    for part in parts:
        yield chunk({"content": part})
        if pause:
            time.sleep(pause)
    yield chunk({}, "stop", USAGE)


def tool_reply():
    call = {
        "index": 0,
        "id": "call-" + str(time.time_ns()),
        "type": "function",
        "function": {
            "name": "bash",
            "arguments": json.dumps({"command": "printf 'hello from the tool\\n'"}),
        },
    }
    yield chunk({"tool_calls": [call]})
    yield chunk({}, "tool_calls", USAGE)


def last_text(messages):
    for message in reversed(messages):
        if message.get("role") == "user":
            content = message.get("content")
            if isinstance(content, list):
                return " ".join(part.get("text", "") for part in content if isinstance(part, dict))
            return content or ""
    return ""


def reply(request):
    messages = request.get("messages", [])
    if messages and messages[-1].get("role") == "tool":
        return text_reply(["The tool ran."])
    text = last_text(messages).lower()
    if "markdown" in text:
        return text_reply(
            [
                "# A heading\n\n",
                "Some **bold** text and `inline code`.\n\n",
                "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n",
            ],
            pause=0.2,
        )
    if "tool" in text:
        return tool_reply()
    if "slow" in text:
        return text_reply(["word%d " % index for index in range(60)], pause=0.25)
    return text_reply(["ok"])


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

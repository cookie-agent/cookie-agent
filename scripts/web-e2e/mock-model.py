"""A scripted OpenAI-compatible model for the web client's browser scenarios.

It answers `POST /v1/chat/completions` with a streamed reply chosen by the
last message: a tool result gets a short final answer, a request without
tools (a compaction) a summary, and a background call's report "ok";
otherwise the last user text decides.

- containing "markdown": a streamed markdown answer with a code block;
- containing "tool": a `bash` call to `printf`;
- containing "slow": a long answer streamed slowly, to steer, queue, and
  cancel while it runs ("very slow": slower still);
- containing "delegate": a `delegate_subagent` call to the `helper` agent,
  whose task asks for a slow answer (below) when the text says "slowly";
  "delegate deeply" asks the helper to delegate in the background, and a
  task to "delegate in the background" delegates a `helper` of its own in
  the background, whose very slow answer outlasts the delegating turn;
- containing "sleepy": a `bash` call that sleeps, to move to the background;
- containing "chatty": a `bash` call that prints a line, then sleeps, so it
  reports progress while it runs;
- containing "many lines": a `bash` call that prints 100 lines;
- containing "picture": a `read` call on `dot.png`, whose result is a media
  block;
- anything else: "ok".

A goal control message (a paused or cancelled goal) gets "ok" whatever its
objective says.

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


def tool_reply(name="bash", arguments=None):
    if arguments is None:
        arguments = {"command": "printf 'hello from the tool\\n'"}
    call = {
        "index": 0,
        "id": "call-" + str(time.time_ns()),
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
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
    if not request.get("tools"):
        # A compaction or other tool-less request: a summary.
        return text_reply(["A short summary of the conversation."])
    text = last_text(messages).lower()
    if "<background_result" in text or "[engine goal control]" in text or "<subagent_report" in text:
        # A stopped background call's report, a paused goal, or a subagent's
        # report: nothing more to do.
        return text_reply(["ok"])
    if "markdown" in text:
        return text_reply(
            [
                "# A heading\n\n",
                "Some **bold** text and `inline code`.\n\n",
                "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n",
            ],
            pause=0.2,
        )
    if "delegate in the background" in text:
        return tool_reply(
            "delegate_subagent",
            {
                "description": "grandchild task",
                "prompt": "grandchild task: answer very slowly",
                "agent_type": "helper",
                "background": True,
            },
        )
    if "delegate deeply" in text:
        return tool_reply(
            "delegate_subagent",
            {
                "description": "child task",
                "prompt": "child task: delegate in the background",
                "agent_type": "helper",
            },
        )
    if "delegate" in text:
        prompt = "child task: answer slowly" if "slowly" in text else "child task: answer"
        return tool_reply(
            "delegate_subagent",
            {"description": "child task", "prompt": prompt, "agent_type": "helper"},
        )
    if "chatty" in text:
        return tool_reply("bash", {"command": "printf 'step one\\n'; sleep 6; printf 'done\\n'"})
    if "many lines" in text:
        return tool_reply("bash", {"command": "seq 1 100"})
    if "sleepy" in text:
        return tool_reply("bash", {"command": "sleep 8; printf 'woke up\\n'"})
    if "picture" in text:
        return tool_reply("read", {"filePath": "dot.png"})
    if "tool" in text:
        return tool_reply()
    if "very slow" in text:
        return text_reply(["word%d " % index for index in range(60)], pause=0.6)
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

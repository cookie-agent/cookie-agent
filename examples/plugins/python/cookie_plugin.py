"""A dependency-free helper for writing cookie-agent plugins in Python.

The daemon starts a plugin and speaks JSON-RPC 2.0 with it over standard
input and output, one message per line (see docs/site/development/plugins.md).
This module handles the framing, the handshake, tool preparation and
execution, hooks, event delivery, and producer requests, so that a plugin is
a few declarations and handlers (see reference_plugin.py). It follows the
protocol's compatibility rules: it ignores members it does not know in what
the daemon sends, and sends only what the daemon's version includes.

It is single-threaded: while a handler waits for the daemon to answer one of
its requests (a progress report, a producer request), messages the daemon
sends meanwhile are kept and handled once the handler returns.
"""

import collections
import json
import sys

# The plugin protocol version this module speaks. A daemon is compatible when
# its major is the same; the two use what the lower minor includes.
PROTOCOL_VERSION = "1.0.0"

# JSON-RPC error codes the protocol uses (docs/site/specs/compatibility-v1.md
# section 6).
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
UNSUPPORTED_PROTOCOL = -32002
APPLICATION_ERROR = -32000


class RpcError(Exception):
    """An error answer: a JSON-RPC code, a message a user can understand,
    and the registry's `data`, whose `code` receivers branch on."""

    def __init__(self, code, message, data):
        super().__init__(message)
        self.code = code
        self.message = message
        self.data = data

    def to_json(self):
        return {"code": self.code, "message": self.message, "data": self.data}


class ToolError(Exception):
    """A tool call that could not run; the model reads `message`."""


def tool_result(text, title=None, display=None, metadata=None, failed=False):
    """A tool's result: `text` is what the model reads, `display` the
    display blocks clients show instead (the text when there are none)."""
    result = {"output": {"kind": "single", "text": text}}
    if title is not None:
        result["title"] = title
    if display:
        result["display"] = list(display)
    if metadata is not None:
        result["metadata"] = metadata
    if failed:
        result["failed"] = True
    return result


def _version(text):
    parts = text.split(".") if isinstance(text, str) else []
    if len(parts) != 3 or not all(part.isdigit() for part in parts):
        return None
    return tuple(int(part) for part in parts)


def compatible(ours, theirs):
    """Whether two protocol versions can talk: the same major. The minor
    and the patch never refuse a connection."""
    ours, theirs = _version(ours), _version(theirs)
    return ours is not None and theirs is not None and ours[0] == theirs[0]


class ToolCall:
    """One executing call, as a tool's `execute` handler sees it."""

    def __init__(self, plugin, prepared_id, params, call):
        self._plugin = plugin
        self.prepared_id = prepared_id
        self.session_id = params.get("session")
        self.run_id = params.get("run")
        self.turn = params.get("turn", {})
        self.background = params.get("background", False)
        self.tool_call_id = call["id"]
        self.name = call["name"]

    def progress(self, message, display=None, output=None):
        """Reports a status `message` and, optionally, a `display` delta
        (`{"type": "append", ...}` or `{"type": "replace", "blocks": [...]}`)
        and output text, and waits for the daemon to take it."""
        progress = {"tool_call_id": self.tool_call_id, "message": message}
        if display is not None:
            progress["display"] = display
        if output is not None:
            progress["output"] = [{"text": output}]
        outcome = self._plugin.request(
            "tool/progress", {"prepared_id": self.prepared_id, "progress": progress}
        )
        if outcome.get("error"):
            raise ToolError(outcome["error"].get("message", "progress refused"))

    def cancelled(self):
        """Whether the daemon has cancelled the call, as far as it has read."""
        return self._plugin._was_cancelled(self.prepared_id)


class Plugin:
    """A plugin: register tools, hooks, an event handler, and producing, then
    call `run`."""

    def __init__(self, name, version, reader=None, writer=None):
        self.name = name
        self.version = version
        self.daemon = None
        self._reader = reader or sys.stdin
        self._writer = writer or sys.stdout
        self._tools = {}
        self._hooks = {}
        self._event_handler = None
        self._event_patterns = []
        self._producing = False
        self._producers = {}
        self._prepared = {}
        self._next_prepared = 1
        self._next_request = 1
        self._backlog = collections.deque()
        self._cancelled = set()
        self._running = True

    # --- registration -----------------------------------------------------

    def tool(self, declaration, execute, resource=None, presentation=None):
        """Declares a tool. `execute(call, arguments)` returns a
        `tool_result`; `resource(arguments)` derives the permission resource
        a call is approved under; `presentation(name, arguments, resource)`
        returns how clients show a prepared call (title, hint,
        primary_argument, arguments, preview), which defaults to the tool's
        name and its resource."""
        self._tools[declaration["name"]] = {
            "declaration": declaration,
            "execute": execute,
            "resource": resource,
            "presentation": presentation,
        }

    def hook(self, name, handler, on_error="allow"):
        """Intercepts the hook `name`, such as `tool_before_call`, with
        `handler(params) -> result`. With `on_error` `block`, a failure of
        the hook (a timeout, a crash, an error answer) blocks its
        operation."""
        self._hooks[name] = {"handler": handler, "on_error": on_error}

    def on_event(self, patterns, handler):
        """Receives the session events whose type a pattern selects: a type,
        a prefix ending in `*`, or `*`."""
        self._event_patterns = list(patterns)
        self._event_handler = handler

    def enable_producers(self):
        """Declares producer messaging; the daemon allows it when the
        plugin's configuration does too."""
        self._producing = True

    # --- I/O --------------------------------------------------------------

    def send(self, message):
        self._writer.write(json.dumps(message, separators=(",", ":")) + "\n")
        self._writer.flush()

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method, params):
        """Sends a request to the daemon and returns its result, keeping what
        else arrives meanwhile for later."""
        request_id = "plugin-%d" % self._next_request
        self._next_request += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            message = self._read()
            if message is None:
                raise RpcError(APPLICATION_ERROR, "The daemon closed the connection.",
                               {"code": "connection_closed"})
            if "method" not in message and message.get("id") == request_id:
                if "error" in message:
                    error = message["error"]
                    raise RpcError(error.get("code"), error.get("message", ""),
                                   error.get("data", {}))
                return message.get("result")
            if message.get("method") == "tool/cancel":
                self._cancelled.add(message.get("params", {}).get("prepared_id"))
            self._backlog.append(message)

    def _read(self):
        while True:
            line = self._reader.readline()
            if not line:
                return None
            line = line.strip()
            if line:
                return json.loads(line)

    def _was_cancelled(self, prepared_id):
        return prepared_id in self._cancelled

    # --- producers --------------------------------------------------------

    def send_message(self, session_id, body, description, idempotency_key, mode="queue"):
        """Sends `body` into the session as the plugin's `default` producer,
        registering it for the session first. Returns the message ID once the
        daemon has durably accepted it; a retry with the same key returns the
        same ID."""
        producer_id = self._producers.get(session_id)
        if producer_id is None:
            registered = self.request(
                "producer/register", {"session_id": session_id, "name": "default"}
            )
            producer_id = registered["producer_id"]
            self._producers[session_id] = producer_id
        sent = self.request("producer/send", {
            "session_id": session_id,
            "producer_id": producer_id,
            "mode": mode,
            "idempotency_key": idempotency_key,
            "description": description,
            "body": body,
        })
        return sent["message_id"]

    # --- dispatch ---------------------------------------------------------

    def run(self):
        """Serves the daemon until it sends `plugin/shutdown` or closes the
        connection."""
        while self._running:
            message = self._backlog.popleft() if self._backlog else self._read()
            if message is None:
                return
            method = message.get("method")
            if method is None:
                continue  # A late reply nothing waits for.
            if "id" in message:
                self._answer(message)
            else:
                self._handle_notification(method, message.get("params") or {})

    def _answer(self, message):
        method, params = message["method"], message.get("params") or {}
        try:
            if self.daemon is None and method != "plugin/initialize":
                raise RpcError(APPLICATION_ERROR, "The connection has not been initialized yet.",
                               {"code": "not_initialized"})
            result = self._handle_request(method, params)
        except RpcError as error:
            self.send({"jsonrpc": "2.0", "id": message["id"], "error": error.to_json()})
            return
        self.send({"jsonrpc": "2.0", "id": message["id"], "result": result})

    def _handle_request(self, method, params):
        if method == "plugin/initialize":
            return self._initialize(params)
        if method == "tool/prepare":
            return {"results": [self._prepare(call) for call in params.get("calls", [])]}
        if method == "tool/revalidate":
            if params.get("prepared_id") not in self._prepared:
                return {"error": {"kind": "operation_changed", "message": "unknown prepared call"}}
            return {}
        if method == "tool/execute":
            return self._execute(params)
        if method.startswith("plugin/intercept/"):
            hook = self._hooks.get(method[len("plugin/intercept/"):])
            if hook is not None:
                return hook["handler"](params)
        raise RpcError(METHOD_NOT_FOUND, "The other side does not know the method %s." % method,
                       {"code": "method_not_found", "method": method})

    def _handle_notification(self, method, params):
        if method == "plugin/event" and self._event_handler is not None:
            self._event_handler(params)
        elif method == "producer/recovery/start" and self._producing:
            # Nothing to restore: this plugin keeps no work across restarts.
            self.request("producer/recovery/complete", {"outcome": {"status": "ready"}})
        elif method == "tool/release":
            self._prepared.pop(params.get("prepared_id"), None)
        elif method == "tool/cancel":
            self._cancelled.add(params.get("prepared_id"))
        elif method == "plugin/shutdown":
            self._running = False
        # Any other notification, such as one a newer daemon sends, is
        # ignored.

    def _initialize(self, params):
        daemon_version = params.get("protocol_version")
        if not compatible(PROTOCOL_VERSION, daemon_version):
            raise RpcError(
                UNSUPPORTED_PROTOCOL,
                "The two sides speak incompatible protocol versions: %s here and %s on "
                "the other side." % (PROTOCOL_VERSION, daemon_version),
                {"code": "unsupported_protocol", "supported_version": PROTOCOL_VERSION,
                 "peer_version": str(daemon_version)},
            )
        self.daemon = params
        producing = self._producing and "producer_messaging" in params.get("features", [])
        return {
            "protocol_version": PROTOCOL_VERSION,
            "name": self.name,
            "version": self.version,
            "features": [],
            "capabilities": {
                "producer_messaging": producing,
                "subscribe_events": self._event_patterns if self._event_handler else [],
                "subscribe_bus": False,
                "publish_bus": False,
                "intercept": [
                    {"hook": name, "on_error": hook["on_error"]}
                    for name, hook in self._hooks.items()
                ],
            },
            "tools": {
                "tools": [tool["declaration"] for tool in self._tools.values()],
                "prompt_sections": [],
            },
        }

    def _prepare(self, call):
        tool = self._tools.get(call.get("name"))
        if tool is None:
            return {
                "status": "failed",
                "error": {"kind": "failed", "message": "unknown tool `%s`" % call.get("name")},
                "presentation": {"title": str(call.get("name") or "tool")},
            }
        arguments = call.get("arguments")
        if not isinstance(arguments, dict):
            return {
                "status": "failed",
                "error": {"kind": "failed", "message": "tool arguments must be an object"},
                "presentation": {"title": call["name"]},
            }
        resource = tool["resource"](arguments) if tool["resource"] else None
        if tool["presentation"]:
            presentation = tool["presentation"](call["name"], arguments, resource)
        else:
            presentation = {"title": call["name"]}
            if resource:
                presentation["primary_argument"] = resource
        prepared_id = self._next_prepared
        self._next_prepared += 1
        self._prepared[prepared_id] = {"tool": tool, "call": call}
        # The daemon builds the call's approval identity from the permission,
        # the resource, and the normalized arguments.
        prepared = {
            "status": "prepared",
            "prepared_id": prepared_id,
            "normalized_arguments": arguments,
            "presentation": presentation,
        }
        permission = tool["declaration"].get("permission")
        if permission:
            prepared["permission"] = permission
            if resource:
                prepared["permission_resource"] = resource
        return prepared

    def _execute(self, params):
        prepared_id = params.get("prepared_id")
        prepared = self._prepared.pop(prepared_id, None)
        if prepared is None:
            raise RpcError(APPLICATION_ERROR,
                           "The tool call or workspace the request names is no longer open.",
                           {"code": "unknown_handle", "detail": "prepared call %s" % prepared_id})
        call = ToolCall(self, prepared_id, params, prepared["call"])
        try:
            result = prepared["tool"]["execute"](call, prepared["call"]["arguments"])
        except ToolError as error:
            outcome = {"status": "failed", "error": {"kind": "failed", "message": str(error)}}
        else:
            outcome = dict(result, status="completed")
        finally:
            self._cancelled.discard(prepared_id)
        return {"outcome": outcome, "media": []}

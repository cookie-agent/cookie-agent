# Write a runtime

A runtime gives sessions tools: a shell on a build server, a browser, a
database console, a device. The daemon reaches it over the
[runtime protocol](../reference/runtime-protocol.md), and any program that
speaks it is a peer of `cookie runtime serve`: its capabilities appear in the
runtimes panel, sessions attach them, and their tools pass the same
permission checks and approvals as the built-in ones. This page is the path
through the protocol; the [runtime protocol reference](../reference/runtime-protocol.md)
holds the details it links to. For the model behind it, see
[Runtimes and capabilities](../concepts/runtimes.md).

A runtime is not a plugin. A [plugin](plugins.md) runs inside the daemon's
`global` runtime, can also hook the engine's lifecycle, and speaks the plugin
protocol, which shares the runtime protocol's framing and `tool/*` messages
but has its own handshake. Write a runtime when the tools belong to another
machine or another process boundary, and a plugin when they extend the daemon
itself.

## Choose a transport

Both carry JSON-RPC 2.0, one message per frame, at most 48 MiB, and either
side sends requests. See [Connections](../reference/runtime-protocol.md#connections)
and [Framing](../reference/runtime-protocol.md#framing).

- **Standard input and output.** The daemon starts your program from a
  [`[runtimes.<id>]` template](../daemon/runtimes.md#configuration), often
  through ssh, and exchanges one message per line. Standard output carries
  protocol messages only: no blank lines, no logging (log to standard error).
  The template's `startup_timeout_ms` bounds starting and initializing.
- **Dialing in.** Your program opens a WebSocket to the daemon's
  `/runtime?id=<runtime ID>` route, the client URL with its path replaced,
  with `Authorization: Bearer <token>`; one message per text frame. A runtime
  ID is a lowercase letter and up to 31 lowercase letters, digits, or
  underscores; `global`, `local`, and the IDs of configured templates are
  taken, as is an ID already connected. The daemon allows 30 seconds for
  initializing and for each capability opening. Reconnecting after a drop is
  your job; the daemon reopens the session's instances when you are back.
  Answer `runtime/ping` at once: the daemon pings a connection that has been
  quiet for 15 s and drops one it has heard nothing on for 45 s. Watch your
  end the same way (WebSocket pings, which the daemon answers with pongs), so
  a connection that died without a close does not keep you waiting. See
  [Runtimes that dial in](../daemon/runtimes.md#runtimes-that-dial-in) and
  [Liveness](../reference/runtime-protocol.md#liveness).

## Initialize

The daemon sends `runtime/initialize` first, with its `protocol_version`, the
`runtime_id` you serve as, and `settings`: the `[tool_output]` and
`[agent_md]` limits, and MCP servers to host. Answer with your version, your
machine, and your capabilities:

```json
{"protocol_version":"1.4.0","platform":"linux",
 "machine":{"id":"3f9a…","host_name":"buildbox"},"home":"/home/ada",
 "capabilities":[{"name":"console","kind":"database","takes_workspace":false,
                  "description":"A SQL console on the reporting replica"}]}
```

- **Versions** must share their major; both sides then use the lower minor.
  Read `protocol_version` before anything else and refuse another major. See
  [Version](../reference/runtime-protocol.md#version).
- **The machine ID** is any stable, printable string of up to 128 bytes that
  is the same for every runtime on one computer; the daemon groups runtimes
  into [machines](../concepts/machines.md) by it, and the `host_name` names a
  new machine. Leave it out and the runtime gets a machine of its own. See
  [Machines](../reference/runtime-protocol.md#machines).
- **The home** is optional: your user's home directory, canonical and
  absolute. Clients then show your paths under it as `~/…`, and permission
  rules starting with `~/` mean paths under it. Leave it out and clients show
  absolute paths. See [Home](../reference/runtime-protocol.md#home).
- **Capabilities** each have a `name`, a free-form `kind` label, a
  `description` for people choosing one, and whether they take a workspace.
  Their tools come later, per instance. See
  [`runtime/initialize`](../reference/runtime-protocol.md#runtimeinitialize).
- **MCP servers** are optional to host. A runtime that hosts none rejects a
  nonempty `settings.mcp_servers` with the `initialize_rejected` error; then
  nobody configures `[runtimes.<id>.mcp]` for it.

## Open capabilities

When a session attaches a capability, the daemon sends `capability/open` with
the capability's name and, when it takes one, the workspace exactly as the
user typed it; resolve it on your machine. Answer with an `instance` handle
of your choosing, the `attachment_name` sessions see it under, the resolved
`workspace`, its `tools`, and its `skills`. Opening an instance that is
already open returns the same one. When an instance's tools change, send
`capability/tools_changed` with the whole new list. See
[`capability/open`](../reference/runtime-protocol.md#capabilityopen) and
[Capabilities and instances](../reference/runtime-protocol.md#capabilities-and-instances).

A tool declaration is its `name`, `description`, and JSON Schema
`parameters`, as the model will see them, plus:

- `permission`: the [permission action](../concepts/permissions.md) it is
  checked under, any label you choose, such as `sql`; absent means it needs no
  permission and is always allowed;
- `collision`: what happens when another attachment offers the same name,
  `prefix` (the default: both become `<attachment>_<name>`), `override`, or
  `fallback`;
- `concurrency` (`exclusive` or `parallel`), `output` (one output or named
  streams), `result_truncation`, and `background` for calls that may run in
  the background.

## Run tool calls

A call takes two steps, so that it can be approved between them:

1. **`tool/prepare`** carries a batch of calls with the session, run, agent,
   and model. For each, check the arguments and answer `prepared` with a
   `prepared_id`, the normalized arguments, the call's `permission` and
   `permission_resource` (the path, command, or URL a rule matches), and a
   `presentation` (at least a `title`) clients show; or answer `failed` with
   an error the model reads. The daemon checks permissions and asks for
   approval from what you returned. See
   [`tool/prepare`](../reference/runtime-protocol.md#toolprepare).
2. **`tool/execute`** runs a prepared call and returns its outcome: output
   the model reads, an optional title, display blocks clients show, metadata,
   and media. While it runs, `tool/progress` streams output and display
   updates; `tool/cancel` asks you to stop, and the execute request still
   gets its answer. See [`tool/execute`](../reference/runtime-protocol.md#toolexecute)
   and [Display blocks](../reference/runtime-protocol.md#display-blocks).

`tool/release` discards a prepared call that will not run, and
`tool/revalidate` asks whether one is still valid before it runs.

Besides tools, `runtime/context` asks for an instance's `AGENTS.md` files
(answer with empty lists when you have none), and `runtime/shutdown` asks you
to stop: answer it, then exit. Producer messaging, for runtimes that send
messages into sessions on their own, is negotiated at initialize; see
[Producer messaging](../reference/runtime-protocol.md#producer-messaging).
Answer a request you do not know with `-32601`, and ignore notifications and
members you do not know.

## A minimal runtime

The sketch below serves one capability with one tool over standard input and
output, in the style of the [Python plugin helper](plugins.md#reference-plugin)
(`examples/plugins/python/cookie_plugin.py`). It is a starting point, not a
complete implementation: a real runtime handles every call of a batch,
cancellation, and requests concurrently.

```python
import json, sys, time

def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()

TOOLS = {"tools": [{
    "name": "clock",
    "description": "Tell the time on the build server.",
    "parameters": {"type": "object", "properties": {}, "additionalProperties": False},
}], "prompt_sections": []}
prepared = {}

def handle(method, params):
    if method == "runtime/initialize":
        # A real runtime refuses another major with -32002 (see Version).
        return {"protocol_version": "1.4.0", "capabilities": [{
            "name": "clock", "kind": "clock", "takes_workspace": False,
            "description": "The build server's clock"}]}
    if method == "capability/open":
        return {"instance": "clock", "attachment_name": "clock",
                "tools": TOOLS, "skills": {"skills": [], "discoveries": []}}
    if method == "runtime/context":
        return {"agent_md": [], "skipped": []}
    if method == "tool/prepare":
        results = []
        for call in params["calls"]:
            prepared_id = len(prepared) + 1
            prepared[prepared_id] = call
            results.append({"status": "prepared", "prepared_id": prepared_id,
                            "normalized_arguments": call["arguments"],
                            "presentation": {"title": "Read the clock"}})
        return {"results": results}
    if method == "tool/execute":
        prepared.pop(params["prepared_id"])
        now = time.strftime("%Y-%m-%d %H:%M:%S %Z")
        return {"outcome": {"status": "completed",
                            "output": {"kind": "single", "text": now}},
                "media": []}
    if method in ("runtime/ping", "runtime/shutdown"):
        return {}
    return None

for line in sys.stdin:
    if not line.strip():
        continue
    message = json.loads(line)
    if "id" not in message or "method" not in message:
        continue  # a notification such as tool/release, or a reply
    result = handle(message["method"], message.get("params") or {})
    if result is None:
        send({"jsonrpc": "2.0", "id": message["id"],
              "error": {"code": -32601, "message": "unknown method"}})
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    if message["method"] == "runtime/shutdown":
        break
```

Run it from a template in the daemon's `config.toml`:

```toml
[runtimes.buildbox]
command = ["ssh", "-o", "BatchMode=yes", "buildbox", "python3", "clock_runtime.py"]
```

Then attach `buildbox/clock` from `/runtime`. The tool declares no
`permission`, so every agent may call it; declare one to put it under
permission rules. A version that dials in instead connects with a WebSocket
library and exchanges the same messages as text frames.

## Check your messages

The runtime protocol's schemas are published with these pages:
[`methods.json`](../schemas/runtime/methods.json) and one JSON Schema per
params and result root under `schemas/runtime/schemas/`, as
`crates/protocol/compat/runtime/` commits them. Validate what you send against
them in your tests. See [Published schemas](../reference/schemas.md#published-schemas).
In Rust, `cookie_agent_coding_runtime`'s server is the reference
implementation, and `cookie_agent_runtime_api` has the tool provider types it
builds on; see the [Rust API](../reference/api.md).

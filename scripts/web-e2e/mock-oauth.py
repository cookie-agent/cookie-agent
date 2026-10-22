"""A remote MCP server that wants OAuth, for the web client's sign-in
scenarios.

- `POST /mcp` without a token it issued answers 401 with a `Bearer`
  challenge naming its protected resource metadata; with one it speaks
  enough Streamable HTTP MCP to connect (`initialize`, `tools/list`, and
  notifications).
- `/.well-known/oauth-protected-resource/mcp` and
  `/.well-known/oauth-authorization-server` describe it as its own
  authorization server, with dynamic client registration and S256 PKCE.
- `POST /register` registers a client for the redirects it names.
- `GET /authorize` approves at once: it sends the browser to the redirect
  with a code, the state, and `iss`, but refuses a redirect the client did
  not register, as a real authorization server does.
- `POST /token` exchanges a code once, for the redirect it was issued for
  and the PKCE verifier of its challenge.
- `GET /registrations` lists the registrations made, for the scenarios.

Run: python3 -I mock-oauth.py <port-file>; it writes the port it listens on.
"""

import base64
import hashlib
import json
import secrets
import sys
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
# client_id -> its registration request
CLIENTS = {}
# code -> (client_id, redirect_uri, code_challenge)
CODES = {}
TOKENS = set()
BASE = ""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def reply(self, status, body=b"", content_type="application/json", headers=()):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
        elif isinstance(body, str):
            body = body.encode()
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.send_header("connection", "close")
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def body(self):
        length = int(self.headers.get("content-length", "0"))
        return self.rfile.read(length) if length else b""

    def do_GET(self):
        url = urllib.parse.urlsplit(self.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        if url.path in ("/.well-known/oauth-protected-resource/mcp", "/resource"):
            self.reply(200, {
                "resource": BASE + "/mcp",
                "authorization_servers": [BASE],
                "scopes_supported": ["mcp"],
            })
        elif url.path in ("/.well-known/oauth-authorization-server", "/.well-known/openid-configuration"):
            self.reply(200, {
                "issuer": BASE,
                "authorization_endpoint": BASE + "/authorize",
                "token_endpoint": BASE + "/token",
                "registration_endpoint": BASE + "/register",
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"],
                "scopes_supported": ["mcp", "offline_access"],
            })
        elif url.path == "/authorize":
            with LOCK:
                client = CLIENTS.get(query.get("client_id", ""))
            redirect = query.get("redirect_uri", "")
            if client is None or redirect not in client.get("redirect_uris", []):
                self.reply(400, "redirect_uri is not registered for this client", "text/plain")
                return
            code = secrets.token_urlsafe(16)
            with LOCK:
                CODES[code] = (query["client_id"], redirect, query.get("code_challenge", ""))
            separator = "&" if urllib.parse.urlsplit(redirect).query else "?"
            location = redirect + separator + urllib.parse.urlencode(
                {"code": code, "state": query.get("state", ""), "iss": BASE}
            )
            self.reply(302, "redirecting", "text/plain", [("location", location)])
        elif url.path == "/registrations":
            with LOCK:
                self.reply(200, list(CLIENTS.values()))
        elif url.path == "/mcp":
            self.reply(405, "", "text/plain")
        else:
            self.reply(404, "", "text/plain")

    def do_POST(self):
        url = urllib.parse.urlsplit(self.path)
        body = self.body()
        if url.path == "/register":
            registration = json.loads(body or b"{}")
            with LOCK:
                client_id = "web-e2e-client-%d" % (len(CLIENTS) + 1)
                CLIENTS[client_id] = registration
            self.reply(201, dict(registration, client_id=client_id))
        elif url.path == "/token":
            form = dict(urllib.parse.parse_qsl(body.decode()))
            if form.get("grant_type") == "refresh_token":
                self.reply(400, {"error": "invalid_grant"})
                return
            with LOCK:
                issued = CODES.pop(form.get("code", ""), None)
            verifier = form.get("code_verifier", "").encode()
            challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier).digest()).rstrip(b"=").decode()
            if issued is None or issued[1] != form.get("redirect_uri") or issued[2] != challenge:
                self.reply(400, {"error": "invalid_grant", "error_description": "unknown code"})
                return
            token = "access-" + secrets.token_urlsafe(16)
            with LOCK:
                TOKENS.add(token)
            self.reply(200, {
                "access_token": token,
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "mcp",
            })
        elif url.path == "/mcp":
            authorization = self.headers.get("authorization", "")
            with LOCK:
                valid = authorization.startswith("Bearer ") and authorization[7:] in TOKENS
            if not valid:
                challenge = 'Bearer resource_metadata="%s/resource", error="invalid_token", scope="mcp"' % BASE
                self.reply(401, "authorization required", "text/plain", [("www-authenticate", challenge)])
                return
            message = json.loads(body or b"{}")
            method = message.get("method")
            if "id" not in message:
                self.reply(202, "", "text/plain")
            elif method == "initialize":
                self.reply(200, {"jsonrpc": "2.0", "id": message["id"], "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "web-e2e-oauth", "version": "1.0.0"},
                }})
            elif method == "tools/list":
                self.reply(200, {"jsonrpc": "2.0", "id": message["id"], "result": {"tools": []}})
            else:
                self.reply(200, {"jsonrpc": "2.0", "id": message["id"],
                                 "error": {"code": -32601, "message": "unsupported"}})
        else:
            self.reply(404, "", "text/plain")

    def do_DELETE(self):
        self.reply(200, "", "text/plain")

    def log_message(self, *_):
        pass


def main():
    global BASE
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    BASE = "http://127.0.0.1:%d" % server.server_address[1]
    with open(sys.argv[1], "w") as port_file:
        port_file.write(str(server.server_address[1]))
    server.serve_forever()


main()

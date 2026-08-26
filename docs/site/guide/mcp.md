# MCP servers

Cookie Agent supports MCP tools through `tools/list` and `tools/call`. It does
not expose MCP resources or prompts. Server requests for sampling, elicitation,
roots, or other interactive input are rejected.

## Configuration

Define servers in either the user configuration or the exact workspace's
`.cookie-agent/config.toml`:

```toml
[mcp.servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "token" }
cwd = "/workspace"
timeout_ms = 30000

[mcp.servers.remote]
url = "https://mcp.example.com/mcp"
oauth = true
lazy = true
```

Each server must set exactly one of `command` or `url`. `args`, `env`, and `cwd`
apply only to stdio commands; `headers` applies only to Streamable HTTP. Servers
are enabled by default. An enabled server connects at startup unless `lazy =
true`; a lazy server exposes no tools until its first named tool use establishes
the connection. `timeout_ms` bounds connection, listing, and calls and defaults
to 30000. Eager servers connect in parallel. The first run waits for every
eager server's initial connection and `tools/list` attempt to finish, so its tool
context is not built from a partial startup snapshot. Each wait remains bounded
by that server's `timeout_ms`; one server's failure does not block the others.

## OAuth for remote servers

Streamable HTTP servers use OAuth automatically when an unauthenticated request
returns an authorization challenge. OAuth can be selected explicitly or disabled:

```toml
[mcp.servers.remote]
url = "https://mcp.example.com/mcp"
oauth = true

[mcp.servers.pre_registered]
url = "https://other.example.com/mcp"
oauth = { client_id = "cookie-agent", scopes = ["mcp"] }

[mcp.servers.no_oauth]
url = "https://static.example.com/mcp"
oauth = false
headers = { Authorization = "Bearer static-token" }
```

Omitting `oauth` is the default auto mode. `true` and `{}` enable the same
reactive flow. The settings object may contain `client_id`, `client_secret`,
`client_metadata_url`, and `scopes`. Cookie Agent follows protected-resource and
authorization-server discovery, uses S256 PKCE, and selects a pre-registered
client, a server-supported Client ID Metadata Document, or Dynamic Client
Registration in that order. A static `Authorization` header takes precedence
over OAuth when both are configured; other static headers are sent alongside
OAuth requests. OAuth is not used for stdio servers.

An authorization challenge changes the server state to `needs_auth`. Run
`cookie mcp auth <server>`, or select the server in `/mcp` and press `a`, then
open the displayed URL. The daemon listens on an ephemeral `127.0.0.1` callback
port for five minutes. The TUI can copy the URL with `c` and cancel the wait with
Escape. Successful authorization stores the token and reconnects the server.
Expired tokens refresh automatically. A rejected refresh or revoked token
returns the server to `needs_auth` instead of repeatedly opening a browser flow.
A transient refresh failure also returns to `needs_auth` immediately; Cookie
Agent does not silently retry token requests or open a browser flow.

Authorization codes are held only by a one-shot in-memory relay. Cookie Agent
passes a fixed redacted surrogate through rmcp's traced exchange path and restores
the real code only in the outbound token request, preventing debug logs from
containing the browser callback code.

OAuth credentials are user-level and stored in the single
`~/.cookie-agent/mcp-oauth.json` file. On Unix the file is owner-only
(`0600`) when cookie agent creates it. The unversioned file is parsed strictly,
but an existing file is used regardless of permissions, ownership, links, or
symlinks. Stop the daemon and delete this file to revoke all locally stored MCP
OAuth credentials. Removing a server through MCP management removes its
credential. Writers serialize through `mcp-oauth.lock`, reread the current file,
and merge one credential key before each atomic replacement.

Each credential is keyed by the server name and a SHA-256 hash of its canonical
resource URL, and is also bound to that URL and the configured OAuth client
identity. Canonicalization lowercases the scheme and host, removes default HTTP
and HTTPS ports, and resolves dot-segments. Path bytes after dot-segment
resolution, including trailing slashes, remain significant. It does not combine
query strings or different subpaths. Editing the endpoint or client
identity invalidates the record before the replacement server can connect, even
when both resources advertise the same authorization-server issuer. The same
server name and canonical URL can reuse authorization across projects.

OAuth credentials establish identity with the remote server. They do not alter
which agents can see or call that server's tools; agent permissions control that
separately.

## TUI management

Run `/mcp` to open the live MCP panel. The list reports `connected` with its
tool count, `connecting`, `failed` with the connection error,
`needs_auth`, `disabled`, and `lazy-not-connected`. The panel polls the daemon
while it is open. Select a server to inspect its complete command, arguments,
environment, and working directory, or URL and headers. Press `a` on a
`needs_auth` remote server to begin OAuth.

The panel can reconnect a failed server, toggle enablement, and add, edit, or
remove definitions. Add/edit forms accept either a command, JSON string array
of arguments, JSON string object of environment variables, and optional working
directory, or a URL plus a JSON string object of headers. Choose
`runtime only`, `user config.toml`, or `project config.toml` before submitting.
Edits do not rename a server; remove the old name and add the new one instead.

Stdio servers negotiate MCP 2025-11-25 when needed. Servers that implement the
2026-07-28 discovery lifecycle use that version. Remote servers use Streamable
HTTP, including SSE responses on that endpoint; the retired two-endpoint legacy
SSE transport is not supported.

## Repository security

There is no separate MCP trust store or approval prompt. Every configured,
enabled server enters the normal lazy or eager connection lifecycle. MCP tools
are permission-gated like normal tools: an agent with no `mcp` permission entry
cannot see them, and scoped rules decide each call.

Project MCP configuration and project agent documents are version-controlled
content equivalent to code. A repository can ship both a server definition and
an agent document that permits its tools. Review those files and work only in
repositories you trust.

Connection failures are isolated per server. A failed server publishes no tools
and does not prevent other servers from connecting.

Definitions created or changed through the TUI are runtime-layer entries and
live only for the current daemon unless explicitly written to a file. A runtime
entry continues to win after write-back.

## Tool names and permissions

An MCP tool is exposed as `<server>_<tool>`. Both components replace characters
outside `[a-zA-Z0-9_-]` with `_`; there is no `mcp_` prefix. For example,
`github` plus `search/repos` becomes `github_search_repos`. Startup rejects a
server whose generated name collides with a built-in or another MCP tool.

```yaml
permissions:
  mcp:
    "github_*": allow
    github_delete_repo: deny
```

MCP availability is permission-driven. With no `mcp` entry, an agent receives
no MCP tools. A bare `deny`, or `"*": deny` with no non-deny exception, also
hides every MCP tool. Any other `mcp` map exposes the connected MCP tools at the
action level. Calls then use the complete generated tool name as their scoped
resource: more-specific patterns override broader patterns, and unmatched calls
ask. Each delegated agent is evaluated against its own permission map.

Each MCP description and result is treated as untrusted server output.

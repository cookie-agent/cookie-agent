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
headers = { Authorization = "Bearer token" }
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

Stdio servers negotiate MCP 2025-11-25 when needed. Servers that implement the
2026-07-28 discovery lifecycle use that version. Remote servers use Streamable
HTTP, including SSE responses on that endpoint; the retired two-endpoint legacy
SSE transport is not supported.

## Trust

Servers from `~/.config/cookie_agent/config.toml` are trusted. A server authored
under `<cwd>/.cookie-agent/config.toml` remains `pending_approval` and is not
started until explicitly approved. The approval presents the complete command
and arguments, environment, and working directory, or the URL and headers.
Approval is stored for that server name in the current project. Existing grants
are recorded in the per-project `mcp-trust-grants.jsonl` file. This store is not
versioned. An incompatible or malformed complete record is a startup error; fix
the record or delete the per-project `mcp-trust-grants.jsonl` file to reset MCP
approvals.

Approvals are keyed by server name; a later change to a project file can replace
the server command under an existing approval — only enable project servers from
repositories you trust.

With the daemon running, inspect and respond to project requests through:

```console
cookie mcp list
cookie mcp approve github
cookie mcp reject github
```

Approval is durable for the server name in that project, including across later
configuration changes. Rejection lasts for the current daemon lifetime; a later
restart presents the project request again. Approving a
non-lazy server starts its connection immediately. Runs started after the
approval response wait for that bounded connection and initial tool listing
before assembling their tool context.

Connection failures are isolated per server. A failed server publishes no tools
and does not prevent other servers from connecting.

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

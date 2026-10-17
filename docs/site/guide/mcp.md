# MCP [mcp]

Cookie Agent supports MCP tools through `tools/list` and `tools/call`. It does
not expose MCP resources or prompts. Server requests for sampling, elicitation,
roots, or other interactive input are rejected.

## Configuration

`mcp` accepts one optional `servers` map, empty by default. Define entries in
`~/.cookie-agent/config.toml`; this is a complete illustrative file:

```toml
[mcp.servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
cwd = "/workspace"
timeout_ms = 30000

[mcp.servers.remote]
url = "https://mcp.example.com/mcp"
oauth = true
```

Servers are configured only in the user file. A `<cwd>/.cookie-agent/config.toml`
is never read, so a repository cannot add MCP servers.

## Where servers run

An MCP server belongs to the [runtime](runtimes.md) that hosts it, and runs
on that runtime's machine:

- **Top-level `[mcp.servers.<name>]`** servers belong to the daemon's global
  runtime and run where the daemon runs. Every session that attaches
  `global` (the default) can use them. Only these servers can use OAuth, and
  only these can be added, edited, removed, or toggled from `/mcp`.
- **`[runtimes.<id>.mcp.servers.<name>]`** servers belong to a coding
  runtime template. Each instance of the template hosts its own copy on the
  instance's machine, for example a language server next to a repository on
  another host over ssh, and sessions attached to that instance can use
  them. `[runtimes.local.mcp.servers.<name>]` servers run in the daemon's
  built-in `local` coding runtime.

```toml
[runtimes.labpc]
command = ["ssh", "labpc", "cookie", "runtime", "serve", "--stdio"]

[runtimes.labpc.mcp.servers.rust_analyzer]
command = "rust-analyzer-mcp"

[runtimes.local.mcp.servers.docs]
command = "docs-mcp"
```

A runtime's servers use the same table schema and are parsed as strictly,
except that a template server that enables OAuth (`oauth = true` or an
`oauth` table) fails validation: OAuth credentials live in the daemon's
`mcp-oauth.json` and only the global runtime uses them. A runtime's remote
servers connect with their static `headers` only. A command, its `cwd`, and
its environment are resolved on the runtime's machine, and the process
inherits the runtime's environment plus `env`. Template servers change only
when the daemon restarts.

Each server must set exactly one of `command` or `url`. `args`, `env`, and `cwd`
apply only to stdio commands; `headers` applies only to Streamable HTTP. Servers
are enabled by default. `timeout_ms` bounds connection, listing, and calls and
defaults to 30000.

## Connection lifecycle

Every enabled server connects eagerly, in every runtime: there is no deferred
connection, and an authored `lazy` key is an unknown field.

- **At startup** every enabled server connects, in parallel: the global
  runtime's when the daemon starts, and a coding runtime's when its instance
  starts, before the instance answers the daemon (so the template's
  `startup_timeout_ms` must cover them). The first run waits for every
  server's initial connection and `tools/list` attempt to finish, so its tool
  context is not built from a partial startup snapshot. Each wait is bounded
  by that server's `timeout_ms`; one server's failure does not block the
  others.
- **A failed server** publishes no tools. It is retried when a session that
  uses its runtime instance is created or opened (and when the instance is
  attached to a session or retried from `/runtime`), and the session's next
  run waits for that attempt too. `r` in `/mcp` (or `mcp.server.reconnect`)
  retries it on demand.
- **A dropped server** (a stdio process that exits, a closed pipe, or an HTTP
  session the transport gives up on) is restarted by the runtime that hosts
  it up to 3 times, after 1 s, 2 s, and 4 s. While it reconnects it shows as `reconnecting`, its tools stay
  published so the model's tool list does not change, and calls fail fast with
  the `runtime_unavailable` failure code; calls are never retried for the
  model. A successful attempt restores the server with its current tool list.
  When every attempt fails, its tools are dropped and it becomes `failed`.

A failure at connect time and a drop that exhausts its reconnects reach every
client viewing a session that uses the server, that is, a session attached to
the instance that hosts it: the TUI shows an error row in each such session
naming the server, its runtime (and the instance's workspace), and the reason
(see [connection failures](../reference/protocol.md#connection-failures)).

## OAuth for remote servers

OAuth is available only to top-level `[mcp]` servers, which the daemon's
global runtime hosts; see [where servers run](#where-servers-run).
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
headers = { Authorization = "Bearer REPLACE_WITH_LOCAL_TOKEN" }
```

Omitting `oauth` is the default auto mode. `true` and `{}` enable the same
reactive flow. The settings object may contain `client_id`, `client_secret`,
`client_metadata_url`, and `scopes`. Cookie Agent follows protected-resource and
authorization-server discovery, uses S256 PKCE, and selects a pre-registered
client, a server-supported Client ID Metadata Document, or Dynamic Client
Registration in that order. A static `Authorization` header takes precedence
over OAuth when both are configured; other static headers are sent alongside
OAuth requests. OAuth is not used for stdio servers.

MCP environment values and headers do not support engine `${env:NAME}`
interpolation. Configure credentials locally, never in a committed example.
Names must be nonempty, at most 128 bytes, and contain no control characters.
`command` and an authored `cwd` must be nonempty. OAuth client IDs, secrets,
and scopes must be nonempty; a secret requires a client ID. Metadata URLs must
use HTTPS with a non-root path. Unknown fields and mixed transport fields fail,
including on disabled entries.

An authorization challenge changes the server state to `needs_auth`. Run
`cookie mcp --token <TOKEN> auth <server>` (or set `COOKIE_DAEMON_TOKEN`),
or select the server in `/mcp` and press `a`, then
open the displayed URL. The client, not the daemon, listens on an ephemeral
`127.0.0.1` callback port and hands the browser's redirect to the daemon, so a
daemon reached through an SSH tunnel or on another machine authorizes the same
way. When the browser runs on a different machine from the client, its
redirect cannot connect; copy the URL from the browser's address bar and paste
it into `/mcp` or the `cookie mcp auth` prompt instead. The daemon keeps the
flow for five minutes. The TUI can copy the URL with `c` and cancel the wait with
Escape. Successful authorization stores the token and reconnects the server.
Expired tokens refresh automatically. A rejected refresh or revoked token
returns the server to `needs_auth` instead of repeatedly opening a browser flow.
A transient refresh failure also returns to `needs_auth` immediately; Cookie
Agent does not silently retry token requests or open a browser flow.

OAuth failures show the available response body or authorization error description
in the MCP diagnostic, capped at 4,096 UTF-8 bytes. Content is preserved, including
credential-like text returned by the server; terminal and Unicode format controls
are stripped or replaced. This does not change credential storage or Debug formatting.

Only the authorization code crosses the client connection: the daemon keeps
the PKCE verifier, performs the token exchange, and stores and refreshes the
tokens. The code is single-use and useless without that verifier.

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

Run `/mcp` to open the live MCP panel. It lists the servers of every runtime,
grouped and labelled by the runtime instance that hosts them: `global` for
top-level `[mcp]` servers first, then each coding runtime instance as
`<runtime> at <workspace>`, with their source. The list reports `connected` with its
tool count, `connecting`, `reconnecting`, `failed` with the connection error,
`needs_auth`, and `disabled`. The panel refreshes on every MCP change and polls
the daemon while it is open. Select a server to inspect its complete command, arguments,
environment, and working directory, or URL and headers. Press `a` on a
`needs_auth` remote server to begin OAuth.

The panel can reconnect any runtime's server (`r`, which the daemon passes to
the runtime that hosts it), and toggle enablement and add, edit, or remove
definitions of global-runtime servers only; a coding runtime's servers come
from its runtime template. Add/edit forms accept either a command, JSON string array
of arguments, JSON string object of environment variables, and optional working
directory, or a URL plus a JSON string object of headers. Choose
`runtime only` or `user config.toml` before submitting.
Edits do not rename a server; remove the old name and add the new one instead.

Stdio servers negotiate MCP 2025-11-25 when needed. Servers that implement the
2026-07-28 discovery lifecycle use that version. Remote servers use Streamable
HTTP, including SSE responses on that endpoint; the retired two-endpoint legacy
SSE transport is not supported.

## Repository security

There is no separate MCP trust store or approval prompt. Every configured,
enabled server connects eagerly. MCP tools
are permission-gated like normal tools: an agent with no `mcp` permission entry
cannot see them, and scoped rules decide each call.

MCP servers and agent documents come only from your user configuration;
a repository's `.cookie-agent/config.toml` and `.cookie-agent/agents/` are never
read, so opening a repository cannot add a server or an agent that permits its
tools. Repository `AGENTS.md` files and project skills still enter model context;
review them in repositories you do not trust.

Connection failures are isolated per server. A failed server publishes no tools
and does not prevent other servers from connecting (see
[Connection lifecycle](#connection-lifecycle)).

Definitions created or changed through the TUI are runtime-layer entries and
live only for the current daemon unless explicitly written to a file. After a
write-back the file holds the entry, and it is no longer a runtime entry.

## Tool names and permissions

An MCP tool is exposed as `<server>_<tool>`. Both components replace characters
outside `[a-zA-Z0-9_-]` with `_`; there is no `mcp_` prefix. For example,
`github` plus `search/repos` becomes `github_search_repos`. Inside the global
runtime, a server whose generated name collides with another MCP or plugin
tool, or with the global `webfetch`, is rejected; inside a coding runtime, one
that collides with another of its MCP servers' tools or its built-in tools is
rejected the same way, and the built-in tools keep their names. A name that
collides with a tool of another runtime or of the base layer is published
with the runtime's attachment prefix instead, for example
`global_github_search_repos` or `labpc_api_rust_analyzer_hover`; see
[tool composition](../specs/runtimes.md#63-collisions).

```yaml
permissions:
  mcp:
    "github_*": allow
    github_delete_repo: deny
```

MCP availability is permission-driven. With no `mcp` entry, an agent receives
no MCP tools; deny-only `mcp` rules also hide them. Any `mcp` `allow` or `ask`
rule in the agent or overlay exposes the connected MCP tools at the action
level. Calls then use the complete generated tool name as their scoped
resource: more-specific patterns override broader patterns, and unmatched calls
are denied. Each delegated agent is evaluated against its own permission map.

Each MCP description and result is treated as untrusted server output.

Media blocks in tool results (images, audio, and embedded resource blobs) are
sniffed by content, gated on the model's capabilities and the provider's
delivery channel exactly like [media reads](../reference/tools.md#media-reads),
and retained as content-addressed attachments. Gated media fails the tool call
with an actionable error; malformed blocks degrade to inline notes. Blob
resources without a declared MIME type are retained under the sniffed type.
Results that would exceed the combined attachment budget keep what fits and
degrade the remainder to inline notes.

## Accepted fields

These are the complete fields under `[mcp.servers.<name>]`. File precedence is
described in [config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `command` | string | *(none)* | Stdio server executable. Exactly one of `command` or `url` is required. |
| `args` | array of strings | empty | Command arguments; valid only with `command`. |
| `env` | map of strings | empty | Command environment additions; valid only with `command`. |
| `cwd` | string | *(none)* | Child working directory; valid only with `command`. |
| `url` | string | *(none)* | Absolute HTTP or HTTPS Streamable HTTP endpoint. Exactly one of `url` or `command` is required. |
| `headers` | map of strings | empty | Static request headers; valid only with `url`. |
| `oauth` | boolean or table | auto | Streamable HTTP OAuth. Omit, set `true`, or use `{}` for reactive OAuth; set `false` to disable it. A table accepts optional `client_id`, `client_secret`, `client_metadata_url`, and `scopes`. A static `Authorization` header takes precedence. |
| `enabled` | boolean | `true` | Whether the server may connect. |
| `timeout_ms` | integer | `30000` | Positive timeout for connect, list, and call operations. |

## Runtime layers and persistence

The running engine adds a mutable `Runtime` layer for MCP server entries. It
wins over the user file and lasts for the daemon lifetime. Runtime removals are
tombstones: the named server is absent even when the user file defines it.
Runtime and user-file entries use the same connection lifecycle. Nothing is
written automatically.

Explicit MCP write-back (`mcp.server.persist`) replaces only
`[mcp.servers.<name>]` in the user `config.toml`, or removes it for a runtime
removal, using a format-preserving TOML document edit, as a
[configuration change](configuration.md#changes-while-the-daemon-runs). Unrelated
keys, tables, and comments are retained. The complete candidate, with the agent documents beside it, is
passed through the strict configuration loader before an atomic replacement;
an existing unknown field, type conflict, or malformed table fails without
changing the file. The replacement syncs the candidate before rename and syncs
the containing directory afterward. The newly staged replacement is owner-only;
the existing source path is not validated before replacement. The source file is re-read and
compared immediately before replacement, and a mismatch fails with a conflict
instead of overwriting the external edit. A modification landing between that
comparison and the rename itself cannot be detected — a narrow residual race
that only matters if another writer edits the file in the same instant; avoid
concurrent external edits while applying changes. A successful write applies
the configuration, and the server's entry comes from the file from then on.

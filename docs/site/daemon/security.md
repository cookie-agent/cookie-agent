# Security guarantees

cookie agent combines permission policy with platform-specific filesystem and
process controls. Permission rules decide whether a prepared tool call may run;
they are not an operating-system security boundary. Rules deny by default: an
unmatched action or resource is refused, and a tool whose action has no `allow`
or `ask` rule is not advertised to the model at all. In particular, a permitted
`bash` command is not parsed into filesystem operations and runs without syscall
filtering.

## Filesystem capabilities

The `read`, `write`, and `edit` tools resolve their filesystem target when the
call is prepared, before policy evaluation, and operate on that resolved
location when it executes. Relative paths are rooted at the workspace. Absolute
paths remain absolute and must be controlled with explicit permission patterns;
the filesystem capability layer does not confine them to the workspace.

These tools follow supported filesystem links. `write` and `edit` preserve a
link and modify its destination rather than replacing the link itself. If a
final link is dangling, `write` prepares the missing destination and creates
it, while `read` and `edit` fail because their destination must already exist.
Link resolution is bounded, so cycles and routes exceeding the bound fail
closed.

Only the route is bound, not the content. A file that changes while an approval
is pending is read, edited, or replaced as it is at execution, and a `write`
prepared for an absent path replaces a file created there in the meantime.

Permission matching is deliberately separate: it uses only the normalized
lexical path requested by the caller, even when that alias leads outside the
workspace. The resolved destination does not receive a second permission check.
See [Permissions](../extend/agents.md#resource-labels).

### Unix

Filesystem traversal is descriptor-anchored. Each path component is opened
relative to its parent descriptor with no-follow semantics, and links are
expanded in place: a relative link target is traversed from the directory that
holds the link, an absolute one restarts from the filesystem root. Preparation
keeps the target's parent directory open, and execution opens, creates, or
renames the target relative to that descriptor with no-follow semantics. A link
swapped into the route after preparation therefore cannot redirect the
operation, and a link planted at the target itself is never followed: reads
fail with `operation_changed` and writes replace the link. Directories missing
at preparation are created at execution without following links.

Writes stage the complete content in a temporary file beside the target, sync
it, rename it over the target, and sync the directory.

### Windows

Windows filesystem capabilities remain path-based. For symbolic links and
junctions that the backend supports, cookie agent follows the route to the
destination while preserving the alias for writes. Other reparse-point forms
fail closed. The backend:

- normalizes the requested native path and rejects unsafe components;
- resolves supported links and canonicalizes the existing portion of the path,
  then checks component-wise, case-insensitive containment in the prepared
  sandbox root;
- resolves the route again immediately before use and fails with
  `operation_changed` if it now leads to a different path; and
- stages complete file content beside the target and publishes it by renaming
  the staged file over the target with the superseding POSIX rename
  (`SetFileInformationByHandle`, `FileRenameInfoEx`), which re-points the name
  in one metadata transaction so a concurrent reader opening the path by name
  never finds it absent; where that rename is unavailable the publish falls back
  to `MoveFileExW` with write-through and replace-existing flags.

Containment is a capability constraint, not another permission decision. In
particular, a relative request remains constrained to its prepared sandbox root
even though permission policy would authorize an external destination through
the lexical alias. Absolute requests retain their existing native-root checks.
Support is limited to link and junction forms that this path-based backend can
resolve; unsupported reparse tags are rejected.

Windows does not provide the descriptor-anchored `openat` traversal used by the
Unix backend. A path or supported link can therefore change after the final
route check and before the path-based read or mutation; the check narrows this
window but does not eliminate it.

## Private state

On Unix, cookie agent creates new private-state directories with mode `0700` and
new files with mode `0600`. Store updates use `flock`-locked journals and atomic
publication. Existing paths are not checked for owner, mode, type, link count,
or symlinks; they are opened and used as-is.

On Windows, new state directories and files are created with a protected DACL
whose single access-control entry grants the current user full control.
Secure-store lock files use an exclusive whole-file `LockFileEx` lock, and
durable state replacement stages and flushes a private file before publishing it
with a superseding POSIX rename (`SetFileInformationByHandle`,
`FileRenameInfoEx` with replace-if-exists and POSIX semantics) followed by a
flush of the target's parent directory. That rename keeps the published name
resolvable for the whole replacement, so a concurrent reader always sees the old
file or the new one. Kernels and filesystems that reject the information class
fall back to `MoveFileExW` with replace-existing and write-through flags, which
is not atomic for concurrent opens by name. Existing
paths are not checked for owner, DACL shape, reparse points, object type, or hard
links; they are opened and used as-is.

Creation-time modes and DACLs protect newly created state from other ordinary
users. They do not protect state that already exists or is later replaced. A
local actor able to pre-create or replace a state path can redirect reads and
writes through a symlink or provide loose/foreign-owned storage; cookie agent
will use that path. Treat the parent storage location as trusted and remove
unexpected state before starting the daemon. Neither platform protects secrets
from privileged code such as `root`, `SYSTEM`, or an elevated administrator.

### Daemon authentication token

The daemon authenticates every connection, from clients and from dial-in
runtimes alike, with a constant-time compare of a bearer token. A runtime
presents it in the `Authorization` header of its WebSocket upgrade; a client
presents it there or in its `handshake` request, which is how a web page
authenticates, since browsers cannot set WebSocket headers. A client
connection that has not authenticated may send only `handshake`, is closed
after a wrong token, and is closed after 10 seconds without one. It binds IPv4
loopback unless `--listen` names another address; off loopback, serve TLS
(`--tls-cert`, `--tls-key`) or keep the listener on a private network, since
a plaintext `ws://` connection carries the token in the clear. Anyone holding
the token is a fully trusted client and may also offer a runtime.

**Web pages.** A request with a browser `Origin` header is refused unless
`[server] allowed_origins` lists that origin (see [Web clients](remote-access.md#web-clients)).
The default list is empty, so no web page can open a connection. Listing an
origin lets that site's pages try; they still need the token. With `"*"`, any
page in any browser on a machine that can reach the daemon can open a
connection, and the token is the only protection. A page served over HTTPS can
only connect to a `wss://` daemon. Runtimes never connect from a web page: a
request to `/runtime` with an `Origin` header is always refused.

The token is durable: it is `[server] token` in the user `config.toml`. When
the configuration has none, the daemon draws a fresh 32-byte token from the
operating system at its first start and writes it there, rewriting the file
owner-only (`0600` on Unix), so it stays the same across restarts and a
runtime that dials in can reconnect after one. The WebSocket subcommands
(`attach`, `connect`, `disconnect`, `mcp`) and `cookie runtime serve --daemon`
read it from the same file unless given `--token` / `COOKIE_DAEMON_TOKEN`.
Any process that can read your `config.toml` can therefore connect to the
daemon; keep the file private, and replace the token (delete the line and
restart the daemon) if it leaks. Plain `cookie` sends the token to whatever
listens at `[server] host` and `port` when it looks for a running daemon (see
[A daemon already running](persistent.md#a-daemon-already-running)), as
`cookie attach` with default settings does; it looks only when the token is
set. The daemon also prints the token in its first stdout line:

```text
daemon-ready {"url":"ws://127.0.0.1:<port>/ws","token":"<43-char base64url>"}
```

A supervisor that logs daemon stdout verbatim persists the token in its
logs, and a token pasted on a shell command line is visible in shell history.
Treat the ready line as secret and do not archive it.

### AGENTS.md context files

For root runs, repository-controlled `AGENTS.md` files are read automatically and
their text enters model context. They are instructions, not trusted policy, and
cannot grant permissions or bypass tool preparation. A malicious repository can
still influence model behavior within permissions. Review or disable AGENTS.md
context before running unfamiliar workspaces, avoid placing secrets in these
files, and use restrictive tool permissions. See
[AGENTS.md context](../extend/agents.md#agentsmd-context).

### Retained tool output

Tool-output truncation is a presentation bound, not redaction. The preview shown
to a model or user can omit bytes that remain in the content-addressed artifact
store, and those retained bytes may contain secrets. A session reads them with
`read_artifact(artifact="<digest>", stream=…)`, which needs no permission rule
and is always allowed: it resolves only artifacts stored in the session's own
tree, without visibility or revert checks. Digests are content-derived, not
random access tokens. Display clipping does not redact stored content. See the
[tool artifact lifecycle](../reference/tools.md#retained-tool-output).

### Media attachments

Media files read through `read` or returned by MCP servers are sniffed by
content and structurally validated before bytes are retained or sent to a
provider: images must parse as a complete PNG, JPEG, static WebP, or GIF
container whose header dimensions, pixel count, and frame count stay within
bounds; PDFs need a version header, an `%%EOF` marker near the end, a
`startxref` offset that lands on a cross-reference table or stream, and no
encryption dictionary; audio and video get frame-sync and container magic-byte
checks. Pixel data and PDF content streams are never decoded, so a file with a
valid container but corrupt compressed data is accepted and left to the
provider. A provider that rejects such a file fails the request, and because
history replays the attachment, later requests fail too until the session is
reverted or forked to before it.

Validation rejects malformed input, but it is not content screening: an image
or PDF can embed adversarial text intended for the model (visible prompt
injection). Media attachments are in the same trust class as file text the model
already reads; apply the same judgement about which files an agent is pointed
at.

## Process boundary

On Unix, Bash commands run in a new session so cancellation and timeout can kill
the process group. On Windows, Bash commands are assigned to a Job Object before
their initial thread resumes, and cancellation or drop kills the job so child
processes cannot escape during startup.

The process-group and Job Object controls bound process lifetime; they do not
restrict what an approved command can access. Windows commands do not run in an
AppContainer or restricted token and have no seccomp-like syscall filter. Treat
`bash` permission as authority to run the complete command with the cookie agent
process's user privileges.

## Web fetching

`webfetch` requests run with the cookie agent process's network access and
inherited proxy environment. Permission policy checks only the exact initial
URL, including its query string; redirect destinations are not re-checked, and
there is no SSRF protection, host or IP blocklist, or DNS pinning. A broad
pattern therefore authorizes requests to any reachable host, including local
network services — scope `webfetch` rules accordingly. See the
[tool reference](../reference/tools.md#webfetch).

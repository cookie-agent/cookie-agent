# Security guarantees

cookie agent combines permission policy with platform-specific filesystem and
process controls. Permission rules decide whether a prepared tool call may run;
they are not an operating-system security boundary. In particular, a permitted
`bash` command is not parsed into filesystem operations and runs without syscall
filtering.

## Filesystem capabilities

The `read`, `write`, and `edit` tools prepare a filesystem target before policy
evaluation and revalidate it before use. Relative paths are rooted at the
workspace. Absolute paths remain absolute and must be controlled with explicit
permission patterns; the filesystem capability layer does not confine them to
the workspace.

These tools follow supported filesystem links while binding both the requested
route and its resolved destination. `write` and `edit` preserve a link and
modify its destination rather than replacing the link itself. If a final link
is dangling, `write` prepares the missing destination and creates it, while
`read` and `edit` fail because their destination must already exist. Link
resolution is bounded, so cycles and routes exceeding the bound fail closed.
Revalidation reports a changed link, route component, or destination as
`operation_changed` instead of accepting a newly resolved operation. The
platform-specific validation-to-use guarantees are described below.

Permission matching is deliberately separate: it uses only the normalized
lexical path requested by the caller, even when that alias leads outside the
workspace. The resolved destination does not receive a second permission check.
See [Permissions](permissions.md#resource-labels).

### Linux

Filesystem traversal is descriptor-anchored. Each existing path component,
including each link object, is opened relative to a held parent descriptor with
no-follow semantics. A relative link target is traversed from that parent; an
absolute link target restarts from the filesystem root. cookie agent retains and
revalidates the descriptors and identities for the traversed route, the resolved
target identity, and prepared content before use. This avoids converting link
traversal into an unbound path lookup. Execution uses the pinned destination even
if an alias changes after the last route check; such a change cannot redirect
the file operation to a different destination.

Atomic writes stage content beside the resolved target. Linux uses atomic
no-replace publication for a prepared absent destination and atomic two-file
exchange for replacement, which lets cookie agent verify the displaced file and
roll back a changed target while leaving any alias link in place.

### Other Unix platforms

Traversal and reads use descriptor-anchored, no-follow validation. Link-object
pinning is implemented on Apple platforms, FreeBSD, and Android; other Unix
platforms reject links when that capability is unavailable. Expected-target
replacement and no-replace publication currently depend on Linux-specific rename
operations, so write preparation fails closed where those guarantees are
unavailable.

### Windows

Windows filesystem capabilities remain path-based. For symbolic links and
junctions that the backend supports, cookie agent follows the route to the
destination while preserving the alias for writes. Other reparse-point forms
fail closed. The backend:

- normalizes the requested native path and rejects unsafe components;
- resolves supported links and canonicalizes the existing portion of the path,
  then checks component-wise, case-insensitive containment in the prepared
  sandbox root;
- records and rechecks the supported link or junction route, and rejects other
  reparse points;
- records the target's volume and file identity plus a content digest, then
  repeats route, canonical-path, containment, reparse, identity, and content checks
  before use; and
- stages complete file content beside the target and publishes it with
  `MoveFileExW`, using write-through and replace-existing flags when replacing a
  file.

Containment is a capability constraint, not another permission decision. In
particular, a relative request remains constrained to its prepared sandbox root
even though permission policy would authorize an external destination through
the lexical alias. Absolute requests retain their existing native-root checks.
Support is limited to link and junction forms that can be resolved and
revalidated by this path-based backend; unsupported reparse tags are rejected.

Windows does not provide the descriptor-anchored `openat` traversal used by the
Unix backend. A path or supported link can therefore change after revalidation
and before the path-based read or mutation. Route, reparse, and canonical
revalidation narrow this validation-to-use window but do not eliminate it.
Windows also has no atomic two-file exchange in this backend, so replacement
cannot verify the displaced file and roll back with the Linux guarantee.

## Private state

On Unix, cookie agent creates new private-state directories with mode `0700` and
new files with mode `0600`. Store updates use `flock`-locked journals and atomic
publication. Existing paths are not checked for owner, mode, type, link count,
or symlinks; they are opened and used as-is.

On Windows, new state directories and files are created with a protected DACL
whose single access-control entry grants the current user full control.
Secure-store lock files use an exclusive whole-file `LockFileEx` lock, and
durable state replacement stages and flushes a private file before publishing it
with `MoveFileExW` using replace-existing and write-through flags. Existing
paths are not checked for owner, DACL shape, reparse points, object type, or hard
links; they are opened and used as-is.

Creation-time modes and DACLs protect newly created state from other ordinary
users. They do not protect state that already exists or is later replaced. A
local actor able to pre-create or replace a state path can redirect reads and
writes through a symlink or provide loose/foreign-owned storage; cookie agent
will use that path. Treat the parent storage location as trusted and remove
unexpected state before starting the daemon. Neither platform protects secrets
from privileged code such as `root`, `SYSTEM`, or an elevated administrator.

### AGENTS.md context files

For root runs, repository-controlled `AGENTS.md` files are read automatically and
their text enters model context. They are instructions, not trusted policy, and
cannot grant permissions or bypass tool preparation. A malicious repository can
still influence model behavior within permissions. Review or disable AGENTS.md
context before running unfamiliar workspaces, avoid placing secrets in these
files, and use restrictive tool permissions. See
[AGENTS.md context](agents.md#agentsmd-context).

### Retained tool output

Tool-output truncation is a presentation bound, not redaction. The preview shown
to a model or user can omit bytes that remain in the content-addressed artifact
store, and those retained bytes may contain secrets. `read_tool_result` can read
that content only for a visible call in the same session and remains subject to
the agent's protocol `Read` permission on the `tool_result:<uuid>` resource, but
it does not retroactively redact the artifact. See the
[tool artifact lifecycle](../reference/tools.md#retained-tool-output).

### Media attachments

Media files read through `read` or returned by MCP servers are sniffed by
content and strictly validated (full image decode with bounded memory, classic
cross-reference PDF structure, frame-sync and container magic-byte checks for
audio and video) before
bytes are retained or sent to a provider. Validation rejects malformed input,
but it is not content screening: an image or PDF can embed adversarial text
intended for the model (visible prompt injection). Media attachments are in the
same trust class as file text the model already reads; apply the same judgement
about which files an agent is pointed at.

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

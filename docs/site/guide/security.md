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

### Linux

Filesystem traversal is descriptor-anchored. Each existing path component is
opened relative to its parent with no-follow semantics, and the component chain,
target identity, and prepared content are revalidated before use. Atomic writes
stage content beside the target. Linux uses atomic no-replace publication for a
prepared absent target and atomic two-file exchange for replacement, which lets
cookie agent verify the displaced file and roll back a changed target.

### Other Unix platforms

Traversal and reads use the same descriptor-anchored, no-follow validation as
Linux. Expected-target replacement and no-replace publication currently depend
on Linux-specific rename operations, so write preparation fails closed where
those guarantees are unavailable.

### Windows

Windows filesystem capabilities are path-based. cookie agent:

- normalizes the requested native path and rejects unsafe components;
- canonicalizes the existing portion of the path and checks component-wise,
  case-insensitive containment in the prepared sandbox root;
- opens each existing component as a reparse point and rejects symlinks,
  junctions, and other reparse points;
- records the target's volume and file identity plus a content digest, then
  repeats canonical-path, containment, reparse, identity, and content checks
  before use; and
- stages complete file content beside the target and publishes it with
  `MoveFileExW`, using write-through and replace-existing flags when replacing a
  file.

Windows does not provide the descriptor-anchored `openat` traversal used by the
Unix backend. A path can therefore change after revalidation and before the
path-based read or mutation. Reparse checks and canonical revalidation narrow
this validation-to-use window but do not eliminate it. Windows also has no
atomic two-file exchange in this backend, so replacement cannot verify the
displaced file and roll back with the Linux guarantee.

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

### Retained tool output

Tool-output truncation is a presentation bound, not redaction. The preview shown
to a model or user can omit bytes that remain in the content-addressed artifact
store, and those retained bytes may contain secrets. `read_tool_result` can read
that content only for a visible call in the same session and remains subject to
the agent's protocol `Read` permission on the `tool_result:<uuid>` resource, but
it does not retroactively redact the artifact. See the
[tool artifact lifecycle](../reference/tools.md#retained-tool-output).

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

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

On Unix, private state directories and files are opened with owner-only modes
and checked for the expected owner, type, link count, and permissions. Store
updates use locked journals and atomic publication.

On Windows, state directories and files are created with a protected DACL whose
single access-control entry grants the current user full control. Existing
entries are rejected unless the owner, protected-DACL flag, access-control entry,
inheritance flags, and current-user SID match that shape. Reparse points,
directories opened as files, and multiply linked files are rejected. Secure-store
lock files use an exclusive whole-file `LockFileEx` lock, and durable state
replacement stages and flushes a private file before publishing it with
`MoveFileExW` using replace-existing and write-through flags.

The Windows DACL protects against other ordinary user accounts. It is not a
boundary against `SYSTEM` or an elevated administrator, who can take ownership
or replace the ACL and read the state. ACL setup and validation fail closed, but
they cannot protect secrets from privileged code on the same machine.

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

# Projects

A *project* is a named body of work the daemon keeps, such as a repository
you come back to across machines. A session may belong to one project, and
moves in or out of one at any time. A project carries:

- a **name** and an optional **emoji** clients show before it;
- **defaults** for a new session: the runtimes it attaches and the agent it
  starts with;
- its own **`AGENTS.md`**, instructions every session of the project gets;
- its **memories**, `project://<name>/…`, with an **overview** shown in full
  to its sessions.

## Sessions in a project

`session.create` takes a project; a session belongs to no project unless one
is chosen. When the creation names a project and no runtimes, the daemon
attaches the project's default runtimes, so every client gets them. The
default agent and preset are applied by the client to the new session's
selection, since a session is always created with one; `cookie --project
<name>` does so.

A session moves to another project, or out of any, with
`session.project.set`, also while it runs: the move takes effect at its next
run. Moving never changes the session's attachments or agent. A subagent
belongs to its root session's project and moves with it.

## What sessions of a project see

At every run of a root session in a project:

- the project's `AGENTS.md` comes first in the
  [AGENTS.md context](../extend/agents.md#agentsmd-context), before every
  workspace's files, so a workspace's own `AGENTS.md` has the last word;
- the [memory snapshot](../sessions/memory.md#what-the-model-sees) gains a
  `## Project <name>` section: the overview in full, then an index of the
  project's other memories. Other projects' memories stay searchable.

Subagents see neither: like the machine index, the project section is for
root runs.

## Memory

A project's memories live in `~/.cookie-agent/memory/projects/<name>/`; the
folder's name is the project's name. The daemon creates
`project://<name>/overview` empty with the project, and owns its
description. A memory can be written only to a project that exists, by an
agent or by you: projects are created from a client, never by writing memory.
Every memory folder that existed before projects did became a project, with
no defaults, the first time the daemon started.

## In the clients

- **The web client** groups its sessions sidebar by project, **No
  project** last; each header shows the project's emoji, its name, and its
  default runtimes as badges. Drag a session onto a header, or use its row's
  **⋮** › **Move to project…**, to move it. The project editor sets the name,
  emoji, defaults, and `AGENTS.md`, and New session starts from a project's
  defaults. See [Projects](../clients/web.md#sessions-and-chat) in the web
  guide.
- **The TUI** starts its bottom bar with the session's project while no
  run is active; click it, or choose `/project`, to move the session or
  pick the new session's project. The session picker filters by project
  with **Ctrl+P**. See [Projects](../clients/tui.md#projects) in the TUI
  guide.
- **The CLI** has `cookie project` and `cookie --project <name>`.

## Managing projects

`projects.list`, `project.create`, `project.update`, and `project.delete`
manage projects from any client; `cookie project` does from the command
line. Every change sends the revision it read, so two clients never overwrite
each other.

- **Rename** moves the memory folder with the project. Old
  `project://<old>/…` URLs in transcripts no longer resolve; sessions keep
  the project, and their next run shows the new name.
- **Delete** removes the project's sessions, its whole memory folder, its
  `AGENTS.md`, and its defaults. It is refused while a session of the project
  runs.

The project itself lives in `~/.cookie-agent/projects/<id>/`: `project.toml`
(name, emoji, defaults) and the optional `AGENTS.md`. See
[`cookie project`](../clients/cli.md#cookie-project),
[Projects](../reference/protocol.md#projects) in the protocol reference, and
the [projects spec](../specs/projects.md).

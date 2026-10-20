# Memory

Memory is what agents keep between sessions: notes about you, about each
agent's work, about each machine, and about each project. The daemon stores
memories as Markdown files with a short frontmatter, and agents read and write
them with tools, so what one session learns the next can use.

## Layers

| Layer | URL | For |
|---|---|---|
| `user` | `user://<category>` | you: preferences, conventions, how you like to work |
| `agent` | `agent://<agent>` | one agent's own notes about doing its job |
| `machine` | `machine://<machine>/<name>` | one [machine](machines.md): toolchains, paths, services |
| `project` | `project://<project>/<name>` | one project, wherever its checkouts live |

A project memory names what identifies the project across machines, such as
repository names, remote URLs, and `<machine>:<path>` places, so an agent can
search for it from any checkout. See
[Layers and files](../sessions/memory.md#layers-and-files).

## How agents use it

Memory comes with the `global` runtime, which new sessions attach. A run
starts with a snapshot of it after the `AGENTS.md` context: every `user`
memory, the run's agent's own, and an index of the memories of the machines
the session works on; project memories the agent searches for. The memory
tools,
`memory_search`, `memory_read`, `memory_write`, `memory_edit`, and
`memory_delete`, read and change memories by URL. Writes name the revision
they read, so a memory that changed meanwhile is never overwritten. See
[What the model sees](../sessions/memory.md#what-the-model-sees) and
[Tools](../reference/tools.md#memory-tools).

The `memory` and `memory_write` permission actions govern the tools, with the
memory URL as the resource. Without rules of its own, a primary agent may read
and write every memory, while a subagent may read every memory but write only
its own `agent://` memory. See
[Memory defaults](../extend/agents.md#memory-defaults).

## Where it lives

Memories are files under `~/.cookie-agent/memory/` on the daemon's machine,
one folder per layer, so you can read, edit, or version them like any other
files. `/memory` in the TUI and the web page, and `cookie memory`, list, edit,
create, and delete them through the daemon, and show files that are not valid
memories with the reason. See
[Managing memory](../sessions/memory.md#managing-memory) and the
[`[memory]` settings](../engine/memory.md).

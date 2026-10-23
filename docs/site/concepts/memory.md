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
| `project` | `project://<project>/<name>` | one [project](projects.md), wherever its checkouts live |

Every agent has its `agent://` memory from the start: the daemon makes one
file per agent, names the agent in its description and keywords, and keeps
those in line with the agent's document; the agent writes the body. An empty
one is not shown to the model. See
[Agent memory](../sessions/memory.md#agent-memory).

Project memories belong to a [project](projects.md): a project memory can be
written only to a project that exists, and every project has an
`project://<name>/overview` whose description the daemon owns. A project
memory names what identifies the project across machines, such as repository
names, remote URLs, and `<machine>:<path>` places, so an agent can search for
it from any checkout. See
[Layers and files](../sessions/memory.md#layers-and-files).

## How agents use it

Memory comes with the `global` runtime, which new sessions attach. A run
starts with a snapshot of it after the `AGENTS.md` context: every `user`
memory, the run's agent's own, and an index of the memories of the machines
the session works on. A session in a project also sees the project's overview
in full and an index of its other memories; other projects' memories the
agent searches for. The memory
tools,
`memory_search`, `memory_read`, `memory_write`, `memory_edit`, and
`memory_delete`, read and change memories by URL. A whole-memory write needs
the memory as the session last saw it, so one that changed meanwhile is never
silently overwritten. The memory related to the session is what the agent is
told to maintain: the system prompt asks it to keep memory small, accurate,
and current, a write that looks like an existing memory is refused unless the
agent says it is new, each kind of memory has a size limit, and every change
is a line in the transcript. See
[What the model sees](../sessions/memory.md#what-the-model-sees) and
[Tools](../reference/tools.md#memory-tools).

The `memory` and `memory_write` permission actions govern the tools, with the
memory URL as the resource. A root run whose agent names no memory rules
reads every memory and writes the memory related to its session (the
user's, its own agent file, and its session's machines and project); a
delegated run without them reads and writes only its own agent memory, and
internal agents get none. A declared rule replaces the default for its
action, so `memory: deny` turns memory off. The snapshot shows each memory
only as far as the `memory` rules allow. See [Memory permissions](../extend/agents.md#memory-permissions).

## Where it lives

Memories are files under `~/.cookie-agent/memory/` on the daemon's machine,
one folder per layer, so you can read, edit, or version them like any other
files. `/memory` in the TUI and the web page, and `cookie memory`, list, edit,
create, and delete them through the daemon, and show files that are not valid
memories with the reason. See
[Managing memory](../sessions/memory.md#managing-memory) and the
[`[memory]` settings](../engine/memory.md).

# Memory

Memory is what Cookie Agent keeps between sessions: Markdown notes that the
agent and you write, which later sessions see or find again. It belongs to the
`global` runtime, so a session has memory while `global` is attached;
detaching `global` turns it off for that session. Memory is gathered context:
reading or writing it never changes a session's attachments, runtimes, or
workspaces. The [memory spec](../specs/memory.md) has the full design.

## Layers and files

Each memory is one Markdown file with a short frontmatter, in
`~/.cookie-agent/memory` (the `memory` folder of the data root):

```text
memory/
  user/<category>.md            user://<category>
  agents/<agent>.md             agent://<agent>
  machines/<machine>/<name>.md  machine://<machine>/<name>
  projects/<project>/<name>.md  project://<project>/<name>
```

| Layer | About | Created by | Deleted by |
|---|---|---|---|
| `user` | You, in every task: profile, preferences, feedback, references | You only (the agent may edit existing categories) | You only; the agent's delete clears the body |
| `agent` | One agent's way of doing its job, such as what you want from reviews | The daemon, one for every agent ([Agent memory](#agent-memory)) | You; the daemon removes an empty one it made when its agent goes; the agent's delete clears the body |
| `machine` | One machine: OS quirks, toolchains, paths, rules such as "read-only unless asked" | The agent or you | The agent or you |
| `project` | One body of work: conventions, decisions, gotchas | The agent or you, in a [project](../concepts/projects.md) that exists; the daemon creates the project's `overview` | The agent or you; the folder goes with its project |

A project's memory folder is the project's: a `project://` write to a name
that is no project is refused, from an agent and from you alike, and the
refusal names the projects there are. Projects are created from a client
(`cookie project create`, the web client), which also creates
`project://<name>/overview` with an empty body. Like an agent file's, the
overview's description is the daemon's: ``Overview of the `<name>` project,
loaded in full into its sessions``; a write keeps it and says so. Renaming a
project moves its folder, and deleting it deletes the folder whole. Every
folder under `projects/` from before projects existed became a project the
first time the daemon started. See [Projects](../concepts/projects.md).

The first time the daemon starts without a memory folder, it creates the four
layer folders and the categories `profile`, `preferences`, `feedback`, and
`references` with empty bodies. Add a category by creating a file in `user/`.

```markdown
---
description: Build and test conventions for cookie-agent
keywords: [cargo, clippy, msrv, no-fail-fast, cookie-agent]
---
- Run `cargo test --no-fail-fast`; plain `cargo test` stops at the first
  failing crate.
```

`description` is required (one line, 1 to 200 characters); `keywords` is an
optional list of up to 32 search terms. Names are a lowercase letter or digit
followed by up to 63 lowercase letters, digits, or `-`; machine folders use
machine names. A file that breaks these rules, or is larger than 57344
bytes, is invalid: search and the snapshot skip it. Each kind of memory has
a size limit ([Memory [memory]](../engine/memory.md)) that writes keep to. You may edit the files with any
editor; a session that writes a memory you changed since it read it gets an
error naming the current revision instead of overwriting your edit.

## Agent memory

The daemon keeps one `agents/<agent>.md` for every agent of the shared agent
set and of every [preset](../extend/agents.md#agent-presets), internal agents
(`approval`, `compaction`, `title`) aside. It makes the missing ones at start
and after every configuration reload or `config.apply`, with an empty body,
and writes nothing when there is nothing to do.

The daemon owns two things in each file:

- **The description**: ``Memory of the `<agent>` agent: <description>``, with
  the agent document's description on one line and the whole cut to 200
  characters. When an ID is in the shared set and in presets, the shared
  document is used; otherwise the preset that comes first alphabetically.
  Changing the agent document's description rewrites it.
- **Two keywords**: the agent ID and the agent's mode (`primary`,
  `subagent`, or `all`; one keyword when the ID is the mode's name). You and
  the agent may add more; the daemon keeps them and puts its two back first
  when they are removed.

So a write of an agent's file, by the agent or by you, keeps the daemon's
description and keywords: `memory_write` and `memory_edit` say what they
kept, and `/memory` and `cookie memory edit` show the daemon's notice after
saving. The body is the agent's: it starts empty, and an empty one is left
out of the model's memory turn entirely.

When an agent leaves every agent set, its file is removed only if the daemon
made it and nobody touched it: the body is empty, and the description and
keywords are exactly the daemon's. A file with notes stays, and `/memory`
and `memory.list` mark it as belonging to no current agent.

## Managing memory

In the TUI, `/memory` lists every layer: the `user` categories, each agent's
file, and the machine and project memories grouped by machine and project,
with invalid files marked and, at the end, anything in the memory folder that
is not a memory, each with the reason. Enter opens the selected memory in
`$VISUAL` or `$EDITOR`, frontmatter included; saving writes it at the
revision it read. `n` creates a memory, starting from the URL of the selected
layer, machine, or project, and `d` deletes the selected one after you
confirm. Every action is also a button under the details. When the text you
saved is not a valid memory, `e` reopens it as you left it; when the memory
changed since you opened it, nothing is overwritten: `r` reloads the current
version into the editor, and `o` overwrites it with your edit. The panel
refreshes when a memory changes through the daemon, by you, another client,
or the agent.

From a terminal, `cookie memory` lists, edits, and deletes memories through
the daemon, so it works against a remote daemon as well (`--url` and
`--token`, or `[server]` in `config.toml`, as for `cookie config`):

```sh
cookie memory list                      # every layer, invalid files with the reason
cookie memory list project              # one layer: user, agent, machine, or project
cookie memory edit project://web/build  # in $VISUAL or $EDITOR; created when absent
cookie memory rm machine://labpc/gpu    # asks first; --yes does not
```

`edit` opens the whole file, frontmatter included, and saves it at the
revision it read. Text that is not a valid memory is offered again as you
left it; a memory that changed since you opened it is not overwritten, and
you can reload it and edit the current version. You may add and remove
`user://` categories and remove `agent://` files, which the agent cannot. An
`agent://` file can be created only for an agent that exists; a file whose
agent was removed or renamed stays and is listed as belonging to no current
agent. Your edits never change the snapshot of an open session; it sees them
when it next takes one.

## Machines

The `machine` layer is keyed by machine names, the computers runtimes run on,
which the daemon assigns on its own and you can rename, merge, and forget from
the Machines view of `/runtime` or with `cookie machine` (see
[Machines](../daemon/runtimes.md#machines)). A machine's memories live in
`memory/machines/<machine>/` and move with it when it is renamed or merged
into another; `machines.list` and `cookie machine list` count them.

## What the model sees

At the start of a run that has memory, the model gets a **snapshot** as one
user turn after the `AGENTS.md` turn: every `user` category, the run's
agent's own file, and an index of the memories of the machines the session's
attachments run on, each as far as the agent's `memory` rules (and the
session overlay's) allow for its URL: a memory they allow is shown, one
they ask for is listed by URL and description only, and one they deny or do
not match is left out and not counted. When nothing is left to show, there
is no memory turn at all; the tools stay offered. A root run of a session in a project also gets the
project's overview in full and an index of its other memories, newest first;
the overview counts toward `[memory] snapshot_user_bytes` and the index
shares `snapshot_index_bytes` with the machines', the project's own taking
its room first. An overview too long to show is listed in the index instead.
Outside a project, project memories are never shown; the model searches for
the project a task belongs to. An agent file with an empty body adds nothing. The [system prompt reference](../reference/system-prompt.md#memory-turn)
shows the exact text.

The snapshot is taken once and kept, so the provider's prompt cache survives
writes. It is taken again only at a run's start when the session has none
yet, after a compaction, when the session's machines, its project, its
project's name, the run's agent, or its `memory` rules changed (a session
overlay change included), or when the run gained or lost memory or write
access. Edits you make
to the files reach open sessions at those points, and new sessions at once.

## Tools

The `global` runtime offers five tools ([Tools](../reference/tools.md#memory_search)):

| Tool | Does |
|---|---|
| `memory_search` | Searches every memory by keywords, names, descriptions, and bodies, and shows each result as far as the `memory` rules for its URL allow |
| `memory_read` | Reads one memory with its revision |
| `memory_write` | Creates a memory, or replaces one at the revision it read; `keywords` may be left out, and `description` too for the agent's file, whose description the daemon sets |
| `memory_edit` | Replaces one occurrence of a text in a memory's body |
| `memory_delete` | Deletes a `machine://` or `project://` memory. A `user://` or `agent://` memory cannot be deleted: its body is cleared, and the file stays with its description and keywords |

## Permissions

The tools use two permission actions with the memory URL as resource:
`memory` for searching and reading, `memory_write` for writing, editing, and
deleting. Memory is opt-in: an agent document that names neither, the
built-in `default` agent included, has no memory tools and no snapshot.
`memory: allow` lets an agent read every memory, and `memory_write` rules let
it write; a session overlay rule grants them for one session tree. See
[Memory permissions](../extend/agents.md#memory-permissions).

```yaml
permissions:
  memory:
    "agent://reviewer": deny
    "*": allow
  memory_write:
    "user://*": ask
    "agent://self": allow
    "project://*": allow
```

`agent://self` in a rule stands for the run's own agent. An agent without a
`memory` rule gets neither the snapshot nor any memory tool, even with
`memory_write` rules.

## Subagents

A delegated run sees only its own agent's memory in its snapshot and may
write only its own `agent://<agent>` file, whatever its rules say; it can still
search and read every layer its `memory` rules allow. Only the root agent
decides what to keep about you, machines, and projects.

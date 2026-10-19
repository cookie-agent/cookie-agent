# Memory

Memory is what cookie agent keeps between sessions: Markdown notes that the
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
| `user` | You, in every task: profile, preferences, feedback, references | You only (the agent may edit existing categories) | You only |
| `agent` | One agent's way of doing its job, such as what you want from reviews | The agent, by its first write to its own file, or you | You only |
| `machine` | One machine: OS quirks, toolchains, paths, rules such as "read-only unless asked" | The agent or you | The agent or you |
| `project` | One body of work: conventions, decisions, gotchas | The agent or you; the first memory creates the project | The agent or you; the project goes with its last memory |

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
machine names. A file that breaks these rules, or is larger than
`[memory] max_file_bytes` ([Memory [memory]](../engine/memory.md)), is
invalid: search and the snapshot skip it. You may edit the files with any
editor; a session that writes a memory you changed since it read it gets an
error naming the current revision instead of overwriting your edit.

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

From a terminal, `cookie memory` lists, edits, and deletes memories through the daemon, so it
works against a remote daemon as well (`--url` and `--token`, or `[server]`
in `config.toml`, as for `cookie config`):

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

## What the model sees

At the start of a run that has memory, the model gets a **snapshot** as one
user turn after the `AGENTS.md` turn: every `user` category, the run's
agent's own file, and an index of the memories of the machines the session's
attachments run on. Project memories are never shown; the model searches for
the project a task belongs to. The [system prompt reference](../reference/system-prompt.md#memory-turn)
shows the exact text.

The snapshot is taken once and kept, so the provider's prompt cache survives
writes. It is taken again only at a run's start when the session has none
yet, after a compaction, when the session's machines or the run's agent
changed, or when the run gained or lost memory or write access. Edits you make
to the files reach open sessions at those points, and new sessions at once.

## Tools

The `global` runtime offers five tools ([Tools](tools.md#memory_search)):

| Tool | Does |
|---|---|
| `memory_search` | Searches every memory by keywords, names, descriptions, and bodies, and shows each result as far as the `memory` rules for its URL allow |
| `memory_read` | Reads one memory with its revision |
| `memory_write` | Creates a memory, or replaces one at the revision it read |
| `memory_edit` | Replaces one occurrence of a text in a memory's body |
| `memory_delete` | Deletes a `machine://` or `project://` memory |

## Permissions

The tools use two permission actions with the memory URL as resource:
`memory` for searching and reading, `memory_write` for writing, editing, and
deleting. An agent document that names neither gets defaults by mode:
`primary` and `all` agents may read and write all memory, `subagent` agents
may read all memory and write their own `agent://` file, and internal agents
get none. A declared key replaces the default, so `memory: deny` turns memory
off for an agent. See [Memory defaults](agents.md#memory-defaults).

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

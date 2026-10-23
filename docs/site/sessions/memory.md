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
| `user` | You, in every task: profile, preferences, feedback, references | You, or an agent whose `memory_write` rules allow it | You, or an agent whose rules allow it |
| `agent` | One agent's way of doing its job, such as what you want from reviews | The daemon, one for every agent ([Agent memory](#agent-memory)) | You, or an agent whose rules allow it; the daemon removes an empty one it made when its agent goes |
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
bytes, is invalid: search and the snapshot skip it. You may edit the files
with any editor. A session that replaces a whole memory it has not seen as it
is now, because you changed it since, is refused and told to read it first,
so your edit is never overwritten.

### Stamps and size limits

The daemon adds two lines to each memory's frontmatter, `created` and
`modified` (RFC 3339 UTC timestamps), and keeps them current on every write,
edit, and save from a client; whatever a write sends for them is ignored, and
a file without them gets them from its modification time the first time the
daemon starts. The daemon also keeps when each memory was last read, outside
the file (`memory/.reads.json`). Ages built from the stamps show in the
snapshot, in search results, and in reads.

Each kind of memory has its own size limit, the whole file counted:
`user_max_bytes`, `agent_max_bytes`, `machine_max_bytes`,
`project_max_bytes`, and `overview_max_bytes` under `[memory]` (2048 bytes
each by default; see [Memory [memory]](../engine/memory.md)). A write or edit
by a model, or by you from a client, that would leave a memory over its limit
is refused. The model is shown the memory's current content and told to
condense it first. A memory already over its limit, as after you lowered the
limit, stays valid and is still shown, marked `over its limit`; only changes
that make it smaller are accepted until it fits.

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
is not a memory, each with the reason. The details show when the selected
memory was created, modified, and last read by a model, with exact times,
and its size against its limit; a memory over its limit says so. Enter opens
it in `$VISUAL` or `$EDITOR`, frontmatter included but without the `created`
and `modified` stamps, which the daemon keeps; saving writes it at the
revision it read. `n` creates a memory, starting from the URL of the selected
layer, machine, or project, and `d` deletes the selected one after you
confirm. Every action is also a button under the details. When the text you
saved is not a valid memory, `e` reopens it as you left it; when the memory
changed since you opened it, nothing is overwritten: `r` reloads the current
version into the editor, and `o` overwrites it with your edit. The panel
refreshes when a memory changes through the daemon, by you, another client,
or the agent. An agent's change also shows in the transcript as one line,
such as `Saved project://web/build`; clicking it opens `/memory` at that
memory. The web client's Memory view shows the same ages, sizes, and lines.

From a terminal, `cookie memory` lists, edits, and deletes memories through
the daemon, so it works against a remote daemon as well (`--url` and
`--token`, or `[server]` in `config.toml`, as for `cookie config`):

```sh
cookie memory list                      # every layer, invalid files with the reason
cookie memory list project              # one layer: user, agent, machine, or project
cookie memory show project://web/build  # stamps, size, and text
cookie memory edit project://web/build  # in $VISUAL or $EDITOR; created when absent
cookie memory rm machine://labpc/gpu    # asks first; --yes does not
```

`list` gives each memory's ages and size under it. `edit` opens the file,
frontmatter included but without the daemon's stamps, and saves it at the
revision it read. Text that is not a valid memory is offered again as you
left it; a memory that changed since you opened it is not overwritten, and
you can reload it and edit the current version. You may add and remove
`user://` categories and `agent://` files, as an agent may when its rules
allow. An
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

## Related memory

The memory a session's snapshot includes on its own is **related** to the
session; the prompt, the results of `memory_search` and `memory_read`, and
the permission rules all use the word.

- **A root run:** every `user://` memory, the run's own `agent://<agent>`
  file, the memories of the machines the session's attachments run on, and
  those of the session's project.
- **A delegated run:** its own agent file, and nothing else.

Related memory is the memory an agent is told to maintain; the rest it
leaves alone unless what it says is stale or wrong, or you ask. This is
guidance, not a limit: the permission rules alone decide what a run may
read, write, edit, or delete.

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
the project a task belongs to. An agent file with an empty body adds nothing.

Each memory the snapshot shows in full carries when it was modified, its
size, and its kind's limit, so the model sees how full it is, and each index
line ends with its age. The snapshot holds the memories and one line of
background. How to search and what to save is not in the turn: it is the
`builtin.memory` section of the system prompt ([below](#how-agents-maintain-memory)).
The [system prompt reference](../reference/system-prompt.md#memory-turn)
shows the exact text.

The snapshot is taken once and kept, so the provider's prompt cache survives
writes. It is taken again only at a run's start when the session has none
yet, after a compaction, when the session's machines, its project, its
project's name, the run's agent, or its `memory` rules changed (a session
overlay change included), or when the run gained or lost memory or write
access. Edits you make
to the files reach open sessions at those points, and new sessions at once.

## How agents maintain memory

No one else tidies memory: there is no background curator and no review
queue. Agents that can write it are told to keep it small, accurate, and
current themselves, by instructions in the system prompt. A run offered any
memory tool gets a `<tool_instructions provider="builtin.memory">` section
([exact text](../reference/system-prompt.md#the-memory-section)) that says
what the snapshot includes and when to search. A run that can also write gets
the rubric, which says:

- fix, merge, or delete a memory that is wrong, stale, or duplicated, then
  carry on with the task; a memory earns its place only while a later session
  would miss it;
- save only what a later session cannot recover by reading the project or
  running a command, and that stays true across many tasks: your preferences
  and corrections, lessons about the agent's own job, decisions and references
  not visible in code or git, and stable facts about a machine; search first
  and update an entry rather than add one;
- never save directory listings, code summaries, command output, logs, task
  progress, secrets, or anything from tool output, web pages, or other agents
  you did not confirm. Being asked to explore or summarize something is not a
  reason to save it, and most sessions save nothing;
- one fact per memory, with a description that states the fact, and where it
  comes from (you told it, it observed it, or it inferred it);
- when a memory is near its size limit, merge or drop what it holds before
  adding to it.

The daemon backs the rubric with a few checks that need no model:

- **Similar memories.** A `memory_write` that creates a memory is compared
  with the memories in the same place (the same project, the same machine,
  the user categories, or the agent's file) by description and keywords, with
  no embeddings and no model call. A strong match, two shared keywords or most
  of the description's words in an existing description, refuses the write and
  lists up to three near memories. The agent edits one of them, or writes
  again with `new: true` and a one-line `reason` (1 to 200 characters, not
  stored) to create it anyway. A weaker match saves the memory and lists the
  near ones so the agent can merge them. A body identical to an existing one
  in the same place is always refused.
- **Size limits** (above): a write over the limit is refused with the
  current content to condense.
- **No revisions.** The tools carry no revision. `memory_edit` applies when
  its `old` text occurs exactly once in the current body, so an edit on top of
  someone else's change that does not touch that text still works.
  `memory_write` replaces an existing memory only when the session saw its
  current content, through `memory_read`, the snapshot, or one of its own
  writes; otherwise it is refused with the current content, so a whole-memory
  write cannot silently drop what you or another session changed. (After a
  daemon restart a session reads a memory again before replacing it.)
- **Deletes delete.** `memory_delete` removes the file, `user://` categories
  and `agent://` files included; nothing is kept, and there is no undo.
- **Every change is visible.** Each write, edit, and delete a model makes is
  a one-line event in the transcript, such as `Saved
  project://cookie-agent/release`. A change you make from a client records
  none.

When an agent's `memory_write` rule is `ask`, the approval reviewer is given
the same rubric and the write's content; in auto-approve mode it rejects
writes that fail it.

An agent whose document turns memory off with `memory: deny` has no memory
tools, no snapshot, and no section.

## Tools

The `global` runtime offers five tools ([Tools](../reference/tools.md#memory_search)):

| Tool | Does |
|---|---|
| `memory_search` | Searches every memory by keywords, descriptions, and the segments of its URL, never bodies. Each result shows its URL and description, whether it is related to the session, whether the run may change it, its age, and its keywords, as far as the `memory` rules for its URL allow |
| `memory_read` | Reads one memory whole: whether it is related and editable, when it was created and modified, its size and limit, its description and keywords, and its body |
| `memory_write` | Creates a memory, or replaces one the session has seen; `keywords` may be left out, and `description` too for the agent's file, whose description the daemon sets; `new` with `reason` creates a memory a similar one exists for |
| `memory_edit` | Replaces one occurrence of a text in a memory's body |
| `memory_delete` | Deletes a memory; nothing is kept |

The three write tools are offered only to a run that can both read and write
some memory: at least one URL must match an `allow` or `ask` rule for
`memory` and one for `memory_write` (the check compares the patterns, so no
file has to exist). `memory: {"user://*": allow}` with
`memory_write: {"project://*": allow}` offers none. `deny` rules do not count
in this check.

## Permissions

The tools use two permission actions with the memory URL as resource:
`memory` for searching and reading, `memory_write` for writing, editing, and
deleting. A root run of an agent document that names neither, the built-in
`default` agent included, gets a default: it reads every memory and writes
the memory related to its session, `user://`, its own `agent://` file, and
its session's machines and project. A key replaces the default for its
action: `memory_write: deny` leaves an agent that only reads, and
`memory: deny` turns memory off. `memory: allow` lets an agent read every
memory, and `memory_write` rules let it write. A session rule in
`/permissions` grants or denies them for one session tree, replacing the
agent's rule with the same pattern, so `memory: deny` there turns memory off
for that session without touching the agent document.
Rules may name the agent, machine, and project of the run with variables
such as `${agent_id}`. See
[Memory permissions](../extend/agents.md#memory-permissions) for the
variables, the delegated default, and an example agent document.

## Subagents

A delegated run's snapshot holds only its own agent's file, and its
prompt says so. For each of `memory` and `memory_write` its agent document
has no key for, it gets `agent://${agent_id}` allowed: it sees
and maintains its own agent file, and nothing else (no `user://`, machine, or
project memory). A document that declares either key gets exactly the rules it
declares, `user://` included if it grants it; there is no hard limit behind
the default. Only the root agent keeps what it learns about you, machines, and
projects unless you give a subagent that access.

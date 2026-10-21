# Projects

Status: **approved** (2026-10-21); phases 1 and 2 implemented 2026-10-21
([section 14](#14-implementation-notes)), phase 3 in progress. Builds on the
[memory spec](memory.md), the [runtimes spec](runtimes.md), and the
[web client spec](web-client.md).

## 1. Goal and scope

Today a project exists only as a memory folder,
`~/.cookie-agent/memory/projects/<project>/`. Nothing links it to sessions:
the model is told to search memory for the project a task belongs to. This
spec makes a project something the daemon knows and sessions belong to.

**Decided 2026-10-21:**

- A session may belong to one project. It is chosen at `session.create`,
  and the session can later move to another project or out of any project.
  Projects can be created, renamed, and deleted.
- A project carries default attachments, a default agent and agent
  preset, its own `AGENTS.md`, and its memory folder.
- Choosing a project in a client's New session prefills the runtime cards
  from the project's defaults, which stay editable. A `session.create` that
  names a project and omits `runtimes` gets the defaults from the daemon,
  so the TUI and the CLI get them too. The default agent and preset prefill
  the draft's agent selection in every client (added 2026-10-21,
  [section 4](#4-sessions-in-a-project)). Moving a session into a project
  never changes its attachments or its agent.
- Sessions in a project see its memory in the snapshot: the reserved
  `project://<name>/overview` in full, and an index (URL, description,
  keywords) of every other memory in the project.
- Deleting a project deletes its sessions, its memories, its `AGENTS.md`,
  and its defaults, after a confirmation that counts them.
- The web session list groups sessions under collapsible project sections,
  then "No project". The workspace filter goes away.
- Users can rename sessions from the TUI and the web client. The protocol
  already has `session.rename`; no client calls it yet.
- Web session rows show where a session runs as machine and runtime badges
  with stable colours ([section 9](#9-session-badges)).

Out of scope: nesting projects, a session in several projects, sharing a
project between daemons, and per-project permission rules.

## 2. Vocabulary

| Term | Meaning |
|---|---|
| Project | A named body of work the daemon keeps: an ID, a name, an optional emoji, default attachments, an `AGENTS.md`, and a memory folder. |
| Project ID | An opaque string the daemon generates. Sessions refer to a project by ID, so a rename never rewrites session logs. |
| Project name | The project's name in URLs, folders, and clients. It follows the memory name rule (memory spec section 4.2): a lowercase letter or digit, then up to 63 lowercase letters, digits, or `-`. |
| Overview | The memory `project://<name>/overview`, loaded in full into every session of the project. |

## 3. Storage

```text
~/.cookie-agent/
  projects/<project-id>/
    project.toml       name, created, default attachments
    AGENTS.md          optional
  memory/projects/<name>/
    overview.md        created empty with the project
    <memory>.md        …
```

- `project.toml`:

  ```toml
  name = "cookie-agent"
  emoji = "🍪"
  created = "2026-10-21T18:00:00Z"

  [defaults]
  preset = "rust"
  agent = "builder"
  global = true
  runtimes = [
    { runtime_id = "local", capability = "coding", workspace = "/home/u/src/cookie-agent" },
    { runtime_id = "labpc", workspace = "/home/u/cookie-agent" },
  ]
  ```

  `emoji` is optional. When set, it is exactly one emoji: a single
  extended grapheme cluster of at most 32 bytes that Unicode marks as an
  emoji. It cannot be ASCII, whitespace, or a control character.
  `global` and `runtimes` in `[defaults]` have the shape of
  `session.create`'s fields of the same names. `agent` and `preset` name an
  agent and its preset, as in `RunSelection`; both are optional, and an
  agent without a preset is one from the shared registry. A project with
  no `[defaults]` gets the daemon's usual defaults.
- Each project file carries a revision, as memory files do, and every
  change is compare-and-swap on it.
- **The memory folder stays where it is**, so `project://` URLs, the memory
  tools, and `memory.*` keep working. The project's name is its memory
  folder's name.
- **First start after the upgrade:** every valid folder under
  `memory/projects/` that has no project gets one, with no defaults and no
  `AGENTS.md`. Memory projects and daemon projects are then the same set.
- **Writing to an unknown project:** a model write to `project://<name>/…`
  for a name that is no project is refused. The message names the existing
  projects and says that projects are created from a client. Writes from
  the user's `memory.write` are refused the same way. Today the first write
  creates the folder.
- The daemon creates `overview.md` empty when it creates a project. Like
  agent memory files (memory spec section 16.7), its description is the
  daemon's: ``Overview of the `<name>` project, loaded in full into its
  sessions``. An empty overview renders nothing.

## 4. Sessions in a project

- `session.create` gains `project: Option<ProjectId>`. An unknown ID is
  refused before anything is attached.
- **Membership is event-sourced.** A new durable event,
  `session_project_set { project: Option<ProjectId> }`, records it. The
  creation writes one when `project` is set. `SessionMeta.project` is its
  fold, so a cache rebuild keeps it.
- **A session tree has one project.** Only root sessions record the event;
  delegated children take their root's project.
- `session.project.set { session_id, project: Option<ProjectId> }` moves a
  root session into a project or out of one, and is refused for a child.
  It is allowed while the session runs and takes effect at the next root
  admission. The move announces `sessions.changed` `upserted`.
- **Defaults at creation:** when `project` is set and `runtimes` is omitted,
  the daemon attaches the project's `[defaults]`. An explicit `runtimes`,
  even an empty one, wins.
- **Default agent.**
  - `session.create` always carries a `RunSelection`, so the daemon never
    replaces it. Clients apply the project's `agent` and `preset` when a
    project is chosen for a draft: the web project picker, the TUI picker,
    and `cookie --project`.
  - The model is the agent's own default model, as when the user switches
    agents. The selection stays editable before the first message.
  - If the agent no longer exists or cannot run as root, the client keeps
    its usual default and says so.
  - `project.update` refuses an agent the daemon does not list in that
    preset, or one that cannot run as root. An agent removed later
    produces no error until a client tries to use it.
- `session.list` gains `project: Option<ProjectFilter>`, which is a project
  ID or "none". A session whose project no longer exists lists as having
  none.

## 5. Project operations

New client requests (client protocol 1.8.0):

| Method | Params | Result |
|---|---|---|
| `projects.list` | — | `{ projects: [ProjectInfo] }` |
| `project.get` | `{ project }` | `{ project: ProjectInfo, agents_md: { text, revision }? }` |
| `project.create` | `{ name, emoji?, defaults? }` | `{ project: ProjectInfo }` |
| `project.update` | `{ project, revision, name?, emoji?, defaults?, agents_md? }` | `{ project: ProjectInfo }` |
| `project.delete` | `{ project, revision }` | `{ deleted_sessions, deleted_memories }` |

- `ProjectInfo` is `{ id, name, emoji?, revision, defaults?, has_agents_md, sessions, memories }`, where `sessions` counts root sessions.
- A `projects.changed` notification announces any change. Clients refetch
  `projects.list` on it.
- **Rename.**
  - The new name must be valid and free.
  - The daemon renames the memory folder, holding the memory lock, before it
    writes `project.toml`. If the folder move fails, nothing changes.
  - Old `project://<old>/…` URLs in transcripts no longer resolve.
  - Sessions keep the ID, so their logs are untouched. Their next snapshot
    shows the new name ([section 7](#7-memory-in-the-prompt)).
- **Delete.**
  - The daemon refuses while any session of the project is running, and
    names those sessions.
  - Otherwise it deletes every session tree of the project, as
    `session.delete` does, then the whole memory folder (non-memory files
    too, since the user asked for the project to go), then
    `projects/<id>/`.
  - Clients confirm first and show the counts from `ProjectInfo`.
- **Edits.** `agents_md` replaces the file, and an empty string removes
  it. `emoji` is set to a value or cleared with `null`. `defaults` is validated like `session.create`'s `runtimes`, except
  that the runtimes need not be connected.

## 6. Project `AGENTS.md`

- At every root admission, the daemon reads the project's `AGENTS.md`,
  alongside the workspace files the coding runtimes return. This needs no
  runtime: the file lives in the daemon's data directory.
- It comes first in the `AGENTS.md` turn, before every workspace's files:
  general before specific, so a workspace's `AGENTS.md` still has the last
  word. It renders as `<contents from="project:<name>/AGENTS.md">`.
- It follows `[agent_md] enabled` and an agent's `agent_md` frontmatter,
  as workspace files do. It does not need `global`.
- It is recorded in `agent_md_loaded`, with a new optional `project` source
  (an additive event field), so a replay renders what the model saw.
- The size cap is the same as for workspace files.

## 7. Memory in the prompt

- `memory_loaded` gains an optional `project` entry (additive):

  ```text
  project: {
    project: <name>,
    overview: { description, keywords, body }?,
    memories: [ { url, description, keywords } ],
    omitted: <count>
  }
  ```

- **Root runs of a session in a project** get it. Delegated runs do not,
  matching the machine index.
- The overview is whole, or absent when empty or missing. Its rendered size
  counts toward `[memory] snapshot_user_bytes`.
- The index lists every other memory in the project, newest first. It
  shares `[memory] snapshot_index_bytes` with the machine index.
- **A new snapshot is taken** (additions to memory spec section 7.2) when:
  - the session's project changed since the latest snapshot;
  - the project's name changed since then.

  Either costs one prompt-cache miss.
- **Rendered text.** A session in a project gets a section after
  "Machines in this session":

  ```text
  ## Project cookie-agent
  This session belongs to the cookie-agent project.

  <memory url="project://cookie-agent/overview" …>
  …overview body…
  </memory>

  Other memories of this project (read with memory_read):
  - project://cookie-agent/release: How releases are cut …
  ```

  The paragraph "Project notes are never shown here…" becomes: "This
  session's project notes are listed below. Search memory (memory_search)
  for other projects only when the task reaches into them." Sessions
  outside a project keep today's text.

## 8. Session titles

- No protocol change. Clients call `session.rename` with `Set`, `Clear`,
  or `Reset`.
- **TUI:**
  - `/rename <title>` sets the title.
  - `/rename` with no argument opens an input prefilled with the current
    title.
  - `/rename --auto` resets it to the generated title.
  - In the session picker, `Ctrl+R` renames the highlighted session.
- **Web:**
  - Clicking the title in the title bar edits it in place. Enter saves and
    Escape cancels.
  - ⋮ menu › Rename does the same.
  - **Session rows.** A pencil button sits to the left of the delete
    button. Both show on hover on desktop and on long-press on a phone,
    with the delete button's layout (centred, the row's hover colour at
    rest, one step darker on hover). The pencil turns the row's title into
    a field in place; Enter saves and Escape cancels.
  - An emptied field offers "Use the automatic title" (`Reset`).

## 9. Session badges

Web session rows replace the folder-and-paths line with badges. The
`global` attachment is ignored for all of these rules:

| Session's attachments | Badges |
|---|---|
| None besides `global` | One grey "no workspace" badge |
| One | `[runtime]` badge, then the workspace as `~/…` |
| Several, all on one machine | `[machine]` badge, then one `[runtime]` badge per attachment |
| On more than one machine | One `[machine]` badge per machine, in attachment order |

- The machine comes from `runtimes.list`, via the attachment's runtime's
  `machine`. A runtime with no known machine counts as its own machine and
  shows its runtime badge.
- **Colours are stable.** Each badge takes one of eight hues, chosen by a
  hash of the machine name or runtime ID. Each hue has a light-theme pair
  and a dark-theme pair that meet contrast. The same runtime is the same
  colour everywhere: rows, cards, and the bottom bar.
- **Machine badges** carry the machine's OS icon, as on the Machines view.
  **Runtime badges** carry the runtime ID, with the capability only when the
  runtime has several capabilities.
- Badges truncate before the time does. The row's tooltip keeps the full
  list with paths.

## 10. Clients

### 10.1 Web

- **Sidebar.**
  - Project sections are collapsible and remembered per browser.
  - Projects are ordered by their latest session's activity, and "No
    project" comes last.
  - Each header shows the project's emoji (a folder icon when it has
    none) and its name, with no count, plus:
    - `+` for a new session in the project, always visible;
    - a pencil, then a delete button, shown like the session rows' buttons
      (on hover on desktop, on long-press on a phone).
  - The pencil opens the project editor. Delete asks for confirmation,
    with the counts of sessions and memories that go
    ([section 5](#5-project-operations)).
  - "No project" has neither button.
- **Moving sessions.** Drag a session row onto a project header. This uses
  the pointer-event drag of the Machines view: long-press then drag on
  touch. For keyboards, a third row button, "Move to project…", sits left
  of the pencil and opens a project picker. On a phone, a long press
  released without moving shows the three row buttons.
- **New session.**
  - A project picker sits above the runtime cards. It starts at the section
    whose `+` was used, else "No project".
  - Picking a project prefills the cards from its defaults, and the agent
    chip from its default agent and preset.
  - A "New project…" entry opens the project editor, empty.
- **Project editor.** A right-side panel on desktop, the same width as
  the model picker's, and a full-height sheet on a phone. It holds:
  - name;
  - emoji, chosen with an emoji picker
    ([section 10.3](#103-emoji-picker));
  - default attachments, edited with the same runtime cards;
  - default agent and preset, chosen with the agent picker, or "None";
  - an "Use this session's attachments" shortcut;
  - an `AGENTS.md` editor;
  - a link to the project's folder in the Memory view.

  Save sends one `project.update` with the editor's revision. If the
  project changed meanwhile, the editor says so and offers to reload it.
  Closing with unsaved edits asks first. The "New project…" entry opens the
  same editor, empty, and saves with `project.create`.
- **Memory view.** Project folders and projects are the same set. A folder's
  "Delete project" becomes `project.delete`, with the session count in its
  confirmation.

### 10.2 TUI and CLI

- Wherever the TUI names a project (the bottom bar, the picker, session
  rows), the emoji comes before the name. Its width is measured as two
  cells.

- **Session picker:**
  - Rows show the project.
  - `Ctrl+P` cycles the filter: all, a project, then no project. This sits
    beside the existing `Ctrl+A` directory scope.
- **Project in the bottom bar.**
  - The bottom-left label starts with the project's name, before the
    runtime and workspaces, whenever no run is active. That covers both a
    loaded idle session and a new draft; a draft with no project shows
    "no project".
  - Clicking the name opens a project picker: the projects, "No project",
    and "New project…".
  - For a loaded session, the picker moves the session with
    `session.project.set`. Its attachments never change.
  - For a draft, the picker sets the project the session will be created
    in. If that project has defaults, the draft takes them, and the label
    shows their workspaces before anything is sent. Otherwise the draft
    keeps the TUI's usual attachments. The draft also takes the project's
    default agent and preset, if set.
- `/project` opens the same picker from the keyboard.
- **Starting in a project.** A new session belongs to no project unless one
  is chosen. `cookie --project <name>` starts in that project, and nothing
  is ever picked from the current directory.
- **`cookie project`** has `list`, `create <name>`, `rename <old> <new>`,
  `delete <name>` (with a confirmation unless `--yes`), and `edit <name>`,
  which opens `AGENTS.md` in `$EDITOR`. Defaults are set from the web
  client, or with `cookie project defaults <name> --from-session <id>`.

### 10.3 Emoji picker

- The web client has its own picker, with no JavaScript library. It shows
  a grid of a few hundred common emoji, bundled as a static list and
  grouped by category, with a search box that filters by each emoji's
  Unicode name.
- A free-text field also takes any single emoji, so the OS emoji keyboard
  works too, including on phones. The daemon validates either path the
  same way.
- "No emoji" clears it.
- On a phone the picker is a bottom sheet.
- The bundled list must keep the web bundle well under its 10 MB budget.
  It is names and code points only, with no images, and emoji render with
  the system font.

## 11. Protocol and compatibility

- **Client protocol 1.8.0** (additive):
  - `session.create.project`, `SessionMeta.project`,
    `session.list.project`;
  - `session.project.set`, the `project.*` methods (with the project
    emoji), `projects.list`, and `projects.changed`.
- **Events** (additive):
  - the `session_project_set` event;
  - `memory_loaded.project`;
  - `agent_md_loaded`'s project source.

  These need a plugin protocol minor, since plugins see events. The
  history format stays 1, and older logs read as having no project.
- **Clients below 1.8** see sessions without projects and cannot manage
  them. A project's `AGENTS.md` and memory still reach those sessions'
  prompts, because the daemon renders them.

## 12. Phases

1. **Store and protocol:**
   - projects on disk and the migration from memory folders;
   - the `project.*` methods and `projects.list`;
   - `session_project_set`, `session.project.set`, and defaults at
     creation;
   - delete;
   - refusing writes to unknown projects.
2. **Prompt:** the project `AGENTS.md`, the overview, the project index,
   and the snapshot triggers.
3. **Clients:**
   - web sections, moving sessions, the project picker and project editor
     with the emoji picker, badges, and rename;
   - TUI rename, `/project`, and the picker filter;
   - `cookie project` and `--project`.
4. **Docs:** a projects concept page, the memory and AGENTS.md pages, the
   web and TUI guides, and the protocol changelog.

## 13. Open questions

None. The user approved the spec on 2026-10-21, including the choices
first proposed with it:
- an ID plus a name that follows the memory name rule;
- the memory folder staying where it is;
- refusing writes to unknown projects;
- the place of the project's `AGENTS.md`;
- root runs only;
- refusing a delete while a session runs;
- hashed badge colours.

## 14. Implementation notes

### 14.1 Phases 1 and 2: store, protocol, and prompt (2026-10-21)

Landed: the project store and the upgrade from memory folders, the
`project.*` methods and `projects.list`, `projects.changed`,
`session_project_set` and `session.project.set`, defaults at creation,
delete, the refusal of writes to unknown projects, the project `AGENTS.md`,
the overview and the project index in the snapshot, the snapshot triggers,
and, from phase 3, `cookie project` and `cookie --project`. Client protocol
1.8.0, plugin protocol 1.3.0, history format 1. The web client and the TUI
pickers are phase 3.

Where it is: `crates/engine/src/projects.rs` (`ProjectStore`),
`crates/engine/src/memory/projects.rs` (the memory folder and the overview),
`crates/engine/src/runtime/project_api.rs` (the requests, membership, and the
project's `AGENTS.md`), `crates/protocol/src/project.rs` (the wire types),
and `crates/cookie_agent/src/project_command.rs`.

Decisions taken where the spec left room, and deviations:

- **Revision.** A project's revision is the first 16 hex digits of the
  SHA-256 of `project.toml`, then, when it exists, of `AGENTS.md`, so an
  edit of either file by hand, or by another client, makes a stale
  `project.update` fail. `project.get`'s `agents_md.revision` is the file's
  own digest, for display.
- **The upgrade runs at every start**, not only the first: it adopts every
  valid folder under `memory/projects/` that has no project, and it is
  idempotent. While a project folder does not read, it adopts nothing, and
  `project.create` refuses, since that project could have the name. It also
  makes sure every project's overview exists with the daemon's description,
  which is how a rename's new description lands even after a crash.
- **Defaults at creation** apply when the project's defaults name runtimes
  or leave out `global`. Defaults that only name an agent or a preset leave
  a new session the usual attachments, so adding a default agent never
  takes the `local` runtime away. `project.update` clears the defaults or
  the emoji with `null`.
- **The overview's description** is the daemon's, as an agent file's is;
  its keywords are the writer's. `memory_write` may leave the description
  out for an overview, and `memory_edit` keeps the daemon's.
- **Refused writes** cover `memory_edit` as well as `memory_write` and
  `memory.write`; deletes of an unknown project's files stay allowed. A
  project's folder now stays when its last memory goes: the folder belongs
  to the project. The model-facing tool descriptions say so.
- **`agent_md_loaded`'s project source** is `project: { id, name }`; the
  event's `attachment`, a required member older readers rely on, is
  `global`, where the file lives. The rendered `<contents>` of a project
  file has no `runtime` attribute, and adding a project never adds one to
  a lone workspace's files.
- **Snapshot limits.** The overview is measured against
  `snapshot_user_bytes` before the user categories and the project index
  against `snapshot_index_bytes` before the machines: the session's own
  project matters most. An overview too long for the user limit is listed
  first in the index instead of being dropped silently. The snapshot
  compares the project by name, which is all the event records; a session
  whose project was deleted is deleted with it.
- **`project.delete`'s result** is two counts: root sessions deleted, each
  with its tree, and memory files deleted. Delete, and `ProjectInfo.sessions`,
  reach the sessions of the daemon's own session store, the sessions of its
  working directory; a second daemon sharing the data root with another
  working directory keeps its sessions, which then list as having no
  project.
- **`SessionMeta.project`** of a delegated session is its root's, filled in
  when the daemon serves `session.get` and `session.list`; delegated
  sessions record nothing. `sessions.changed` carries the metadata as
  stored.
- **`ProjectFilter`** spells "no project" as the string `none`. Project IDs
  are UUIDv7 strings, so no project has that ID.
- **Emoji validation** uses `unicode-segmentation` and `unicode-properties`
  (both already in the dependency tree): one extended grapheme cluster, at
  most 32 bytes, not all ASCII, no whitespace or control character, made of
  characters with `Emoji` or `Emoji_Component`, at least one of them
  `Emoji`. A project file whose emoji does not pass reads as unreadable.
- **`cookie --project`** works with plain `cookie`, through a running daemon
  or standalone: the TUI starts on a new session in the project, with the
  default agent and preset when they resolve; the project applies to that
  first session only. `cookie project defaults --from-session` takes the
  session's agent and preset as well as its attachments.
- **Corpora.** The 1.8.0 client corpus and the 1.3.0 runtime corpus are
  recorded and pinned, with the session created in a project. The recorder
  now writes the runtime's host name, the machine name derived from it, and
  its home directory as placeholders.

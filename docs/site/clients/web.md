# Web

`cookie daemon` serves a web client on the same port as its client endpoint.
Open it in a browser on the daemon's machine, or on a laptop or phone that
reaches that port (see [Remote access](../daemon/remote-access.md)). It does what the TUI does with the daemon's sessions and
the daemon itself: chat, approvals, subagents, history edits, goals,
attachments, skills, permissions, runtimes and machines, MCP servers,
providers, the configuration, memory, and plugins. Two things stay in the
TUI and the CLI; see [What the page does not do](#what-the-page-does-not-do).

## Opening the page

The daemon takes the page's connection from any origin, and the token is the
guard (see [Web clients](../daemon/remote-access.md#web-clients)). Print the
page's address with the token:

```sh
cookie web          # prints http://127.0.0.1:7419/#token=<token>
cookie web --open   # also opens it in the system browser
```

`cookie web` finds the daemon like `cookie attach`:

- `--url` and `--token` (or `COOKIE_DAEMON_TOKEN`) first;
- then `[server] url` and `[server] token` in this machine's `config.toml`.

The page is the client URL with `ws` turned into `http` (`wss` into
`https`) and the final `/ws` dropped. The daemon also prints the page's
address, without the token, after its ready line.

The token travels in the address's fragment (`#token=`), which a browser
never sends to a server. The page keeps the token in the browser's
`localStorage` under the daemon's URL and removes the fragment from the
address bar.

## The connect screen

- **Daemon URL.** The page fills it in from its own address:
  - `http` becomes `ws`, and `https` becomes `wss`;
  - the host and port stay the same;
  - `ws` is added to the page's directory.

  You can edit it; the page remembers the last URL it connected to.
- **Token.** It comes from a `cookie web` link, or you paste it from the
  daemon's ready line.
- **Connecting.** With both a URL and a token, the page connects without a
  click.
- **Forget this daemon** deletes the stored token. Once connected, the
  same button is in the connection panel (below), behind a confirmation.
- **A refused connection.** A browser shows a page no reason when the daemon
  refuses or closes its connection before the handshake, or nothing answers.
  The page says so and asks you to check the daemon URL and that the daemon
  is running.
- **Once connected**, the foot of the rail (see [Layout](#layout)) shows
  the connection's state: connected, connecting, reconnecting, or offline.
  Hover over it for the daemon's address and version, or click it for a
  panel with them and **Forget this daemon**. Settings › This browser has
  **Forget this daemon** too.
  - **Dropped connection.** The page redials with the TUI's backoff,
    500 ms rising to 10 s, under the same client identity.
  - **Background tab.** When it becomes visible again, the page checks the
    connection.
  - **Separate clients.** Each browser tab is its own client.
- **Theme.** The theme button at the foot of the rail (in the header of
  the connect screen) cycles through system (a half circle), light (a sun),
  and dark (a moon). The choice stays in the browser.

## Layout

- **The rail.** An icon strip on the far left switches between five views;
  each icon names itself in a tooltip, and the view shown is marked by a
  bar at the rail's edge:
  - **Chat** (the default): the sessions sidebar and the transcript;
  - **Agents**: the agent documents and their presets, in a sidebar beside
    the one open;
  - **Settings**: `config.toml`, providers, MCP servers, plugins, and this
    browser's preferences;
  - **Machines & runtimes**: the daemon's machines and its runtime
    registry;
  - **Memory**.

  The connection and the theme button are at its foot.
- **Chat.** The title bar has the sidebar button, the session's title, and
  the ⋮ menu of the session's commands. Click the title to rename the
  session (see [Renaming](#sessions-and-chat)). Below the transcript are
  the composer and the bottom bar. While a subagent's session is open, the
  title is a path from its root, such as
  `Fix the greeting › reviewer: check tests`: click a parent to go up to
  it. The ⋮ menu acts on the session shown.
- **Widths.**
  - **1024 px and wider:** the sessions sidebar is pinned beside the
    transcript, and the agents beside the open agent. The sidebar button
    hides and shows it, and the browser remembers the choice (one for
    both views).
  - **640 to 1023 px:** the rail stays; the sidebar button slides the
    sessions over the transcript.
  - **Narrower, a phone:** see [Phones](#phones).
- **Sheets.** The session's panels open as a sheet on the right (from the
  bottom on a phone): approvals, the agent, model, permission mode, and
  preset pickers, usage, the diagnostics filter, compacting, the goal,
  background calls, skills, permission rules, a message's actions, the
  session's runtimes, and the ⋮ menu.
- **Long lines** in code, diffs, and terminal output scroll inside their
  block.

## Sessions and chat

- **Paths** show under the home directory of the runtime they belong to,
  as `~/web`, and absolute when that runtime reported no home (a daemon
  older than client protocol 1.7 reports none).
- **Sessions.** The page has no directory of its own, so the list shows
  every session, grouped by [project](../concepts/projects.md) (below). A
  daemon older than client protocol 1.8 keeps no projects: its list has a
  filter by the session's directory (its first workspace) instead, named
  by its runtime, such as `labpc · ~/api`.
  - **Rows.** Each session shows its title, how long ago it was last
    active beside it, and where it runs as badges on the line below.
    In each project, active sessions come first (those that run, or wait
    on something that will start their next run: an active goal, a
    background call, a subagent, or a plugin or runtime), then the others,
    each newest first. A session that runs has a turning circle before its
    title, one that waits has a clock (its tooltip says what it waits on,
    such as `waiting on: goal, background call`), and one whose last run
    failed or was interrupted has a mark; the others have none. A daemon
    older than client protocol 1.9 does not say what a session waits on.
    The open session's row has a chevron before its badges, which turns
    with its subagent tree. Hover over a row for its status and every
    workspace with its machine, runtime, and path.
  - **Badges** show machines, not runtimes. The `global` attachment never
    counts:
    - nothing else: no badge;
    - one runtime: its machine's badge, then its workspace, as `~/web`;
    - several, all on one machine: the machine's badge, then the
      workspaces, separated by commas;
    - more than one machine: a badge for each machine, in the order the
      session attached them.

    A root session's badges also count where its subagents work: the
    runtimes and workspaces they were given, after the root's own (see
    [Delegating to runtimes](../sessions/delegation.md#badges)). A root
    with only `global` whose subagents work on `labpc` shows `labpc` and
    their workspace. A subagent's row shows its own attachments.

    A machine's badge has its operating system's icon, as on the Machines
    view. A runtime whose machine the daemon does not know counts as a
    machine of its own, under its runtime ID. Each machine keeps one of
    eight colours, chosen from its name, so it looks the same in every row
    and every browser; the dots marking runtimes in the bottom bar and on
    the runtime cards take their machine's colour. The badges have the
    row's second line to themselves. A machine's name is never cut: when
    the line is too narrow, the workspaces shrink first, down to `…`, then
    give way, and then the last machines give way to a **+N** badge whose tooltip names
    them. The row's tooltip lists every
    workspace with its machine, runtime, and path. A daemon older than
    client protocol 1.7 lists no attachments: the row shows the session's
    directory instead.
  - **New session** opens on the composer with only `global` attached
    (below); the first message creates the session with the runtimes the
    bottom bar's **Runtimes…** chip shows and the agent, model, and
    permission mode beside it.
  - **Owned elsewhere.** A session another cookie process owns opens as a
    read-only snapshot.
  - **Renaming.** Click the title in the title bar, or choose ⋮ ›
    **Rename**, and the title becomes a field holding it: **Enter** saves
    and **Escape** (or clicking elsewhere) keeps the old title. In the
    list, hover over a row for its pencil, beside the trash, which does
    the same with the row's title. Empty the field for **Use the automatic
    title**, which goes back to the title the daemon generates (as does
    Enter on an empty field). Every client sees the new title at once.
  - **Deleting.** Hover over a session's row for its trash button (on a
    touch screen, press the row for about half a second and let go; the
    press never opens the session, scrolling cancels it, and it offers
    **Move** and **Rename** too). A confirmation in the row
    names the session; **Delete** removes it and its subagents from the
    daemon's disk, for good. The daemon refuses while the session tree
    runs, while another cookie process holds it, or when it changed since
    the list showed it; the row then shows the daemon's reason. A deleted
    session leaves every client's list, and a page showing it starts a new
    session. A daemon older than client protocol 1.5 cannot delete, and
    the page offers no trash.
- **Projects.** The sessions sidebar has a section for each project,
  ordered by its latest session's activity, then **No project** last.
  - **Headers.** A project's header shows its emoji (a folder when it has
    none) and its name; when its defaults attach runtimes, a second line
    shows them as badges, by the session rows' rules. Click a header to
    fold or unfold its section; the browser remembers it. **+**, always
    there, starts a new session in the project. Hover over a header (on a
    touch screen, press it for about half a second) for its pencil, which
    opens the project editor, and its trash. **No project** has only its
    **+**. The folder button beside **New session** makes a new project.
  - **Moving a session.** Drag its row onto a project's header, or onto
    **No project**: a mouse drags at once, a finger after pressing the row
    for half a second (the list scrolls when the row nears its edge). Or
    hover over the row for **Move to project…**, left of its pencil, which
    lists the projects and **No project**. The session keeps its runtimes
    and agent; its next message sees the project's memory and `AGENTS.md`.
  - **Deleting a project** asks first, saying how many sessions and
    memories go with it; the daemon refuses while one of its sessions
    runs, and the header names the running sessions.
  - **The project editor** opens on the right (filling a phone's screen):
    the name; the emoji; the runtime cards a new session in the project
    starts with, as on New session, with **Use this session's
    attachments**; the default agent, with its preset, or **None**; the
    project's `AGENTS.md`; and a way to its memory folder. **Save** sends
    the whole change at once; if the project changed meanwhile, in another
    client or on disk, the editor says so and offers **Reload**, which
    drops your edit. Closing with unsaved edits asks first.
  - **Emoji.** The picker lists a few hundred common emoji by category,
    with a search by name; the field below it takes any one emoji, from
    your system's emoji keyboard too, and says at once when what you typed
    is not one. **No emoji** clears it. On a phone the picker rises from
    the bottom.
- **New session.** The composer opens at once, with only `global`
  attached: such a session has no workspace, and hands work that needs
  files or commands to subagents on the runtimes it names (see
  [Delegating to runtimes](../sessions/delegation.md)). The bottom bar's
  **Runtimes…** chip shows the draft's runtimes as badges, or **global
  only**, and opens the runtime cards as a sheet (filling a phone's screen);
  **Change runtimes** above the transcript does the same. The browser
  remembers no selection: each new session starts with `global` alone, or
  with its project's defaults.
  - **Project.** The sheet's **Project** picker comes first: the section
    whose **+** you used, else **No project**, and **New project…**, which
    opens the editor and picks the project it makes. A project whose
    defaults attach runtimes prefills the cards with them, and the chip
    shows them; the session is created with them unless the sheet gives the
    draft others. Picking a project also gives the bottom bar its default
    agent and preset, with the agent's own model; an agent that is gone, or
    cannot start a session, leaves the usual one, and the status line says
    so. Everything stays editable, and choosing a project never changes a
    running session.

  The runtimes a session can use show as cards, grouped by the machine each
  runs on, this daemon's machine first.
  - **Groups.** A group's header says its machine, how many runtimes it
    has, how many are ready, and how many cards are selected; click it to
    fold or unfold the group. This daemon's machine, and a machine with a
    selected card, start unfolded; the browser remembers what you fold.
  - **Cards.** One for `global` (the daemon's own tools, MCP servers, and
    memory; selected to start with, and you can deselect it), and one for
    each runtime capability that takes a workspace. Each says whether its
    runtime is **ready**, **starts on use** (the daemon starts it), or
    **offline** (a runtime that dials in and is not connected), which
    cannot be selected. Click anywhere on a card to select or deselect it.
    Only `global` is selected to start with.
  - **Workspaces.** A selected card asks for its workspace, suggesting
    the directories this browser opened on that runtime and the ones
    sessions use there, as `~/…`; any path works. **another workspace**
    adds a field for the same runtime at a second directory. **Use these
    runtimes** waits until every field is filled in. With no coding card
    selected, the session works in no workspace. With two workspaces or
    more, a note says their tools get prefixes, with an example name.
  - **Colours.** Each card has a dot in its machine's colour, the colour
    of that machine's badge in the session rows.
  - **Use these runtimes** opens every workspace at once. A runtime that
    refuses says why on its card, and the sheet stays; once all of them
    open, the sheet closes, the page shows where the new session will work,
    and the chip shows them. The page never falls back to the daemon's own
    directory.
- **Subagents.** Only the open session's row has its subagents nested
  under it, each with its status (a turning circle while it runs, a check,
  failed, or cancelled), its agent, its short handle, and its task. Click
  one to open it. Subagents have no trash: they go with their root. The
  tree has two states; click the open session's own row, whose chevron
  turns with it, to switch between them (clicking another session still
  opens it), and the browser remembers the choice for each session. A
  subagent's handle gives way before its agent's name when the row is
  narrow:
  - **Collapsed** (the default): only the subagents that run, at any
    depth, and the one you have open. A running subagent whose parent has
    finished shows under its parent, dimmed, so it sits in its place. When
    nothing runs, nothing shows under the session.
  - **Expanded:** the whole hierarchy, finished subagents included; the
    chevron beside a subagent folds its own.
- **Transcript.** Markdown is rendered with any HTML in it shown as text.
  Links open in a new tab, but only `http`, `https`, and `mailto` links;
  images show as links.
  - **Pinned at the top**, folded side by side on one line, as in the
    TUI: the system prompt of the session's last run (`system prompt ·
    primary (last run) (42 lines)`; before its first run, the one it was
    created with), with
    `· next: <agent>` when the bottom bar's agent differs, and the tools
    the last model request offered (`tools · 12 available (last
    request)`). Click one to open it: the prompt in monospace with its
    line breaks and indentation, up to 256 lines or 32 KiB, and the tool
    names as chips, up to 8 KiB of names.
  - **Tool calls** are one line each until you open them, as in the TUI:
    an icon for the kind of tool (from its presentation hint, never its
    name), the title, the primary argument unless the title names it, and
    the status: a turning circle while running, a check when done, and a
    word with its icon when it failed, was cancelled, or was interrupted.
    While a call runs, its latest progress follows the title. A call
    waiting for your approval says **approve?** on its line, which opens
    the approvals. A running call that can move to the background offers
    **background**.
  - **An open call.** Click a call's line (or press Enter on it) to open
    it, and again to close it. It shows its arguments under a muted
    **arguments** label, leaving out what the line already says (a block
    or key/value row that only repeats the primary argument or the text
    the title quotes), then its output. Output past 64 lines ends with
    "… N more lines (click to expand)", which shows up to 1024. The page
    remembers which calls you opened in each session until you reload it.
  - **A delegate call** names the subagent it started with its handle and
    an **open** link; while the subagent runs, the row says whether it is
    thinking, running a tool, or waiting for an approval.
  - **Output blocks** each have their own view: code highlighted by the
    bundled highlight.js, diffs with the file's line numbers, terminal
    output with its colours, tables, JSON, links, session links, and media
    shown inline.
  - **Cut output** offers its complete content.
  - **Your messages** show the files they attached. Their **⋯** shows while
    the mouse is over the message's row (or the keyboard is in it); on a
    touch screen a long press on the message (about half a second; moving
    or scrolling cancels it) opens the same menu. The menu copies the
    message, **reverts** to before it (it and every later turn are
    removed, and its text comes back into the composer), or **forks** a new
    session with the conversation up to it. A revert is refused when another
    client added to the session since the menu opened.
- **Composer.** One line to start; it grows with what you write up to
  five lines, then scrolls, and shrinks back when you send. The paperclip
  (and on a phone the camera) is inside it at the bottom left, and round
  buttons at the bottom right:

  | The session | The box | The buttons |
  |---|---|---|
  | idle | empty | **Send**, disabled |
  | idle | has text or files | **Send** starts a run |
  | running or compacting | empty | **Stop** cancels it, as Escape does |
  | running | has text or files | **Queue** and **Steer** |

  Text that starts with `/` is sent like any other message: the page has
  no slash commands (the ⋮ menu has them). Each button's tooltip names
  its key. The keys work like the TUI's:
  - **Enter** sends, steering a running run.
  - **Alt+Enter** queues the message behind the run.
  - **Shift+Enter or Ctrl+J** inserts a newline.
  - **Escape** cancels the run, or a compaction in progress. Cancelling
    stops the run's tool calls at once (see [Cancelling a
    run](../reference/tools.md#cancelling-a-run)).
  - **Up** in an empty composer takes back your newest pending or queued
    message.

  The strip above the composer lists pending and queued messages; tap one
  of yours to take it back.
- **Commands.** The ⋮ menu in the title bar has the session's commands,
  each by name with an icon and a line saying what it does, and each opens
  a sheet with labelled fields and a main button: **Compact context**,
  **Goal**, **Skills**, **Background calls**, **Runtimes**, **Permission
  rules**, **Usage**, **Diagnostics**, and **Agent preset**. The others
  have their place in the page: the bottom bar picks the agent, model, and
  permission mode and answers approvals; the composer attaches, sends, and
  stops; the sidebar starts and opens sessions; and the rail has the
  daemon's views.
- **Approvals.** When a tool call needs an answer, the bottom bar says so.
  Open it to approve once, approve for the session tree, or reject.
- **Compacting.** **Compact context** opens a sheet with an optional
  **Focus** (what the summary should keep) and **Compact**. While it
  compacts, the sheet offers **Cancel compaction**, as the composer's
  **Stop** and Escape do, and then says how it went.
- **Goal.** **Goal** shows the root session's goal, its checklist, and the
  controls its state allows (**Pause**, **Resume**, **Cancel**), and sets a
  **New objective** with the agent and model shown. The goal bar above the
  composer shows the objective with the same controls. Goals belong to root
  sessions; a child session or a read-only snapshot offers none.
- **Background calls.** While the session has calls running in the
  background, a bar above the composer lists them. Click it, or use
  **Background calls**, to see how long each has run and stop one; a stopped call
  still reports to the model.
- **Files.** Attach files with the paperclip (the file picker), by dropping them on the page, or by pasting them; on a phone,
  the camera button takes a photo. They upload at once and wait on the
  draft as chips; the × on a chip removes it.
  - Only the kinds of file the selected model takes are offered: the picker
    lists their types, and a model that takes none offers no attaching.
  - The page reads each file's type from its content, as the daemon does,
    and says why it cannot attach one the model does not take.
  - An image too large for the model or the daemon is offered as
    **Downscale and attach**: the page redraws it smaller and attaches that.
    Video is never downscaled.
- **Skills.** **Skills** lists the skills you can run, searched as you
  type. Pick one, give its arguments, and run it.
- **Permissions.** **Permission rules** shows the session's rules by action, and
  where each comes from: the agent, the default, or a session override. A
  rule limited to one runtime shows its key as agent documents write it,
  such as `bash@labpc`. Click an effect to override it, the pencil on an
  override to edit it in the form, or the × to remove it. The form takes a
  rule as `action @ runtime`, a pattern, and an effect. The action is any
  name a tool declares (`[a-z][a-z0-9_]{0,31}`), suggested from the
  actions the session's tools and rules use. The runtime is optional: empty
  applies the rule on every runtime, and suggestions come from the
  daemon's runtimes. `message` and `skill` take no runtime, so the field
  turns off for them. A name the daemon would refuse is flagged before it
  is sent. While a new session is composed, the same table edits the
  rules it will be created with, previewed against its agent.
- **Bottom bar.** Below the composer; each part has an icon and says more
  on hover:
  - the agent, the model, and the permission mode, as three chips of one
    shape, each opening its picker. The mode chip has a shield of its own
    for each mode; its picker lists every mode with its shield and what it
    does, checks the current one, and sets the one you click:
    - **auto-approve** (the default): the approval agent decides; what it
      won't approve comes to you;
    - **auto-approve-n**: the approval agent decides; what it would pass
      to you is rejected;
    - **auto-approve-y**: the approval agent decides; what it would pass
      to you is approved once;
    - **ask**: every ask comes to you;
    - **yolo**: every ask is approved at once.

    Deny rules and the loop guard hold in every mode;
  - the approvals waiting, when there are any (click to answer them);
  - **working…** while the session runs;
  - the last status message;
  - where the session (or the new session) works, its workspaces under
    their runtimes' homes, after a dot in each runtime's machine's colour (click for
    **Runtimes**; not on a phone);
  - the context size, in tokens, of the last request (also in **Usage**);
  - the session's cost (click for **Usage**).
- **Diagnostics.** **Diagnostics** picks which diagnostic rows the
  transcript shows; the choice stays in the browser.
- **Messages from producers** (goal control and reminders, background-call
  reports, delegation reports, plugins) show as one folded line with their
  summary; click one to read it whole.

## Managing the daemon

- **Runtimes.** **Runtimes** (in the ⋮ menu) lists the session's attached runtimes with their
  status: **Attach a runtime** searches every runtime's capabilities and asks
  for a workspace when one is needed, **Detach** removes one (the daemon
  refuses while the session runs), and **Retry** restarts a failed
  instance.
  - Under each runtime are the MCP servers its instance uses (from
    `mcp.server.list`, by runtime and workspace): each one's name, state
    (`connected`, `connecting`, `needs_auth`, `failed`, …), tool count, and
    why it failed or what it waits for; a server its runtime shares across
    workspaces (`scope = "runtime"`) is marked `runtime-wide`.
  - While a new session is composed, the list is the runtimes its cards
  opened. One added here starts at once, so a runtime that cannot start
  says why before the session exists.
  - The open panel reads the runtimes again every second, and links to
    the Machines & runtimes view.
- **Machines.** The **Machines & runtimes** view shows each machine as a
  card: an icon for its operating system (Linux, macOS, Windows, or a
  generic computer), its name, its runtimes and memories, and buttons to
  rename it, merge it into another (its memories move too), or forget it.
  Under it are its runtimes, each with an icon for how it came to the
  daemon (built in, started from a template, or dialled in), whether it is
  connected (a green dot), pinned (a pin), its platform, its capabilities,
  its MCP servers counted by state (`MCP 1 connected, 1
  failed`) and listed with their state, tool count, and workspace, its
  runtime-scoped ones (`runtime-wide`) and its workspace-scoped ones that no
  workspace has started yet (`starts with a workspace`) included, and why it
  last failed to connect. A runtime that connected before and is
  offline now stays on its machine, under the connected ones, dimmed, with
  when it was last seen and **Forget**, which drops the daemon's record of
  where it ran (see [Offline runtimes](../concepts/machines.md#offline-runtimes)).
  Runtimes that have never connected, such as a template no session has
  used, sit under **On no machine yet**.
  - **Moving a runtime.** Drag it onto another machine's card: with a
    mouse, press anywhere on its row and drag; on a touch screen, hold the
    row for about half a second until it lifts, then drag (moving sooner
    scrolls the page). The card under it is outlined, and dropping it
    there pins the runtime to that machine. A row held and let go without
    moving opens **Move to…**, which each row also has as a button (the
    keyboard reaches it): pick another machine, or type a new name to
    create one. **Unpin** (the crossed-out pin) lets a pinned runtime
    follow the machine it reports again.
  - **Add a runtime** opens three ways to add one:
    - **Start one over ssh:** a runtime ID, the ssh host, and optionally
      the one directory its workspaces must be under. The page shows the
      `[runtimes.<id>]` template, as in
      [the ssh example](../daemon/runtimes.md#example-a-repository-on-another-machine-over-ssh),
      and **Add to config.toml** writes it with `config.apply`. The daemon
      starts the runtime only after a restart, and says so. The page
      refuses an ID the daemon has, and a host or directory that ssh would
      split or take as an option.
    - **Let one dial in:** the `cookie runtime serve --daemon` command to
      run on the other machine (see
      [Runtimes that dial in](../daemon/runtimes.md#runtimes-that-dial-in)),
      with an optional runtime ID and **Copy**. The daemon's address is
      the page's own unless the page reached it on a loopback address,
      when it is a placeholder. The command carries the token this page
      connected with: it shows as dots until **Show token**, and **Copy**
      takes the real one. A page that keeps no token shows a placeholder.
    - **Guides:** links to the runtimes, ssh, dial-in, remote access, and
      service guides of the released documentation.
- **MCP servers.** Settings › MCP servers lists every runtime's servers with their state,
  tools, and last error. The global runtime's servers can be added, edited,
  enabled or disabled, removed, and kept in your `config.toml`; a coding
  runtime's come from its template. **Reconnect** works for any server.
  - A server that needs authorization offers **Sign in** (see
    [Signing in to MCP servers](#signing-in-to-mcp-servers)).
- **Providers.** Settings › Providers lists the catalog's providers. Pick one to
  connect it, or to update or disconnect its stored connection. Credentials
  are typed into password fields, read once when you connect, and never
  kept by the page; closing the form clears them.
- **Configuration.** Settings › Configuration has `config.toml`. Open it
  to edit it in the page and **Apply** it. A change the daemon refuses
  stays open with the reason, to be fixed and applied again; a document
  that changed meanwhile is reloaded instead.
- **Agents.** The **Agents** view lists the agent documents in a sidebar,
  like the sessions in Chat: the shared agents, then each preset with its
  agents. Each row has an icon for the agent's mode (a robot for a primary
  agent, a person with a gear for a subagent), its ID, its description,
  and **off** when it is disabled or **not applied** when an edit was
  refused. Click one to edit it beside the list; **Apply** and **Close**
  are at the editor's foot, with **Delete** behind a confirmation. **New
  agent** asks for the document's path (`agents/<id>.md`, or
  `agents/<preset>/<id>.md`), and the ↻ button reloads the documents from
  disk after editing them by hand.
- **Memory.** The **Memory** view lists the memories by layer, grouped by machine and
  project, with the files that are not valid memories. Open one to edit it,
  create one at a URL (clicking a layer or group, or a project's **+**,
  fills in its prefix), or delete one. A memory that changed since you
  opened it is never overwritten silently: you choose to reload it,
  overwrite it, or drop your edit. After a save, the status line says what
  the daemon kept in place of your text, when it kept anything.
  - **Projects** are folders, folded until you open one; the browser
    remembers which you opened. A project's trash deletes the project,
    with its sessions, memories, and `AGENTS.md`, after a confirmation
    that counts them, as its header's trash in the sessions sidebar does.
    A daemon older than client protocol 1.8 has no projects, only memory
    folders: there the trash deletes the folder's memories, one
    `memory.delete` each; the files in it that are not memories stay, and
    so does the folder when there are any.
  - **An agent's memory** (`agent://<agent>`) opens as its parts: the
    description and the first keywords (the agent's ID and its mode)
    belong to the daemon and show locked; the other keywords (separated
    by commas) and the memory itself are yours to edit.
- **Plugins.** Settings › Plugins lists each plugin with its state, tools, hooks,
  and why it last failed, and restarts one. A plugin's events show in the
  bottom bar.
- **This browser.** Settings › This browser picks the diagnostics the
  transcript shows (as **Diagnostics** does) and forgets the daemon.

## Phones

- **Size.** The page works from 360 px wide, in portrait and landscape.
  Buttons are at least 44 px, and long lines in code, diffs, and terminal
  output scroll inside their block rather than the page.
- **Chat.**
  - **Title bar:** the sidebar button, the session's title on one line,
    and ⋮, which opens the session's commands as a sheet from the bottom.
    A subagent's session has **‹** before its title, which goes to its
    parent.
  - **Bottom bar:** one row. The model chip takes the room left and
    shortens its name first; the agent chip shows only its icon below
    about 380 px; the permission mode chip shows only its shield; the cost
    shows to the cent ("$0.02") and opens **Usage**, which also has the
    context size. Every picker, and every sheet of the ⋮ menu, opens from
    the bottom.
  - **Status line:** a thin line above the composer, only while the
    session runs, while approvals wait (tap the badge to answer), or for a
    few seconds after a status message.
- **The drawer.** The sidebar button opens a drawer from the left over
  the dimmed page: the rail as an icon strip, with the theme button at its
  foot, beside the sessions, with the open session's subagents nested
  under its row. A tap outside, browser Back, or choosing a session or a
  view closes it; choosing Chat leaves it open on the sessions, and a tap
  on the open session's row switches its tree and leaves it open too. It opens only from the button: a swipe from the edge is
  the browser's Back. A long press on a session's row, let go in place,
  reveals its **Move**, which lists the projects, its **Rename**, which
  makes its title a field, and its **Delete**, then the confirmation; held
  and moved, it drags the row onto a project's header. A long press on a
  project's header shows its pencil and trash.
- **List first.** A view with a list shows the list when you choose it
  on the rail, and opens an item only when you choose it there: Chat's
  sessions stay in the drawer; the agents, the Settings sections, and the
  memories fill the screen. Machines & runtimes has no list and opens at
  once.
- **Agents:** the agent open fills the screen, with the back arrow to the
  agents; the sidebar button opens the drawer with the rail beside the
  agents.
- **Other views** fill the screen under the same title bar. Lists open
  their details one level at a time, such as a Settings section, then a
  document: the back arrow in the title bar, or browser Back, goes up one
  level. An editor fills the screen with **Apply** or **Save** and
  **Cancel** at its foot. Only Chat has the composer and the bottom bar.
- **On its side** (about 844×390), a phone gets the 640 px layout: the
  rail stays, the sessions slide over, and the bottom bar sits beside the
  composer.
- **The on-screen keyboard.** The page sizes itself to what the keyboard
  leaves visible, so the composer and an open sheet stay above it. While
  the composer has focus, the bottom bar steps aside, so only the composer
  (and the status line, when it shows) sits above the keyboard.
- **Home screen.** The page installs as an app; see
  [Installing as an app](#installing-as-an-app).
- **Background tabs.** A phone suspends a tab it does not show. When the tab
  shows again, the page checks its connection, reconnects if the daemon
  stopped answering, and catches up on what happened meanwhile.
- **Reaching the daemon** from a phone takes one of the ways in
  [Forwarding and tunnels](../daemon/remote-access.md#forwarding-and-tunnels). Without
  TLS the token crosses the network in the clear.

## Installing as an app

The page installs to a phone's home screen, or as a desktop app, and then
opens in its own window without the browser's address bar, with the cookie
icon. It still works only while it reaches the daemon: it stores nothing
for offline use, so it always matches the daemon's own build.

**It needs HTTPS.** A browser installs only a page from a secure origin:
HTTPS, or `localhost` and `127.0.0.1` on the daemon's own machine. A page
opened over plain HTTP on a LAN or Tailscale address cannot be installed,
and Settings › This browser says so. The easy way to HTTPS is
[Tailscale Serve](https://tailscale.com/kb/1312/serve) on the daemon's
machine, with the daemon on its default loopback address:

```sh
tailscale serve --bg 7419   # https://<host>.<tailnet>.ts.net/ → 127.0.0.1:7419
```

Open the page on the phone with
`cookie web --url wss://<host>.<tailnet>.ts.net/ws`'s link.

Tailscale Serve needs HTTPS certificates turned on for the tailnet (MagicDNS
and HTTPS in its admin console). `tailscale serve reset` turns it off again
(see [Remote access](../daemon/remote-access.md#forwarding-and-tunnels)).

Where the browser can install the page, the page offers it:

- **Android (Chrome and other Chromium browsers) and desktop Chrome or
  Edge.**
  **Install app** opens the browser's install prompt. It is in Settings ›
  This browser and under the connect screen, and on a phone a banner over
  the page offers it once (× dismisses it for good). Chrome offers the
  prompt once the page has been used for a moment; before that, or in a
  browser without one (Firefox), Settings › This browser points to the
  browser's own menu (**Install app**, **Add to Home screen**).
- **iPhone and iPad.** iOS has no install prompt, so the same places say
  how: tap **Share** in the browser's toolbar, then **Add to Home Screen**
  and **Add**. The app iOS adds keeps its own storage, apart from the
  browser's, so the first time it opens it asks for the token: paste the
  link `cookie web` prints into its Token field (the page takes the token
  out of it).
- **Already installed.** Running as an app, the page offers nothing, and
  Settings › This browser says it runs as an installed app.

The app opens at the page's own address without a token and connects with
the token the browser stored when a `cookie web` link was opened. Its title
bar follows the page's theme, the user's choice included.

To uninstall the app, use the platform's own way; cookie keeps nothing
outside the browser's storage for it:

- **Android.** Long-press the app icon, then **Uninstall** (or **App info**
  › **Uninstall**).
- **iPhone and iPad.** Long-press the icon, then **Remove App** › **Delete
  App**.
- **Desktop Chrome or Edge.** Open the app, open its window menu (the three
  dots), and choose **Uninstall**; or open `chrome://apps` (`edge://apps`
  in Edge), right-click the app, and choose **Remove**.

The browser keeps the daemon's token and the page's settings in site
storage even after the app is gone. **Forget this daemon** (Settings ›
This browser, or the connect screen) deletes the token; to clear everything
else, clear the site data for the daemon's address in the browser's
settings.

The page has a service worker, `sw.js`, which installing needs on some
browsers. It caches nothing and passes every request to the daemon, so a
daemon that is not running shows the browser's own error, and a restarted
daemon of a new build serves its own page at once. It never touches the
`/ws` and `/runtime` WebSockets.

## Signing in to MCP servers

A remote MCP server that wants OAuth shows `needs_auth` in Settings › MCP
servers, with **Sign in**. The page is its own redirect: the authorization
server sends the browser back to the page's address with `?oauth=callback`,
for example `https://<host>.<tailnet>.ts.net/?oauth=callback`. The daemon
keeps the PKCE verifier and exchanges the code; the tokens never reach the
page. Only the daemon's own (global) servers can sign in, and only with a
daemon of client protocol 1.11 or later; an older daemon's servers say to
run `cookie mcp auth <server>` instead.

- **Sign in** opens the authorization page in a new tab. If the browser
  blocks the tab, the page goes there itself.
- **Coming back.** The tab the authorization server sends back takes the
  code and state out of its address bar at once, then hands the address to
  the tab that started the sign-in and closes. That tab finishes the
  sign-in and says "Signed in to `<server>`" or what went wrong. With no
  such tab open, the tab that came back finishes the sign-in itself and
  opens Settings › MCP servers.
- **Paste the address you landed on.** While a sign-in waits, the MCP
  servers show it with a field for the address the browser ended on. Use
  it when the sign-in ends somewhere the page cannot continue from: on an
  iPhone, the app added to the home screen keeps its storage apart from
  Safari, so the page Safari opens cannot finish a sign-in the app started.
  It says so and shows the address with **Copy the address**; paste it
  into the app. **Cancel** gives the sign-in up.
- **Refused callbacks.** A sign-in waits 10 minutes in the page (the daemon
  gives up after 5). A callback for one that waited longer, or whose state
  this browser does not know, is not finished.
- **HTTPS.** Most authorization servers send the browser back only to an
  `https` address or to this machine's loopback. On a page served over
  plain `http` at another address, the MCP servers warn and point to
  [remote access](../daemon/remote-access.md); Tailscale Serve (see
  [Installing as an app](#installing-as-an-app)) is the easy way to HTTPS.
  **Sign in** stays, for servers that allow it.
- **A configured `client_id`.** The daemon registers a client for the
  page's address by itself when the server offers dynamic client
  registration. A server configured with `oauth = { client_id = "…" }`
  needs the page's address with `?oauth=callback` among the redirect URIs
  allowed for that client, on the server's side (see
  [MCP](../extend/mcp.md#oauth-for-remote-servers)).

## What the page does not do

- **Offer this machine as a runtime.** A browser has no workspace to share.
  The TUI's machine can attach as a runtime that dials in; the page uses the
  runtimes the daemon already has.

## Credits

The page's icons are [Font Awesome Free](https://fontawesome.com) by
Fonticons, Inc., under the
[CC BY 4.0 licence](https://fontawesome.com/license/free). The connect
screen and the connection panel credit them. They are inline SVG in the
page's code (from the `icondata` crate, shown through `leptos_icons`), not
a web font.

## Hosting the page elsewhere

Every release attaches the page as `cookie-agent-web.tar.gz`. Its files use
relative paths, so any static host can serve them from any path. The page
then fills in a URL on that host, which you edit to name the daemon.

## Building the page

A `cookie` built from a fresh checkout serves a placeholder page.
`scripts/build-web.sh` builds the real one:

- it needs `trunk` 0.21.14 and the `wasm32-unknown-unknown` target;
- it writes the page to `crates/web_assets/bundle/`, which the next build
  of `cookie` embeds;
- it fails when the page, gzipped, is over its budget of 10 MB, a backstop against runaway growth.

The daemon sends the page gzip-compressed to browsers that accept it, under a
Content-Security-Policy that allows only the page's own scripts and
WebSocket connections to daemons.

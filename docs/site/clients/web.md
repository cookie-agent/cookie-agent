# Web

`cookie daemon` serves a web client on the same port as its client endpoint.
Open it in a browser on the daemon's machine, or on a laptop or phone that
reaches that port (see [Remote access](../daemon/remote-access.md)). It does what the TUI does with the daemon's sessions and
the daemon itself: chat, approvals, subagents, history edits, goals,
attachments, skills, permissions, runtimes and machines, MCP servers,
providers, the configuration, memory, and plugins. Two things stay in the
TUI and the CLI; see [What the page does not do](#what-the-page-does-not-do).

## Opening the page

Allow the page's origin first. A browser connection is accepted only from
an origin `[server] allowed_origins` lists (see
[Web clients](../daemon/remote-access.md#web-clients)), and the page is no
exception. For a daemon on the default port, opened on the same machine:

```toml
[server]
allowed_origins = ["http://127.0.0.1:7419"]
```

Restart the daemon after changing the list. While the list is empty, the
daemon's startup output says the page cannot connect yet.

Then print the page's address with the token:

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
  closes its connection before the handshake. The usual cause is an origin
  missing from `allowed_origins`, so the page quotes its own origin as the
  entry to add.
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
  - **Agents**: the agent documents and their presets;
  - **Settings**: `config.toml`, providers, MCP servers, plugins, and this
    browser's preferences;
  - **Machines & runtimes**: the daemon's machines and its runtime
    registry;
  - **Memory**.

  The connection and the theme button are at its foot. `/config`,
  `/connect`, `/mcp`, `/plugins`, and `/memory` open their view; the
  session's own commands open sheets.
- **Chat.** The title bar has the sidebar button, the session's title, and
  the ⋮ menu of the session's commands. Below the transcript are the
  composer and the bottom bar. While a subagent's session is open, the
  title is a path from its root, such as
  `Fix the greeting › reviewer: check tests`: click a parent to go up to
  it. The ⋮ menu acts on the session shown.
- **Widths.**
  - **1024 px and wider:** the sessions sidebar is pinned beside the
    transcript. The sidebar button hides and shows it, and the browser
    remembers the choice.
  - **640 to 1023 px:** the rail stays; the sidebar button slides the
    sessions over the transcript.
  - **Narrower, a phone:** see [Phones](#phones).
- **Sheets.** The session's panels open as a sheet on the right (from the
  bottom on a phone): approvals, the agent, model, and preset pickers,
  usage, the diagnostics filter, the goal, background calls, skills,
  permissions, a message's actions, the session's runtimes, and the ⋮
  menu.
- **Long lines** in code, diffs, and terminal output scroll inside their
  block.

## Sessions and chat

- **Sessions.** The page has no directory of its own, so the list shows
  every directory's sessions, with a filter by directory.
  - **Rows.** Each session shows its title, an icon for its status
    (running, completed, failed, cancelled, interrupted, or idle), the name
    of its directory, and how long ago it was last active. Hover over a row
    for the directory's full path.
  - **New session** starts a draft in a directory you name; leave it empty
    for the daemon's own. The first message creates the session with the
    agent, model, and permission mode the bottom bar shows.
  - **Owned elsewhere.** A session another cookie process owns opens as a
    read-only snapshot.
  - **Deleting.** Hover over a session's row for its trash button (on a
    touch screen, press the row for about half a second; the press never
    opens the session, and scrolling cancels it). A confirmation in the row
    names the session; **Delete** removes it and its subagents from the
    daemon's disk, for good. The daemon refuses while the session tree
    runs, while another cookie process holds it, or when it changed since
    the list showed it; the row then shows the daemon's reason. A deleted
    session leaves every client's list, and a page showing it starts a new
    session. A daemon older than client protocol 1.5 cannot delete, and
    the page offers no trash.
- **Subagents.** The open session's row expands to show its subagents,
  nested under it: each with its status (a turning circle while it runs, a
  check, failed, or cancelled), its agent, its short handle, and its task.
  Click one to open it, and the chevron beside it to fold its own
  subagents. Subagents have no trash: they go with their root.
  - `/agent-panel auto` (the default) expands the row while a subagent is
    live or a subagent's session is open; `/agent-panel show` and
    `/agent-panel hide` always expand or fold it.
- **Transcript.** Markdown is rendered with any HTML in it shown as text.
  Links open in a new tab, but only `http`, `https`, and `mailto` links;
  images show as links.
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
  - **Your messages** show the files they attached. Their **⋯** menu copies
    the message, **reverts** to before it (it and every later turn are
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

  Each button's tooltip names its key. The keys work like the TUI's:
  - **Enter** sends, steering a running run.
  - **Alt+Enter** queues the message behind the run.
  - **Shift+Enter or Ctrl+J** inserts a newline.
  - **Escape** cancels the run, or a compaction in progress.
  - **Up** in an empty composer takes back your newest pending or queued
    message.

  The strip above the composer lists pending and queued messages; tap one
  of yours to take it back.
- **Commands.** `/` starts a command:
  - `/new`;
  - `/sessions` (also `/resume`, `/load`, `/continue`);
  - `/agent`, `/model`, `/preset`;
  - `/approvals`;
  - `/mode` cycles the permission mode;
  - `/usage`, `/cancel`, `/events`;
  - `/goal`, `/compact`, `/background` (also `/bg`), `/skills`,
    `/permissions` (also `/perms`), `/attach` (also `/file`, `/image`);
  - `/agent-panel show`, `/agent-panel hide`, `/agent-panel auto`;
  - `/runtime` (also `/runtimes`), `/mcp`, `/connect`, `/config`,
    `/memory` (also `/memories`), `/plugins` (also `/plugin`).

  The ⋮ menu in the title bar lists the session's own commands: `/runtime`,
  `/permissions`, `/goal`, `/background`, `/usage`, `/compact`,
  `/skills`, `/events`, `/preset`, and `/agent-panel`. The others have
  their place in the page: the bottom bar picks the agent, model, and mode
  and answers approvals; the composer attaches, sends, and cancels; the
  sidebar starts and opens sessions; and the rail has the daemon's views.
- **Approvals.** When a tool call needs an answer, the bottom bar says so.
  Open it to approve once, approve for the session tree, or reject.
- **Compacting.** `/compact` compacts the session's context, and
  `/compact <focus>` says what the summary should keep. Escape, or
  `/cancel`, cancels a compaction in progress.
- **Goal.** `/goal <objective>` sets the root session's goal with the agent
  and model shown; `/goal pause`, `/goal resume`, and `/goal cancel` control
  it, and `/goal` alone shows it. The goal bar above the composer shows the
  objective with the controls its state allows. Goals belong to root
  sessions; a child session or a read-only snapshot offers none.
- **Background calls.** While the session has calls running in the
  background, a bar above the composer lists them. Click it, or use
  `/background`, to see how long each has run and stop one; a stopped call
  still reports to the model.
- **Files.** Attach files with `/attach` or the paperclip (the file
  picker), by dropping them on the page, or by pasting them; on a phone,
  the camera button takes a photo. They upload at once and wait on the
  draft as chips; the × on a chip removes it.
  - Only the kinds of file the selected model takes are offered: the picker
    lists their types, and a model that takes none offers no attaching.
  - The page reads each file's type from its content, as the daemon does,
    and says why it cannot attach one the model does not take.
  - An image too large for the model or the daemon is offered as
    **Downscale and attach**: the page redraws it smaller and attaches that.
    Video is never downscaled.
- **Skills.** `/skills` lists the skills you can run, searched as you type
  (`/skills <search>` starts the search). Pick one, give its arguments, and
  run it.
- **Permissions.** `/permissions` shows the session's rules by action, and
  where each comes from: the agent, the default, or a session override.
  Click an effect to override it, add a rule for a pattern, or the × on an
  override to remove it. While a new session is composed, the same table edits the
  rules it will be created with, previewed against its agent.
- **Bottom bar.** Below the composer; each part has an icon and says more
  on hover:
  - the agent, the model, and the permission mode, as three chips of one
    shape: the agent and model chips open their pickers; the mode chip,
    with a shield of its own for each mode, switches to the next mode;
  - the approvals waiting, when there are any (click to answer them);
  - **working…** while the session runs;
  - the last status message;
  - the context size, in tokens, of the last request (also in `/usage`);
  - the session's cost (click for `/usage`).
- **Diagnostics.** `/events` picks which diagnostic rows the transcript
  shows; the choice stays in the browser.
- **Messages from producers** (goal control and reminders, background-call
  reports, delegation reports, plugins) show as one folded line with their
  summary; click one to read it whole.

## Managing the daemon

- **Runtimes.** `/runtime` (in the ⋮ menu) lists the session's attached runtimes with their
  status: **Attach a runtime** searches every runtime's capabilities and asks
  for a workspace when one is needed, **Detach** removes one (the daemon
  refuses while the session runs), and **Retry** restarts a failed
  instance.
  - While a new session is composed, the list is the runtimes it will
  attach: `global` and `local`'s `coding` at its directory to start with.
  Each starts at once, so a runtime that cannot start says why before the
  session exists.
  - The open panel reads the runtimes again every second, and links to
    the Machines & runtimes view.
- **Machines.** The **Machines & runtimes** view lists each machine with
  its runtimes. Rename or forget a machine, merge it into another (its
  memories move too), move a runtime to a machine, or unpin it. Under the
  machines, the runtime registry lists every runtime: whether it is
  connected, its machine and platform, its capabilities, and why it last
  failed to connect.
- **MCP servers.** Settings › MCP servers (`/mcp`) lists every runtime's servers with their state,
  tools, and last error. The global runtime's servers can be added, edited,
  enabled or disabled, removed, and kept in your `config.toml`; a coding
  runtime's come from its template. **Reconnect** works for any server.
  - A server that needs authorization says so. The page cannot sign in to
    it: run `cookie mcp auth <server>` on a machine with a browser, or
    authorize it from the TUI.
- **Providers.** Settings › Providers (`/connect`) lists the catalog's providers. Pick one to
  connect it, or to update or disconnect its stored connection. Credentials
  are typed into password fields, read once when you connect, and never
  kept by the page; closing the form clears them.
- **Configuration.** Settings › Configuration (`/config`) has
  `config.toml`, and the **Agents** view the agent documents, by preset.
  Open one to edit it in the page and **Apply** it. A change the daemon
  refuses stays open with the reason, to be fixed and applied again; a
  document that changed meanwhile is reloaded instead. The Agents view
  creates an agent document or deletes one; both reload from disk after
  editing files by hand.
- **Memory.** The **Memory** view (`/memory`) lists the memories by layer, grouped by machine and
  project, with the files that are not valid memories. Open one to edit it,
  create one at a URL (clicking a layer or group fills in its prefix), or
  delete one. A memory that changed since you opened it is never
  overwritten silently: you choose to reload it, overwrite it, or drop your
  edit.
- **Plugins.** Settings › Plugins (`/plugins`) lists each plugin with its state, tools, hooks,
  and why it last failed, and restarts one. A plugin's events show in the
  bottom bar.
- **This browser.** Settings › This browser picks the diagnostics the
  transcript shows (as `/events` does) and forgets the daemon.

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
    about 380 px; the permission mode chip shows only its shield, and a tap
    says
    "now: <mode>" for a moment; the cost shows to the cent ("$0.02") and
    opens `/usage`, which also has the context size. Every picker opens
    from the bottom.
  - **Status line:** a thin line above the composer, only while the
    session runs, while approvals wait (tap the badge to answer), or for a
    few seconds after a status message.
- **The drawer.** The sidebar button opens a drawer from the left over
  the dimmed page: the rail as an icon strip, with the theme button at its
  foot, beside the sessions, with the open session's subagents nested
  under its row. A tap outside, browser Back, or choosing a session or a
  view closes it. It opens only from the button: a swipe from the edge is
  the browser's Back. A long press on a session's row reveals its
  **Delete**, then the confirmation.
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
- **Home screen.** Add the page to the home screen from the browser's menu.
  It opens on its own, without the browser's address bar, with the cookie
  icon. The page keeps working only while it reaches the daemon: it stores
  nothing for offline use.
- **Background tabs.** A phone suspends a tab it does not show. When the tab
  shows again, the page checks its connection, reconnects if the daemon
  stopped answering, and catches up on what happened meanwhile.
- **Reaching the daemon** from a phone takes one of the ways in
  [Forwarding and tunnels](../daemon/remote-access.md#forwarding-and-tunnels). Without
  TLS the token crosses the network in the clear.

## What the page does not do

- **Sign in to an MCP server.** Authorizing a remote MCP server needs a
  callback on the machine's loopback address, which a page in a browser
  cannot offer. `/mcp` marks such a server as needing authorization: run
  `cookie mcp auth <server>` on a machine with a browser, or authorize it
  from the TUI.
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
then fills in a URL on that host, which you edit to name the daemon. Allow
the host's origin in the daemon's `allowed_origins`.

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

# Web

`cookie daemon` serves a web client on the same port as its client endpoint.
Open it in a browser on the daemon's machine, or on a laptop or phone that
reaches that port. It works the daemon's sessions as the TUI does. A later
version adds what is still missing: subagents, fork and revert, goals,
attachments, and the management panels.

## Opening the page

Allow the page's origin first. A browser connection is accepted only from
an origin `[server] allowed_origins` lists (see
[Web clients](../engine/server.md#web-clients)), and the page is no
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
`https`) and the final `/ws` dropped.

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
- **Forget this daemon** deletes the stored token.
- **A refused connection.** A browser shows a page no reason when the daemon
  closes its connection before the handshake. The usual cause is an origin
  missing from `allowed_origins`, so the page quotes its own origin as the
  entry to add.
- **Once connected**, the header shows the daemon's address and version.
  - **Dropped connection.** The page redials with the TUI's backoff,
    500 ms rising to 10 s, under the same client identity.
  - **Background tab.** When it becomes visible again, the page checks the
    connection.
  - **Separate clients.** Each browser tab is its own client.
- **Theme.** The theme button cycles through system, light, and dark. The
  choice stays in the browser.

## Sessions and chat

- **Layout.**
  - **Wide windows:** the session list is on the left, the transcript in
    the middle, and the composer and status bar at the bottom.
  - **Panels:** these open as a sheet on the right. They cover approvals,
    the agent, model, and preset pickers, usage, the diagnostics filter,
    the goal, background calls, skills, permissions, a message's actions,
    and the command menu.
  - **Narrow windows such as a phone:** the session list and every panel
    open full screen. Long lines in code, diffs, and terminal output scroll
    inside their block.
- **Sessions.** The page has no directory of its own, so the list shows
  every directory's sessions, with a filter by directory.
  - **New session** starts a draft in a directory you name; leave it empty
    for the daemon's own. The first message creates the session with the
    agent and model shown, and the permission mode the status bar shows.
  - **Owned elsewhere.** A session another cookie process owns opens as a
    read-only snapshot.
- **Subagents.** The **Agents** panel above the session list shows the
  viewed session's tree: each delegated agent with its task, `●` while it
  runs. Click one to open it, and ▾ to fold its children. A session link in
  a delegation's output opens the child too.
  - `/agent-panel auto` (the default) shows the panel while a delegated
    agent is live or a child session is open; `/agent-panel show` and
    `/agent-panel hide` always show or hide it.
- **Transcript.** Markdown is rendered with any HTML in it shown as text.
  Links open in a new tab, but only `http`, `https`, and `mailto` links;
  images show as links.
  - **Tool calls** show their title and status. Click one for its
    arguments and output. A running call that can move to the background
    offers **⇲ background**.
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
- **Composer.** It works like the TUI's:
  - **Enter** sends, steering a running run.
  - **Alt+Enter** queues the message behind the run.
  - **Shift+Enter or Ctrl+J** inserts a newline.
  - **Escape** cancels the run.
  - **Up** in an empty composer takes back your newest pending or queued
    message.

  On a phone, **Send** steers, and a long press on it, or its ▾ menu,
  queues. The strip above the composer lists pending and queued messages;
  tap one of yours to take it back.
- **Commands.** `/` starts a command:
  - `/new`;
  - `/sessions` (also `/resume`, `/load`, `/continue`);
  - `/agent`, `/model`, `/preset`;
  - `/approvals`;
  - `/mode` cycles the permission mode;
  - `/usage`, `/cancel`, `/events`;
  - `/goal`, `/compact`, `/background` (also `/bg`), `/skills`,
    `/permissions` (also `/perms`), `/attach` (also `/file`, `/image`);
  - `/agent-panel show`, `/agent-panel hide`, `/agent-panel auto`.

  The ☰ button lists them all.
- **Approvals.** When a tool call needs an answer, the status bar says so.
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
- **Files.** Attach files with `/attach` or 📎 (the file picker), by dropping
  them on the page, or by pasting them; on a phone, 📷 takes a photo. They
  upload at once and wait on the draft as chips; × removes one.
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
  Click an effect to override it, add a rule for a pattern, or × to remove
  an override. While a new session is composed, the same table edits the
  rules it will be created with, previewed against its agent.
- **Status bar.** It shows the permission mode (click it for the next one),
  the session's cost (click for `/usage`), and its context size.
- **Diagnostics.** `/events` picks which diagnostic rows the transcript
  shows; the choice stays in the browser.

## Reaching the daemon from elsewhere

The page and `/ws` share one port, so forwarding that port forwards both.
The page then finds the daemon at its own address.

- **An SSH tunnel:** `ssh -L 7419:127.0.0.1:7419 host`. Open
  `http://127.0.0.1:7419/` and allow `http://127.0.0.1:7419`.
- **Tailscale or a LAN.** Run `cookie daemon --listen 0.0.0.0:7419` and
  allow the address you open, such as `http://100.101.102.103:7419`.
  Without TLS the token crosses the network in the clear, so keep this to a
  private network.
- **A tunnel such as cloudflared** gives an HTTPS address. Point it at the
  daemon's port, and allow the tunnel's origin, such as
  `https://cookie.example.com`.
  - A page served over HTTPS connects with `wss://`. The tunnel provides
    that.
  - A proxy may serve the daemon under a subpath, such as
    `https://example.com/cookie/`. The page then connects to
    `wss://example.com/cookie/ws`.

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
- it fails when the page, gzipped, is over its budget of 1.6 MB.

The daemon sends the page gzip-compressed to browsers that accept it, under a
Content-Security-Policy that allows only the page's own scripts and
WebSocket connections to daemons.

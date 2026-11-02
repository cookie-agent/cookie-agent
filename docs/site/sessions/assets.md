# Assets

An agent can give you a file: a chart, a diagram, an HTML page, a Markdown
report, or anything else. It saves the file as an **asset** and shows it
in the conversation, as a card under the text it writes next. The web
client shows images and video as a deck, HTML pages and Markdown
documents as cards that open beside the chat, and other files as
downloads; the TUI lists them with links that open them in the web
client. The [spec](../specs/assets.md) has the details.

## How an agent uses them

The `assets` tools ([reference](../reference/tools.md#asset-tools)) are a
[deferred](../reference/tools.md#deferred-tools) group, so an agent loads
them when it needs them:

- **`write_asset`** saves a file from one of the session's machines, up to
  25 MiB, and returns its handle, such as `asset_3f9a1c2e`. Given a handle,
  it replaces that asset instead. With `show: true` it shows the asset at
  once.
- **`show_asset`** shows one to twelve assets, with a caption. They show
  under the next text the agent writes, which may name them by handle.
- **`read_asset`**, **`list_assets`**, and **`delete_asset`** read, list,
  and delete them.

The model gets one line back from each call, never the file, so showing
an asset costs almost nothing of its context.

## Session and project assets

An asset belongs to the session tree that saved it (`scope: "session"`,
the default): every session of the tree can use it, and it is deleted with
the tree. An agent saves something the project should keep with
`scope: "project"`: every session of the project can then use it, and it
stays when the session is deleted. Sessions in no project share one store
for those.

There are no versions. Writing an asset again replaces it, and everywhere
it shows, it shows the new content and says it was updated. Where a
deleted asset showed, it says it was deleted.

```text
~/.cookie-agent/
  projects/<project-id>/assets/     a project's assets
  assets/                           the assets of sessions in no project
  session-assets/<root-id>/          a session tree's own
```

## Permissions

The tools check the `asset` action, whose resource is what a call does:
`read` (`read_asset`, `show_asset`, `list_assets`), `write`
(`write_asset`), or `delete` (`delete_asset`). A root run is allowed them
all by default, and a subagent is denied them, so its asset tools are
hidden unless its agent document grants them:

```yaml
permissions:
  asset:
    read: allow
    write: allow
```

`write_asset` also checks its file as `read` on the machine it reads it
from, so your `read` rules decide which files can become assets. See
[Permissions](../concepts/permissions.md).

## In the web client

- **The shelf.** A run's calls that show assets fold into its work, and
  the assets show as cards under the text that follows them, sharing rows
  where they fit: images and video as a deck the media viewer steps
  through, an HTML page as a live thumbnail, a Markdown document as its
  first lines, audio as a player, any other file as a download chip.
- **The side panel.** A page or document card opens beside the chat, with
  *Preview* and *Source*, *Full screen*, and *Download*. An HTML page runs
  there in a sandboxed frame with its scripts and network access, with no
  access to the page, its storage, or its connection to the daemon. A
  page must be one self-contained file.
- **Handles.** A handle the agent names in its reply is a chip that opens
  the asset, and every card can copy its handle, to give the asset back
  to an agent.
- **The Assets view** on the rail lists every project's and every
  session's assets, newest first. Its filter narrows them to a project (or
  no project) and to a level: a session's own, or those kept for a
  project. Chips by kind and a search of titles and file names narrow
  further. Each row says where the asset lives, a session's own naming
  its session, and opens to show it, or is renamed, downloaded, or
  deleted.

See [the web client](../clients/web.md#assets).

## In the TUI

The shelf lists each asset with its caption and a link into the daemon's
web page, such as
`http://127.0.0.1:7419/#session=<id>&asset=asset_3f9a1c2e`. Click it, or
press Enter on it, to open it in your browser. The link is the daemon's
[`[server] page_url`](../engine/server.md) when set, for a tunnel, or the
page at the address the TUI reached the daemon at. It carries no token. A
TUI running its own daemon has no web page, so it says to open the web
client instead. See [the TUI](../clients/tui.md#assets).

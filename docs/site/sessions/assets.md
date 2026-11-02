# Assets and Figures

An agent can give you a file: a chart, a diagram, an HTML page, a Markdown
report, or anything else. It saves the file as an **asset** and shows it
in the conversation as a **figure**. The web client shows images, SVG,
video, audio, Markdown, and live HTML pages inline; the TUI shows each
figure with a link that opens it in the web client. The
[spec](../specs/assets.md) has the details.

## How an agent uses them

The `assets` tools ([reference](../reference/tools.md#asset-tools)) are a
[deferred](../reference/tools.md#deferred-tools) group, so an agent loads
them when it needs them:

- **`write_asset`** saves a file from one of the session's machines, up to
  25 MiB, and returns its handle, such as `asset_3f9a1c2e`. Given a handle,
  it replaces that asset instead. With `show: true` it shows the asset at
  once.
- **`show_asset`** shows one to twelve assets as one figure, with a
  caption.
- **`read_asset`**, **`list_assets`**, and **`delete_asset`** read, list,
  and delete them.

The model gets one line back from each call, never the file, so showing a
figure costs almost nothing of its context.

## Session and project assets

An asset belongs to the session tree that saved it (`scope: "session"`,
the default): every session of the tree can use it, and it is deleted with
the tree. An agent saves something the project should keep with
`scope: "project"`: every session of the project can then use it, and it
stays when the session is deleted. Sessions in no project share one store
for those.

There are no versions. Writing an asset again replaces it, and every figure
of it shows the new content and says when it was updated. A deleted
asset's figures say it was deleted.

```text
~/.cookie-agent/
  projects/<project-id>/assets/     a project's assets
  assets/                           the assets of sessions in no project
  sessions/<month>/<root-id>/assets/  a session tree's own
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

- **Figures** show in the transcript, outside the reduced mode's folded
  work, with the asset's title, *Full screen*, *Download*, and for an HTML
  page *Reload*. A Markdown document longer than 480 px shows *Show all*.
- **HTML pages** run in a sandboxed frame with their scripts and network
  access. The frame has no access to the page, its storage, or its
  connection to the daemon, and it grows with its content up to 80% of the
  window. A page must be one self-contained file.
- **The Assets view** on the rail lists a project's assets (or those of
  sessions in no project), filtered by kind and searched by title. Each
  opens as a figure, and can be renamed, downloaded, or deleted. A
  session's own assets are not listed there.

See [the web client](../clients/web.md#figures).

## In the TUI

A figure lists its caption and each asset with a link into the daemon's web
page, such as
`http://127.0.0.1:7419/#session=<id>&asset=asset_3f9a1c2e`. Click it, or
press Enter on it, to open it in your browser. The link is the daemon's
[`[server] page_url`](../engine/server.md) when set, for a tunnel, or the
page at the address the TUI reached the daemon at. It carries no token. A
TUI running its own daemon has no web page, so it says to open the web
client instead. See [the TUI](../clients/tui.md#figures).

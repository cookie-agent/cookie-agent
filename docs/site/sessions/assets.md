# Assets and attachments

An agent can give you a file: a chart, a diagram, an HTML page, a Markdown
report, or anything else.

- **An attachment** is a file it shows you once, where it writes it in its
  reply: `<attach src="runs/loss.png"/>`. The file is read once from the
  machine and kept with the conversation. See
  [Attachments](#attachments-in-a-reply).
- **An asset** is a file it saves to keep, which it can replace and you
  can find in the Assets view; it shows one with
  `<attach asset="asset_3f9a1c2e"/>`, always as the asset is now.

The web client shows images, video, HTML pages, and Markdown documents
as a gallery of cards (pages and documents open beside the chat), and
other files as downloads; the TUI lists them with links that open them in the web
client. The specs have the details: [assets](../specs/assets.md) and
[attachments](../specs/message-attachments.md).

## Attachments in a reply

An agent writes a tag where a file should show:

| Tag | Shows |
|---|---|
| `<attach src="runs/loss.png"/>` | A file of the session's workspace machine; add `machine="name"` for another machine |
| `<attach artifact="<sha256>"/>` | An artifact of the conversation, such as tool output kept aside |
| `<attach asset="asset_3f9a1c2e"/>` | An asset, live |

`title="…"` names it. A tag in code is text. When the agent's message
ends, each tag is resolved once:

- **A file** is read as the agent's `read` rules allow: allowed, it is
  attached; denied, it fails; when the rule asks, its card asks you, with
  *Allow* and *Deny*. Up to 25 MiB, and 12 attachments a message.
- **An artifact** is attached as it is, and **an asset** shown as the
  `asset` rules allow.
- **A failure** says why on its card, and the agent is told before its
  next request, in a line the transcript shows.
- **Save as asset** on a file's card keeps it as an asset of the session
  or the project.

ACP agents (Claude Code, Codex) attach files the same way, from their own
machine.

## How an agent uses them

The `assets` tools ([reference](../reference/tools.md#asset-tools)) are a
[deferred](../reference/tools.md#deferred-tools) group, so an agent loads
them when it needs them:

- **`write_asset`** saves a file from one of the session's machines, up to
  25 MiB, and returns its handle, such as `asset_3f9a1c2e`. Given a handle,
  it replaces that asset instead. The agent shows it with
  `<attach asset="…"/>` in its reply.
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
`read` (`read_asset`, `list_assets`, and an `<attach asset/>` tag), `write`
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

- **Cards.** Each tag of a reply is a card where it is, tags next to each
  other one row: its images, video, pages, and documents a gallery of
  cards laid out by how many there are (an image alone large and whole),
  the media viewer stepping through its images and video; an HTML page as
  a live thumbnail, a Markdown document as its first lines; then audio as
  a player and any other file as a download chip.
- **The side panel.** A page or document card opens beside the chat, with
  *Preview* and *Source*, *Full screen*, and *Download*. An HTML page runs
  there in a sandboxed frame with its scripts and network access, with no
  access to the page, its storage, or its connection to the daemon. A
  page must be one self-contained file.
- **Handles.** A handle the agent names in its reply is a chip that opens
  the asset, and every card can copy its handle, to give the asset back
  to an agent.
- **The Assets view** in the sidebar lists every project's and every
  session's assets, newest first. Its filter narrows them to a project (or
  no project) and to a level: a session's own, or those kept for a
  project. Chips by kind and a search of titles and file names narrow
  further. Each row says where the asset lives, a session's own naming
  its session, and opens to show it, or is renamed, downloaded, or
  deleted.

See [the web client](../clients/web.md#assets).

## In the TUI

A tag shows as `📎` and its name in the text, and under the text each
attachment has a line with a link into the daemon's web page, such as
`http://127.0.0.1:7419/#session=<id>&asset=asset_3f9a1c2e` (an asset) or
`…#session=<id>&attachment=<sha256>` (an attached file), or *Allow* and
*Deny* while it waits for you. Click a link, or
press Enter on it, to open it in your browser. The link is the daemon's
[`[server] page_url`](../engine/server.md) when set, for a tunnel, or the
page at the address the TUI reached the daemon at. It carries no token. A
TUI running its own daemon has no web page, so it says to open the web
client instead. See [the TUI](../clients/tui.md#assets).

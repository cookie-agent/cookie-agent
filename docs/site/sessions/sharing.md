# Share links

A share link is a public URL for one [asset](assets.md). Anyone who has it
can open it in a browser, without the daemon token, an account, or the
cookie web client, until it expires or you revoke it. The spec has the
details: [share links](../specs/share-links.md).

## What a link shows

The link opens a small page with the asset's title, what it is, a preview,
and **Download**:

- an HTML page runs in a sandboxed frame, and **Open full page** opens it in
  a tab of its own, still sandboxed;
- images, video, audio, and PDFs show in the browser;
- a Markdown file reads as a document, and text and code show their first
  256 KB;
- anything else gets a file card with **Download**.

A link always shows the asset **as it is now**: when an agent replaces the
asset, the link shows the new content. Deleting the asset ends its links,
and so does deleting the session or project it belongs to.

Pasted into a chat app, a link to an image previews the image.

## Making a link

**In the web client**, every asset card, attachment card, row of the Assets
view, and the side panel has a share button. The dialog shows the link with
**Copy**, and its expiry: **1 day**, **7 days** (the default), **30 days**,
or **Never**. An asset that already has a link shows that link; **New link**
makes another, for example to revoke one person's copy without breaking the
others. An attachment is saved as an asset of the session first, and the
dialog names the new asset.

**From the CLI:**

```sh
cookie share create asset_3f9a1c2e --expires 30d   # prints the URL
cookie share list
cookie share revoke <id>
cookie share revoke --all
```

**An agent** can make a link with the `share_link` tool. It asks you first:
the `share` action defaults to `ask` for a root agent and `deny` for a
subagent at any depth. An agent cannot list or revoke links.

## Managing links

The web client's **Sharing** view lists every live link: its title, the
asset, who made it, when it expires, and how often it was opened. Each row
has **Copy**, **Change expiry**, and **Revoke**; **Revoke all** ends every
link at once. Counts are approximate: chat apps that preview a link open it
too.

## Where links point

A link is built on `[share] url`, else `[server] page_url`, else the address
your browser reached the daemon at. That address must be one the people you
send it to can reach. The dialog warns when it looks private (plain `http`,
loopback, a LAN or Tailscale address). The safe way to go public is the
share-only port: see
[Remote access](../daemon/remote-access.md#public-share-links) and
[`[share]`](../engine/share.md).

## Keep in mind

- A link is a secret: whoever has it can open it until it expires or is
  revoked.
- What a shared asset holds is public as soon as it changes.
- `[share] enabled = false` turns every link off at once, and keeps them.

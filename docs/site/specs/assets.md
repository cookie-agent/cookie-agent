# Assets: files agents give the user, and figures in the conversation

Status: **implemented** (2026-11-01; client protocol 1.67.0, plugin
protocol 1.35.0, runtime protocol 1.15.0). The user's decisions are in
section 12, section 13 compares Hermes Agent, and the
[implementation notes](#implementation-notes) say where the code differs.

## 1. Problem

An agent that makes a chart, a diagram, a screenshot, an HTML report, or
any other file for the user has no good way to give it to them:

- **Saying where the file is** leaves the user to find it on the right
  machine and open it there. A phone cannot do that.
- **`read` on an image** shows the result's `media` block inline in the web
  client, but it also puts the image into the model's context, and it reads
  as the agent looking at the file rather than handing it over. In the
  reduced transcript it sits inside the folded `Working…` row.
- **HTML cannot be shown at all.** A `media` block of `text/html` is a
  download link on the web. It cannot be rendered inline safely as things
  stand: a `srcdoc` or `blob:` frame inherits the page's CSP, so its
  scripts never run, and a `blob:` URL opened in a tab runs with the page's
  origin, where it could read the stored daemon token.
- **Nothing outlives the moment.** There is no place where the things an
  agent made for the user are kept and can be found again, updated, or
  used by another session.

What exists already and is reused:

- **The web client** shows images and videos inline, with a full-screen
  viewer (`crates/web/src/media.rs`).
- **The daemon reads files on a machine** with `fs/read_open` and
  `fs/read_chunk` (runtime protocol 1.13.0, from `transfer`).
- **Projects** have their own folder under `~/.cookie-agent/projects/`.
- **Links into the web page** already work: `#session=<id>` (push
  notifications) opens a session.

## 2. Goals and non-goals

**Goals:**

- **Assets:** named files that agents write for the user. They are kept
  by one session tree and deleted with it (the default), or kept per
  project and shared by every session in it.
- **Figures:** an agent shows an asset in the conversation. The web client
  renders images, SVG, video, audio, Markdown documents, and HTML (scripts
  and network included) inline, and every other file as a download card. HTML runs in
  a sandbox that cannot reach the token, the connection, or the page.
- **An Assets tab** in the web client lists the project's assets (a later
  phase, section 11).
- **The TUI** shows a figure row with a link that opens it in the web
  client.
- **The model's context** grows by one line per call, never by the file.

**Non-goals:**

- **Versions.** Writing an asset replaces it (decided 2026-11-01). Its
  figures then show the new content.
- **Restyling a page.** An HTML page is shown as the agent wrote it, with
  no theme, font, or margins injected (decided 2026-11-01). The only script
  added is the one that reports the page's height (section 8.2).
- **Interactive figures:** a page that sends prompts back to the agent.
  This is left for a later round. Such a prompt would have to show as a
  visible user row, because everything the model reads is shown to the
  user.
- **Images in the TUI itself** (kitty, iTerm2, sixel). These can come later
  on top of the same rows.
- **Multi-file HTML,** such as a page that loads `./data.json` from beside
  it. A page must be one self-contained file.
- **Changing an asset's scope,** such as keeping a session asset for the
  project (decided 2026-11-01). It could come later as
  `write_asset(asset, scope: "project")` and a *Keep in assets* button on
  the figure.
- **Users uploading assets** from the Assets tab, and publishing an asset
  at a public URL.

## 3. Assets

An asset is one file with a title, kept in a store (section 4):

| Field | Meaning |
|---|---|
| `id` | A UUIDv7 `AssetId`, fixed for the asset's life. The protocol, storage, and transcript use it. |
| `handle` | `asset_<8 hex>`: what the model sees and types (section 3.1). |
| `title` | Shown on figures and in the Assets tab. When none is given, it is the file name. |
| `media_type`, `byte_length`, `sha256` | Of the current content (section 7). |
| `filename` | The name of the file it was last written from. Downloads use it. |
| `scope` | `session` (the session tree) or `project`, fixed when the asset is made (section 4.1). |
| `created` | When, and by which session tree and agent. |
| `updated` | When it was last written, and by which session tree and agent. |

**No versions.** Writing to an existing asset replaces its content at
once, and the old content's blob is freed when no other asset uses it
(section 4.2).

### 3.1 IDs and handles

The model never handles the UUID. It uses a **handle**, `asset_<8 hex>`,
as subagents have handles (`subagent-handles.md`):

- **Unique where it is used.** A handle is generated when the asset is
  made, and generated again if it collides with one in the store it goes
  to. A session asset's handle is also kept clear of its project's
  handles. Should a session move into a project that already has the same
  handle, the session's own asset wins in that session. The 8 hex digits
  are random. They are not taken from the UUID, whose first digits are a timestamp
  that assets made close together share.
- **It never changes.** A title can change, so the handle is not a slug of
  it. An asset can be rewritten from HTML to PNG, so it has no prefix by
  type either.
- **Paired with the title in every result,** as in
  `asset_3f9a1c2e "Loss curves" (text/html, 48 KB)`.
- **A handle that does not resolve fails,** and the error lists the store's
  assets, newest first, so the model can fix the call at once. It never
  creates an asset instead.
- Every tool also accepts the full UUID.

## 4. Stores and lifecycle

### 4.1 Where assets live

```text
~/.cookie-agent/
  projects/<project-id>/
    project.toml
    AGENTS.md
    assets/
      index.json        every asset's fields (section 3)
      blobs/<sha256>
  assets/               the shared store of sessions in no project
    index.json
    blobs/<sha256>
  sessions/<month>/<root-id>/
    assets/             the session tree's own assets
      index.json
      blobs/<sha256>
```

There are three kinds of store, all with the same index and blobs:

- **A project's assets belong to the project,** not to the session that
  wrote them. Every session of the project, subagents and ACP agents
  included, can list, read, show, write, and delete every one of them
  when its `asset` rule allows (section 6). No asset has an owner.
- **Sessions in no project share one store** (decided 2026-11-01), as the
  clients already group them under "No project".
- **A session's project store** is the one of its project when a run
  starts. A subagent uses its root's.
- **Session assets** (`scope: "session"`, the default) are for something
  quick that does not belong to the project. They belong to the **session
  tree** (decided 2026-11-01) and live in its root's folder:
  - every session of the tree, the root and its subagents, can show, read,
    list, write, and delete them. Sessions of other trees never see them;
  - the Assets tab never lists them, but their figures show and download
    in the tree's transcripts as any figure does;
  - they are deleted with the tree, and they move with it when its root
    changes project.

### 4.2 Blobs and references

- **The index is the only reference.** Session logs name an asset by its
  ID, never by its blob. So a blob is in use exactly when an index entry
  names its sha, and the count is exact. No log has to be swept.
- **Replacing or deleting an asset** writes the index first, then deletes
  the old blob if no entry still names it. Two assets with the same bytes
  share one blob.
- **A crash between the two** leaves an unnamed blob. The store's first
  open after a start removes blobs that no entry names, together with
  half-written temporaries.
- **Writes are atomic.** A blob is written to a temporary file and renamed
  to its sha. The index is rewritten whole and renamed into place.
- **Writers are serialised** by one lock per store, which covers the index
  and blob deletions. Two sessions writing the same asset at once are
  applied one after the other, and the last write wins.

### 4.3 What happens to assets when

| Event | Assets |
|---|---|
| A session tree is deleted | Project assets stay, and the tab shows "written in a deleted session". Its session assets are deleted with it. |
| A session tree moves to another project | Project assets stay in the project where they were written, and its figures of them still render, because a figure names its store (section 10). Its session assets move with it. |
| A project is deleted | They are deleted with it. `project.delete` reports `deleted_assets`, and its confirmation names the count. |
| An asset is deleted | Its figures show "deleted", with who deleted it and when. Model history does not change. |
| An asset is written again | Its figures show the new content, marked "updated" with when. |
| A session is reverted | Nothing changes: assets are not part of a conversation. |
| Compaction | Nothing changes. History holds only the tools' one-line results. |

## 5. Tools

The `assets` group, in `global`. Its tools run in the daemon, which is
what lets ACP agents use them too: an ACP agent takes no routed tools, but
cookie's own tools reach it through the MCP bridge.

| Tool | Does |
|---|---|
| `write_asset` | Saves a file as an asset, or replaces an asset |
| `show_asset` | Shows one or more assets in the conversation |
| `read_asset` | Gives the model an asset's content |
| `list_assets` | Lists the assets the session can use |
| `delete_asset` | Deletes an asset |

### 5.1 The group and its summary

**Deferred by default** (decided 2026-11-01). `assets` joins the built-in
groups that are deferred: the model loads the group with `tool_search` when
it needs it, and the group's summary in the system prompt tells it what the
group is for. `[deferred_tools] always_load = ["assets"]` sends the group up
front instead, and `assets` joins that key's allowed names.

The summary, in the style of the other groups' summaries:

```text
Assets: files you give the user, such as charts, diagrams, HTML pages, and reports, shown in the conversation and kept for this session or the project.
```

The group is offered only to a run whose `asset` rules allow or ask for
something (section 6), so subagents do not see it by default.

### 5.2 `write_asset`

```text
write_asset(
  path: string,                        the file to save
  machine?: string,                    the machine it is on
  asset?: string,                      a handle, to replace that asset
  title?: string,                      at most 200 characters
  scope?: "session" | "project",       default "session"; not with `asset`
  show?: boolean,                      also show it, as show_asset does
  caption?: string,                    with `show`, at most 300 characters
)
```

**Description:**

> Save a file as an asset, a file you give the user, or replace an asset
> you saved before. The file is read from a machine of this session:
> `path` resolves as `read` resolves it, against the machine's working
> directory or under `~`, and `machine` names the machine as the machines
> block and list_runtimes do; omitted, it is the machine of this session's
> workspace. Any file up to 25 MiB works. Images, SVG, video, and audio are
> shown as they are, and a Markdown file (`.md`) as a rendered document,
> for reports and notes. An HTML page is rendered live, with its scripts and
> network access, and must be one self-contained file. Anything else is
> offered as a download. Without `asset` this saves a new asset and
> returns its handle, such as `asset_3f9a1c2e`. With a handle it replaces
> that asset's content, and every place it was shown updates, so revise
> something you saved before by its handle rather than saving it again.
> `scope` is `session` by default: the asset belongs to this conversation
> and its subagents, and is deleted with it. Use `project` for something
> the user will want to find again, in the Assets tab or from another
> session of the project. Saving does not show the asset: pass
> `show: true`, with an optional `caption`, or call show_asset. A shown
> asset shows under the next text you write; name it there by its handle
> to link to it.

**Parameter descriptions:**

| Parameter | Description |
|---|---|
| `path` | The file to save: relative to the machine's working directory, absolute, or under `~` |
| `machine` | A machine of this session, as the machines block names it. Defaults to the machine of this session's workspace |
| `asset` | The handle of an asset to replace. Omit it to save a new asset |
| `title` | Shown with the asset. Defaults to the file name for a new asset, and to the current title when replacing |
| `scope` | `session` (default): this conversation's, deleted with it. `project`: kept in the project's assets. Only for a new asset |
| `show` | Also show the asset in the conversation, as show_asset does. Defaults to false |
| `caption` | With `show`: one short line on what the asset shows |

**Behaviour:**

- **Replacing takes a handle,** so it is always a deliberate step: an
  agent that saves a second `plot.png` gets a second asset.
- **The scope is fixed** when the asset is made (decided 2026-11-01).
  Replacing keeps it, so `scope` given with `asset` is an error.
- **`machine`** is declared by the tool itself, as `transfer` declares
  its machines. Its description names no particular machine, so attaching
  one never changes the definition. For an ACP agent, the session's
  workspace is the agent's own.
- **The run:**
  1. check the path as `read@<runtime>`, and the write under section 6;
  2. read the file with `fs/read_open` and `fs/read_chunk`, up to 25 MiB
     (`MAX_INLINE_MEDIA_BYTES`);
  3. decide the media type (section 7);
  4. store the blob, update the index, and free the old blob (section 4.2);
  5. with `show`, add the figure as show_asset would.
- **The model reads:**
  - `Saved asset_3f9a1c2e "Loss curves" (text/html, 48 KB, session scope), new.`
  - or `…, replaced.`
  - and, with `show`, ` Shown to the user.` after either.
- **In the transcript:** without `show`, one short row, "Saved *Loss
  curves*". With `show`, the asset goes on the shelf (section 8.1).
- **Refusals** are made before anything is stored, and each says why:
  - a file that is missing, is a folder, or is too large;
  - a handle that does not resolve, with the assets listed;
  - `scope` together with `asset`, or `caption` without `show`;
  - a session with no machine to read from, such as a web session that
    started global-only;
  - a runtime without `files: true` ("labpc's runtime is too old to save
    assets").

### 5.3 `show_asset`

```text
show_asset(
  assets: [string],                    1 to 12 handles, shown in this order
  caption?: string,                    at most 300 characters
)
```

**Description:**

> Show one or more assets to the user in the conversation, by their
> handles. They show as cards under the next text you write, as they are
> now: images, SVG, and video as pictures, an HTML page live, a Markdown
> document rendered, and any other file as a download. `caption` is one
> short line on what they show. The user may be on a phone, or in a
> terminal where an asset is a link, so still say in your reply what they
> show. Naming an asset by its handle in your reply, such as
> `asset_3f9a1c2e`, makes it a link the user can open. You get nothing
> back but a confirmation: use read_asset to look at an asset yourself.

**Parameter descriptions:**

| Parameter | Description |
|---|---|
| `assets` | The handles of the assets to show, 1 to 12, in the order to show them |
| `caption` | One short line on what they show |

**Behaviour:**

- **It displays and stores nothing.** The result's display is the caption
  as a `text` block, then one `asset` block per asset (section 10), under
  the hint `figure`.
- **Any asset the session can use** can be shown, a project asset another
  session saved included, and the same asset can be shown more than once.
- **The handles resolve all together.** One that does not resolve fails
  the call before anything is shown, with the assets listed. A handle
  given twice is an error too.
- **The model reads:**
  `Shown to the user: asset_3f9a1c2e "Loss curves" (text/html, 48 KB), asset_8b01d7aa "Confusion matrix" (image/png, 120 KB).`
- **The caption belongs to this showing,** and the title belongs to the
  asset.

### 5.4 `read_asset`

```text
read_asset(
  asset: string,
  offset?: integer,                    1-based first line, for text
  limit?: integer,                     lines, for text
)
```

**Description:**

> Read an asset's content by its handle, as `read` reads a file: text
> with `offset` and `limit` in 1-based lines, an image as an image when
> you take images, and otherwise only the asset's type and size.

- It reads like `read` and is cut like it, and a cut says how to read on.
  An asset is never sent to a model as bytes it cannot take.
- **The model reads** a header line,
  `asset_3f9a1c2e "Loss curves" (text/html, 48 KB, session scope)`, and
  then the content.

### 5.5 `list_assets`

```text
list_assets(
  scope?: "session" | "project",       only that scope; both when omitted
)
```

**Description:**

> List the assets you can use: this conversation's and the project's,
> newest first, each with its handle, title, type, size, scope, and which
> session last saved it and when. At most 100 are listed.

- **One line per asset:**
  `asset_3f9a1c2e "Loss curves" text/html 48 KB · session · saved by this session 4 min ago`.
  For a project asset another session saved, it names that session's
  title and handle.
- **Past 100,** a last line says how many more there are and suggests
  `scope`.

### 5.6 `delete_asset`

```text
delete_asset(
  asset: string,
)
```

**Description:**

> Delete an asset by its handle. Every place it was shown then says it
> was deleted, and it cannot be brought back.

- **The model reads:**
  `Deleted asset_3f9a1c2e "Loss curves".`
- When the user's rules make the call ask, its approval shows the asset
  as a figure, so the user sees what would go.

## 6. Permissions

One action, **`asset`**. Its resource is what a call does:

| Resource | Calls |
|---|---|
| `read` | `read_asset`, `show_asset`, `list_assets` |
| `write` | `write_asset`, creating an asset or replacing one |
| `delete` | `delete_asset` |

**Assets have no owner** (decided 2026-11-01). A rule applies the same to
every asset a session can see, its tree's session assets and its project's
assets (section 4.1), whichever session saved it.

**Defaults,** when the agent document has no `asset` key, follow whether
the run is delegated, as `cron`'s and memory's do:

| Run | Default |
|---|---|
| Root, any agent but an internal one | `asset: { "*": allow }` |
| Delegated, at any depth | `asset: { "*": deny }`, so its asset tools are hidden |
| Internal agent | denied, as every action is: internal agents call no tools |

- **Subagents are denied by default** (decided 2026-11-01): a subagent's
  work goes back to its parent, and the root decides what the user sees.
  A declared `asset` key replaces the default. For example,
  `asset: { read: allow, write: allow }` suits a subagent that makes
  charts, and `asset: { "*": allow, delete: ask }` suits a root whose
  user wants to approve every delete.
- **`write_asset` also checks** its source as `read@<runtime>` on the
  path, as `transfer` checks its source, so the user's `read` rules
  decide which files can become assets.

## 7. Media types

| Type | How it is recognised | Web | TUI |
|---|---|---|---|
| PNG, JPEG, GIF, WebP | Sniffed, as today | `<img>` inline, full screen on click | Figure row |
| SVG (`image/svg+xml`) | `.svg` and an `<svg` root | `<img>`: scripts in an SVG do not run there | Figure row |
| HTML (`text/html`) | `.html` or `.htm`, and valid UTF-8 | Sandboxed frame (section 8.2) | Figure row |
| Markdown (`text/markdown`) | `.md` or `.markdown`, and valid UTF-8 | A rendered document (section 8.1) | Figure row |
| Video, audio | Sniffed, as today | As today | Figure row |
| Anything else (PDF, CSV, zip, …) | Sniffed, else `application/octet-stream` | Download card: name, type, size | Figure row |

- **SVG, HTML, and Markdown recognition is for assets only.** Elsewhere,
  an `.html` or `.md` file `read` as text stays `text/plain`.
- **Markdown** (decided 2026-11-01) is for reports, notes, and other
  documents: what Claude's artifacts show as a document.
- **Any file can be an asset** (decided 2026-11-01). A PDF gets a download
  card for now. Showing it inline needs `frame-src blob:` on the page,
  which can come later.

## 8. The web client

### 8.1 The shelf

Revised 2026-11-01 after the first version shipped: a figure showed as a
tool row that looked the same folded or open. The user chose the shelf
(mockup "Inspiration 1"):

```text
▸ Worked for 1m 12s · 6 tool calls          ← write_asset and show_asset fold here
Run 14 converges fastest: …                  ← the reply
FROM THIS REPLY
[ deck of 3 images ] [ page card ] [ doc card ]
[ file chip ] [ file chip ]
```

- **The calls fold like any other.** A `figure` call has no place of its
  own: it folds into the work row, and open it shows each asset as one
  line ("shown below").
- **A shelf under the text that follows.** The assets a run's `figure`
  calls showed go on a shelf, "From this reply", under the first text the
  run writes after them: the message that comes out of their work, an
  intermediate one or the reply. Each asset is there once, in the order
  first shown, as the latest call showed it. Until that text comes, the
  shelf sits under the work (decided by the user 2026-11-01: it does not
  move down to the reply when the run ends). A text that is only `…` does
  not count.
- **One wrapping row of cards** whose widths let them share rows: the
  Markdown card sits beside the image and page cards, and file chips two
  to a row.
  - **Images, SVG, and video:** two or more are a **deck**, stacked
    cards that fan out on hover, with a count; one is a single card. A
    click opens the media viewer at the card clicked, which steps through
    the deck with its arrows, ← and → on an image, and its filmstrip.
  - **An HTML page:** a **page card**, a live thumbnail of the page (its
    sandboxed frame, scaled down, taking no input) over its title. A click
    opens it in the side panel.
  - **A Markdown document:** a **document card**, its title and first
    lines. A click opens it in the side panel.
  - **Audio:** a card with a player.
  - **Any other file:** a **file chip**: its type, name, size, and a
    download button.
- **The caption** of a call shows under the deck or card of its first
  asset.
- **The side panel** opens beside the transcript on a desktop's width, and
  over it, full screen, on narrower windows. Its head has the asset's
  title and handle, *Preview* and *Source* tabs, *Copy handle*, *Full
  screen* (the whole window, as a figure's did), *Download*, and *Close*
  (Escape too). Preview shows the asset as a figure does (the page
  in its sandbox, filling the panel; the whole document); Source shows the
  file's text.
- **Links in the reply.** A handle the reply's text names (`asset_…`), of
  an asset on a shelf of the session, is a chip with the asset's icon and
  title, which opens it as its card does. The tool descriptions tell the
  model it may name an asset by its handle.
- **Copying a handle,** to give it back to an agent:
  - every card has *Copy handle* in its corner, the side panel in its
    head, and the media viewer in its bar;
  - a chip's tooltip names its handle, and text copied across a chip
    holds the handle, not the title, as the reply's own text does;
  - where the browser cannot copy (a page not served over HTTPS or from
    localhost), *Copy handle* names the handle in the status line, and the
    side panel's head shows it to select.
- **No client checks a tool's name.** The hint `figure` decides what goes
  on the shelf, and an `asset` block shows the same wherever it comes
  from.
- **Live:** a card follows `assets.changed` (section 10). When its asset
  is replaced, it reloads and says it was updated. When it is deleted, it
  says so and no longer opens.
- **Markdown** renders with the transcript's own Markdown renderer
  (`render/markdown.rs`), under the same rules as a reply:
  - raw HTML is escaped, not rendered;
  - links go only where `safe_link` allows, and open in a new tab;
  - images are links, so a document never loads a remote image by itself;
  - code blocks are highlighted.

  A document over 1 MiB is a file chip.

### 8.2 HTML in a sandbox

```text
web page (daemon origin, holds the token)
  └─ <iframe sandbox="allow-scripts allow-popups" src="viewer.html">
       the viewer: opaque origin, its own CSP, holds nothing
       └─ <iframe sandbox="allow-scripts allow-popups" srcdoc="…">
            the agent's page, sent in by postMessage
```

- **`viewer.html`** is a small static file in the web bundle that holds no
  data. With `sandbox` and no `allow-same-origin`, its origin is opaque.
  It cannot read `localStorage`, cookies, the token, or the page's DOM,
  and it cannot use the page's connection.
- **The page reads the asset** with `asset.read` over its own connection
  and sends the bytes to the viewer with `postMessage`. The viewer puts
  them in an inner frame's `srcdoc`, which takes the viewer's policy and
  sandbox. *Reload* makes a new inner frame.
- **Messages carry a token.** Each figure gets a random token, which the
  page sends with the bytes. The page takes a message from a frame only
  when it comes from that frame's window and carries that frame's token.
  Only two kinds are accepted: `ready`, and a height. No page can resize
  or speak for another figure.
- **Its own CSP.** The daemon serves `viewer.html` with its own policy
  instead of the page's:
  - `sandbox allow-scripts allow-popups` is in the header too, so
    `viewer.html` opened directly in a tab is still sandboxed;
  - `frame-ancestors 'self'` and `form-action 'none'`;
  - **the network is open** (decided 2026-11-01): scripts, styles,
    images, fonts, media, workers, `fetch`, and WebSockets from any
    source, with `unsafe-inline` and `unsafe-eval`. The opaque origin is
    what protects the token and the connection, not a list of sources. A
    page has nothing secret to leak but what the agent already put in it,
    and the agent can send that out with `webfetch` or `bash` anyway. So a
    page can load Plotly from a CDN or fetch live data.

  A bundle on a static host gets the same policy from a `<meta>` tag,
  without `sandbox` and `frame-ancestors`, which a `<meta>` tag cannot
  carry. The iframe's `sandbox` attribute still applies there.
- **The page's own CSP is unchanged.** `default-src 'self'` already lets
  the page frame its own origin.
- **Shown as is.** Nothing is added to the agent's page but the height
  script below. The frame's background is white, as a browser tab's is,
  so a page with no background of its own reads the same in a dark
  client.
- **Never in a tab of the page's origin.** HTML has no *Open in new tab*.
  *Full screen* shows the same sandboxed frame over the page, and
  *Download* saves the file.
- **Size follows the content.** The viewer adds a short script to the
  page that reports its content's height whenever it changes. The frame
  follows it between 120 px and 80% of the window, and ignores changes of
  a few pixels, so a page sized in `vh` cannot oscillate. Its width is the
  transcript column's.
- **Sandbox flags:** `allow-scripts` and `allow-popups`, so links open in a
  new tab. There are no forms, no top navigation, no downloads from the
  frame, and never `allow-same-origin`.

### 8.3 The Assets tab (phase 4)

- **A rail entry, "Assets."** It lists the current project's assets (or
  the shared store's), newest first, with a filter by type and a search by
  title.
- **Each row** shows the asset's type icon or thumbnail, title, size, the
  session that last wrote it (a link to that session), and when.
- **Opening a row** shows the asset as a figure does, full width.
- **Row actions:** rename (the title), download, and delete. The user's
  delete never asks for permission, but it is confirmed like any delete.
- **A project's total size** heads the list.

### 8.4 Links

- `#session=<id>&asset=<handle>` opens the session, scrolls to the
  shelf that last showed the asset, and opens it as its card does: an
  image or video in the media viewer, a page or document in the side
  panel.
- `#project=<id>&asset=<handle>` opens the asset in the Assets tab, or
  full screen until the tab exists.

Neither carries the token. A browser that already holds the token
connects as usual, and one that does not shows the connect screen first.

## 9. The TUI

```text
│ ▸ Worked for 1m 12s · 6 tool calls
│ Run 14 converges fastest: validation loss under 0.32 by epoch 25.
│
│ From this reply
│  3 images — Loss curves · Accuracy · Confusion matrix
│    Loss, accuracy, and confusion.
│    ↗ http://127.0.0.1:7300/#session=01JC…&asset=asset_3f9a1c2e
│    ↗ http://127.0.0.1:7300/#session=01JC…&asset=asset_8b01d7aa
│    ↗ …
│  Run explorer — runs.html (text/html, 42 KB)
│    ↗ http://127.0.0.1:7300/#session=01JC…&asset=asset_77c0e1a4
```

- **The calls fold like any other,** and their assets show on a shelf
  under the text that follows them, as on the web: one row per asset
  (images and video grouped under one line naming them), each with its
  caption and link. Open, a call lists its assets, without links.
- **The link:** clicking it, or pressing Enter on it, opens it with the
  system opener (`xdg-open` or `open`) when the TUI's machine has one. The
  URL is always printed in full, so it can be copied over SSH. The TUI
  never opens an asset's file itself (decided 2026-11-01), so an agent's
  HTML never runs as a `file://` page.
- **The base URL** comes from the daemon when the daemon has one, and from
  the TUI otherwise (decided 2026-11-01):
  - **The daemon's `page_url`.** `[server] page_url` in the daemon's
    configuration is where its web page can be reached, such as a
    cloudflared hostname. The daemon reads it and sends it in
    `ServerHello` (section 10), so it is set once and every TUI on every
    machine uses it.
  - **Otherwise, derived by the TUI** from the daemon URL it is attached
    to, as `cookie web` derives it: `ws` becomes `http`, `wss` becomes
    `https`, and `/ws` is dropped. The daemon cannot make this one
    itself, because it does not know the address a client reaches it
    by, which is why push notifications leave its URL out.
  - The link never carries the token.

  | Setup | Link |
  |---|---|
  | A TUI on a laptop, attached to `wss://<tunnel>/ws` | Derived: `https://<tunnel>/` |
  | A TUI over SSH on the server, attached to `127.0.0.1:7419`, with the browser on the laptop | The daemon's `page_url`, since the derived `http://127.0.0.1:7419/` does not open on the laptop |
  | A TUI attached through an SSH port forward, with the browser on the same machine | Derived |

## 10. Protocol and compatibility

**Client protocol, minor:**

- **The `asset` display block:** `{ store, asset }`. show_asset's caption
  is a `text` block before its `asset` blocks.
  - `store` is `{ project: ProjectId }`, `{ shared }`, or
    `{ session: SessionId }`, where `session` is the tree's root.
  - `asset` is the `AssetId`.
  - Clients look up the title, type, and size with `asset.get`, so a
    figure always shows the asset as it is now.
  - **Text rendering** (`to_text`): `asset "Loss curves" (text/html, 48 KB)`
    when the daemon knows the asset, else `asset (deleted)`.
- **The hint `figure`** joins the documented presentation hints.
- **Methods:**
  - `assets.list { store }` returns the assets' fields. The Assets tab
    lists only project and shared stores;
  - `asset.get { store, asset }` returns one asset's fields, or
    `asset_not_found`;
  - `asset.read { store, asset, offset, length }` reads like
    `artifact.read`: at most 1 MiB, base64 `data`, `total_bytes`, and
    `media_type`;
  - `asset.rename { store, asset, title }`;
  - `asset.delete { store, asset }`.
- **The event `assets_changed { store, upserted: [Asset], removed:
  [AssetId] }`.** Figures and the tab follow it.
- **`project.delete`** also returns `deleted_assets`.
- **`ServerHello` gains `page_url?`**, the daemon's `[server] page_url`
  when it is set. Clients use it to build links into the web page
  (section 9).

**Older clients** see an `asset` block as its text rendering, so every
client shows something.

**Other parts:**

- **Runtime protocol:** no change. `write_asset` needs `files: true`
  (1.13.0).
- **Daemon:** serves `viewer.html` with its own CSP (section 8.2).
- **Configuration:**
  - the `asset` permission action and its defaults;
  - `assets` in `[deferred_tools] always_load`;
  - `[server] page_url`, optional: an `http` or `https` URL, which the
    daemon reads, as it reads `host` and `port`. Clients on its machine
    do not read it, unlike `url`. The configuration reference says which
    side reads each key of `[server]`.
- **History:** results are one line of model text. An `asset` block names
  an ID, so a log never holds asset bytes and needs no converter.

## 11. Phases

1. **Store and tools:**
   - the stores, index, blobs, and lock;
   - the five tools and the `asset` permission;
   - SVG and HTML recognition;
   - the `asset` block, the `figure` hint, the methods, and the event;
   - `deleted_assets` on project deletion;
   - engine tests for:
     - create, replace, and delete, with blob counting;
     - crash leftovers;
     - concurrent writers;
     - the `asset` defaults for roots, subagents, and internal agents;
     - the deferred group and `always_load`;
     - handles, and errors that list candidates;
     - model text;
   - `[server] page_url` and `ServerHello.page_url`.
2. **Web figures:**
   - the figure card and download card, and their place outside the fold;
   - `viewer.html` with its CSP, the inner frame, the token, and the
     height script;
   - full screen, download, and reload;
   - live updates;
   - the `#…&asset=` links;
   - headless-browser tests that a script in the frame runs, cannot read
     `localStorage`, cannot reach the page's connection, and cannot move
     another figure's frame.
3. **TUI:**
   - the figure row;
   - the link, with the daemon's `page_url` and the derived fallback;
   - the system opener.
4. **The Assets tab** (section 8.3).
5. **Documentation:**
   - the tools in `reference/tools.md`;
   - the permission in the agents guide;
   - the block, methods, and event in the protocol reference;
   - a user guide page on assets.

## 12. Decisions

**Decided by the user (2026-11-01):**

- **Project assets are project-wide.** Every session of a project can
  read, list, write, and delete them. Sessions in no project share one store.
- **Agents can delete assets,** and write over them, whichever session
  saved them.
- **One `asset` permission action** with the resources `read`, `write`,
  and `delete`, and no owners: a rule applies the same to every asset,
  whichever session saved it. Defaults are `"*": allow` for roots and
  `"*": deny` for subagents at any depth. This replaces the earlier rule
  that deleting another session's asset asks first.
- **No versions.** A write replaces the asset and frees the old blob when
  nothing else uses it.
- **Tools are named `*_asset`.** `write_asset` both creates and updates,
  and `show_asset` only displays.
- **HTML is shown as is,** with no theme CSS injected.
- **The network is fully open for HTML:** `fetch`, WebSockets, and every
  other source. The opaque-origin sandbox is the isolation.
- **Any file can be an asset.** Files the client cannot render get a
  download card, PDFs included for now.
- **Interactive figures** are left for a later round.
- **Markdown** assets render as documents.
- **The shelf** (revised 2026-11-01, mockup "Inspiration 1"): figure
  calls fold like any other, and a run's assets show as cards under its
  reply: a deck for images and video, a page card and a document card
  that open in a side panel, file chips; one wrapping row whose cards
  share rows. Chips for handles named in the reply.
  The user added: the shelf shows under the text that comes out of the
  work, and stays there; the side panel is beside the chat on a desktop
  and can go full screen; a handle can always be copied.
- **Session assets, the default scope.** `scope: "session"` keeps an asset
  to the session tree that wrote it: every session of the tree can use
  it, other trees cannot reach it, the Assets tab does not show it, and it
  is deleted with the tree. `scope: "project"` is the option.
- **The scope never changes** for now.
- **The asset tools are deferred by default,** as a built-in group.
- **IDs:** a UUIDv7 inside cookie, and an `asset_<8 hex>` handle for the
  model (section 3.1). There is no URL form for now.
- **Writing and showing in one call:** `write_asset(…, show: true,
  caption)`.
- **One or several assets per `show_asset`** call, 1 to 12, shown
  together.
- **The TUI only links** to the web client. It never opens an asset's
  file itself.
- **The link's base URL** is the daemon's `[server] page_url`, read by the
  daemon and sent in `ServerHello`. Without it, the TUI derives one from
  the URL it attached with.
- **`write_asset` takes a file only,** never a string of content. A session
  with no machine to read from cannot save assets (section 5.2).

No questions are open.

## 13. How Hermes Agent does it

Read from NousResearch/hermes-agent at `c7da41dd` (2026-11-01):

- **Text tags, not tools.** The agent writes `MEDIA:/absolute/path` in its
  reply, and each surface decides what to do with it, with a system-prompt
  hint per surface:
  - messaging platforms upload the file as an attachment;
  - the desktop app shows images, audio, and video inline, and any other
    file as a card;
  - the API server inlines images as base64;
  - the CLI and TUI do nothing, and the model is told to write the path
    instead.
- **What tags cost them:**
  - a path denylist, and a strict mode for public bots, so a prompt
    injection cannot use `MEDIA:` to send out secrets;
  - fetching files from remote sandboxes;
  - repairing paths the model mangled;
  - holding back half-streamed tags.

  Cookie's tools avoid all of this. Paths go through `read` rules and
  runtimes, and a bad path is an error the model sees.
- **Live HTML widgets.** `::preview{file="x.html"}` in the desktop app
  renders the page in an opaque-origin frame (`allow-scripts`, no
  `allow-same-origin`):
  - its height follows the content through `postMessage` reports tagged
    with a token;
  - the app's theme is injected, which cookie does not do;
  - widgets can send hidden prompts back (`hermes.send`).

  In a browser, the shell page is served with its own CSP, which includes
  `sandbox`, and the HTML arrives by `postMessage` into a nested frame,
  because a `srcdoc` would inherit the app's CSP. The network is fully
  open. Section 8.2 takes the nested frame, the `sandbox` in the header,
  the token, and the open network from this.
- **Files are read live** from disk whenever a message renders, so there
  is no store, and the agent updates a widget by rewriting its file.
  Cookie's assets keep the "update in place" part, but in a store, so they
  work from any machine and outlive the file.

## 14. Alternatives considered

- **Tags in the reply** (`MEDIA:/path`, as Hermes does, or Markdown
  `![](plot.png)`). Rejected for these reasons:
  - the costs listed in section 13;
  - the path names a file on some machine, which the client would have to
    fetch from there when it renders;
  - the web never loads images on its own (`render/markdown.rs`).
- **Assets in the session tree's artifact store.** Rejected:
  - assets would die with their session, and other sessions could not see
    them;
  - the artifact store frees blobs by sweeping session logs, which only
    works when no log names a blob, and assets do not need it.
- **Versions.** Rejected by the user. A history of every write costs
  storage and a version picker, and figures that update in place are what
  "update the chart" should mean.
- **Replacing on a matching name.** Rejected: an agent showing a second
  `plot.png` would silently overwrite the first. Replacing takes a handle.
- **An authenticated HTTP URL per asset** (`/view/<capability>`).
  Rejected:
  - it is a new authentication surface;
  - URLs that work as capabilities leak into browser history and the
    transcript.

  The `postMessage` viewer needs no URL that grants anything.
- **A `srcdoc` or `blob:` frame in the page.** Rejected: either inherits
  the page's CSP, so scripts do not run, and loosening that CSP would
  weaken the page itself.
- **Content as a string** (`write_asset(content, filename)`), for
  sessions with no machine and quick snippets. Rejected by the user: an
  asset is always saved from a file, so a whole document never sits in a
  call's arguments in history.
- **Routed runtime tools.** Rejected: ACP agents take no routed tools, and
  the daemon can already read files from runtimes.

## Implementation notes

Implemented on 2026-11-01 (client protocol 1.67.0, plugin protocol 1.35.0,
runtime protocol 1.15.0). Where the code differs from the sections above:

- **`assets.changed` is an empty notification,** coalesced like
  `memory.changed`, instead of section 10's `{ store, upserted, removed }`:
  figures read their assets again with `asset.get`, and the Assets tab
  lists its store again with `assets.list`.
- **The `asset` block also carries `handle` and `sha256`.** A figure
  compares the `sha256` it was shown with with the asset's current one to
  say it was updated.
- **The store lock is the process's,** as for cron jobs: two daemons
  sharing a data root are not serialized.
- **Full screen on the web** is an overlay over the whole page with a frame
  of its own, so an HTML page runs again there; closing it leaves the
  figure's own frame as it was.
- **Only `#session=<id>&asset=<handle>` links exist.** Section 8.4's
  `#project=<id>&asset=<handle>` is not implemented: nothing prints one,
  since the TUI links to a figure in its session.
- **The Assets view** opens on the open session's project, and shows no
  session's own assets, as section 8.3 says; it has no thumbnails, only an
  icon by kind.
- **The TUI's page URL** comes from the handshake when the TUI starts, or
  from the address it attached at. A daemon whose `page_url` changes later
  is seen at the TUI's next start.

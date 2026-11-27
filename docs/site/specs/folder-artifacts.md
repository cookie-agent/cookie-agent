# Folder artifacts: a page with its files, and every file over HTTP

Status: **approved** (2026-11-26), being implemented. Decisions are in
[section 11](#11-decisions); where the code differs, see the
[implementation notes](#implementation-notes).

## 1. Problem

An attachment or an asset is one file. An HTML page shows in a sandboxed
frame whose `srcdoc` is the page's bytes ([assets](assets.md) section 8.2),
so a relative reference in it, `<img src="loss.png">`,
`<script src="app.js">`, `fetch("data.json")`, resolves against
`viewer.html`'s address and finds nothing. The docs and `write_asset`'s
description say so: "A page must be one self-contained file."

An agent that wants a page with figures has to inline every image as
base64 and every script and data file into one HTML file. That is slow for
the model (it writes the encoding, or a build step to do it), makes files
that are hard to read back with `read_asset`, and runs into the 25 MiB cap
sooner, since base64 adds a third. The same applies to a Markdown report
whose images are files beside it (`![](figs/loss.png)`), which shows its
images broken today.

The user's sim and training code already writes report folders
(`index.html`, `figs/`, `videos/`, `data.json`). The agent should be able to
attach or save the folder as it is.

## 2. Goals and non-goals

Goals:

- **A folder with a main file** is one attachment or one asset. It shows as
  its main file shows: an HTML main file runs in the same sandbox as today,
  and its relative references reach the folder's other files.
- **The agent names a folder** where it names a file today:
  `<attach src="report/"/>` and `write_asset(path: "report/")`.
- **Markdown main files** show their relative images, and link to the
  folder's other files.
- **Share links** serve a folder, with the same relative references working
  on the landing page and in *Open full page*.
- **Downloads** give a `.zip`, and `transfer` from a folder artifact writes
  the folder.
- **Every file over HTTP too.** A single-file attachment or asset keeps
  its storage and events, and gets a URL on the same route as a folder's
  files. The web client loads it over HTTP and falls back to the WebSocket
  when HTTP does not work (section 6.6).

Non-goals:

- **Uploading a folder** from a client (the composer's attach, a drop of a
  folder). The daemon could take one later through the same manifest
  (section 3), but no client offers it in this spec.
- **Editing one file of a folder asset.** Writing an asset again replaces
  the whole folder, as it replaces a file today. No versions.
- **A web server.** No directory listings, no server-side code, no
  rewriting of the page, no `index.html` for a folder below the root.
- **Workers, service workers, and pages that read `location.href`** to build
  URLs. Section 6.3 says what does not work.
- **Sending a folder to a model.** A model gets a folder's label (section
  7), never its files, as for a `file` upload today.

## 3. A folder in the stores

A folder is stored as its files and one **manifest**, all in the store's
existing content-addressed `blobs/<sha256>`:

```json
{
  "cookie_folder": 1,
  "main": "index.html",
  "entries": [
    { "path": "data.json",     "sha256": "…", "size": 18342,  "media_type": "application/json" },
    { "path": "figs/loss.png", "sha256": "…", "size": 120044, "media_type": "image/png" },
    { "path": "index.html",    "sha256": "…", "size": 9120,   "media_type": "text/html" }
  ]
}
```

- **The folder's digest is the manifest's.** The manifest is written in one
  canonical form (keys in this order, entries sorted by `path` as bytes, no
  whitespace), so the same folder always has the same digest, and two
  folders that share files share their blobs.
- **Paths** are relative, `/`-separated, NFC-normalized UTF-8, at most 1024
  bytes, with no empty, `.`, or `..` component and no leading `/`. Two
  paths that differ only by case are both kept; a client that unpacks onto
  a case-insensitive disk gets the later one (section 8).
- **Only files are entries.** Folders are implied by paths; an empty folder
  is dropped. A link inside the folder (the tar stream keeps those, see
  [runtime protocol](../reference/runtime-protocol.md#transfers)) becomes an
  entry with its target's digest; a link to a folder inside the folder is
  expanded; a link that leaves the folder is already left out by the
  runtime and counted as skipped.
- **Media types** are decided per file as an asset's are
  ([assets](assets.md) section 7): by content first, then by extension,
  which matters here for `.js`, `.css`, `.json`, and `.wasm`.
- **`main`** names an entry. It decides how the folder shows (section 5).

### 3.1 How a reference says it is a folder

The places that name an artifact or asset gain one optional field:

```text
folder?: {
  main: string,        the main file's path
  files: integer,      entries in the manifest
}
```

- **`ArtifactRef` and `PersistedFilePart`** (an attachment's `file`): the
  `sha256` is the manifest's, `byte_length` the sum of the entries' sizes,
  `media_type` the main file's type, and `filename` the folder's name. A
  client that shows a card by media type shows an HTML folder as a page
  card without further changes.
- **The asset index entry and the `asset` display block:** the same, with
  the asset's `sha256` the manifest's.
- **Reading a folder by its digest** with `artifact.read` or `asset.read`
  without a `path` (section 4) reads the **main file**, so a client or tool
  that reads "the content" gets the page, never the manifest JSON.

### 3.2 References and cleanup

The stores count references by digest. A folder makes them transitive:

- **Session artifact stores.** A fork takes the blobs its history names;
  for a `file` part with `folder`, that is the manifest and every entry.
  A revert collects a blob only when no remaining record names it, either
  directly or through a manifest a remaining record names.
- **Asset stores.** A blob is in use when an index entry names it, or names
  a manifest that lists it ([assets](assets.md) section 4.2 now says "the
  index is the only reference"; with folders, it is the index and the
  manifests it names). Replacing or deleting an asset frees the manifest
  and each entry no other asset uses. The startup sweep of unnamed blobs
  reads the manifests of folder entries, which the index marks, so it never
  sniffs blobs.
- **Writes** store every entry first, then the manifest, then the index or
  log record, so a crash leaves only unnamed blobs for the sweep.

## 4. Protocol

Additive; one minor version of the client protocol, and of the plugin
protocol for the fields it mirrors.

| Method | Change |
|---|---|
| `artifact.read` | Optional `path`: read that entry of a folder artifact. Without it, a folder reads its main file. A `path` on a file artifact, or one not in the manifest, is `-32602`. |
| `asset.read` | The same `path`. |
| `artifact.manifest` (new) | `session_id`, `sha256` → `main`, `entries` (`path`, `size`, `media_type`). For the side panel's file list and the share landing page. |
| `asset.manifest` (new) | `store`, `asset` → the same, for the asset's current content. |
| `content.urls` (new) | `targets`: up to 64 of `{ session_id, sha256 }` or `{ store, asset }`, a file or a folder → `urls`, one per target in order: a file's URL or a folder's base on the `/f/` route (section 6.1), or `null` for a target that does not resolve. It computes keys and stores nothing. A share guest gets paths under its share link instead (section 6.5). |
| `attachment.save_asset` | Unchanged signature; a folder attachment makes a folder asset. |

`ArtifactRef.folder`, `PersistedFilePart.folder`, and `AssetBlock.folder`
are the fields of section 3.1. History stays at its current format: the
field is optional. A daemon that predates it, run on a store this one
wrote, would show a folder part as a file whose bytes are the manifest.

## 5. Showing a folder

A folder shows as its main file would, with its relative references
resolved:

| Main file | Card | Side panel |
|---|---|---|
| HTML | A live thumbnail, as a page | The page, in the sandbox of section 6 |
| Markdown | Its first lines | The rendered document; relative image sources and links resolve into the folder (section 6.2) |
| Anything else | As that file's card today | As that file today |

- **The side panel gains a *Files* tab** beside *Preview* and *Source*,
  listing the manifest's entries (path, type, size); clicking one shows it
  as a single file of its type would show, read with `path`. *Source*
  shows the main file.
- **The card's meta line** says it is a folder: `Report · HTML · 14 files ·
  3.2 MB`.
- **Download** saves `<folder name>.zip` (section 8).
- **The Assets view** lists a folder asset by its main file's kind, with a
  folder badge on the card. Its thumbnail is the main file's (an HTML or
  Markdown main has none today, as now).
- **The TUI** shows `📎 report/ · HTML · 14 files · 3.2 MB`, with the same
  web link as any attachment or asset.

## 6. Serving files, and the sandbox

### 6.1 The `/f/` route and its key

Every artifact and asset, a folder's files and a single file alike, is
served by the daemon over HTTP, at a URL whose key only the daemon can
make:

```text
<page origin>/f/<store>/<sha256>/<key>/<path>             a folder's entry, inline
<page origin>/f/<store>/<sha256>/<key>.zip                a folder, as a download
<page origin>/f/<store>/<sha256>/<key>/<name>             a file, inline
<page origin>/f/<store>/<sha256>/<key>/<name>?download    a file, as a download
```

- **`<store>`** names the store the content is in (a tree's artifacts, a
  tree's assets, a project's assets, or the shared store), `<sha256>` the
  file or the folder's manifest, and **`<key>`** is
  `HMAC-SHA256(daemon token, "cookie-folder/1" ‖ store ‖ sha256)`, cut to
  128 bits, base64url. It is computed as the page ticket is
  (`plugin_pages::page_ticket`): nothing is stored, and there is no expiry
  or renewal.
- **A file's `<name>`** is for the reader and the download, as a share
  link's is: the file name, or the title slugged with its extension. The
  daemon ignores it.
- **The web page asks for URLs** with `content.urls`, a batch for the
  cards it is about to show. The key is not put on `ArtifactRef` or
  `AssetBlock`: those are saved in history, where a key would outlive a
  token rotation. The page caches the answers for the connection.
- **Only the owner's page gets a key.** A share guest's `content.urls`
  answers a path under its share link (section 6.5), never a key, so
  revoking the link ends the guest's access to its folders.
- **The URL names content, not an asset.** An asset's URL is for its
  content at the time; when the asset is rewritten, its figures ask again
  and get the new content's URL, as they read again today. So a page never
  mixes files of two writes.
- **A URL works while the store still holds that content**, and until the
  daemon token is rotated, which changes every key. Deleting or rewriting
  the asset, or deleting the session tree that holds the attachment, ends
  it.
- **The route takes no token,** page ticket, or `Origin` check, as `/s/`
  does, because the page's requests cannot carry a header the browser does
  not add (section 12). The daemon recomputes the key from the path. It
  serves `GET` and `HEAD`, with range requests, and answers one `404` for
  every failure (a wrong key, a path not in the manifest, content the
  store no longer references).
- **Headers on every answer:** the file's media type (`text/*` with
  `charset=utf-8`), `Accept-Ranges: bytes`, `Cache-Control: private, max-age=31536000, immutable`
  (content-addressed, so a cache is never stale), `Referrer-Policy:
  no-referrer`, `X-Content-Type-Options: nosniff`, `X-Robots-Tag: noindex`,
  and `Access-Control-Allow-Origin: *` without credentials, because the
  page that loads them has an opaque origin (section 6.2). HTML and SVG
  entries get the CSP `sandbox` header that `raw` gets for share links
  ([share links](share-links.md) section 5.2), so a file opened at the
  top level cannot act as the daemon's origin. A PDF is served as share
  `raw` serves one, without a sandbox, which the browser's viewer refuses,
  and with `frame-ancestors 'self'`. `?download` adds
  `Content-Disposition: attachment` with the name.
- **On the share-only port** (`[public] listen`) the `/f/` route is not
  served. Share links have their own path (section 6.4).

### 6.2 The page

The page and the viewer keep today's architecture: the page reads the main
file with `asset.read` or `artifact.read`, and sends the bytes to
`viewer.html`, which puts them in the inner frame's `srcdoc`. For a folder
it also sends the folder's absolute base from `content.urls`, and the
viewer inserts

```html
<base href="https://<daemon>/f/<store>/<sha256>/<key>/<main's folder>/">
```

at the start of the document, after a `<!doctype>` when there is one (a
`<base>` before it would put the page in quirks mode).

- **Every relative URL** the document resolves then goes to the folder:
  `src`, `href`, CSS `url()`, `@import`, `<script type="module">` and its
  `import`s, `fetch`, XHR, `<video>` and `<audio>`, `srcset`.
- **The sandbox is unchanged:** both frames keep `sandbox="allow-scripts
  allow-popups"` and an opaque origin, and `viewer.html` keeps its policy,
  whose open network already allows the daemon's address. The files'
  `Access-Control-Allow-Origin: *` is what lets a module script or `fetch`
  from origin `null` read them.
- **`#name` links** still stay on the page: the viewer's link script
  already handles them, and a `<base>` makes it needed for every `#` link.
- **A page with its own `<base href>`** loses it, since the first one wins.
  The viewer's is first.
- **The web bundle on a static host** reaches the daemon at its WebSocket
  address, so it builds `base` from that address, not the page's.
- **Markdown** main files render in the page itself, not a frame. Their
  relative image sources become URLs under the folder's base, which the page's own
  CSP allows (`img-src` includes the daemon's origin; the static-host
  bundle adds the daemon's address). A relative link to another entry
  opens that entry in the panel's *Files* tab; a link to an `.html` entry
  opens it as a page in the sandbox, with the same base.

### 6.3 What does not work

Beyond the sandbox's existing limits ([attachments](message-attachments.md)
section 7):

- **Workers and service workers** from the folder: a worker's script must
  be same-origin with the page, and the page's origin is opaque. A page can
  still make a worker from a `blob:` URL of a script it fetched.
- **`location.href`, `document.URL`, and `import.meta.url`** say
  `about:srcdoc`, not the folder. A page that builds URLs from them, rather
  than with relative URLs, misses its files. `document.baseURI` is the
  folder.
- **Absolute-path references** (`/figs/loss.png`) resolve to the daemon's
  root, not the folder, as on any static host serving a subfolder.
- **Pages over 25 MiB of HTML** are refused as today; the cap applies per
  file (section 7.1).

### 6.4 Share links

A share link to a folder asset serves its entries under the existing `raw`
prefix, whose trailing file name the daemon ignored until now:

```text
<share base>/s/<id>                     the landing page
<share base>/s/<id>/raw/<path>          an entry, with the headers of 6.1
<share base>/s/<id>/download/<name>.zip the folder as a zip
```

- **For a folder**, the path after `raw/` names an entry. The landing
  page's frame and *Open full page* open `raw/<main>`, so the page's
  relative references resolve to `raw/<path>` with no `<base>` at all.
  A share link to a single file keeps ignoring the name.
- **Live, as share links are:** a rewrite serves the new folder at once.
  A page open during a rewrite may load a file of the new content; the
  landing page loads the main file with `no-store` as now.
- **The landing page** gains the file count in its meta line and a
  collapsed list of entries, each linking to its `raw` URL.
- **Markdown** main files rendered by the daemon get `raw/<path>` image
  sources, which its `img-src 'self'` already allows.
- **The share-only port** serves these paths too, as it serves `/s/` now.

### 6.5 Conversation share guests

A guest of a [conversation share](conversation-shares.md) sees the
folders of the shared session through its own link:

```text
<share base>/s/<id>/f/<store>/<sha256>/<path>     a folder's entry
<share base>/s/<id>/f/<store>/<sha256>/<name>     a file
```

- **Checked on every request:** the link is live, it is a conversation
  share, and the content is one the shared session tree can show (its
  artifacts, its session assets, or its project's assets). Anything else
  is the share route's one `404`.
- **Nothing new is stored.** The share record that already exists is the
  key, so revoking the link, or its expiry, ends the guest's URLs with it.
- **Same headers** as the `/f/` route (section 6.1), including the CSP
  `sandbox` on HTML and SVG and `Access-Control-Allow-Origin: *`, but with
  `Cache-Control: no-store`, as every share answer has, so a revoked
  link is not served from a cache in front of the daemon.
- **The share-only port** serves these paths, as it serves the guest page.

### 6.6 Single files over HTTP, with a WebSocket fallback

The web client reads every file it shows over HTTP first, and over the
WebSocket (`artifact.read`, `asset.read`) when HTTP does not work. HTTP
lets the browser stream, seek, and cache; the WebSocket stays the path that
always works, since the page already holds that connection.

| What | Over HTTP | The fallback |
|---|---|---|
| An image, a video, audio | The element's `src` is the file's URL, so a video seeks with range requests and plays before it has all arrived | On the element's `error`, the bytes are read over the WebSocket into a `blob:` URL, as today |
| A PDF | The reader's frame loads the URL | As today |
| An HTML page | The page `fetch`es the main file and sends it to the viewer | `artifact.read`, as today |
| Markdown, text, code | `fetch` | `artifact.read`, as today |
| *Download* | A hidden link to the URL with `?download`, which the browser streams to disk | Read whole over the WebSocket, as today |
| A folder's files | The `<base>` of section 6.2 | None: a frame's own requests cannot use the WebSocket. The main file shows, read over the WebSocket, with a note on the card, "This page's files could not be loaded" |

- **When HTTP does not work:** a request fails at the network level, the
  URL answers `404` though `content.urls` just gave it, or no URL comes
  back. A daemon behind a proxy that forwards only `/ws`, or a static-host
  bundle whose daemon address the browser blocks for HTTP, are the cases
  this is for.
- **The client remembers for the connection.** After the first failure it
  reads over the WebSocket for the rest of the connection, so a page full
  of images does not fail once per image. A new connection tries HTTP
  again.
- **Thumbnails** stay on `asset.thumbnail`, read over the WebSocket and kept
  in Cache Storage ([asset thumbnails](asset-thumbnails.md)).
- **Share guests** use the paths of section 6.5 the same way, with the
  same fallback over their guest connection.
- **The TUI** reads nothing itself and is unchanged.

## 7. The agent's side

### 7.1 Limits

A folder is read once, as a file is, and the daemon keeps its bytes, so it
has caps of its own (section 11, question 1):

| Limit | Value |
|---|---|
| One entry | 25 MiB (`MAX_INLINE_MEDIA_BYTES`, as a file) |
| The folder's entries together | 200 MiB |
| Entries | 2000 |
| A path | 1024 bytes |

The read fails as soon as a limit is passed, saying which, and stores
nothing. Hidden files and folders (`.git/`, `.DS_Store`) are entries like
any other; the entry count and size caps catch a folder that holds a
checkout by mistake, and the error names the largest subfolders.

### 7.2 `<attach>`

```markdown
<attach src="runs/report/" title="Run 14 report"/>
<attach src="runs/report" main="summary.md"/>
```

- **`src` may name a folder** (a trailing `/` is optional). It is checked
  as `read` on the folder's path, as `transfer` checks a folder, and read
  with `fs/read_open` as a tar stream, which the daemon unpacks into blobs
  and a manifest.
- **`main`** is a new optional attribute, a path inside the folder. Omitted:
  `index.html` if the folder has one at its top, else `README.md`, else
  `index.md`, else the folder's only top-level file; otherwise the tag
  fails with "the folder has no index.html; name its main file with
  main=…".
- **The `attachments` prompt block** gains one sentence: "`src` may name a
  folder whose files a page or Markdown document references by relative
  paths; it shows as its `index.html`, or the file `main="…"` names."
- **Failures** add: the folder is over a limit; `main` is not in the
  folder. "is a folder" is no longer a failure.

### 7.3 Assets

- **`write_asset`** takes a folder as `path`, and a new optional `main`,
  with the same default as the tag. Replacing an asset may change it
  between a file and a folder, as it may change its type today. Its
  description replaces "must be one self-contained file" with: "An HTML
  page is rendered live, with its scripts and network access; to give it
  images, scripts, or data as separate files, save the folder that holds
  them and its relative paths will work. `main` names the page when it is
  not `index.html`."
- **The result line** says what was saved: `Saved asset_3f9a1c2e "Run 14
  report" (folder, 14 files, 3.2 MB, main index.html, session scope), new.`
- **`read_asset`** takes an optional `path` and reads that entry as it
  reads a file. Without `path`, a folder's header line is followed by its
  entries, one per line (path, type, size), up to the tool's line limit,
  then the main file's first lines; a cut says to pass `path`.
- **`list_assets`** shows `folder, 14 files` in place of the type.
- **The `asset` permission action** is unchanged: the folder is one
  resource. The read of the folder is checked as `read` on its path.

### 7.4 Artifacts, labels, and `transfer`

- **A folder's label** for a model, where an attached folder appears in
  history: `[folder report/, 14 files, 3.2 MB, main index.html, artifact
  5f2c81d0]`.
- **`read_artifact`** takes an optional `path`, as `read_asset` does.
- **`transfer`** from a folder artifact (or a folder asset, once
  [assets](assets.md) and `transfer` share sources) writes a folder at the
  destination: the daemon turns the manifest into the tar stream
  `fs/write_open` takes with `kind: folder`. Files get mode `0644`; the
  original modes and times are not kept (section 11, question 4).

## 8. Downloads

- **The zip** is made by the daemon as it streams: entries stored without
  compression when their media type is already compressed (images, video,
  audio, `zip`, `gz`, `woff2`), deflated otherwise; paths under one top
  folder named after the folder; times set to when the folder was stored.
  No size is known in advance, so the answer has no `Content-Length`.
- **The web's *Download*** opens the folder's `.zip` URL in a hidden link with
  `download`, so the browser streams it to disk; it never builds the zip in
  memory. Without HTTP, a folder has no download; a file is read over the
  WebSocket as today (section 6.6).
- **The TUI** has no download; its link opens the web client.

## 9. Security notes

- **A content URL is a bearer secret** for one file or folder, with no
  expiry. Every artifact and asset has one, an uploaded file or a tool's
  kept output as much as a page. It
  appears in the page's frame and in requests the page makes. A page that
  sends `document.baseURI` elsewhere leaks a URL to its own files, which it
  could send directly anyway, reachable only at the daemon's address. A
  leaked URL works until the content is deleted or rewritten, or the token
  is rotated, as a share link with *Never* works until it is revoked.
- **A share guest never holds a key.** Its folder URLs are under its
  share link (section 6.5), so revoking or expiring the link ends them at
  once, as it closes the guest's connection.
- **The token never reaches the page.** Only the key does, and a key opens
  that one folder's files and nothing else on the daemon.
- **The daemon's origin stays clean.** Entries the page loads are
  subresources of an opaque-origin frame. An entry opened at the top level
  (a link with `target="_blank"` to an `.html` or `.svg` entry) gets the
  CSP `sandbox` header, as share `raw` does. No entry can read the token,
  `localStorage`, or the page ticket cookie, and its requests carry no
  credentials.
- **`Access-Control-Allow-Origin: *`** exposes only the folder's own
  bytes to whoever holds its URL, who could already `GET` them.
- **Paths are matched against the manifest**, never joined onto a disk
  path, so the route cannot reach other blobs or files.
- **Unpacking** applies the runtime protocol's rules again in the daemon
  (no absolute names, no `..`, no entries below a link the stream made), so
  a runtime that sends a hostile tar cannot name a path outside the
  manifest's rules.

## 10. Phases

1. **Stores and protocol:** the manifest, `folder` on the three references,
   `path` on `artifact.read` and `asset.read`, `*.manifest`, transitive
   references in forks, reverts, asset replacement, and the startup sweep.
2. **The agent's side:** folders in `<attach src>` and `write_asset` with
   `main`, `read_asset` and `read_artifact` with `path`, the label, the
   prompt and description text, `transfer` from a folder artifact.
3. **The web:** `content.urls`, the `/f/` route for files and folders, the
   viewer's `<base>`, Markdown with relative images, the *Files* tab, the
   folder badge, zip downloads; every file read over HTTP with the
   WebSocket fallback (section 6.6). The TUI's line.
4. **Share links:** `raw/<path>` for folders, the landing page's list, the
   zip download.

Each phase is one commit and push; the docs (`sessions/assets.md`,
`reference/tools.md`, `reference/protocol.md`, the protocol changelog) move
with the phase that changes them.

## 11. Decisions

Decided by the user on 2026-11-26, as recommended:

1. **Limits** are fixed as section 7.1 drafts them: 200 MiB and 2000
   entries per folder, 25 MiB per entry. No configuration key until one is
   asked for.
2. **The default main file** is `index.html`, then `README.md`, then
   `index.md`, then the folder's only top-level file (section 7.2).
3. **No modes or times through `transfer`.** The manifest keeps none, and
   files come back `0644` (section 7.4).
4. **Folder uploads from the clients** are left to a later spec (section 2).
5. **Folder URLs carry a key derived from the daemon token**, computed as
   the page ticket is, with no stored state, no expiry, and no renewal
   (section 6.1). The user asked to keep this simple rather than manage
   expiring grants. Share links keep using their own ID (section 6.4).
6. **Single files are served over HTTP too**, on the same route, and the
   web client tries HTTP first and falls back to the WebSocket (section
   6.6). Asked for by the user on 2026-11-26.

## 12. Alternatives considered

- **Inlining in the viewer:** rewrite the page's references to `blob:` or
  `data:` URLs before putting it in `srcdoc`. Only static references
  (`src`, `href`, simple CSS `url()`) can be found; `fetch`, module
  imports, and URLs built in script cannot. Rejected as half-working.
- **A service worker in the viewer** answering the page's requests from
  bytes the page sends in. Service workers need a real origin; the
  viewer's is opaque by design. Rejected.
- **The frame's `src` at the folder's `index.html` URL,** with no `srcdoc`. The
  page's `location` would be right, but the viewer could no longer add its
  height and `#name` scripts to a frame of another origin; the daemon would
  have to inject them when serving HTML, which rewrites the agent's file.
  Rejected for the `<base>` insertion, which keeps today's viewer.
- **The daemon token in an `Authorization` header.** The browser sends no
  custom header with the requests a page makes for `<img>`, `<link>`,
  `<script>`, `<video>`, or CSS `url()`, so only the page's own `fetch`
  could add one. And the page would have to hold the token, with which it
  could open `/ws` and act as the user. Rejected for the derived key.
- **Expiring grants** with a nonce and an expiry, asked for with a method
  and renewed before they end. A leaked URL would end sooner, at the cost
  of renewal and of open pages that fail once their grant ends. Rejected
  as more than this needs (decision 5).
- **A zip as the stored form,** one blob per folder. Simpler references,
  but every entry served means seeking into the zip, two folders never
  share files, and a rewrite of one figure stores the whole folder again.
  Rejected for the manifest.

## Implementation notes

Phase 1 (client protocol 1.104.0, plugin protocol 1.49.0, runtime protocol
1.18.0). Where the code differs from the sections above:

- **An entry's size is `byte_length`,** not `size`, the member the engine's
  reference walk already follows (`collect_artifact_references` counts an
  object with `sha256`, `byte_length`, and `media_type`). So a fork or a
  revert that keeps a manifest keeps its files with no code of its own.
- **`folder` is `{ main, files, bytes }`.** `bytes` is the files' total, so
  a card shows a folder's size without reading its manifest. The
  reference's own `byte_length` and `sha256` are the manifest blob's, which
  the stores check a blob against, and an `Asset`'s `byte_length` is the
  manifest's too.
- **Paths are kept as the runtime sends them,** not NFC-normalized: the
  workspace has no Unicode normalization crate, and the runtime's tar
  stream already gives each name once.
- **A blob is a manifest** when it starts with `{"cookie_folder":` and reads
  as one; `artifact.read` without a `path` looks at the first bytes to tell.
- **Folder assets' files are staged** (`AssetStores::stage_blob`) before the
  write that names them, which checks each is still there. A staged file no
  write named is removed by the store's sweep at the next start.

Phase 2 (client protocol 1.107.0, plugin protocol 1.50.0):

- **The daemon unpacks a folder** from the runtime's tar stream, kept in an
  anonymous temporary file, one file at a time into the store, so one file
  at most is in memory. Links are resolved inside the folder (a link to a
  file is that file again, one to a folder each file under it, a few hops
  at most); links that leave it are left out, as the runtime leaves them
  out of a copy.
- **No label change for models.** Folders reach a model only through tags
  in its own replies and tool results, never as user content, so the
  `[folder …]` label of section 7.4 is not written; `read_artifact` and
  `read_asset` name a folder in their own output.
- **`read_artifact` without `path`** lists a folder's files rather than
  reading its main file, as `read_asset` lists them before it.
- **A `transfer` of a folder artifact** builds its tar stream in a
  temporary file first, so the stream's SHA-256 is known before it is
  sent, as the runtime's commit checks it.

Phase 3 (client protocol 1.108.0):

- **`content.urls` targets are tagged** (`{ type: "artifact", … }` or
  `{ type: "asset", … }`) and return a `path` from the daemon's root, which
  the page puts after its own base, so a page under a sub-path or on a
  static host builds the URL from the address it reached the daemon at.
- **The key is derived from the daemon token** as section 6.1 decides; the
  daemon's listener keeps the token for it beside the page ticket.
- **A zip stores its files** as they are, without deflating them, and
  carries no `Content-Length`. Folders are at most 200 MiB, and most of a
  report's bytes are pictures and video already compressed.
- **The *Files* tab** lists the files, and each opens in a tab of its own
  over HTTP (an HTML or SVG file under the sandbox policy), rather than in
  the panel as a single file of its type.
- **Guests read over their connection** until phase 4: `content.urls` is
  refused to a share guest, whose page falls back to `artifact.read` and
  `asset.read`. A folder's page shows its main file without its other files
  there.
- **Video and audio play from their URL**, and every other kind is read
  over HTTP whole, as it was over the WebSocket, so the card code is the
  same; a read that fails over HTTP marks HTTP failed for the connection.


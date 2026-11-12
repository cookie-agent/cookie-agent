# Share links: public URLs for assets

Status: **implemented** (2026-11-04; client protocol 1.86.0). The user's decisions
are in [section 12](#12-decisions-2026-11-03); how it landed is in
[section 14](#14-implementation-notes-2026-11-04). A mockup of the landing page is
[`share-links/preview.html`](share-links/preview.html). Builds on
[assets](assets.md), [attachments in assistant messages](message-attachments.md),
and the daemon's HTTP listener as [plugin web pages](plugin-web-ui.md) left it.

## 1. Problem

An agent's figure, report page, or file can only be seen by someone who
holds the daemon token. Today, to show a plot to a PI or a colleague, the
user downloads it and sends it some other way. For an HTML page this loses
what makes it useful: the page runs only inside cookie's sandbox.

The user wants to press **Share**, get a URL, and paste it anywhere. Anyone
with the URL can then see the file, without a token, an account, or the
cookie web client.

## 2. Goals and non-goals

**Goals**

- A **share link** is a public, unguessable URL for one asset.
- The link opens a small **landing page**: the title, a preview, and a
  download button ([section 5](#5-serving-a-link)).
- The user makes, lists, and revokes links in the web client and the CLI.
  Links expire after 7 days unless the user picks otherwise.
- A root agent may make a link when the user approves it. Delegated agents
  may not, by default.
- Sharing never exposes the daemon. An optional share-only port serves
  share links and nothing else ([section 6](#6-the-public-address)).
- Shared HTML can never read the daemon's origin: no token, no page
  ticket, no `localStorage`.

**Non-goals (this round)**

- Sharing artifacts directly. An attachment is shared by saving it as an
  asset first, which the card does in one step
  ([section 7.1](#71-the-user)).
- Per-viewer access, passwords, or sign-in for viewers.
- Sharing sessions or transcripts.
- Sites of several files. A shared HTML page that loads `./data.csv` finds
  nothing; it can load absolute URLs, as in the sandbox today.
- The TUI (decided 2026-11-03: web and CLI only).
- Uploading to third-party hosts ([section 13](#13-alternatives-considered)).
- View analytics beyond a count and the last view.

## 3. Share links

A share link is one record in the daemon's share store:

| Field | Meaning |
|---|---|
| `id` | 128 random bits, base64url (22 characters). It is the secret: the URL is the capability. |
| `target` | `{ store, asset }`, as the `asset` display block names an asset. |
| `title` | The asset's title when the link was made, which the user can change. It heads the landing page and names the download. |
| `created` | When the link was made, and by whom: the user, or a session and agent. |
| `expires_at` | When the link ends: 7 days after it is made unless the user picks *1 day*, *30 days*, or *Never*. |
| `views`, `last_viewed` | Landing page views, counted by the daemon. These are approximate: link unfurlers such as Slack's count too. |

**Links are live** (decided 2026-11-03). A link always serves the asset's
current content, as a figure does. Assets have no versions. Rewriting the
asset changes what the link shows, and deleting the asset ends the link.

### 3.1 The URL

```text
<share base>/s/<id>                       the landing page
<share base>/s/<id>/raw/<file name>       the bytes, inline
<share base>/s/<id>/download/<file name>  the bytes, as a download
```

- The **file name** is for the reader and the download. It comes from the
  asset's file name, or from its title slugged with the right extension.
  The daemon ignores it when it looks the link up, so renaming never breaks
  a link.
- The **share base** is chosen as in [section 6.1](#61-the-share-base).
- What the user copies is the landing page's URL.

## 4. Storage and lifecycle

```text
~/.cookie-agent/
  shares.json      every link's fields, rewritten whole and renamed into place
```

- **No bytes are copied.** A link names its asset, and every request reads
  the asset store. The share store holds only records.
- **Writes are atomic** and serialized by one lock, as the asset index is.
  View counts are kept in memory and written at most once a minute and at
  shutdown. A crash loses at most a minute of counts.

| Event | Its links |
|---|---|
| The asset is written again | Serve the new content. |
| The asset is deleted | Removed with it. |
| A session tree is deleted | The links of its session assets are removed with them. |
| A session tree moves to another project | Unchanged: a session asset's store is `{ session: root }`. |
| A project is deleted | Its assets' links are removed. `project.delete` also reports `deleted_shares`. |
| A link expires | It answers as gone at once. It is removed at the next sweep, at startup and hourly. |
| The daemon token is rotated | Unchanged: links do not derive from the token. |

## 5. Serving a link

The daemon serves `GET` and `HEAD` on `/s/{id}`, `/s/{id}/raw/{*name}`, and
`/s/{id}/download/{*name}`, next to `/ws`, `/runtime`, and `/plugins/`.

- **No gate.** The route takes no token, page ticket, or `Origin` check.
  The ID is the only key.
- **One answer for every failure.** An unknown, revoked, or expired link
  answers the same `404` page, "This link isn't available", and so does a
  link whose asset is gone or a link while sharing is off. Nobody can learn
  whether a link ever existed.
- **Headers on every answer:**
  - `Cache-Control: no-store`, so a revoked link is not served from a cache
    in front of the daemon;
  - `Referrer-Policy: no-referrer`;
  - `X-Robots-Tag: noindex, nofollow`;
  - `X-Content-Type-Options: nosniff`.
- **Range requests** work on `raw` and `download`, so video seeks.

### 5.1 The landing page

The daemon renders it as static HTML, with no script, in the Quiet Ink
palette (light or dark by `prefers-color-scheme`) and the bundled Inter.
See the [mockup](share-links/preview.html); its toolbar switches between the
kinds below and the gone page.

- **Header:** the cookie mark and "cookie agent", the title, and a meta
  line: kind, size (and dimensions or duration when known), and file name.
- **Buttons:** *Download* (primary), and *Open full page* for HTML, images,
  video, and PDF, which opens `raw` in a new tab.
- **Preview,** in one card:

  | Kind | Preview |
  |---|---|
  | HTML | A frame of `raw` with `sandbox="allow-scripts allow-popups"`, about 70% of the window tall, with *Reload*. |
  | Image, SVG | The image from `raw`, fitted to the card, on a checkerboard so transparency shows. |
  | Video, audio | The browser's player on `raw`. |
  | PDF | A frame of `raw`, where the browser shows PDFs. |
  | Markdown | Rendered by the daemon into the card, with raw HTML in the source escaped. |
  | Text and code | The first 256 KB in a monospace block, with "Showing the first 256 KB" when cut. |
  | Anything else | A file card: an icon with the extension, the name, the size, "no preview for this type", and *Download*. |

- **Footer:** "Shared from cookie agent", and the expiry ("Link expires in
  6 days · Nov 10, 2026", or nothing for *Never*).
- **Its own policy:** `default-src 'none'; style-src 'unsafe-inline';
  font-src 'self'; img-src 'self'; media-src 'self'; frame-src 'self';
  frame-ancestors 'none'`. It runs no script.
- **Link previews:** `og:title`, `og:type`, and for images `og:image`
  pointing at `raw`, so chat apps show the picture when the link is pasted.
- **Phones:** the buttons go full width under the title, and the frame is
  about 64% of the window tall.

### 5.2 The bytes and their sandbox

`raw` serves the asset with its own type, and `download` adds
`Content-Disposition: attachment` with the file name. `raw` serves text
and code as `text/plain; charset=utf-8`. Any type the landing page cannot
preview is served by `raw` as a download too.

`raw` of HTML or SVG is a page **on the daemon's origin**, unless the
share-only port serves it ([section 6.2](#62-the-share-only-port)), and
*Open full page* opens it at the top level. It must not be able to act as
that origin, so the daemon sends the policy that `viewer.html` already uses
([assets](assets.md) section 8.2), as a header:

```text
Content-Security-Policy: sandbox allow-scripts allow-popups; frame-ancestors 'self'; …open network…
```

- The CSP `sandbox` directive gives the document an **opaque origin** even
  at the top level. The page cannot read `localStorage`, the page ticket
  cookie, or the token, and it cannot call the daemon as the daemon's
  page. Its requests come from origin `null`, and `SameSite=Lax` cookies
  are not sent with them.
- The rest is the viewer's policy: scripts, `eval`, and the network are
  open, so a Plotly page from a CDN works as it does in a card, with the
  limits in [attachments](message-attachments.md) section 7.
- SVG gets `sandbox` without `allow-scripts`.

## 6. The public address

### 6.1 The share base

Every link's URL is built from the first of these that is set:

1. **`[share] url`**, such as `https://share.example.com`: the public
   address of the share-only port;
2. **`[server] page_url`**;
3. the address the client reached the daemon at, which the client adds
   before the path the daemon returns.

**Warnings.** When the base is plain `http`, loopback, a private or CGNAT
(Tailscale) address, or a `.ts.net` name behind `tailscale serve`, the
share dialog still shows the link but warns: "This link works only where
cookie's page can be reached," with a link to the setup docs. The daemon
cannot tell for certain whether an address is public, so this is a hint
and not a block.

### 6.2 The share-only port

**`[share] listen`** (optional, decided 2026-11-03), such as
`"127.0.0.1:7420"`, makes the daemon open a second listener that serves
`/s/…` and the landing page's font, and nothing else. Every other path
answers `404`, `/ws`, `/runtime`, `/plugins/`, and the web page included.

A Cloudflare tunnel or Tailscale Funnel then points at that port, and the
internet reaches share links but never the control plane, not even its
connect screen:

```yaml
# cloudflared config.yml
ingress:
  - hostname: share.example.com
    service: http://127.0.0.1:7420
  - service: http_status:404
```

```sh
tailscale funnel --bg 7420      # the share port only; the page stays on `serve`
```

With `[share] url = "https://share.example.com"`, links point there. The
main listener serves `/s/` as well, so a link also opens at the private
page's address.

The share port is plain HTTP by default. A non-loopback address takes the
same `tls` settings as `[server]`.

## 7. Making and revoking links

### 7.1 The user

**Web client.**

- **A Sharing button.** Every asset card, attachment card, Assets tab row,
  and the side panel gets a share button. It opens the share dialog:
  - the URL with *Copy*;
  - the expiry: *1 day*, *7 days* (the default), *30 days*, or *Never*,
    which can be changed later;
  - the warning from [section 6.1](#61-the-share-base) when it applies;
  - *Revoke*.

  Sharing an asset that already has a link shows that link rather than
  making a second one. *New link* makes another, for example to revoke one
  recipient's copy without breaking the others.
- **An attachment card's share button** first saves the attachment as a
  session asset (*Save as asset*, [attachments](message-attachments.md)
  section 8), and the dialog says so, naming the new handle. After that,
  the card is shared through its asset.
- **A card with a live link** shows a small link badge. Clicking it opens
  the dialog.
- **A Sharing view** in the sidebar, next to Assets, lists every link:
  title, asset (a link to it in the Assets tab), created by, expiry, views,
  and last view. Each row has *Copy*, *Change expiry*, and *Revoke*.
  *Revoke all* empties the list, and a filter hides expired links.
- The user's own actions check no permission rule.

**CLI.**

```text
cookie share list [--json]
cookie share create <asset handle or UUID> [--session <id> | --project <id>] [--expires 1d|7d|30d|never]
cookie share revoke <id> | --all
```

`create` prints the URL. Without `--session` or `--project`, it looks for
the handle in the shared store, then in every project's store, and stops
with a list of matches when there are several.

### 7.2 An agent

The tool `share_link` joins the deferred `assets` group:

```text
share_link { asset: "asset_3f9a1c2e", expires_in?: "1d" | "7d" | "30d" | "never" }
→ https://share.example.com/s/q3Vt0x9LmA2bCdEfGh1iJk (expires 2026-11-10)
```

- The asset is a handle or UUID, as for the other asset tools.
  `expires_in` defaults to `7d`.
- It reuses an existing link to the same asset with the same expiry, so an
  agent calling it twice does not make two links.
- **The permission action `share`**, decided 2026-11-03: the default is
  **ask** for root agents and **deny** for delegated agents at any depth.
  A link publishes content, so `allow` is never a default. The approval
  card shows the asset's title, type, size, and expiry.
- An ACP agent gets the tool over the bridge, under the same rule.
- Agents cannot list or revoke links. Those stay with the user.

## 8. Protocol and compatibility

**Client protocol, minor:**

- `Share { id, url?, path, target, title, media_type, byte_length,
  created, created_by, expires_at?, views, last_viewed? }`, where `url` is
  absent when the client must build it ([section 6.1](#61-the-share-base));
- `shares.list { target? }`;
- `share.create { target, expires_at?, reuse: bool }`;
- `share.update { id, title?, expires_at? }`;
- `share.revoke { id }` and `shares.revoke_all`;
- the notification `shares.changed`, coalesced like `assets.changed`;
- `ServerHello` gains `share_url?`, the resolved share base, so clients
  can warn before they make a link;
- `project.delete` also returns `deleted_shares`.

**Configuration:** `[share] enabled` (when it is off, every share path
answers 404 and creation is refused), `url`, and `listen`, all read by the
daemon. Add the `share` permission action and its defaults.

**Plugin and runtime protocols:** no change.

**History:** none. The tool's result is one line of text, and links live
outside sessions.

## 9. Security notes

- **A link is a bearer secret.** Anyone who has it can open it until it is
  revoked or expires. Chat apps fetch links to preview them. The docs say
  this plainly.
- **Live links can change after sharing.** If an agent rewrites a shared
  asset, the new content is public at once. The asset's card carries the
  link badge so the user can see that it is shared, and the 7-day default
  limits how long this lasts.
- **The daemon's origin stays clean** through the sandbox policy (section
  5.2) and a landing page that runs no script, even without the share-only
  port.
- **The control plane stays private** when only the share-only port is
  exposed (section 6.2).
- **IDs come from the OS's random source.** With 128 bits, guessing an ID
  is not a practical attack, so the route needs no rate limit.

## 10. Tests

Expiry values in tests are explicit, never the default.

- Routing: a known link serves its landing page, `raw`, and `download`.
  Unknown, revoked, expired, and asset-gone links, and links while sharing
  is off, all give byte-equal 404s on all three paths. The share port
  answers 404 for `/`, `/ws`, `/runtime`, and `/plugins/x/`.
- Headers: `raw` of HTML carries `sandbox` without `allow-same-origin`,
  and `raw` of SVG carries it without `allow-scripts`. The landing page's
  policy has no `script-src`. Every answer carries `no-store` and
  `no-referrer`. `download` sets `attachment`, and range requests return
  206.
- Landing page: each kind's preview (snapshot tests of the rendered HTML),
  with Markdown's raw `<script>` escaped and text cut at its limit.
- Lifecycle: rewriting an asset changes `raw`. Deleting an asset, a
  session tree, or a project removes its links. An expired link is swept.
- Tool: the `share` rule decides (ask, deny, and allow, each set
  explicitly). A second call reuses the link. A subagent under the
  default rules is refused.
- Browser suite: a shared HTML page opened from *Open full page* cannot
  read `localStorage` or `document.cookie` of the daemon's origin.

## 11. Phases

1. **Daemon:** the share store, the routes, the landing page, the share
   port, config, protocol methods, lifecycle hooks, and the tests above.
2. **Web client:** the share button and dialog, the attachment card's
   save-then-share, badges, and the Sharing view.
3. **Agent and CLI:** `share_link`, the `share` action, and `cookie share`.
4. **Docs:** a Sharing page under Sessions, and Remote access gets "Public
   share links" with the cloudflared and Funnel recipes. Check Funnel's
   syntax against Tailscale's docs then.

## 12. Decisions (2026-11-03)

The user decided:

1. **Assets only,** with a sharing button in the web client. Cookie
   artifacts (the session's blob store) are not shared. An attachment is
   shared by saving it as an asset, in one step from its card.
2. **Links show the current content** of the asset.
3. **`share_link`** exists: **ask** by default for non-delegated agents,
   **deny** for delegated ones.
4. **Links expire after 7 days** by default.
5. **An optional share-only port** (`[share] listen`).
6. **Web and CLI only:** nothing in the TUI.
7. **A small landing page** with a preview and a download button. The draft
   mockup is [`share-links/preview.html`](share-links/preview.html).

## 13. Alternatives considered

- **Uploading to an external host** (S3/R2, a gist, Cloudflare Pages).
  Links would survive the daemon being down. But it needs credentials, it
  cannot serve live assets, and revoking depends on the host. It could come
  later as a plugin "share provider" behind the same dialog.
- **The file itself at the link, without a landing page.** Images would
  embed elsewhere directly. The user chose the landing page; `raw` still
  gives the bare file, and `og:image` lets chat apps preview images.
- **Signed URLs (an HMAC of the target and expiry) with no store.** These
  need no state. But a single link cannot be revoked without rotating the
  key, which kills every link, and there is nowhere to list them.
- **A share-only host name matched on the main listener.** This needs no
  second port, but it trusts the `Host` header, and a request that reaches
  the main listener with another `Host` gets the control plane. A separate
  port does not depend on headers.

## 14. Implementation notes (2026-11-04)

Implemented in client protocol 1.86.0. Where the code settled what the
draft left open, or differs:

- **The share-only port is plain HTTP.** `cookie daemon`'s TLS identity
  comes from its command line, not `[server]`, so the second listener takes
  none; it is meant for a tunnel on loopback, which brings its own TLS.
  Changing `[share] listen` takes a daemon restart.
- **Making a link is explicit.** The dialog of an asset with no link shows
  its expiry and *Create link* rather than making one on opening, since a
  link publishes the asset. An asset with a link shows it with *Copy*,
  *Change expiry…* (counted from now), *New link*, and *Revoke*.
- **The Sharing view** lists only live links: `shares.list` never returns
  expired ones, so it has no filter for them. *Revoke all* asks first.
- **`share_link` reuses** the asset's newest live link whatever its expiry,
  and says so with that link's expiry, rather than matching the expiry it
  was asked for.
- **Links go with their assets** at `asset.delete`, `delete_asset`, a tree's
  deletion, and a project's (`deleted_shares`); `shares.list` also drops a
  link whose asset is gone however it went. Expired links are dropped by
  the engine's janitor, which writes counted views every minute, and at
  shutdown.
- **Views** count the landing page's `GET`s only, not `raw`, `download`, or
  `HEAD`.
- **The landing page's font** is the web bundle's Inter at
  `/s/_font/inter.woff2` (no ID is that short), with the system font when
  the bundle is not built. Markdown and text are rendered into the page;
  `raw` serves them as `text/plain`. PDF is served without a sandbox, which
  the browser's viewer refuses, but with `frame-ancestors 'self'`.
- **The private-address warning** treats a `.ts.net` name as private only
  when it is the page's own address, not a configured `[share] url`, since
  a Funnel address is public.
- **Errors:** `share_not_found`, `shares_disabled`, and `-32602` for an
  expiry already past.
- **Not yet:** the per-link title in `share.update` has no control in the
  web client, only in the protocol.
- **Renamed (2026-11-12):** `[share] url` and `[share] listen` are now
  `[public] url` and `[public] listen`, which webhooks share
  ([webhooks](webhooks.md) decision 5); `[share]` keeps `enabled`.

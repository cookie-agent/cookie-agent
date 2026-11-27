# Forwards: a machine's web app at the daemon's address, and ports between machines

Status: **approved** (2026-11-26), being implemented. The
user's decisions are in
[section 12](#12-decisions-2026-11-26).

## 1. Problem

An agent often starts something that listens on a port of the machine it
works on: a Vite or Jupyter server, a TensorBoard, a docs preview, a
database. The user may be on a phone, or on another computer, and can reach
the daemon but not that machine. Today they need an SSH tunnel or a VPN
set up by hand, and the agent cannot do it for them.

The same holds between machines. A job on labpc needs the Postgres on
yunhao-labserver, or a training run on Unicorn wants to stream to a viewer
on labpc. The machines may not reach each other, but each reaches the
daemon, and the daemon reaches each of them.

## 2. Goals and non-goals

Goals:

- **A web app of any machine at the daemon's address.** An HTTP forward
  serves a machine's `host:port` under `/p/<handle>/` on the daemon's own
  listener, WebSockets included, so the user opens it where they open
  cookie.
- **A port of one machine on another.** A TCP or UDP forward listens on
  one machine and connects each connection (or each UDP peer) to a
  `host:port` reached from another machine. Either end may be any machine
  of the session, the daemon's own included.
- **Agents make them,** with tools in a deferred `forwards` group, under a
  `forward` permission action that asks the user by default.
- **A page in a reply.** An `<attach/>` tag shows a forward that exists as
  a live page in the conversation, as an HTML attachment shows (section
  7.1).
- **Access as for assets** (decision 2): the user; a guest of a
  conversation share, for the forwards of the shared session; and anyone
  with a share link to the forward, which is made and revoked as an
  asset's is.
- **A Forwards view in every client** (decision 4): the web client's view
  beside Machines, Schedules, and Webhooks and a tab in the session panel,
  the TUI's `/forwards`, and `cookie forward`.
- **Forwards end on their own**: after 24 hours by default (decision 3),
  and with the session tree that made them.

Non-goals:

- **Isolating a forwarded app from the web client** (decision 1). It runs
  on the daemon's origin, unsandboxed, as plugin pages do
  ([plugin web pages](plugin-web-ui.md)); it can read the page's storage,
  the daemon token included. Forward only what you would run in that tab.
- **Rewriting pages.** Only headers are rewritten (section 6.2). An app
  that loads `/static/app.js` from the root misses it under `/p/<handle>/`
  unless it is told its base path; section 6.3 says how agents handle that.
- **Throughput.** Bytes travel over each runtime's existing connection as
  base64 in JSON (section 4.3). This is fine for web apps, terminals, and
  databases used by hand, and slow for bulk copies, which `transfer` does
  better.
- **Forwards between runtimes without the daemon.** Every byte goes
  through the daemon; runtimes never connect to each other.
- **Listening on the daemon's own listener for TCP or UDP.** A TCP or UDP
  forward listens on a machine through its runtime. The daemon's host is
  a machine when a runtime runs there, as the coding runtime usually does.

## 3. Forwards

A forward is one record in the daemon's forward store:

| Field | Meaning |
|---|---|
| `id` | A UUIDv7 `ForwardId`. |
| `handle` | `fw_<8 hex>`, random, what the model and the URL use, unique among live forwards. |
| `kind` | `http`, `tcp`, or `udp`. |
| `to` | The target: `machine` and `host:port`, reached from that machine. `host` defaults to `127.0.0.1`. |
| `from` | `tcp` and `udp` only: the `machine` that listens, its `bind` address (default `127.0.0.1`), and `port` (`0` picks a free one). |
| `title` | Optional, at most 200 characters; shown in the clients. |
| `owner` | The root session whose tree made it, or none for one the user made in a client (decision 5). |
| `created` | When, and by which session and agent, or by the user. |
| `expires_at` | 24 hours after it is made by default; `null` for never. |

And its live state, reported to clients but not stored:

| Field | Meaning |
|---|---|
| `status` | `active`; `waiting` (a machine it needs is not connected); `failed` with a `reason` (the listen failed, such as a port in use); `expired`. |
| `address` | Where it is reached: the URL of an `http` forward, or the `bind:port` it listens on. |
| `connections` | Open now, and in all. |
| `bytes_in`, `bytes_out` | Since it started. |
| `recent` | The last 50 connections: when, the peer, how long, bytes, and how it ended. |

### 3.1 Storage and lifecycle

```text
~/.cookie-agent/
  forwards.json      every forward's fields, rewritten whole and renamed into place
```

- **Writes are atomic** and serialized by one lock, as the share store is.
  Counts and recent connections are kept in memory only.
- **A daemon start** restores every forward that has not expired: each
  waits for its machines and starts when they connect.

| Event | Its forwards |
|---|---|
| It expires | It stops at once: listeners close and open connections end. It is removed at the next sweep, at startup and hourly. |
| A machine it needs disconnects | `waiting`; open connections end. It starts again when the machine reconnects. |
| The owning session tree is deleted | Deleted with it. |
| The user or an agent deletes it | It stops, and its share links are revoked. |
| The daemon token is rotated | Unchanged. A forward's web address checks the page ticket, which changes with the token. |

## 4. The runtime side

### 4.1 Capability

`runtime/initialize`'s result gains `net: true` (runtime protocol 1.19.0)
for a runtime that answers the `net/*` requests. The coding runtime does.
A machine whose runtime does not cannot be either end of a forward, and the
tool says so ("labpc's runtime is too old to forward ports").

### 4.2 Requests

Every connection and listener is a number the runtime chooses. Requests go
daemon → runtime; notifications go both ways.

| Message | Direction | Params | Result |
|---|---|---|---|
| `net/listen` | request | `protocol` (`tcp`/`udp`), `bind`, `port` | `listener`, the `port` it got |
| `net/unlisten` | request | `listener` | `{}`; its connections stay until closed |
| `net/accepted` | runtime → daemon | `listener`, `connection`, `peer` | |
| `net/connect` | request | `protocol`, `host`, `port` | `connection`, or `net_failed` with the reason (refused, unreachable, timed out after 10 s) |
| `net/data` | both | `connection`, `data` (base64, at most 64 KiB) | |
| `net/ack` | both | `connection`, `bytes` | |
| `net/close` | both | `connection`, `reason?` (`eof`, `reset`, or text) | |
| `net/datagram` | both | `listener` or `connection`, `peer?`, `data` | |

- **Flow control.** Each side may have at most 1 MiB sent on a connection
  and not yet acknowledged with `net/ack`; it stops reading its socket
  until acks arrive. So a slow end slows the other, and the daemon holds
  at most a window per connection.
- **Half-close.** `net/close` with `eof` shuts the writing side only; the
  connection ends when both sides have closed.
- **UDP.** A `udp` listener reports each datagram with its `peer`. The
  daemon keeps one `net/connect` of `udp` per peer on the target machine,
  and relays replies back to that peer through the listener. A peer is
  forgotten after 60 seconds without datagrams.
- **Bind addresses.** The runtime refuses a `bind` that is not one of its
  machine's addresses. A `bind` other than a loopback address makes the
  forward reachable by anyone who can reach that machine, so the tool asks
  for it separately (section 7).
- **A dropped runtime connection** closes every listener and connection it
  had, on both sides.

### 4.3 Throughput

The runtime connection is newline-delimited JSON (`peer::LineTransport`)
or WebSocket text frames, so `net/data` is base64, a third larger, plus a
JSON parse per 64 KiB. A WebSocket runtime connection could carry binary
frames later. That would be a runtime-protocol feature of its own, and
nothing here depends on it.

## 5. The daemon side

- **The relay.** For a TCP forward, each `net/accepted` on the listening
  machine makes a `net/connect` on the target machine. The daemon pairs
  the two connections and moves `net/data`, `net/ack`, and `net/close`
  between them. A connect that fails closes the accepted connection with
  its reason, and the reason shows in `recent`.
- **HTTP.** An `http` forward has no listener on a machine. The daemon
  serves it (section 6) and opens a `net/connect` to the target for each
  connection a browser makes. Keep-alive connections are reused while the
  browser keeps its own.
- **Limits.** At most 64 forwards and 256 open connections per forward; a
  connection past that is closed at once and counted.
- **`[forwards]`** in `config.toml`: `enabled` (default `true`) and
  `max_forwards` (default `64`; `0` or less is no limit).

## 6. Web addresses

### 6.1 The routes

```text
<daemon>/p/<handle>/<path>                 the user's
<share base>/s/<share id>/p/<handle>/<path>  a conversation share's guest
<share base>/s/<share id>/<path>             a share link to the forward
```

- **The user's** route is gated by the page ticket cookie, as `/plugins/`
  is ([plugin web pages](plugin-web-ui.md) section 4): the web client sets
  it after each hello. A request without it answers a page that links to
  the web client, which opens the forward again once it has connected.
- **A guest's** route checks that the share is live, lets the guest see
  the session, and that the forward belongs to that session's tree. It
  ends when the share does.
- **A share link to the forward** serves the app at the link's root. It is
  a share record whose `target` is `{ forward }`, made, listed, changed,
  and revoked as an asset's link is ([share links](share-links.md)), with
  the same 7-day default expiry and the public port. It ends early if the
  forward does.
- **Every failure** on these routes answers one `404` page, "This forward
  isn't available", as share links do.

### 6.2 What the proxy changes

The proxy is the one plugin pages use (`plugin_pages.rs`), with a tunnel in
place of a loopback socket:

- **The path** loses its prefix; the query is kept.
- **Headers:** hop-by-hop headers dropped, `Host` set to the target's
  `host:port`, and `X-Forwarded-For`, `-Host`, `-Proto`, and `-Prefix` set
  (the prefix is the forward's root, such as `/p/fw_3f9a1c2e`). The
  `cookie_page_ticket` cookie is removed from what the app sees.
- **Responses:** a `Location` that starts with `/` gets the prefix; a
  `Set-Cookie` without a `Path`, or with `Path=/`, gets `Path=<prefix>/`.
  Bodies are passed as they are.
- **WebSockets** are upgraded on both sides, then bytes are relayed.
- **Request bodies** stream, with no size limit beyond the target's.

### 6.3 Apps that load from the root

Many dev servers take a base path: Vite's `--base`, Jupyter's
`--ServerApp.base_url`, TensorBoard's `--path_prefix`, and `mkdocs serve`
behind `site_url`. `forward_create` returns the forward's prefix before the
app needs it, and its description tells the agent to start the server with
that base path, or to restart it with one if the page loads without its
assets.

## 7. Tools

The deferred `forwards` group on the `global` runtime:

```text
forward_create(
  kind: "http" | "tcp" | "udp",
  to: { machine?: string, host?: string, port: integer },
  from?: { machine?: string, bind?: string, port?: integer },   tcp and udp only
  title?: string,
  expires_in_hours?: integer,     default 24; 0 or less: never
)
forward_list()
forward_delete(forward: string)   a handle
```

- **`machine`** names a machine as the machines block does; omitted, it is
  the session's workspace machine.
- **The model reads** one line:
  - `Forwarded fw_3f9a1c2e "Vite" (http → labpc 127.0.0.1:5173) at http://127.0.0.1:7419/p/fw_3f9a1c2e/, until Nov 27 14:02. Start the server with base path /p/fw_3f9a1c2e/ if it loads from the root.`
  - `Forwarded fw_7c1d02aa (tcp yunhao-labserver 127.0.0.1:15432 → labpc 127.0.0.1:5432), until …`
  - `forward_list` lists the forwards the session can use, one a line, with status and counts.
- **`<attach/>` tags** show forwards in a reply (section 7.1).
- **`share_link`** takes a forward's handle as it takes an asset's.
- **Permissions.** The `forward` action, with resources `create`,
  `delete`, and `read`, and for `create` the label
  `<kind> <from> -> <to>`, such as `tcp labpc:0.0.0.0:8080 ->
  labserver:127.0.0.1:80`. Defaults: `create` and `delete` ask for root
  agents, `read` is allowed, and all are denied for delegated runs. A
  `bind` that is not loopback always asks, even where a rule allows
  `create`.

### 7.1 Pages in a reply

An agent shows a forward it made with `forward_create`, or any forward the
session can use, in its reply with the attachment tag
([attachments](message-attachments.md)):

```markdown
The notebook is up: <attach forward="fw_3f9a1c2e" path="/lab" title="Jupyter"/>
```

- **`forward="fw_…"`** names a forward that exists, by its handle, checked
  as `forward` `read`: allowed, it is shown; denied, the tag fails; asked,
  the card asks the user with *Allow* and *Deny* (attachments section 4).
  A tag never makes a forward (decision 8).
- **`path`** (optional, starting with `/`, with a query if wanted) is the
  page of it to show; omitted, its root. `title` names the card; omitted,
  the forward's title, else its route.
- **What is recorded:** the `assistant_attachment` outcome gains
  `forwarded { forward: ForwardBlock, path }`, the forward as it was when
  the tag resolved. A card shows the forward as it is now, read with
  `forwards.list`, as an asset's card shows the asset as it is now.
- **Failures** are told to the model as other tags' are: no forward has
  that handle, it has expired, or the rule denied it. A forward waiting for
  its machine, or whose target nothing listens on, is not a failure: the
  card says so, and shows the page once it answers.
- **A TCP or UDP forward** in a `forward` tag shows as a status card (route,
  status, address, *Copy*), not a page.

**The card.** An `http` forward's card is a live page, as an HTML
attachment's is: in the gallery a live thumbnail, and opened in the side
panel with *Reload*, *Full screen*, and *Open in new tab*, which HTML
attachments lack and forwards have because the app is a real address. The
frame loads `/p/<handle>/<path>` directly, on the daemon's origin, without
`sandbox` (decision 1); a guest's frame loads its share's route. When the
forward has expired, been deleted, or waits for its machine, the card says
so in place of the page, with *Extend* (24 hours more) for the user when it
expired. Its meta line names the route: `Jupyter · labpc:8888 · expires in
23 h`.

**The TUI** shows the tag as `🌐 Jupyter` with a line holding the web link
(`#session=<id>&forward=fw_…`), which opens the forward once the web client
connects, and *Allow* and *Deny* while it waits for the user.

## 8. The clients

### 8.1 Web

- **The Forwards view** in the sidebar, beside Machines, Schedules, and
  Webhooks, with the same rows and pages as the Webhooks view
  ([webhooks view](webhooks-view.md)):
  - **Rows:** title (or `from → to`), kind, status dot, open connections,
    bytes, expiry, and *Open* for `http`.
  - **A forward's page:** an *Overview* tab (route as `from → to`, the
    address with *Copy* and *Open*, expiry with *Change…*, *Share…*,
    *Delete*), and a *Connections* tab (the recent connections).
  - **New forward:** a form with the kind, the target's machine, host, and
    port, for TCP and UDP the listening machine, bind, and port, the title,
    and the expiry (*1 hour*, *24 hours*, *7 days*, *Never*).
- **The session panel** gains a *Forwards* tab listing the session tree's
  forwards, as it lists schedules and webhooks
  ([session schedules](session-schedules.md)), and the title bar a badge
  while one is active.
- **Cards** for forwards in replies (section 7.1), and a share dialog like
  an asset's.
- **A guest** sees the shared session's forwards in its panel and opens
  them through its share; it cannot create, change, or delete them.

### 8.2 TUI

`/forwards` lists them as `/webhooks` does, with *Open* (the web link, as
assets have), *Delete*, *Change expiry*, *Share*, and *New* (the form in
the editor). The bottom bar shows a forward count while any is active.

### 8.3 CLI

```text
cookie forward list
cookie forward http labpc:5173 [--title …] [--expires 24h|never]
cookie forward tcp labserver:127.0.0.1:15432 labpc:5432
cookie forward delete fw_3f9a1c2e
cookie forward share fw_3f9a1c2e [--expires 7d]
```

## 9. Protocol and compatibility

Client protocol, one minor: `ForwardInfo`; `forwards.list`,
`forward.create`, `forward.update` (`title`, `expires_at`),
`forward.delete`; `forwards.changed` (empty, coalesced, as
`assets.changed` is); `ShareTarget` gains `forward`; the attachment tag's
`forward` source with `path`, the `forwarded` outcome, and `ForwardBlock`. Guests get `forwards.list` for their session. Runtime
protocol 1.19.0: `net` and the `net/*` messages. Plugin protocol: the new
display block, if it reaches plugins.

## 10. Security notes

- **A forwarded app is trusted like a plugin page** (decision 1): it shares
  the web client's origin and can read the token. The tool's description,
  the docs, and the permission prompt say so.
- **A share link to a forward is public.** Anyone with it reaches the
  app, which may have no login of its own. The share dialog says so, and
  the default expiry is 7 days, as for assets.
- **Non-loopback binds** expose a port to a machine's network. They always
  ask (section 7).
- **Targets** are reached from the target machine, so `host` may be any
  address that machine reaches, such as another host on its LAN. The
  permission label names it.

## 11. Phases

1. **Runtime:** `net` and `net/*` in the protocol and the coding runtime,
   TCP and UDP, flow control (runtime protocol 1.19.0).
2. **Engine:** the store, the relay, restore and expiry, the `forward`
   action, the tools, and the client methods.
3. **Server:** `/p/` and the guest and share routes; share links to
   forwards; `<attach forward/>`.
4. **Web:** the Forwards view, the session panel tab, cards, the share
   dialog.
5. **TUI and CLI:** `/forwards` and `cookie forward`; docs.

The estimate is 9 to 11 thousand lines with tests and docs, about 6 to 7
thousand of them code.

## 12. Decisions (2026-11-26)

1. **The daemon's own origin.** A forwarded web app is served under
   `/p/<handle>/` on the daemon's listener, unsandboxed; it may read the
   web client's storage and the daemon token.
2. **Access as for assets and attached media:** the user, a conversation
   share's guests for that session's forwards, and share links to a
   forward, made like an asset's.
3. **24 hours by default.** `expires_in_hours`, where `0` or less is never.
4. **Every client:** web, TUI, and CLI.
5. **A forward the user makes** in a client belongs to no session: it lives
   until it expires or is deleted.
6. **One target per HTTP forward.** A frontend and its API on two ports are
   two forwards.
7. **No auto-forwarding** of ports a command starts listening on; the
   agent asks for a forward.
8. **`<attach forward/>` shows a forward that exists** as a live page
   (section 7.1), by its handle and an optional `path`. A tag never makes
   one: the agent makes it with `forward_create` first.
9. **The attachment enums open, in a minor** (2026-11-27). The tag needs a
   new `AttachSource` and `AttachmentOutcome` variant, and both were
   closed, which the compatibility rules count as breaking. The user chose
   to make both open in the same minor (1.110.0), each with an unknown
   fallback, over a major bump or a card in `forward_create`'s result:
   only clients older than the release, which ship with the daemon,
   cannot read a forward tag.

## Implementation notes

Phase 1 (runtime protocol 1.19.0):

- **`net/unlisten` is a request** answered `{}`, like the other requests
  the daemon makes; the listener's connections stay until closed.
- **A TCP connection's end** is a `net/close` with reason `eof` from the
  side whose socket reached its end, which the daemon passes on so the
  other side shuts its writing half; any other reason is a reset. The
  daemon ends a connection both of whose directions ended with a
  `net/close` of its own.

Phase 2 (client protocol 1.109.0):

- **No `[forwards]` table yet.** The limits are fixed: 64 forwards and 256
  open connections a forward. A table comes when a limit needs changing.
- **An expired forward is removed** at the supervisor's next look, within
  15 seconds, rather than shown as `expired` until an hourly sweep.
- **The supervisor** looks again whenever a runtime connects or
  disconnects, whenever the forwards change, and every 15 seconds; a
  listener that failed is tried again after 30 seconds. A forward listens
  only while both its machines are connected.
- **A machine's runtime** is any of its connected runtimes that reports
  `net`; the daemon's own machine has the local coding runtime.
- **Each runtime connection has one outbox**, a queue the relay sends its
  `net/*` notifications through in order, so a connection's bytes never
  overtake each other. The daemon holds a connection's bytes only while
  the target is being connected, at most the sender's 1 MiB window.
- **A listener others reach** is checked as the resource
  `public <machine>:<bind>` beside `create`. No default rule allows it,
  so the user is asked unless a rule of theirs allows every `forward`
  resource.
- **`forwards.changed`** is sent at most once a second, since counts move
  with every connection.
- **An `http` forward** is active with its address once its machine is
  connected; the daemon serves it from phase 3.

Phase 3 (client protocol 1.110.0, plugin protocol 1.51.0):

- **The daemon's end of a web forward** is a pipe the proxy speaks HTTP
  through: the relay's link to the target's connection has a local end
  instead of a runtime's, under the same 1 MiB window, and each browser
  connection is one connection to the target.
- **Share links to a forward** are made with `forward_share.create`, not
  `share.create`, whose params name an asset; `shares.list` lists them in
  `forwards`, and `share.revoke` revokes one. `share.update` keeps to asset
  links: a forward's link is changed by making a new one. `share_link`
  takes a forward's handle.
- **A WebSocket upgrade** carries `Connection: upgrade` to the app again,
  which dropping the hop-by-hop headers takes out.
- **`ForwardBlock`** holds the handle, kind, title, and route; a card reads
  the forward's address from where the page reaches the daemon, its own
  for the user and its share for a guest.
- **The opened enums** take an `Unknown` fallback (`#[serde(other)]`, left
  out of the schema, which says `x-open`) rather than an `Open<T>` field,
  so their many matches stay as they are; nothing writes `Unknown`.


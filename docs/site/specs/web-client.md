# Web Client and Local Daemon Use

Status: **implemented** (2026-10-20), in phases L and W0 to W5; what landed
and where it differs from the plan is in
[section 17](#17-implementation-notes), with a summary of the deviations at
its end. The decisions in [section 1](#1-goal-and-scope) come from the
planning discussion of 2026-10-19, and no questions are open
([section 15](#15-open-questions)). The spec builds on the
[compatibility spec](compatibility-v1.md), which froze the client protocol and
added browser authentication, and on the
[runtime capabilities spec](runtime-capabilities.md), which added the
listening daemon. The addendum of 2026-10-21
([section 16](#16-addendum-2026-10-21-web-layout-v2-and-session-deletion))
adds web layout v2 and session deletion, implemented on 2026-10-21, and
its [section 16.4](#164-the-agents-machines-and-memory-views) the Agents
sidebar, the machines' drag and drop and Add a runtime, and the Memory
view's project folders, also implemented on 2026-10-21. On
2026-10-21 the user also reversed the "no service worker" decision of
[section 9](#9-phones): the page installs as an app, with a network-only
service worker. On 2026-10-21 the user also reversed the origin allowlist
(sections [1](#1-goal-and-scope) and [7.1](#71-embedded-page)):
`[server] allowed_origins` is gone, and `/ws` takes a connection from any
origin, with the token as the guard.

## 1. Goal and scope

Before this spec the TUI was the only interactive client. This spec adds a browser client
that does everything the TUI does that a browser can do. It works on a phone
and on a desktop, against a daemon on the same machine or on another one.

It also makes plain `cookie` use a daemon already running on the machine
instead of starting a private one beside it (section 13). Once a persistent
daemon holds the machine's sessions, every client should see the same ones.

**Decided 2026-10-19:**

- **Use.** One client serves the desktop browser on the daemon's machine and
  remote access from a laptop or phone. The layout is responsive, and every
  action works by touch.
- **Stack.** The client is Rust compiled to WebAssembly with Leptos, using
  client-side rendering. It reuses the Rust protocol client and a reducer
  shared with the TUI. No TypeScript client and no generated TypeScript
  types are written.
- **Hosting: both.**
  - The daemon serves the built page.
  - The same bundle also works from any static host, connecting to a daemon
    whose `allowed_origins` lists that host. (Since 2026-10-21, any daemon:
    the allowlist is removed, see the next decision but four.)
- **Who serves the page.** `cookie daemon` always serves it. Plain `cookie`
  never does: it either connects to a running daemon (section 13) or runs
  one in-process with no port. There is no
  configuration key or flag for this.
- **Highlighting.** Code is highlighted by highlight.js shipped inside the
  bundle, not by syntect compiled to WebAssembly and not by Shiki (several
  times larger). The web and TUI colours may therefore differ slightly.
- **Editors.** `/config` and `/memory` edit in a plain text area in
  version 1. A code editor can come later.
- **No project-site hosting.** The bundle is not published on
  `cookie-agent.github.io`. Releases attach it as an archive.
- **MSRV.** If the chosen Leptos release needs a Rust newer than 1.88, the
  workspace MSRV rises to match.
- **Origins: no special case.** Every browser connection, the embedded
  page's included, must come from an origin `[server] allowed_origins`
  lists. Whoever sets up the daemon decides which addresses reach it.
  **The user reversed this on 2026-10-21:** `[server] allowed_origins` is
  removed. `/ws` accepts an upgrade from any `Origin`, or none, because a
  reverse proxy makes browser-origin gating pointless and the token is the
  real guard; `/runtime` still refuses any request with an `Origin`. A
  config that still has the key fails as an unknown key.
- **One listener.** The page and `/ws` share the daemon's one TCP listener.
  Forwarding that one port (with cloudflared, Tailscale, an SSH tunnel)
  forwards both, and the connect screen fills in the daemon URL from the
  page's own address.
- **Styling.** The page uses standard tooling: Tailwind CSS through Trunk's
  built-in Tailwind support, as the Leptos documentation describes. No
  hand-written stylesheet framework.
- **No conformance client.** No Python or Node smoke client is added. The
  compatibility checker and the frozen corpora stay the guard for
  third-party clients.
- **Milestone.** The milestone is full TUI parity, phases W0 to W5. Two
  exclusions stay with the TUI and CLI:
  - MCP OAuth, whose redirect must reach a loopback listener (since
    closed: [MCP OAuth from the web client](web-mcp-oauth.md), 2026-10-21);
  - offering this machine as a runtime, as `cookie attach` does when the
    daemon is remote.
- **Plain `cookie` and a running daemon** (section 13):
  1. When a daemon is running on this machine, plain `cookie` connects to
     it.
  2. It then checks whether the daemon has a coding runtime on this machine.
     If so, sessions use the daemon's runtime. If not, plain `cookie`
     provides its own local coding runtime.
  3. When no daemon is running on this machine, plain `cookie` behaves as
     today: it runs the daemon in-process.
  4. `cookie attach` and `cookie --standalone` override the automatic
     choice.
  5. There is no discovery file. Plain `cookie` finds the daemon at the
     address `[server]` configures (section 13.1).
  6. `cookie run` stays in-process.
  7. A daemon running as another OS user on the same machine is not
     detected. It reports the same machine ID, so its `local` runtime is
     used, with that user's file permissions. Users who run the daemon
     under a service account use `--standalone` or a dial-in runtime.

**Out of scope:**
- protocol changes, beyond any small additions phase W0 finds it needs (section 11);
- a TypeScript SDK;
- hosting the bundle on the project's Pages site;
- offline use;
- push notifications.

## 2. Vocabulary

- **Bundle.** The built web client: `index.html`, the WebAssembly module,
  its JavaScript glue, the highlighter, styles, and icons.
- **Embedded page.** The bundle served by `cookie daemon` from its own
  listener.
- **Hosted page.** The same bundle served by any other web server.
- **Shared client.** The Rust code the TUI and the web client both use: the
  protocol client in `cookie_agent_protocol` and the state crate of
  section 5.

## 3. Architecture

```
crates/protocol            wire types + Client (now also wasm32)
crates/client_state        NEW: reducer and view state, shared by TUI and web
crates/tui                 ratatui rendering over client_state
crates/web                 NEW: Leptos CSR app over client_state (wasm32)
crates/web_assets          NEW: the prebuilt bundle as bytes, for the server
crates/server              serves /ws, /runtime, and now the bundle
```

- **`crates/web`** is a workspace member.
  - It builds for `wasm32-unknown-unknown` with `trunk`.
  - It also compiles natively, so `cargo build --workspace` and the unit
    tests keep working without the wasm target.
  - Natively, only its pure logic is tested. Browser APIs are not called in
    native tests.
- **`crates/web_assets`** includes the bundle at compile time.
  - When no bundle has been built (a fresh checkout, or the regular CI
    jobs), it includes a placeholder page: "the web client was not built;
    run `scripts/build-web.sh`".
  - So `cargo build --locked --workspace` never needs `trunk` or the wasm
    target.
- **Dependency direction.** `web` and `tui` depend on `client_state`.
  `client_state` depends only on `cookie_agent_protocol` and plain crates.
  Nothing in it renders.

## 4. Protocol client on WebAssembly

`cookie_agent_protocol` builds for `wasm32-unknown-unknown` with the client
side only.

- **Features.** The native-only parts are gated by target:
  - the server (`session/server*`);
  - the stdio transport and codecs;
  - `oauth-callback`;
  - `StopSignals`;
  - process helpers.

  On wasm, tokio is limited to `sync` (and `macros` where needed). `uuid`
  and `jiff` enable their browser features for randomness and the clock.
  The core `oven-sdk` crate already has no tokio or HTTP dependency.
- **Executor shim.** `Client` calls `tokio::spawn`, `tokio::time::timeout`,
  `sleep`, and `interval` today. A small internal module provides spawn,
  timeout, sleep, and interval:
  - native: tokio, unchanged;
  - wasm: `wasm_bindgen_futures::spawn_local` and browser timers.

  The client's logic, its limits (2,000-event replay pages, the 5 s page
  timeout, 6 recovery attempts) and its tests stay one copy.
- **Browser WebSocket transport.**
  - The `web_sys::WebSocket` is not `Send`. It lives in one local task that
    exchanges frames with the client over channels, so the client's own
    futures keep their bounds.
  - The transport sends no `Authorization` header (browsers cannot set one).
    The token goes in `ClientHello.token`, as the
    [protocol reference](../reference/protocol.md#handshake-and-authentication)
    describes.
  - A close before the handshake reply is reported like a refused native
    upgrade. A browser exposes no HTTP status for a refused upgrade (Firefox
    reports close code 1006), so an origin refusal cannot be told from
    others: the page shows every refusal before the handshake with the
    `allowed_origins` hint naming its own origin.
- **Reconnect** uses the same backoff (500 ms rising to 10 s) and the same
  `ClientIdentity` across reconnects, so this tab's origin stays stable. The
  instance id lives in `sessionStorage`, so each tab is its own client, the
  same as each TUI process.
- **CI.** A job runs `cargo check --target wasm32-unknown-unknown` for
  `cookie_agent_protocol` (client features) and `cookie_agent_client_state`,
  and clippy over them; the `web` job runs the browser tests (section 12).

## 5. Shared client state

The reducer and view state move from `crates/tui/src/state/` into
`crates/client_state`, and the TUI uses them from there. Their tests move
with them (`state/tests.rs`).

- **Moves:**
  - `StateStore`, `SessionState`, and `reduce_event`/`reduce_transient`
    (event attribution, pending and queued lanes, approvals, goal, live
    tool display deltas);
  - the runtime snapshot revision chain (`state/runtime.rs`);
  - model display names (`state/model_names.rs`).
- **Markdown.**
  - Today the state stores a `MarkdownDocument` already styled with
    ratatui.
  - The state will keep only markdown that any client can use: the source
    text plus a pulldown-cmark event list, or the source alone if parsing
    in each client proves cheap enough.
  - The ratatui styling moves to `crates/tui/src/markdown.rs`, and the web
    client renders the same markdown to HTML.
  - Phase W0 picks the representation by measuring the TUI's redraw cost
    on a long session.
- **Moves too, where it separates cleanly:**
  - the subscription policy (keep the 8 most recently viewed trees
    subscribed, release settled ones, retry a failed recovery after 2 s);
  - treating a `session.resume` "owned by another process" error as a
    read-only snapshot;
  - the model-selection draft: publish after 300 ms, and yield to a
    `session_model_selected` from another origin;
  - upload chunking.

  These are client behaviour, not rendering. Pieces too tangled with
  `App` stay in the TUI, and the web client reimplements them. Phase W0
  records which.
- **Stays in the TUI:**
  - rendering, pointer targets, hover;
  - `tui.toml` and the theme;
  - `$EDITOR` editing;
  - the OSC 52 clipboard;
  - the terminal-only debounces (approval hotkey grace).

## 6. The web client

- **Shape.**
  - **Wide screens:** a session sidebar (list and subagent tree), the
    transcript, and a composer docked at the bottom. Panels (approvals,
    `/runtime`, `/mcp`, `/config`, …) open as side sheets.
  - **Narrow screens:** the sidebar and panels become full-screen sheets,
    and the transcript fills the screen.
- **Commands.** The composer accepts the same slash commands as the TUI,
  with the same names and aliases. Every command is also reachable from a
  menu, because phones lack a convenient `/`. *Superseded on 2026-10-21
  (section 16.3): the web composer has no slash commands; the ⋮ menu has
  them all.*
- **Keys and touch.**
  - **Keys:** Enter steers, Alt+Enter queues, Shift+Enter and Ctrl+J insert
    a newline, Escape cancels, Up recalls.
  - **Touch:** Send steers, and long-pressing Send (or its menu) queues.
  - Every clickable element has a visible hover on pointer devices and a
    visible pressed state on touch, mirroring the TUI's rule that every
    click target hovers.
- **Transcript rendering.**
  - **Markdown:** rendered to sanitized HTML. Raw HTML in model output is
    escaped, never rendered.
  - **Display blocks:** every kind gets its own renderer: `text`,
    `markdown`, `code`, `file_excerpt`, `diff`, `terminal`, `key_value`,
    `table`, `json`, `media`, `link`, `session_ref`.
  - **Diffs** show real file line numbers and their five context lines.
  - **Terminal blocks** turn SGR sequences into styled spans.
  - **Tool names:** no renderer branches on a tool's name.
  - **Unknown blocks** use their text fallback, and unknown events render
    as nothing.
- **Media.**
  - Attachments come from the file picker, drag and drop, paste, and the
    camera on phones.
  - They are offered only for the media kinds the selected model's
    `input_media` accepts.
  - On a too-large rejection, the client offers "downscale and attach?".
    Images are re-encoded through a canvas; video is not downscaled.
  - `media` blocks and attachments are shown inline from `artifact.read`
    through `Blob` URLs.
- **Highlighting.**
  - A JavaScript highlighter is shipped in the bundle (no CDN, since a
    daemon may run with no internet) and called through `wasm-bindgen`.
  - It is loaded lazily on the first code block.
  - The highlighter is highlight.js.
- **Editors.** `/config` and `/memory` edit in the page.
  - They keep the TUI's revision and conflict handling: `config.apply`
    reports configuration errors, and a memory edit that conflicts with a
    newer revision asks what to do.
  - Version 1 uses a monospace text area.
- **Clipboard.**
  - Copy uses `navigator.clipboard`. Browsers offer that only in a secure
    context (HTTPS or localhost).
  - Elsewhere the client falls back to selecting the text and asking the
    user to copy it.
- **Preferences.**
  - Theme (light, dark, or system) and the `/events` level filter live in
    `localStorage`.
  - There is no server-side preference store.
- **Styling.**
  - Tailwind CSS, wired in through Trunk's `rel="tailwind-css"` asset. Trunk
    runs the standalone Tailwind CLI at a version pinned in `Trunk.toml`, so
    the build needs no npm or `node_modules`.
  - Components use Tailwind utility classes in their `view!` markup.
    Tailwind finds them by scanning `crates/web/src/**/*.rs` (an `@source`
    entry), so class names are always complete string literals, never
    assembled from pieces at runtime.
  - The palette is defined once as Tailwind theme tokens (CSS variables):
    the landing site's colours, which are the TUI colours from
    `crates/tui/src/theme.rs`. Light and dark variants use Tailwind's
    `dark:` variant, following the system setting or the user's override.
  - Other web assets come in through Trunk's standard asset pipeline too:
    the highlighter script, fonts, icons, and the manifest are copied by
    Trunk, not by hand. Copied files keep their names (the manifest names
    its icons by path), so they are revalidated rather than cached forever
    (section 7.1).
  - JetBrains Mono for code. Fonts are bundled, not loaded from Google.
  - Icons are Font Awesome Free (CC BY 4.0, credited on the connect screen
    and in the connection panel) from the `icondata` crate, rendered by
    `leptos_icons` as inline SVG that takes the text colour. Only the icons
    the page names are in the bundle; there is no icon font. Icon-only
    buttons have an `aria-label` and a tooltip.

## 7. Hosting and authentication

### 7.1 Embedded page

- `cookie daemon` serves the bundle on its listener, over TLS when TLS is
  configured.
  - `GET /` returns `index.html`.
  - The bundle's files are served at their paths.
  - `/ws` and `/runtime` keep their meaning, and any other path is a 404.
  - Files whose names carry Trunk's content hash (the WebAssembly module,
    its glue, the stylesheet) are sent with `Cache-Control: immutable`.
    `index.html` and the files Trunk copies under their own names (fonts,
    highlighter, manifest, icons, screenshots, the service worker) are sent
    with `no-cache` and an `ETag`, so a restarted daemon's page always
    matches its own build.
  - A compressible file is sent gzip-compressed to a browser that takes
    gzip; there is no Brotli.
- **Serving the page needs no token.** The page holds no secret. Every
  connection it opens still authenticates in the handshake.
- **One listener for both.** The page and `/ws` share the daemon's TCP
  listener. Anything that forwards that one port forwards both, and the
  page finds the daemon at its own address (7.2).
- **Origins have no special case.**
  - The page's `/ws` connection is a browser connection like any other.
    It carries the page's `Origin`, and the daemon accepts it only if
    `[server] allowed_origins` lists that origin or is `["*"]`.
  - The default list is empty, so the embedded page cannot connect until
    the user lists the origins they open it from. Examples:
    `http://127.0.0.1:7419`, `http://192.168.1.5:7419`, or a tunnel's
    `https://cookie.example.com`.
  - The daemon trusts no headers to work out where it is reachable from.
  - **The user reversed this on 2026-10-21:** `[server] allowed_origins` is
    removed. `/ws` accepts an upgrade from any `Origin`, or none, because a
    reverse proxy makes browser-origin gating pointless and the token is the
    real guard; `/runtime` still refuses any request with an `Origin`. A
    config that still has the key fails as an unknown key.
- **Telling the user what to add.**
  - A refused connection gives the page no reason to show. When the
    handshake never happens, the connect screen names the likely cause:
    "add `"<location.origin>"` to `[server] allowed_origins` in the
    daemon's `config.toml` and restart it". It quotes its own origin
    exactly.
  - When `allowed_origins` is empty, the daemon says, in a log line after
    its ready line, that the web page needs an entry before it can
    connect.
  - **Since 2026-10-21** (the allowlist's removal) the connect screen says
    only that the daemon refused or closed the connection before its
    handshake, and to check the daemon URL and that the daemon is running.
    The daemon prints the page's address after its ready line and nothing
    about origins.
- **Plain `cookie`** opens no port and serves nothing.

### 7.2 The connect screen

The same page runs embedded in the daemon or from any other static host, and
both open on one connect screen.

- **The daemon URL is filled in from the page's own address.**
  - The scheme changes from `http` to `ws`, or from `https` to `wss`.
  - Host and port stay the same.
  - `ws` is appended to the page's directory: `https://abc.efg/` gives
    `wss://abc.efg/ws`, and `https://abc.efg/cookie/` gives
    `wss://abc.efg/cookie/ws`.
- **What the fill covers.**
  - The embedded page, reached directly or through any forward of the
    daemon's port, always gets the right URL.
  - A page on a static host without a daemon gets a URL to edit. The edited
    URL is remembered in `localStorage`.
- **When the page connects by itself.** It connects without a click when it
  has both a URL (filled in or remembered) and a token (from `#token=` or
  `localStorage`, section 7.3). Otherwise the screen waits for input.
- **Relative paths.** The bundle uses relative paths only, so it works from
  any path on any static host.
- **HTTPS pages.** A page served over HTTPS can only reach `wss://`
  daemons. The [web clients](../daemon/remote-access.md#web-clients) section
  already says so, and the fill keeps the page's scheme.
- **Version skew** between page and daemon is handled by the handshake:
  same major required, and the page gates methods on the daemon's minor.
- **Archive.** The release attaches the bundle as an archive, so it can be
  hosted anywhere.

### 7.3 Getting the token to the page

- **`cookie web`** is a new CLI command. It prints the page URL with the
  token in the fragment: `http://<host>:<port>/#token=<token>`, or `https`
  when the daemon uses TLS.
  - URL and token resolve like `cookie attach`: `--url` and `--token`
    flags first, then `[server] url` and `[server] token` in this machine's
    configuration.
  - `--open` opens the system browser.
  - The URL is derived from the client URL: `ws`→`http`, `wss`→`https`,
    and the `/ws` path dropped.
- **The fragment never reaches a server or its logs.** On load, the page
  reads `#token=`, stores the token in `localStorage` under the daemon URL,
  and removes the fragment from the address bar.
- **Fallback:** a paste box on the connect screen.
- **The daemon's ready line** is unchanged, since supervisors parse it. The
  page URL, without the token, follows it as a log line.
- **Forget:** a "Forget this daemon" action deletes the stored token.

## 8. Feature parity

Everything in the TUI except the two exclusions. Rows marked as phases name
where each feature lands.

| Area | TUI surface | Web phase |
|---|---|---|
| Sessions | create, `/sessions` list filtered by cwd, resume, read-only snapshot, live `sessions.changed` | W2 |
| Transcript | markdown, every display block, live tool progress, diffs, terminal SGR, full-content `artifact.read` | W2 |
| Composer | submit (steer/queue/start), queue strip, recall, cancel | W2 |
| Approvals | panel, respond with revision and fingerprint | W2 |
| Models and agents | `/model`, `/agent`, `/preset`, debounced `session.select_model` | W2 |
| Permissions | mode toggle; `/permissions` overrides and preview | W2 (toggle), W3 (overrides) |
| Usage | `/usage`, cost in the status bar | W2 |
| Subagents | tree panel with fold modes, `session_ref` navigation | W3 |
| History edits | fork, revert, `/compact` and its cancel | W3 |
| Goal | goal bar, `/goal` set, pause, resume, cancel | W3 |
| Background | backgrounding a running call, the bar, `/background` list and stop | W3 |
| Media | `/attach`, chips, upload, downscale offer, inline media | W3 |
| Skills | `/skills` search and run | W3 |
| Runtimes | `/runtime` attachments, draft runtimes, reconnect | W4 |
| Machines | rename, assign, unassign, merge, forget | W4 |
| MCP | list, add, edit, remove, enable, reconnect, persist; OAuth sign-in since [web MCP OAuth](web-mcp-oauth.md) | W4 |
| Providers | `/connect` forms, disconnect | W4 |
| Configuration | `/config` documents, apply, reload | W4 |
| Memory | `/memory` by layer, edit at revision, delete | W4 |
| Plugins | `/plugins` status, restart, plugin status lines | W4 |
| Diagnostics | `/events` level filter | W2 |

**Excluded:**

- **MCP OAuth.** `mcp.auth.begin` requires a loopback `redirect_uri`
  (`crates/mcp/src/oauth.rs`).
  The web client shows the server as needing authorization and tells the
  user to run `cookie mcp auth` or use the TUI. (Closed on 2026-10-21 by
  [MCP OAuth from the web client](web-mcp-oauth.md): the page is its own
  redirect, client protocol 1.11.0.)
- **This machine as a runtime.** A browser has no workspace to offer.

## 9. Phones

- **Layout.** Layouts work from 360 px wide. Touch targets are at least
  44 px, nothing scrolls sideways, and long lines in code, diff, and
  terminal blocks scroll inside their block.
- **Home screen.**
  - The bundle has a web app manifest and icons (any and maskable, 192 and
    512 px, an SVG, and an Apple touch icon), `display: standalone`, and a
    theme colour for light and dark, so it can be added to the home screen.
  - The spec first left a service worker out: it would need a secure
    context and adds caching the daemon's version matching does not want.
    **The user reversed this on 2026-10-21:** the page advertises itself as
    an installable app, with a service worker that caches nothing (below).
- **Installable app (user decision, 2026-10-21).** Users opening the page in
  a browser, typically over `tailscale serve` HTTPS
  (`https://<host>.<tailnet>.ts.net/` in front of the loopback daemon), can
  install it to a phone or desktop.
  - **Manifest.** `name`, `short_name`, and `id`, `start_url`, and `scope`
    of `./` (relative, so the page installs from any path), `display:
    standalone`, background and theme colours, the icons above, and a
    narrow and a wide screenshot for Chromium's richer install dialog.
  - **Service worker, network-only.** `sw.js` beside `index.html`, so its
    scope is the page's directory without `Service-Worker-Allowed`. It
    caches nothing: no Cache Storage, and its fetch handler passes the
    page's own `GET` requests to the network (`respondWith(fetch(request))`)
    and leaves every other request, and `/ws` and `/runtime`, to the
    browser. The page must always match its daemon's build, which is why
    caching stays out. It calls `skipWaiting` and `clients.claim`, and
    `scripts/build-web.sh` stamps the build on its last line, so a new
    build replaces it on the next load. The page registers it only in a
    secure context, with `updateViaCache: "none"`. Since 2026-10-25 it also
    shows push notifications and opens a clicked one's session, still
    caching nothing ([push notifications](push-notifications.md) section
    9.2).
  - **Serving.** `sw.js` keeps its name, so it is sent with `no-cache`
    and an `ETag` like `index.html`, as `text/javascript`. The
    Content-Security-Policy adds `worker-src 'self'` and
    `manifest-src 'self'`.
  - **Advertising it.** An inline script keeps Chromium's
    `beforeinstallprompt` (it may fire before the WebAssembly runs) and
    prevents the mini-infobar. Settings › This browser, the connect screen,
    and a phone's one-time dismissible banner offer **Install app**, which
    opens the prompt; `appinstalled` or running standalone
    (`display-mode: standalone`, iOS's `navigator.standalone`) hides it.
    iOS and iPadOS have no prompt, so they get the steps instead: Share,
    then Add to Home Screen. A page that is not a secure context cannot be
    installed, and Settings says it needs HTTPS. The decision is pure logic
    in `crates/web/src/install.rs`.
  - **Launching.** The app opens at `start_url`, which has no token, and
    connects with the stored one. iOS keeps a home-screen app's storage
    apart from Safari's, so there the token field takes a pasted
    `cookie web` link once. The `theme-color` follows the page's theme,
    the user's override included.
- **On-screen keyboard.** The page's frame and sheets take the visual
  viewport's height, which the keyboard shrinks, so the composer and an
  open sheet stay above it; the frame keeps clear of safe-area insets.
- **Background tabs.**
  - A phone suspends background tabs. On `visibilitychange` to visible,
    the client checks its connection and reconnects through the shared
    client's resync.
  - Lost events are recovered by cursor, as after any reconnect.
- **Remote access.** Reaching a remote daemon from a phone means
  `cookie daemon --listen` on a private network or VPN, or TLS. The guide
  page says the token crosses the network in the clear without TLS, as the
  daemon's `--listen` help already does.

## 10. Build, CI, and packaging

- **`scripts/build-web.sh`**
  - runs `trunk build --release` for `crates/web`, with `wasm-opt` for
    size;
  - copies the result to `crates/web_assets/bundle/`, which git ignores;
  - is pinned to trunk 0.21.14, to the `wasm-bindgen` version in
    `Cargo.lock`, and to the Tailwind version in `Trunk.toml`. Trunk
    downloads its tools on first use, so CI caches Trunk's tool directory.
- **Bundle size.** `scripts/build-web.sh` fails when the bundle, gzipped,
  is over its budget of 10,000,000 bytes; CI reports the sizes in the job
  summary.
- **Regular gates** (`cargo build/test/clippy --workspace`, both
  toolchains) build the native side only and use the placeholder page.
  If the chosen Leptos release needs a Rust newer than 1.88, phase W1
  raises the workspace `rust-version` to match. It also updates the MSRV
  Clippy gate in CI and `AGENTS.md`, so `crates/web` is checked like
  every other crate.
- **New CI jobs:**
  - `wasm`: the wasm `cargo check` and clippy of section 4;
  - `web`: wasm clippy of the page, `scripts/build-web.sh`, and both
    headless-Firefox suites (section 12) with pinned Firefox, geckodriver,
    and `wasm-bindgen-test-runner`, keeping the scenario screenshots of a
    failure.
- **Release.**
  - The release workflow builds the bundle before cargo-dist builds
    `cookie`, so every release binary embeds the real page.
  - `cookie_agent_web_assets` is published to crates.io with the bundle
    included through `include` in its `Cargo.toml`, so `cargo install
    cookie_agent` gets the real page too.
  - The bundle archive is attached to the GitHub release for hosting
    elsewhere.

## 11. Protocol and compatibility

- **No wire change is planned.** The web client speaks client protocol 1.x
  as it is, with `client.kind = "web"`. The daemon never branches on that
  kind.
- **If parity needs a method or a field,** for example a convenience the
  TUI does through several calls, it is a minor bump under the
  compatibility rules and is recorded in the implementation notes.
- **Not part of the client protocol:**
  - The new HTTP routes are not part of the client protocol and carry no
    version.
  - The embedded page always matches its daemon.
- **Origins are unchanged.** `allowed_origins` keeps its current rules
  for every browser connection, the embedded page's included.
- **No configuration changes.** `cookie web` is a new command. Its flags
  follow `cookie attach`. `--standalone` is a new flag of plain `cookie`.
  Finding a local daemon reads the existing `[server]` keys
  (section 13.1).

## 12. Testing

- **Shared state.** The tests that move with it run against both its users:
  the TUI's rendering snapshots stay, and new web rendering tests cover
  each display block kind's HTML.
- **Protocol client on wasm.**
  - `wasm-bindgen-test` in headless Firefox (`scripts/test-wasm-client.sh`)
    covers the browser transport: handshake with token, refused origin,
    reconnect, and replay across a gap. Chrome is not run.
  - It runs against a real `cookie daemon` started by the test harness
    with a scripted provider.
- **End-to-end.** Headless-Firefox scenarios (`scripts/test-web-page.sh`)
  against real daemons and a scripted model cover connecting via
  `#token=`, runs, approvals, steering, queueing, cancelling, subagents,
  fork and revert, compaction, goals, background calls, attachments and
  media, skills, permissions, every management panel, a suspended tab's
  reconnect, read-only snapshots, the refused origin, and the manifest.
  Every view is audited at 360 and 390 px portrait and a landscape phone
  size for sideways scroll, clipped controls, and touch targets.
- **Server.**
  - Bundle serving: paths, cache headers, the 404 rule.
  - `/ws` and `/runtime` precedence.
  - The embedded page's connection is refused without an `allowed_origins`
    entry and accepted with one. The ready line names the missing entry.
- **Connect screen.** URL fill-in for root and subpath pages over `http`
  and `https`; connecting without a click when URL and token are known;
  the origin hint after a refused connection.

## 13. Plain `cookie` and a local daemon

Before this change, plain `cookie` always started a private daemon inside its
own process, even when `cookie daemon` was already running on the machine.
That private daemon could not open a session the other one owned: it showed
the session as a read-only snapshot. Its own sessions were invisible to the
web client and to every other client of the running daemon.

From this change, plain `cookie` follows the decisions in section 1:

1. find a daemon running on this machine and connect to it (13.1);
2. pick the coding runtime for its sessions (13.2);
3. with no daemon found, run in-process exactly as today.

### 13.1 Finding the daemon

- **The token comes first.** The probe happens only when this machine's
  `config.toml` has `[server] token`. `cookie daemon` writes that key on
  its first start and keeps it across restarts. With no token, no daemon
  has run with this configuration, so plain `cookie` starts in-process
  without touching the network.
- **The probe address** is `ws://<[server] host>:<[server] port>/ws`, the
  address a plain `cookie daemon` listens on.
  - It is not `[server] url`. That key may name a daemon on another
    machine (a laptop configured for labpc), and plain `cookie` must never
    go remote on its own. Reaching another machine stays explicit:
    `cookie attach`.
  - Plain `cookie` authenticates with the `Authorization` header and the
    configured token, as `cookie attach` does.
- **Deadlines.** A refused connection falls through to in-process at once.
  The connect and the handshake each have a short deadline (500 ms and
  2 s), so a stuck listener delays startup by seconds at most.
- **What each outcome does:**

  | Outcome | Result |
  |---|---|
  | Nothing listens (refused, or the connect deadline passes) | In-process, silently |
  | Handshake succeeds | Connected to the daemon (13.2, 13.3) |
  | Something answered but the handshake failed: wrong token, another protocol major, a TLS listener, not a cookie daemon | In-process, with a notice (below) |

  The notice appears in the TUI's status area when it starts. It names the
  address and the reason, and suggests `cookie attach` to require the
  daemon or `--standalone` to stop probing. Failing outright would lock
  the user out when, for example, a daemon from an older major keeps
  running after an upgrade.
- **Daemons this does not find:**
  - one started with `--port` or `--listen` on an address the probe does
    not reach;
  - one serving TLS only.

  For those the user runs `cookie attach`, or sets `[server] host` and
  `port` to match.
- **Token exposure.** Plain `cookie` sends the token to whatever listens on
  that loopback port. `cookie attach` with default settings does the same
  today. `config.toml` is owner-only, and the probe only happens when a
  token exists.

### 13.2 Choosing the coding runtime

After the handshake and before the first session is created, plain `cookie`
picks the runtime its sessions use for the current directory. The steps go
in order, and the first match wins:

1. **The daemon is on this machine.** If `ServerHello.machine_id` equals
   this machine's ID, the daemon's `local` runtime serves the directory.
   `session.create` names the directory as `cwd` and no runtime, as
   `cookie attach` does today. If either side has no machine ID, this
   counts as a match, again as `cookie attach` does today.
2. **A runtime of the daemon runs on this machine.** Otherwise, plain
   `cookie` looks for a connected runtime that:
   - `runtimes.list` places on a machine whose `ids` (from
     `machines.list`) include this machine's ID; and
   - offers a capability that takes a workspace.

   Sessions attach that capability at the directory. Example: the daemon
   runs in a container or behind a forwarded port, with
   `cookie runtime serve --daemon` running here. When several runtimes
   match, the lowest runtime ID wins, and `/runtime` shows the choice. The
   choice uses only reported facts (machine, `takes_workspace`), never a
   runtime's kind label.
3. **Nothing on this machine.** Plain `cookie` dials in its own runtime,
   `client_<host>`, limited to the directory and living as long as the TUI.
   `cookie attach` already does this for a daemon on another machine.

`cookie attach` switches to the same three steps, replacing its current
machine-ID check. That way the two never pick differently.

### 13.3 Once connected

- **Behaviour matches `cookie attach`.**
  - The session list is filtered by the directory.
  - A dropped connection is retried with the same backoff. Plain `cookie`
    never falls back to in-process mid-session, because the sessions
    belong to the daemon.
  - Quitting the TUI leaves the daemon running.
- **The TUI shows where it runs.** The status bar shows the daemon's
  address when connected, and `standalone` when in-process. Today the two
  look the same.
- **Version notice.** When the daemon's `engine_version` differs from this
  binary's version, the TUI notes it once: "the daemon runs 0.5.0, this is
  0.6.0; restart the daemon to upgrade". This happens after an upgrade
  while a service daemon keeps running. The protocol versions still decide
  whether the two connect.

### 13.4 Overrides

- **`cookie --standalone`** always runs in-process and never probes. It
  applies to plain `cookie` only. Combined with a subcommand, it is a usage
  error.
- **`cookie attach`** stays explicit:
  - its target comes from `--url`, `--token` (or `COOKIE_DAEMON_TOKEN`),
    then `[server] url` and `[server] token`, and may be another machine;
  - a failure is an error, never a fallback;
  - it chooses its runtime by 13.2.
- **`cookie run`** stays in-process and never probes.
  Headless runs are often scripted, and a script attaching to a long-lived
  daemon by surprise is riskier than a TUI doing so.
- **In-process alongside a running daemon** behaves as today: a session the
  daemon owns opens as a read-only snapshot.

### 13.5 Testing

- **The decision logic is a pure function.** It takes the probe outcome,
  `ServerHello`, `runtimes.list`, `machines.list`, and the local machine
  ID, and returns one of: in-process, in-process with a notice, `local`, an
  existing runtime, or a dial-in. It is tested over every row of 13.1 and
  every step of 13.2.
- **Integration tests** use a scratch home whose `[server] port` points at
  a test daemon:
  - plain `cookie` connects and creates its session through the daemon;
  - with nothing listening, it starts in-process silently;
  - with a wrong token, plain `cookie` starts in-process with the notice;
  - `--standalone` and `cookie run` never connect;
  - a daemon reporting another machine ID gets a dial-in runtime.

## 14. Phases

Each phase lands as focused commits and is pushed when its gates pass.

| Phase | Content |
|---|---|
| **L** | Plain `cookie` and a local daemon (section 13): probe of `[server]`, runtime choice shared with `cookie attach`, `--standalone`, TUI connection indicator and notices, docs. Independent of the web client, so it can land first |
| **W0** | Protocol client on wasm32 (feature gating, executor shim, browser WebSocket transport), `crates/client_state` extracted from the TUI with its tests, markdown representation chosen, wasm `cargo check` in CI |
| **W1** | `crates/web` shell with Trunk and Tailwind (connect screen, token from fragment or paste, reconnect, theme), `crates/web_assets` with placeholder, server bundle routes, connect-screen URL fill-in and origin hint, `cookie web`, ready-line URL, `scripts/build-web.sh`, release plumbing |
| **W2** | Core chat: sessions, transcript and every display block, composer, approvals, model and agent pickers, permission mode, usage, `/events` |
| **W3** | Session depth: subagent tree, fork, revert, compact, goal, background, media, skills, permission overrides |
| **W4** | Management: runtimes, machines, MCP, providers, configuration, memory, plugins |
| **W5** | Phone polish, manifest, end-to-end suite, `clients/web.md`, server and CLI references, specs index status |

## 15. Open questions

None. Every choice was settled on 2026-10-19:

- The probe of `[server]` was chosen over a discovery file.
- `cookie run` stays in-process.
- A daemon run as another OS user is not detected.
- highlight.js, a plain text area for editors, no project-site hosting,
  and a rising MSRV were confirmed.
- `allowed_origins` keeps its current rules with no special case for the
  embedded page. (Reversed 2026-10-21: the allowlist is removed; see
  section 7.1.)

## 16. Addendum (2026-10-21): web layout v2 and session deletion

Status: **implemented** (2026-10-21). Session deletion (section 16.2) is a
minor protocol addition, client protocol 1.5.0, in the daemon, the shared
client state, the TUI, and the web client; the web client's layout (section
16.1) and its delete control build on it. Section 16.1 also records the
decisions the web layout settled while it was built. Section 16.3 records
the web-only decisions of 2026-10-21 on commands, the subagent tree, the
permission mode picker, and the message menu, also implemented.

### 16.1 Web layout v2

**Decided 2026-10-21:**

- **Bottom bar.** The agent, the model, the permission mode, and the cost
  sit together in the bottom bar.
- **Title bar.** Only the chat's title and a ⋮ menu of commands that act on
  this session.
- **Sidebar toggle.** The ☰ becomes a button that shows and hides the
  sidebar, on desktop too.
- **Icon rail.** A rail of icons on the left switches between the views:
  - **Chat:** sessions and the transcript;
  - **Agents:** agent documents and presets;
  - **Settings:** `config.toml`, providers, MCP, plugins, and client
    preferences, in the sections Configuration, Providers, MCP servers,
    Plugins, and This browser;
  - **Machines and runtimes;**
  - **Memory.**
- **Theme toggle.** At the bottom of the rail.
- **Desktop chrome.** Once connected, the app header is gone: the
  connection indicator and the theme toggle sit at the rail's foot.
- **Session panel.** Keeps only the commands and settings that belong to
  the session.
- **Deleting a session.** A trash icon appears on a session row on hover,
  or on a long press on touch screens. It opens a confirmation, and only
  the confirmation sends `session.delete`.

**Decided while the layout was built (2026-10-21):**

- **Subagent tree.** The open session's subagents nest under its row in the
  sessions list, labelled "Subagents". The tree expands on its own while a
  subagent is live; `/agent-panel show|hide|auto` controls the expansion.
  *Superseded on 2026-10-21 by the tree rules of section 16.3.*
- **Children.** Child nodes have no delete control: a subagent goes with
  its root (`not_a_root_session`).
- **Delegate rows.** A delegation row in the transcript shows the child's
  live activity.
- **Title bar when viewing a child.** A breadcrumb from the root to the
  child, which collapses on phones to ‹ followed by the child's title.
- **The mobile layout (approved):**
  - breakpoints below 640 px (phone), 640 to 1023 px (tablet), and 1024 px
    and up (desktop);
  - landscape phones use the tablet layout;
  - a drawer holds the rail and the sessions;
  - the session's commands open in a bottom action sheet;
  - the bottom bar is one row, and it hides while the composer has focus;
  - the status line shows only when it has something to say;
  - lists open their items as list → detail views.

### 16.2 Session deletion

**Decided 2026-10-21:**

- Users delete sessions from the TUI and from the web client.
- **Deleting is a hard delete of the whole session tree from disk:** the
  root session and every delegated subagent session under it, with their
  events, metadata, and artifacts. Nothing is kept for undo.
- A client sends the delete only after the user confirms it.
- The daemon refuses while any session of the tree has an active run, and
  while another process holds the tree's `owner.lock`.
- A delete that succeeds sends the existing `sessions.changed` `removed`
  change to every connection.
- **TUI.** In the session picker (`/sessions`), Ctrl+D, Delete while the
  list has focus, or the picker's Delete button asks to delete the
  highlighted session, in a confirmation over the picker; `y` or Enter (or
  Confirm) deletes and `n` or Esc (or Cancel) keeps it. Ctrl+A, or the
  picker's directory button, switches the list between this directory's
  sessions and every directory's, whose rows then name their directory. The
  picker's action row names both keys.
- **Web client.** Section 16.1's trash icon and confirmation.

#### The method

`session.delete` deletes one root session's tree.

```json
{ "jsonrpc": "2.0", "id": 7, "method": "session.delete", "params": {
    "session_id": "<root session ID>", "expected_last_seq": 42 } }
```

- **`session_id`** names a root session. A delegated session is refused:
  its tree is deleted through its root.
- **`expected_last_seq`** is required, at least 1. It is the root's
  `last_event_seq` from the `SessionMeta` the client showed the user
  (`session.list`, `session.get`, or a `sessions.changed` `upserted`). A
  root whose last event is now another one is refused, so a delete never
  removes a turn the user has not seen, such as one another client just
  added. Only the root's log is compared. A change elsewhere in the tree
  happens during a run, and a running tree is refused anyway.
- **Params are strict** like every command: an unknown member is
  `unknown_field`.

The result names every session that was deleted, the root first:

```json
{ "deleted": ["<root session ID>", "<subagent ID>", "…"] }
```

#### Errors

All are `-32000` registry errors with `message`, `hint`, and `detail`.
Three codes exist already, and three are new in 1.5.0:

| `data.code` | Fields | When |
|---|---|---|
| `session_not_found` | `session_id` | No such session, or it was already deleted |
| `not_a_root_session` (new) | `session_id`, `root_session_id` | The session is a delegated subagent. The hint says to delete its root |
| `session_tree_running` (new) | `session_id`, optional `running_session_id` | A session of the tree has an active run, a compaction, or a subagent waiting for a slot. The hint says to wait for it or cancel it |
| `session_owned_elsewhere` | `session_id` | Another cookie process holds the tree's `owner.lock` |
| `session_changed` (new) | `session_id`, `current_last_seq` | `expected_last_seq` is no longer the root's last event. The hint says to look at the session again |
| `engine_error` | none | The tree could not be moved aside on disk. Nothing was deleted, and `detail` says why |

`session_running` is not reused: its message and hint speak of starting a
run, which a delete does not do.

#### What the daemon does

1. **Checks.** It resolves `session_id`: an unknown session is
   `session_not_found`, a delegated one `not_a_root_session`.
2. **Ownership.** A tree this daemon holds is deleted on the root's session
   actor, which orders the delete after every request already queued for
   the root. A tree it does not hold is not adopted: adoption would repair
   the tree and append events first. The daemon takes the tree's
   `owner.lock` for the deletion itself, which fails with
   `session_owned_elsewhere` when another process holds it. Neither path
   writes an event.
3. **Busy and stale checks.** In a held tree it refuses with
   `session_tree_running` when:
   - a run of any of the tree's sessions is active;
   - a compaction holds one of them;
   - a delegated child waits in the delegation queue;
   - or a resident session's status is `running`.

   A tree whose lock was free has no live run. A `running` status there is
   what a crashed process left, so it does not refuse. Then
   `expected_last_seq` is compared with the root's last event:
   `session_changed`.
4. **Quiescing.** The tree's background calls are stopped without a report.
   Its uploads in progress are dropped. Every resident log of the tree
   closes its append handle, so no handle stays open in the directory
   (Windows cannot rename a directory with an open file).
5. **Commit.** Under the session store's exclusive mutation lock, the root
   directory `sessions/<workdirkey>/<root>/` is renamed in place to a
   tombstone, `sessions/<workdirkey>/.deleted-<root>-<uuid>`, and the work
   directory is synced. The rename is the commit point: from then on no
   listing, lookup, or tree load finds the tree. A rename that fails changes
   nothing and is `engine_error`. On Windows it is retried while another
   program briefly holds a file open.
6. **Forgetting.** The daemon drops what it holds for each session of the
   tree. In the session store:
   - locations;
   - the lazy tree-load state and queued load products;
   - resident projections, evicted summaries, and deferred metadata tips;
   - ownership (the tree's entry, which drops its lock and write authority,
     and each session's writable or adopting entry);
   - live tails;
   - cached read-only logs;
   - per-tree gates.

   In the engine:
   - session actors, whose queued requests then fail with
     `session_not_found`;
   - producers and the goal and producer projections;
   - skill grants and pending skill invocations;
   - compaction state;
   - delegation records and queue entries;
   - delegation recovery claims;
   - pending approvals;
   - the tree's permission mode;
   - the tree's grant journal, after which the visible tree grants are
     rebuilt;
   - the tree's artifact store handle;
   - upload leases;
   - fork idempotency keys whose source or fork was deleted;
   - steer ordering.

   Queued inputs and the goal live in the log, so they go with it.
7. **Unlocking.** The tree lock is released. On Unix it is inside the
   tombstone. On Windows it is the sidecar `<root>.owner.lock` beside the
   tree, which is then removed.
8. **Telling clients.** A `sessions.changed` `removed` change goes to every
   connection for each deleted session, the root first.
9. **Removal.** The tombstone is removed with everything in it. A failure is
   logged and leaves only the tombstone. `SessionStore::open` removes every
   `.deleted-*` entry of its work directory, so a daemon that crashed
   between the rename and the removal cleans up at its next start.

**What is removed on disk** is the root directory and everything in it:

- `events.jsonl` and `metadata`;
- `owner.lock` on Unix;
- `grant-invalidations.jsonl`;
- `artifacts/`: uploads, retained tool output, and attachments;
- `subagents/index.json`;
- `subagents/<child>/` with each child's `events.jsonl` and `metadata`;
- any temporary files.

On Windows the `<root>.owner.lock` sidecar goes too. The work directory's
own files (`cwd`, `layout.json`, and the runtime revision index) are shared
by every session and stay. A fork made earlier is not touched: it
hard-linked its artifacts into its own tree.

#### Subscribers

`sessions.changed` `removed` is the only signal. No `events.subscription`
message is added. When a tree is deleted:

- the daemon drops the live tails of its sessions;
- each connection forgets its subscriptions to them, so it sends nothing
  further for them, and `events.unsubscribe` answers `false`;
- a client viewing a deleted session treats `removed` as "this session is
  gone": it leaves the view and drops the session's state.

A connection that fell behind gets `resync` as before, and the deleted
sessions are then absent from `session.list`.

#### Clients

- **Only from 1.5.** A client sends `session.delete` only to a daemon whose
  handshake `protocol_version` has minor 5 or later. To an older daemon it
  shows "The daemon is too old to delete sessions." without sending
  anything. A `-32601` it gets anyway reads the same way.
- **Shared helpers.** `cookie_agent_client_state` provides:
  - whether the daemon's version supports deleting;
  - the call that builds the params from the `SessionMeta` the user
    confirmed;
  - the outcome the TUI and the web client show.
- **TUI.** The confirmation names the session's title and says how many
  subagents go with it. Refusals show their registry message and hint.

### 16.3 Commands, the subagent tree, and the mode picker

**Decided by the user, 2026-10-21 (web only; the TUI is unchanged):**

- **No slash commands in the web composer.** `/` parsing and the
  completion popup are gone: text that starts with `/` (say `/compact`) is
  sent as a message. The placeholder is "Message". Every session command
  is in the title bar's ⋮ menu; the commands with another place keep it
  (the bottom bar's agent, model, and mode pickers and approvals badge,
  the composer's attach and Stop, the sidebar's New session, and the
  rail's views).
- **The ⋮ menu** lists each command by a readable name with an icon and a
  one-line description, never slash syntax, and each opens a sheet (a side
  sheet on desktop, a bottom sheet on phones) with labelled fields and a
  main button:
  - **Compact context:** an optional **Focus** and **Compact**
    (`session.compact` with the focus); while it compacts, **Cancel
    compaction**, as the composer's Stop and Escape do;
  - **Goal:** the goal with **Pause**, **Resume**, and **Cancel** as the
    goal bar offers them, and a **New objective** field with **Set the
    goal**;
  - **Skills:** a search and the picked skill's arguments with **Run**;
  - **Background calls**, **Runtimes**, **Permission rules**, **Usage**,
    **Diagnostics**, and **Agent preset** open their existing sheets (the
    background sheet says so when no call runs in the background).
- **`/agent-panel` is gone**, with its menu entries. Only the open
  session's row has a subagent tree, in one of two states; a click on the
  open session's own row switches them (a click on another session still
  opens it), and the browser keeps the state per session in
  `localStorage` (`cookie.tree:<session id>`):
  - **collapsed** (the default): the running subagents at any depth, and
    the subagent the page shows; a running subagent's ancestors that do
    not run show dimmed, only to place it; with nothing running, nothing
    shows;
  - **expanded:** the whole hierarchy, finished subagents included, each
    foldable by its chevron.

  The nodes keep their rendering (status, agent, handle, task, a click
  opens the child) and the child's breadcrumb stays.
- **Permission mode picker.** The mode chip no longer cycles modes or
  shows a toast: like the agent and model chips, it has a chevron and
  opens a picker (the side sheet on desktop, a bottom sheet on phones).
  The picker lists every mode with its shield, its name, and a line from
  the permissions guide, checks the current one, and sets the one clicked
  (`session.set_permission_mode`); a footnote says deny rules and the loop
  guard hold in every mode. The web client names the modes as the guide
  does: auto-approve, auto-approve-n, auto-approve-y, ask, yolo.
- **The message menu.** A user message's ⋯ shows only while a mouse or pen
  is over the message's row (the whole band at the bubble's height) or the
  keyboard is in it; its room is kept, so the bubble never moves. A touch
  screen has no ⋯: a long press on the bubble (500 ms, cancelled by
  moving or scrolling, without the browser's callout) opens the same menu.
- **The composer's padding** for its in-box buttons is static CSS chosen
  by which buttons show, so the text clears them from the first render
  (it was measured after a render, and missed the attach buttons that
  appear once the model's media are known).

### 16.4 The Agents, Machines, and Memory views

**Decided by the user, 2026-10-21 (web only; the TUI is unchanged; no
protocol change):**

- **Agents** gets a sidebar like Chat's sessions: the agent documents (the
  shared agents, then each preset's) on the left with an icon for each
  agent's mode, the open agent's editor on the right. The sidebar button
  and its remembered pinning are shared with Chat (`cookie.sidebar`);
  below 1024 px the agents slide over, and a phone's drawer carries them
  beside the rail, as it carries the sessions in Chat. **New agent** and
  **Reload from disk** sit above the list; **Apply**, **Close**, and
  **Delete** (behind a confirmation) at the editor's foot. Settings ›
  Configuration keeps only `config.toml`.
- **Machines** shows each machine as a card with its runtimes, with icons
  for the machine's operating system, each runtime's lifetime (built in,
  spawned from a template, dialled in), and its status. Moving a runtime
  is a pointer-event drag onto another machine's card: a mouse drags once
  it moves 4 px; a finger after a 500 ms long press (moving sooner
  scrolls), during which a non-passive `touchmove` listener keeps the page
  still, and a long press let go in place opens **Move to…**. Every row
  keeps a **Move to…** button for the keyboard, which picks another
  machine or names a new one. A drop on its own machine does nothing;
  runtimes the machine registry places nowhere (never connected) show
  under "On no machine yet" and can be dropped onto a machine to pin them.
- **Add a runtime** in Machines: an ssh form (runtime ID, host, optional
  `--root`) that writes the runtimes guide's template with one
  `config.apply` `set` of `["runtimes", <id>]` (retried once when the
  revision moved, refused when `runtimes.list` or `config.toml` already
  has the ID); the `cookie runtime serve --daemon <url> [--id <id>] --token
  "<the daemon's token>"` command with a copy button, whose URL is the
  page's own unless it is a loopback address (then `<daemon host>`); and
  links to the guides of the released documentation.
- **Memory**: project folders fold, folded by default, and the browser
  keeps an opened one in `localStorage` (`cookie.memory_project:<project>`,
  value `open`; storage that fails leaves folders folding for the page's
  lifetime only). **Delete project** confirms, then sends `memory.delete`
  for each memory of the project at its listed revision; files that are
  not memories stay, and so does the folder when there are any.
- **Agent memory files** (added the same day, after the daemon took over
  their description and first keywords, client protocol 1.6.0): the web
  editor shows an `agent://` file's description read-only and its
  daemon-owned keywords (the agent's ID, then its mode) as fixed, with the
  other keywords in a comma-separated field and the body in the text
  area. A save writes the whole file with the description and keywords as
  read, the extra keywords, and the body; unchanged parts write nothing.
  `memory.write`'s optional `notice` follows "Saved …" in the status
  line, as the TUI shows it; `MemoryWriteOutcome::Saved` in
  `client_state` carries it, and both clients word the status with
  `saved_status`.

## 17. Implementation notes

Filled in as phases land.

### Phase L (2026-10-19)

Landed as specified, with no wire change and no configuration key.

- **Where it lives.** `crates/cookie_agent/src/local_daemon.rs` holds the
  probe and the runtime choice; `main.rs` keeps the wiring thin.
  `cookie attach` and plain `cookie` with a found daemon share one
  function, `run_daemon_tui`, after their different connects.
- **Probe.** The TCP connect runs on its own under the 500 ms deadline, so
  "nothing listens" is told apart from "answered but unusable". The
  WebSocket upgrade then runs over that same connection through a new
  Rust API, `Client::connect_websocket_over` in the protocol crate, and it
  shares the 2 s deadline with the handshake. The configuration is loaded
  once: the in-process daemon composes from the same load, so a broken
  configuration still fails before the probe, as it did before.
- **Decision as pure functions.** The decision is two pure functions, one
  per section, rather than one:
  - `Probe` and its `notice()` cover the rows of 13.1;
  - `choose_runtime` covers the steps of 13.2.

  `runtime_for` reads `runtimes.list` and `machines.list` only when the
  machine IDs differ. A daemon that cannot list them gets a dial-in.
- **Step 2 details.**
  - **Own ID excluded.** Step 2 never picks the runtime ID this client
    dials in as (`client_<host>`). That runtime belongs to another TUI on
    this machine, which limits it to that TUI's directory, and it goes away
    when that TUI quits. Dialing in again under the same ID retries until
    that TUI leaves.
  - **Facts only.** The exclusion is by runtime ID, a reported fact, not
    by kind.
  - **Several workspace capabilities.** A runtime that offers more than one
    capability taking a workspace uses the first in `runtimes.list` order.
  - **Not only `coding`.** `SessionPlacement` gained a `capability`, so a
    chosen runtime's workspace capability need not be named `coding`.
- **Notices (deviation).** The notices render as `NOTICE` rows below the
  transcript, not in the one-line status row:
  - the status row truncates the address and the suggestions at narrow
    widths;
  - the first action overwrites it.

  They stay for the TUI's lifetime, like goal notices. The connection
  label leads the bottom bar: the daemon's address (`127.0.0.1:7419`, with
  `wss://` kept), or `standalone`. `cookie attach` shows it too.
- **Version notice.** It compares the daemon's `engine_version` (the
  server crate's version) with this binary's package version, not with
  `cookie --version`, whose git hash would always differ. `cookie attach`
  shows it too.
- **`--standalone`.**
  - Before a subcommand, `main_result` refuses it with a clap usage error
    (exit code 2).
  - After a subcommand's name, clap rejects it as an unknown argument.
- **Tests.**
  - **Unit.** The pure functions are tested over every row and step.
  - **Against a real daemon.** `crates/cookie_agent/src/tests.rs` runs a
    listening daemon, with a scratch `[server]` pointing at it. It covers:
    connect and create a session through it; no token; nothing listening;
    a wrong token; a listener that closes; a silent listener and the
    handshake deadline; and a dial-in for another machine ID, whose
    session attaches `client_<host>` at the directory.
  - **Binary.** `crates/cookie_agent/tests/local_daemon.rs` runs the real
    binary against a listener at the scratch home's `[server] port` that
    counts connections. Plain `cookie` dials once. Without a token, and
    for `--standalone` and `cookie run`, nothing dials.
- **Not tested end to end.**
  - **The TUI over a pty.** "Plain `cookie` creates its session through
    the daemon" is covered by `session.create` over the probed connection,
    not by driving a TUI over a pty.
  - **Step 2 against a real daemon.** A test daemon reports the test
    process's own machine ID, so it cannot place a runtime on another
    machine. The pure function covers step 2.

### Phase W0a (2026-10-19)

The first half of W0: the protocol client on wasm32 (section 4). The
`client_state` extraction and the markdown choice follow in W0b. There is
no wire change.

- **Gating.** On `wasm32` the crate builds as the wire types and the
  `Client`. Gated out by target:
  - the server (`session/server*`);
  - `paths` and `peer` (the stdio peer and its codecs);
  - `StopSignals`, `oauth_callback`, and the test corpus;
  - `RuntimeMachine::this_machine`.

  Tokio there is `sync` and `macros` only, in a target section of
  `crates/protocol/Cargo.toml`, as are `gloo-timers`,
  `wasm-bindgen(-futures)`, `web-sys`, and the `js` features of `uuid` and
  `jiff`. No `getrandom` is pulled in. `oven-sdk` (unchanged pin) and
  `cookie_agent_identity` build for wasm32 as they are.
- **Executor shim.** `session/rt.rs` provides `spawn`, `timeout`, `sleep`,
  and `interval`:
  - natively it is tokio, with the old `Delay` missed-tick behaviour;
  - on wasm, `spawn_local` and `gloo-timers`, with no `Send` bound.

  The client's logic, limits, and tests stay one copy.
- **Browser transport.** `BrowserWebSocketTransport` keeps the
  `web_sys::WebSocket` in one local task that talks to the transport over
  channels.
  - **Size limit.** A text message over `MAX_WEBSOCKET_MESSAGE_BYTES` ends
    the transport with an error.
  - **Entry points.** A page calls `Client::connect_browser_websocket(url)`,
    then the new `Client::handshake_with_token`. That sends the token in
    `ClientHello.token`, serialized from wiping buffers like other secrets.
- **Refusals before the handshake.** A socket that closes before it opens
  fails with the new `ClientError::ConnectionRefusedBeforeHandshake { code,
  reason }`. That covers a refused upgrade, such as an origin
  `allowed_origins` does not list, and nothing listening.
  - Firefox reports code 1006 and no HTTP status, so the client cannot tell
    an origin refusal from other refusals.
  - The page shows the `allowed_origins` hint for this whole error
    (section 7.2).
- **URLs.**
  - The browser side checks the new `validate_page_websocket_url`: `ws://`
    or `wss://`, with a path ending in `/ws` (`wss://abc.efg/cookie/ws`).
  - Native clients keep `validate_websocket_url`, which requires the path
    to be exactly `/ws`.
  - Both moved to `session/daemon_url.rs`.
- **Tests.**
  - `crates/protocol/tests/browser.rs` runs in headless Firefox. It covers:
    - handshake with the token, and a wrong token;
    - an origin that is not allowed, and nothing listening;
    - an invalid endpoint, and a subpath endpoint;
    - a reconnect that keeps the identity and origin;
    - a reconnect that replays across a gap in which another client
      renamed the session.
  - `scripts/test-wasm-client.sh` runs them against two real
    `cookie daemon`s in scratch homes: one allowing `"*"` and one allowing
    no origin. It passes their URLs and token in at compile time.
  - Built without them, each test passes without connecting.
  - Natively, a test covers `handshake_with_token` against a token-requiring
    `serve`, and another covers the URL rules.
- **CI.** A `wasm` job checks the crate for wasm32 and runs clippy over it
  with `websocket` and its tests. The browser tests are not run in CI: they
  need Firefox, geckodriver, and `wasm-bindgen-test-runner`.

### Phase W0b (2026-10-19)

The second half of W0: the shared client state (section 5). There is no
wire change, and the TUI's behaviour and tests are unchanged, except for
paths that moved.

- **New crate.** `crates/client_state` (`cookie_agent_client_state`) is
  published with the other library crates, after `cookie_agent_protocol`.
  - It depends on `cookie_agent_protocol` and plain crates only:
    `pulldown-cmark`, `foldhash`, `jiff`, `serde_json`, `base64`, and
    `web-time`. `web-time` gives the replay watchdog an `Instant` that
    works in a browser, where `std`'s panics.
  - Nothing in it renders, and it builds for wasm32.
  - The TUI's `state` module is now a re-export of it.
  - **Visibility.** `SessionState`'s bookkeeping fields became public,
    because the TUI's tests build it with struct-update syntax.
  - **Test helpers.** The few helpers the TUI's tests use sit behind a
    `test-support` feature.
- **Moved.**
  - The reducer, `StateStore`, `SessionState`, the runtime snapshot chain,
    model display names, and their tests.
  - The markdown parse (`client_state::markdown`).
  - `ownership`: classifying a `session.resume` result, with "owned by
    another process" meaning a read-only snapshot.
  - `model_draft`: the 300 ms publish debounce as a generation check, and
    yielding to a live `session_model_selected` from another origin.
  - `subscriptions`:
    - the eight-tree recency set with its eviction;
    - `SessionState::is_settled`;
    - the 2 s retry wait;
    - the redial backoff, 500 ms to 10 s.
  - `uploads`: the model's accepted media, the chunked
    `artifact.upload.*` exchange, and its refusal messages.

  No timer runs in the crate: it returns the delays, and the client
  sleeps them.
- **Stayed in the TUI.**
  - **The subscription state machine** (subscribing, live, recovering,
    resubscribe-after-ownership). Its transitions start the TUI's RPC
    tasks. It shares the ownership sets with the view, and it holds the
    watch the session switch waits on. Moving it needs a command interface
    better shaped with the web client as a second user (W2).
  - **The ownership bookkeeping.** The generation map and the owned and
    read-only sets are read across the view.
  - **The draft's reset intent and fallback resets.**
  - **Upload file handling.** Reading the file and sniffing its content
    with `runtime_api`'s `validate_upload` stay in the TUI, because that
    crate is not a plain dependency. The web client needs its own sniffing
    or a move of that function.
  - **Everything section 5 lists as staying.**
- **Markdown: measured.** The release-mode `transcript_perf` harness ran on
  150 runs: 632 items, 12,814 lines, 300 code blocks, 419 KB of reply
  markdown. Before and after the move it measured:
  - unchanged redraw: 0.60 ms (0.59 ms with descendant rows), the same as
    before;
  - streaming text delta and redraw: 1.06 ms;
  - first draw: 1.7 to 1.8 s, the same within noise;
  - resize: 92 ms.

  Two further measurements:
  - Parsing all 419 KB of reply markdown from source takes 1.26 ms, twice
    an unchanged redraw.
  - Streaming an eight-run reply in 909 deltas costs 1.16 ms when only the
    open tail is re-parsed, and 33.9 ms when the whole reply is re-parsed
    after every delta.
- **Markdown: chosen.** The state keeps the source plus pulldown-cmark
  events per top-level block, parsed incrementally, as the TUI already did.
  - A source-only state would make each client re-implement the
    incremental open-tail parse, about 30 times cheaper per streamed delta
    than parsing the whole reply.
  - It would also make each client cache its parse to keep redraws at
    0.6 ms.
  - The ratatui styling, the syntax highlighting, and the TUI's per-block
    render and highlight caches stay in `crates/tui/src/markdown.rs`. They
    live in each block's `RendererCache`, a slot that clones of a block
    share, so no redraw re-renders what it did not before.
- **CI.** The `wasm` job also checks `cookie_agent_client_state` and runs
  clippy over it.

### Phase W1 (2026-10-19)

Landed as specified, with no wire change and no configuration change.

- **Leptos and MSRV.** Leptos 0.8.22 is the current stable release; 0.9 is
  a release candidate. It declares `rust-version = 1.88`, and its
  dependencies build on 1.88, so the workspace MSRV is unchanged.
- **`crates/web`** (`cookie_agent_web`, not published) is the client-side
  rendered Leptos page.
  - It contains:
    - the connect screen (URL fill, `#token=` handling, paste box, "Forget
      this daemon", automatic connect, the origin hint after a refusal
      before the handshake);
    - the connected state, showing the daemon's address and versions;
    - redial with `client_state`'s backoff under the tab's identity, kept
      in `sessionStorage`;
    - a check on `visibilitychange`;
    - the theme toggle.
  - It compiles natively, where only the browser connect is a stub. Native
    tests cover `connect` (URL fill, fragment, auto-connect, hint) and
    `theme`.
- **Styling.**
  - Trunk's `rel="tailwind-css"` asset runs Tailwind 4.3.3, pinned in
    `Trunk.toml` with wasm-bindgen 0.2.126 (Cargo.lock's) and wasm-opt
    `version_131`.
  - `style/main.css` defines the landing palette as `@theme` colour
    tokens, `@source`s `src`, and makes `dark:` follow a `dark` class set
    from the system setting or the stored override.
  - JetBrains Mono (regular and bold, with its OFL licence) is bundled
    through Trunk's `copy-dir`.
- **Build.** `scripts/build-web.sh` checks the pinned trunk (0.21.14) and
  wasm-bindgen versions, then builds with a new `web` cargo profile
  (`opt-level = "z"`, fat LTO, `panic = "abort"`) and `wasm-opt -Oz`.
  - It copies the result to `crates/web_assets/bundle/` and prints sizes.
  - `wasm-opt` needs the feature flags current Rust emits (bulk memory and
    others), passed in `index.html`.
  - W1's bundle is 3.24 MB, 1.24 MB gzipped. Most of it is the 3.0 MB
    wasm, mainly the protocol crate's types.
  - The daemon serves it uncompressed. Precompressed serving and a size
    budget are left for W2, which section 10 allows.
- **`crates/web_assets`** embeds the bundle through a build script that
  writes a table of path, content type, immutability, `ETag`, and bytes.
  - It falls back to a placeholder `index.html`.
  - It is published with the bundle through `include`. Cargo counts the
    git-ignored bundle as uncommitted, so the release publishes this one
    crate with `--allow-dirty`.
- **Server.** A fallback route serves the page, with `/ws` and `/runtime`
  explicit and so taking precedence. Anything else, or a method other than
  GET or HEAD, is a 404.
  - **Caching (deviation).** Spec 7.1 assumed every bundle file has a
    hashed name, but Trunk does not hash files copied with `copy-dir`
    (here, the fonts). So a file is sent `immutable` only when its name
    carries Trunk's hash, and `no-cache` with an `ETag` otherwise, like
    `index.html`.
  - **Headers.** Every file carries `nosniff`.
  - **No origin special case.** The page's own `/ws` connection follows
    `allowed_origins`.
- **Ready line.** The machine-readable `daemon-ready` line is unchanged,
  because its format is pinned for supervisors. The page's address and the
  empty-`allowed_origins` warning follow it as human log lines.
- **`cookie web`.** It prints `<page>#token=<token>`, resolving the daemon
  like `cookie attach`. `--open` runs `xdg-open`, `open`, or `start`.
  `cookie_agent_server::page_url` derives the page from the client URL,
  for this command and the ready lines.
- **CI and release.**
  - **A `web` CI job:**
    - installs trunk through `install-action`;
    - caches `~/.cache/trunk`;
    - runs wasm32 clippy on `cookie_agent_web`;
    - runs `scripts/build-web.sh`, with the sizes in the job summary.
  - **The release workflow:**
    - builds the bundle once (`build-web`);
    - hands it to every `dist` build job and to `publish-crates`;
    - attaches `cookie-agent-web.tar.gz` to the release.
  - `AGENTS.md` lists the wasm clippy gate and the build script.
- **Tests.**
  - **Server:**
    - every page path, its content type, cache rule, and `ETag`
      revalidation;
    - 404s for other paths and methods;
    - `/ws` and `/runtime` precedence;
    - the page's origin refused without an `allowed_origins` entry and
      accepted with one.
  - **CLI:** `cookie web` derivation and the ready-line page lines.
  - **Browser:** `scripts/test-web-page.sh` runs two real daemons, one
    allowing the page's origin and one allowing none, and drives headless
    Firefox through geckodriver. It passes:
    - the page connects by itself from the `cookie web` link, shows the
      connected state, removes the fragment, and stores the token;
    - against the refusing daemon, it quotes its exact origin as the entry
      to add.
  - **Not in CI.** The browser test is not a CI job yet, because it needs
    Firefox and geckodriver; W5's end-to-end suite adds them.

### Phase W2 (2026-10-20)

Core chat landed as specified, with no wire change.

- **Moved into `cookie_agent_client_state`.**
  - **The subscription state machine (`subscriptions::Subscriptions`).** It
    covers subscribing, live, and recovering, a resubscribe once ownership
    arrives, the recent-tree release, and recovery after a failure.
    - Its methods return the `events.subscribe` requests to send (a
      `Subscribe` with its attempt), and the client reports each outcome
      back (`finish`, `note_replay_end`, `release`, `recovery_failed`).
      The client owns the clock and the RPCs.
    - The TUI drives it from `ui/app/subscriptions.rs`. The `watch` its
      session switch waits on stays in the TUI, keyed by session.
    - The TUI's tests are unchanged except that
      `app.next_subscription_attempt` reads
      `app.subscriptions.last_attempt()`.
  - **`session_actions`, also switched over in the TUI:**
    - installing an `approval.list` into the store;
    - the permission-mode cycle and its labels;
    - toggling and finishing the complete content of a cut block
      (`toggle_full_content`, `finish_full_content`,
      `FullContentState::of_read`, `block_full`).
  - **Ownership bookkeeping stayed in each client (deviation).** The
    generation map and the owned and read-only sets stayed, because the
    TUI's tests read and write those `HashSet`s and `HashMap`s directly. The
    web client keeps the same small bookkeeping over the shared
    `ownership::classify_session`.
- **The page** (`crates/web/src/{workspace,views}.rs`):
  - **Sessions:** a list with a directory filter fed by the sessions'
    `cwd_identity` (all directories by default) and live
    `sessions.changed`.
  - **New session:** a draft whose first message creates the session. It
    attaches `global` and `local`'s `coding` at the named directory, or
    takes the daemon's defaults, and applies the draft's mode.
  - **Ownership:** read-only snapshots for sessions another process owns.
  - **The shared reducer,** fed by every delivery, with voided input
    restored to the composer.
  - **Composer:**
    - Enter, Alt+Enter, Shift+Enter, Ctrl+J, Escape, and Up, mapped by the
      pure `keys::composer_action`;
    - Send, with a long press or the ▾ menu to queue;
    - the queue strip with recall;
    - `run.cancel`;
    - one `session.submit_input` per message.
  - **Approvals:** answered at their revision and fingerprint, refreshed
    on `approvals_changed`.
  - **Pickers:** agent, model and variant, and preset, from the
    `runtime.snapshot.get` / `runtime.changed` chain. The draft is
    published after 300 ms and yields to another client's pick.
  - **Status bar:** the permission mode and its cycle, the cost (from
    `session.usage`, debounced 250 ms as in the TUI), the `/usage` panel
    with the session and its tree, and the context size.
  - **`/events`:** the level filter, kept in `localStorage`.
  - **Commands:** the TUI's names and aliases for W2's commands (`/new`,
    `/sessions` with `/resume`, `/load`, `/continue`, `/agent`, `/model`,
    `/preset`, `/usage`, `/cancel`, `/events`), plus two with no TUI
    command of the same name: `/approvals` (the TUI docks approvals over
    the composer) and `/mode` (the TUI cycles the mode from its bottom
    bar). The ☰ menu lists them all.
- **Transcript HTML** (`crates/web/src/render/`): pure functions, tested
  natively.
  - **Markdown** comes from `MarkdownBlock` events. Raw HTML becomes text;
    links are kept only for `http`, `https`, `mailto`, and relative URLs,
    and open in a new tab without an opener; images become links.
  - **Display blocks:** every kind has its own renderer, and none branches
    on a tool name.
    - Diffs number lines from the hunks' starts.
    - Terminal SGR becomes spans: classes for the 16 standard colours in
      both palettes, inline styles for 256-colour and RGB.
    - Media blocks are read with `artifact.read` into `Blob` URLs.
    - A block of a kind this build does not know renders nothing, as in
      the TUI. The daemon already turns a newer kind into its `text`
      fallback for an older client.
  - **Interaction.** Interactive parts carry `data-action` attributes,
    which one click handler dispatches.
  - **Re-rendering.** The page re-renders only items whose `item_key` (the
    item's version, its tools' revisions, expansion, and the level filter)
    changed.
- **Highlighting.** highlight.js 11.12.0 is vendored with its BSD-3-Clause
  licence and checked against the npm `sha512` integrity, with no CDN.
  - It is copied by Trunk and loaded on the first code block that names its
    language, through `js_sys` calls.
  - Its token colours come from the palette, light and dark.
- **Serving.**
  - `web_assets`' build script gzips each compressible file. The daemon
    sends that copy when `Accept-Encoding` takes gzip, with
    `Vary: Accept-Encoding` and an `ETag` of its own. Brotli is left out:
    it would need another crate, and gzip covers every browser.
  - **CSP.** Every response carries a Content-Security-Policy:
    - `script-src 'self' 'wasm-unsafe-eval'` plus a hash of Trunk's inline
      loader, computed at build time;
    - `connect-src 'self' ws: wss:`;
    - `img-src` and `media-src` with `blob:`;
    - `style-src 'unsafe-inline'`, which inline terminal colours need;
    - no objects, base URI, framing, or form targets.
- **Size.** The bundle is 4.28 MB, 1.65 MB gzipped. Of that, the wasm is
  3.88 MB (1.40 MB gzipped), highlight.js 43 KB gzipped, and the fonts
  187 KB.
  - `scripts/build-web.sh` now fails over a budget of 1,900,000 bytes
    gzipped, which CI enforces in the `web` job.
- **Tests.**
  - **Native web tests:**
    - every block kind's HTML;
    - markdown sanitization (raw HTML escaped, `javascript:` links
      neutralized, images as links, code language classes);
    - SGR to spans;
    - diff line numbers;
    - the composer key mapping and the slash commands;
    - the event-level filter and item keys.
  - **`client_state` tests:** every transition of the subscription
    machine, and the mode cycle.
  - **Server tests:** gzip negotiation and revalidation of the compressed
    copy, and the CSP on every response.
  - **`scripts/test-web-page.sh`.** It now runs a scripted OpenAI-compatible
    model (`scripts/web-e2e/mock-model.py`) behind a daemon whose agent asks
    before `bash`. `scripts/web-e2e/scenarios.py` drives headless Firefox
    through:
    - connecting from the `cookie web` link;
    - a new session with streamed, highlighted markdown;
    - a tool call approved from the approvals sheet, with its terminal
      output;
    - steering a slow run, queueing behind it with Alt+Enter, and
      cancelling it with Escape, after which the unsent messages return to
      the composer;
    - a 375 px window with no sideways scroll and the sessions sheet;
    - the origin hint from a daemon that allows no origin.

    All pass, and the screenshots were checked.
  - **Not in CI yet.** The browser scenarios still need Firefox and
    geckodriver in CI (W5).

### Phase W3 (2026-10-20)

Session depth landed as specified, with no wire change.

- **Moved into shared crates.**
  - **`cookie_agent_protocol::media_sniff`.** Recognizing and validating
    media from its content (`validate_upload`, the image, PDF, audio, and
    video checks, the caps) moved out of `runtime_api::media`, which
    re-exports it. The page sniffs a file exactly as the daemon will.
  - **`client_state::permissions`:** the `/permissions` table rows
    (`permission_rows`, `permission_actions`, the effect and source labels)
    and a draft's rules (`set_draft_rule`, `remove_draft_rule`). The TUI
    uses them.
  - **`client_state::uploads`.** `upload` now returns an `UploadFailure`
    whose `too_large` carries the limits a `media_too_large` refusal names
    (`RefusedLimits`), so the page can offer a downscale to them. The TUI
    keeps showing the message.
  - **`SessionState::user_files`:** the files each user message attached,
    by its sequence, for inline display.
- **The page** (`crates/web/src/workspace/`, `views/depth.rs`):
  - **Subagents.** The Agents panel in the sidebar (and the sessions
    sheet) shows the viewed session's `session.tree`, foldable per session,
    with `/agent-panel show|hide|auto` as in the TUI: `auto` shows it while
    a delegate is live or a child is viewed.
    - The tree is re-read on `tool_call_linked`, `run_finished`,
      `delegation_progressed`, `delegation_finished`,
      `delegate_child_terminated`, and a finished replay of any session in
      it.
    - Its live sessions are subscribed, sharing the root's ownership.
    - A `session_ref` or tree row opens the child. Its root comes from the
      shown tree, the session list, or `session.get`'s origin.
  - **History edits.** A user message's ⋯ menu copies it
    (`navigator.clipboard`, else the text is selected), reverts to before
    it (`session.revert` with `expected_last_seq`, confirmed first; the
    `rewound` notification rebuilds the transcript and the text returns to
    the composer; `stale_revert` is explained), or forks at it
    (`session.fork` with a `client_fork_id`, then opens the fork).
    `/compact [focus]` compacts; Escape and `/cancel` cancel a compaction
    outside a run (`session.compact.cancel`).
  - **Goal.** The goal bar shows the root's goal with the actions its
    status allows. `/goal <objective>` sets it with the draft's selection;
    `/goal pause|resume|cancel` reads `session.goal.get` and changes it at
    its revision. Goals are root-only and read-only snapshots offer none.
    Only `goal_*` events change the bar.
  - **Background.** A running call with `backgroundable` offers
    "⇲ background" (`run.tool_background`). `session.background.list`
    is read after each subscribe and `background.changed` keeps it
    current. The bar and the `/background` sheet list the calls with their
    age and stop one (`session.background.stop`); the sheet closes with
    the last call.
  - **Media.** `/attach` and 📎 open a file input whose `accept` lists the
    selected model's `input_media` types; 📷 (narrow screens, image models
    only) opens one with `capture`. Drop and paste attach too. Nothing is
    offered for a model without input media.
    - Each file is sniffed and checked against the model
      (`attach::assess`), then uploaded at once through the shared chunked
      `upload` (a new session is created first, as in the TUI), and shown
      as a chip.
    - An image over the model's or the daemon's limits, or refused by the
      daemon as `media_too_large`, is offered "Downscale and attach?". It
      is decoded and redrawn through a canvas, as JPEG when the model takes
      it (else PNG), at the size `attach::downscale_target` computes, and
      smaller again until it fits. Video is never re-encoded.
    - Attachments of user messages and `media_file` parts of assistant
      messages render inline through the same `artifact.read` → `Blob`
      placeholders as media blocks.
  - **Skills.** `/skills [search]` lists `skills.list`, ranked as the TUI
    ranks them (`skills::matching_skills`). Running one checks it with
    `skills.get`, then submits `encode_skill_submission`.
  - **Permissions.** `/permissions` shows the effective rules by action
    with their source. An effect click or the new-rule form sets an
    override (`session.permission.set`), and × clears one. While a new
    session is composed, the same table edits the draft's rules, previewed
    with `session.permission.preview` and sent as `permission_rules` on
    `session.create`.
  - **`/events` default.** The level the page starts with is the TUI's
    (`EventLevel::DEFAULT_MINIMUM`, warnings), so `[I]` rows are hidden by
    default; a stored choice still wins.
- **Fixes found on the way.**
  - **Item keys include the session.** Item IDs repeat across sessions, so
    switching between two sessions of the same shape showed the first
    one's cached rows.
  - **Following the transcript.** It stops only when the user scrolls up
    (wheel, touch, or the scrollbar), not when a re-rendered row or a new
    bar briefly moves the scroll position.
- **Deviations.**
  - **`/goal pause|resume|cancel`** are typed arguments. The TUI's palette
    asks only for an objective and puts the controls on the bar; the page
    has both.
  - **The skill arguments, compaction focus, and goal objective** are typed
    after the command (`/skills gre`, `/compact keep the API notes`), or in
    the sheet, instead of a palette step.
  - **Fork and revert** are offered on any user message the session's
    owner can edit, child sessions included, as the daemon decides what is
    allowed.
- **Size.** Profiled with `twiggy` on an unstripped `web` build, the wasm
  was mostly serde code, plus two things the page never needed:
  - `regex`, used only for the control-character table in
    `diagnostics`. It is now a hand-written table, checked against the
    regex in a native test.
  - Runtime schema generation, used by `open_enum!` to list a tagged
    enum's tags. The tags are now listed in each `open_enum!` and checked
    against the schema in a native test.

  Together they were about 1 MB of the wasm: before W3 it was 3.88 MB
  (1.40 MB gzipped), and without them 2.85 MB (1.06 MB gzipped). With all of W3 it is 3.19 MB (1.20 MB
  gzipped), and the bundle 3.60 MB (1.45 MB gzipped). The budget in
  `scripts/build-web.sh` is now 1,600,000 bytes gzipped (from 1,900,000).
- **Tests.**
  - **Native:** tree rows and folding and the `auto` panel; `/goal`
    arguments and the bar's actions; skill ranking; the `accept` list, the
    assessment of a file (attach, downscale, refuse), downscale targets
    and refused-limit tightening; attachment names; the permission rows
    and draft rules; `user_files`; session-scoped item keys; the slash
    commands with arguments.
  - **`scripts/test-web-page.sh`** now gives the daemon a model that takes
    images, a `helper` subagent, a `greet` skill, an image in the
    workspace, and a session made by `cookie run` before the daemon
    started. The scenarios add:
    - a delegation shown in the Agents panel (`/agent-panel show`), its
      `session_ref` opening the child, and folding;
    - a fork at the first message and a revert of the last, with its text
      back in the composer;
    - `/compact` with a focus;
    - `/goal` set, then paused from the bar;
    - a slow `bash` call approved, moved to the background, listed with
      `/background`, and stopped;
    - an image attached from the file input, and one too large downscaled
      on acceptance, both shown inline in the sent message;
    - a `read` of an image whose media block shows inline;
    - `/skills` search and run;
    - a session permission override added and removed, and a draft rule
      previewed for a new session;
    - the `cookie run` session, whose owner lock the scenario holds as
      another process would, opening as a read-only snapshot.

    All pass, and the screenshots were checked.

### Phase W4 (2026-10-20)

Management landed as specified, with no wire change.

- **Moved into `cookie_agent_client_state`, with the TUI switched over:**
  - `runtimes`: attachment rows matched to their instances, a new session's
    draft runtimes, the attach choices and their search, the attach and
    retry requests, the machines view's rows, the machine actions each row
    offers, the changes they make, and their status lines.
  - `mcp`: the server order, which servers `mcp.server.*` edits, a server
    form as text and the definition it makes (`McpFields`), and the labels.
  - `providers`: a provider row's state, the model-availability and catalog
    lines, the setup values and credentials a connect sends, and the
    store-contention policy. Credentials are serialized once from the
    caller's buffers into a zeroized buffer (`credential_values`), as the
    TUI did.
  - `config`: the document rows, a new agent's template and path rules,
    and what an apply or reload answered and what the panel says about it.
  - `memory`: the rows by layer and group, a new memory's template and URL
    rules, what a write answered, and a refused delete's message.
  - `plugins`: a plugin's state and details, and its events' status line.
  - `tool_header` and `rows`: a tool call's header text, which drops an
    argument its title already names, the hint icons, and the producer and
    goal row labels.
- **The page** (`crates/web/src/workspace/manage.rs`,
  `crates/web/src/views/manage.rs`):
  - **`/runtime`:** the session's attachments with their status, attach
    (search, then a workspace when needed), detach, and retry
    (`runtimes.reconnect`). In a new session, the draft's runtimes start as
    `global` and `local`'s `coding` at the chosen directory (else where the
    daemon's own `coding` instance works, from `runtimes.list`). Each opens
    at once, and `session.create` attaches them. The **Machines** tab
    renames, merges, forgets, assigns, and unassigns. The panel re-reads
    `runtimes.list` and `machines.list` every second while open, as the TUI
    polls, and on `runtimes.changed` and `machines.changed`.
  - **`/mcp`:** the servers in the TUI's order with state, tools, source,
    and error, and add, edit, enable or disable, remove, reconnect, and
    persist. A server that needs authorization says to run
    `cookie mcp auth <server>` or use the TUI (since 2026-10-21 it offers
    **Sign in**; see [web MCP OAuth](web-mcp-oauth.md)). Polled every second while
    open, and on `mcp.changed`.
  - **`/connect`:** the catalog's providers with their state, a connect
    form per auth method with password fields for credentials, and
    disconnect with its confirmation. Connects go through
    `connect_provider_retrying`, and disconnects retry once on store
    contention, as in the TUI.
  - **`/config`:** the documents, an in-page monospace editor, `config.apply`
    at the revision and fingerprint read, refused edits kept to fix,
    conflicts dropped and reloaded, new agent documents, delete, and
    `config.reload`.
  - **`/memory`:** the memories by layer, an in-page editor, writes at the
    revision read, the invalid and changed outcomes with their choices
    (edit again, reload, overwrite, drop), create, and delete with its
    revision. It refreshes on `memory.changed` and `machines.changed`.
  - **`/plugins`:** state and details, restart, `plugins.changed`, and
    `events.plugin` status lines for the viewed session.
- **From the W3 review:** producer messages (goal control and reminders,
  background-call and subagent reports) render as the TUI renders them. A
  claimed or consumed one is a folded line with its summary that opens to
  its owner, delivery, and body. A pending or admitted one does not show
  yet, and a discarded one shows only at the debug level. Goal activations
  show as their `/goal` action and goal states as the objective, status,
  and checklist. Plugin messages, AGENTS.md, loaded skills, and compactions
  fold as in the TUI. Tool titles use the TUI's header text, so a title
  that names its argument does not repeat it.
- **Size.** `Client::call` now serializes its parameters before awaiting
  one shared request future instead of one per parameter type, which saved
  16 KB gzipped. The wasm is 3.63 MB (1.37 MB gzipped), and the bundle
  4.04 MB (1.62 MB gzipped). Most of the W4 growth is each RPC's request
  and answer code. The budget in `scripts/build-web.sh` is raised to
  1,700,000 bytes gzipped, with that reason recorded beside it.
- **Deviations.**
  - **Editors are in the page.** `/config` and `/memory` edit in a text area
    (section 6) instead of `$EDITOR`. A refused config edit stays open in
    the editor rather than reopening on the next edit.
  - **Provider rows** say their state in words, without the TUI's key
    hints.
  - **The TUI's copy changed slightly** where it now shares the web's text:
    a refused config edit says "Not applied; edit config.toml again.", and
    an existing new-agent or new-memory path says "exists; edit it
    instead".
- **Tests.**
  - **Native:** `client_state` tests for the runtime, machine, config,
    memory, MCP, and provider rules, the producer summary, and the tool
    header; web tests for the folded producer rows and the new commands.
  - **`scripts/test-web-page.sh`** now configures a runtime template that
    never starts (`[runtimes.box]`) and the reference plugin. The scenarios
    add:
    - a draft runtime that fails, is retried, and is removed;
    - a session's `local` coding attachment detached and attached again;
    - a machine renamed;
    - an MCP server that cannot start added, disabled, and removed;
    - a provider connected with an API key (which never shows on the page)
      and disconnected;
    - a config edit with an unknown key refused, then a valid edit applied;
    - a memory refused as invalid, created, overwritten after a change on
      disk, and deleted;
    - the reference plugin restarted;
    - goal-control and background-report rows folded, and one opened.

    All pass, and the screenshots were checked.

### Phase W5 (2026-10-20)

Phone polish, the home screen, the end-to-end suites in CI, and the
documentation. There is no wire change.

- **Phones.** A new audit in `scripts/test-web-page.sh` opens every view
  and sheet (the transcript, sessions, every picker, and every panel) at
  360 × 640 and 390 × 844 portrait and 844 × 390 landscape. It fails on
  sideways scroll, a control the screen's edge cuts off, or a target under
  44 px. Firefox windows are at least 500 px wide, so a narrower screen is
  that window with the page zoomed in, which needs geckodriver's
  `--allow-system-access`. It found, and the page now fixes:
  - the composer's Send button cut off at 360 px: the text field could not
    shrink. The composer now wraps its buttons below the field when the
    field would be narrower than 12rem;
  - buttons narrower than 44 px (attach, camera, the command menu, the
    send menu, the permission effects): every button has a 44 px minimum
    width;
  - session rows squeezed into each other in a short landscape sidebar:
    rows no longer shrink;
  - the header wrapping its title and buttons over three lines: shorter
    labels ("Forget", the theme's name) below the `sm` breakpoint.
- **The on-screen keyboard.** The page keeps `--app-height` at the visual
  viewport's height, which a phone's keyboard shrinks while `100dvh` may
  not, and sizes its frame and sheets from it; the viewport meta asks for
  `interactive-widget=resizes-content` and `viewport-fit=cover`, and the
  frame pads by the safe-area insets. Firefox cannot raise a keyboard, so
  this is checked by reading, not by a scenario.
- **Background tabs.** A scenario pauses the daemon (`SIGSTOP`), hides and
  shows the tab, sees the page give up on the silent connection, resumes
  the daemon, and sees the page reconnect and restore its session.
- **Home screen.** `crates/web/assets/manifest.webmanifest`
  (`display: standalone`, relative `start_url` and `scope`), icons drawn
  from the logo by `scripts/web-icons.py` (any and maskable 192 and
  512 px PNGs, the SVG, and an Apple touch icon, palette-reduced to 22 KB
  together), and `theme-color` for light and dark. Trunk copies them under
  their own names; the daemon serves them with `no-cache`, and the CSP's
  `default-src 'self'` already covers the manifest. A scenario checks the
  manifest's content type and that every icon it names is served.
- **CI.** The `web` job now also installs Firefox 157.0.1
  (`browser-actions/setup-firefox`), geckodriver 0.37.1 (checked by
  SHA-256), and `wasm-bindgen-test-runner` 0.2.126 (`install-action`,
  matching `Cargo.lock`; the suite checks the match), then runs
  `scripts/test-wasm-client.sh` and `scripts/test-web-page.sh`, uploading
  the scenario screenshots when a step fails. `release_integrity` pins
  these gates, and `AGENTS.md` lists the two suites.
- **Size.** The bundle is 4.07 MB, 1.64 MB gzipped, within the 1.7 MB
  budget; the manifest and icons add 23 KB.
- **Docs.** `clients/web.md` is the complete guide (phones, the home screen,
  and what the page does not do, besides every panel and command);
  `engine/server.md`, `daemon/persistent.md`, `architecture.md`, and the README
  describe the web client as it is; sections 1 to 14 of this spec now
  match what landed.

### Deviations across the phases

- **Notices of plain `cookie`** render as rows below the transcript, not in
  the status row (L).
- **Refused connections** cannot be told apart in a browser (close code
  1006, no HTTP status), so every refusal before the handshake shows the
  `allowed_origins` hint (W0a, W1). Since 2026-10-21 it is a plain hint to
  check the daemon URL and that the daemon is running.
- **Browser tests run in Firefox only,** not Chrome (W0a, W5).
- **Caching:** only files whose names carry Trunk's hash are immutable;
  copied files are revalidated (W1).
- **The ready line is unchanged;** the page address and the
  empty-`allowed_origins` warning follow it as log lines (W1). The warning
  went with the allowlist on 2026-10-21.
- **Gzip only,** no Brotli (W2).
- **Shared code:** the subscription machine, panel rules, and labels moved
  to `client_state` over the phases; the ownership bookkeeping stayed in
  each client (W2–W4).
- **Commands:** `/approvals` and `/mode` exist only in the page; `/goal
  pause|resume|cancel` and typed arguments replace palette steps (W2, W3).
- **Editors** are in-page text areas, and a refused config edit stays open
  (W4).
- **The budget** was 1.9 MB gzipped in W2, 1.5 MB after W3's size work,
  then 1.6 MB with W3's features and 1.7 MB with W4's panels, each raise
  with its reason recorded (W2–W4). After the UI polish of 2026-10-21 the
  release build (Rust 1.88.0) measured 1.75 MB, and the budget was raised
  to 10 MB, a backstop against runaway growth rather than a target.

## 18. Addendum (2026-10-21): canonical paths and runtime homes

Status: **implemented** (2026-10-21): runtime protocol 1.3.0 and client
protocol 1.7.0 in the protocol, the daemon, and the TUI, then the web
client's display and its runtime cards (section 18.3).

The web client's directory filter listed one directory twice: the page sent
the `~/x` the user typed as `cwd`, which the daemon stored verbatim because
it canonicalized on its own machine and kept the text when that failed,
while the TUI sent `/home/ada/x`. Runtimes already resolved and reported the
canonical workspace, and nothing reported a home, so permission rules
written `~/…` never matched either.

### 18.1 Decisions (2026-10-21)

1. **Paths are stored canonical and absolute, and shown as `~/…`**
   everywhere a client shows them.
2. **A session's directory identity (`cwd_identity`) is its first
   workspace attachment's canonical workspace,** as the runtime reported it,
   never the client's typed text.
   - It is decided once, in `session.create`, after the attachments have
     opened and before the creation event is written: the first attachment
     in attach order whose capability reported a workspace. The daemon does
     not branch on the attachment's kind; in practice it is the first
     `coding` attachment.
   - A session with no workspace attachment keeps the old rule: `cwd`
     canonicalized on the daemon's machine, or the raw text when that fails
     (the daemon's working directory without `cwd`).
   - Existing sessions keep what they stored: there is no migration.
   - Delegated children copy their parent's identity, as before.
3. **Runtimes may report their home directory:** an optional `home` in the
   result of `runtime/initialize`, a canonical absolute path on the
   runtime's machine (runtime protocol 1.2.0 → 1.3.0).
   - It is on the result, not on `RuntimeMachine`: one machine runs
     runtimes of several users, and the machine ID is the operating
     system's.
   - A reader takes an absolute POSIX or Windows path of at most 4096 bytes
     without control characters (`runtime_protocol::is_valid_home`) and
     reads anything else as unreported.
   - `cookie runtime serve` reports its user's home, canonicalized; the
     built-in `global` and `local` report the daemon's. A runtime that
     reports none is fine: clients show its paths absolute.
4. **The client protocol exposes homes and attachments** (client protocol
   1.6.0 → 1.7.0; decided as 1.5.0 → 1.6.0, but `memory.write`'s `notice`
   took 1.6.0 first):
   - `RuntimeListing.home` in `runtimes.list`. Only there, not on
     `RuntimeAttachment`: the home belongs to the runtime, and attachments
     are folded from the persisted `runtime_attached` events, so a home on
     them would need a history change or go stale. A client maps an
     attachment's `runtime_id` to its listing.
   - `SessionMeta.attachments`: the session's `RuntimeAttachment`s now
     (`name`, `runtime_id`, `capability`, `kind`, `workspace`), so session
     lists show and filter by runtime and workspace without a `session.get`
     each.
   - The daemon keeps them current on attach and detach: the metadata
     cache is written through and `sessions.changed` `upserted` goes out.
     Metadata an older build wrote decodes with none until the session
     records an event.
   - Clients gate on minor 7: the TUI fetches homes only from a 1.7 daemon
     (`client_state::paths::daemon_reports_runtime_homes`).
5. **Permission rules:** a pattern `~` or `~/…` expands against the home
   of the runtime the tool runs on, when that runtime reported one.
   - Without a home (the base layer included) it stays literal and so
     matches no absolute label; `~user/…` never expands.
   - A Windows home is spelled with `/`, as path labels are.
   - Expansion is pure and allocation-free when it does not apply. It is
     used wherever a pattern is matched or ranked: agent and overlay rules,
     specificity, skill grants, and the stricter-overlay check.
   - `action@runtime` selectors apply as before.
   - The coding runtime expands a path argument's `~` against the same
     canonical home, so labels and rules agree.
6. **A shared display helper:** `client_state::paths::display_path(path,
   home)` gives `~` for the home itself, `~/rest` under it, and the absolute
   path otherwise.
   - A Windows path keeps its separators (`~\src`) and matches its home
     ignoring case and either separator. There is never a `~` without a
     home, or for a root home.
   - `RuntimeHomes` collects the homes from `runtimes.list` and names a
     session's directory under the home of the runtime whose attachment has
     that workspace.
   - The TUI uses them in the bottom bar's runtime label (replacing its
     `shorten_home`, which used the client's home and mislabelled remote
     runtimes), the session picker's directory labels (now `~/…` rather
     than the basename), and the `/runtime` panel's workspaces.

### 18.2 What the web client does next

- Fetch `runtimes.list` on connect and on `runtimes.changed` and
  `machines.changed` from a 1.7 daemon, and keep `RuntimeHomes`.
- Show every path through `display_path` or `RuntimeHomes`: the sessions
  list's directory names and filter labels (`session_directory`), the
  session panel's runtimes, and the runtime cards' workspaces.
- Filter and group sessions by `cwd_identity` as before; it now agrees
  across clients for new sessions. Runtime cards read
  `SessionMeta.attachments` to show which sessions use a runtime and
  workspace.
- Keep sending what the user typed as `cwd` and `workspace`; the daemon and
  the runtime resolve it.

### 18.3 The web client's paths and runtime cards (implemented 2026-10-21)

**Paths.** The page reads `runtimes.list` when it connects and on
`runtimes.changed` and `machines.changed`, keeps `RuntimeHomes`, and shows
every path through it:

- session rows list where a session works, `~/web, labpc:~/api` (a
  runtime other than `local` is named), or "no workspace";
- the sessions filter is by `cwd_identity`, as before, labelled with
  `RuntimeHomes::session_directory` and the runtime whose workspace it is
  (`labpc · ~/api`);
- the bottom bar shows the session's (or the draft's) workspaces;
- the `/runtime` sheet's attachments;
- a tool row's title names its runtime's paths under that runtime's home.

From a daemon older than client protocol 1.7, which lists no attachments,
a session's place is its `cwd_identity`, absolute.

**New session = runtime cards** (approved plan, 2026-10-21). New session
opens cards in place of the transcript (a phone's fill the screen under the
title bar, in one column, Start pinned at the foot):

- **Groups by machine** from `runtimes.list` (this daemon's machine is
  `local`'s, with `global`), first; then the others by name, then runtimes
  of no known machine. A header folds its group to "name · N runtimes ·
  N ready · N selected"; this daemon's machine and a machine with a
  selected card start open, and the browser keeps each machine's choice
  (`cookie.machine_open:<machine>`).
- **Cards:** `global`, then `local`, then each other runtime's capabilities
  that take a workspace, by runtime ID; a runtime that has not said what it
  offers is one card that asks for a workspace and becomes its capability's
  card once it connected, keeping its selection. States: ready (connected
  or built in), starts on use (a template), offline (a dial-in runtime
  that is not connected; it cannot be selected). A click anywhere on a card
  but its fields selects or deselects it.
- **Workspaces:** a selected card has a field per workspace (more with
  "another workspace": the same runtime at another directory), suggesting
  this browser's recent workspaces on that runtime
  (`cookie.recent_workspaces:<runtime>`) and the sessions' attachments
  there, as `~/…`; the text typed is sent, and the runtime resolves it.
  Start waits for every field. With two workspaces or more a note says
  tools get prefixes, naming the first as `<runtime>_<last component>_bash`.
- **Start** sends `runtimes.reconnect` for every selected workspace at
  once. A refusal shows on its card and nothing is created; once all open,
  the draft composes with them, in card order, at the canonical workspaces
  they reported, plus `global` when selected (`session.create`'s
  `runtimes` and `global`; `cwd` is the first workspace, or none).
- **Defaults:** the open session's attachments; else the browser's last
  selection (`cookie.new_session`); the first time, `global` and `local`
  with an empty, focused field. The old "the daemon's own directory" field
  is gone, and nothing falls back to the daemon's directory.

**Follow-ups:** a subagent's handle truncates before its agent's name, and
the open session's row has a chevron that turns with its tree.

## 19. Addendum (2026-10-21): session row status icons

Client protocol 1.9.0 adds `SessionMeta.waiting_on`: what will start a
session's next run without the user, kept current by `sessions.changed`
`upserted` (see [What a session waits on](../reference/protocol.md#what-a-session-waits-on)).
Session rows show one status icon, from
`cookie_agent_client_state::activity::session_activity`:

- **Running** (`status` is `running`): the running icon. It takes precedence:
  a running session shows as running whatever it waits on.
- **Waiting on a producer** (not running, `waiting_on` not empty): the
  waiting icon, for an active goal, a background call, a subagent's turn,
  or an external producer. Its tooltip names the kinds.
- **Completed or idle** with nothing to wait on: no icon.
- A failed, cancelled, or interrupted last run with nothing to wait on is a
  `Problem` to the helper; rows keep showing it as they do today.

A daemon before 1.9 sends no `waiting_on`
(`activity::daemon_reports_waiting`), so its sessions never show the waiting
icon. The TUI's session picker marks rows `⏳` (running) and `⏰` (waiting)
after the title; the web rows take the icons when they are reworked.

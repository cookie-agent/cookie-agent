# Web Client and Local Daemon Use

Status: **approved** (2026-10-19), being implemented. The decisions in
[section 1](#1-goal-and-scope) come from the planning discussion of
2026-10-19, and no questions are open
([section 15](#15-open-questions)). The spec builds on the
[compatibility spec](compatibility-v1.md), which froze the client protocol and
added browser authentication, and on the
[runtime capabilities spec](runtime-capabilities.md), which added the
listening daemon.

## 1. Goal and scope

Today the TUI is the only interactive client. This spec adds a browser client
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
    whose `allowed_origins` lists that host.
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
  - MCP OAuth, whose redirect must reach a loopback listener;
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
    upgrade. A 403 caused by the origin check is shown as "this page's
    origin is not in `[server] allowed_origins`" when the browser exposes
    enough to tell. Otherwise it is shown as a generic connection failure
    with that hint.
- **Reconnect** uses the same backoff (500 ms rising to 10 s) and the same
  `ClientIdentity` across reconnects, so this tab's origin stays stable. The
  instance id lives in `sessionStorage`, so each tab is its own client, the
  same as each TUI process.
- **CI.** A job runs `cargo check --target wasm32-unknown-unknown` for
  `cookie_agent_protocol` (client features) and `cookie_agent_client_state`.

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
  menu, because phones lack a convenient `/`.
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
    the highlighter script, fonts, icons, and the manifest are Trunk assets
    with hashed names, not hand-copied files.
  - JetBrains Mono for code. Fonts are bundled, not loaded from Google.

## 7. Hosting and authentication

### 7.1 Embedded page

- `cookie daemon` serves the bundle on its listener, over TLS when TLS is
  configured.
  - `GET /` returns `index.html`.
  - The bundle's files are served at their paths.
  - `/ws` and `/runtime` keep their meaning, and any other path is a 404.
  - Bundle files have hashed names and are sent with
    `Cache-Control: immutable`. `index.html` is sent with `no-cache`, so a
    restarted daemon's page always matches its own build.
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
- **Telling the user what to add.**
  - A refused connection gives the page no reason to show. When the
    handshake never happens, the connect screen names the likely cause:
    "add `"<location.origin>"` to `[server] allowed_origins` in the
    daemon's `config.toml` and restart it". It quotes its own origin
    exactly.
  - When `allowed_origins` is empty, the daemon's ready line says the web
    page needs an entry before it can connect.
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
  daemons. The [web clients](../engine/server.md#web-clients) section
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
- **The daemon's ready line** also prints the page URL, without the token.
  The token is already printed separately.
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
| MCP | list, add, edit, remove, enable, reconnect, persist (no OAuth) | W4 |
| Providers | `/connect` forms, disconnect | W4 |
| Configuration | `/config` documents, apply, reload | W4 |
| Memory | `/memory` by layer, edit at revision, delete | W4 |
| Plugins | `/plugins` status, restart, plugin status lines | W4 |
| Diagnostics | `/events` level filter | W2 |

**Excluded:**

- **MCP OAuth.** `mcp.auth.begin` requires a loopback `redirect_uri`
  (`crates/mcp/src/oauth.rs`).
  The web client shows the server as needing authorization and tells the
  user to run `cookie mcp auth` or use the TUI.
- **This machine as a runtime.** A browser has no workspace to offer.

## 9. Phones

- **Layout.** Layouts work from 360 px wide. Touch targets are at least
  44 px, nothing scrolls sideways, and long lines in code, diff, and
  terminal blocks scroll inside their block.
- **Home screen.**
  - The bundle has a web app manifest and icons, so it can be added to the
    home screen.
  - A service worker is not part of this spec. It would need a secure
    context and adds caching the daemon's version matching does not want.
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
  - is pinned to the trunk and `wasm-bindgen` versions in `Cargo.lock`, and
    to the Tailwind version in `Trunk.toml`. Trunk downloads that Tailwind
    CLI on first use, so CI caches Trunk's tool directory.
- **Bundle size.** The budget is tracked in CI and reported, not enforced,
  in phase W1, with a limit set once W2 shows a realistic size.
- **Regular gates** (`cargo build/test/clippy --workspace`, both
  toolchains) build the native side only and use the placeholder page.
  If the chosen Leptos release needs a Rust newer than 1.88, phase W1
  raises the workspace `rust-version` to match. It also updates the MSRV
  Clippy gate in CI and `AGENTS.md`, so `crates/web` is checked like
  every other crate.
- **New CI jobs:**
  - the wasm `cargo check` of section 4;
  - `scripts/build-web.sh`;
  - headless browser tests (section 12).
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
  - `wasm-bindgen-test` in headless Chrome and Firefox covers the browser
    transport: handshake with token, refused origin, reconnect, and replay
    across a gap.
  - It runs against a real `cookie daemon` started by the test harness
    with a scripted provider.
- **End-to-end.** A small set of headless browser scenarios covers:
  connect via `#token=`, start a run, approve a tool, steer, queue,
  cancel, attach an image, fork, and open a subagent. A narrow viewport
  variant covers the phone layout.
- **Server.**
  - Bundle serving: paths, cache headers, the 404 rule.
  - `/ws` and `/runtime` precedence.
  - The embedded page's connection is refused without an `allowed_origins`
    entry and accepted with one. The ready line names the missing entry.
- **Connect screen.** URL fill-in for root and subpath pages over `http`
  and `https`; connecting without a click when URL and token are known;
  the origin hint after a refused connection.

## 13. Plain `cookie` and a local daemon

Today plain `cookie` always starts a private daemon inside its own process,
even when `cookie daemon` is already running on the machine. That private
daemon cannot open a session the other one owns: it shows the session as a
read-only snapshot. Its own sessions are invisible to the web client and to
every other client of the running daemon.

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
| **W5** | Phone polish, manifest, end-to-end suite, `guide/web.md`, server and CLI references, specs index status |

## 15. Open questions

None. Every choice was settled on 2026-10-19:

- The probe of `[server]` was chosen over a discovery file.
- `cookie run` stays in-process.
- A daemon run as another OS user is not detected.
- highlight.js, a plain text area for editors, no project-site hosting,
  and a rising MSRV were confirmed.
- `allowed_origins` keeps its current rules with no special case for the
  embedded page.

## 16. Implementation notes

Filled in as phases land.

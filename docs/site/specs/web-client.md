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
    `cookie mcp auth <server>` or use the TUI. Polled every second while
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

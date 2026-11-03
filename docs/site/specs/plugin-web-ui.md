# Plugin web pages and settings

Status: **implemented** (2026-11-03; client protocol 1.80.0 and 1.81.0,
plugin protocol 1.40.0 and 1.41.0), from the user's decisions in
[section 2](#2-decisions-2026-11-03); how it landed is in
[section 13](#13-implementation-notes-2026-11-03). Builds on
[plugins v1](plugins-v1.md), the [web client](web-client.md), and
[MCP OAuth from the web client](web-mcp-oauth.md).

## 1. Goal and scope

A plugin can declare tools, intercept hooks, watch events, and produce
messages, but it cannot reach the user. Clients see a plugin only through
`plugins.list` and `plugins.restart`. A plugin that needs a client ID, a key,
or an OAuth sign-in (a Google Calendar plugin, say) has no way to ask for
them. Users have to hand-edit `env` in `config.toml`, and nothing can receive
an OAuth redirect.

This spec adds two independent, optional features:

1. **Pages.** A plugin that runs an HTTP server on loopback is served by the
   daemon at `<daemon address>/plugins/<name>/`, on the daemon's own origin.
   The pages are the plugin's: settings, sign-ins, a calendar view, anything.
   An OAuth redirect lands at `/plugins/<name>/…`, so the plugin handles it
   itself.
2. **Settings.** A plugin with no pages can declare a settings form. The
   daemon renders it, stores the values, and passes them to the plugin in
   `plugin/initialize`.

A plugin may also ignore both and read its own configuration: it is a
command-line program with the user's rights.

**Out of scope:** pages of plugins on remote runtimes (they would need a
tunnel through the runtime connection), pages in the TUI, a sandbox for
plugin pages, and OAuth brokering by the daemon.

## 2. Decisions (2026-11-03)

- **No parity across clients.** Plugin pages are a web-client feature. The
  TUI shows a plugin's page URL and nothing more.
- **Same origin, no sandbox.** Plugin pages are served on the daemon's
  origin. They may read the web client's `localStorage`, the daemon token
  included, and use `/ws`. Plugins are already trusted local code
  ([trust](../extend/plugins.md#trust)).
- **Settings are optional.** The daemon may host a plugin's settings form
  and pass the values at initialization. A plugin that wants something else
  reads its own configuration.

- **One process.** The plugin process serves its own pages; there is no
  separate UI program, and no entry that mounts a web app without a plugin.
- **Access** as in [section 4](#4-access): a ticket in `ServerHello`, a
  cookie on the page's base path, public paths, and the order of checks.
- **Settings** are kept in a file per plugin, not in `config.toml`
  ([section 5.2](#52-storage)), and saving them restarts the plugin unless
  it declares `settings_live` ([section 5.3](#53-changes)).
- **The page's address** reaches the plugin at initialize (`ui_base_url`),
  so its tools can send the user to it ([section 3.1](#31-declaring-them)).
- **The example** lives in `examples/plugins/python/` for now.
- **Open** shows a plugin's pages in the side frame by default
  ([section 7.1](#71-web)).

## 3. Pages

### 3.1 Declaring them

The plugin binds a loopback port, usually `127.0.0.1:0`, before it answers
`plugin/initialize`, and reports it in the result:

```json
{
  "ui": {
    "url": "http://127.0.0.1:43117",
    "public_paths": ["/oauth/callback"]
  }
}
```

- `url` must be `http://` to a loopback IP literal or `localhost`, with no
  user info, query, or fragment. Anything else is refused with a plugin
  diagnostic, and the plugin runs without pages.
- The pages are served under the plugin's `name`, which the daemon already
  requires to equal its `[plugins.<name>]` key. The web client labels them
  with that name.
- `public_paths` are paths, relative to the plugin's root, that skip the
  access gate ([section 4](#4-access)). Each one matches itself and
  everything below it.
- The pages are served by the plugin process itself, from the port it
  declares. A plugin that wants a fixed port reads it from its own
  configuration.
- **Its own address.** `plugin/initialize` params carry `ui_base_url`:
  `[server] page_url` joined with `/plugins/<name>/`, and absent when
  `page_url` is not set. A tool can then answer "Not signed in: open
  `<ui_base_url>` to connect your account" without a page request. While
  serving a request, the `X-Forwarded-*` headers of
  [section 3.2](#32-the-proxy) remain the authority, since the page may be
  reached at another address than `page_url`. A changed `page_url` reaches
  plugins when they next start, as other `[server]` changes do.
- A restarted plugin declares its port again, and the daemon routes to the
  new one. While the plugin is not connected, its pages answer
  `503 Service Unavailable` with a short HTML page that names the plugin's
  state.

### 3.2 The proxy

The daemon serves `/plugins/<name>/*` on its HTTP listener, next to `/ws` and
`/runtime`:

- `/plugins/<name>` redirects to `/plugins/<name>/`.
- The `/plugins/<name>` prefix is stripped, and the rest of the path and the
  query are forwarded unchanged. Every method is forwarded.
- **Headers to the plugin.** `X-Forwarded-Prefix: /plugins/<name>`,
  `X-Forwarded-Host` and `X-Forwarded-Proto`, and `X-Forwarded-For`. Host
  and scheme come from `[server] page_url` when it is set. Otherwise they
  come from the same headers on the incoming request, when a front such as
  `tailscale serve` sent them, and otherwise from the request's `Host` and
  the listener's scheme. With these the plugin builds absolute URLs such as
  its OAuth redirect. Relative URLs need nothing.
- **Headers removed.** The daemon removes `Authorization` when it carries
  the daemon token, and the gate cookie, so neither reaches plugin logs.
  Other cookies pass through: a plugin may set its own, scoped to
  `Path=/plugins/<name>/`.
- **Responses** pass through unchanged, headers included. The daemon adds
  no CSP, `X-Frame-Options`, or caching headers of its own, so a plugin
  decides what its pages load and whether they may be framed.
- **Streaming.** Bodies stream both ways with no size bound and no timeout,
  so server-sent events and large downloads work. WebSocket upgrades are
  forwarded.
- **Unknown plugins** answer 404, and so does a known plugin that declared
  no `ui`, but only after the gate passes ([section 4](#4-access)).

### 3.3 What the page may do

The page is on the daemon's origin. It may:

- call the daemon over `/ws` with the stored token, as the web client does,
  for example to list sessions or send a message to an agent;
- link into the web client (`/#session=…`);
- register a service worker scoped to `/plugins/<name>/`.

The plugin docs say plainly that an XSS in a plugin's page leaks the daemon
token, and with it shell access on the daemon's host. Third-party data
(calendar event titles, email subjects) must be escaped. The design does not
guard against this.

## 4. Access

Plugin pages carry the user's settings and accounts, so they are not
readable by anyone who can reach the daemon's address. Today only `/ws` and
`/runtime` are guarded, and a browser authenticates inside the WebSocket
handshake. A plain navigation cannot carry that, so:

- **The ticket.** `ServerHello`, the daemon's answer to an authenticated
  client's hello, gains `page_ticket`:
  `HMAC-SHA256(daemon token, "cookie-page-ticket/1")`, hex-encoded. It is
  not plugin-specific: any later HTTP route the page reaches by plain URL
  can be gated by it. The value is stateless: rotating the daemon token
  revokes every ticket. The web client never derives it itself, so the
  derivation stays in the daemon.
- **The cookie.** After each hello, the web client sets
  `cookie_page_ticket=<ticket>; Path=<base>; SameSite=Lax; Max-Age=34560000`,
  with `Secure` when the page is on `https`. `<base>` is the page's own base
  path, `/` unless a front serves the page under a sub-path.
  - The daemon checks the cookie only under `/plugins/`. It is never accepted
    on `/ws` or `/runtime`, which keep their own token handshake.
  - `Path` is not narrowed to `/plugins/`: it would protect nothing (a page
    on the origin can read the token from `localStorage` anyway), and it
    would be wrong for a page served under a sub-path.
  - The cookie holds the ticket, not the token, because cookies reach places
    `localStorage` does not, such as a front's request logs. A leaked ticket
    opens plugin pages, not the daemon.
  - `Lax`, not `Strict`: an OAuth provider's redirect back is a cross-site
    top-level navigation, and a `Strict` cookie is dropped on it.
  - Not `HttpOnly`: the page script sets it, and the token it is derived
    from is already in `localStorage`.
- **The check.** A request to `/plugins/<name>/…` is decided in this order,
  so that without access nobody can tell which plugins exist:
  1. its path is one of the `public_paths` of a connected plugin `<name>`
     (each matches itself and everything below it): it is forwarded;
  2. it carries neither the cookie nor the daemon token in `Authorization`
     (both compared in constant time): `401` and a short HTML page, "Open
     this from the cookie web client", with a link to the page;
  3. `<name>` is not a plugin with declared pages: `404`; it is one but is
     not connected: `503`;
  4. otherwise it is forwarded ([section 3.2](#32-the-proxy)).

  A valid cookie opens every plugin's pages. There is no per-plugin access,
  no `Origin` check, and no approval.
- **Public paths are for callbacks.** On iOS, an installed web app does not
  share storage with Safari, so a sign-in finished in Safari arrives without
  the cookie. The plugin protects such a path itself, with OAuth `state`.
- **The service worker** leaves `/plugins/` alone: `sw.js` returns early for
  those paths, as it does for `/ws` and `/runtime`.

## 5. Settings

### 5.1 The form

A plugin may declare `settings` in its `plugin/initialize` result. This is an
`ElicitationForm` (`crates/protocol/src/elicitation.rs`), with one addition:
a text input may set `secret: true`.

```json
{
  "settings": {
    "title": "Google Calendar",
    "description": "An OAuth client of type Web application from Google Cloud.",
    "fields": [
      { "name": "client_id", "title": "Client ID", "required": true,
        "input": { "type": "text" } },
      { "name": "client_secret", "title": "Client secret", "required": true,
        "input": { "type": "text", "secret": true } }
    ]
  }
}
```

- The web client renders the form in the plugin's sheet. A secret field is
  a password input. A secret that is already stored shows as "Set", with
  **Replace** and **Clear**.
- ACP elicitation never sets `secret`, and clients that render ACP questions
  ignore it there.

### 5.2 Storage

- The values are kept in `<data_root>/plugin-settings/<name>.json`, mode
  0600, written atomically. They are not kept in `config.toml`, so
  `config.get`, `/config`, and the configuration editor never see them.
  This is how provider credentials already work
  ([engine configuration](engine-configuration.md), decided 2026-10-17).
- `plugins.settings.get` returns the plain values. For a secret it returns
  only whether one is set.
- `plugins.settings.set` takes the changed fields. A secret field takes a
  value, or `null` to clear it. An absent field is unchanged. Values are
  checked against the last declared form (required fields, lengths,
  options), and a failure names the field.
- The server zeroizes received secret strings after decoding, as it does
  for `provider.connect`.
- Values of fields the plugin no longer declares are kept, in case a later
  version declares them again, and are not passed to the plugin.

### 5.3 Changes

- `plugin/initialize` params gain `settings`: the stored values of the
  declared fields. The first start, before any form is known, sends the
  values stored for every field.
- When the user saves, a plugin that declared the `settings_live` feature
  gets a `plugin/settings_changed` notification with the new values. Any
  other plugin is restarted with them, through the normal restart path.
- A form that a plugin's new version changes takes effect at its next
  initialize.

## 6. Protocol

**Plugin protocol, minor bump:**

- `ExtensionInitializeParams.settings: Option<Map<String, Value>>`;
- `ExtensionInitializeParams.ui_base_url: Option<String>`;
- `ExtensionInitializeResult.ui: Option<PluginUi>` (`url`,
  `public_paths`);
- `ExtensionInitializeResult.settings: Option<ElicitationForm>`;
- the `settings_live` feature, and the `plugin/settings_changed`
  notification (daemon to plugin, `{ settings }`).

**Client protocol, minor bump:**

- `PluginInfo.ui_path: Option<String>`: `/plugins/<name>/`, present while
  the plugin has declared pages;
- `PluginInfo.settings: Option<ElicitationForm>`, and `settings_missing`
  (required fields with no value);
- `ServerHello.page_ticket` (section 4);
- `plugins.settings.get`, `plugins.settings.set`;
- `secret` on the elicitation text input.

`plugins.changed` already fires when a plugin restarts or changes state, so
`ui` and `settings` reach clients through it.

**Configuration:** none.

## 7. Clients

### 7.1 Web

The web client has no plugins view yet. Phase 2 adds one under Settings,
next to Machines:

- one row per plugin, with its state, tools, last failure with the stderr
  tail, and **Restart** (what the TUI's `/plugins` shows);
- **Open**, when the plugin has pages. It opens the page in the session
  panel's side frame, a same-origin iframe, with **Open in new tab** beside
  it. The frame is the default because on iOS a new tab of an installed app
  is Safari, which has no cookie;
- **Settings**, when the plugin declares a form, with a dot while a required
  field is missing.

### 7.2 TUI

`/plugins` shows the page's absolute URL (from `[server] page_url`, as asset
links do) and "settings missing" when that applies. It does not edit
settings.

## 8. The example: Google Calendar

`examples/plugins/python/google_calendar.py`, on `cookie_plugin.py` and the
standard library's `http.server`:

- **Settings page:** the client ID and secret, stored in the plugin's own
  file. The page shows the exact redirect URI to register in Google Cloud,
  `<X-Forwarded-Proto>://<X-Forwarded-Host>/plugins/<name>/oauth/callback`.
- **Connect** redirects to Google with PKCE and `state`. The public path
  `/oauth/callback` exchanges the code and stores the refresh token.
- **Tools:** `calendar_list_events`, `calendar_create_event`.
- **A small agenda page,** to show a real UI that escapes event text.

The docs explain Google's client types. A "Web application" client needs a
registered `https` redirect, such as a `tailscale serve` address; plain
`http` is allowed only at `localhost`. A "Desktop" client accepts loopback
redirects only, which a phone cannot use.

## 9. Tests

- **Server:**
  - the proxy forwards the method, path, query, and body, and adds the
    `X-Forwarded-*` headers;
  - the `/plugins/<name>` redirect;
  - streaming and a WebSocket upgrade;
  - the token and gate cookie are removed, and other cookies pass;
  - responses pass through unchanged;
  - 404 for unknown plugins, 503 while one is not connected, and the new
    port used after a restart;
  - without access, an unknown plugin and a known one both answer 401.
- **Gate:**
  - the cookie passes, a wrong cookie gets 401;
  - `Authorization` with the token passes;
  - public paths pass and match their subtree but not a sibling prefix
    (`/oauth/callbackx`);
  - a ticket made with a rotated token fails.
- **Declaration:**
  - non-loopback, user info, query, and fragment URLs are refused with a
    diagnostic;
  - `ui_base_url` is `page_url` joined with the plugin's path, and absent
    without `page_url`.
- **Settings:**
  - set, get, and clear, with secrets never returned;
  - validation against the form;
  - the file mode is 0600;
  - initialize carries only declared fields;
  - `settings_live` gets the notification, and other plugins are
    restarted.
- **Python example:** `check_reference_plugin.py` validates the new
  initialize fields against the published schemas.
- **Web browser suite:** one scenario. Open a plugin page in the frame, and
  check that the cookie is set and a request without it gets 401.

## 10. Docs

- `extend/plugins.md`: a Pages section (declaring, prefix headers, public
  paths, the XSS warning) and a Settings section.
- `development/plugins.md`: the new initialize fields and
  `plugin/settings_changed`.
- `daemon/security.md`: plugin pages share the page's origin; the gate.
- `clients/web.md`: the plugins view.
- The protocol changelogs.

## 11. Phases

Each phase lands as its own commits with every gate green, and is pushed.

1. **Pages in the daemon.** The `ui` declaration and setting, the proxy, the
   gate, `ServerHello.page_ticket`, `PluginInfo.ui_path`, and `sw.js`.
2. **Web plugins view.** The list, restart, **Open** in the frame and in a
   new tab, the cookie set on connect, and the TUI's URL.
3. **Settings.** The form, the store, the RPCs, initialize, `settings_live`,
   and the web sheet.
4. **Example and docs.** The Google Calendar plugin and section 10.

## 12. Open questions

None.

## 13. Implementation notes (2026-11-03)

Implemented in four commits: the pages and their access (client protocol
1.80.0, plugin protocol 1.40.0), the web client's frame with the Python
helper and the example, a fix for a phone layout the suite caught, and the
settings (client protocol 1.81.0, plugin protocol 1.41.0). Where the code
settled what the draft left open, or differs:

- **`ui.url`** takes no path: `http://<loopback>:<port>` with at most a
  trailing `/`, since every path below it is the plugin's own. A port of 0
  is refused.
- **`ui_path`** is relative, `plugins/<name>/`, so it resolves against a page
  served under a sub-path. The name is percent-encoded as one segment, and a
  name of only dots is encoded whole so it is never read as `.` or `..`.
- **Public paths** are matched after percent-decoding each segment, and a
  path with a `.` or `..` segment, an encoded `/` or `\`, or bytes that are
  not UTF-8 never matches, so `/oauth/callback/../settings` cannot reach a
  gated page. `/` as a public path opens every page.
- **The proxy** opens a new loopback connection per request (HTTP/1.1 with
  upgrades), with no timeout. `X-Forwarded-Prefix` puts `page_url`'s path, or
  a front's own prefix, before `/plugins/<name>`. A front's
  `X-Forwarded-*` headers are trusted as they come when `page_url` is not
  set. An unreachable plugin server answers `502`.
- **The ticket** is sent in every `ServerHello`. The web client sets the
  cookie only when the daemon it connected to serves the page (the same
  origin), since another daemon's ticket would replace this one's; **Open**
  shows only then.
- **The frame** is in Settings › Plugins, in place of the list (**Plugins**
  goes back, **Reload**, **Open in new tab**), rather than the session
  panel, which belongs to a session.
- **`PluginInfo`** carries `has_settings` and `settings_missing`, not the
  form: the form and values come from `plugins.settings.get`, so the list
  stays small and comparable.
- **The form stays known while the plugin is down**, from its last
  connection, so its settings can be read and changed then; a daemon that
  has not yet seen the form refuses both.
- **Secrets** are kept in memory and in the 0600 file; received ones are not
  zeroized as `provider.connect`'s credentials are. The web form can replace
  a secret but not clear an optional one.
- **The Python helper** (`cookie_plugin.py`) speaks plugin protocol 1.40.0:
  `Plugin.pages(handler, public_paths)` serves an `http.server` handler on a
  loopback port and declares it to daemons of 1.40.0 or later, and
  `external_url(request, path)` builds an address from the forwarded
  headers. The reference plugin has a page, and the browser suite's
  management scenario opens it in the frame and checks that a request
  without the cookie is refused.
- **The example**, `examples/plugins/python/google_calendar.py`, keeps its
  client and tokens in its own 0600 file and does not use the daemon's
  settings form, since its pages already serve one. Google's sign-in opens
  in a new tab, as Google refuses to be framed.


# Giving agents a browser

[Playwright MCP](https://github.com/microsoft/playwright-mcp) is an
[MCP server](mcp.md) that drives a web browser: it opens pages, reads them as
accessibility snapshots, clicks, types, and takes screenshots. Configured in
cookie, its tools become agent tools like any other MCP server's, gated by the
agent's `mcp` permissions. This page shows how to set it up, where to run it,
and how to decide which agents may use it.

Playwright MCP is Microsoft's project, not part of cookie. The flags and tool
names below are those of `@playwright/mcp` 0.0.83 and the Playwright Extension
0.4.0 (October 2026); check its
[README](https://github.com/microsoft/playwright-mcp#readme) when a newer
version behaves differently.

## Two ways to give agents a browser

| | A browser Playwright launches | Your own running Chrome |
|---|---|---|
| Flag | none (or `--headless`, `--isolated`, `--user-data-dir`) | `--extension` |
| Needs | Node.js and a browser Playwright can start | Node.js, Chrome or Edge with the Playwright Extension, your login session |
| Logins | none, unless you sign in once in its persistent profile | every site you are signed in to |
| Privacy | sees only what the agent opens | sees the tabs you hand it, with your cookies and sessions |
| Interference | its own window, or none when headless | works in your browser while you use it |
| Runs on | any machine, headless on a server | only the machine whose Chrome it attaches to |

**A browser Playwright launches** is the safe default. Playwright starts
Chrome (or Chromium, Edge, Firefox, WebKit) for the agent, headed or
headless, with one of three profiles:

- **persistent** (the default): a profile directory Playwright keeps under the
  user's cache directory, named after the browser and a hash of the server's
  working directory, so a login the agent (or you) makes there survives
  restarts. `--user-data-dir <dir>` puts it somewhere you choose. One browser
  at a time can use a profile.
- **isolated** (`--isolated`): the profile lives in memory and is gone when
  the browser closes. `--storage-state <file>` seeds it with cookies and local
  storage saved earlier.
- `--isolated` and `--user-data-dir` exclude each other; Playwright refuses
  the combination.

**Your own Chrome** (`--extension`) attaches to the browser you are already
using, through the
[Playwright Extension](https://chromewebstore.google.com/detail/playwright-extension/mmlmfjhmonkocbjadbfplnigmagldckm)
(earlier listed as *Playwright MCP Bridge*; same extension). The agent can
then work on sites where you are signed in, without handing it passwords. On
the agent's first browser call the extension opens a connect page in Chrome,
and keeps the tabs each MCP client may use in a tab group of its own, so the
agent sees only the tabs in its group. Without a token you approve every
connection in that page; with the token from the extension's status page
(`PLAYWRIGHT_MCP_EXTENSION_TOKEN`) it connects without asking.

!!! warning "What the agent sees in your browser"
    In extension mode the agent acts as you on every site you are signed in
    to. Tools such as `browser_network_request` return request headers and
    bodies, which can include cookies and tokens, and every page the agent
    reads is untrusted text that may try to instruct it. Keep `ask` on the
    tools that act (see [Permissions](#permissions)), drag only the tabs the
    task needs into the agent's tab group, and prefer a launched browser for
    anything that does not need your logins.

Playwright MCP starts or attaches to the browser at the first browser tool
call, not when cookie connects to the server: a server shows `connected` in
`/mcp` even while Chrome is closed or missing, and browser problems surface as
a failed tool call.

## Where to run it

An MCP server runs on the machine of the [runtime](../runtimes/index.md) that
hosts it (see [where servers run](mcp.md#where-servers-run)), and the browser
runs where the server runs. Put it where the browser belongs:

| Configured as | Runs | Use it for |
|---|---|---|
| `[mcp.servers.<name>]` | on the daemon's machine, in the global runtime; every session that attaches `global` (the default) gets it | a headless browser next to a daemon on a server or your own desktop |
| `[runtimes.<id>.mcp.servers.<name>]` | on the machine of the runtime template `<id>`, which the daemon starts | a browser on a host the daemon reaches over ssh |
| `[runtimes.<id>.mcp.servers.<name>]` without a `command` for the runtime | on the machine that dials in as `<id>`, in that runtime | your desktop's Chrome, while the daemon runs elsewhere |

In the two runtime forms, give the server `scope = "runtime"`. A browser
belongs to the machine, not to a workspace: the runtime starts one copy when
it connects, in its user's home directory, and every workspace of that
runtime shares it (see
[scope](mcp.md#scope-one-per-workspace-or-one-per-runtime)). With the default
`scope = "workspace"`, every workspace a session opens on that runtime would
start another server and another browser, each with its own profile.
Top-level `[mcp.servers]` entries take no `scope`; they already run once.

**The browser must run where its display and Chrome are.** A headless browser
can run anywhere. A headed browser needs a desktop session, and extension mode
needs the very Chrome you use: the server starts Chrome from its local
install, opens the extension's connect page in it, and talks to the extension
over a loopback connection. So when the daemon runs on a Linux server and you
work on a Windows desktop, a server under the daemon's `[mcp.servers]` would
look for Chrome on the server. Run it on the desktop instead, in a runtime
that dials in from there:

```text
Windows desktop                                  Linux server
  Chrome + Playwright Extension                    cookie daemon
    ^ loopback                                       sessions, agents, permissions
  npx @playwright/mcp --extension                    ^
    ^ stdio                                          |
  cookie runtime serve --daemon ... --id winbox -----+  (WebSocket to /runtime)
```

The runtime has to run as you, inside your login session, or it cannot reach
your Chrome: on Windows a Task Scheduler task at logon, not a Windows service
(see [Running a runtime as a service](../runtimes/service.md#windows)).

**One browser per runtime, shared by its sessions.** A runtime-scoped server
is one MCP client of Playwright, so every session using that runtime drives
the same browser. Two sessions browsing at the same time can switch each
other's tabs; run them one after the other, or configure a second server with
its own name and profile.

## Permissions

A Playwright tool is published as `<server>_<tool>`: with the server named
`my_chrome`, `browser_navigate` becomes `my_chrome_browser_navigate` (see
[tool names](mcp.md#tool-names-and-permissions)). Name the server for what it
is (`my_chrome`, `playwright`), not `browser`, or the tools read
`browser_browser_click`. Give each browser server a name no other runtime's
server uses: when two attached runtimes publish the same tool name, both are
prefixed with their attachment, and rules written for the bare names stop
matching.

Each tool is offered by its own rules: an agent sees a browser tool only when
its merged rules (agent document, defaults, session overlay) do not deny `mcp`
for that tool's name. An agent document with no `mcp` key gets the default
`mcp: {"*": ask}`, so the browser tools are offered and every call asks (see
[MCP default](agents.md#mcp-default)). A declared `mcp` key replaces that
default entirely, so keep a `"*"` rule if the agent should still see other
servers' tools:

```yaml
permissions:
  mcp:
    "*": ask                                # other servers: offered, each call asks
    "my_chrome_*": allow                    # browse without asking...
    my_chrome_browser_evaluate: ask         # ...but ask before page JavaScript,
    my_chrome_browser_file_upload: ask      # handing local files to a page,
    my_chrome_browser_fill_form: ask        # submitting forms,
    my_chrome_browser_handle_dialog: ask    # and answering dialogs
```

The most specific rule wins, so the exact names override `"my_chrome_*"`. An
agent that should only read pages gets just those tools; nothing else of the
server is offered to it:

```yaml
permissions:
  mcp:
    my_chrome_browser_navigate: allow
    my_chrome_browser_snapshot: allow
    my_chrome_browser_find: allow
    my_chrome_browser_take_screenshot: allow
```

`mcp@winbox` keys limit rules to one runtime's tools (see
[runtime selectors](agents.md#runtime-selectors)), and `mcp: deny` in
`/permissions` hides every MCP tool for one session.

### Trimming the server's tools

Permissions choose among the published tools per agent; `include_tools` and
`exclude_tools` decide what is published at all, for every agent. They match
the server's own tool names (`browser_click`), not the published ones (see
[choosing a server's tools](mcp.md#choosing-a-servers-tools)).

Exclude `browser_run_code_unsafe` unless you need it. It is one of
Playwright MCP's default tools, and in its own words it "executes arbitrary
JavaScript in the Playwright server process and is RCE-equivalent": a call
runs code with the runtime user's rights, outside the page.

```toml
[mcp.servers.playwright]
command = "npx"
args = ["-y", "@playwright/mcp@latest", "--headless"]
exclude_tools = ["browser_run_code_unsafe"]
# or publish a fixed set:
# include_tools = ["browser_navigate", "browser_navigate_back", "browser_snapshot", "browser_find", "browser_click", "browser_type", "browser_tabs"]
```

The default tools of `@playwright/mcp` 0.0.83 are `browser_click`,
`browser_close`, `browser_console_messages`, `browser_drag`, `browser_drop`,
`browser_emulate_media`, `browser_evaluate`, `browser_file_upload`,
`browser_fill_form`, `browser_find`, `browser_handle_dialog`, `browser_hover`,
`browser_navigate`, `browser_navigate_back`, `browser_network_request`,
`browser_network_requests`, `browser_press_key`, `browser_resize`,
`browser_run_code_unsafe`, `browser_select_option`, `browser_snapshot`,
`browser_tabs`, `browser_take_screenshot`, `browser_type`, and
`browser_wait_for`. `--caps` adds groups: `vision` (coordinate mouse tools
such as `browser_mouse_click_xy`), `pdf` (`browser_pdf_save`), `devtools`
(tracing, video, highlighting), and, listed in the README's tool sections,
`network`, `storage`, `testing`, and `config`.

## Windows: your own Chrome, daemon elsewhere

The daemon runs on another machine (a Linux server, say, reachable at
`wss://host.example.ts.net/ws`; see [Remote access](../daemon/remote-access.md)).
The Windows desktop dials in as the runtime `winbox` and hosts the browser
server.

1. **Install Node.js 18 or newer** (the current LTS is fine), for example with
   `winget install OpenJS.NodeJS.LTS`, then open a new PowerShell and check
   `node --version`.
2. **Install cookie** on the desktop (see [Install](../install.md)).
3. **Install the extension.** In Chrome (or Edge), add the
   [Playwright Extension](https://chromewebstore.google.com/detail/playwright-extension/mmlmfjhmonkocbjadbfplnigmagldckm),
   open its status page from the toolbar icon, and copy the
   `PLAYWRIGHT_MCP_EXTENSION_TOKEN` value. The token belongs to that Chrome
   profile.
4. **Configure the server** in the daemon's `config.toml`, for the runtime
   the desktop dials in as. The table has no `command`, since the daemon
   does not start the runtime; the server runs on the desktop:

    ```toml
    [runtimes.winbox.mcp.servers.my_chrome]
    command = "npx"
    args = ["-y", "@playwright/mcp@latest", "--extension"]
    env = { PLAYWRIGHT_MCP_EXTENSION_TOKEN = "<extension-token>" }
    scope = "runtime"
    exclude_tools = ["browser_run_code_unsafe"]
    timeout_ms = 90000
    ```

    - If the extension is installed in several Chrome profiles, it connects to
      the one used last; add `"--profile-dir-name", "Profile 2"` (the last part
      of *Profile Path* in `chrome://version`) to pick one, with that
      profile's token.
    - `timeout_ms` bounds the connection and each call. The first start
      downloads the package; pre-fetch it with
      `npx -y @playwright/mcp@latest --version` so the first connection does not
      run out of time.
    - The token sits in the daemon's `config.toml`, which reaches the desktop
      in the runtime's settings; keep that file to yourself.

5. **Try it in a terminal first**, with the daemon's token from `[server]
   token` in the daemon's `config.toml`:

    ```powershell
    cookie runtime serve --daemon wss://host.example.ts.net/ws --id winbox --token <token>
    ```

    In a second terminal, list the servers:

    ```powershell
    cookie mcp list --url wss://host.example.ts.net/ws --token <token>
    ```

    ```text
    my_chrome	24 tools	Connected	winbox
    ```

    The tool count depends on the Playwright MCP version and your filters. A
    `Failed` state means the server did not start; `/mcp` in the TUI shows
    why (see [Troubleshooting](#troubleshooting)).

6. **Keep it running.** Stop the terminal runtime (Ctrl-C) and register the
   logon task from
   [Running a runtime as a service](../runtimes/service.md#windows),
   with `--id winbox`. It runs in your session, so it reaches your Chrome.
   A change to the server's table reaches the connected runtime at once.

7. **Use it in a session.** The browser tools come with the runtime: attach
   `winbox/coding` in `/runtime` (`a`, then a workspace such as `~`), or let a
   root session delegate to `winbox` (see
   [Delegating to runtimes](../sessions/delegation.md)). Attaching the
   `coding` capability also gives the session `read`, `write`, and `bash` on
   the desktop; `cookie runtime serve --root <dir>` limits its workspaces, and
   `bash@winbox` and `write@winbox` rules limit the rest.

When you only need the browser while you are at the desktop, the TUI does
the same without a task: `cookie attach --url wss://host.example.ts.net/ws
--token <token>` on the desktop dials in as `client_<host name>` for as long as
it runs, hosting the `[runtimes.client_<host name>.mcp.servers]` of the
daemon's configuration (see
[A client's machine](../concepts/runtimes.md#a-clients-machine)).

## Linux and macOS: a headless browser next to the daemon

A browser for research and testing that never touches your own profile, on
the daemon's machine.

1. **Check Node.js** 18 or newer (`node --version`); install it from your
   package manager or [nodejs.org](https://nodejs.org) if needed.
2. **Install a browser for Playwright.** By default Playwright MCP uses Google
   Chrome (`--browser chrome`). On a server, Playwright's own Chromium build is
   simpler:

    ```sh
    npx -y @playwright/mcp@latest install-browser chromium
    ```

    `install-browser` runs Playwright's installer for the version the package
    ships with. `install-browser chrome` installs Google Chrome instead, and on
    Linux asks for administrator rights. A Linux machine missing the browser's
    system libraries fails at the first browser call with
    `Missing system dependencies ... Install them with: sudo npx playwright install-deps chromium`;
    run what it says.

3. **Configure the server** in `~/.cookie-agent/config.toml`:

    ```toml
    [mcp.servers.playwright]
    command = "npx"
    args = [
      "-y", "@playwright/mcp@latest",
      "--headless",
      "--browser", "chromium",
      "--isolated",
    ]
    exclude_tools = ["browser_run_code_unsafe"]
    timeout_ms = 90000
    ```

    - `--headless`: on Linux, Playwright already runs headless when `DISPLAY`
      is unset, as under a systemd user service; elsewhere it is headed by
      default. To watch the browser on a Linux desktop from a service, drop
      `--headless` and add `env = { DISPLAY = ":0" }`.
    - `--isolated`: a fresh, in-memory profile each time the browser starts.
      For a profile that keeps logins, replace it with
      `"--user-data-dir", "/home/ada/.cache/cookie-browser"`, sign in once, and
      keep `--headless` off for that first sign-in.
    - `--output-dir <dir>` sets where screenshots without an explicit file name
      go; without it, `.playwright-mcp` in the server's working directory (the
      daemon user's home here).
    - `@latest` checks for a newer release when the server starts; pin a
      version (`@playwright/mcp@0.0.83`) to upgrade on your own schedule.
    - On Linux, Playwright already starts its own Chromium build without
      Chrome's sandbox. Google Chrome (`--browser chrome`) under `root`, as in
      many containers, needs `--no-sandbox`.

4. **Apply and check.** `cookie config reload` applies `[mcp]` changes at
   once (see [changes while the daemon runs](../engine/configuration.md#changes-while-the-daemon-runs)),
   then:

    ```sh
    cookie mcp list
    ```

    ```text
    playwright	24 tools	Connected	global
    ```

Every session that attaches `global` now gets `playwright_browser_*` tools,
subject to its agent's `mcp` rules. To run the browser on another machine the
daemon starts over ssh, put the same table under
`[runtimes.<id>.mcp.servers.playwright]` with `scope = "runtime"`.

## ACP agents

[ACP agents](acp-agents.md) such as Claude Code get
cookie's tools through cookie's MCP bridge: one MCP server named `cookie`
carrying the session's tools as cookie composes them for a model, the browser
tools of every attached runtime included. Claude Code lists them as
`mcp__cookie__my_chrome_browser_navigate` and so on.

- **`acp.hide_tools`** in the agent's [document](agents.md#acp-agent-documents)
  removes tools from the bridge. Its default hides only cookie's coding tools
  (`read`, `write`, `edit`, `bash`, `webfetch`), so the browser tools pass
  through; add `"my_chrome_*"` to keep them from an agent that has a browser of
  its own.
- **Permissions** work as for a model: a tool the merged rules hide is not
  listed, and each call is decided by the agent document's `mcp` rules.
- When an agent asks permission for one of cookie's own tools (Claude Code's
  `mcp__cookie__<tool>`), cookie allows the request at once: the call itself
  reaches cookie's MCP server, where the `mcp` rules decide it, so you are
  asked at most once. See [ACP agents](acp-agents.md#permissions).

## Troubleshooting

`/mcp` in the TUI shows each server's state and, for a failed one, the error;
`cookie mcp list` shows the state alone.

**The server fails to start.**

- *Node.js missing or not on `PATH`*: the error names the command that could
  not be started. Services start with a smaller environment than your shell:
  a systemd user unit needs `Environment=PATH=...` with Node's directory, and
  a version manager such as nvm installs Node where services do not look. An
  absolute `command` (`/usr/bin/npx`) avoids the question.
- *`connect timed out` on the first start*: `npx` was still downloading the
  package. Pre-fetch it (`npx -y @playwright/mcp@latest --version`, as the
  user the daemon or runtime runs as) or raise `timeout_ms`.

**The server connects, but browser calls fail.** The browser starts at the
first call, so its errors come back as tool errors:

- `Browser "chrome" is not installed ... Run npx @playwright/mcp install-browser chrome`:
  install it, or switch to `--browser chromium` and install that.
- `Missing system dependencies required to run browser ...`: run the
  `install-deps` command it prints.
- `Browser is already in use for <dir>, use --isolated`: two servers (or a
  server and a browser you opened) share one profile. Give each its own
  `--user-data-dir`, or use `--isolated`.
- Screenshots need a model that accepts images; for others a
  `browser_take_screenshot` call fails like any
  [gated media](../reference/tools.md#media-reads). `browser_snapshot` is
  text and works with every model; `--image-responses omit` stops Playwright
  attaching images at all.

**Protocol versions.** cookie first offers the newest MCP version and falls
back to the 2025-11-25 `initialize` handshake for a server that does not know
it: one that answers "method not found", or, over Streamable HTTP, refuses
the version with HTTP 400, as servers built on the MCP TypeScript SDK
(Playwright MCP among them) can. That covers Playwright MCP over stdio and
also run by hand as an HTTP server (`npx @playwright/mcp@latest --port 8931`,
with `url = "http://localhost:8931/mcp"` in cookie). Nothing needs
configuring. If `/mcp` still shows a protocol error, update Playwright MCP
(or drop a pinned version) and cookie.

**Extension mode does not connect.**

- `Playwright extension did not connect within 30s after opening the connect page. Make sure the extension is installed in the Chrome profile and PLAYWRIGHT_MCP_EXTENSION_TOKEN matches its token.`:
  the token is wrong, or belongs to another Chrome profile. Copy it again from
  the status page of the profile you mean, and add `--profile-dir-name` when
  you have several.
- Without a token, the first call waits for you to approve the connection in
  Chrome, and cookie ends it after `timeout_ms`. Set the token.
- Nothing opens at all: the runtime is not in your login session (a Windows
  service, an ssh session), or the daemon's own `[mcp.servers]` entry is
  looking for Chrome on the daemon's machine. Run the server in a runtime
  on the desktop, as above.

**Files land in odd places.** Playwright MCP takes its working directory as
its workspace root: the default profile's hash, `.playwright-mcp` for output
files, and the directories `browser_file_upload` may read from all follow it.
cookie starts it in the directory its
[working directory table](mcp.md#working-directory) gives: the daemon user's
home for `[mcp.servers]`, the runtime user's home for `scope = "runtime"`, the
workspace for `scope = "workspace"` (so a `.playwright-mcp` folder appears in
your repository). Set `cwd` to a dedicated directory, or `--output-dir`, to
keep both elsewhere, and a narrow `cwd` also narrows what uploads can read.

**The tools do not show up for an agent.**

- `cookie mcp list` or `/mcp` shows the server `Connected` with fewer tools
  than you expect: `include_tools` and `exclude_tools` filtered them (`/mcp`
  says how many, as `12 tools, 13 filtered`).
- The agent document declares an `mcp` key without a rule offering
  `my_chrome_*`, or only a `mcp@<other runtime>` key: a declared key replaces
  the default `"*": ask`. `/permissions` shows the merged rules.
- The session does not attach the runtime that hosts the server: runtime
  servers reach a session only through an attachment of their runtime
  (`/runtime`), and top-level servers only with `global` attached.
- The runtime is offline: a dial-in runtime's tools leave the session while
  it is away, and come back when it reconnects.
- The tool names are prefixed (`winbox_my_chrome_browser_click`) because
  another attached runtime has a server of the same name; rename one.

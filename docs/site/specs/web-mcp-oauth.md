# MCP OAuth from the web client

Status: **approved** (2026-10-21); queued after
[delegating to runtimes](runtime-delegation.md). It closes the one parity
gap the [web client spec](web-client.md) left open ("MCP OAuth, whose
redirect must reach a loopback listener").

## 1. Goal and scope

The protocol already splits OAuth between client and daemon:

- the daemon holds the PKCE verifier and exchanges the code;
- the client gives `mcp.auth.begin` a `redirect_uri`, and hands the URL
  the browser lands on to `mcp.auth.complete`.

The TUI binds a loopback listener for this (`OAuthCallbackListener`). The
web client cannot, and the daemon accepts only loopback redirects, so the
web client has no sign-in for MCP servers.

This spec lets the web client use its own page as the redirect. It
covers both the daemon serving the page and the page hosted anywhere
else.

Out of scope: model-provider sign-ins. Providers whose client IDs are
fixed to a `localhost` redirect need a device-code or paste-back flow of
their own.

## 2. Daemon

- **Redirect URIs.** `mcp.auth.begin` accepts, besides today's loopback
  form:
  - `https://` URIs, with a host, no user info and no fragment; any path
    and query are allowed;
  - `http://` to a loopback IP literal or `localhost`, with any path and
    query, since the page may itself be served from loopback.

  Everything else is refused as today, with a message naming the accepted
  forms.
- **Safety.** Only a client holding the token can call `begin`, and the
  code is single-use. The verifier never leaves the daemon, so a code
  sent to an unexpected URI cannot be exchanged by anyone else. The
  `state` check in `mcp.auth.complete` is unchanged.
- **Client registration.**
  - With dynamic client registration, the daemon registers the redirect
    the flow uses.
  - When a stored dynamic registration was made for a different redirect,
    the daemon registers again instead of reusing it.
  - A configured `client_id` is used as is. If its server refuses the
    redirect, the error reaches the client unchanged, and the docs explain
    that the redirect must be allowed on the server's side.
- **Protocol.** Accepting more values is additive, but older daemons
  refuse them, so this is a client protocol minor bump. The web client
  offers sign-in only from that minor on.

## 3. Web client

- **The redirect is the page itself.** The URI is the page's own URL with
  `?oauth=callback`, for example
  `https://yunhao-labserver.tail34018a.ts.net/?oauth=callback`. The OAuth
  server appends `code` and `state`.
- **Starting.** The MCP server's "Sign in" button:
  1. calls `mcp.auth.begin`;
  2. records the pending server in `sessionStorage` and `localStorage`
     (`cookie.oauth_pending`), keyed by `state` with a 10-minute expiry;
  3. opens `authorization_url` in a new tab.

  If the browser blocks the new tab, it navigates the current tab
  instead.
- **Finishing.** When the page loads with `oauth=callback`:
  1. it removes `code` and `state` from the address bar at once
     (`history.replaceState`);
  2. it connects as usual;
  3. it calls `mcp.auth.complete` for the pending server with the full
     callback URL;
  4. it shows the result, "Signed in to `<server>`" or the daemon's
     error.

  If another tab of the page started the flow, the callback tab passes
  the URL to that tab over a `BroadcastChannel` (`cookie-oauth`) and
  closes itself. The original tab completes the flow and refreshes the
  MCP list. With no other tab listening, the callback tab completes the
  flow itself.
- **Paste fallback.** While a sign-in is pending, the MCP sheet shows
  "Paste the address you landed on". This covers the system browser on a
  phone, which on iOS does not share storage with the installed app, and
  any other case where the callback lands somewhere the flow cannot
  continue from. Pasting calls `mcp.auth.complete` with the pasted URL.
- **Cancel** calls `mcp.auth.cancel` and clears the pending entry.
- **HTTPS note.** When the page is on plain `http` at an address that is
  not loopback, the sheet warns that most OAuth servers refuse such
  redirects. It suggests `tailscale serve` or another HTTPS front, and
  links to the remote-access guide. The button stays available, since
  some servers allow it.

## 4. Tests

- **Daemon:** redirect validation cases. Accepted: `https` with a path and
  query, loopback `http`, and `localhost`. Refused: `https` with user info
  or a fragment, a non-loopback `http`, and other schemes. Also a
  dynamic registration redone for a new redirect, and an end-to-end flow
  against the existing mock OAuth server with an `https`-style redirect.
- **Web, browser suite:** against a mock OAuth server in the scenario
  daemon's config, three flows:
  - sign in through a new tab with the `BroadcastChannel` handoff;
  - sign in in the same tab;
  - the paste fallback.

  Also check that the address bar is cleaned and that a stale or
  mismatched `state` is refused.

## 5. Docs

- **The web guide:** signing in to MCP servers.
- **The MCP page:** web sign-in, the redirect a configured client must
  allow, and HTTPS.
- **The protocol changelog:** the minor bump.
- **The web-client spec:** the parity line updated.

## 6. Open questions

None.

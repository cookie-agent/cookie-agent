# Forwards

A forward makes something one machine reaches available elsewhere, through
the daemon:

- **A web forward** serves a machine's web app, such as a dev server on
  `127.0.0.1:5173` of a lab machine, at the daemon's own address, below
  `/p/<handle>/`. You open it from the web page, a reply shows it live, and a
  share link lets someone else open it.
- **A TCP or UDP forward** listens on a port of one machine and carries each
  connection, or each datagram, to an address another machine reaches, such
  as a database on `127.0.0.1:5432` of another machine.

The daemon relays every byte over the connections it already has to each
machine's runtime, so neither machine needs to reach the other, and nothing
listens on a public address unless you bind it there. Agents make forwards
with the forward tools, you make them from the Forwards view of the web
page or with `cookie forward`, and the daemon keeps them in `forwards.json`
in its data directory. The [forwards spec](../specs/forwards.md) has the full
design.

Forwards need a daemon of client protocol 1.109 or later and runtimes of
runtime protocol 1.19 or later; a machine whose runtimes are older cannot
take part.

## A forward

```sh
cookie forward http labpc:5173 --title "Vite dev server"
cookie forward tcp labserver:127.0.0.1:15432 labpc:5432
cookie forward udp labserver:0 labpc:127.0.0.1:9999
```

Each prints the forward: its handle (`fw_` and eight hex digits), kind,
route, status, where it is reached, and when it ends. An endpoint is
`<machine>:<port>`, or `<machine>:<host>:<port>` for a host other than
`127.0.0.1`. The target is reached from its machine, so its host may be
anything that machine reaches, another host on its network included. A
listening port of 0 picks a free one.

A forward lives 24 hours unless you name another expiry: `1h`, `24h`, `7d`,
or `never`. `cookie forward expires <handle> <expiry>` counts a new one from
now, and `cookie forward delete <handle>` stops it at once. A forward an
agent made belongs to its session and goes when the session is deleted; one
you made belongs to no session.

### Status

| Status | Means |
|--------|-------|
| `active` | It is listening, or for a web forward, serving. |
| `waiting` | A machine it needs is not connected. It starts when it is. |
| `failed` | Listening failed, such as on a port in use. The reason says why, and the daemon tries again every 30 seconds. |

A forward also counts its open and total connections and the bytes each way,
and keeps its latest 50 connections with where they came from.

## Web forwards

A web forward's address is `<daemon page>/p/<handle>/`. It opens from the web
page, which holds the daemon's token, so a browser that has not opened the
page is asked to open it first. The daemon passes requests and WebSocket
connections to the app with their path below the prefix, and tells the app
where it is with `X-Forwarded-Prefix`, `X-Forwarded-Host`, and
`X-Forwarded-Proto`. A redirect to a path from the root, and a cookie for the
whole site, get the prefix added. An app that loads its scripts and styles
from absolute paths needs its base path set to the prefix, such as Vite's
`--base /p/<handle>/`.

!!! warning "A forwarded app is trusted like a plugin page"
    The app runs on the web page's own origin, unsandboxed, so its scripts
    can read the daemon's token. Forward only apps you trust.

### Share links

```sh
cookie forward share fw_3f9a1c2e --expires 7d
```

prints a public link, `<public address>/s/<id>/`, which serves the app at the
link's root. Anyone with it reaches the app, whether or not the app has a
login of its own, until the link expires, you revoke it with
`cookie share revoke`, or the forward ends. Links are served on the
[public port](../engine/public.md) as other share links are. A guest of a
shared conversation also reaches that conversation's web forwards, through
its own share link.

## In the clients

- **Web:** the Forwards view lists every forward with its status, traffic,
  and expiry, and *Open* for a web app. A forward's page shows its address,
  changes its expiry, makes and revokes share links, lists its recent
  connections, and deletes it; *New forward* makes one. The session panel's
  Forwards tab lists the forwards the session's agents made, and the title
  bar counts the active ones. A reply can show a web forward live with
  `<attach forward="fw_…" path="/"/>`.
- **Terminal UI:** `/forwards` lists them, with Open, Delete, Change expiry,
  and Share.
- **Command line:** [`cookie forward`](../clients/cli.md#cookie-forward).

## Agents and permissions

The forward tools, `forward_create`, `forward_list`, and `forward_delete`,
are in the deferred `forwards` group; see the
[tool reference](../reference/tools.md#forward_create). Their permission
action is `forward`. By default a root session asks before it makes or
deletes a forward and lists them freely, and a subagent may not use them. A
listener on an address other than loopback is checked as the resource
`public <machine>:<bind>` as well, which no default rule allows, so the user
is always asked for it.

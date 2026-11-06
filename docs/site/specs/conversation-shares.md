# Conversation shares: guest links to a session

Status: **implemented** (2026-11-04; client protocol 1.88.0). The user's
decisions are in [section 13](#13-decisions-2026-11-04); how it landed is in
[section 15](#15-implementation-notes-2026-11-04). A mockup of the guest page,
the owner's view, and the share dialog is
[`conversation-shares/preview.html`](conversation-shares/preview.html). Builds
on [share links](share-links.md) (the share store, `/s/`, the share-only port,
expiry, the Sharing view) and the [web client](web-client.md).

## 1. Problem

Share links publish one asset. The user also wants to bring someone into a
conversation: let a colleague read what the agent did and carry on with
it, without giving them the daemon token, which opens every session,
setting, and machine.

The user presses **Share** on a session and gets a URL. Whoever opens it
gets that session in the web client as it is, live: the owner's own
transcript, composer, and session panel. They can do what the owner does
in the session, but not change how it is set up. Nothing else of the
daemon.

## 2. Goals and non-goals

**Goals**

- A **conversation share** is a public, unguessable URL for one session,
  in the same share store and `/s/<id>` space as asset links, with the
  same expiry, listing, and revoking.
- A **guest** who opens it sees **everything the transcript shows**, live,
  as the owner does: messages, tool calls and results, thinking, the
  system prompt and tools rows, figures, attachments, and the delegated
  sessions under it.
- A guest **uses the session as the owner does**: sends messages (with
  attachments), steers and queues, stops a run, **answers approvals and
  elicitations**, **compacts**, **sets goals**, **runs skills**, stops
  background calls, and **reverts** to an earlier message. Guests do not
  fork (decided 2026-11-04).
- A guest **sees the session panel** whole, read-only where it changes
  the setup ([section 6](#6-the-guest-page)).
- A guest **cannot change the session's setup**: permissions, runtime and
  machine bindings, tool exposure, model, agent, ACP agent options, or
  project. Nor can they reach any other session or daemon state.
- The owner sees which messages a guest sent, and who is connected.
- Revoking a link closes its guests' connections at once.

**Non-goals (this round)**

- Guest accounts, sign-in, or per-guest identities beyond the link's label
  ([section 7](#7-attribution-and-presence)).
- Guests renaming or deleting sessions.
- A read-only static export.
- Spending limits per link (decided 2026-11-04: none).

## 3. Conversation shares

A record in `shares.json` beside the asset links; the records gain a
`target` of `asset` or `session`.

| Field | Meaning |
|---|---|
| `id` | 128 random bits, base64url, as for asset links. The URL is the capability. |
| `session` | The shared root session. Its delegated sessions come with it, read-only. |
| `label` | Who the link is for, such as "Alice"; "Guest" unless the owner names one. Messages sent through the link carry it. |
| `can_send` | Whether guests may act (send, answer, stop, compact, goal, skills, revert). Off makes a watch-only link. On by default. |
| `created`, `expires_at`, `views`, `last_viewed` | As for asset links: 7 days unless the owner picks *1 day*, *30 days*, or *Never*. `views` counts guest connections. |

```text
<share base>/s/<id>          the guest page
<share base>/s/<id>/ws       the guest connection
<share base>/s/_app/<file>   the web bundle
```

Same base, warnings, and share-only port as asset links
([share links](share-links.md) section 6). The share-only port now also
serves `/s/<id>/ws` and `/s/_app/`, and still nothing outside `/s/`.

**Lifecycle.** Deleting the session's tree or its project removes the
link (`deleted_shares`). Moving it to another project, compacting,
reverting, or forking the session leaves the link on the same session;
a fork the owner makes is not shared.
Expiry or revocation ends the link: the page answers the uniform 404 and
open guest connections close with "This link isn't available".

## 4. What a guest can do

The guest connection allows a fixed list of client protocol methods,
each scoped to the shared tree. Every other method is refused with
`not_allowed_for_guest`, new methods included, until someone adds them.

| | Allowed |
|---|---|
| **Read** (the shared session and its delegated sessions) | Everything the session view and its panel read: events and history, cut results whole, the assets and artifacts the tree's events show, the session's meta, usage, permission rules and mode, attached runtimes, tool groups and skills, goal, background calls, and pending approvals |
| **Act** (the shared session only, when `can_send`) | Answering approvals and elicitations, `session.revert` at every cut point, `session.submit_input` with text and attachments (uploads under the daemon's usual limits), steering and queueing, recalling and dropping queued inputs, stopping the current run and background calls, `session.compact` and cancelling it, `session.goal.set` and the goal's lifecycle, reading and running skills (`skills.get`, sent as input), and the session's compact and goal commands for an ACP agent |
| **Refused** | `session.select_model`, `session.set_permission_mode` and rule changes, `session.tools.set`, `session.runtime.attach`/`detach`, `session.agent_option.set`, `session.fork` and *Edit in fork*, `session.project.set`, ACP agent commands other than compact and goal, rename, delete, pin and unread, and everything outside the session: other sessions, config, providers, runtimes, memory, projects, cron, assets lists, shares, plugins |

A turn a guest starts runs exactly as one the owner starts: the session's
agent, model, tools, machines, and permission rules, with no budget of its
own. Approvals go to every client on the session, guests included, and
the first answer wins, as between the owner's own clients (see
[section 10](#10-security-notes)).

## 5. The guest connection

`/s/<id>/ws` speaks the client protocol, so the web client talks to it as
it talks to `/ws`, with three differences:

- **The link is the credential.** The connection takes no token or page
  ticket, and is accepted from any `Origin`, `null` included
  ([section 6](#6-the-guest-page)). The daemon looks the link up when the
  socket opens and again at every request, so a revoked or expired link
  stops working mid-connection.
- **Requests are checked before dispatch** against the list in section 4
  and the shared tree. A request about any other session answers as if it
  did not exist.
- **Notifications are filtered** the same way, fail closed: the guest gets
  the events and changes of its tree, and none of the daemon's broadcasts
  (other sessions in `sessions.changed`, config, runtimes, shares, usage
  across sessions).

The **hello** a guest gets carries a guest marker, the shared session's
ID, `can_send`, and the link's expiry, and none of the daemon's
configuration, providers, runtimes, or agents. The web client reads the
marker to enter guest mode.

## 6. The guest page

The web client's own bundle, in **guest mode**: the owner's session view
as it is, with no guest-only views (decided 2026-11-04).

- **The same as the owner's:** the transcript with everything it shows,
  the composer with attachments, steer and queue, and *Stop*, the session
  panel with every tab, the commands sheet, and the sheets the transcript
  opens: approvals, a message's actions (copy and revert; fork and *Edit
  in fork* are left out), and the agent's questions.
- **Usable:** compact (the sheet and `/compact`), the goal tab and
  `/goal`, running a skill from the skills tab, stopping a background
  call, and everything in the transcript and composer.
- **Read-only:** whatever changes the setup shows its current value and
  is disabled, with "Only the owner can change this": the permission mode
  and rules, attaching and detaching runtimes, the tool group and skill
  switches, the bottom bar's agent, model, and mode pickers, ACP agent
  options, and the project.
- **Not there:** the sidebar and every other view, rename and delete.
- **A top bar** with the cookie mark, the title, "shared with you as
  Alice", the expiry, and who is connected.

**How the code is reused.** The web client already gates a read-only
session with one check, `Workspace::writable()`, at 18 places. Guest mode
splits it in two: `writable()` stays for acting (sending, compacting, the
goal, running skills, attachments), and a new `configurable()` gates the
setup (permission mode and rules, ACP options, which use `writable()`
today) and the setup controls that have no gate yet (runtime attach, the
tool group and skill switches, the model and agent pickers, and fork
and *Edit in fork* in a message's menu). A guest is
writable and not configurable; a watch-only guest is neither. The daemon's
method list (section 4) refuses the same things whatever the page does.
- **Watch-only links** (`can_send` off) show no composer.
- **Link previews:** the page carries `og:title` (the session's title) and
  "A conversation shared from cookie agent".

**Its sandbox.** On the main listener the page shares the daemon's origin,
where the owner's browser keeps the token. So `/s/<id>` carries a CSP
`sandbox allow-scripts allow-popups` header: the page runs in an opaque
origin and cannot read the token, the page ticket cookie, or
`localStorage`, nor open `/ws` as the daemon's page. Its socket to
`/s/<id>/ws` comes from origin `null`, which that path accepts, and
`/s/_app/` answers with `Access-Control-Allow-Origin: *` so the bundle
loads. Guest mode keeps no state in storage, which an opaque origin does
not have. Whether the bundle boots under the sandbox is phase 1's first
check; if it cannot, guest pages are served only on the share-only port.

## 7. Attribution and presence

- **Messages sent through a link** carry the label in their event's origin
  ([section 15](#15-implementation-notes-2026-11-04)). The owner's clients show
  them with the label ("Alice · via link"), and so do guests'. The model
  gets the message exactly as if the owner had sent it: the label is for
  people, not for the agent (decided 2026-11-04).
- **Presence:** the owner's session view shows the guests connected now,
  by label, beside its other clients, and the Sharing view shows each
  link's connected count.
- **Several guests on one link** all appear under its label. To tell
  people apart, the owner makes one link each.

## 8. Making and revoking

**Web client.**

- **Share** in the session header's menu, and in the *⋯* and right-click
  menus of its row in the session list, opens the **share panel**: the
  private-base warning when it applies, the warning of
  [section 10](#10-security-notes) in one line, and a card for every link of
  the conversation, newest first (the label, who is connected, *Can send*, the
  URL with *Copy*, the expiry and views, *Change expiry…*, and *Revoke*, which
  asks first). *New link* opens a form in the panel (the label, *Can send
  messages*, the expiry, and *Create link*), and a conversation with no link
  opens on that form. With two or more links, *Revoke all* ends this
  conversation's links, and no others, and asks first (decided 2026-11-04).
- A session with a live link shows the link badge in its header and
  sidebar row.
- **The Sharing view** lists conversation shares beside asset links: the
  session, label, `can_send`, expiry, connected guests, and last view.

**CLI.**

```text
cookie share session <session id> [--label <name>] [--watch-only]
                     [--expires 1d|7d|30d|never]
```

`cookie share list` and `cookie share revoke` cover both kinds.

**TUI** (decided 2026-11-04): `/share` makes a link with the defaults
(label "Guest", can send, 7 days) and prints its URL, and `/share
<label>` names it. Guests' messages show their label ("Alice · via link"),
and connected guests show beside the session's other clients.

**Agents:** no tool to share a session.

## 9. Protocol and compatibility

**Client protocol, minor:**

- `SessionShare { id, path, url?, session_id, title?, label, can_send,
  created, expires_at?, views, last_viewed?, connected }`;
- `SharesListResult` gains `sessions: Vec<SessionShare>` (empty when
  absent) and `SharesListParams` gains `session?`, so older clients list
  asset links as before;
- `session_share.create { session_id, label?, can_send, expiry }` and
  `session_share.update { id, label?, can_send?, expiry? }`;
- `share.revoke`, `shares.revoke_all`, and `shares.changed` cover both
  kinds, in one ID space;
- the hello's guest marker ([section 5](#5-the-guest-connection)), and
  the error `not_allowed_for_guest`;
- `session.delete` and `project.delete` count both kinds in
  `deleted_shares`.

**History:** no change: a guest's label is in the origin events already carry.

**Configuration:** none new. `[share] enabled` turns both kinds off.

**Plugin and runtime protocols:** no change.

## 10. Security notes

- **A guest is the owner, inside one session.** A guest's turn has the
  session's tools, machines, and permission rules, and the guest answers
  its approvals. So whatever the session's agent can do on any attached
  machine, a guest can: read `~/.ssh` or cookie's `config.toml` with its
  provider keys, run commands, write files. A guest cannot widen the
  rules, change the setup, or reach another session. The dialog says this
  in one line, and the docs in full. Hand a link out as you would a shell
  on those machines, and make it watch-only otherwise.
- **A guest spends the owner's provider budget,** without a limit
  (decided 2026-11-04). Expiry and revoking are the controls.
- **A link is a bearer secret** that chat apps fetch to preview. It opens
  a connection only when the bundle runs, so an unfurler counts a view but
  sends nothing.
- **The method and notification lists fail closed:** a method or
  broadcast added later is not reachable by guests until it is listed.
- **The control plane stays private** behind the share-only port: it
  serves the guest socket, whose dispatcher refuses everything outside the
  list, and no other path.
- **The guest page cannot act as the daemon's page** (section 6).

## 11. Tests

Expiry values in tests are explicit, never the default.

- The method list: every allowed method works on the shared session; each
  refused one answers `not_allowed_for_guest`, including a method added to
  the protocol after the list (a test enumerates the method registry so a
  new method must be classified).
- Scope: a request naming another session, or acting on a delegated one,
  answers as not found. Notifications of other sessions never arrive.
- The hello holds no config, provider, runtime, or agent data.
- A guest's revert works; a guest's fork is refused.
- Watch-only links refuse every act.
- Revoking or expiring a link closes its open connections and makes the
  next request fail.
- A guest's origin carries its label, and the owner's view shows it.
- A guest answers an approval and an elicitation; on a watch-only link
  the answer is refused.
- Browser suite: the guest page boots under the sandbox, sends a message,
  answers an approval, compacts, sets a goal, runs a skill, sees the
  permissions and runtimes tabs with their controls disabled, and cannot
  read the daemon origin's
  `localStorage` or `document.cookie`.

## 12. Phases

1. **Spike:** the bundle in guest mode under the CSP sandbox, with a
   socket from origin `null` (section 6).
2. **Daemon:** the record's `target`, the guest socket with its method and
   notification lists, the guest hello, `via_share`, presence, the
   protocol methods, and lifecycle.
3. **Web client:** guest mode, the share dialog, badges, presence, and
   the Sharing view's rows.
4. **CLI and TUI.**
5. **Docs:** "Sharing" gains conversations with the warning of section 10;
   Remote access notes that the share port also serves guests.

## 13. Decisions (2026-11-04)

The user decided:

1. **Guests get the session as it is,** not a filtered snapshot: they see
   everything the session shows and act in it. They cannot change
   permissions, runtime bindings, tool exposure, models, or agents.
2. **Everything else is the same as for the owner:** guests answer
   approvals and elicitations, and their messages reach the model as the
   owner's do. No stricter mode for guests.
3. **No provider budget** per link.
4. **Guests can revert** too, but **not fork** (nor *Edit in fork*), to
   keep a link to one session. Rename and delete stay the owner's.
5. **Messages a guest sends show its link's label** in the clients, not to
   the model.
6. **The TUI** gets `/share` and shows guests' labels.
7. **Guests see the session panel** with every tab, read-only where it
   changes the setup, and can compact, set goals, and run skills (revised
   later on 2026-11-04, after a version without the panel). The owner's
   views are reused as they are, split by `writable()` and
   `configurable()`.
8. **Every link of a conversation shows in one panel** (2026-11-04, after a
   first version that showed only the newest): *Revoke all* stays in it and
   asks first, and cards stay fully open rather than collapsing.
9. **The session list's *⋯* menu offers Share** too, beside the right-click
   menu.

## 14. Alternatives considered

- **A read-only snapshot filtered by the daemon** (this spec's first
  draft): an allowlist of event kinds, tool-call levels, and a secret
  scan. Safer to hand out, but a guest could not continue the
  conversation, which the user wants.
- **A token scoped to one session** handed to a full web client instead
  of a guest socket. It would reuse `/ws` as it is, but every method would
  then need a scope check, where the guest socket checks one list at its
  door.
- **Plain `GET`s and polling** for the transcript. Simpler to proxy, but
  guests need the live stream and the composer's methods, which the client
  protocol already has.

## 15. Implementation notes (2026-11-04)

Implemented in client protocol 1.88.0. Where the code settled what the draft
left open, or differs:

- **A link opens a root session.** `session_share.create` refuses a delegated
  one with `shared_session_not_root`: its approvals, assets, and tree all hang
  from the root, so sharing part of a tree had no clear meaning.
- **The check is one guard in the connection loop**
  (`ConnectionGuard` in the transport, `crates/server/src/service/guest.rs`).
  It asks before a request reaches the server and before a queued frame
  reaches the client, and a test fails until every client method is either in
  its list or named as refused, and checks each listed method's params really
  have the member it scopes by.
- **Guests cannot pick an agent or model through a side door.** `run.start`
  takes a required `selection`, so it is refused; `session.submit_input`,
  `session.goal.set`, and `session.goal.lifecycle` are refused when they carry
  one, or `reset_fallback`. A guest's page leaves them out, so its runs use the
  session's selection.
- **Labels live in the origin, not a new event field.** Every event already
  carries its `origin`, through the queue too, so a guest's is
  `client:guest-<label in base 32>-<8 hex of its tab>`, which clients decode
  (`guest_label`). History did not change, a label outlives its link, and the
  link's secret is not written anywhere. It also keeps a label to 30 bytes.
- **Any change to a link closes its guests' connections.** A client whose link
  still works connects again and reads what it may do, and under what label,
  from the hello. So `can_send` and the label apply at once, in the page too.
- **A guest reads the model catalog** (`runtime.snapshot.get`, `runtime.changed`)
  so the agent and model show by name. That shows provider names and their
  non-secret setup values, such as a custom endpoint, and never a credential.
  `runtimes.list` is refused, so the Runtimes tab shows the session's
  attachments without their status, and absolute workspace paths. *(Since client
  protocol 1.89.0 a guest reads `session.runtimes.list`, so it has its session's
  machines, status, and homes: see
  [The machine on a tool call](tool-call-machines.md).)*
- **Assets:** a guest reads the tree's own assets, and a project's or the
  shared store's by ID, which it cannot list or guess, rather than only those
  the transcript shows.
- **The page reads its link's state from its own address.** A browser gives no
  reason for a socket that failed, so when one does the page fetches its own
  address: 404 shows "This link isn't available", and no answer keeps
  redialing, as for a daemon that restarts. So the page and the 404 answer any
  origin.
- **`/s/<id>/ws` goes through the `/s/` route**, since the router allows no
  more specific route beside its wildcard.
- **The web client has no `/` commands:** its compact, goal, and skills are in
  the ⋮ menu and the session panel. The guest's menu leaves out Rename, Share,
  Schedule, and Preset.
- **Not done:**
  - the top bar names the guest and the expiry, but not who else is connected,
    which a guest cannot read; the owner sees the count;
  - the link badge is in the title bar, not on sidebar rows;
  - `session.delete` reports no `deleted_shares`; its links go with it, and
    `project.delete` counts only links of the project's own assets;
  - a guest's message that starts with `/` still reaches an ACP agent as text,
    and that agent may treat it as a command. The menu offers a guest only
    compact and goal, but the daemon cannot tell a command from a message.

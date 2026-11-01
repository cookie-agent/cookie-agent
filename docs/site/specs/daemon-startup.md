# Daemon startup and status

Status: **approved** (2026-11-01), with the user's decisions in
[section 9](#9-decisions).

## 1. Problem

A daemon listens only once everything has started. `cookie daemon`
composes the runtime first (`compose` in
`crates/cookie_agent/src/cli/compose.rs`): it loads the catalog, starts the
`local` runtime, opens the session store (`SessionStore::open`, which
migrates and converts it), rehydrates memory, machines, projects, and
crons. `run_daemon` (`crates/cookie_agent/src/cli/daemon.rs`) binds the
port only after that.

Measured on yunhao-labserver at the 2026-11-01 restart, from the journal:

| Time | What happened |
| --- | --- |
| 11:05:50.24 | The old daemon says it is restarting and closes clients. |
| 11:05:51.88 | The new daemon starts moving 97 trees to layout 3 and converting them to format 2. |
| 11:05:59.85 | The migration is done (8.0 s). |
| 11:06:00.09 | The new daemon listens. |

For those 10 seconds, nothing listened on the port:

- The web page could not load at all, because the daemon serves it.
- Clients redialed with their back-off (500 ms, doubling to 10 s). The
  dials failed at 0.5, 1.5, 3.5 and 7.5 s, and the next one came at about
  15.5 s, so they reconnected about 5 s after the daemon was up.
- A client could not tell a daemon that is starting from one that is down
  or failed. If startup fails (for example, a store that a newer build laid
  out), that reason reaches only the journal.

A one-time migration is the worst case, but every start does this work. As
the store, plugins, and runtimes grow, it grows with them.

## 2. Goals

1. The daemon accepts connections as soon as it has its configuration and
   token, before any slow work.
2. Clients learn, as soon as they connect, that the daemon is starting, what
   it is doing, and how far along it is. They show that instead of a
   connection error.
3. A connection made during startup turns into a normal connection when the
   daemon is ready, with no reconnect.
4. Generic: anything the daemon does that a user would wonder about reports
   through one mechanism, during startup or later. Examples are a store
   migration, a long conversion, a slow runtime start, rebuilding an index,
   or waiting to restart. The daemon never branches on which task it is,
   and clients show any task the same way. This follows
   [no hard-coded kinds](runtime-capabilities.md).
5. A failed startup reaches the clients that are connected, with its reason.

Non-goals: making startup itself faster. That is separate work, and this
spec makes it visible.

## 3. Daemon status

The daemon has one **status**: a phase and a list of tasks.

```text
DaemonStatus {
  phase: "starting" | "ready" | "stopping" | "failed",  // open enum
  restart: bool,           // phase "stopping": a relaunch follows
  tasks: [DaemonTask],     // what the daemon is doing that a user would want to see
  error: Option<ErrorData> // phase "failed": the registry error and its message
}

DaemonTask {
  id: String,              // stable for the task's life, opaque
  label: String,           // for people: "Moving sessions to the new storage layout"
  detail: Option<String>,  // for people: "2026-10/0193…"
  progress: Option<{ done: u64, total: Option<u64>, unit: Option<String> }>,
  blocking: bool,          // the daemon is not ready until it ends
  started_at: Timestamp,
}
```

- **phase** answers one question: what the daemon accepts now.
  - `starting`: from the bind until every blocking task ends and the
    engine is open. Only the handshake and `daemon.status` work.
  - `ready`: everything works, whatever tasks are running.
  - `stopping`: the daemon is going down, for a stop signal or a restart
    that is due (`restart` tells them apart). New runs are refused, as
    `daemon_restarting` refuses them today, and connections close
    soon after.
  - `failed`: startup failed; `error` says why.
- **tasks** say what the daemon is busy with, in any phase. Only a
  blocking task holds the phase at `starting`; a non-blocking one changes
  nothing a client may send. A restart that waits for idle is one: the
  phase stays `ready`, runs keep starting in every session (and the
  restart waits for them too), and the task counts down the active runs
  and background calls. When the restart is due, the phase becomes
  `stopping` with `restart: true`.
- **tasks** are free-form. Producers name them, clients show `label`,
  `detail`, and a bar when `progress.total` is known (a spinner and the
  count otherwise). Nothing branches on `id` or `label`.
- A task that ends is removed. A failed blocking task fails startup, and its
  error is the status's `error`.
- Updates are coalesced: at most one notification every 250 ms per
  connection, plus one at every phase change and every task start or end.
  Progress counters are cheap atomics, so a migration can report each tree
  without slowing down.

### 3.1 Where it lives

The status lives in a small `StatusBoard` in a crate below the engine
(`cookie_agent_identity` or a new module of `engine_store`) that has no
dependencies of its own:

```rust
let task = board.start("Moving sessions to the new storage layout", Blocking::Yes);
task.set_total(97);
task.advance(1);            // per tree
task.detail("2026-10/0193…");
drop(task);                 // ends it
```

- The daemon creates the board before anything else. It hands it to
  `compose`, and from there to `SessionStore::open`, the runtime starts,
  and plugin loads. A component that has nothing slow to report never sees
  it.
- After startup, the engine keeps the board. A long maintenance pass, a
  restart waiting for runs, or a slow runtime reconnect can add a
  non-blocking task, and clients show it as a notice.

The first producers are the layout migration and the format-2 conversion,
one task each, with per-tree progress. `compose`'s own steps become one
task each: the catalog, the `local` runtime, opening the store,
rehydrating memory, machines, projects, and crons. A step that takes under
a second is never seen, because it ends before the first notification.

## 4. Startup order

`run_daemon` changes to:

1. Load the configuration and the token, as today.
2. **Bind** the port and print the `daemon-ready` line. Launchers already
   wait for that line, and it now means "connectable". The phase is
   `starting`.
3. Compose the runtime with the board. The `Server` and the `Engine` are
   built as today.
4. **Hand over:** the listener's router switches from the startup front end
   to the full server, and the phase becomes `ready`. `announce_started`
   (push) runs here, not at the bind.

If composing fails, the phase becomes `failed` with the error. The daemon
sends that status to every connected client and exits non-zero as soon as
each one has received it, waiting at most 5 seconds for slow ones. A
launcher's wait still sees the process exit.

### 4.1 The startup front end

From the bind until the handover, the listener serves:

- **The web page** (`GET /` and its assets). It is static and embedded, so
  it needs no engine.
- **`/ws`**:
  - The handshake works as usual, with the token check.
  - `ServerHello` gains `status: DaemonStatus`.
  - The connection then stays open. `daemon.status` (method and
    notification) works. Every other method is refused at once with the
    new registry error `daemon_starting` (retryable; `data.status` carries
    the status) instead of waiting.
  - At the handover, the same connection runs the server's normal
    `connected` path, and the client gets a `daemon.status` notification
    with phase `ready`. No reconnect.
- **`/runtime`** (dial-in runtimes): answers 503 with `Retry-After: 1`.
  Dial-in runtimes already retry. The daemon starts its own runtimes after
  the handover, as today.

The front end is a small part of `crates/server`, a router over a
`watch::Receiver<Option<Arc<Server>>>`, so the same `axum::serve` and TLS
listener run before and after the handover.

### 4.2 Older clients

A client whose hello names a protocol older than this one has its
handshake answer **held** until the handover, then answered as today. This
is how it behaves now, with the connection made earlier. If the hello
arrives while the phase is `failed`, the connection is closed with the
error.

## 5. Protocol

Client protocol minor bump (1.65.0, or whatever is next):

| Member | What it is |
| --- | --- |
| `ServerHello.status` | The status when the hello is answered. Absent from older daemons, which only answer when ready. |
| `daemon.status` (method) | Returns the current `DaemonStatus`. It works in every phase. |
| `daemon.status` (notification) | Sent when the status changes, coalesced as in section 3. |
| Error `daemon_starting` | Refused because the daemon is starting. `data.status`. |

- `daemon.restarting` stays, and is still sent to clients that predate
  this version. Newer clients read the status instead: the restart's
  non-blocking task while it waits, then `stopping` with `restart: true`.
  `RestartWhen::Idle`'s doc in `crates/protocol/src/daemon.rs` says new
  runs are refused while it waits. The engine does not refuse them
  (`crates/engine/src/runtime/restart.rs`), so the doc is corrected with
  this change.
- Phase and task fields are open enums and free-form strings, so later
  producers need no protocol change.
- The runtime and plugin protocols do not change. A runtime or plugin that
  wants to report a slow step can get a producer path to the board later,
  if one is ever needed.

## 6. Clients

All clients share one presentation, built in `client_state` so that the TUI
and the web read the same model:

- **Starting:** In place of the session list's loading state and the
  transcript, show "Cookie is starting", the blocking tasks with their
  bars, and the elapsed time. The composer stays mounted but disabled,
  with the same reason as during a reconnect. Drafts are kept.
- **Ready:** The client runs what it runs after a connect today
  (`session.list`, subscriptions, `session.resume` for the open session).
  The handshake already happened, so this runs at once.
- **Non-blocking tasks:** a one-line notice in the status area
  (TUI status line, web header) with the label and progress. Clicking it
  opens the list. It disappears when the task ends.
- **Failed:** the error's message and hint, the same block clients use for
  any registry error. They stay shown after the daemon exits, and the
  client keeps redialing with its back-off, so a fixed daemon is picked
  up.
- **Restart waiting:** the non-blocking notice above, and nothing else
  changes. Sessions stay usable, and the composer sends as usual.
- **Stopping:** "Cookie is restarting" (or "stopping"). The composer is
  disabled as during a reconnect, and drafts are kept.

Redials while no daemon listens are unchanged. To shorten the gap at a
restart, clients redial at the short end of the back-off (500 ms, not
doubling) for the first 30 seconds after a close with code 1012
(restarting).

**CLI:** `cookie daemon start` and the auto-launch wait for the
`daemon-ready` line as today. Commands that need the engine print each
blocking task's label and progress on stderr while they wait for `ready`.
`cookie daemon status` prints the phase and tasks.

## 7. The restart gap

The old daemon closes its listener before the relaunch. The new process
binds when it starts. Between the two, at about 1.6 s in the measurement,
dials are refused.

**Optional** (decision 4): the restarting daemon passes its listening
socket to the new process. On Unix, it clears `FD_CLOEXEC` and names the
descriptor in an environment variable. On Windows, it uses
`WSADuplicateSocket` or rebinds. The kernel then queues connections
across the relaunch, and the new process takes over the same socket at
step 2. Without this, the 1012 fast redial in section 6 covers the gap
well enough.

## 8. Tests

- Server: a client that connects during startup gets a hello with phase
  `starting`, sees `daemon_starting` for a session method, gets
  `daemon.status` notifications with progress from a test task, then gets
  `ready` and runs `session.list` on the same connection.
- An older client's handshake is held until the handover.
- A failed startup reaches a connected client with the error before the
  process exits.
- `/runtime` answers 503 before the handover.
- Board: coalescing (many advances produce one notification per window),
  the ending of a task, a failed blocking task failing the phase.
- Store: the migration and the conversion report per-tree progress (count
  equals trees).
- Clients: `client_state` model tests for starting, ready, failed, and
  notice. One web e2e opens the page while a test daemon holds a blocking
  task, and sees the startup view turn into the session list.
- Protocol: compat baseline, schema additive, and errors reference
  regenerated.

## 9. Decisions

The user's decisions of 2026-11-01:

1. **Engine methods during startup are refused** at once with
   `daemon_starting`, never held. Newer clients do not send them before
   `ready`.
2. **A failed daemon exits right away** once its failure has reached the
   connected clients, waiting at most 5 seconds for delivery.
3. **Non-blocking tasks after startup show as a notice**, so the status is
   general, not startup-only.
4. **The socket handoff at restart is later work** (section 7). The 1012
   fast redial covers the gap for now.
5. **`daemon.restarting` folds into the status.** A restart's wait is a
   non-blocking task counting down the active runs and background calls,
   while the phase stays `ready` and every session stays usable (user,
   2026-11-01: waiting to restart is not like starting up). Once the
   restart is due, the phase is `stopping` with `restart: true`. Clients at this version read only that.
   `daemon.restarting` is still sent, but only to connections whose hello
   names an older protocol.

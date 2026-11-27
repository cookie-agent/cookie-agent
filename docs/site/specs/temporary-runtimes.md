# Temporary runtimes: one runtime per session tree

Status: **implemented** (2026-11-26; client protocol 1.102.0). The user's
decisions are in [section 8](#8-decisions-2026-11-26); where the build
differs from the draft, [section 9](#9-implementation-notes) says so.

## 1. Problem

A template runtime (`[runtimes.<id>]` with a `command`) is one process
that serves every session. It starts at first use and runs until the daemon
exits or someone presses Disconnect
([runtimes](runtimes.md) section 10,
[runtime management](runtime-management.md)). This fits an ssh bridge to a
computer that is always there. It does not fit a sandbox that should exist
only for one piece of work, such as a docker container per session.

A shared temporary container works today:

```toml
[runtimes.sandbox]
command = ["docker", "run", "--rm", "-i", "sandbox-img", "cookie", "runtime", "serve", "--stdio"]
```

It has three gaps:

- **Every session shares the container.** There is no way to give each
  session its own sandbox, or to name per-session resources (a container,
  a volume) in the command.
- **A new machine per container.** A runtime reports a machine ID hashed
  from `/etc/machine-id`, else from a random ID it writes to
  `~/.cookie-agent/machine-id` (`protocol/src/paths.rs`). Most images have
  neither, so each fresh container is a new machine in `machines.json`,
  with its own memory folder. An image that bakes in `/etc/machine-id` has
  the opposite problem: every container of it is one machine, whatever the
  template.
- **Nothing stops it or cleans up after it.** The container lives until the
  daemon exits. Killing the `docker run` client (a daemon crash, a SIGKILL
  after `shutdown_grace_ms`) leaves the container running, and per-session
  volumes are never removed.

## 2. Configuration

```toml
[runtimes.sandbox]
command = ["docker", "run", "--rm", "-i", "--name", "cookie-{session_id}",
           "-v", "cookie-{session_id}:/work", "sandbox-img",
           "cookie", "runtime", "serve", "--stdio"]
scope = "session"
idle_stop_after = "30m"
stop_command = ["docker", "rm", "-f", "cookie-{session_id}"]
delete_command = ["docker", "volume", "rm", "cookie-{session_id}"]
```

New keys on a template that has a `command`:

| Key | Meaning |
|---|---|
| `scope` | `"shared"` (default, today's behaviour): one process for every session. `"session"`: one process per session tree (section 3). |
| `idle_stop_after` | A duration, in the format of `[mcp] idle_stop_after`. The process stops once it has been idle this long (section 4). `"0"` never stops it. Default `"0"` for `shared`, so today's templates keep running, and `"30m"` for `session`. |
| `stop_command` | Optional. Run after the process has exited or been killed, every time it stops (section 5). |
| `delete_command` | Optional. Run once when the owning session tree is deleted (section 5). `scope = "session"` only. |

**Placeholders.** `{session_id}` is the ID of the root session of the tree
that owns the instance, such as `01a1946d-d1a0-7063-9890-004906b59348`
(lowercase hex and hyphens, so it is a valid docker container and volume
name). It is substituted wherever it appears inside an element of
`command`, of the hook commands, and of `env` values, with no shell
interpretation, as `{workspace}` once was. A remote shell (ssh) quotes for
itself. The child and both hooks also get `COOKIE_SESSION_ID` in their
environment.

**Validation** (configuration load fails, naming the template):

- `{session_id}` in a `shared` template.
- `scope`, `idle_stop_after`, `stop_command`, or `delete_command` on a
  table without `command` (a dial-in runtime's settings) or on
  `[runtimes.local]`. Only runtimes the daemon starts can be temporary
  (decision 2).
- `delete_command` on a `shared` template.
- An empty hook array, or one whose first element is empty.
- `{workspace}` stays refused as today.

Changing these keys needs a daemon restart, like the rest of
`[runtimes.*]`.

## 3. Instances and the owner

A `session` template's process is keyed by **(template ID, root session)**.
The root session is the root of the session's delegation tree
(decision 1). A delegated child that attaches `sandbox/coding`, through
`capabilities` on `delegate_subagent` or its own `attach_machine`, gets its
root's instance, so a worker sees the files its parent made. Parallel
children share the container. The rule against overlapping tools on one
machine is about one session's attachments, so it is unaffected.

What does not change:

- **The runtime ID stays the template's ID.** Attachments
  (`RuntimeAttachment.runtime_id = "sandbox"`), permission rules,
  `list_runtimes`, project defaults, `capabilities: ["sandbox/coding"]`,
  and the runtime protocol all still say `sandbox`. The runtime never learns
  it is temporary.
- **Workspaces** are paths inside the instance, such as `/work`, recorded
  on the attachment as for any runtime.
- **Template MCP servers and ACP agents** run inside each instance. An ACP
  agent's own state in the container is lost when the instance stops,
  unless it lives on a volume the command mounts.

In the engine, `RuntimeRegistry::ensure` and `open` gain the owner, and the
registry keeps a map of live instances per template. A `shared` template
has one owner, so its path is today's.

## 4. Lifecycle

- **Start** at first use within the tree: a run of a session that attaches
  the template, an attach, a delegation that names it, or a cron or webhook
  run. Opening or viewing a session does not start it, as with Disconnect
  today. The start counts against `startup_timeout_ms`. A cold image pull
  can exceed the 10 s default, and the reference docs say to raise it.
- **Idle** means not busy, where busy is what blocks Disconnect today: an
  active run of a session in the tree that attaches the template or runs one
  of its ACP agents, or a background call executing on the instance. A
  `shared` instance is busy while any session's run or background call is,
  and with `idle_stop_after` set it stops after that long without one, then
  starts again at next use like a disconnected template today.
- **Stop** happens when the instance has been idle for `idle_stop_after`,
  when someone presses Disconnect on that instance, when the owning tree is
  deleted, and when the daemon exits. A stopped instance starts again fresh
  at its next use. State carries over only through what the command mounts,
  which is why `{session_id}` exists.
- **A failed or crashed instance** reconnects per the existing policy
  ([runtimes](runtimes.md) section 6.4). The new process is a new
  container, so the agent gets a transcript notice that the runtime
  restarted.

## 5. Hooks

Both hooks run on the daemon's machine, without a shell, with the daemon's
environment plus the template's `env`, and with the same substitutions.
Each gets 60 s. A hook that fails or times out leaves a diagnostic on the
template's Machines card and in the daemon log. It never blocks a stop or a
deletion, and it is not retried.

- **`stop_command`** runs after every stop, once the process has exited or
  been killed after `shutdown_grace_ms`. For docker it removes a container
  that the killed client left running.
- **Recovery after a crash.** The daemon records each temporary
  template's running processes, and each owner of a `session` template, in
  `runtime-processes.json` under its data root, and marks a process stopped
  after its `stop_command` has run (or right away when there is none). At
  start, it runs `stop_command` for every process still recorded as
  running, so a daemon killed mid-run does not leave containers behind, and
  it forgets the records of templates `config.toml` no longer has.
  (Implementation note: the draft put these records in `machines.json`; a
  file of their own keeps the machine registry's format unchanged.)
- **`delete_command`** runs when the root session is deleted (a project
  deletion counts, since it takes its sessions), after the instance has
  stopped and `stop_command` has run. It runs even when the instance was
  never started in that daemon's lifetime, because a volume from an
  earlier start may remain. It runs only for root sessions that have used
  the template, which the registry records. A template removed from
  `config.toml` loses its hooks, and its leftover resources are the
  user's to remove.

Neither hook is required (decision 3). Without them, a `docker run --rm`
template still works, with the leaks described in section 1.

## 6. Machines

Every instance of a `session` template is placed on **one machine pinned to
the template**: at first connect, the daemon pins the template's runtime ID
to a machine named after it (`sandbox`), unless the user has already
assigned it elsewhere. Placement rule 1 (the pin) wins, so the reported
machine ID is never added to the machine. The machine stops collecting a
random ID per container, and `machine://sandbox/...` memory holds notes
about the image that outlast its containers. Platform and home are taken
from the latest instance to connect. The machine is online while any
instance is connected.

A `shared` template that runs a container gets the same pin with
`idle_stop_after` or `stop_command` set, since either marks it as
temporary. A plain `shared` template places as today.

## 7. Clients and protocol

- **Client protocol (minor bump).** The template summary gains `scope` and
  `idle_stop_after`. A `session` template's listing gains its live
  instances (root session ID, state, started at). `runtimes.disconnect`
  gains an optional `session` that names the instance; without it, it
  disconnects every idle instance of the template.
- **Machines view (web) and `/runtime` (TUI).** A `session` template's card
  says "one per session" and lists its live instances by root session
  title, each with its own Disconnect. Hook failures show as diagnostics on
  the card.
- **`list_runtimes` and the machines block.** A `session` template reads as
  `sandbox (one per session tree; starts on use)`. In a session whose
  instance has stopped, the machine reads as offline and starts on next use.
- **Runtime protocol:** unchanged.

**Phases**

1. Config keys and validation, owner-keyed registry, `{session_id}`
   substitution, machine pin.
2. Idle stop, both hooks, the live-owner record and recovery at start.
3. Client protocol, web and TUI cards, `list_runtimes` text, and the
   reference docs (`runtimes/index.md`, `concepts/machines.md`, a docker
   example).

## 8. Decisions (2026-11-26)

1. **Owner is the root session of the delegation tree.** Delegated children
   share their root's instance. No per-child scope.
2. **Dial-in runtimes are out of scope.** Only templates the daemon starts
   can be `session`-scoped or stop when idle.
3. **`stop_command` and `delete_command` are accepted and optional.**

Chosen in this draft, open to change: the `30m` default of
`idle_stop_after` for `session` templates, the 60 s hook timeout, the pin
for temporary `shared` templates (section 6), and a `runtimes.disconnect`
without `session` stopping every idle instance.

## 9. Implementation notes

- **Records** live in `runtime-processes.json`, not `machines.json`
  (section 5).
- **The machine pin** comes from the existing placement rules: a temporary
  template's listing leaves out the machine its process reports, so the
  daemon places it on a new machine named after the runtime ID and pins it
  there.
- **A shared template stopped for idleness** is parked as after a
  Disconnect: opening a session leaves it down until a run, attachment,
  session creation, or delegation uses it.
- **Clients** show `one per session` beside the lifetime and offer no
  Connect for a per-session runtime; its Disconnect stops every process
  nothing uses. The card does not yet list each live instance by its root
  session's title with a Disconnect of its own; `runtimes.list` carries
  each instance's `owner` and `runtimes.disconnect` takes `session` for
  that.
- **Client-side lookups without a session** (`runtimes.reconnect`,
  `runtimes.connect`, an MCP server reconnect from the MCP view, runtime
  groups) refuse a per-session runtime or pick any owner's process.
- **An ACP agent** on a per-session runtime runs once per owner, inside
  that owner's process. Deleting a session does not start the process
  only to end the agent's sessions there.

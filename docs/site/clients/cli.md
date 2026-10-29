# Command line

This page is the reference for the `cookie` command: every subcommand and
option, how commands find a daemon, and how they exit, then headless runs with
`cookie run` in detail. Interactive work happens in the
[terminal UI](tui.md) or the [web client](web.md), and
[Persistent daemon](../daemon/persistent.md) covers running `cookie daemon` and
attaching clients to it.

## Commands

| Command | What it does |
|---|---|
| [`cookie`](#cookie) | The terminal UI, on a daemon already running on this machine or in this process |
| [`cookie run`](#cookie-run) | Run one prompt without the TUI |
| [`cookie daemon`](#cookie-daemon) | Serve the client protocol, the web page, and dial-in runtimes |
| [`cookie daemon restart`](#cookie-daemon-restart) | Restart a running daemon, once idle or at once |
| [`cookie attach`](#cookie-attach) | The terminal UI on a running daemon, here or elsewhere |
| [`cookie web`](#cookie-web) | Print the daemon's web page address with its token |
| [`cookie onboard`](#cookie-onboard) | Set up a daemon: a provider, the default and small models, the starter kit |
| [`cookie connect`](#cookie-connect), [`cookie disconnect`](#cookie-disconnect) | Store or remove a managed provider connection |
| [`cookie runtime serve`](#cookie-runtime-serve) | Serve this machine's `coding` runtime over stdio or by dialing in |
| [`cookie runtime mcp-bridge`](#cookie-runtime-mcp-bridge) | Connect an ACP agent's MCP client to cookie's tools (run by a runtime) |
| [`cookie config`](#cookie-config) | Read and change the daemon's `config.toml` and agent documents |
| [`cookie machine`](#cookie-machine) | List and regroup the machines the daemon's runtimes run on |
| [`cookie memory`](#cookie-memory) | List, edit, and delete the daemon's memories |
| [`cookie project`](#cookie-project) | List, create, rename, delete, and edit the daemon's projects |
| [`cookie cron`](#cookie-cron) | List, create, edit, run, and delete the daemon's cron jobs |
| [`cookie skills`](#cookie-skills) | List the daemon's skills, refresh the official ones, and check skills |
| [`cookie sessions upgrade`](#cookie-sessions-upgrade) | Convert stored sessions to this build's history format |
| [`cookie mcp`](#cookie-mcp) | List MCP servers and authorize a remote one |

`cookie --help`, and `--help` after any subcommand, prints clap's usage;
`cookie --version` prints the version.

### Global options

| Option | Meaning |
|---|---|
| `--error-details` | Accepted by every command. When the daemon refuses a request, also print the technical detail of its error, such as the upstream cause or a path (see [Error details](tui.md#error-details)). |
| `--standalone` | Plain `cookie` only: run the engine in this process, without looking for a running daemon. With a subcommand it is a usage error. |
| `--project <NAME>` | Plain `cookie` only: start a new session in this [project](../concepts/projects.md). With a subcommand it is a usage error. |

### Reaching a daemon

`attach`, `web`, `connect`, `disconnect`, `daemon restart`, `config`,
`machine`, `memory`, `project`, `cron`, `mcp`, and `runtime serve --daemon`
talk to a running daemon. Each takes `--url` and `--token`; for `config`, `machine`, `memory`,
`project`, and `cron` the two may also follow the nested subcommand. They resolve the daemon this way:

- **URL:** `--url`, else `[server] url` in this machine's `config.toml`, else
  the local daemon that table describes, `ws://<host>:<port>/ws`. The URL must
  be `ws://` or `wss://` with the path `/ws`.
- **Token:** `--token`, else the `COOKIE_DAEMON_TOKEN` environment variable,
  else `[server] token`. With none of them the command fails with
  `no daemon token`. cookie removes `COOKIE_DAEMON_TOKEN` from its
  environment at startup, so the processes it starts do not inherit it.

When both `--url` and a token are given, `config.toml` is not read. See
[Server](../engine/server.md) for the `[server]` keys and
[Remote access](../daemon/remote-access.md) for daemons on other machines.

### Exit status

A command that succeeds exits `0`. One that fails prints `cookie: ` and the
error on standard error and exits `1`; when the error is a daemon refusal
that has technical detail, it adds that detail with `--error-details`, or a
line saying to run again with it. Command-line usage errors, such as an
unknown option or a missing argument, exit `2` with clap's message.
`cookie run` has its own [exit codes](#exit-codes).

## Command reference

### `cookie`

```text
cookie [--standalone] [--project <NAME>] [--error-details]
```

Opens the terminal UI for the current directory. Unless `--standalone` is
given, it first looks for a daemon on this machine, as
[A daemon already running](../daemon/persistent.md#a-daemon-already-running)
describes, and opens the TUI on it; otherwise it runs the engine in this
process and the TUI with it. Quitting the TUI exits `0`. See
[Terminal UI](tui.md).

With `--project <NAME>` the TUI starts on a new session in that
[project](../concepts/projects.md): it takes the project's default agent and
preset, with the agent's own default model, and, when the project's defaults
name runtimes, the daemon attaches those instead of the TUI's usual ones. An
agent that is gone or cannot run as root leaves the usual agent, and the
status line says so. A new session belongs to no project otherwise; nothing
is picked from the current directory.

### `cookie run`

```text
cookie run [OPTIONS] <PROMPT | -p PROMPT | -f PATH>
```

Runs one prompt through an engine in this process, without the TUI, and
never looks for a running daemon. [Headless runs](#headless-runs) below
describes each option.

| Option | Default | Meaning |
|---|---|---|
| `PROMPT`, `-p`, `--prompt <PROMPT>` | | The prompt; `-` reads standard input. Exactly one prompt source is required. |
| `-f`, `--prompt-file <PATH>` | | Read the prompt from a file; `-` reads standard input. |
| `-a`, `--agent <AGENT>` | see [Selection and limits](#selection-and-limits) | Root-runnable agent ID. |
| `--preset <PRESET>` | | Agent preset for a new session. |
| `-m`, `--model <MODEL>` | see [Selection and limits](#selection-and-limits) | `provider/model` to run. |
| `--variant <VARIANT>` | | Named model variant, or `base` for none. |
| `--permission-mode <MODE>` | `auto-approve` | `auto-approve`, `auto-approve-n`, `auto-approve-y`, `ask`, or `yolo`; the underscore spellings are aliases. |
| `--allowed-tools <ACTION,...>` | | Permission actions, or `skill:<name>`, to allow; repeatable and comma-delimited. |
| `--skill <NAME>` | | Load a user-invocable skill before the prompt. |
| `--skill-args <TEXT>` | | Arguments of `--skill`; requires it. |
| `--max-turns <N>` | `100` | Most committed root model turns, at least 1. |
| `--timeout <SECONDS>` | `600` | Run timeout, at least 1. |
| `--resume-session <ID>` | | Continue this session instead of creating one. |
| `--data-dir <PATH>` | `~/.cookie-agent` | Session and artifact store. |
| `-o`, `--output <FORMAT>` | `text` | `text`, `json`, or `none`. |
| `--json` | | Alias for `--output json`; conflicts with `--output`. |
| `--output-file <PATH>` | | Write output to a file; not with `--output none`. |
| `--verbose` | | Progress on standard error and, in JSON mode, tool-output records. |

### `cookie daemon`

```text
cookie daemon [--port <PORT> | --listen <ADDR:PORT>] [--tls-cert <PATH> --tls-key <PATH>]
```

Serves the client protocol on `/ws`, the web page, and dial-in runtimes on
`/runtime`, for sessions of any directory; the sessions are stored for the
directory it starts in. It prints the ready line, then its address and client
protocol version, its process ID, and the web page's address, and runs
until a stop signal or a [restart](#cookie-daemon-restart).
Without a `[server] token` it writes a fresh one to `config.toml` first. See
[Persistent daemon](../daemon/persistent.md#separate-server-and-client).

| Option | Default | Meaning |
|---|---|---|
| `--port <PORT>` | `[server] port` | Loopback port to bind; `0` lets the OS pick one, which the ready line reports. Conflicts with `--listen`. |
| `--listen <ADDR:PORT>` | `127.0.0.1`, the port above | Address and port to bind, such as `0.0.0.0:7419`, for clients and runtimes on other machines. |
| `--tls-cert <PATH>` | | PEM certificate chain to serve `wss` with; requires `--tls-key`. |
| `--tls-key <PATH>` | | PEM private key of `--tls-cert`; requires `--tls-cert`. |

See [Remote access](../daemon/remote-access.md#listening-on-other-addresses)
before listening off loopback.

#### `cookie daemon restart`

```text
cookie daemon restart [--now] [--url <URL>] [--token <TOKEN>]
```

Asks a running daemon to restart (`daemon.restart`): it relaunches its own
executable with the same arguments and environment, which applies the
configuration `cookie config status` lists as needing a restart and runs a
binary upgraded on disk. Without `--now` the daemon waits until no turn is
running in any session and refuses new turns meanwhile; the command prints
what it waits for and how many turns are left. It then waits for the daemon
to come back, up to a minute, and says whether its version changed.
Ctrl-C stops waiting; the restart stays scheduled. See
[Restarting the daemon](../daemon/persistent.md#restarting-the-daemon).

| Option | Meaning |
|---|---|
| `--now` | Restart at once; running turns end as interrupted. Also turns a restart already waiting into an immediate one. |
| `--url <URL>` | The daemon's client URL (see [Reaching a daemon](#reaching-a-daemon)). |
| `--token <TOKEN>` | The daemon's token (see [Reaching a daemon](#reaching-a-daemon)). |

```sh
cookie daemon restart --url ws://127.0.0.1:7419/ws --token <token>
```

A daemon not started by `cookie daemon`, such as an engine embedded in
another program, answers `restart_unsupported`.

### `cookie attach`

```text
cookie attach [--url <URL>] [--token <TOKEN>]
```

Opens the terminal UI on a running daemon, found as
[Reaching a daemon](#reaching-a-daemon) says. It never runs an engine of its
own: a daemon it cannot reach or use is an error. A dropped connection is
redialed, and quitting leaves the daemon running. See
[Persistent daemon](../daemon/persistent.md#separate-server-and-client).

### `cookie web`

```text
cookie web [--url <URL>] [--token <TOKEN>] [--open]
```

Prints the daemon's web page with the token in its fragment,
`http://127.0.0.1:7419/#token=<token>`: the client URL with `ws` turned into
`http` (`wss` into `https`) and the final `ws` dropped. It does not contact
the daemon. `--open` also opens the page in the system browser (`open` on
macOS, `start` on Windows, `xdg-open` elsewhere). See [Web](web.md).

### `cookie onboard`

```text
cookie onboard [--from <FILE>] [--diff] [--yes]
               [--default-model <KEY[:VARIANT]>] [--small-model <KEY[:VARIANT]>]
               [--url <URL>] [--token <TOKEN>]
```

Sets up the daemon that `--url` and `--token` name, here or on another
machine, through its onboarding API; `cookie onboard` writes nothing itself.
The steps, as numbered questions:

1. **Provider**: connects one, as [`cookie connect`](#cookie-connect) does,
   when none is connected; otherwise offers to connect another.
2. **Default model** (`default_model`): a numbered list of the connected
   providers' available models, grouped by provider; `/text` searches it.
   Its variant question preselects the model's default variant.
3. **Small model** (`small_model`): the same list with nothing suggested; its
   variant question preselects the model's first listed variant, the
   cheapest. It may be the same model.
4. **Starter kit**: shows what installing it would do to each file (new,
   same as yours and skipped, or differs and written beside yours as
   `<name>.new`) and asks whether to install it.

The kit comes from the daemon's own release (`v<version>`), else its
newest nightly of the same version that carries one (`v<version>-alpha.<n>`,
from GitHub's release list), else the latest stable release; `--from` takes a downloaded `cookie-agent-defaults.tar.gz` instead.
Its manifest's hashes are checked before anything is written. `--diff`
prints what installing the kit would change, with a diff of each changed
file, and changes nothing. `--default-model` and `--small-model` answer
those questions (`provider/model`, or `provider/model:variant`); `--yes`
answers the rest with their defaults (keep what is set, install the kit) and
refuses rather than guess a model or connect a provider. Without `--yes` or
`--diff` it needs an interactive terminal. Running it again on a set-up
daemon shows the current provider and models and offers the kit again. See
[Getting started](../quickstart.md).

### `cookie connect`

```text
cookie connect [PROVIDER_ID] [--url <URL>] [--token <TOKEN>]
```

Creates or updates the daemon's durable, global connection to a managed
provider: pick the provider (or name it), confirm, choose the auth method,
then type its setup values and credentials. It needs an interactive terminal
on standard input, output, and error: credentials are never accepted as
arguments. Answering anything but `y` or `yes` to the confirmation cancels it
with an error. See [Providers](../extend/providers.md).

A provider connected by a browser sign-in, such as `chatgpt`, prints the
sign-in address instead of asking for credentials. The redirect returns to a
listener this command opens on `127.0.0.1`; when the browser runs on another
machine, paste the address it fails to load, or forward the printed port over
SSH first. See [ChatGPT](../extend/providers.md#chatgpt).

### `cookie disconnect`

```text
cookie disconnect [PROVIDER_ID] [--url <URL>] [--token <TOKEN>]
```

Removes a managed provider connection after a `[y/N]` confirmation; it needs
an interactive terminal like `connect`. Disconnecting `chatgpt` signs out and
revokes the sign-in.

### `cookie runtime serve`

```text
cookie runtime serve --stdio [--root <DIR>]...
cookie runtime serve --daemon [<URL>] [--id <RUNTIME_ID>] [--token <TOKEN>] [--root <DIR>]...
```

Serves this machine's runtime, the `coding` capability (`read`, `write`,
`edit`, `bash`, `webfetch`), over the runtime protocol. It needs no provider
configuration. One of `--stdio` and `--daemon` is required.

| Option | Meaning |
|---|---|
| `--stdio` | Speak the runtime protocol on standard input and output, one JSON-RPC message per line, as a [`[runtimes.<id>]` template](../runtimes/templates.md) starts it. Exits when the daemon sends `runtime/shutdown` or closes standard input. |
| `--daemon [<URL>]` | Dial in to a daemon and stay connected: this client URL, or, without one, the daemon [Reaching a daemon](#reaching-a-daemon) finds. After a drop it connects again, waiting 1 second, doubling up to 30 seconds. Its machine's `[runtimes.local.mcp]` servers and `[runtimes.local.acp.agents]` come along, and it reports changed agents as its configuration changes. |
| `--id <RUNTIME_ID>` | The runtime ID to dial in as; this machine's host name, sanitized, when omitted. Not with `--stdio`. |
| `--token <TOKEN>` | The daemon's token, for `--daemon`; also `COOKIE_DAEMON_TOKEN`. |
| `--root <DIR>` | Limit `coding` to workspaces under this directory; repeat for several. A leading `~` is expanded; a root that is not a directory is an error. Any workspace when omitted. |

It runs until a stop signal. See
[Runtimes that dial in](../runtimes/dial-in.md).

### `cookie runtime mcp-bridge`

```text
cookie runtime mcp-bridge --socket <PATH>
```

Copies standard input to a runtime's bridge socket and the socket to
standard output: the stdio MCP server `cookie` a runtime hands the ACP agents
it hosts, through which they call cookie's tools (see the
[ACP agents spec](../specs/acp-agents.md#9-cookies-tools-for-the-agent-mcp-over-the-runtime)).
The runtime starts it for each ACP session; it is not meant to be run by
hand.

| Option | Meaning |
|---|---|
| `--socket <PATH>` | The bridge socket the runtime opened for the agent's session: a Unix socket, or a named pipe on Windows. |

### `cookie config`

```text
cookie config <COMMAND> [--url <URL>] [--token <TOKEN>]
```

Reads and changes the daemon's configuration documents, `config.toml` and the
agent documents, through the daemon, which applies each change live or says
what needs a restart. See
[config.toml](../engine/configuration.md#changes-while-the-daemon-runs).
Its status notes also name the providers
[native compaction](../sessions/compaction.md#the-provider-record) is off for,
with why and since when.

#### `cookie config edit`

`cookie config edit [PATH]` opens a document in `$VISUAL`, else `$EDITOR`,
else `vi`, or on Windows `code --wait` when VS Code is on `PATH`, else
Notepad: `config.toml` by default, or `agents/<id>.md` or
`agents/<preset>/<id>.md`. A path that does not exist yet is created, an agent
document from a template. It needs an interactive terminal. Saving it
unchanged does nothing; a change the daemon refuses as invalid offers to edit
again.

#### `cookie config set`

`cookie config set <KEY> <VALUE>` sets one dotted key of `config.toml`, such
as `cookie config set loop_warning.enabled false`. The value is TOML; text
that does not parse as TOML is a string.

#### `cookie config unset`

`cookie config unset <KEY>` removes one key of `config.toml`.

#### `cookie config remove`

`cookie config remove <PATH>` removes an agent document; a preset's last one
removes the preset.

#### `cookie config reload`

`cookie config reload` re-reads the configuration files and applies them,
after editing them by hand.

#### `cookie config status`

`cookie config status` shows whether the applied configuration matches the
files, why the last reload failed, and what needs a restart.

### `cookie machine`

```text
cookie machine <COMMAND> [--url <URL>] [--token <TOKEN>]
```

Manages the machines the daemon's runtimes run on. See
[Machines](../runtimes/index.md#machines).

#### `cookie machine list`

Lists each machine with its machine IDs, pinned and connected runtimes, the
offline runtimes it remembers with when each was last seen, platform, and
memories.

#### `cookie machine rename`

`cookie machine rename <MACHINE> <NAME>` renames a machine; its memory folder
moves with it.

#### `cookie machine assign`

`cookie machine assign <RUNTIME> <MACHINE>` pins a runtime ID to a machine,
created when the name is new.

#### `cookie machine unassign`

`cookie machine unassign <RUNTIME>` removes a runtime's pin, so that it
follows the machine ID it reports.

#### `cookie machine merge`

`cookie machine merge <FROM> <INTO>` moves one machine's IDs, pins, and
memories into another and removes it.

#### `cookie machine forget`

`cookie machine forget <MACHINE>` removes a machine that has no connected
runtimes and no memories.

#### `cookie machine forget-runtime`

`cookie machine forget-runtime <RUNTIME>` forgets where an offline runtime
ran, so it is no longer listed on its machine. A pin stays.

### `cookie memory`

```text
cookie memory <COMMAND> [--url <URL>] [--token <TOKEN>]
```

Manages the daemon's memories. See
[Memory](../sessions/memory.md#managing-memory).

#### `cookie memory list`

`cookie memory list [LAYER]` lists the memories of every layer, or of one:
`user`, `agent`, `machine`, or `project`. Invalid files and entries that are
not memories are listed with the reason. Under each memory, a line gives
when it was created, modified, and last read by a model, and its size
against its limit.

#### `cookie memory show`

`cookie memory show <URL>` prints a memory, such as `project://web/build`:
when it was created, modified, and last read, its size against its limit
(and whether it is over it), then its text without the `created` and
`modified` lines the daemon keeps.

#### `cookie memory edit`

`cookie memory edit <URL>` opens a memory, frontmatter included but without
the `created` and `modified` lines the daemon keeps, in
the editor `cookie config edit` uses, such as `project://web/build`; a memory
that does not exist yet is created. Text that is not a valid memory, or a
memory that changed since it was opened, offers to edit again.

#### `cookie memory rm`

`cookie memory rm <URL> [-y | --yes]` deletes a memory, asking first unless
`--yes`.

### `cookie project`

```text
cookie project <COMMAND> [--url <URL>] [--token <TOKEN>]
```

Manages the daemon's [projects](../concepts/projects.md) by name.

#### `cookie project list`

`cookie project list` prints one line per project: its emoji and name, its
sessions and memories, whether it has an `AGENTS.md`, and its defaults.

#### `cookie project create`

`cookie project create <NAME> [--emoji <EMOJI>]` creates a project with an
empty `project://<name>/overview` memory. The name is a lowercase letter or
digit, then up to 63 lowercase letters, digits, or `-`; the emoji is exactly
one emoji.

#### `cookie project rename`

`cookie project rename <OLD> <NEW>` renames a project; its memory folder moves
with it, so its memories become `project://<new>/…`.

#### `cookie project delete`

`cookie project delete <NAME> [-y | --yes]` deletes a project with its
sessions, its memories, its `AGENTS.md`, and its defaults, asking first unless
`--yes`, with the counts of sessions and memories that go. The daemon refuses
while a session of the project runs.

#### `cookie project edit`

`cookie project edit <NAME>` opens the project's `AGENTS.md` in the editor
`cookie config edit` uses; saving it empty removes the file. A project that
changed since it was opened offers to reload it and edit again.

#### `cookie project defaults`

`cookie project defaults <NAME> --from-session <SESSION_ID>` sets the
project's defaults from a session: its runtime attachments, whether it has
`global`, and its agent and agent preset.

### `cookie cron`

```text
cookie cron <COMMAND> [--url <URL>] [--token <TOKEN>]
```

Manages the daemon's cron jobs: prompts it fires on a schedule into a
session, or into a new session per fire. See
[Scheduling prompts](../sessions/scheduling.md). The daemon needs client
protocol 1.15 or later; an older one is refused with a message saying so.
Times are printed in this machine's time zone; cron expressions are the
daemon host's local time.

#### `cookie cron list`

`cookie cron list` prints two lines per job: its ID, its name (or the start
of its prompt), and why it is disabled; then its schedule read plainly (such
as `every day at 03:00`), its next fire, its target, and the outcome of its
last fire. When `[cron] enabled = false` holds on the daemon, a first line
says jobs do not fire.

#### `cookie cron show`

`cookie cron show <ID>` prints a job: its schedule and its plain reading,
target, state, next fire, runs against its limit, delivery or agent, model
pin, and permission mode, failures in a row, who created it and when, and
the session an agent created it in; the target and that session are named
by title with their session IDs; then
its prompt and its history, newest first, with each fire's outcome and the
session it went to.

#### `cookie cron create`

```text
cookie cron create --prompt <TEXT> --schedule <SCHEDULE>
                   (--session <SESSION_ID> [--delivery steer|queue]
                    | --agent <AGENT> [--model <MODEL>] [--permission-mode <MODE>] [--project <NAME>])
                   [--name <NAME>] [--max-runs <RUNS>] [--disabled]
```

Creates a job and prints its ID and next fire.

| Option | Meaning |
|---|---|
| `--prompt <TEXT>` | What each fire sends. |
| `--schedule <SCHEDULE>` | Five cron fields in the daemon host's local time, such as `"0 3 * * *"`, or a delay, such as `"in 20m"` (`s`, `m`, `h`, or `d`, from `1s` to `365d`), which fires once. |
| `--name <NAME>` | What lists show instead of the prompt's start. |
| `--max-runs <RUNS>` | Cron schedules only: the fires the job delivers, a whole number, 0 or less for no limit; 1 when omitted. |
| `--session <SESSION_ID>` | Fire into this session, a root or a delegated one. |
| `--delivery <MODE>` | With `--session`: `queue` (after the running turn, the default) or `steer` (into it). |
| `--agent <AGENT>` | Start a new session with this agent per fire instead. |
| `--model <MODEL>` | With `--agent`: pin `provider/model` or `provider/model[variant]`; the agent's own models otherwise. |
| `--permission-mode <MODE>` | With `--agent`: the run sessions' mode, spelled as for `cookie run`; `auto-approve-n` when omitted. |
| `--project <NAME>` | With `--agent`: the project the run sessions belong to. |
| `--disabled` | Create the job disabled. |

#### `cookie cron edit`

`cookie cron edit <ID> [OPTIONS]` changes a job; only what the options give
changes, and an edit that changes nothing says so. It takes `--prompt`,
`--schedule`, `--name` (empty removes the name), `--max-runs`, `--session`
(fire into that session instead), `--delivery`, `--agent` (start a new
session with that agent per fire, or change a new-session job's agent),
`--model` (empty removes the pin), and `--permission-mode`. A new schedule
computes the next fire from now.

#### `cookie cron rm`

`cookie cron rm <ID> [-y | --yes]` deletes a job, asking first unless
`--yes`. Its run sessions stay.

#### `cookie cron enable`

`cookie cron enable <ID>` enables a job: its next fire is computed from now,
its failure count resets, and a finished job's run count starts again. A
one-shot whose time has passed needs a new `--schedule` first.

#### `cookie cron disable`

`cookie cron disable <ID>` disables a job; it keeps its history.

#### `cookie cron run`

`cookie cron run <ID>` fires a job now, as its schedule would, and prints
the fire's outcome and the session it went to. A run now counts toward
neither the job's run limit nor its failures, and fires even with
`[cron] enabled = false`.

### `cookie skills`

```text
cookie skills <COMMAND> [--url <URL>] [--token <TOKEN>]
```

Lists the daemon's skills and refreshes its official skills, the ones it
keeps up to date in `official_skills/` of its data root; see
[Official skills](../extend/skills.md#official-skills).

#### `cookie skills list`

`cookie skills list` prints the daemon's own skills, its user and official
ones (project skills belong to a session's workspaces): each skill's name,
where it comes from (`user`, or `official` with its version), marked
`(shadowed)` when another skill of the same name wins, and its file.

#### `cookie skills update`

`cookie skills update` refreshes the official skills now, as the daemon does
at start and daily, and prints what changed and the skills it keeps. A
refresh that fails keeps the skills it had and fails with the reason; with
`[skills] official = false` it does nothing.

#### `cookie skills check`

```text
cookie skills check <DIR> [--docs <DIR>]
```

Checks skills before they are published, with no daemon: each
`<DIR>/<name>/SKILL.md` (or `<DIR>/SKILL.md` alone) must parse, every
backticked `cookie …` command it names must exist in this build, and every
link to `https://cookie-agent.github.io/doc/dev/…` must name a page. Links
are fetched from the published site, or checked against a `docs/site`
checkout with `--docs`. An `index.json` in `<DIR>` must list every skill
there, with a version, a `requires_cookie` range, and each file's SHA-256.
Each problem is printed with its file, and any problem fails the command.
The official skills repository runs it on every push.

### `cookie sessions upgrade`

```text
cookie sessions upgrade [--all]
```

Converts the sessions of the current directory, or with `--all` of every
directory, in `~/.cookie-agent` to this build's history format, as opening
each one would. The originals stay beside them as `events.jsonl.v<N>.bak`,
and a session tree another cookie process holds is skipped. It works on the
files directly, without a daemon. It prints a line per tree it converted or
could not convert and a summary, and exits `1` when any session could not be
converted. See [Sessions](../sessions/index.md).

### `cookie mcp`

```text
cookie mcp <COMMAND> [--url <URL>] [--token <TOKEN>]
```

#### `cookie mcp list`

Lists every runtime's MCP servers, one per line: the name, its tool count,
its state, and the runtime (with its workspace) that hosts it.

#### `cookie mcp auth`

`cookie mcp auth <SERVER>` authorizes a remote MCP server of the `global`
runtime that needs OAuth. It prints the authorization URL and waits for the
browser to return to a callback on this machine's loopback; when the browser
runs on another machine, paste the URL it was redirected to. A stop signal
cancels the authorization. See [MCP](../extend/mcp.md).

## Headless runs

`cookie run` executes one prompt through the local engine without starting the
TUI, daemon, or an in-process protocol server. It is intended for CI and scripts.

### Prompt input

Provide exactly one prompt source:

```console
cookie run "Review this workspace"
cookie run -p "Review this workspace"
cookie run -f request.txt
printf '%s\n' 'Review this workspace' | cookie run -
printf '%s\n' 'Review this workspace' | cookie run -p -
```

The positional prompt, `-p/--prompt`, and `-f/--prompt-file` conflict with each
other. `-` reads standard input for any of them.

### Selection and limits

The default agent is the root-runnable `primary` agent, or the first
root-runnable agent. The default model is its first live fallback, including a
valid variant; if none is live, the first available model and its default
variant are used. Select an agent preset with `--preset`; override the effective
selection with `-a/--agent`, `-m/--model`, and `--variant`. Use `--variant base`
to select no named variant. Every override is validated against the current
coherent runtime before a session starts. An [ACP agent](../extend/acp-agents.md)
chooses its own model and needs no provider: `-a codex` runs it as it is, and
`--model` or `--variant` with it is refused (name its model in its agent
document's `models`).

`--resume-session <id>` continues an existing session and defaults to its
creation preset. Supplying `--preset` selects a different preset for that run
without rewriting the session's creation selection. See
[Agent presets](../extend/agents.md#agent-presets) for resolution and persistence details.
`--data-dir <path>` selects the session and artifact store. `--max-turns` and
`--timeout` are positive guards and default to 100 root model turns and 600
seconds. Reaching either guard cancels the run and waits for its terminal event.
A stop signal (`SIGINT`, `SIGTERM`, or `SIGHUP`; on Windows Ctrl-C,
Ctrl-Break, a closed console, logoff, or shutdown) follows the same
cancellation path.

### Permissions

`--permission-mode` accepts `auto-approve` (`auto_approve`), `auto-approve-n`
(`auto_approve_n`), `auto-approve-y` (`auto_approve_y`), `ask`, or `yolo`.
The selected mode applies to the whole runtime session tree, including delegated
descendants.
Headless runs never wait for approval input. In `auto-approve-n`, a reviewer
escalation is rejected and cancels the root run with exit code `3`. In
`auto-approve-y`, an escalation is approved once and the run continues
automatically, except a call whose review failed, which escalates. Other escalations anywhere in the session tree are rejected;
the runner cancels the root run and waits for the matching terminal event.
A question an [ACP agent](../extend/acp-agents.md#questions-from-the-agent)
asks anywhere in the tree is declined, with the reason "a headless `cookie
run` has no one to answer", and the run goes on.

`--allowed-tools` may be repeated or comma-delimited and accepts any
permission action, such as `read`, `write`, `bash`, `webfetch`, `delegate`, or
`mcp` (a lowercase letter, then lowercase letters, digits, or `_`, at most 32
bytes), and `skill:<name>` — each entry adds an `allow` overlay rule (resource
`*`, or the given skill name). It does not deny omitted actions. The rule
replaces only the agent's rule with the same pattern: `--allowed-tools bash`
turns the agent's `bash: ask` into `allow`, but a narrower agent rule such as
`delegate: {reviewer: ask}` still decides the calls it matches (see
[How the levels combine](../concepts/permissions.md#how-the-levels-combine)).

### Skills

`--skill <name>` loads a user-invocable skill before the prompt run.
`--skill-args <text>` supplies its raw arguments and requires `--skill`. Skill
permission is evaluated before injection; use `--allowed-tools skill:<name>` to
grant it explicitly in unattended runs. The load appends the same durable event
used by interactive and model invocation.

### Output

Select `text`, `json`, or `none` with `-o/--output`. `--json` is an alias for
`--output json` and conflicts with an explicit `--output`. `--output-file`
redirects text or JSON output to a file and cannot be combined with
`--output none`.

Text mode writes only the terminal `final_text` to standard output. It does not
stream model deltas. With `--verbose`, ANSI-free progress lines are written to
standard error, including one for each finished tool call: `cookie run: tool`
and the call's title (with `(failed)`, `(cancelled)`, or `(interrupted)` when
it did not complete), then the first eight lines of its display as plain text,
indented, and a count of the rest. `--output none` prints the same lines. Failures and model fallback diagnostics are written to standard
error regardless of verbosity, including recovered failures in a successful run.

JSON mode writes JSON Lines. Records use these stable `type` tags:

- `event`: one accepted, ordered event for the active run.
- `tool_output`: a retained or live tool-output delta, emitted with `--verbose`.
- `tool_output_gap`: a tool-output retention or delivery gap, emitted with
  `--verbose`.
- `summary`: the final record, containing terminal status and exit code, IDs,
   turn/rejection/recovery counts, cancellation cause, final text, error detail, and the
  session usage and estimated-cost rollup.

`--output none` suppresses command output. Diagnostics and verbose progress
still use standard error.

### Failure diagnostics

`cookie run` writes failure diagnostics to stderr even with `--output none` and
without `--verbose`. JSON output retains diagnostic event fields and adds an
`error` field to the final summary (null for success). A failed tool or fallback
can produce stderr diagnostics even if the overall run subsequently succeeds.
This includes internal-agent fallback failures and session-scoped plugin notices
for the selected session. Events from unrelated sessions/runs remain excluded.
Exit codes retain their existing meanings.

### Exit codes

The active run's terminal event determines the runtime exit code. Command-line
syntax errors use Clap's exit code `2` before the runtime starts.

| Exit code | Meaning |
|---|---|
| `0` | `RunCompleted`, or `user_before_input` intentionally handled the input without starting a run |
| `1` | `RunFailed`, blocked model selection, or an unrecoverable active-driver failure |
| `3` | `RunCancelled` after the engine accepted a permission-triggered cancellation |
| `4` | Other `RunCancelled` outcomes and every `RunInterrupted`, including a stop signal, timeout, and turn-limit cancellation |
| `5` | Environment or setup failure before the run becomes active |

# The user's shell environment for runtimes

Status: **implemented** (2026-10-28; runtime protocol 1.12.0; notes in
[section 14](#14-implementation-notes)). The user decided on 2026-10-28 that capture
is a configuration setting that is on by default, and that the default
capture is an interactive shell ([section 13](#13-decisions)). Builds on
[runtimes](runtimes.md), [runtime capabilities](runtime-capabilities.md)
(the persistent daemon), [ACP agents](acp-agents.md),
[plugins](plugins-v1.md), and [live engine configuration](engine-configuration.md).

## 1. Problem

A daemon run as a service starts with the service manager's environment,
not the user's shell's. Everything the daemon's runtimes start inherits
that environment: the `bash` tool and its background calls, stdio MCP
servers, ACP agents, plugins, and the commands of `[runtimes.<id>]`
templates. On a machine whose setup lives in the shell's rc files, the
`local` runtime is then close to unusable.

Observed on yunhao-labserver on 2026-10-28:

- The `cookie-daemon` systemd user unit's process has only `HOME`, `USER`,
  `LOGNAME`, `SHELL`, `LANG`, `PATH`, `XDG_*`, `DBUS_SESSION_BUS_ADDRESS`,
  `SSH_AUTH_SOCK`, and systemd's own variables. Its `PATH` is a hand-copied
  `Environment=PATH=...` drop-in, the workaround
  [Persistent daemon](../daemon/persistent.md#linux) recommends. The copy
  has already gone stale once: it names a versioned Node directory.
- The user's setup is in `~/.bashrc`, below Debian's
  `case $- in *i*) ;; *) return;; esac` guard: micromamba (`MAMBA_EXE`,
  `MAMBA_ROOT_PREFIX`, the `micromamba` shell function, `alias
  conda=micromamba`), cargo, nvm, bun, the coding-config variables
  (`OPENCODE_IMAGE_GEN_*`), a Node `PATH` entry, and the synced_utils
  aliases.
- The `bash` tool runs `bash -c <command>` (`crates/coding_runtime/src/bash.rs`),
  which is neither interactive nor a login shell. A login shell does not
  help either: `~/.profile` sources `~/.bashrc`, which stops at the guard.

From an empty environment like systemd's:

| Shell | micromamba | `BUN_INSTALL` | `OPENCODE_*` | cargo |
|---|---|---|---|---|
| `bash -c` (the `bash` tool today) | no | no | no | no |
| `bash -lc` | no | no | no | yes (`~/.profile`) |
| `bash -lic` | yes (function) | yes | yes | yes |

The same holds for runtimes on other machines: `cookie runtime serve`
started by an ssh template runs from sshd's non-interactive shell, which
returns at the same guard.

## 2. Goal and scope

Every process a runtime starts gets the environment the user has in a new
terminal on that machine. The `bash` tool also gets the user's shell
functions and aliases, so `micromamba activate`, `conda`, and `nvm` work
in a command the way they do at a prompt.

In scope: the `coding` runtime, wherever it runs (the daemon's `local`,
`cookie runtime serve` over ssh or dial-in, and a client's offered
runtime); the daemon's own launches (plugins and template runtime
commands); configuration; refresh; failure reporting.

Out of scope:

- Windows. Windows has no rc-file convention: a service started by Task
  Scheduler at logon already has the user's environment, and Git Bash's
  rc files are rarely where the setup lives. The setting is accepted and
  has no effect there.
- A persistent shell per session, where `cd` or `export` in one command
  carries over to the next. Each command still starts a fresh shell.
- The daemon's own process environment, which stays as the service
  manager started it ([section 6](#6-what-gets-it)).

## 3. Prior art

- **VS Code** resolves the login shell's environment once at startup by
  running `$SHELL -i -l -c` with a marker around the printed environment
  and a 10-second timeout. Its output is used as the environment of
  terminals, tasks, and extensions.
- **Zed** does the same when it starts the project's language servers and
  tasks.
- **Claude Code** writes a shell snapshot (options, functions, and
  aliases from the user's rc file) once per session and sources it before
  every Bash tool command.
- **Codex** runs commands through the user's login shell (`bash -lc`) and
  filters the variables with `shell_environment_policy`.

This spec does what VS Code and Zed do for the environment, and what
Claude Code does for functions and aliases.

## 4. Configuration

```toml
[shell_environment]
# "interactive": $SHELL -l -i, the environment of a new terminal.
# "login": $SHELL -l only; misses rc files that return early when not interactive.
# "off": the runtime's own environment, as before this spec.
capture = "interactive"
# How long a capture may take; 0 or less waits indefinitely.
timeout_ms = 10000
# The shell to capture with; $SHELL when empty, else /bin/sh.
shell = ""
```

The top-level table applies to every runtime and to the daemon's own
launches. `[runtimes.<id>.shell_environment]` overrides any of its keys
for one runtime. The default `capture` is `"interactive"`. Every key
applies live: a change reaches each runtime as a settings change and sets
off a new capture ([section 8](#8-refresh)). No restart is needed.

The configured `env` of an MCP server, ACP agent, plugin, or runtime
template still overrides captured variables ([section 6](#6-what-gets-it)).
That `env` stays the way to set something just for one process. It is no
longer needed for what the shell already sets.

## 5. Capture

A runtime captures once when it starts, in the background. A process
start that needs the result, such as the first `bash` call or the first
MCP server, waits for it, bounded by `timeout_ms`.

1. Run `<shell> -l -i -c <script>` (`-l` only for `"login"`) with stdin
   from `/dev/null`, in a new session (`setsid`, so there is no controlling
   terminal and nothing can read from one), in the user's home directory,
   and with the runtime's own environment. The shell's job-control warnings
   ("no job control in this shell") go to stderr, which is discarded.
2. The script prints a random per-capture nonce, then `env -0`, then the
   nonce again, so text that rc files print before or after (banners,
   `fortune`, a `conda` notice) cannot be mistaken for variables. For a
   bash shell it also writes the snapshot ([section 7](#7-the-bash-snapshot)).
   The script uses only `printf`, `env`, and `;`, so it runs unchanged in
   sh, bash, zsh, and fish.
3. Parse the NUL-separated entries between the two nonces, reading only
   until the second one arrives: a process an rc file starts in the
   background may hold the output open. A shell that exits without
   printing both nonces, or runs past `timeout_ms`, is a failed capture.
   When the capture ends, whatever is still running in its session is
   killed.
4. Drop the variables that describe that shell rather than the user's
   setup: `PWD`, `OLDPWD`, `SHLVL`, `_`, `PS1`–`PS4`, `PROMPT_COMMAND`,
   `COLUMNS`, `LINES`, `HISTFILE`, `TERM`, and any `COOKIE_*` (cookie's own
   variables always come from the runtime's environment).

The result is the runtime's **shell environment**: the captured variables
in place of the runtime's own, plus the dropped names where the runtime
had them. Variables an rc file unsets stay unset.

The capture shell starts from the runtime's own environment, not a clean
one, so what was active where the daemon or runtime was started carries
over. A daemon started from a terminal with a conda environment or a
virtualenv active runs every command in it. This is intended
([section 13](#13-decisions)).

On yunhao-labserver a capture takes about 0.1 s and yields 24 variables
(prototype, 2026-10-28).

A failed capture falls back to the runtime's own environment, as before
this spec, and is reported ([section 10](#10-failures-and-diagnostics)).
The next attempt is at the next refresh, not on every process start.

## 6. What gets it

The shell environment is the base environment of every process a runtime
or the daemon starts for the user's work:

| Process | Where it is started | Today |
|---|---|---|
| `bash` tool commands and background calls | `coding_runtime::bash` | inherits |
| stdio MCP servers | `mcp` | inherits + `env` |
| ACP agents | `coding_runtime::acp_host` | inherits + `env` |
| plugins | `engine::runtimes::process` | inherits + `env` |
| `[runtimes.<id>]` template commands (`ssh host cookie runtime serve`) | `engine::runtimes::process` | inherits + `env` |

Precedence, lowest first: the runtime's own environment, the shell
environment, then the process's configured `env`. A process is found
through the resulting `PATH`. `std::process::Command` already looks a
program up in the child's `PATH` once the child's `PATH` is set, so an MCP
server that names `npx` finds the user's Node.

The daemon process and the runtime process keep their own environment
untouched. Three reasons:

- `cookie daemon restart` re-executes with the environment the daemon
  started with (`daemon_restart.rs`), and that environment should stay the
  service manager's.
- The user's secrets, such as the API keys exported from rc files, reach
  only the processes started for the user's work. The daemon's logs and
  panic reports cannot include them.
- A refresh replaces the shell environment without touching the process.

Plugins and template commands are started by the daemon on its own
machine, so the daemon keeps a shell environment of its own machine,
shared with its in-process `local` runtime.

## 7. The bash snapshot

Variables cannot carry shell functions or aliases: `micromamba activate`
needs the `micromamba` function, and `conda` is an alias. When the capture
shell is bash, the capture script also writes a snapshot file:

```sh
shopt -s extdebug
{ shopt -p; alias -p; <functions>; } > "$snapshot"
```

- **Shell options come first.** Functions from bash-completion use
  extended globs (`--!(no-*)dir*)`). Without `shopt -s extglob` ahead of
  them, sourcing the snapshot stops at a syntax error and loses every
  function after it. The prototype hit exactly this.
- **Completion functions are left out.** `<functions>` prints `declare -f`
  for each function except those whose source file (`declare -F` under
  `extdebug`) lies under `/usr/share/bash-completion/` or
  `/etc/bash_completion*`. Completion functions are never needed outside a
  prompt and make up most of the snapshot: 58 KB with them on
  yunhao-labserver.
- The snapshot holds no variables, only options, aliases, and functions.
  Variables travel in the process environment ([section 6](#6-what-gets-it)).

The snapshot is a mode-0600 file in a private mode-0700 temporary
directory that the runtime owns and removes when it stops, the same as
the ACP bridge's socket directory. Each capture replaces it.

The `bash` tool then runs:

```sh
bash -c "source '<snapshot>' 2>/dev/null
<command>"
```

The command goes on its own line because bash expands an alias only in
commands it parses after the alias is defined; the snapshot's `shopt -p`
turns `expand_aliases` on. Error messages from the command report its
line numbers one higher than before. `$0` and the positional parameters
are unchanged. The snapshot adds about 45 ms to a command on
yunhao-labserver, most of it parsing the functions. The approval prompt,
the permission check, and the transcript all show the command as written,
without the prefix.

Activation does not carry from one command to the next. Each command is
a fresh shell, so `micromamba activate ucma` in one call is gone in the
next. This is expected (the user, 2026-10-28), and the `bash` tool's
description does not mention it: models already assume a fresh shell
per command.

With a non-bash capture shell (zsh, fish) there is no snapshot: their
functions and aliases are not bash, and the `bash` tool still gets the
captured variables.

## 8. Refresh

A runtime captures again when:

- it starts, which includes every daemon restart for the `local` runtime;
- its `[shell_environment]` settings change (`runtime/settings_changed`);
- the configuration is reloaded (`config.reload`: `cookie config reload`,
  `/config` → Reload, the web client's Reload). The daemon sends each
  runtime `runtime/shell_environment_refresh` even when nothing changed,
  since editing `~/.bashrc` changes nothing cookie can see.

A new capture applies to processes started afterwards. Running MCP
servers, ACP agents, and plugins keep the environment they started with
until they are restarted, for example with `/mcp` → Restart. A command
already running is not affected.

## 9. Remote runtimes

Each runtime captures on its own machine, with its own `$SHELL` and its
own user's rc files. The daemon sends the settings, not an environment.

Runtime protocol 1.12.0:

- `RuntimeSettings.shell_environment`: the resolved table for that
  runtime (`capture`, `timeout_ms`, `shell`). It defaults to `capture =
  "off"` when absent, so a daemon older than 1.12.0 gets the old behavior
  from a new runtime.
- `runtime/shell_environment_refresh` (daemon → runtime, notification).
- `runtime/shell_environment` (runtime → daemon, notification): the
  outcome of each capture. It carries `captured`, the shell, the number
  of variables, whether a snapshot was written, the duration, and on
  failure the reason (`error`). It never carries variable names or values.

An older runtime ignores the unknown settings field and keeps its own
environment. The daemon knows a runtime's version from its initialize
reply and reports one older than 1.12.0 in diagnostics while its
configuration asks for a capture.

A client that offers its own machine as a runtime (the TUI when remote)
captures like any other runtime. The client already runs in the user's
terminal, so its capture usually changes little.

## 10. Failures and diagnostics

A failed or skipped capture shows in `ConfigStatus.diagnostics`, which
the TUI's `/config`, the web client's settings, and `cookie config
status` already show:

- ``runtime `local`: shell environment capture failed (timed out after 10000 ms); using the runtime's own environment``
- ``runtime `labpc`: shell environment capture failed (the shell exited before printing its environment); using the runtime's own environment``
- ``runtime `labpc`: runtime protocol 1.11.0 does not capture the shell environment; update cookie on its machine``

A diagnostic clears at the next successful capture. Each capture is logged
on the runtime's standard error (shell, variable count, whether a snapshot
was written, duration, or the reason it failed). Values are never logged.

Known causes of failure, and what the user does about them:

- An rc file that `exec`s another shell or program, or attaches to tmux,
  never prints the nonces. Guard that line with
  `[ -t 0 ]` or `[ -z "$COOKIE_SHELL_CAPTURE" ]`. The capture sets
  `COOKIE_SHELL_CAPTURE=1` for exactly this purpose.
- An rc file that waits for input reads end-of-file from `/dev/null` and
  usually carries on.
- An rc file that takes longer than `timeout_ms`: raise the timeout or
  set `capture = "login"`.

No client protocol change is needed. The diagnostics are strings in an
existing field.

## 11. Security

- The capture runs the user's rc files as the user, just as opening a
  terminal does. `capture = "off"` is there for those who do not want
  that.
- The shell environment lives in the runtime's memory only. It is never
  written to disk, never logged, and never sent to the daemon. The
  snapshot file holds functions and aliases, not variables.
- Agents see the user's exported variables, API keys included, through
  `env` in a `bash` command, just as a coding agent in the user's terminal
  does today. This is a change for daemons run as services, whose agents
  saw almost nothing. [Security](../daemon/security.md) gets a sentence
  saying so.

## 12. Implementation

**Phase 1: capture and local use.** A `shell_environment` module in
`coding_runtime` with the capture, the parser, the exclusion list, and a
cache behind an `Arc` that the spawn sites read. Wire it into the `bash`
tool, MCP servers, ACP agents, plugins, and template commands. Add the
`[shell_environment]` and `[runtimes.<id>.shell_environment]`
configuration with its schema, re-capture on settings change and reload,
and the local diagnostics.

**Phase 2: the bash snapshot.** Snapshot writing with options first and
completion functions left out, the private directory, and the `bash`
tool's prefix.

**Phase 3: remote runtimes.** Runtime protocol 1.12.0 (settings field,
refresh, outcome notification), and diagnostics for remote and outdated
runtimes.

**Documentation**, with phase 1:

- [Persistent daemon](../daemon/persistent.md) and
  [Running a runtime as a service](../runtimes/service.md): replace "set
  `Environment=PATH=...` in the unit" with a section on the shell
  environment. `Environment=` remains for overrides.
- The configuration reference: `[shell_environment]`.
- [Tools](../reference/tools.md): `bash` runs with the shell environment
  and the snapshot.
- [Security](../daemon/security.md): section 11's last point.

**Tests**, all with explicit values (none pin defaults):

- The parser: stray output before and after the nonces, a missing nonce,
  values with newlines and `=`, the exclusion list.
- Capture against a scratch `HOME` whose `.bashrc` exports a variable
  below a Debian-style interactivity guard. `"interactive"` sees it,
  `"login"` and `"off"` do not. This is the yunhao-labserver bug as a
  test.
- An rc file that sleeps past `timeout_ms` falls back and produces the
  diagnostic. An rc file that `exec`s `cat` fails cleanly.
- Precedence: captured over inherited, and a process's `env` over
  captured.
- The snapshot: a function with an extended glob is defined after
  sourcing; an alias expands in a `bash` command; a function from a fake
  completion directory is left out; the approval sees the command without
  the prefix.
- A settings change and `runtime/shell_environment_refresh` re-capture.
- Windows: the setting is accepted and ignored.
- The runtime protocol compatibility corpus for 1.12.0.

## 13. Decisions

- **Capture is a configuration setting, on by default** (the user,
  2026-10-28): `[shell_environment] capture`, with `"off"` to disable it.
- **The default capture is an interactive login shell** (the user,
  2026-10-28): `"interactive"`, because typical rc files (Debian's and
  Ubuntu's default `.bashrc`) return early unless interactive, and that is
  where the user's setup lives.
- **The capture inherits the runtime's environment** (the user,
  2026-10-28): an environment activated where the daemon or runtime was
  started (conda, a virtualenv) carries into the capture and into every
  command. This is expected, not a leak to fix. There is no clean-base
  capture and no `passthrough` list.

## 14. Implementation notes

Implemented on 2026-10-28 in one change rather than the three phases of
section 12, since the daemon's own `local` runtime already reaches its
settings over the runtime protocol:

- The capture lives in `cookie_agent_runtime_api::shell_environment`, which
  the `bash` tool, the MCP crate, the ACP host, and the engine's process
  connector (plugins and template commands) all depend on. There is one
  shell environment per process, configured by the runtime the process
  serves: the daemon's in-process `local` runtime configures it at engine
  start, before any plugin starts, so plugins and template commands use
  `local`'s settings.
- Runtime protocol 1.12.0: `RuntimeSettings.shell_environment`,
  `runtime/shell_environment_refresh`, and `runtime/shell_environment`
  (`ShellEnvironmentReport`). The report has no mode field.
- Completion functions are recognised by their source file, from
  `declare -F` under `extdebug`; there is no test of that filter, since it
  needs files under `/usr/share/bash-completion/`.
- Configuration fixtures in tests set `capture = "off"`, so tests never run
  the developer's own shell.


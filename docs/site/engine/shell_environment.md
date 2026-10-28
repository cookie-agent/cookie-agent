# Shell Environment [shell_environment]

Give the processes a runtime starts the environment you have in a new
terminal on that machine (see the
[shell environment spec](../specs/shell-environment.md)). Complete
`config.toml`:

```toml
[shell_environment]
capture = "interactive"
timeout_ms = 10000
shell = ""
```

A daemon run as a service starts with the service manager's environment, and
a runtime started over ssh with sshd's, not your shell's. Your setup usually
lives in your shell's rc files instead: `PATH` entries, conda or micromamba,
nvm, API keys. So each runtime runs your shell once, on its own machine,
captures its environment, and starts every process of its work in it:

- the `bash` tool's commands, background calls included;
- stdio MCP servers;
- ACP agents;
- on the daemon's machine, plugins and the commands of
  [runtime templates](../runtimes/templates.md), such as `ssh`.

The daemon's and the runtime's own processes keep the environment they
started with. A process's configured `env` still applies over the captured
variables.

| Key | Type | Default | Description |
|---|---|---|---|
| `capture` | string | `"interactive"` | `"interactive"` runs `$SHELL -l -i`, which reads what a new terminal reads, including a `.bashrc` that returns early unless interactive, as Debian's and Ubuntu's do. `"login"` runs `$SHELL -l`, which misses such an rc file. `"off"` keeps the runtime's own environment. |
| `timeout_ms` | integer | `10000` | How long a capture may take before the runtime gives up and keeps its own environment. 0 or less waits indefinitely. |
| `shell` | string | `""` | The shell to capture with. Empty: the runtime's `$SHELL`, else `/bin/sh`. |

A runtime's own `[runtimes.<id>.shell_environment]` table, `local` and
runtimes that dial in included, replaces the keys it sets for that runtime
and keeps the others:

```toml
[runtimes.labpc.shell_environment]
capture = "login"
```

An omitted table inherits; an authored top-level table replaces the lower
table and uses defaults for omitted fields. Unknown fields fail; see
[config.toml](configuration.md).

## Functions and aliases

When the shell is bash, the capture also writes a snapshot of its options,
aliases, and functions, and the `bash` tool sources it before each command.
So `micromamba activate`, `conda`, `nvm use`, and your aliases work in a
command as they do at your prompt. Each command is still a fresh shell: an
environment activated in one command is gone in the next, so activate it in
the same command (`micromamba activate ucma && python train.py`). Shells
other than bash give the `bash` tool their variables only.

What was active where the daemon or runtime was started carries over: a
daemon started from a terminal with a conda environment active runs every
command in it.

## When it is captured

A runtime captures when it starts, which for the daemon's `local` runtime is
every daemon start, and again when its settings change and when the
configuration is reloaded (`cookie config reload`, or Reload in a client),
since you may have edited your rc files. A new capture applies to processes
started afterwards; running MCP servers, ACP agents, and plugins keep their
environment until they restart.

## When it fails

A capture that times out, or a shell that exits before printing its
environment, leaves the runtime's own environment, and the configuration
status (`cookie config status`, `/config` in the TUI, the web client's
settings) names the runtime and the reason until a capture succeeds. A
runtime older than cookie with runtime protocol 1.12.0 does not capture, and
the status says so too. Common causes:

- An rc file that `exec`s another shell or attaches to tmux. The capture sets
  `COOKIE_SHELL_CAPTURE=1`, so guard such a line:

  ```sh
  [ -z "$COOKIE_SHELL_CAPTURE" ] && exec tmux new -A -s main
  ```

- An rc file slower than `timeout_ms`: raise the timeout or use
  `capture = "login"`.

The capture runs your rc files with no terminal and nothing on standard
input, in a session of its own; what they leave running in it is ended when
the capture finishes.

Agents can read every exported variable, API keys included, as an agent in
your terminal can. Use `capture = "off"` to keep a runtime's processes on its
own environment.

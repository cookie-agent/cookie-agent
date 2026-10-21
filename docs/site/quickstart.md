# Quickstart

This page takes you through a first session in the terminal UI: connecting a
provider, asking for a change, approving the tools it calls, reading the
transcript, and coming back to the session later. It assumes `cookie` is
[installed](install.md).

## Start in a project

Change to the repository the agent should work on and start cookie:

```sh
cd ~/src/my-project
cookie
```

No configuration file is needed. With no daemon running on this machine,
`cookie` runs one inside the process; the left of the bottom bar says
`standalone`, and the runtime and directory the session will work in follow
it. Sessions you create here work in this directory.

## Connect a provider

When no model is available, the TUI opens in setup mode. Press `/` in the
empty composer, or Ctrl-P, to open the command palette, and choose
`/connect`. Pick a [managed provider](extend/providers.md#managed-providers),
then enter its setup values and credentials. The connection is stored for
your user and shared by every workspace; the credentials are checked when the
provider is first used, not while connecting. To use a model the catalog does
not list, configure a [custom model](extend/providers.md#custom-model-example)
in `config.toml` instead.

`/model` chooses the model and its variant for the next run, and `/agent` the
agent. Without agents of your own, the built-in `default` coding agent runs.

## Ask for something

Type a request in the composer and press Enter; Shift-Enter inserts a
newline. The agent reads files, edits them, and runs commands through its
tools, and the transcript shows each call as it happens. While it works,
Enter sends another message into the running conversation, and Alt-Enter
queues one for after the run (see
[Steering and queueing](clients/tui.md#steering-queueing-and-the-pending-strip)).
Esc twice, or `/cancel`, stops the run.

## Approve tools

The built-in `default` agent reads freely but asks before it writes a file,
runs a command, or delegates, and never reads files such as `.env` or SSH
keys. A session starts in the `auto-approve` permission mode: an approval
classifier answers these asks and passes to you the ones it will not decide.
Click the mode in the bottom bar to cycle it; in `ask` every ask comes to
you.

An approval appears above the status bar in place of the composer, showing
the command or the diff the call will make. Press `y` to allow it once, `a` to
allow it for the whole session tree, `n` to reject it, or `e` to reject it with
a note telling the agent what to do instead. See
[Approvals](clients/tui.md#approvals) and
[Permissions](extend/agents.md#permissions).

## Read the transcript

Click a tool row to expand it: its arguments, then its output, such as a
command's terminal output or an edit's diff with line numbers. The system
prompt, the `AGENTS.md` files the session loaded, and skills appear as
collapsed rows you can open the same way, so everything that went into the
model's context can be read. `/usage` shows the tokens and estimated cost of
the session and of the subagents it started. See
[Transcript details](clients/tui.md#transcript-details).

## Come back later

Quit with `/quit`, or Ctrl-C twice when nothing is running. Sessions are
kept: start `cookie` in the same directory again, open `/sessions`, and choose
the session to continue it. A past message's menu (click it) copies it,
reverts the session to before it, or forks a new session from it. See
[Sessions](sessions/index.md).

## Next steps

- **In a browser.** Run `cookie daemon`, then `cookie web --open`; see
  [Web](clients/web.md#opening-the-page).
- **A daemon that keeps running.** With `cookie daemon` running, every
  `cookie` on this machine opens on it and shares its sessions; see
  [Persistent daemon](daemon/persistent.md).
- **Without the TUI.** `cookie run "…"` runs one prompt for scripts and CI;
  see [Command line](clients/cli.md#headless-runs).
- **Your own agents.** Write agent documents with their own prompts, models,
  and permissions; see [Agents and presets](extend/agents.md).

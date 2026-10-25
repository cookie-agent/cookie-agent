# Quickstart

This page takes you through a first session in the terminal UI: setting
cookie up with a provider and its models, asking for a change, approving the tools it calls, reading the
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

## Set up: a provider, two models, and the starter kit

Cookie is set up once it has a connected model provider and two models:
`default_model`, which agents run on, and `small_model`, which runs the quick
internal jobs (approval reviews and session titles) and the `explore`
subagent. Until then the TUI shows a notice offering **Set up** (or **Not
now**, which it remembers for this machine), and `/onboard` opens the same
panel at any time. It walks four steps:

1. **Provider.** Pick a [managed provider](extend/providers.md#managed-providers)
   and its sign-in: an API key, typed hidden, or a browser login. The
   connection is stored for your user and shared by every workspace. Several
   may be connected.
2. **Default model.** A searchable list of the connected providers' models,
   with their context size; choosing one offers its variants, the model's
   own default preselected.
3. **Small model.** The same list, nothing suggested; its variant picker
   preselects the model's first listed variant, the cheapest. It may be the
   same model as the default.
4. **Starter kit** (optional). A commented `config.toml`, three agents
   (`primary`, which runs sessions, and its subagents `explore` and
   `worker`), downloaded from the release of your cookie version. Each file is marked **new**, **same as
   yours** (skipped), or **differs**: an existing file is never overwritten;
   the kit's version is written beside it as `<name>.new`.

The daemon also keeps cookie's [official skills](extend/skills.md#official-skills)
up to date by itself, such as `customize-cookie-agent` for changing cookie and
`add-cookie-runtime` for adding another machine; they need no step.

**Finish** writes the settings and the kit together, and stays disabled, with
the reason, until a provider and both models are chosen.

The same steps run from a terminal against a running daemon, here or on
another machine (`--url` and `--token`, as for `cookie config`):

```sh
cookie daemon &                 # once; or the persistent daemon service
cookie onboard                  # numbered questions; `/` searches the lists
cookie onboard --diff           # what the kit would change, changing nothing
cookie onboard --yes --default-model anthropic/claude-opus-4-1:high \
               --small-model deepseek/deepseek-chat
```

The web page offers the same as a first-run sheet, and Settings → **Set up
Cookie** reopens it. Whether the daemon is set up comes from its
configuration alone: setting `default_model` and `small_model` in
`config.toml` by hand counts the same, and disconnecting the last provider
makes it ask again. See [`cookie onboard`](clients/cli.md#cookie-onboard),
[default models](engine/configuration.md#default-models), and
[agents](extend/agents.md#models).

To use a model the catalog does not list, configure a
[custom model](extend/providers.md#custom-model-example) in `config.toml`.
`/model` chooses the model and its variant for the next run, and `/agent` the
agent: `primary` once the kit is installed, else the built-in `default` agent,
which runs on `default_model`.

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
keys. A session starts in the `auto-approve` permission mode: the approval
reviewer answers these asks against your task and passes to you, with its
reason, the ones it will not decide.
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

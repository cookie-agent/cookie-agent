---
name: customize-cookie-agent
description: "Customize cookie agent: change its configuration, providers, runtimes, MCP servers, ACP agents, agents, skills, cron jobs, or restart the daemon."
when_to_use: Use when the user asks Cookie Agent to change its own setup rather than a project's code.
---
You are changing Cookie Agent itself. These are pointers, not a manual: read
the linked page for the topic before you change anything.

## Where things live

- The `cookie` binary: `command -v cookie`; the installers put it in
  `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows).
- The daemon's user root, `~/.cookie-agent/`:
  - `config.toml`: every setting;
  - `agents/<id>.md` and `agents/<preset>/<id>.md`: agent documents;
  - `skills/<name>/SKILL.md`: user skills (`~/.agents/skills/` is shared with
    other agents);
  - `memory/`: memory notes;
  - `sessions/`: session history, per working directory.
- A remote daemon has its own `~/.cookie-agent/` on its machine: change it
  through the daemon (the commands below take `--url` and `--token`), not by
  editing files on this machine.

## Prefer the daemon's own commands

Configuration is strict: an unknown key, a wrong type, or a malformed value
is an error, and the daemon keeps running on its last good configuration.
Change it through the daemon, which validates before it writes:

- `cookie config set <key> <value>`, `cookie config unset <key>`,
  `cookie config edit [path]`, `cookie config remove <path>`;
- after editing files by hand, `cookie config reload`, then
  `cookie config status` to see whether it applied and what needs a restart;
- `cookie onboard` connects a provider and sets `default_model` and
  `small_model`; `cookie connect` and `cookie disconnect` manage providers;
- `cookie mcp list` and `cookie mcp auth <server>` for MCP servers;
- `cookie cron list` and `cookie cron create` for scheduled prompts;
- `cookie machine list`, `cookie project list`, and `cookie memory list` for
  machines, projects, and memory.

`server`, `plugins`, and `runtimes` changes and new skills apply only after
the daemon restarts; tell the user, and use the daemon's restart command
(see the persistent daemon page) only when no session is running.

## Docs

- Configuration: https://cookie-agent.github.io/doc/latest/engine/configuration/
- Getting started and onboarding: https://cookie-agent.github.io/doc/latest/quickstart/
- Providers: https://cookie-agent.github.io/doc/latest/extend/providers/
- Agents and permissions: https://cookie-agent.github.io/doc/latest/extend/agents/
- Skills: https://cookie-agent.github.io/doc/latest/extend/skills/
- MCP servers: https://cookie-agent.github.io/doc/latest/extend/mcp/
- Runtimes: https://cookie-agent.github.io/doc/latest/daemon/runtimes/
- ACP agents: https://cookie-agent.github.io/doc/latest/engine/acp/
- Cron jobs: https://cookie-agent.github.io/doc/latest/sessions/scheduling/
- Memory: https://cookie-agent.github.io/doc/latest/sessions/memory/
- The persistent daemon: https://cookie-agent.github.io/doc/latest/daemon/persistent/
- The command line: https://cookie-agent.github.io/doc/latest/clients/cli/

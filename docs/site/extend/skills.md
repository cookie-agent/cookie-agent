# Skill

Skills are reusable instruction bundles loaded once when the runtime that
contributes them starts. A skill is a directory whose name contains lowercase
letters, digits, and single hyphens, with an exact-case `SKILL.md` file inside
it. Other files under the directory are available as scripts, references, or
assets.

## Discovery and precedence

A session's skills come from the [runtimes](../specs/runtimes.md) it is
attached to. The global runtime contributes the user skills:

- `~/.agents/skills/<name>/SKILL.md` (cross-client convention, shared with
  other agents)
- `~/.cookie-agent/skills/<name>/SKILL.md` (cookie-agent-native user skills,
  which win over the shared location)

Each coding runtime contributes the project skills it finds for its own
workspace: `.agents/skills/<name>/SKILL.md`, then
`.cookie-agent/skills/<name>/SKILL.md`, in every directory from the repository
root (the nearest ancestor with `.git`) down to the workspace, deeper
directories winning. The built-in `local` runtime does this for the daemon's
working directory when the daemon starts; another coding runtime does it when
its instance starts.

The user locations resolve from the standard home directory and are the normal
place for reusable personal skills; `~/.agents/skills` is the right home for
skills shared with other agents that follow the cross-client convention.

Same-named skills are replaced whole, never merged: a project skill shadows a
user skill of the same name, and within one directory scope the
cookie-agent-native root wins over `.agents/skills`. When project skills of two
attachments share a name, each is offered as `<attachment>_<name>`, for example
`local_web_deploy` and `labpc_api_deploy`, like colliding tools. Shadowed skills
remain visible in `skills.list`, but cannot be invoked and are not offered by
the TUI. A session without the global runtime has no user skills, and one
without a coding runtime has no project skills. Skill files are not
hot-reloaded; restart the engine after changing them.

## Official skills

Cookie keeps a set of official skills about itself up to date on every
daemon: `customize-cookie-agent`, pointers for an agent asked to change
cookie (its configuration, providers, runtimes, MCP servers, agents, skills,
and cron jobs), and `add-cookie-runtime`, for bringing another machine in as
a runtime over ssh or by dialing in. They come from the
[cookie-agent/skills](https://github.com/cookie-agent/skills) repository,
served at `https://cookie-agent.github.io/skills/`, a source whose URL
carries no version.

- **Where.** `official_skills/<name>/` in the daemon's data root
  (`~/.cookie-agent/official_skills/`), which only the daemon writes. It is
  the lowest skill root: a user or project skill of the same name wins, and
  the official one shows as shadowed.
- **Refresh.** At start and then every 24 hours the daemon reads the
  source's `index.json`, downloads the files whose SHA-256 changed, and
  checks each against the index before it replaces the cached copy. Skills
  the index no longer lists, or whose `requires_cookie` range this version
  does not satisfy, are removed. A failed refresh keeps the cached skills
  and shows as a configuration diagnostic (`cookie config status`,
  `/config`) until a refresh succeeds; an offline daemon keeps working. A
  refresh that changes them reloads the daemon's skills, user skills
  included, for new sessions.
- **Off.** `[skills] official = false` in `config.toml` stops refreshing at
  once and leaves the folder unused from the next start.
- **Commands.** `cookie skills list` shows each skill with where it comes
  from and an official skill's version, `cookie skills update` refreshes
  now, and `cookie skills check <dir>` validates skills you write; see
  [`cookie skills`](../clients/cli.md#cookie-skills). The TUI's skill picker
  and the web page's skill list mark official skills.

Skills grant no tools; the `skill` permission and every tool's own rules
still decide each call.

```toml
[skills]
official = true   # the default
```

`official` is the table's only key; an unknown key fails.

## Authoring

Complete `release-check/SKILL.md`:

```markdown
---
name: release-check
description: Check a release candidate for packaging and changelog problems.
when_to_use: Use before publishing a tagged build.
allowed-tools: Bash(git:*) Bash(jq:*) Read
argument-hint: <tag>
---
Review release $1 from $ARGUMENTS.
Read supporting files from ${COOKIE_SKILL_DIR}.
```

`name` and `description` are required; descriptions are limited to 1024 bytes.
Optional fields are `when_to_use`, `allowed-tools`,
`disable-model-invocation`, `user-invocable`, `model`, `context`,
`argument-hint`, `license`, `compatibility`, and `metadata`. Unknown fields are
errors. `model` must be a valid `provider/model` key and is rejected during
discovery otherwise. `context` accepts only `fork`.

`disable-model-invocation` defaults to false and `user-invocable` defaults to
true. `allowed-tools` is omitted by default (no extra grants); an authored empty
string is invalid. Optional text fields are strings; `model` and `context` are
unset by default, and `metadata` is an empty map of YAML values. Files are
limited to 256 KiB and must have a nonempty body. The declared name must match
the directory name.

Arguments replace `$ARGUMENTS` and positional `$1`, `$2`, and later forms.
`${COOKIE_SKILL_DIR}` expands to the skill directory. Expansion is textual and
does not execute a shell.

## Invocation and permissions

The model uses `skill({"name":"release-check","args":"v1.2.0"})`. The tool is
absent when policy leaves no skill effectively usable, and skills with
`disable-model-invocation: true` are omitted from the model listing. In the TUI, users
choose `/skills` in the [command palette](../clients/tui.md#command-palette), search for a
`user-invocable` skill, press Enter, and type optional arguments such as
`v1.2.0` into the prompt that follows. The highlighted skill's source,
permission effect, and location show under the list. Headless runs use
`cookie run --skill release-check --skill-args v1.2.0 "Check the release"`.

Skill access uses the `skill` permission action and the skill name as its
resource. A denied skill is hidden from the model listing. Headless runs can
grant one skill with `--allowed-tools skill:release-check`.

`allowed-tools` uses the agentskills.io/Claude Code syntax: a space-separated
string of bare `Tool` names or `Tool(pattern)` entries. Patterns may contain
spaces and parentheses; a bare tool means `*`. Tool names are case-insensitive,
and `Edit` maps to the `write` permission just like the builtin edit tool.
Every other name is the permission action itself, lowercased: `Read`, `Bash`,
`Webfetch`, and so on for the built-in tools, or a runtime's own action such
as `Browse(https://docs.rs/*)`. A name that lowercases to no valid action
(see [permissions](agents.md#permissions)) is an error. The older Cookie Agent `action:pattern` list form is not accepted.

A plugin's tools are granted by the actions their plugin declares, like a
runtime's: `Issue_read` grants every `issue_read` call, and
`Issue_read(42)` only the calls on resource `42`.

Claude's `prefix:*` convention is translated to Cookie Agent's full-resource
glob semantics. For example, `Bash(git:*)` creates grants for both `git` and
`git *`, matching `git`, `git status`, and longer git commands. Other inner
patterns pass through unchanged: `Write(src/**)` grants the `src/**` glob.

On load, these entries create turn-scoped allow grants labeled for that skill.
A project skill's grants apply only to the tools of the attachment that
contributed it (and, for `Skill` and `Delegate` grants, to the base layer's
`skill` and `delegate_subagent` tools); a user skill's grants apply to every
runtime.
Grants from every skill loaded during the same turn are merged; loading a
grantless skill does not remove grants from an earlier skill. The complete
collection disappears on the next committed user input. Grants can only upgrade
an `ask` decision to allow, including the ask of a call no rule matches: they
never override a deny, and they do not make a tool visible on their own. A
granted action whose tools a `"*": deny` hides stays hidden. A `model` override applies
to the next model turn only.

Every first rendered load appends `skill_loaded`, including the rendered body,
source and base paths, arguments, and up to ten supporting-file paths. Loading
the identical rendered body again appends `skill_invocation_noted`. Loaded skill
bodies remain in model context across compaction. With `context: fork`, the
exact staged skill payload is persisted in delegation admission. The child gets
the rendered instructions once through its `skill_loaded` event and a synthetic
user prompt starts the run. Forks use the normal delegation approval, queue,
paging, cancellation, recovery, and permission behavior.

# Skills

Skills are reusable instruction bundles loaded once when the engine opens. A
skill is a directory whose name contains lowercase letters, digits, and single
hyphens, with an exact-case `SKILL.md` file inside it. Other files under the
directory are available as scripts, references, or assets.

## Discovery and precedence

Cookie Agent discovers skills from:

- `~/.cookie-agent/skills/<name>/SKILL.md`
- `.cookie-agent/skills/<name>/SKILL.md` from the working directory upward to
  the worktree root

The user location is resolved from the standard home directory and is the normal
place for reusable personal skills.

Project skills override same-named user skills. The shadowed user skill remains
visible in `skills.list` and `/skills` diagnostics, but cannot be invoked. Skill
files are not hot-reloaded; restart the engine after changing them.

## Authoring

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

Arguments replace `$ARGUMENTS` and positional `$1`, `$2`, and later forms.
`${COOKIE_SKILL_DIR}` expands to the skill directory. Expansion is textual and
does not execute a shell.

## Invocation and permissions

The model uses `skill({"name":"release-check","args":"v1.2.0"})`. The tool is
absent when policy leaves no skill effectively usable, and skills with
`disable-model-invocation: true` are omitted from the model listing. Users invoke
a `user-invocable` skill with `/release-check v1.2.0` or headlessly with
`cookie run --skill release-check --skill-args v1.2.0 "Check the release"`.

Skill access uses the `skill` permission action and the skill name as its
resource. A denied skill is hidden from the model listing. Headless runs can
grant one skill with `--allowed-tools skill:release-check`.

`allowed-tools` uses the agentskills.io/Claude Code syntax: a space-separated
string of bare `Tool` names or `Tool(pattern)` entries. Patterns may contain
spaces and parentheses; a bare tool means `*`. Tool names are case-insensitive,
and `Edit` maps to the `write` permission just like the builtin edit tool.
Accepted names are `Read`, `Write`, `Edit`, `Bash`, `Delegate`, `Skill`, and
`Mcp`, and `Plugin`. The older Cookie Agent `action:pattern` list form is not
accepted.

`Plugin` grants the complete plugin permission group. `Plugin(name:*)` grants
both `name` and `name *`, where `name` is a plugin-declared permission prefix;
this is the same prefix expansion used by `Mcp(name:*)`.

Claude's `prefix:*` convention is translated to Cookie Agent's full-resource
glob semantics. For example, `Bash(git:*)` creates grants for both `git` and
`git *`, matching `git`, `git status`, and longer git commands. Other inner
patterns pass through unchanged: `Write(src/**)` grants the `src/**` glob.

On load, these entries create turn-scoped allow grants labeled for that skill.
Grants from every skill loaded during the same turn are merged; loading a
grantless skill does not remove grants from an earlier skill. The complete
collection disappears on the next committed user input, and an agent-policy
deny remains authoritative. A `model` override applies to the next model turn
only.

Every first rendered load appends `skill_loaded`, including the rendered body,
source and base paths, arguments, and up to ten supporting-file paths. Loading
the identical rendered body again appends `skill_invocation_noted`. Loaded skill
bodies remain in model context across compaction. With `context: fork`, the
exact staged skill payload is persisted in delegation admission. The child gets
the rendered instructions once through its `skill_loaded` event and a synthetic
user prompt starts the run. Forks use the normal delegation approval, queue,
paging, cancellation, recovery, and permission behavior.

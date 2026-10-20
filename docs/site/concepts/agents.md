# Agents and presets

An *agent* is a role a model plays: a system prompt, the models it runs on
with their fallbacks, the permissions its tools get, and whether it runs as a
root, as a subagent, or both. Agents are Markdown documents with YAML
frontmatter in `~/.cookie-agent/agents/`, `<id>.md`:

```markdown
---
description: Reviews a change for correctness
mode: subagent
enabled: true
models: [{ model: "openai/gpt-5" }]
permissions:
  read: allow
  bash: ask
---
You review diffs. Report bugs with file and line.
```

Replace the model with one your providers offer. See
[Agent document structure](../extend/agents.md#agent-document-structure) for
every key.

## Roots, subagents, and internal agents

An agent's `mode` says where it may run: `primary` as a root that you talk to,
`subagent` only when another agent delegates to it, `all` for both, or
`internal` for the daemon's own helpers. A root agent's permissions say which
subagents it may delegate to, so a set of agents forms a team: a primary that
plans and delegates, and specialists for exploring, testing, or review.

The daemon also runs three internal agents of its own: `approval`, the
classifier that answers permission asks in the `auto-approve` mode;
`compaction`, which summarizes context; and `title`, which names sessions.
Each can be replaced by an authored `internal` document. See
[Internal agents](../extend/agents.md#internal-agents).

When no authored agent can run as a root, the daemon supplies a built-in
`default` coding agent on the first available model, so a fresh install works
without any documents. See
[Layering and replacement](../extend/agents.md#layering-and-replacement).

## Presets

A *preset* is a named team. Documents directly in `agents/` are shared; a
directory one level down, such as `agents/python/`, is a preset whose
documents replace shared ones of the same ID and add their own. Choosing a
preset (`/preset`, or `cookie run --preset`) switches the whole team for new
runs at once; no preset is selected by default. See
[Agent presets](../extend/agents.md#agent-presets).

## Choosing what runs

A session's selection, its agent, model, variant, and preset, is shared by
every client and changes with `/agent`, `/model`, `/preset`, or the CLI's
`--agent`, `--model`, and `--preset`. When a model fails, the run falls back
along the agent's model chain, and later runs keep starting from the model
that worked. See [Selecting an agent](../extend/agents.md#selecting-an-agent)
and [Providers](../extend/providers.md) for models and variants.

## Context an agent gets

Besides its own prompt, a run's context carries the repository's `AGENTS.md`
files, the skills it may load, sections the tools contribute, and a memory
snapshot; all of it is fixed when the run starts and visible in the
transcript. See [System prompt](../reference/system-prompt.md),
[Skills](../extend/skills.md), and [Memory](memory.md).

Agents are changed live: edit them with `/config` or `cookie config`, and the
next run uses the new documents. See
[config.toml](../engine/configuration.md#changes-while-the-daemon-runs).

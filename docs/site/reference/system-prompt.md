# System Prompt Composition

cookie agent keeps the system prompt deliberately small and inspectable. The
Markdown body of the selected agent document is the base prompt. Parsing removes
only trailing newlines and restores one final newline; frontmatter configures the
agent but is not copied into prompt text.

## Composition order

For a root or delegated run, the engine composes the model request in this order:

1. The selected agent document body becomes `AgentSnapshot.composed_prompt`.
2. If skills are visible to the model, a generated available-skills listing is
   appended to that prompt. The prompt and document fingerprints are updated.
3. `agent_before_start` plugins run in configured order. A plugin can replace the
   composed prompt or append an addendum, subject to the 128 KiB prompt limit.
   Each accepted change recomputes the prompt fingerprint.
4. History assembly puts the final composed prompt in `history[0]` as the sole
   system turn.
5. Root runs may then add a durable [AGENTS.md context](#agentsmd-context-turn)
   user turn. Loaded skill bodies follow as user turns, followed by normal
   session history.

The resulting agent snapshot is frozen in `run_started`. Model retries and
fallbacks reuse that run policy and prompt fingerprint; configuration refreshes
cannot alter an admitted run. Request assembly reuses the first event snapshot
for the first attempt and refreshes session events only when a later attempt
needs current history.

## Fingerprints

The document fingerprint covers the agent identity, strict frontmatter, and
body. The prompt fingerprint covers prompt text only. Skill-listing and plugin
composition update the effective fingerprints before `run_started` is persisted.
Model-attempt events record the prompt fingerprint, and replay validation rejects
attempt attribution that contradicts the frozen run snapshot.

AGENTS.md context is intentionally excluded from the system-prompt fingerprint. It
has its own durable event, provenance, and user-role boundary.

## AGENTS.md context turn

At each root run start, cookie agent discovers applicable `AGENTS.md` files and
stores their bounded content in `agent_md_loaded`. History replays the
latest run's entries as one user turn, with each file delimited as:

```text
<agent_md source="AGENTS.md">
...
</agent_md>
```

Truncated entries include their original byte size. Making this context a turn
rather than system text keeps the stable system cache key independent of normal
repository edits, preserves per-file provenance, and lets compaction pin the
turn alongside loaded skill bodies. The rolling non-system cache breakpoint can
still move when AGENTS.md context changes, which is the intended invalidation.

## Cache breakpoints

For capable Anthropic adaptors, cache strategy is structural rather than text
injection. The engine clears prior markers, then can mark:

- the non-empty system turn at `history[0]`;
- the final emitted tool definition; and
- the last eligible non-system history turn.

AGENTS.md context does not move or rewrite the system breakpoint. Tool schemas are
separate request fields, so tool changes use the tool breakpoint instead of
changing system text.

Anthropic also documents an automatic caching mode. Cookie agent intentionally
uses the structural marker strategy above so system, tool, and rolling-history
boundaries remain deterministic. OpenAI GPT-5.6+ likewise uses implicit or
explicit cache breakpoints. Cookie agent's OpenAI `system` placement marks the
last non-empty text part only when the first turn is an eligible system turn,
while `rolling` considers only the latest user turn and marks its last non-empty
text part. Either placement is omitted when that exact turn has no eligible text;
Cookie agent does not search another turn. Assistant and tool-result content is
never selected because those marker locations are rejected by the provider. See
the [prompt-caching configuration reference](configuration.md#prompt_caching).

## Agent matrix

| Agent type | System prompt | Additional context |
|---|---|---|
| Root | Selected authored agent body, or the concise built-in default coding prompt; optional skill listing and plugin composition | Root-only AGENTS.md context turn, loaded skill bodies, then session history |
| Delegated | Frozen delegated agent body; optional skill listing and plugin composition | No filesystem AGENTS.md context load. Explicit inherited user/assistant text and ordinary child history remain separate turns. |
| Internal | Authored reserved internal agent body when available, otherwise its built-in prompt | No AGENTS.md discovery, skill listing, or plugin prompt interception. Invocation-specific input is supplied separately. |

The built-in `approval`, `compaction`, and `title` prompts are each roughly 100
bytes: one narrow instruction plus an output-format constraint. The synthesized
default coding prompt is similarly concise. Authored internal agents can replace
the built-in backend while retaining the same isolated invocation contract.

## Deliberate omissions

The system prompt does not contain an environment dump, tool-schema prose, MCP
server instructions, filesystem inventories, or generic operational boilerplate.
Tools and MCP capabilities are represented by typed request definitions and
permission policy. Runtime state belongs in events or request fields. The design
is simple yet mighty: keep stable authority in a lean prompt, and put changing,
attributed information in explicit context turns.

See [Agents](../guide/agents.md), [Events](events.md), and
[Configuration](configuration.md).

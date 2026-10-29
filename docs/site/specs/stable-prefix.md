# A stable prompt prefix

Status: **approved** (2026-10-29), being implemented in the phases of
[section 7](#7-phases). Builds on [deferred tools in cookie](deferred-tools-cookie.md).

## 1. Goal and scope

**Tool rounds made the tool list stable on models that defer:** a tool that
joins mid-round is deferred and announced, and an eager tool that leaves
stays declared until the next compaction. A goal set mid-session, a runtime
attached or detached, and a permission change already cost no cache there.
What still moves the cached prefix:

1. **The system prompt is composed again at every run.** Its runtimes block,
   its tool-provider sections (the delegate section lists the subagents),
   and its skill listing change when a runtime is attached, a project is
   changed, an agent or a skill is added, and so on. On Anthropic the order
   is tools, system, messages, so a changed system prompt rewrites the
   cache of the whole conversation.
2. **Attaching a second runtime renames the first one's tools.** Two coding
   attachments both offer `bash`, so both become `<attachment>_bash`. Within
   a round the model's working `bash` departs and the renamed tools arrive
   deferred, to be loaded again.
3. **Models that do not defer have no rounds.** Every change to their tools
   rewrites the cache, a departure as much as an arrival, and so does every
   changed description (the delegate tool's `agent_type` list, the notes
   added once a session has two attachments).
4. **Only MCP and plugin tools are deferred.** The built-in groups used now
   and then (cron, agents, runtimes, goal, memory) cost their full
   definitions, and memory its instructions, in every request.
5. **Loading takes two tools.** The announcements already list every
   loadable tool by name, so a separate `tool_search` that never loads
   adds a tool and often a round trip.

**Decided by the user, 2026-10-29:** do all five. One loader tool,
`tool_search`, both finds and loads. Models that do not defer tools keep
composing the system prompt at every run, as today: their tool list moves
anyway, so freezing their prompt buys nothing.

## 2. One loader tool

**`tool_search` replaces `tool_search` and `tool_load`** (this reverses
decision 12 of the deferred-tools spec). Like Claude Code's tool search, it
loads what it finds:

```text
tool_search(query: string, limit?: integer)
```

- **`select:a,b`** loads the named deferred tools, exactly, as
  `tool_load(names)` did. Names are separated by commas, with optional
  spaces.
- **Keywords** load the best matches: the same matching and ranking as
  before, at most `limit`, 5 when absent, every match when 0 or less.
- **The result** is a load result: `loaded_tools` records what newly
  loaded, the output lists each loaded tool with its group and the first
  sentence of its description, and notes say why a selected name did not
  load (unknown, eager already, already loaded). A keyword query with no
  match loads nothing and says so. The first load of a deferred group still
  carries the group's instructions (deferred tools in cookie, section 7.3).
- **The announcement's closing line** says to load a listed tool with
  `tool_search`, by `select:` and its exact name, or by keywords.
- **Compaction:** the retained messages drop every loader call and its
  result, `tool_search` now as `tool_load` before (section 8.3 of the
  deferred-tools spec). `tool_load` stays a loader name for the history
  that holds its calls; it is no longer offered.
- **History before the change** declares both tools from its round record
  until its next compaction. A call to `tool_load` there is refused as no
  longer available.

## 3. Built-in groups the policy defers

**The deferral policy also picks** the tools of these built-in groups:
`cron`, `agents` (`list_agents`), `runtimes` (`list_runtimes`), `goal`, and
`memory`. Every other built-in group stays eager: `coding`, `delegation`,
`skills`, `messaging`, `artifacts`, `background`.

- **Memory's instructions** (`<tool_instructions provider="builtin.memory">`)
  leave the system prompt with its tools, and arrive with the first load
  (section 7.3 of the deferred-tools spec). The memory turn still shows the
  snapshot, and the announcement shows memory's summary.
- **Goal tools keep their gate:** they are offered only while the root's
  goal is active or paused. Deferred, they never enter a round's eager
  tools, so a goal that ends leaves nothing declared until compaction.
- **The opt-out** is a new top-level table:

  ```toml
  [deferred_tools]
  always_load = ["memory"]
  ```

  `always_load` lists built-in group names the policy would defer, sent up
  front instead. A name that is not one of those groups is a configuration
  error, as is a repeated name. The default is empty.

## 4. Tool rounds for every model

**Every session has tool rounds,** whether its model defers or not. A round
starts at a session's first request and again at each compaction, as
before. On a model that does not defer:

- **The round** records every composed tool as eager; no loader tools.
- **A tool that joins mid-round** is sent up front at once, as today, from
  its live definition. That moves the prefix, which a model without
  deferral cannot avoid.
- **An eager tool that leaves** stays declared from the round record until
  the round ends, and a call to it is refused as no longer available, as on
  a deferring model. A permission change, a failed MCP server, or a
  detached runtime no longer rewrites the cache.
- **A changed definition** of a round tool keeps the round's definition
  until the round ends; calls run against the live tool.

**Switching models keeps the round.** A round started on a model that does
not defer holds every tool eager, so a deferring model after it defers only
the tools that join later. A round started on a deferring model holds fewer
eager tools, so a model that does not defer gets the others up front, as
tools that joined.

## 5. Tool names stay put within a round

**A round tool keeps its published name until the round ends.** Composition
learns the round's names and their sources:

- **`RoundToolDefinition` gains `source: Option<RoundToolSource { label,
  tool }>`,** defaulted: the composition label (the attachment, or the
  provider's own label) and the tool's own name, for every attachment tool.
  Base-layer tools carry none.
- **Composition pins them:** a candidate whose label and tool name match a
  round tool's source takes that published name. Every other candidate is
  named as today, except that one whose name a pin holds takes its
  `<label>_<tool>` form.
- **Example:** a session works with `bash` from `local_web`, and `labpc_api`
  is attached mid-round. `bash` stays `local_web`'s, and `labpc_api`'s
  arrives as `labpc_api_bash`. At the next compaction both are named as
  composition names them without pins: `local_web_bash` and
  `labpc_api_bash`.
- **Pins are read everywhere composition runs** for the session, so the
  names a request publishes, permissions, and dispatch agree.
- A round written before this change carries no sources and pins nothing.

## 6. A frozen system prompt on models that defer

**On a model that defers, the system prompt's changing parts are frozen
for the round,** and their changes are announced in the conversation, as
deferred tools are.

### 6.1 Prompt blocks

The **prompt blocks** are the parts of the system prompt composed from the
session's state:

| Key | Block |
|---|---|
| `runtimes` | The runtimes block: working directory, machine, and project, or the attachments list |
| `tool_instructions:<provider>` | One provider's `<tool_instructions>` section, keyed by the provider label `prompt_groups` uses |
| `skills` | The `<available_skills>` listing |

- **`AgentSnapshot` gains `prompt_blocks: Vec<PromptBlockSpan { key, start,
  len }>`,** defaulted: where each block sits in `composed_prompt`, in
  bytes. Every run records them, whatever its model. A plugin that replaces
  the system prompt clears them; one that appends keeps them.

### 6.2 Reusing the previous run's blocks

**A run admitted on a deferring model,** other than an ACP agent's, reuses
the previous run's blocks when no checkpoint lies after that run's start:

- **Each block the previous run carried** goes into the new prompt with the
  previous run's text, in the usual order, whatever the live block says now.
- **A live block the previous run did not carry** is left out of the prompt
  and announced (section 6.3).
- **Everything else** is composed as today: the agent's body, plugin hooks.
- **Otherwise** (the first run, a checkpoint since the previous run's start,
  a previous run without spans, a model that does not defer) the blocks are
  composed live, as today.

### 6.3 Announcing changes

**A new event, `PromptBlocksAnnounced { blocks: Vec<AnnouncedPromptBlock {
key, text: Option<String> }> }`,** an information event, renders in place
as a user turn:

```text
[cookie: instructions changed]
<instructions_changed>
<block name="runtimes">
This session has several attachments. …
</block>
<block name="tool_instructions:builtin.delegate" removed="true"/>
</instructions_changed>
Each block replaces the part of the system prompt it names; a removed block no longer applies.
```

- **When:** before each request on a deferring model, beside the deferred
  tools' announcement, when the live blocks differ from the blocks the
  model knows.
- **The blocks the model knows** are the run's own blocks, folded with the
  announcements after both the latest checkpoint and the latest run that
  composed its blocks live. A mid-run checkpoint therefore leaves out the
  earlier announcements, and the first request after it announces again.
- **`text: None`** is a removed block.
- **The clients** show it as a model-input row, word for word, through
  `cookie_agent_protocol::model_text`, as they show the deferred tools'
  announcement.
- **An older engine** skips the event; it composes every prompt live
  anyway.

## 7. Phases

Each phase is gated, committed, and pushed on its own:

1. One loader tool (section 2).
2. The built-in groups and `[deferred_tools] always_load` (section 3).
3. Tool rounds for every model (section 4).
4. Stable names (section 5).
5. The frozen system prompt and its announcements, with the clients'
   rows (section 6).

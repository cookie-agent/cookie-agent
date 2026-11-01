# Stale reasoning and a frozen context

Status: **approved** (2026-10-31); phases 1 and 2 implemented (2026-11-01;
oven-sdk 72c6127, client protocol 1.61.0), phase 3 in progress. See
[implementation notes](#implementation-notes). Builds on
[thinking-replay recovery](thinking-replay-recovery.md) and
[a stable prompt prefix](stable-prefix.md), and reverses part of each
(section 2).

## 1. Problem

Anthropic binds each replayed thinking block to the `system` prompt, the
`tools`, and every message before it. Cookie recovers from a refusal by
retrying without the earlier reasoning (`native_replay_cut`), which works,
but costs a refused request, a warning and a diagnostic in the transcript,
and every earlier thinking block for the rest of the session.

**What the session logs on yunhao-labserver show** (2026-10-31): 9 refusals,
all from Anthropic, each costing about one second, every retry succeeding.
8 came on the first request after an idle gap in which a new engine build
had been installed (changed rendering, tool schemas, or prompt text); the
9th came after a summary compaction, whose kept turns still carried thinking
bound to the conversation before it. Runs with no engine change in between
were never refused.

**What a probe showed** (2026-10-31, `claude-haiku-5-5` through the user's
gateway, adaptive thinking): adding an eager tool is refused (*The `tools`
list differs*); adding a `defer_loading` tool, removing one, or changing its
description is accepted. Tool rounds already keep mid-round tool changes
safe on models that defer.

**What still moves the prefix within a session:**

- **The system prompt on models that do not defer** is composed again at
  every run (stable prefix, section 1, decision of 2026-10-29).
- **The `AGENTS.md` turn** is loaded again at every root run and rendered
  from the latest run's events at the top of history.
- **The memory turn** is taken again when the session's machines, project,
  agent, memory rules, or memory access change (memory spec, section 7.2),
  and rendered at the top.
- **A loaded skill** is pinned at the top of history, after the memory turn
  (`model_history.rs`), so loading one moves everything after it.

Each of these rewrites the provider's prompt cache from that point on, on
every provider, and on Anthropic also invalidates every thinking block after
it.

**What Anthropic's documentation adds:**

- With the `thinking-binding-controls-2026-08-01` beta header,
  `thinking.block_binding.prefix_mismatch_behavior: "drop_block"` makes the
  API drop the failing block and every later one and answer 200, listing
  them in a top-level `input_transformations` array (`thinking_dropped`).
  On Sonnet 5.5 and Haiku 5.5 it works only with adaptive thinking. Bedrock
  supports it.
- The same array lists `thinking_mismatch_allowed` blocks: ones that failed
  the check on an account that does not enforce it yet.
- The closing sentence of the refusal, naming what changed, is for people;
  its wording can change.

## 2. Decisions

Decided by the user, 2026-10-31:

1. **Cut stale reasoning before the request,** not only after a refusal.
   This reverses the "no prevention ahead of time" decision of the
   thinking-replay recovery spec. The refusal-and-retry path stays as the
   backstop.
2. **Whether reasoning binds is declared per model,** never decided by an
   engine branch on the provider. First-party Anthropic endpoints bind;
   Anthropic-compatible ones (Kimi, GLM, MiniMax) do not.
3. **Stale reasoning is found by a prefix fingerprint per turn,** which also
   catches engine upgrades.
4. **Freeze the context for every model,** not only models that defer: the
   system prompt, the `AGENTS.md` turn, and the memory turn. No rescanning
   between runs. They are composed again only at refresh points (section 6).
   This reverses the stable prefix decision that models without deferral
   keep composing the prompt at every run.
5. **Eager tool definitions change only at a refresh point,** which a
   changed parameter schema of an eager tool causes (an engine build or a
   runtime changed it); a changed description alone waits for the next
   one. A refresh composes the system prompt, the `AGENTS.md` turn, and the
   memory turn again together. A change of the run's agent is also a
   refresh point; a change of the session's project is not, and waits for
   compaction.
6. **A machine attached mid-session** adds its `AGENTS.md` and its machine
   memories once, as a message, without rescanning anything else.
7. **Edits wait for compaction.** No manual refresh command: an edited
   `AGENTS.md`, agent document, or memory reaches an open session at its
   next compaction or refresh point.
8. **Prompt block changes keep being announced,** now on every model.
9. **On a model that does not defer, a joining tool is a refresh point.**
10. **`drop_block` is sent** to models that bind (section 5).
11. **One quiet marker** replaces the warning and the diagnostic when
    reasoning is cut, except right after a compaction or a refresh, whose
    rows stand for it (option A: reasoning at or before the mark is never
    sent again; reasoning after it still is).
12. **A refresh shows as a block in both clients,** saying why it happened
    and which parts of the context were composed again (section 7.2). There
    is no separate detection of cache misses from usage.

## 3. Which models bind reasoning

- **oven-sdk:** `ReplayDeclaration` gains `reasoning_binding:
  ReasoningBinding`, `None` or `Prefix`.
- **cookie:** the model descriptor gains `reasoning_binding`, defaulted from
  the adapter:
  - `Prefix` for Claude through Anthropic's own endpoints: the Anthropic and
    Anthropic-on-AWS protocols, Bedrock, and Vertex;
  - `None` for every other adapter, including Anthropic-compatible
    endpoints (`AnthropicCompatibleModel`, `MiniMaxModel`).

  A model entry can override it: `reasoning_binding = "prefix"` or
  `"none"`. A wrong `Prefix` only drops reasoning at changes that would have
  been accepted; a wrong `None` costs one refused request, which the retry
  recovers.
- **The refusal becomes a typed error.** oven-sdk classifies the 400 whose
  message says the block *is bound to a different conversation* as
  `ModelErrorKind::ReasoningPrefixMismatch`, and the engine matches the
  kind, not the text. A signature that failed to verify on its own (the
  message stops after *Invalid `signature` in `thinking` block*) stays an
  ordinary invalid request.

## 4. Cutting stale reasoning before the request

**Every committed turn records the prefix it was made in.**
`ModelTurnCommitted` gains `prefix_fingerprint: Option<String>`, defaulted:
the SHA-256 of the request content before the turn, as the engine built it.
That is the system prompt, the eager tools, and every earlier message,
reasoning included as it was sent. Cache markers and request options are
left out. Every model records it; only models that bind use it.

**Before each request on a model that binds,** context building computes the
same fingerprint at each replayed assistant turn that still carries
reasoning, in one pass over the history:

- **Every turn matches:** the request goes out as built.
- **Any turn differs:** the engine appends `native_replay_cut { through_seq
  }` through the last committed turn and builds again, as after a refusal
  today. Every later request leaves out that reasoning.
- **A turn without a fingerprint** (written before this change) is not
  judged; the refusal path covers it.

This one rule covers a compaction (the kept turns' prefix starts with the
summary now), a refresh point, a changed tool, a cut, and an engine build
that renders history differently. A change in oven-sdk's wire encoding is
invisible to it; the refusal-and-retry path stays for that.

The cut stays a leading run, which the API accepts: thinking made after it
was made without the reasoning before it, so the two stay consistent, and a
history that changes back later does not bring the cut reasoning back.

## 5. `drop_block` and what the provider dropped

- **oven-sdk sends** `thinking.block_binding.prefix_mismatch_behavior:
  "drop_block"` and the `thinking-binding-controls-2026-08-01` beta value
  when the model binds and thinking is adaptive. With any other thinking
  setting it sends neither, as the API requires on Sonnet 5.5 and Haiku 5.5.
- **oven-sdk reads `input_transformations`** and reports its entries on the
  turn's finish, typed: `thinking_dropped` and `thinking_mismatch_allowed`,
  each with its path.
- **A dropped block the engine did not cut** means the provider saw less
  than cookie's history says. The engine appends `native_replay_cut` through
  the turn before the request, so later requests and the transcript match
  what the model saw.
- **`thinking_mismatch_allowed`** changes nothing the model sees. The engine
  records a diagnostic (`reasoning_prefix_drift`) with the count, shown where
  diagnostics are shown: it finds prefix drift before an account starts
  enforcing the check.
- **The user's gateway** strips `thinking.block_binding` today, so this takes
  effect there only once the gateway forwards it. Sections 3 and 4 and the
  retry cover it until then.

## 6. A frozen context for every model

### 6.1 Refresh points

A **refresh point** composes the context afresh: a new tool round, the
system prompt composed live, `AGENTS.md` loaded again from every coding
attachment, and a new memory snapshot. The refresh points are:

- **the session's first request;**
- **a compaction,** as today;
- **an eager tool change,** checked before each request:
  - the live parameter schema of an eager tool differs from the round's
    (an engine build or a runtime changed it). A changed description alone
    is not one: the round's description stays until the next refresh
    point, and calls run against the live tool, as today;
  - a tool joins that the model cannot defer: every joining tool on a model
    that does not defer, and a tool the policy keeps eager (`always_load`);
- **a change of the run's agent,** since its body is the system prompt.

A change of the session's project is not a refresh point (section 6.2).

An eager tool that leaves still stays declared until the next refresh point,
and a call to it is refused as no longer available (stable prefix,
section 4).

**After a refresh point** the model sees the new prefix only:
announcements, in-place additions (section 6.3), and in-place skills
(section 6.4) from before it are folded into the new system prompt and the
top turns, as after a compaction.

**A refresh records `context_refreshed { reason, changed }`** before the
request, an information event:

- **`reason`:** `engine_updated` (an eager tool changed and the engine build
  differs from the one that started the round, which `ToolRoundStarted`
  gains as `engine_build`, defaulted: `release::RELEASE_TAG`, or
  `release::GIT_HASH` for a source build), `tools_changed` (an eager tool
  changed or joined otherwise), or `agent_changed`.
- **`changed`:** what the refresh composed differently from the context it
  replaces: `system_prompt: bool`, `tools: { added, removed, changed }` by
  name, `agent_md: bool`, and `memory: bool`. A part that came out the same
  is `false` or empty.

The session's first request and a compaction record no `context_refreshed`;
the compaction has its own row.

### 6.2 Between refresh points

- **The system prompt** is the previous run's, byte for byte, on every model
  except an ACP agent: the agent's body, its blocks, and plugin
  composition. The system-prompt hooks of plugins run at refresh points
  only; message injections run as today.
- **Prompt block changes** (`runtimes`, `tool_instructions`, `skills`) are
  announced as today (`prompt_blocks_announced`), now on every model.
- **The `AGENTS.md` turn** is not reloaded. A root run no longer appends
  `agent_md_loaded` unless it is at a refresh point.
- **The memory turn** is not retaken. Section 7.2 of the memory spec keeps
  only its first two triggers, and a refresh point.
- **Edits** to `AGENTS.md`, agent documents, and memory reach the session at
  its next compaction or refresh point.
- **A change of the session's project** is announced through the `runtimes`
  block, which names the project. The new project's `AGENTS.md` and project
  memory arrive at the next compaction or refresh point.

### 6.3 A machine attached mid-session

When a runtime is attached between refresh points:

- **its `AGENTS.md`** is loaded once, from that attachment only, and recorded
  as `agent_md_loaded` with `in_place: true`;
- **its machine memories,** when the session has memory, are taken once for
  that machine only, and recorded as `memory_loaded` with `in_place: true`.

Both render where they were recorded, as user turns in their usual form, so
nothing before them moves. A detached machine's `AGENTS.md` and memories
stay in the context until the next refresh point; the runtimes announcement
already says the machine is gone.

### 6.4 Skills where they were loaded

**A skill loaded between refresh points renders in place,** as a user turn
right after the call or command that loaded it. At the next refresh point or
compaction it joins the pinned skills after the memory turn, as today.

## 7. In the transcript

### 7.1 The cut marker

- **An attempt abandoned for a reasoning mismatch** shows no row.
- **`native_replay_cut` gains `cause`:** `refused` (the default, for logs
  written before), `prefix_changed`, `provider_dropped`, `compaction` (the
  prefix change is explained by a checkpoint after the cut turns), or
  `refreshed` (explained by a `context_refreshed` after them).
- **The clients show one quiet row** for `refused`, `prefix_changed`, and
  `provider_dropped`: *Earlier thinking is no longer sent to the model: the
  conversation before it changed.* After a compaction or a refresh, that
  row stands for it.
- **The `native_replay_cut` diagnostic** is no longer written.

### 7.2 The refresh block

**`context_refreshed` shows as a block** in both the TUI and the web client,
where it happened: one line with the reason, and what was composed again.

```text
Context refreshed: cookie was updated
  system prompt · tools (memory_write changed) · AGENTS.md · memory
```

- **The reason** reads *cookie was updated*, *tools changed*, or *switched
  to agent `<name>`*.
- **The parts** list only what changed; tools name what was added, removed,
  or changed, cut to the first few names with a count of the rest.
- **It says** that the conversation's prompt cache is written again from the
  start and earlier thinking is no longer sent, so neither needs a row of
  its own (section 7.1).
- **The parts are expandable** to the text the model now reads, through
  `cookie_agent_protocol::model_text`, as the announcements are (model
  input transparency).

## 8. Protocols and history

- **Client protocol (minor):** `ModelTurnCommitted.prefix_fingerprint`,
  `NativeReplayCut.cause`, `in_place` on `agent_md_loaded` and
  `memory_loaded`, `ToolRoundStarted.engine_build`, and the new information
  event `context_refreshed`, all defaulted or skipped by older readers.
  History format unchanged.
- **Plugin protocol:** no wire change; the system-prompt hook's
  documentation says it runs at refresh points.
- **oven-sdk:** `ReasoningBinding` on `ReplayDeclaration`,
  `ModelErrorKind::ReasoningPrefixMismatch`, `block_binding` on the
  Anthropic, Bedrock, and Vertex Claude adapters, and `input_transformations`
  on the finish; a version bump, published before a stable cookie release.
- **Older sessions:** a session opened by this engine freezes from its next
  refresh point. Its turns have no fingerprints and rely on the refusal path
  until a cut or compaction moves past them.

## 9. Phases

Each phase is gated, committed, and pushed on its own:

1. **oven-sdk:** the declaration, the typed error, `block_binding`, and
   `input_transformations`.
2. **Binding and fingerprints:** the descriptor field and its override, the
   typed error in the retry path, fingerprints, the cut before the request,
   cuts for provider drops, the drift diagnostic, `cause`, and the cut
   marker in both clients (sections 3 to 5 and 7.1).
3. **The frozen context:** refresh points, the frozen system prompt for every
   model, `AGENTS.md` and memory at refresh points only, in-place additions
   on attach, skills in place, and the refresh block in both clients
   (sections 6 and 7.2).

## 10. Tests

- A model with `None` and a compatible endpoint never get cuts or
  `block_binding`; a `Prefix` model's override to `none` does not either.
- A scripted Anthropic endpoint: a kept turn after a summary checkpoint is
  cut before the request, with no refused request and no marker row.
- A changed eager tool parameter schema between runs: one refresh point,
  one cut, one refresh block naming the tool, and no separate cut row.
- A changed eager tool description alone: no refresh point; the request
  declares the round's description.
- A fingerprint mismatch from a changed earlier message: a cut through the
  last committed turn; reasoning made after it is replayed on later
  requests.
- A history whose prefix changes back after a cut does not replay the cut
  reasoning.
- `input_transformations` with `thinking_dropped` produces a cut with cause
  `provider_dropped`; `thinking_mismatch_allowed` produces only the
  diagnostic.
- `block_binding` is sent with adaptive thinking only, and never without the
  beta value.
- Between refresh points: an edited `AGENTS.md`, an edited agent document,
  and a memory write leave the request's prefix byte-identical on a model
  that does not defer.
- Attaching a machine appends its `AGENTS.md` and memories in place; the
  bytes before them are unchanged.
- A skill loaded mid-round renders in place, and moves to the pinned area at
  the next compaction.
- An agent change is a refresh point; a project change is not, and keeps
  the earlier `AGENTS.md` and memory turns until the next compaction.
- The refresh block: `engine_updated` when the round's `engine_build`
  differs, `tools_changed` when it is the same; the parts list only what
  changed; a refresh that changed nothing but the tools lists only the
  tools.

## 11. Documents to update on landing

The thinking-replay recovery spec (superseded parts), the stable prefix spec
(models without deferral now freeze), the memory spec (section 7.2 and 7.3),
the [system prompt reference](../reference/system-prompt.md), the
[events reference](../reference/events.md), the protocol changelog, and the
spec index.

## Implementation notes

- **Phase 1** (oven-sdk d9b8960 and 72c6127, core 0.10.0, anthropic
  0.11.0). `block_binding` is sent by the Anthropic Messages adapter (direct
  Anthropic and Claude Platform on AWS, and an Anthropic-compatible
  endpoint whose model is declared `prefix`), not by Bedrock: Bedrock goes
  through Converse, where the field would need
  `additionalModelRequestFields`. Bedrock classifies the refusal as
  `ReasoningPrefixMismatch`, and its models bind, so the cut before the
  request and the retry cover it. Google Vertex in oven-sdk serves Gemini
  only.
- **Phase 2.** Where it differs from sections 3 to 7.1:
  - The automatic rule keys on the model, not only the adapter: a
    reasoning model whose ID starts `claude-`, or contains
    `anthropic.claude-` on Bedrock, on an Anthropic, Anthropic-compatible,
    or Bedrock adaptor. A Claude model through a gateway binds; Kimi on the
    Anthropic adapter does not.
  - The refusal kind is recorded in `model_error.kind` as
    `invalid_request`, as before: a new value of the protocol's closed
    `ModelErrorKind` would break older readers.
  - Fingerprints are recorded only on models that bind. A turn of another
    model carries reasoning no binding model replays (it is foreign), so
    its fingerprint would never be read.
  - The fingerprint is taken on the request the context builds, before
    media fitting and plugin interception: what matters is that recording
    and checking take it at the same point.
  - A `provider_dropped` cut covers the new turn too: its reasoning was
    made without the blocks the provider dropped, which a leading cut
    cannot express, so it would be dropped on the next request anyway.
  - A cut turn's replay decision is `reconstructed_normalized_history`, a
    debug row, instead of a discard warning on every later run.


# Deferred tool loading in oven-sdk

Status: **draft** (2026-10-27); approach and every decision in section 9
made 2026-10-27, eight cookie-side decisions recorded in section 10. The
OpenAI encoding was verified live on 2026-10-27, and the Kimi encoding
against Moonshot's own client, Kimi Code (section 5.4). Covers oven-sdk only; how cookie
decides which tools to defer, and its search tool, are a separate spec
(section 10).

## 1. Goal and scope

**Every tool a request declares sits in the model's context, and in its
cached prefix.** Adding or removing one (an MCP server connecting, a
permission change) rewrites the cache from the tools onward. On an account
that enforces Anthropic's thinking binding it is also a 400, which cookie
recovers from with `native_replay_cut` (thinking-replay-recovery spec).

**Several providers can now hold a tool back until the conversation loads
it.** The model sees a deferred tool only from the point where it is loaded,
and the prefix stays stable. This spec gives oven-sdk one provider-neutral
way to say "this tool is deferred" and "this tool result loads these tools".
Each adapter encodes those natively, or falls back to sending every tool up
front.

**The caller owns the search.** The loader is an ordinary function tool the
caller implements, for example cookie's own `tool_search`. Oven only encodes
which tools a result loads. Hosted search (the provider runs it) is out of
scope for v1 (section 9, decision 3).

**Out of scope:** hosted search, choosing which tools to defer, the loader
tool itself, and token thresholds. Those belong to the caller.

## 2. Provider support (checked 2026-10-27)

| Provider surface | Native mechanism | Verified |
|---|---|---|
| Anthropic Messages, first party and Claude Platform on AWS (Claude 4.5 and later, Haiku included) | `defer_loading: true` on the tool; a `tool_result` whose content is `tool_reference` blocks loads them | Docs; probed through the user's gateway (section 3) |
| OpenAI Responses, official and Azure | Deferred tools left out of `tools`, then an `additional_tools` input item (`role: developer`) at the load point (decision 2). Hosted and client `tool_search` need gpt-5.4 or later; the item worked on every model probed | Documented by OpenAI and Azure; probed 2026-10-27 on `api.openai.com` (gpt-4.1 through gpt-5.6, section 5.3); calls to loaded tools carry a `namespace` that replay must keep |
| Kimi Chat Completions: the Moonshot platform (`kimi-k3`, `kimi-k3.1-pro`) and the Kimi Code endpoint (`kimi-for-coding` on `api.kimi.com/coding/v1`, where `/models` declares support) | Deferred tools left out of `tools`, then a `{"role": "system", "tools": [...]}` message with no `content` appended at the load point | Kimi docs, and Moonshot's Kimi Code client, which uses it (section 5.4); not probed (the gateway's Kimi Code plan has lapsed). K2.x models fail with `tokenization failed` |
| Anthropic-compatible endpoints (DeepSeek, MiniMax, Kimi Code) | Undocumented. DeepSeek v4-flash honored `defer_loading` and `tool_reference` in probes; v4-pro ignored the flag (no error); server-side search is a 400 | Probed. Cookie tries deferral on them anyway and falls back on rejection (section 7) |
| DeepSeek Responses and Chat Completions | Responses ignores `defer_loading`; Chat rejects Kimi's system message | Probed |
| Bedrock Converse, OpenAI Chat, Azure Chat, Gemini, Vertex Gemini, Cohere | None (Bedrock has it only on InvokeModel, which oven does not use) | Docs |

## 3. Anthropic behavior that shapes the design

Probed on 2026-10-27 against `claude-sonnet-5-5` through the gateway:

1. **A `tool_result` cannot mix text and `tool_reference` blocks.** The API
   answers 400: "Tool definitions/code execution functions cannot be mixed
   with other content". A result that loads tools carries only references
   (section 5.2).
2. **Adding a deferred tool keeps the cache.** Two requests that differ only
   by one more deferred tool both read the full 21,149-token prefix from the
   cache. Adding an eager tool instead rewrote all of it.
3. **Adding a deferred tool passes thinking binding.** Replaying a thinking
   block after adding a deferred tool succeeded with the same input tokens.
   Adding an eager tool gave the binding 400 ("The block is bound to a
   different conversation").
4. **Every tool must stay in `tools`.** A `tool_reference` names a tool the
   request declares; an unknown name is a 400 ("Tool reference ... not found").
   At least one tool must not be deferred, and a deferred tool cannot carry
   `cache_control` (both 400s, per the docs).

Points 2 and 3 are why deferral helps cookie beyond saving tokens: MCP tools
that connect or disconnect mid-session stop costing the cache and stop
causing `native_replay_cut`, as long as they are deferred.

## 4. Core types (`oven-sdk`)

### 4.1 Deferred tools

`ToolDefinition` (`crates/oven-sdk/src/lib.rs`) gains:

```rust
/// When the model sees a declared tool.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLoading {
    /// In the model's context from the start of the request.
    #[default]
    Eager,
    /// Hidden until a `ToolContent::LoadTools` result loads it.
    Deferred,
}

pub struct ToolDefinition {
    // ...existing fields...
    #[serde(default)]
    pub loading: ToolLoading,
}
```

Add a `ToolDefinition::deferred(self) -> Self` builder. `Request.tools` keeps
every tool, deferred ones included, in the caller's order.

### 4.2 Loading tools from a result

`ToolContent` gains a variant (decision 1):

```rust
pub enum ToolContent {
    // ...existing variants...
    /// This result loads deferred tools: the named tools become callable
    /// from this point on.
    LoadTools {
        /// Names of deferred tools declared in `Request.tools`.
        names: Vec<String>,
        /// Text for adapters that cannot load natively, and for surfaces
        /// that need tool-message content. Optional.
        text: Option<String>,
    },
}
```

A tool is **loaded** from the first `LoadTools` result that names it.

### 4.3 Capability

`ModelCapabilities` gains a field modelled on `CompactionCapability`:

```rust
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredToolsCapability {
    /// The adapter encodes deferred tools and loads natively.
    Native,
    /// Deferred tools are sent up front (section 6).
    #[default]
    Unsupported,
}
```

- **Declared by the caller per model,** like every other capability.
  `#[serde(default)]` keeps stored declarations readable.
- **Validation:** `Native` requires `TOOL_CALLING`.
- **An adapter that has no native encoding** (section 5) rejects `Native` when
  the model is built, the same way an unsupported compaction surface is
  rejected.

## 5. Adapter encodings (capability `Native`)

### 5.1 Validation shared by every adapter

Run once in core before encoding, so every adapter agrees:

- **Some tool is eager.** A request whose declared tools are all deferred is
  `InvalidRequest`. The loader must stay visible, and Anthropic requires one.
- **`ToolChoice::Tool(name)`** may name a deferred tool only once it is
  loaded; otherwise `InvalidRequest`.
- **`LoadTools` names that are not declared** are dropped from that result,
  with a warning (decision 5).
- **`LoadTools` names declared eager** are dropped without a warning: the
  tool is already visible. This is the expected case after a compaction that
  kept a load in its recent tail and made the tool eager (section 10,
  decision 8).
- **A result left with no new names** renders as its `text` (5.2), or as
  `Loaded no new tools.` when there is none.
- **Loading a tool twice** renders only the first load. Later loads of the
  same tool count as already loaded.

### 5.2 Anthropic Messages (`oven-sdk-anthropic`)

For the `Anthropic` and `AnthropicAws` protocols, and for compatible
endpoints and MiniMax when the model declares `Native`:

- **Tools:** each deferred tool is encoded with `"defer_loading": true`, in
  the caller's order (`request.rs`, the `Tool` list built in
  `encode_request`).
- **Cache markers:** a `cache_control` the caller put on a deferred tool
  moves to the last eager tool before it. Deferred tools are outside the
  cached prefix (section 3, point 2), so the cached range is the same. With
  no eager tool before it, the marker moves to the first eager tool after
  it. `cache_plan` does this; `validate_cache_plan` keeps its marker-count
  limit after the move.
- **Results:** a `LoadTools` result with new names encodes as a
  `tool_result` whose content is only `{"type": "tool_reference",
  "tool_name": ...}` blocks (`tool_result_block`). Its `text` is not sent
  (section 3, point 1). A result with no new names is a text `tool_result`.
- **Stream and replay:** unchanged. The loader is an ordinary tool, so no
  server blocks appear.

### 5.3 OpenAI Responses (`oven-sdk-openai`, `oven-sdk-azure`, `oven-sdk-open-responses`)

Uses an `additional_tools` input item (decision 2):

- **Tools:** deferred tools are left out of the request's `tools`.
- **Results:** a `LoadTools` result encodes as its `function_call_output`,
  with `text` or `Loaded tools: a, b` as the output. After the last output
  item of that tool message comes one input item:
  `{"type": "additional_tools", "role": "developer", "tools": [<full
  definitions of the newly loaded tools>]}`. Definitions use the same
  function encoding as top-level tools (`responses/request.rs`, `tools` and
  `encode_input`).
- **Documented:** OpenAI's and Azure's tool search guides both describe the
  item ("Add tools at a specific point in the input"). Its tools become
  available only after it, and a later request must keep it at the same
  position. The official `openai-python` SDK, generated from the API schema,
  types it as an input item (`role` is always `developer`, plus `tools` and
  an optional `id`). It is also an output and stored item, so the Responses
  stream parser must accept one if a response carries it. Neither guide
  requires `tool_search` or `defer_loading` with it.
- **Probed on 2026-10-27 against `api.openai.com`** with `store: false`, on
  gpt-5.6-sol, gpt-5.5, gpt-5.4, gpt-5.4-mini, gpt-5.2, gpt-5, gpt-4.1, and
  gpt-4.1-mini:
  - **Hidden before the item:** with the tool absent from `tools`, the model
    lists only the eager tools.
  - **Callable after the item:** the model calls the loaded tool and lists it
    among its tools. Without the item, the same history gets a call to
    `search_tools` or `noop`, never the tool.
  - **Every model tried accepts it,** older ones included, unlike tool
    search itself.
  - **The cache holds:** after items are appended past the item, the prefix
    up to and including it is read from the cache (6,656 of 7,283 tokens on
    gpt-5.5, 7,168 on gpt-5.4-mini). Adding the tool to `tools` instead was
    a full miss on its first request.
  - **Not probed:** Azure, and compatible gateways.
- **Loaded tools live in their own namespace.** A call to a tool the item
  loaded comes back as a `function_call` item with `"namespace":
  "<tool name>"` (models list it as `get_weather.get_weather`). Replaying
  that call without the field is a 400: "Missing namespace for
  function_call 'get_weather'. It does not exist in the default namespace.
  Round-trip the model's function_call item with its namespace field
  included." Oven ignores `namespace` today, so three places change:
  - **Stream:** `responses/state.rs` keeps a `function_call` item's
    `namespace` in the `ToolCallPart` metadata (`openai.namespace`), as it
    keeps `provider_item_id`.
  - **Native replay:** `function_semantics` in
    `crates/oven-sdk/src/replay/openai_responses.rs` accepts `namespace` as
    an optional key. Today the extra key fails `exact_keys`, so the artifact
    is rejected and the turn falls back to normalized encoding.
  - **Normalized encoding:** the `FunctionCall` item in
    `responses/request.rs` sends `namespace` only for a tool that an earlier
    `LoadTools` result in the same request loaded. It uses the metadata's
    value, or the tool's name without metadata (a history from another
    provider, or one written before this change), the value every probe
    returned. A call to a tool that is eager in the request gets no
    `namespace`, even when its metadata has one. The API accepts the field on
    an eager tool too, but a history without it is what compacts cleanly
    (section 10, decision 8).

  The same applies to the Azure and compatible Responses adapters when they
  declare `Native`.
- **Native replay otherwise:** unchanged. The item is derived from history
  on every request, so replay stays deterministic.

### 5.4 OpenAI-compatible Chat, Kimi dialect (`oven-sdk-openai`, `chat/`)

- **Opt-in:** `OpenAiCompatibleChatSettings` gains `deferred_tools:
  CompatibleChatDeferredTools` with `Unsupported` (default) and
  `SystemMessage`. `Native` on a model whose settings say `Unsupported` is
  rejected at build.
- **Tools:** deferred tools are left out of `tools`.
- **Results:** a `LoadTools` result encodes as its `tool` message, with
  `text` or `Loaded tools: a, b` as the content (`tool_result_message`).
  After the last tool message of that group comes `{"role": "system",
  "tools": [<full definitions of the newly loaded tools>]}`, with no
  `content` key (Kimi returns 400 when one is present). It uses `system`
  whatever `SystemMessageRole` says.
- **Cache:** Kimi says appending keeps the prefix cache and editing an
  earlier load point does not ("append, never insert"). The encoding is
  derived from history, so a load point never moves unless the caller edits
  history.
- **Confirmed in Moonshot's own client** (`MoonshotAI/kimi-code` at 68a2838,
  2026-10-27):
  - **The same wire shape.** kosong's `kimi` provider
    (`packages/kosong/src/providers/kimi.ts`, `convertMessage`) sends a
    `system` message whose only field is `tools`, on Chat Completions. Each
    definition goes through the same conversion as top-level `tools`,
    including Kimi's schema normalization (`normalizeKimiToolSchema`).
  - **The same placement.** Its `select_tools` loader queues the definitions
    it loads and appends them as that message before the next request,
    right after the loader's result. It also leaves loaded tools out of the
    request's `tools` (`wireTools`).
  - **The same fallback.** For a model without the capability, it strips
    the tools messages from the history (`stripDynamicToolContext`).
- **Which models.** Kimi Code enables the feature only on models whose
  capability `dynamically_loaded_tools` is set:
  - **On the Kimi Code endpoint, the server declares it.** Each model in
    `GET {base}/models` reports `"tools": {"dynamic_load": true}` when it
    supports loading (`packages/oauth/src/managed-kimi-code.ts`). So
    `kimi-for-coding` supports it whenever that endpoint says so, over Chat
    Completions.
  - **Elsewhere,** a user declares it, or models.dev does. Kimi Code reads a
    `dynamically_loaded_tools` field from models.dev, but no model there
    carries it as of 2026-10-27.
- **Cookie reaches `kimi-for-coding` through its Anthropic adapter** today
  (the models.dev family is `anthropic`). That endpoint's support for
  `defer_loading` is unknown, and Kimi Code does not use it, so the model
  stays `Unsupported` there. Deferral for `kimi-for-coding` needs the
  provider on the OpenAI-compatible Chat adapter with `SystemMessage`.
- **A live probe** (`/tmp/defer-probe/probe.py`, `kimi` mode) is still worth
  running once the gateway's Kimi Code plan works again. Kimi Code's use of
  the format is evidence enough to build it.

## 6. Fallback (capability `Unsupported`)

- **Deferred tools are sent up front** like eager ones, with one request
  warning: `deferred tools sent up front: <adapter> has no native loading`
  (decision 4).
- **`LoadTools` results render as text:** `text`, or `Loaded tools: a, b`.

The fallback means a history stays provider-neutral. A session can move from
Claude to Gemini and back, and each request encodes the same history its own
way.

Cookie never defers a tool for an `Unsupported` model (section 10), so the
first rule is a safety net for other callers. The second one still matters
to cookie: after a switch from Claude to such a model, the earlier
`tool_search` results in the history render as text.

## 7. Errors

- **Oven exports the wire keywords** of each native encoding as constants:
  - `defer_loading`, `tool_reference`;
  - `additional_tools`;
  - for Kimi, the text `cannot be used with content` and `tokenization
    failed`.
- **The adapter's error classification** maps a 400 or 422 whose body names
  one of them to `ModelErrorKind::Unsupported`.
- **The caller retries without deferral.** It can mark the model
  `Unsupported` and retry, as cookie does for native compaction windows, and
  as Claude Code does when a gateway rejects tool search.

### 7.1 Trying deferral where support is unknown

Decided 2026-10-27, following Claude Code: on an endpoint that may or may
not support deferral, try it, and fall back to whatever works.

- **Only the Anthropic encoding can be tried blind.** Every deferred tool's
  full definition is in `tools` (section 5.2), so an endpoint has three
  possible outcomes:
  - it honors `defer_loading`, and deferral works;
  - it ignores the flag, and the model simply sees the tools up front (as
    DeepSeek v4-pro did): harmless;
  - it rejects the request, and the retry below recovers.
- **The inline encodings cannot.** OpenAI's `additional_tools` item and
  Kimi's tools message leave deferred tools out of `tools` (sections 5.3
  and 5.4). An endpoint that silently ignored the item would leave the model
  without the tools it loaded, and nothing would report it. Those surfaces
  stay declared-only.
- **Attempted against verified.** A caller that tries deferral on an
  unverified model knows the model is unverified; oven needs no extra state
  for it. Claude 4.5 and later on Anthropic itself, OpenAI Responses, and
  models whose provider declares support (Kimi Code's `/models`) are
  verified.
- **The retry.** A request to an attempted model that carries deferral (a
  deferred tool or a `LoadTools` result) and gets a `400` or `422` is
  retried once without it. The retry runs as if the capability were
  `Unsupported`: every tool up front, and `LoadTools` results as text.
  - **Any 400 or 422 qualifies,** not only the keywords above: gateways
    reword errors, and Claude Code retries on any rejection whose cause it
    does not recognize for the same reason.
  - **A retry that succeeds** marks the model as rejecting deferral.
  - **A retry that fails too** reports the original error and marks
    nothing, since deferral was not the cause.
  - **A verified model gets no such retry.** Its 400 has another cause, and
    only the keywords above mark it.

## 8. Tests and release

- **Core:** serde round trips for `ToolLoading`, `LoadTools`, and the
  capability (missing field reads as the default). Validation rules from
  section 5.1, each with explicit values.
- **Encodings:** golden request bodies per adapter in `oven-sdk-conformance`:
  - deferred tools in the tools array, or left out of it;
  - a cache marker moved off a deferred tool;
  - a load result as references, or followed by an `additional_tools` item
    or a Kimi system message;
  - a repeated load;
  - an unknown name dropped;
  - the `Unsupported` fallback.
- **Live checks:** ignored by default, run by hand with keys. Claude is the
  reference (section 3 points 1 to 3 as tests), and so is OpenAI Responses
  (section 5.3's probes, the namespace round trip included). Kimi has a
  probe ready for when a key works (section 5.4).
- **Versions:** `ToolContent` gains a variant, so core and every adapter take
  a minor bump under 0.x (core 0.8.0). Publish them to crates.io before the
  next cookie release, then bump cookie's pin.

## 9. Decisions

Decided by the user on 2026-10-27:

1. **How a result loads tools: a `ToolContent::LoadTools` variant.** It
   matches Anthropic's rule that such a result holds nothing else. Rejected:
   a `loaded_tools` list on `ToolResultPart` next to ordinary content, which
   Anthropic would have to split or drop.
2. **The OpenAI Responses encoding: an `additional_tools` input item**
   (section 5.3), verified live on 2026-10-27. It is the same
   append-at-load-point shape as Kimi, and the loader stays an ordinary
   function on every provider. The option not taken is OpenAI's client
   `tool_search` tool,
   with `tool_search_call` and `tool_search_output` items. That option makes
   the caller's loader a special tool on one provider, and it defers less for
   a tool on its own: per Azure's guide, the model still sees each deferred
   function's name and description, and only the parameter schema waits.
3. **No hosted search.** Anthropic's `tool_search_tool_regex` and
   `tool_search_tool_bm25`, and OpenAI's hosted `tool_search`, exist on two
   providers only. They would need server-block streaming and replay, and
   they search without the caller's rules.
4. **The fallback sends deferred tools up front with a warning** (section 6).
   It does not return an `Unsupported` error.
5. **Unknown names in `LoadTools` are dropped with a warning,** as a last
   resort. A loaded tool can disappear when its MCP server fails. Before
   that happens, cookie reconnects the server (section 10, decision 3), so
   oven drops a name only when the reconnect failed too, or the server no
   longer lists the tool.

## 10. For the cookie spec that follows

Not part of this work; noted so the oven API fits it.

**User decisions (2026-10-27):**

1. **No search tool without native loading.** A model whose capability is
   `Unsupported` gets every tool up front and no `tool_search` tool, since
   there is nothing for it to load.
2. **The clients show the two sets apart.** The tools row in the TUI and the
   web client lists the tools sent up front separately from the searchable
   (deferred) ones, for example `tools · 12 provided, 40 searchable`.
   - `model_tools_published` carries only `tool_names` today
     (`record_model_tools` in `crates/engine/src/runtime/model_loop.rs`).
   - It gains a `deferred_tool_names` field, an additive client protocol
     change that older readers ignore.
   - `model_tools_summary` (`crates/client_state/src/rows.rs`) renders both
     sets.

3. **Reconnect before dropping a loaded tool.** MCP servers already keep
   their tools published while they reconnect (3 attempts, after 1 s, 2 s,
   and 4 s; [connection lifecycle](../extend/mcp.md#connection-lifecycle)).
   A server whose attempts all failed drops its tools, and today it is
   retried only when a session that uses it is created or opened.
   - **The new rule:** before a model request, a session whose history has
     loaded a tool of a `failed` server retries that server and waits for the
     attempt, bounded by its `timeout_ms`.
   - **Scope:** only servers whose tools the session has loaded, so a dead
     server nobody loaded from never delays a run.
   - **Only when that attempt fails too** is the tool left out of the
     request, and oven drops its name from the earlier results (decision 5).

4. **Try deferral anyway on Anthropic-compatible endpoints.** A model there
   is attempted rather than verified: cookie sends deferral, retries without
   it on a rejection, and remembers the model (section 7.1). The Anthropic
   encoding lists every loadable tool in `tools`, so an endpoint that ignores
   the flag still works. OpenAI-compatible endpoints stay declared-only,
   since their encodings would fail silently.

5. **Announce what can be loaded by appending changes.** As Kimi Code does
   (`packages/agent-core-v2/src/agent/toolSelect/dynamicTools.ts`), cookie
   appends an announcement whenever the set of loadable tools changes: a
   `<tools_added>` block and a `<tools_removed>` block of tool names. The
   model folds them in order to get the current set. Appending changes,
   rather than restating a list in the system prompt or in a reminder that
   moves, keeps the prefix stable as MCP servers connect and disconnect.
   - **The loader,** `tool_search`, searches only the tools offered after
     permissions. It takes exact names (Claude Code's `select:a,b`) and
     keywords.
6. **Always defer MCP and plugin tools** on a model with `Native` (verified
   or attempted). No token threshold: the prefix is the same from a
   session's first request.
   - **Never deferred:** cookie's built-in tools, and the tools of the
     coding and remote runtimes.
7. **An opt-out per server and per tool.** `always_load = true` on an MCP
   server, or on a plugin, sends all its tools up front. A per-tool list
   (`always_load_tools`, patterns over the server's own tool names, as
   `include_tools` uses) sends single tools up front.

8. **Compaction folds loads and announcements into the new prefix.** A
   compaction replaces the history before its recent tail, so the cached
   prefix starts over there anyway. This holds for the internal summary and
   for OpenAI native compaction alike; they differ only in when the fold
   happens.
   - **To fold** is to mark every tool loaded so far, and still offered,
     eager. Oven then writes the history as if the tools had been in
     `tools` all along: load results as text, no `additional_tools` items,
     and calls without `namespace` (sections 5.1 and 5.3). Oven needs
     nothing more; the caller marks the tools eager. Cookie's session log
     never changes.
   - **Internal summary: fold after.** The summarizer's request reuses the
     latest conversation request's system prompt, tools, and history, so it
     reads them from the cache ([compaction](../sessions/compaction.md),
     replaced-span trial). Folding before it would change `tools`, which
     come before the history, and miss the cache for the whole request. The
     summary is plain text either way, so the summarizer gets the history
     as the conversation encoded it, and the fold applies from the first
     request after the checkpoint.
   - **OpenAI native compaction: fold first.** The `/responses/compact` input
     is written with the tools already eager. A window made from a history
     with loads leaves the tools unusable (table below). This costs no
     cache: a `/responses/compact` request never reads the conversation's
     cache. Probed on 2026-10-27 (gpt-5.5, a 7,400-token history whose
     conversation requests read 6,656 tokens from the cache): the first
     compact request read 0 cached tokens, folded or not, with or without the
     conversation's `tools`, `instructions`, and `prompt_cache_key`. An
     identical compact request repeated read about 6,500 tokens, so compact
     caches only against itself.
   - **Loaded tools stay eager after it.** From the first request after the
     compaction, they sit in the stable tool prefix. A load result in the
     kept tail that names one renders as text (section 5.1).
   - **One announcement restates the set.** The first message after the
     summary or window carries a single `<tools_added>` block listing every
     tool still loadable. Earlier announcements, additions and removals
     alike, are not repeated; a removed tool simply is not listed.
   - **Deferral then continues.** Tools loaded after the compaction use
     `LoadTools` again, and the next compaction folds them too.
   - **Why native compaction must fold first** (probed on 2026-10-27,
     gpt-5.5, 3 windows per history, 3 requests per window; success means
     the model calls the loaded tool after the window):

     | History compacted | Then | Calls the tool |
     |---|---|---|
     | Tool eager all along | tool eager | 9/9 |
     | Tool loaded by `additional_tools` | tool made eager | 1/9 |
     | Tool loaded by `additional_tools` | `additional_tools` re-appended | 5/9 |
     | Tool loaded by `additional_tools` | both | 3/9 |
     | Tool loaded, folded to eager before compacting | tool eager | 9/9 |

     The compact output is a user message and a `compaction` item; it keeps
     no `additional_tools` item, so a window never carries a load. A window
     made from a history with loads also leaves the model unwilling to call
     the tool however it is offered afterwards, and often claiming it has no
     tools at all.

**Still to specify:**

- **Capabilities:** cookie declares `Native` (verified) for first-party
  Anthropic Claude 4.5 and later, OpenAI Responses models (gpt-4.1 and later
  were probed), Azure OpenAI Responses (documented, not probed), Kimi K3 on
  the Moonshot platform, and Kimi Code models whose
  `/models` entry says `tools.dynamic_load` (section 5.4). Every other model
  on an Anthropic-compatible adapter is `Native` (attempted, decision 4).
  - **models.dev** has a `dynamically_loaded_tools` field that Kimi Code
    reads. No model carries it yet; cookie should read it too.
  - **A per-model override** in `[providers.<id>]` handles DeepSeek and
    gateways.
- **The retry and its memory** (section 7.1):
  - **Remember models that reject deferral** per provider and model, as
    `<data_root>/native-compaction.json` remembers providers that reject
    compaction windows.
  - **Entries expire after 7 days and on a configuration digest change,**
    so deferral is tried again later, as Claude Code does.
  - **A remembered model is `Unsupported`:** no `tool_search` (decision 1),
    every tool up front.
  - **The configuration status lists the remembered models.**

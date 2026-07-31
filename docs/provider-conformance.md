# Provider adapter conformance checklist

Derived from OpenCode's provider layer (`anomalyco/opencode` dev @ e024e2e,
2026-07-31) and primary vendor docs. Each adapter in `crates/providers` is
implemented and tested against this checklist (see ARCHITECTURE.md §6.2).

Legend: **MUST** = coding-agent correctness. **NICE** = fidelity/cost/UX.
`Compat` = typical generic OpenAI-compatible endpoint: **Yes** / **Probe** / **No**.

---

## 1. Format and provider inventory

The useful unit is **wire format**, not vendor branding: model catalogs can
route many vendor names onto the same format, and one vendor can offer
several (DeepSeek offers both OpenAI-compatible Chat and an
Anthropic-compatible endpoint).

| Wire-format family | Providers (examples) | Important OpenCode special cases |
|---|---|---|
| Anthropic Messages + SSE | Anthropic; Vertex Anthropic; Anthropic-compatible Kimi/MiniMax/DeepSeek routes | `interleaved-thinking` + fine-grained tool streaming headers; filters empty messages but retains signed/redacted reasoning (`provider/transform.ts` L166-193) |
| Google generateContent / Vertex Gemini + SSE | Google AI Studio, Vertex | Gemini schema lowering (numeric enums → strings); thinking enabled by default for reasoning models |
| Amazon Bedrock Converse + AWS EventStream | Bedrock-hosted Claude/Nova/Meta/Mistral/… | Region/profile/cross-region model-ID normalization; binary event-stream framing |
| OpenAI Responses + SSE/WS | OpenAI; Azure default; xAI default; Bedrock Mantle | Defaults `store:false`; requests encrypted reasoning for compatible models |
| OpenAI Chat Completions + SSE | Generic compatible; DeepSeek, Groq, Cerebras, DeepInfra, Together, Fireworks, Baseten, Mistral, Perplexity, Venice, DashScope, NVIDIA, Cloudflare; Azure Chat; xAI Chat; OpenRouter Chat | DeepSeek empty-reasoning workaround; Mistral tool-ID normalization (exactly 9 alphanumerics); Snowflake role patches |
| OpenRouter (OpenAI-shaped router) | OpenRouter | Nonstandard `usage`, `reasoning`, `prompt_cache_key` body fields; attribution headers |
| Provider-native non-OAI | Cohere v2, Alibaba native, Vercel AI Gateway, GitLab workflow | GitLab workflow is server-side tool execution, not client function calling |

Primary docs: Anthropic Messages/thinking, OpenAI Responses, OpenAI Chat,
Gemini function calling, Bedrock Converse, Azure Responses, OpenRouter,
Cohere Chat, Mistral function calling.

---

## 2. Cross-format requirements (every adapter)

- [ ] **MUST — Preserve an ordered, typed transcript, not merely role/text.**
  Persist provider-native opaque fields alongside normalized content:
  block/item type, IDs, call IDs, signatures, encrypted/redacted blobs,
  finish data, unknown provider fields. A reconstructed "assistant text"
  turn loses the state required to continue tool/reasoning conversations.
- [ ] **MUST — Separate provider object ID from tool-call ID.** Responses
  `item.id` vs `call_id`, Chat `tool_calls[].id`, Anthropic `tool_use.id`,
  Bedrock `toolUseId` are not interchangeable.
- [ ] **MUST — Treat tool calls as an ordered batch.** Append all original
  assistant calls and every corresponding result before the next model turn.
  Never drop a failed tool result; encode it as the provider's error result.
- [ ] **MUST — Preserve unknown structured output** (hosted tools, citations,
  safety metadata, annotations, refusal data, computer-use actions) as opaque
  data even if not executed/rendered.
- [ ] **MUST — Do not blindly retry a completed/partially streamed request.**
  Retry only before meaningful output, or via an idempotent/resumable
  provider mechanism.
- [ ] **NICE — Track inclusive and component usage separately** (input,
  cache-read, cache-write, output, reasoning tokens have different inclusion
  rules per provider; do not double-add subsets).

---

## 3. Anthropic Messages

| Requirement | Priority / Compat | Why it breaks if missed |
|---|---|---|
| Preserve every assistant content block **in exact original order** (`thinking`, `redacted_thinking`, `text`, `tool_use`, server-tool blocks/results). Replay `thinking` with original text + `signature`; replay `redacted_thinking` as opaque `data` | **MUST** / No | Claude validates signed thinking against history; reordering/stripping breaks the next request |
| Preserve thinking that is empty but signed/redacted | **MUST** / No | Signature/redacted payload is state, not emptiness |
| Interleaved thinking: reasoning can occur before/between tool calls and text; keep block boundaries and indices | **MUST** when enabled / No | "All reasoning first" assumption loses valid turns |
| Echo each `tool_use`, then user `tool_result` with exact `tool_use_id`; multiple results per user turn; `is_error` | **MUST** / No | Pairing is protocol validity |
| Distinguish local `tool_use` from `server_tool_use`; never locally execute server tools; preserve server results for replay | **MUST** if hosted tools / No | Duplicate actions or lost context |
| `cache_control` at allowed positions; cap breakpoints (4); retain cache read/write accounting | **MUST** when caching exposed / No | Extra markers → 400; lost markers → cost/latency regression |
| Preserve `stop_reason`/`stop_sequence`; map `tool_use`, `max_tokens`, `end_turn`, `pause_turn`, refusal separately | **MUST** / Probe | `pause_turn`/`max_tokens` as completion truncates the turn |
| Native image/document blocks where attachments claimed; preserve MIME/type/source, also in tool results | **MUST** if advertised / No | Textifying media bloats context, breaks visual tasks |
| Thinking-mode/model compatibility (budget_tokens, adaptive, effort, interleaving headers vary by Claude generation) | **MUST** when exposing controls / No | Obsolete modes → HTTP 400 |

Streaming:

- [ ] **MUST:** parse `message_start` usage, `content_block_start`, indexed
  deltas, `content_block_stop`, final `message_delta`; accept `ping`.
- [ ] **MUST:** append `input_json_delta.partial_json` by block index; parse
  JSON only at block stop.
- [ ] **MUST:** retain `signature_delta` (often arrives after thinking text).
- [ ] **MUST:** SSE `error` is a terminal provider error despite HTTP 200.
- [ ] **NICE:** final `message_delta.usage` is authoritative; can supersede
  `message_start` usage.

Cancellation/errors:

- [ ] **MUST:** abort HTTP stream on cancellation; stop local tool
  scheduling; partial transcript is incomplete, not success.
- [ ] **MUST:** classify `429` rate limit vs **`529 overloaded_error`**
  (retry with backoff); `413 request_too_large`/context overflow; 400
  validation. Inspect in-stream `error.type`, not just HTTP status.

---

## 4. OpenAI Chat Completions

| Requirement | Priority / Compat | Why it breaks if missed |
|---|---|---|
| Replay assistant `tool_calls` retaining every `id`, name, exact serialized arguments; subsequent `role:"tool"` messages with matching `tool_call_id` | **MUST** / Yes | Result without the original call is invalid |
| Multiple/parallel `tool_calls` in one assistant message; preserve array order | **MUST** / Yes | Dropped calls stay permanently pending |
| Assistant `content: null` with tool calls; `content` + calls; preserve `refusal`/content-filter data | **MUST** / Yes (null/tools), Probe (refusal) | Valid tool-call turns often have null content |
| Preserve/replay `reasoning_content`/`reasoning_details` where the endpoint requires it (DeepSeek); omit deliberately elsewhere | **MUST** for such profiles / Probe | Unknown extensions → 400; missing required reasoning breaks continuation |
| Project tool schemas to the OpenAI JSON-Schema subset; honor `strict:true` rules or don't claim it | **MUST** for tools / Probe | Unsupported shapes → 400 or invalid arguments |
| Tool errors as tool messages, not fabricated assistant replies | **MUST** / Yes | Model must diagnose failures |
| Capture cache/reasoning token details in `usage` | **MUST** for telemetry / Probe | Subset fields are not additive |

Streaming:

- [ ] **MUST:** tolerate role-only initial delta, empty deltas,
  `content:null`, usage-only chunk with `choices: []`.
- [ ] **MUST:** key tool-call assembly by choice + `tool_calls[].index`;
  IDs/names may arrive only in the first fragment.
- [ ] **MUST:** finalize/parse arguments only at
  `finish_reason:"tool_calls"` or stream end; malformed JSON = model
  failure, not transport success.
- [ ] **MUST:** recognize `stop`, `length`, `content_filter`,
  `tool_calls`/legacy `function_call`.
- [ ] **MUST:** consume SSE comments/keepalives and terminal `[DONE]`;
  never JSON-decode them.

Cancellation/errors:

- [ ] **MUST:** classify `429` rate limit, `400` invalid/schema/context,
  `401/403`, `404` model, `413`, `5xx` transient; profile endpoint-specific
  context strings (`context_length_exceeded`, vendor wordings, Azure
  content-filter 400).
- [ ] **NICE:** preserve `x-request-id`, retry-after, rate-limit headers.
- [ ] **Compat reality:** Chat usually works; reasoning fields, strict
  tools, multimodal, stream usage, cache controls, exact error bodies are
  all **Probe**.

---

## 5. OpenAI Responses

| Requirement | Priority / Compat | Why it breaks if missed |
|---|---|---|
| Preserve the ordered heterogeneous `output`/`input` item list (`message`, `reasoning`, `function_call`, `function_call_output`, `item_reference`, opaque hosted-tool items) | **MUST** / No-Probe | Responses is item-oriented; role-collapse loses state |
| Pair `function_call_output.call_id` with the original `function_call.call_id`, NOT item `id` | **MUST** / Probe | Wrong pairing makes results unavailable |
| Stateful mode: `previous_response_id`/`item_reference` only when server storage is available | **MUST** / Probe | References to non-stored items fail |
| `store:false` stateless: preserve and replay reasoning items with `encrypted_content` plus intervening function calls/results since last user turn; request `include:["reasoning.encrypted_content"]` when required | **MUST** / No-Probe | Reasoning continuity degrades or requests rejected |
| Retain reasoning item `id`, summary ordering, encrypted state, phase/opaque fields | **MUST** / No-Probe | Continuation state, not decoration |
| Ignore hosted/provider-executed tools for LOCAL dispatch, but preserve/replay their items | **MUST** when hosted tools / No-Probe | Local execution duplicates actions |
| Distinguish `response.completed`/`incomplete`/`failed`; map `max_output_tokens`/filters correctly | **MUST** / Probe | Incomplete ≠ success |
| Image content in `function_call_output` where visual tools supported | **NICE** generally / No-Probe | Stringified screenshots destroy semantics |

Streaming:

- [ ] **MUST:** `response.output_item.added` precedes argument deltas; key
  assembly by item ID while emitting user-visible call ID.
- [ ] **MUST:** handle full-call-only delivery in `response.output_item.done`
  (some clones skip argument deltas).
- [ ] **MUST:** track reasoning summary parts by (item ID, summary_index);
  retain encrypted content on the item.
- [ ] **MUST:** terminate on completed/incomplete/failed; do not require
  `[DONE]`.
- [ ] **MUST:** parse top-level `error` and nested `response.error` after
  HTTP 200.
- [ ] **NICE:** record response ID/service tier/final usage.

Cancellation/errors:

- [ ] **MUST:** cancel HTTP/SSE/WS locally; background Responses need the
  provider cancellation lifecycle — disconnect ≠ server stopped/unbilled.
- [ ] **MUST:** `response.failed` and SSE `error` are failures despite 200.
- [ ] **MUST:** classify `context_length_exceeded`, `max_output_tokens`
  incomplete, 429/`rate_limit_exceeded`, `insufficient_quota`, 5xx.
- [ ] **Compat reality:** "OpenAI-compatible" usually means Chat, NOT the
  Responses item model, encrypted reasoning, references, or hosted tools.

---

## 6. Gemini / Vertex (post-MVP)

- [ ] **MUST:** `systemInstruction` separate from `contents`.
- [ ] **MUST:** preserve `thought:true` parts + `thoughtSignature`
  (including on `functionCall` parts) and replay in the same location.
- [ ] **MUST:** `functionResponse` with matching name/position per call.
- [ ] **MUST:** translate tool schemas to Gemini's dialect deliberately.
- [ ] **MUST:** capture `promptFeedback`, safety ratings, empty-candidate
  blocks, safety/recitation finish reasons.
- [ ] **MUST:** inline media preserved (MIME/data), not textified.
- [ ] **NICE/MUST-budgets:** `promptTokenCount`, cached, candidate, and
  `thoughtsTokenCount` have distinct inclusion semantics.
- Streaming: candidates may be absent or carry thought/text/function/usage
  only; no reasoning-then-text ordering assumption; preserve late thought
  signatures.
- Errors: `400 INVALID_ARGUMENT`, `401/403`, `404`, `429
  RESOURCE_EXHAUSTED`, `500`, `503 UNAVAILABLE`; safety blocks are normal
  responses with metadata, not retries.

## 7. Amazon Bedrock Converse (post-MVP)

- [ ] **MUST:** Converse body model (`messages`, `system`, `toolConfig`,
  `inferenceConfig`, `additionalModelRequestFields`) + SigV4 signing.
- [ ] **MUST:** preserve `reasoningContent` (text, signature,
  `redactedContent`); signatures cover the conversation — modified/missing
  history errors.
- [ ] **MUST:** `toolUse.toolUseId` ↔ `toolResult.toolUseId`; structured
  text/JSON/image result blocks.
- [ ] **MUST:** positional `cachePoint` blocks per model limits; cache
  read/write fields.
- [ ] **MUST:** native image/document constraints; `guardrail_intervened`
  etc. stop reasons.
- Streaming: **AWS binary EventStream** (frame CRC/headers, not SSE);
  correlate by `contentBlockIndex`; completion only after `messageStop` AND
  trailing `metadata`; parse event-stream exceptions
  (`internalServerException`, `modelStreamErrorException`,
  `validationException`, `throttlingException`,
  `serviceUnavailableException`).
- Errors: 400 ValidationException (context variants), 403, 404, 424
  ModelErrorException, 429 throttling, 500, 503.

---

## 8. Vendor deviations

- **Azure:** `/openai/v1` conventions + deployment naming; api-key and Entra
  auth; Responses-vs-Chat selection is deployment/API-version dependent;
  content filtering is a distinct outcome (400 `content_filter`, mid-stream
  error events at HTTP 200).
- **OpenRouter:** capability-variant routing per request; use
  `provider.require_parameters:true`/pin routing when tools/reasoning/strict
  schemas matter; preserve `reasoning` extensions; `usage:{include:true}`;
  router retries may land on different providers with different behavior.
- **DeepSeek:** profile both OpenAI-compatible and Anthropic-compatible
  interfaces; `reasoning_content` replay where required (empty field only
  where required, not globally).
- **Mistral:** tool-ID restrictions (9 alphanumerics); result ordering
  immediately after tool turn; model-specific reasoning controls.
- **Groq/Cerebras/DeepInfra/Together/Fireworks/Baseten/Perplexity/Venice/
  NVIDIA/Cloudflare/etc.:** Chat checklist as base; capability-probe streamed
  argument deltas, parallel calls, images, `stream_options.include_usage`,
  strict tools, reasoning replay, cache fields, max-token spelling; preserve
  provider-specific raw fields.
- **xAI:** Chat + Responses; encrypted reasoning replay in Responses;
  reasoning models may reject ordinary sampling params.
- **Cohere/Alibaba/Vercel Gateway/GitLab:** do not route through generic
  OpenAI compatibility; represent server-executed tools separately.

---

## 9. Streaming transport (all formats)

- [ ] **MUST:** incremental UTF-8 decoding; CRLF/LF; multi-line `data:`;
  comments/keepalives; blank-event boundaries; `[DONE]` where applicable.
  Never assume one network chunk = one event.
- [ ] **MUST:** accept role-only, usage-only, ping, empty-delta,
  metadata-only events.
- [ ] **MUST:** append raw argument fragments; parse once per call at
  completion; never concatenate same-name calls.
- [ ] **MUST:** accept initial, final, or only-final usage; don't finish
  before trailing usage metadata.
- [ ] **MUST:** every parser has a terminal in-band error path (errors after
  HTTP 200).
- [ ] **MUST:** key by provider-native index/item ID/block ID, never by
  displayed text order.
- [ ] **MUST:** separate connect/header, overall, and stream-idle deadlines.
- [ ] **MUST:** propagate abort to the socket and cancel queued local tools;
  transcript stays interrupted; disconnect ≠ server stopped/unbilled.

---

## 10. Error-classification reality

| Family | Retryable overload/rate-limit | Context/length |
|---|---|---|
| Anthropic | 429; **529 overloaded_error**; 5xx | 400 validation/context wording; 413 request_too_large |
| OpenAI Chat/Responses | 429/quota; 5xx; in-band failures | `context_length_exceeded`; Responses `incomplete` w/ max_output_tokens |
| Azure | 429/`too_many_requests`/`no_capacity`; in-stream errors at 200 | 400 `content_filter`; deployment-specific wording |
| Google/Gemini | 429 RESOURCE_EXHAUSTED; 503; 500 | 400 INVALID_ARGUMENT; safety blocks are normal responses |
| Bedrock | 429 ThrottlingException; 500; 503; 424 ModelError; stream exceptions | 400 ValidationException (model-specific wording) |
| OpenRouter/compatible | 429 + 5xx/502/503/504 (router/upstream dependent) | Upstream text, router error object, or silent truncation |
| DeepSeek/Mistral/Groq/… | usually 429 + 5xx, varying bodies | 400 with vendor-specific message/code |

**MUST classifier rule:** classify from `(HTTP status, provider error
code/type, body text, in-stream event type, retry-after headers)` — never
from status alone.

---

## Bottom line for `openai_compatible`

- **Claim by default (MUST-supported):** chat text, assistant tool-call
  echo, tool-result `tool_call_id` pairing, standard Chat SSE, basic
  429/5xx handling.
- **Probe before enabling:** parallel calls, tool argument deltas, images,
  `stream_options.include_usage`, strict JSON schema, `reasoning_content`,
  cache fields, vendor max-token names.
- **Never claim without a profile:** Anthropic signed/redacted thinking,
  Gemini thought signatures, Bedrock Converse/EventStream/SigV4, Responses
  item references/encrypted reasoning/hosted tools, provider-native
  documents/cache controls.

**Architectural conclusion:** round-trip fidelity is a first-class persisted
data model (ARCHITECTURE.md §6.2). Normalized events alone are insufficient
for Anthropic, Responses, Gemini, and Bedrock continuations.

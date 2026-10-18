# Tool Reference

Cookie Agent publishes tools according to the active agent's permissions and the
capabilities of the selected model. Tool argument objects are strict: unknown
fields and wrong types are rejected.

## Execution concurrency

Tool providers declare whether each tool is safe to overlap with sibling calls
from the same model turn through `ToolSpec::concurrency`. `ToolConcurrency` is
`Exclusive` by default; parallel execution requires an explicit `Parallel`
declaration.

| Tools | Eligibility | Coordination |
|---|---|---|
| `read`, `read_artifact`, `bash`, `webfetch`, `stop_background` | Parallel | Each call owns its execution and streaming state. |
| `write`, `edit` | Parallel | Matching prepared serialization keys serialize mutations to the same target. |
| `delegate_subagent` | Parallel | Delegate admission serializes durable child reservation and session creation internally. |
| `send_message` | Parallel | Message acceptance serializes guard checks and the durable append inside the recipient actor. |
| MCP tools | Parallel | Each MCP server's service mutex serializes calls to that server; different servers can overlap. |
| `skill`, `get_subagent_result` | Exclusive | These calls interact with session-scoped state. |
| Plugin and otherwise undeclared tools | Exclusive | External declarations cannot currently opt in. |

All parallel-eligible calls in the turn are dispatched together without a
fan-out limit. Exclusive calls run one at a time after the parallel calls have
finished. Results remain associated with their tool call IDs, while terminal
events are persisted in completion order.

## Tool call failures

A failed tool call terminates with a failure code and a message for the model:

| Code | Meaning |
|---|---|
| `execution_failed` | The tool ran, or tried to, and failed. |
| `operation_changed` | What the call was prepared and approved against changed before it ran, such as an MCP tool list or a restarted runtime. |
| `prepared_capability_lost` | The prepared call was lost, for example in a daemon restart between preparation and execution. |
| `permission_denied` | A permission rule denied the call. |
| `runtime_unavailable` | The runtime or MCP server that owns the tool is reconnecting or dropped during the call. The call is not retried for the model; the tool stays published while its owner reconnects. |
| `unsupported_platform` | The tool cannot run on this platform. |
| `invalid_url`, `redirect_error`, `timeout`, `transport_error`, `unsupported_content_type` | `webfetch` failures (see [Webfetch](#webfetch)). |

## Webfetch

`webfetch` accepts `{"url":"https://docs.quantumcookie.xyz/", "raw":false}`.
`url` is required; `raw` defaults to false. The URL must start with a lowercase
`http://` or `https://` prefix; any other scheme is rejected with `invalid_url`
before a request is sent. One permission check uses the initial URL including
its query string. Without a matching rule the call is denied, and without any
`allow` or `ask` rule for `webfetch` the tool is not advertised. Explicit `ask`
rules still use the normal approval flow.

Output starts with four header lines (`final_url`, `status_code`, `content_type`,
and `truncated`), followed by a blank line and the text body verbatim. Body
newlines are physical newlines, not JSON escapes. Structured `url`, `final_url`,
`status_code`, `content_type`, and `truncated` fields remain in result metadata.
HTML (`text/html` or `application/xhtml+xml`) is rendered by html2text at width
80 unless `raw` is true. Text types, JSON, XML, JavaScript, form data, and
`+json`/`+xml` types pass through as text; raw and non-HTML text use UTF-8 lossy
decoding. Binary types are rejected, including in raw mode. Missing or invalid
Content-Type is treated as `application/octet-stream` and rejected.

The request has a 30-second overall timeout, inherits environment proxies, and
uses reqwest's default redirect policy (10 hops). Redirect hops are not
permission-checked. HTTP error statuses are returned with their text bodies.
There are no additional SSRF, host/IP, DNS, or userinfo checks.

Downloads are streamed to a fixed 16 MiB cap, or 4 MiB for HTML that is
converted to text. Over-cap responses return the fetched prefix with
`truncated: true`, not an error. The caps have no input or configuration
parameter. Full results use the standard event log and result
store. The standard Bounded policy retains oversized output and sends a preview
to the model. `read_artifact` pages the retained header and body by physical
line; its zero-based offset 5 starts at the body. Follow `next_offset` for
successive pages. Metadata is separate and is not paged. `read_artifact` needs
no permission rule.

Errors distinguish `invalid_url`, `redirect_error`, `timeout`,
`transport_error`, and `unsupported_content_type` (which names the content type).
`permission_denied` uses the standard policy-denial path and its structured
`tool_denied` reason.

## Filesystem tools

`read` accepts `filePath`, plus optional zero-based `offset` and positive
`limit`. The defaults return at most 2,000 lines or directory entries. The path
is resolved when the call is prepared; the content is read when it executes.
A text line longer than 4 KiB is cut and ends with a
`[line truncated: N more bytes]` marker, and a page that would exceed the result
size limit stops early with a `continue with offset=N` note (also reported as
`next_offset` in the metadata). Media files are checked against the size limits
before they are read in full.

`write` accepts `filePath` and complete `content`. `edit` accepts `filePath`,
`oldString`, and `newString`, and requires the old text to identify one
unambiguous replacement. Preparation checks the replacement against the current
content so a mismatch fails before approval, but the edit itself is applied to
the file's content at execution: changes made while an approval was pending are
kept, and an `oldString` that no longer matches fails the call. Both tools
publish through a staged file renamed over the target. Writes and edits that
resolve to the same file share a serialization key and run one at a time, in
call order, so same-file edits in one turn apply on top of each other.

Tool results may include image, PDF, audio, or video attachments. Attachments
are validated, content-addressed, and supplied to models as file parts rather
than embedded in text output.

### Media reads

When `read` targets an image (PNG, JPEG, GIF, WebP), PDF, audio file (MP3,
WAV, Ogg, FLAC), or video container (MP4/MOV/WebM/MKV/AVI/FLV/MPEG/WMV/3GPP),
the file is sniffed by content (never by extension alone), strictly validated,
and retained as an attachment. Whether the attachment reaches the model
depends on two checks, in order:

1. **Model capability.** The selected model must declare the matching input
   modality in the catalog (for example `image` for `image/png`). If it does
   not, the call fails with a tool error naming the model and the missing
   capability.
2. **Family deliverability.** The provider's wire API must accept the media
   kind either inside a tool result or in a following user turn (see the
   [delivery matrix](#media-delivery)). If it does
   not, the call fails with a tool error naming the family.

Both rejections are ordinary tool errors: the model sees the reason and can
recover (for example by asking the user, or by sampling the file through
`bash`). Size is clamped to the smaller of the model's advertised limit and
the provider's inline limit (Anthropic: 7.5 MiB raw images, 10 MiB once
base64-encoded; Bedrock: 3.75 MiB images, 4.5 MiB documents, ≈18.7 MiB raw
video; Vertex: 7 MiB images; Gemini API: 14 MiB for any media, keeping the
base64 request under its 20 MiB cap; other families: 20 MiB images, PDFs, and
audio, or 25 MiB video). Media goes in the tool result when the family's wire
API accepts it there; otherwise, when the API accepts that kind in user
messages, it is sent as a file in one emitted user turn immediately after the
tool result. Each model also advertises per-kind count limits, enforced per
request (catalog models: 20 images, 5 PDFs, 5 audio files, and 2 videos, 1 on
Bedrock). When the history holds more parts of a kind than the limit, the
oldest are replaced in the outgoing request by a placeholder such as
`[image omitted: over the model's per-request image limit]`; the saved history
keeps them. The elided count grows in steps of half the limit, so later media
appends to an unchanged prompt prefix (and prompt cache) until the limit is
reached again, at the cost of sending as few as half the limit's parts right
after a step. The latest user message is never elided: a request fails only
when that message alone attaches more parts than the limit. Media parts do not
contribute to context fit estimates, except video, which carries a flat
conservative cost.

MCP media blocks (images, audio, embedded resource blobs) follow the same gate
and delivery selection. Blob resources without a declared MIME type are
retained under the sniffed type. Results that would exceed the combined
attachment budget keep what fits and degrade the rest to inline notes. The MCP
wire format cannot author arbitrary additional messages.

Media does not survive context pressure. Tool-output elision removes the parent
result and all of its emitted messages as one unit, and compaction checkpoints
drop attachments. The model can re-read the source file to recover it.

### Media delivery

This matrix describes tool-origin media, not every input type the upstream API
accepts. The effective model must also declare the modality and limits.

| Effective adaptor family | Images | PDF | Video | Audio |
|---|---|---|---|---|
| Anthropic | Tool result | Tool result | No | No |
| Anthropic-compatible | Tool result | Tool result | User turn on capable MiniMax models | No |
| Bedrock Converse | Tool result | Tool result | Tool result on capable Nova models | No |
| OpenAI/compatible Responses, Azure Responses | Tool result | Tool result | No | No |
| OpenAI Chat | User turn | User turn | No | No |
| Azure Chat | User turn | No | No | No |
| OpenAI-compatible Chat | User turn | User turn | User turn on capable Kimi/Qwen models | No |
| Gemini, Vertex Gemini | User turn | User turn | User turn | User turn |
| Cohere | User turn | No | No | No |

## Bash

`bash` accepts a complete `command` and an optional `timeout` in
milliseconds (default 120000; `0` never times out). It is
[background-capable](#background-calls): the host adds `background`
(default `false`), and the timeout applies in the background too, so a
command that keeps running, such as a dev server, needs `timeout: 0`.
The command's standard input is empty: tools take no input while they run.
Standard output and standard error are streamed separately during execution
and retained as separate artifacts for the terminal result.

The command runs in its own session and process group (a job object on
Windows). When the shell exits, whatever it left in that group is killed, so
`cmd &` and `nohup cmd &` do not outlive the call; only a process that leaves
the group, such as one started with `setsid`, does.

A bash call reaches its detach point as soon as its command runs; its detach
note is `Process <pid>`. In the background it keeps its timeout and
process-group cleanup.

## Background calls

A tool that declares `background` support (`bash`, `delegate_subagent`, and
a runtime's own tools that declare it) can run in the background, as the [background tool calls
spec](../specs/background-tools.md) describes:

- **The host owns the argument.** It adds `background` (a boolean, defaulting
  to the tool's `background_default`) to the tool's schema and removes it
  before the tool prepares the call, so the tool never sees it and an
  approval covers the call either way.
- **Detaching.** A call the model starts with `background: true`, or one the
  user moves to the background (`run.tool_background`; the TUI's background
  button), detaches once the tool reaches its detach point. A call that
  finishes first returns as an ordinary result. Detaching commits an
  immediate result for the call, `Started in the background as job
  job_1a2b3c4d. …` or `User manually moved this call to the background; it
  is still running as job job_1a2b3c4d. …`, ending with `Stop it with
  stop_background(job_1a2b3c4d).` and followed by the tool's note, and
  the run goes on. Cancelling the run no longer reaches the call.
- **Reporting.** When the call ends, the engine accepts one producer message
  owned by `tool { tool_call_id }` that steers into the active run or wakes
  the idle session: `<background_result job="…" tool="…" status="…">`
  wrapping exactly what the foreground result would have said, its output
  previewed with a `read_artifact` hint into the retained output for
  anything omitted. `status` is `completed`, `failed`, or `stopped`. The
  call reports only after its immediate result is committed; a revert past
  the call, deleting its session, or the daemon stopping kills it without a
  report.
- **Stopping.** `stop_background` (`job`) stops a background call of the
  session tree and returns once it has ended; it needs no permission and is
  published only while a background-capable tool is. A user stops one with
  `session.background.stop`. Either way the call still reports, with
  `status="stopped"` and a first line saying who stopped it.
- **Subagents report themselves.** A `delegate_subagent` call in the
  background, or moved there, gets no job: its immediate result names the
  subagent's handle, and every turn the subagent finishes reports to the
  parent as a `<subagent_report handle="…" status="…">` producer message
  owned by `subagent { session_id }`, keyed by the turn's run ID so it
  reports once. A foreground call's first turn is its tool result instead.
  The subagent's running turn lists among the parent's background calls
  under its handle, and `stop_background` with the handle cancels that turn
  (`status="cancelled"`; an idle subagent is an error). Cancelling the
  parent's run does not reach its subagents: a foreground call it was
  waiting on ends cancelled with the detach note, and the subagent reports
  its turn as if the call had been moved to the background.

## Retained tool output

Tools declare `ToolOutputDeclaration::Single` or an ordered list of 1 to 8 named
streams. Names are unique, case-sensitive ASCII path segments, 1 to 64 bytes,
using letters, digits, `_`, `-`, and `.`; `.` and `..` alone are forbidden. The
same validator governs declarations and the `stream` argument of `read_artifact`.

The runtime keeps bounded previews and counts of accepted text chunks in
memory. A stream that fits its preview is stored nowhere else: the result output
holds all of it and it has no artifact. Once a stream outgrows its preview, its
bytes are buffered and written in batches to a temporary file, hashed as they
are written, and published as a full artifact at completion. An artifact keeps
at most the first 128 MiB of a stream; past that only the last mebibyte is
kept, after a marker line counting the dropped bytes, and the stream's
`dropped_bytes` and the result's read hint report the loss. A single output has
at most one artifact; named output gets a generic manifest when any stream has
an artifact, and the manifest lists every declared stream. Declaration
order determines model rendering. Bash uses the same named-output interface as
other tools; it does not build its own retained-output manifest.

Each stream receives its own configured `[tool_output]` line/byte preview limits.
Only truncated streams receive a `read_artifact(artifact="<digest>",
stream="<stream>", offset=...)` hint, where the digest identifies the manifest.
Single-output hints omit `stream` and the heading. A partial-line byte cut points back to that
line, so readback does not skip unseen text. Aggregate preview construction also
reserves 16 KiB for headings/hints inside the existing 2 MiB output bound; when
necessary, previews are reduced with truthful read hints.

`ToolProgress.output` carries at most 8 typed chunks and 64 KiB of authoritative
text per delta. Its optional `display` is independent UI text. The `message` and
`display` fields are each limited to 1 KiB; the runtime caps their cumulative
live-presentation budget at 64 KiB per call without stopping authoritative output.
Final `display` replaces live display and has its own 64 KiB bound. Control
characters are sanitized while newlines and tabs remain usable in display text.
Display is never included in model history.

Executors return `ToolCompletion`: `Single`/`Named` supplies terminal output once,
whereas `Streamed` finalizes accepted deltas without resending them. Resupplying
terminal output after streaming is an error. Failures and cancellation preserve
accepted output as incomplete, retain the correct terminal status, and expose the
error alongside output previews to the model. Terminal events carry
`retained_output` references and final `display`; replay does not recapture chunks.

The self-paginating `read`, `read_artifact`, and `get_subagent_result` tools
declare an absolute truncation opt-out. Their requested page is returned in full
without artifact retention or truncation metadata, regardless of
`[tool_output]`. Callers bound these results with each tool's offset/limit
arguments. A requested page above the event schema's 2 MiB output limit fails
with a resource-limit tool error; it is never silently truncated. The
`delegate_subagent` terminal result also opts out, because its teaser preview is
bounded by `[subagent_output]` instead; the owner of the delegation flow is the
only place it is bounded. MCP and plugin tools remain subject to normal truncation. External
opt-out would require a future extension-protocol capability and is not currently
authorable.

Artifacts are content-addressed by SHA-256, deduplicated, and newly created with
mode `0600` on Unix. Their digest is verified when content is first read. A
missing or corrupt artifact produces a normal tool error and does not prevent
the engine or session from opening. Artifacts are written without fsync and
without a store-wide lock (each publish is one atomic rename), so a crash or
power loss can lose an artifact a durable event already references. Reads then
report the output as unavailable, and model history replaces a lost attachment
with a short note instead of failing.

Artifacts are collected only when a session is reverted. The revert deletes
the digests that the cut-away events (and any delegated children the revert
deletes) referenced, directly or through a generic manifest, unless an event
still in the tree's logs references them too. The sweep is skipped while any
session of the tree runs or a capture is still publishing its terminal
references. Artifacts orphaned any other way, such as by a crash or a cancelled
capture, stay in the tree's `artifacts/` directory until the tree is deleted.

Artifact lookup uses the `read_artifact` tool:

| Argument | Meaning |
|---|---|
| `artifact` | The 64-character lowercase SHA-256 digest a hint names |
| `stream` | Optional declared stream name of a named-output manifest |
| `offset` | Optional zero-based line offset; default `0` |
| `limit` | Optional line count; default and maximum `2000` |

`read_artifact` is a base-layer tool. It needs no permission rule: it is always
sent to the model and every call is allowed, whatever the agent's rules, because
it can reach only artifacts stored in the calling session's own tree. A digest
that names content another tree stored does not resolve. Digests are
content-addressed SHA-256 references, not random secrets. Malformed digests
(bare `artifact://` URIs, the internal `sha256/` form, uppercase or short
digests) and invalid stream names are rejected. `read` reads only the
filesystem.

Artifact reads return the stored content without unwrapping JSON. Normal
single-output artifacts contain the original full text. Named-stream artifacts
contain their complete individual text. Compaction retry artifacts may contain serialized
tool-content JSON, including output text and metadata. Their markers identify
this format and expose usable IDs without requiring durable elision events.

Without `stream`, `read_artifact` reads stored text as-is, including manifest
JSON; it does not merge streams. `stream` selects a declared name from the generic
manifest. This is content-based and works for `results`, `diagnostics`, or any
other valid declared name, not just Bash streams. Unknown names and selection on
non-manifests are errors. Stored `ArtifactReference` values keep the internal
`artifact://sha256/<digest>` form, which is not a `read_artifact` argument.

Returned pages include `next_offset` metadata when more lines remain. A zero
limit fails; larger limits are capped at 2000 lines. Pages over the 2 MiB
terminal-result limit fail and must be requested with a smaller limit. Missing
and corrupt artifacts produce tool errors.

```json
{"artifact":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stream":"results","offset":0,"limit":200}
```

## Goal checklist tools

`goal_get` returns the current root goal's objective, status, revision, and entire
checklist. `goal_update { items }` replaces the entire ordered checklist;
each item contains only `description` and `finished`, with no item ID. The session
actor serializes replacements, and the last accepted update wins. The tool has no
`goal_id` or `expected_revision` parameter or lost-update protection. Its target is
the current/latest session goal when the engine actor accepts the update, so an
older run's update can intentionally affect a newly activated active or paused
goal. Both tools are available
only for root goals that are active or paused at run admission; lifecycle changes
do not change an already-admitted run's tool set. Updates reject if the current
goal is absent or terminal, even when the tool remains in an admitted run.
Engine-owned goal IDs and revisions remain; `SessionGoalLifecycleParams.goal_id`
and `expected_revision` apply to user lifecycle RPC controls, not model checklist
updates.

The model cannot set an objective, pause, resume, or cancel. Empty checklists
preserve the active or paused lifecycle; nonempty all-finished checklists complete
it even while paused. The root must verify evidence before marking items finished. Both tools
return the full state without display truncation. Their existing permission
actions are `read` and `write`, respectively, both with resource `goal:current`.
Ordinary permission matching, deny/ask rules, and the unmatched default still apply.

## Delegation and skills

Delegation tools start, inspect, and cancel owned subagent sessions. Skill
tools load configured skill instructions for the current turn. Their
availability and targets are derived from the frozen agent policy.

When delegation is available, its provider also freezes the currently eligible
target IDs and agent descriptions into the run's system prompt under
`<tool_instructions provider="builtin.delegate">`. The list uses the same depth
ceiling and enabled-target filtering as `delegate_subagent`; no section is added
when no target is available.

`delegate_subagent`, `get_subagent_result`, `stop_background`, and
`send_message` address subagents by a short tree-unique **handle** (`<agent_type>_<8 hex>`, e.g.
`explore_1a2b3c4d`) or a full session UUID. A handle is generated when the
session is created and resolves only within the caller's tree; a partial handle,
another tree's handle, or a fabricated value is rejected with a self-repairing
error that lists the live candidates as
`handle — description — status` plus their UUIDs. Detach notes and reports
pre-wrap the handle, for example
`[subagent session explore_1a2b3c4d] Each time its turn ends you get a <subagent_report handle="explore_1a2b3c4d">. …` and
`[subagent session explore_1a2b3c4d; completed; 79 lines; use get_subagent_result with session_id "explore_1a2b3c4d" for the full output]`.
The report previews the turn's final message per
[`[subagent_output]`](../engine/subagent_output.md), with its line breaks and
tabs kept and other control characters folded to spaces. A report over the limit
shows its first and last lines around a marker such as
`[… 200 lines omitted. Read more: get_subagent_result(session_id="explore_1a2b3c4d", offset=20)]`;
a complete one ends in `full output shown`.
Terminal and lifecycle results keep the UUID under the unified `session_id`
metadata key and add `handle`. `resume_session_id` accepts the same two forms,
and a terminal child woken by `send_message` reports as running until its new
turn ends.

## Agent messaging

`send_message` delivers a durable message to another session in the same
delegation tree. It replaces the removed `steer_subagent` tool; see the
[migration notes](../specs/agent-messaging.md#migration-notes). Its argument
object is strict: `to` is the recipient handle or full session UUID, `body` is
the markdown message text, and optional `mode` is `steer` (default) or `queue`.
The recipient argument is named `to`; the earlier `recipient_session_id`
spelling is not an alias and fails with `send_message:invalid_arguments`. A
non-UUID recipient is resolved as a handle in the sender's tree; if it matches
no live peer the send fails with the self-repairing `unknown subagent reference`
error rather than a `send_message:` code.

Calls use the `message` permission action. The resource label is the
recipient's relationship to the sender — `parent`, `child`, `sibling`, or `*`
for any other same-tree peer — derived from stored session-origin metadata.
Messaging is deny-by-default and the tool is hidden until an agent document or
session overlay declares a `message` `allow` or `ask` rule. Denied calls use
the standard policy-denial path and `ask` rules use the normal approval flow;
there is no messaging-specific permission error.

A successful result means **durably accepted**: the message is persisted in
the recipient's event log and survives a daemon restart. It does not mean the
recipient has seen or consumed the message. The result carries `message_id`,
the effective `mode`, and `recipient_state` (`running`, `queued`, or
`waking_finished`). A `steer` send joins a running recipient's next safe
model-request boundary; a `queue` send is claimed into the recipient's prompt
at the next safe boundary. Idle and finished recipients are woken through the
producer reconcile path, resuming the session with the message as input.
There is no inbox tool.

Failures are explicit tool errors with stable codes:

| Code | Meaning |
|---|---|
| `send_message:disabled` | `[messaging] enabled = false` |
| `send_message:invalid_arguments` | Missing, unknown, or wrongly typed arguments, including the removed `recipient_session_id` spelling |
| `send_message:invalid_body` | Empty body or body over `max_body_bytes` |
| `send_message:unknown_session` | Well-formed recipient UUID that names no session |
| `send_message:not_tree_peer` | Well-formed recipient UUID in a different delegation tree |
| `send_message:self_send` | Sender targeted its own session |
| `send_message:inbox_full` | Recipient has `max_pending_per_session` pending agent messages; nothing was admitted |
| `send_message:max_hops_exceeded` | The message chain exceeded `max_hops` |
| `send_message:inflight_full` | The directed pair already has `max_inflight_per_pair` unacknowledged messages |
| `send_message:engine_shutdown` | The send raced daemon shutdown |

Retries are safe: the idempotency key derives from the sender session, run,
and tool call, so a retried call returns the original `message_id` and
delivers once. Two loop-safety guards reject sends, and each can be disabled:
`max_hops <= 0` turns off hop counting (hop metadata is internal and never
appears in the delivered envelope; mail between a parent and its direct child
adds no hop, and subagent reports are not messages), and `max_inflight_per_pair = 0` turns off
the per-pair window. A pair message counts as unacknowledged while accepted,
admitted, claimed, or released, and is acknowledged once consumed or
discarded. See [Agent Messaging](../engine/messaging.md) for the settings and
the [agent messaging spec](../specs/agent-messaging.md) for the full contract.

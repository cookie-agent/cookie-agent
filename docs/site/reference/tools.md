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
| `read`, `bash`, `webfetch` | Parallel | Each call owns its execution and streaming state. |
| `write`, `edit` | Parallel | Matching prepared serialization keys serialize mutations to the same target. |
| `delegate_subagent` | Parallel | Delegate admission serializes durable child reservation and session creation internally. |
| MCP tools | Parallel | Each MCP server's service mutex serializes calls to that server; different servers can overlap. |
| `skill`, `get_subagent_result`, `steer_subagent`, `cancel_subagent` | Exclusive | These calls interact with session-scoped state. |
| Plugin and otherwise undeclared tools | Exclusive | External declarations cannot currently opt in. |

All parallel-eligible calls in the turn are dispatched together without a
fan-out limit. Exclusive calls run one at a time after the parallel calls have
finished. Results remain associated with their tool call IDs, while terminal
events are persisted in completion order.

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

Downloads are streamed to a fixed 16 MiB cap. Over-cap responses return the
fetched prefix with `truncated: true`, not an error. The cap has no input or
configuration parameter. Full results use the standard event log and result
store. The standard Bounded policy retains oversized output and sends a preview
to the model. `read` with an artifact URI pages the retained header and body by physical
line; its zero-based offset 5 starts at the body. Follow `next_offset` for
successive pages. Metadata is separate and is not paged. Grant
`read: {"artifact://*": allow}` for result paging.

Errors distinguish `invalid_url`, `redirect_error`, `timeout`,
`transport_error`, and `unsupported_content_type` (which names the content type).
`permission_denied` uses the standard policy-denial path and its structured
`tool_denied` reason.

## Filesystem tools

`read` accepts `filePath`, plus optional zero-based `offset` and positive
`limit`. The defaults return at most 2,000 lines or directory entries. Reads are
prepared and revalidated against the target before execution.

`write` accepts `filePath` and complete `content`. `edit` accepts `filePath`,
`oldString`, and `newString`, and requires the old text to identify one
unambiguous replacement. Both tools stage and validate filesystem mutations
before publication. Writes and edits to the same existing file, or writes to the
same absent path, share a prepared serialization key and cannot execute at the
same time. Because all calls are prepared before execution, an edit in the same
turn does not observe bytes written by an earlier sibling call; models must not
use same-turn calls to express a write-then-edit dependency.

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
the provider's inline limit (Bedrock: 3.75 MiB images, 4.5 MiB
documents, ≈18.7 MiB raw video; other families: 20 MiB images or 25 MiB
video). Bedrock receives supported video in the tool result. OpenAI-compatible,
Anthropic-compatible, Gemini, and Vertex models that declare video receive it
as a file in one emitted user turn immediately after the tool result; Gemini
and Vertex receive audio the same way. Each model also advertises per-kind
count limits, enforced per request. Media parts do not contribute to context
fit estimates, except video, which carries a flat conservative cost.

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
| OpenAI/compatible Responses, Azure Responses | Tool result | No | No | No |
| OpenAI Chat, Azure Chat | No | No | No | No |
| OpenAI-compatible Chat | No | No | User turn on capable Kimi/Qwen models | No |
| Gemini, Vertex Gemini | No | No | User turn | User turn |
| Cohere | No | No | No | No |

## Bash

`bash` accepts a complete `command`, an optional timeout in milliseconds, and
`interactive` (default `false`). Interactive calls can receive bytes or EOF
through the `run.tool_stdin` RPC while the call is active. Standard output and
standard error are streamed separately during execution and retained as
separate artifacts for the terminal result.

## Retained tool output

Tools declare `ToolOutputDeclaration::Single` or an ordered list of 1 to 8 named
streams. Names are unique, case-sensitive ASCII path segments, 1 to 64 bytes,
using letters, digits, `_`, `-`, and `.`; `.` and `..` alone are forbidden. The
same validator governs declarations and artifact stream suffixes.

The runtime captures accepted text chunks into locked temporary files, maintains
incremental hashes/counts and bounded previews, and publishes full artifacts at
completion. A single output has one artifact; named output has a generic manifest
and an artifact for each declared stream, including empty streams. Declaration
order determines model rendering. Bash uses the same named-output interface as
other tools; it does not build its own retained-output manifest.

Each stream receives its own configured `[tool_output]` line/byte preview limits.
Only truncated streams receive a `read(filePath="artifact://<digest>/<stream>",
offset=...)` hint, where the digest identifies the manifest. Single-output hints
omit the stream suffix and heading. A partial-line byte cut points back to that
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

The self-paginating `read` and `get_subagent_result` tools
declare an absolute truncation opt-out. Their requested page is returned in full
without artifact retention or truncation metadata, regardless of
`[tool_output]`. Callers bound these results with each tool's offset/limit
arguments. A requested page above the event schema's 2 MiB output limit fails
with a resource-limit tool error; it is never silently truncated. MCP and plugin
tools remain subject to normal truncation. External opt-out would require a
future extension-protocol capability and is not currently authorable.

Artifacts are content-addressed by SHA-256, deduplicated, and newly created with
mode `0600` on Unix. Their digest is verified when content is first read. A
missing or corrupt artifact produces a normal tool error and does not prevent
the engine or session from opening.

The idle janitor scans durable `events.jsonl` files for live artifact references
and removes unreferenced digest files only after a one-hour grace period. It also
follows generic manifests to their stream artifacts and skips malformed or
torn event lines. Publication guards exclude garbage collection until terminal
references have been appended and flushed. Concurrent publishers can coexist;
the artifact write mutex still protects individual retain/commit operations.
Deduplication refreshes artifact modification time. Temporary artifact and capture
files also hold an exclusive file lock while their writer is alive. Startup
cleanup removes only files older than one hour whose lock is immediately
available, so a silent long-running capture is not mistaken for abandoned work.

Artifact lookup uses the existing `read` tool:

| Argument | Meaning |
|---|---|
| `filePath` | `artifact://<64 lowercase hex digest>` or `artifact://<digest>/<stream>` |
| `offset` | Optional zero-based line offset; default `0` |
| `limit` | Optional line count; default and maximum `2000` |

**Artifact IDs grant access by possession.** The tool reads the
named artifact in the configured project store without checking session ownership,
tool-call visibility, or whether the reference survived a revert. An ID learned
from another session is usable. IDs remain content-addressed SHA-256 references,
not random secrets; anyone who can compute an artifact's digest can address it.
Ordinary `read` approval/preparation rules still apply. Artifact resources use the
public URI as their permission label and an artifact resource identity, never a
filesystem-path permission. Dispatch occurs before filesystem normalization or
existence checks. Malformed artifact URIs do not fall back to filesystem reads.
Bare hashes, internal `artifact://sha256/` URIs, extra path segments, traversal,
queries, fragments, and uppercase digests are rejected. Ordinary filesystem read
formatting, media handling, and permissions are unchanged.

Artifact reads return the stored content without unwrapping JSON. Normal
single-output artifacts contain the original full text. Named-stream artifacts
contain their complete individual text. Compaction retry artifacts may contain serialized
tool-content JSON, including output text, metadata, and references to original
truncation artifacts. Their markers identify this format and expose usable IDs
without requiring durable elision events. Follow any nested truncation reference
to retrieve the original full output rather than its serialized preview.

Without a suffix, an artifact URI reads stored text as-is, including manifest
JSON; it does not merge streams. A suffix selects a declared name from the generic
manifest. This is content-based and works for `results`, `diagnostics`, or any
other valid declared name, not just Bash streams. Unknown names and selection on
non-manifests are errors. There is no separate `stream` argument. Stored
`ArtifactReference` values retain the internal `artifact://sha256/<digest>` form.

Returned pages include `next_offset` metadata when more lines remain. A zero
limit fails; larger limits are capped at 2000 lines. Pages over the 2 MiB
terminal-result limit fail and must be requested with a smaller limit. Missing
and corrupt artifacts produce tool errors. Artifact-ID access does not change
garbage-collection lifetime: unreferenced retry artifacts remain subject to the
existing one-hour grace period.

```json
{"filePath":"artifact://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/results","offset":0,"limit":200}
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

Delegation tools start, inspect, steer, and cancel owned subagent sessions.
Skill tools load configured skill instructions for the current turn. Their
availability and targets are derived from the frozen agent policy.

When delegation is available, its provider also freezes the currently eligible
target IDs and agent descriptions into the run's system prompt under
`<tool_instructions provider="builtin.delegate">`. The list uses the same depth
ceiling and enabled-target filtering as `delegate_subagent`; no section is added
when no target is available.

# Tool Reference

Cookie Agent publishes tools according to the active agent's permissions and the
capabilities of the selected model. Tool argument objects are strict: unknown
fields and wrong types are rejected.

## Filesystem tools

`read` accepts `filePath`, plus optional zero-based `offset` and positive
`limit`. The defaults return at most 2,000 lines or directory entries. Reads are
prepared and revalidated against the target before execution.

`write` accepts `filePath` and complete `content`. `edit` accepts `filePath`,
`oldString`, and `newString`, and requires the old text to identify one
unambiguous replacement. Both tools stage and validate filesystem mutations
before publication.

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
   [provider matrix](../guide/providers.md#media-in-tool-results)). If it does
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

## Bash

`bash` accepts a complete `command`, an optional timeout in milliseconds, and
`interactive` (default `false`). Interactive calls can receive bytes or EOF
through the `run.tool_stdin` RPC while the call is active. Standard output and
standard error are streamed separately during execution and retained as
separate artifacts for the terminal result.

## Retained tool output

For most tools, when a terminal result exceeds the configured line or byte
limit, the event stores a bounded preview and an
`artifact://sha256/<digest>` reference to the full original output. Compaction
may also replace older bulky previews with artifact references. Retained files
live below the project state directory at `projects/<project-hash>/artifacts/`.

The self-paginating `read`, `read_tool_result`, and `get_subagent_result` tools
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
follows retained Bash manifests to their stream artifacts and skips malformed or
torn event lines. Event appends may remain buffered for up to 8 ms, but a newly
written digest is younger than the grace period; deduplicating an existing digest
refreshes its modification time before the event append. Garbage collection and
artifact retain/commit operations share the artifact write mutex, so a pending
append cannot race deletion of its retained bytes.

`read_tool_result` reads retained content from a prior visible tool call in the
same session:

| Argument | Meaning |
|---|---|
| `tool_call_id` | Required UUID of the prior tool call |
| `offset` | Optional zero-based line offset; default `0` |
| `limit` | Optional line count; default and maximum `2000` |
| `stream` | For retained Bash manifests, `stdout` or `stderr` |

Resolution prefers the original truncation artifact, then a compaction-elision
artifact, then the inline terminal output. This means compaction cannot make a
full truncation artifact unreachable. Reverted tool calls and calls from other
sessions do not resolve. Returned pages include `next_offset` metadata when more
lines remain. Pages over the 2 MiB terminal-result limit fail and must be
requested with a smaller limit.

## Delegation and skills

Delegation tools start, inspect, steer, and cancel owned subagent sessions.
Skill tools load configured skill instructions for the current turn. Their
availability and targets are derived from the frozen agent policy.

When delegation is available, its provider also freezes the currently eligible
target IDs and agent descriptions into the run's system prompt under
`<tool_instructions provider="builtin.delegate">`. The list uses the same depth
ceiling and enabled-target filtering as `delegate_subagent`; no section is added
when no target is available.

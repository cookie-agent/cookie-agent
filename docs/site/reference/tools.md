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

Tool results may include image or PDF attachments. Attachments are validated,
content-addressed, and supplied to models as file parts rather than embedded in
text output.

## Bash

`bash` accepts a complete `command`, an optional timeout in milliseconds, and
`interactive` (default `false`). Interactive calls can receive bytes or EOF
through the `run.tool_stdin` RPC while the call is active. Standard output and
standard error are streamed separately during execution and retained as
separate artifacts for the terminal result.

## Retained tool output

When a terminal tool result exceeds the configured line or byte limit, the
event stores a bounded preview and an `artifact://sha256/<digest>` reference to
the full original output. Compaction may also replace older bulky previews with
artifact references. Retained files live below the project state directory at
`projects/<project-hash>/artifacts/`.

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
sessions do not resolve. Returned pages include `next_offset` metadata when
more lines remain and are capped by the 2 MiB terminal-result limit.

## Delegation and skills

Delegation tools start, inspect, steer, and cancel owned subagent sessions.
Skill tools load configured skill instructions for the current turn. Their
availability and targets are derived from the frozen agent policy.

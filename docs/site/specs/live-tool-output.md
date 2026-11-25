# Reading a Background Call's Output While It Runs

Status: **implemented** (2026-11-25; no protocol change). The user approved
the recommended answer to every question in
[Decisions](#7-decisions) on 2026-11-25, and added that a negative `offset`
works in every tool that pages by line, not only `read_artifact`.

## 1. Problem

A model that starts `bash` with `background: true` (or whose call the user
moves to the background) gets an immediate result naming a job, and then
nothing until `<background_result>` arrives when the call ends. For a
training run, a dev server, or a long build, the model cannot check whether
the job is making progress, has hung, or printed the error it is waiting
for. Its only options are to wait blind or to stop the job.

Claude Code solves this by writing a background command's output to a file
and naming the file in the result (`Output is being written to:
/tmp/…/tasks/<id>.output`); the model reads or `tail`s it whenever it likes.

Cookie already has everything a live view needs. Every chunk a tool emits
goes to the engine's output capture (`runtime/output_capture.rs`), which
keeps each stream's head preview and rolling tail in memory and, once a
stream outgrows its preview, its whole content in a temporary spill file
plus at most 1 MiB not yet flushed. Clients already see this output live
through the call's `OutputHub`. Only the model cannot reach it.

## 2. Goal

- The model can read a running background call's output, by job ID, at any
  time: a snapshot like the result it would get if the call ended now, and
  any stream paged by line, from the start or from the end.
- The same handle keeps working after the call ends, so a model paging
  through live output does not hit an error the moment the job finishes.
- It works for every background-capable tool that streams output (bash,
  runtime and plugin tools), on every machine, with no change to the runtime
  protocol: the capture lives in the engine, whichever machine ran the
  command.

Out of scope: waiting for a pattern or for the job to end (a `Monitor`-style
tool, a possible follow-up); ACP agents' own background calls, whose output
cookie does not capture; foreground calls (the model is blocked on them
anyway; a user can move one to the background, and then it is covered).

## 3. Design (recommended): job IDs in `read_artifact`

### 3.1 The handle

`read_artifact`'s `artifact` accepts, besides a digest, the job ID of a
background call of the session's own tree (`job_1a2b3c4d`), the same set
`stop_background` reaches. No new tool is published, and the permission
story does not change: `read_artifact` already reads only output the
session's own tree retained.

The immediate result gains one sentence, before the `stop_background`
sentence:

```text
Started in the background as job job_1a2b3c4d. You will get a
<background_result job="job_1a2b3c4d"> with its result when it finishes; do
not wait for it or run it again. Read its output so far with
`read_artifact(artifact="job_1a2b3c4d")`. Stop it with
`stop_background(job="job_1a2b3c4d")`.
Process 48213
```

It is added only for calls whose tool has captured output (every tool but
`delegate_subagent`, whose handle already reads through
`get_subagent_result`).

### 3.2 A snapshot: no `stream`, `offset`, or `limit`

`read_artifact(artifact="job_1a2b3c4d")` returns the call's output rendered
exactly as its result would be if it ended now: each stream's head and
tail previews within the `[tool_output]` limits, with the usual omission
hints, under a status header.

```text
[job job_1a2b3c4d: bash `python train.py`; running for 12m 04s]

[stdout]
epoch 1 loss 2.31
…
[… 1842 lines omitted. Read more: `read_artifact(artifact="job_1a2b3c4d", stream="stdout", offset=21)`]
epoch 37 loss 0.412
epoch 38 loss 0.409

[stderr]
```

The snapshot comes from the head preview and rolling tail the capture keeps
in memory, so it costs no disk read. A call with no output yet shows
`(no output yet)` under the header. This is the model's "how is it going?"
call and the one the immediate result names.

### 3.3 Paging: `stream`, `offset`, and `limit`

With any of `stream`, `offset`, or `limit` (a call with named streams must
name its `stream`), the read pages the stream's content so far exactly as an artifact page
does: numbered lines, the 4 KiB line cut, the result size limit, and the
continuation note. A page of a running job ends with a note that more may
come: `[job job_1a2b3c4d is still running: 1862 lines so far]`, and `metadata.running` is
`true`.

The content read is what the finished artifact will hold:

- a stream that still fits its preview: the preview;
- a spilled stream: its spill file up to the bytes written, then its
  pending bytes, read under the capture's lock so a flush cannot interleave;
- a stream past the 128 MiB retain limit: the kept head, then the marker
  line counting the bytes dropped so far, then the rolling overflow tail,
  as `close_overflow` will write it.

Reads run on the blocking pool like other artifact reads.

### 3.4 Reading from the end: negative `offset`

Live output is read mostly from the end, so `offset` may be negative,
counting lines back from the last: `offset=-50` reads the last 50 lines
(and up to `limit` from there). This applies to every tool that pages by
line: `read` (files and directory listings), `read_artifact` (artifacts and
jobs), `memory_read`, `get_subagent_result`, and `read_asset`. `0` stays an
error. A negative offset past the first line starts at line 1. The page's
numbered lines and the metadata's `offset` give the line it started at, so
the model can page backwards or forwards from it.

The shared paging code (`runtime_api::text_tools`) takes a `LineRequest`
that becomes a `LineWindow` once the text's lines are counted; only a
request from the end counts, so a file or artifact read from the end is
read twice. `ToolArtifacts::read_page` takes the request, a Rust-only
interface; no wire type changes.

### 3.5 After the call ends

When a background call ends with its output stored, its job ID keeps
resolving, to the output it published (the manifest or single artifact its
`<background_result>` names, or the text of a stream that fit its preview),
among the last 64 background calls of the daemon to end, until the daemon
restarts. A count rather than "while the session is loaded" keeps the
bookkeeping in one place; a finished capture frees its buffers and keeps
only its rendered output and the text of streams with no artifact. A job ID
that no longer resolves is an error pointing at the `<background_result>`'s
digest. A finished job's page has `metadata.running: false`, and its
snapshot header says how it ended (`completed after 3m 10s`).

A call stopped or killed (revert, session deletion, shutdown) resolves the
same way to whatever its capture published, marked incomplete.

### 3.6 Compaction and history

`read_artifact` calls on job IDs are artifact reads: the compaction retry
already omits their output. Nothing new is stored durably: a job read is an
ordinary tool call and result.

### 3.7 What changes where

| Area | Change |
|---|---|
| `output_capture.rs` | A live read of one channel's content so far, and a snapshot render from the in-memory previews. |
| `background_tasks.rs` | Job → capture lookup for running calls; job → published output for finished ones while the session is loaded; the extra sentence in the immediate result. |
| `artifacts/reads.rs`, `read_artifact` | Accept a job ID; negative `offset`; the running note and `metadata.running`. |
| Tool reference | `read_artifact`'s description and schema; the Background calls section. |
| Protocols | None. The client, plugin, and runtime protocols are unchanged; only a model-facing tool description and schema change. |

## 4. Alternative: a log file on the machine that runs the command

The Claude Code way, in cookie: `bash` tees both pipes, in arrival order,
to a file on its own machine (say
`$XDG_STATE_HOME/cookie-agent/jobs/<tool-call-id>.log`, mode `0600`), and
its detach note names the file. The model reads it with `read`, or
`tail`/`grep` through `bash` on that machine.

For it: models know this pattern well, `grep` and `wc -l` work on it, and
the user can `tail -f` it from their own terminal.

Against it:

- The runtime does not know whether a call is in the background (the host
  strips `background`, and the user can move a call at any time), so it
  must write the file for every bash call, and the full output is then
  stored twice: in the file and in the engine's capture.
- Someone must delete the files. The runtime does not see sessions end, so
  it needs its own age-based sweep.
- It works for `bash` only. Other streaming tools would each need their own
  file, against the rule that the engine, not tools, owns output capture
  ([Tool output streams](tool-output-streams.md)).
- stdout and stderr merge into one file, losing the separation the retained
  output keeps.

Recommendation: section 3. If `grep` over live output turns out to matter, a
later `[bash] log_files = true` can add the file on top without changing
section 3.

## 5. A practical catch: buffered output

A program writing to a pipe often buffers its output: Python block-buffers
stdout (about 8 KiB) when it is not a terminal, and so does C stdio. A
training script that prints one line per epoch may show nothing for a long
time, live view or not. Claude Code has the same problem. `bash`'s
description says so in one sentence and names `python -u` and `stdbuf -oL`;
the command's environment is not changed.

## 6. Phases

One phase: the capture's live read and snapshot, job resolution, negative
offsets, the immediate result's sentence, docs. Tests only where existing
ones break (per the user's standing rule), unless asked.

## 7. Decisions

All of 2026-11-25, by the user, each the recommended answer:

1. **Approach.** Section 3: job IDs in `read_artifact`, read from the
   engine's capture. No log file per bash call.
2. **Handle.** `read_artifact`'s `artifact` takes job IDs; no separate tool.
3. **Tail reads.** A negative `offset`, in every tool that pages by line
   (the user's addition).
4. **Buffered output.** One sentence in `bash`'s description; no environment
   change.
5. **Finished jobs.** A job ID keeps resolving after the call ends (bounded
   as section 3.5 says).

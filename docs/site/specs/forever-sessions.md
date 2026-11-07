# Sessions that run forever: recent events in memory, opening without a full parse

Status: **approved** by the user on 2026-11-06. Being drafted on the `forever-sessions` branch;
not on `main`. See [section 11](#11-decisions).

## 1. Problem

**The goal is a session that can run forever** (the user, 2026-11-06): it is not a
memory shortage today, but nothing a long-lived agent does should cost more because its
log is longer. So a term that is small now but grows with the log counts against this.

A resident session holds every event of its log in memory. A long ACP session's
old events serve no model request, as the agent holds its own context, and
compaction is the agent's own command, so the log never gets a checkpoint. They
are kept only because the code reads the whole list.

Measured on 2026-11-06 with a counting allocator, on copies of the six largest
logs of yunhao-labserver's store, all ACP sessions converted to history format 3:

| Log | Events | Live: `EventLog` | Live: with projection | Peak on opening | Events newer than the boundary | Largest run |
|---|---|---|---|---|---|---|
| 12.4 MB | 6,945 | 37.0 MB | 38.5 MB | 55.1 MB | 0.09 MB | 5.0 MB |
| 11.2 MB | 9,286 | 27.9 MB | 28.3 MB | 54.8 MB | 0.00 MB | 17.3 MB |
| 10.2 MB | 6,105 | 24.4 MB | 24.9 MB | 41.9 MB | 0.01 MB | 8.1 MB |
| 10.1 MB | 6,449 | 24.4 MB | 24.9 MB | 41.8 MB | 0.01 MB | 4.3 MB |
| 9.7 MB | 9,813 | 26.7 MB | 27.0 MB | 52.1 MB | 0.04 MB | 9.5 MB |
| 9.3 MB | 7,800 | 25.2 MB | 26.1 MB | 41.6 MB | 0.01 MB | 4.2 MB |

What this settles:

- **The events are nearly all of it today.** The line index, raw lines, and validation
  maps add 0 to 3.2 MB, and the projection, with its per-run agent copies, 0.4 to
  1.5 MB. But those grow with the log too: at 100 times this length they would be
  hundreds of MB. Section 6 deals with them.
- **A session needs almost none of them.** Under 0.1 MB at rest, and about one turn's
  events during a run (section 3.2). The largest run holds up to 17 MB in memory, which
  a window per turn does not keep.
- **Opening costs more than holding.** Parsing peaks at about 1.7 times what stays.
  Section 4 removes the parse.
- **The tree load keeps little.** It parses and drops children: with 20 subagents
  its peak was 15.3 MB against 8.1 MB without. But it parses the root too, which is
  probably why viewing a session parses its log at all (not measured apart).

These are the storage crate's numbers. The daemon's RSS rose about 61.5 MB on opening
the largest of these, so the rest is the parse's peak, which the allocator keeps until
`malloc_trim`, and whatever the engine adds.

**Why not cap idle sessions instead?** A cap frees this at the cost of a parse when the
session comes back, which is what a forever agent does on every wake: a session evicted
after 30 idle minutes parses its whole log each time a cron job or a message wakes it. So
a cap makes it worse, and it is left out.

## 2. The API

`EventLog::event_snapshot()` goes. Its 125 production uses each move to one of:

| Call | Gives | Audit's count |
|---|---|---|
| `recent()` | The events since the log's window start | 36 |
| `state()` | A small fold kept up to date on append (section 3.1) | 51 |
| `history()` | An iterator over events read from disk, one at a time, unless the log holds them all | 30 |
| `model_context_events()` | Pinned records and the window, for a native model's context | 8 |

The compiler finds every use, so none is missed, and each states what it needs. Until
a log is windowed, it holds every event and these cost nothing. ACP sessions are
windowed first (section 3), then native ones (section 5).

## 3. ACP sessions

### 3.1 `state()`

Each is a fold over one event at a time, in the log's order, so opening a log
computes it once, an append updates it, and a revert computes it again.

| Fold | What reads it today |
|---|---|
| Per agent: the latest `acp_session_started`, the detach markers since it | `lifecycle::plan`, each turn; `acp_host_hints` |
| The ACP settings: option values, starting options, commands, context, prompt context | `options.rs`, `acp/compaction.rs`, `stored_acp_context`, `recorded_prompt_context` |
| Inputs not yet taken, with their bodies | `input_since`, `recall_input`, `drain_queued_input` |
| Producer messages, with the bodies of unsettled ones only | `input_since`, `woke_since`, `producer_input_waiting`, `goal_tools_offered` |
| Pending approvals, each response's id, and each approval's latest user decision | `approval/projection.rs`, `resolve_interrupted_direct`, `cancel_run`, `user_answer` |
| Latest values: `SessionModelState`, memory snapshot, published tools, permission overlay epoch | `start_run`, `acp_prompt_context`, `latest_model_tool_names` |
| "Once" facts: a seed, a title, a skill body, a link, a delegation's completion | `admission.rs`, `install_prepared_skill`, `delegation.rs`, `producers.rs` |

A "once" fact is a set of keys, which grows with the log but stays small: a key per
delegation or skill. `recorded_option_values` has the most subtle rule: it stops at an
earlier session of another agent or declaration.

### 3.2 The window

After each committed turn, a windowed log's window start moves to the earlier of that
turn's record and its `input_through_seq`. So a run of any length holds about one turn's
events, and a run that never ends is bounded too (the user's choice, 2026-11-06).

Measured on the store's 12 largest logs, on disk, without what blobs hold (11,222 turns):

| | Median | 99th percentile | Largest |
|---|---|---|---|
| Between two committed turns | 7.9 KB | 23 KB | 60 KB |
| A whole run, for comparison | | | 8 MB |

Reads that a run-long window would have served move to `state()`, as they look further
back than one turn:

| Fold | What reads it today |
|---|---|
| The current run's `run_started`, with its prompt | `run_agent_prompt`, `acp_machines_note` |
| Open tool calls: the started record of each | `tool_call_owner`, `signal_approval_needs_you`. Up to 6 were open at once, and one stayed open across 28 turns |
| Inputs of the current run not yet applied | `pending_inputs`, `hold_inputs_to_steer`, `promote_steered_inputs`, `void_runless_pending_inputs` |
| Whether this run was cancelled or failed, and its last attempt | `run_cancelled_recorded`, `run_failure_event` |
| Attached context since the last run ended | `add_attached_context` |

The loop guard and the doom-loop check read a run's last calls, but only for native
sessions (section 5.1). Phase 1 confirms each of these against the code,
and any other that a turn-long window breaks shows in section 7's tests as a fold that
differs.

A log is windowed while its session's agent is an ACP agent. A held snapshot keeps
what it holds until it goes, as the list is behind an `Arc`.

### 3.3 `history()`

It streams the file a record at a time and keeps nothing, so its memory does not depend
on the log's length. Its time does: a read of old events is a pass over the log. A
caller keeps only what it needs, as an older page keeps its window and folds the header
as it goes, which also gives the page's state for free. That serves pages, `event.read`,
revert, fork, delete, and rare lookups such as `attachment_record`.

The parsed read-only cache was the first choice, being the least new code, but its newest
log stays whatever its size, and a forever agent's reaches hundreds of MB. It stays for
sessions that are not windowed.


### 3.4 Frozen context

An ACP session's context is **frozen for the life of that ACP session** (the user,
2026-11-06), and never refreshed: it has no checkpoint, and the agent holds its own context.
Code at `b339c902e` already does most of this:

- `acp_prompt_context` composes it once, when a new ACP session starts: the agent document's
  body, attachments block, tool instructions, skill listing, and memory snapshot. It goes as
  the appended system prompt or ahead of the first prompt, and `prompt_context` records it.
- A resume sends what was recorded, so a daemon restart or a reinstall does not change it.
  `AGENTS.md` is the harness's own.
- A new ACP session is the only thing that composes again: a lost session, or a change of agent
  or declaration, as a new session has been told nothing.
- A skill the agent loads over the bridge goes to it as a tool result, which the agent then
  holds. So nothing is read again at the agent's own `/compact`.

For this spec, that means `state()` holds the latest `prompt_context` per agent, a bounded text,
and a skill's duplicate check is a set of (name, digest of body), as old `SkillLoaded` records
leave the window. No record is pinned. Phase 1 confirms that a branch after a revert or fork
keeps what was recorded.

### 3.5 Revert

A revert's cut can fall before the window. It reads `history()`, recomputes `state()`
over the prefix, and holds the events since the new latest boundary.

## 4. Opening without parsing the whole log

Nothing about opening needs the old records as such: it needs what they fold to. So a
windowed log saves that beside it, and an open parses only what comes after. This
reopens the resume from a checkpoint rejected on 2026-10-14, at the user's request of
2026-11-06: the reason then was that readers needed the whole list, which section 2
removes for ACP sessions.

### 4.1 `state.json`

Written next to `events.jsonl`, private, as a temporary file renamed over the old one
and never synced, as `view.json` is. It holds, **as of a record at a turn's boundary**, and names where the window starts,
which is at or before that record:

| Part | From | Today's source |
|---|---|---|
| Where: the record's `seq`, `timestamp`, and byte offset; `history_format`; `STATE_VERSION` | | As `view.json` |
| `ValidationState`, its taint, diagnostics, `render_only` | `validate_loaded` | Whole list |
| `EventIndex` and the prompt block and deferral folds | `EventStorage` | Whole list |
| `LineIndex`, and the raw lines no record reproduces | `load_event_jsonl`, `RawLines` | Whole file |
| The projection: meta, usage, `runs`, fold state | `projection_fold_with` | Whole list |
| `state()`'s folds | Section 3.1 | |
| The tree load's harvest: grants, delegation records, producer state | `harvest_records` | Whole list |

None of these types serialize today, and some maps have tuple keys, so each needs
serde derives and its maps written as lists, in JSON, with no new dependency. These are
internal types, not history: this file is a cache, and it stands outside the
compatibility rules.

**Its size is not measured.** What it holds took 0.4 to 4.7 MB of heap on the logs in
section 1. Phase 4 measures the file.

### 4.2 Writing it

After a committed turn, when over 1 MiB of log came since the last write, so a long run
is covered and a turn does not pay for a write, and when a session leaves memory. Outside
every lock, and only by the process that owns the log. A crash loses nothing: the log stays the only source of truth.

### 4.3 Opening

1. Read `state.json`, and check it as `view.json` is checked: its `history_format`, and
   that the line at its offset is the record it names, whole.
2. Read the log from the window's start. Records up to the snapshot's join the window
   as they are; each after it also goes through the same steps a live append takes:
   validation, index, projection, folds.

**Any mismatch means folding the whole log from its start.** That covers a missing or
unreadable file, a revert, a converted or rewritten log, and a line the loader would
repair. A revert also removes the file. A torn final record is handled by step 2.

That fallback streams too, through the same steps, dropping each record once it has
been folded, so it costs time and not memory. Reading the whole file into a buffer, as
the loader does today, would not do at this scale.

Line by line, the tail is read as the loader reads it, including blobs.

### 4.4 Across builds

A file stays valid across builds, as the store's formats do, so a reinstall costs
nothing (the user's question, 2026-11-06). It names a `STATE_VERSION`, and a different
one means folding from the start and writing the file again. There is no converter and
no store-wide step, as this is a cache: the log is what is kept.

The difference from `history_format` is what can go wrong. A log's meaning is fixed when
it is written, while this file holds what a fold **computed**. A fold whose logic changes
leaves the file's shape alone and its contents wrong, quietly, so a version someone must
remember to raise is too weak by itself. A test raises the alarm instead: a committed
digest of what every fold gives at every turn boundary of the golden logs. A change to a
fold or a field that alters any of it fails that test until `STATE_VERSION` rises and the
digest is recorded again. A change the golden logs do not reach is not caught, which is
why the golden ACP scenario is built to reach every fold (section 7).

### 4.5 What this does elsewhere

- **The tree load** reads `state.json` for the root and for each child that has one,
  and so parses none of them. Native children parse as today.
- **Viewing** then parses nothing until the user reads older pages. Skipping the tree
  load for viewing is no longer needed for ACP trees.
- **Damage in old records is found later** (section 11): an open no longer
  validates records before the offset. A damaged one shows when `history()` reads it.
- **A read-only open** uses the file too, and writes none.

## 5. Native sessions

A native session really does read its log: each model request is built from it. So its
window cannot move per turn. **It moves when a checkpoint commits**, and memory follows
the model's context, not the log: a native forever agent stays bounded because compaction
keeps its context bounded. A session with compaction off stays whole.

### 5.1 What a model request needs

`assemble_model_context` reads the latest checkpoint and selects from the whole list.
Read at `b339c902e`, it needs:

| Needed | Why it can be old | In the design |
|---|---|---|
| Events from the summary's `recent_from_seq`, or after a provider window's `source_through_seq` | Both start before the checkpoint record | The window starts at the earlier of them, so a switch to a model that cannot use the window still has the summary's range |
| Every `SkillLoaded` before the boundary | Of any age today, with its full body | **Not carried** (section 5.2): the skill tool reads fresh when called |
| The latest `AGENTS.md` and memory records | Latest of their kind, from the run's start | **Read fresh at each checkpoint** (section 5.2) |
| Producer admissions not yet consumed | Of any age | Pinned records, and the fold of section 3.1 |
| What `close_event_dependencies` pulls in: an applied input's submission, a started call's turn, a terminated call's start | "Late tool completions can refer to events before the retained range" | At a checkpoint, pin the closure. Afterwards, open calls' records and inputs not yet applied are pinned, so any later reference resolves |
| Latest `NativeReplayCut`, `ToolCallLinked`, `ToolOutputElided`, whether any producer message exists | `assemble_history_with_replay`'s forward pass over everything | The latest cut is a fold. Links and elisions matter only for calls in view, which the window and pins hold |
| The loop guard's last 64 calls, the doom-loop count | Read back to a run's start, which can precede the window | Folds, as a ring of digests |

So `model_context_events()` returns **pinned records and the window, in order**, and
assembly keeps working on a slice. Its producer fold and replay cut come from `state()`.
Only producer admissions and back-references are ever pinned.

### 5.2 Fresh at each checkpoint, and skills when called

A native session's whole context refreshes at a checkpoint, and nothing is pinned for it (the
user, 2026-11-06; ACP sessions are the opposite, section 3.4).

| Part | When it is read | Today |
|---|---|---|
| System prompt, with its skill listing | At the checkpoint | The same |
| Tool round | At the checkpoint | The same |
| `AGENTS.md`, from every coding attachment | At the checkpoint | At the next run, so a compaction mid-run keeps the old one until then |
| Memory snapshot | At the checkpoint | The same as `AGENTS.md` |
| A skill's body | **Only when `/skill` or the skill tool is called** | Read when called, then pinned forever |

**A loaded skill is a regular turn.** `invoke_skill` already reads the skill afresh on every call.
A body in the summary's kept range stays verbatim like any other turn; an older one is summarized
like any other, and nothing is injected again (the user, 2026-11-06). After a checkpoint, the
listing says what is available, and a model that still needs a skill loads it again, which gets
its current version. A forever agent would otherwise hold every skill it ever loaded.

How other harnesses do this, read on 2026-11-06:

| Harness | After compaction |
|---|---|
| Codex | Not injected again, in the code read. The `<skill>` body is a contextual fragment left out of the kept user messages, and the listing returns with the initial context |
| opencode | A regular message. A `/skill`'s text goes with its user message and the `skill` tool's as a tool result, and `recentUserMessages` strips `skills` from what it keeps |
| DeepSeek harness | A regular message, as far as read: its compaction code and design doc name no skills |
| Claude Code | **Injected again**, per its docs: each skill's latest invocation, first 5,000 tokens each, 25,000 in all, newest first. Its docs also list "the conversation was compacted" as why a model stops following a skill |

The cost is that a model following a long skill may lose detail in the summary, and must load it
again to get it back. That is accepted.

What follows from that:

- **The duplicate check looks since the latest checkpoint.** It now looks at the whole log and
  records `skill_invocation_noted` instead of a body, so a load after compaction would show the
  model nothing.
- **The summarizer's prompt changes.** It stops being told to omit loaded skills, and is still
  told to omit `AGENTS.md`.
- **What a load granted stays.** A skill's permission overlay and model, in memory by session
  (`skills_runtime`), survive a refresh point (the user, 2026-11-06), so no approval prompt
  appears mid-task because of compaction.

**`AGENTS.md` and memory** are read as compaction starts, as their size counts in its budgets, and
held in memory. They are written just before the commit, so a compaction that fails leaves no
trace. If one cannot be read, the latest record is written again, from `state()`, so a machine
that is away does not lose its instructions, as a refresh already does for memory.

**Existing logs.** A commit gains an optional `context_fresh`, which is additive. Selection pins
older records only under a commit without it, so existing logs read as they did, and hold those
records until their next checkpoint. No format bump is needed.

**In-stream compaction** (ChatGPT sign-ins) commits inside a request, so phase 6 confirms that it
can read these first.

### 5.3 The check that matters most

A wrong selection changes what the model is sent, and quietly. So **the assembled request
from pinned records and the window must equal the one from the whole log**, at every turn
boundary: `ModelContext` whole (history, native window, replay decisions, assistant
prefixes), `project_summary_context`, `compaction_tail_candidates`,
`checkpoint_retained_history`, and `get_history`. In tests, every request also assembles
from the whole log and asserts the two match, as the per-append reference fold does. That
also protects prompt caching and reasoning bound to its prefix, as an unchanged request
cannot break either.

The golden logs cover compaction with and without a provider window, replay, delegation,
revert and fork. A new long scenario adds several compactions, skills loaded before them, a
call finishing after a checkpoint, a producer message pending across one, and a model
switch.

### 5.4 Switching agents

- **ACP to native.** With no checkpoint, a native model's context is the whole log: its
  first request reads it through `history()` and the log holds it until the next
  checkpoint. Nothing is lost, and it is no worse than today.
- **Native to ACP.** The window narrows at the next committed turn.

### 5.5 The approval reviewer gets its own log

The reviewer applies only to native sessions (ACP permissions spec). Today it has no context of
its own: its conversation is rendered from the session's log on every escalated approval, "nothing
about it is stored separately", and `gather` passes over the whole log each time, holding what it
gathers. Its one reset is an **epoch**: when the next turn would not fit the window, it starts over
from a fresh render within its budgets.

It gets its own context and durable log, and the same refresh flow (the user, 2026-11-06):

- **A hidden child session** holds its conversation: a `user` turn per review, an `assistant` turn
  per verdict, as they were sent. So window, `state()`, and `state.json` apply unchanged, and its
  prompt's bytes survive a restart by being stored rather than by being rendered again.
- **Each review adds only its delta:** it reads the session's events since the last review's cut,
  from the nearest run boundary, so its time follows the gap between reviews and not the log.
- **An epoch is its refresh point.** Its system prompt and `AGENTS.md` are read fresh, and its fresh
  turn is rendered from its own log's kept entries within today's budgets. **It makes no model call
  to summarize** (decision 1).
- **A delegated session's opening** (`root_messages_until`, a pass over the root's log) is written
  to its log once, when the log is created.
- **Existing sessions** have none. The first review creates it with a fresh turn, from one pass
  over the session's log, and misses the provider's cache once. No conversion is needed.
- Usage stays recorded on the parent as `internal_agent_usage_recorded`, and clients keep hiding the
  child from the session tree.

### 5.6 `state.json`

The same file, adding the pinned records, written after a checkpoint commits and per 1 MiB
of log. A session with no checkpoint yet has a whole window, so opening reads the log, which
its context limit bounds.

## 6. What else grows with the log

Trimming events leaves these, each small today and each linear in the log. `state.json`
holds them, so they cost twice: in memory, and in writing the file.

| Term | Grows per | Bound |
|---|---|---|
| `SessionProjection.runs`: agent copy, input, final text | Run | Only unfinished runs, and the latest; older ones read from `history()` |
| `ProjectionFoldState.tool_names` | Tool call | Only the window's |
| `tool_starts`, `terminated_tools`, `elided_tools`, call owners | Tool call | Only calls of unfinished runs. A termination for an older call is refused by a new "before the floor" rule |
| `turns`, `turn_models`, `usage_turns`, `attempts` | Turn | Only the window's, under the same rule |
| `admissions` | Input | Only unsettled, plus a digest set bounded by age if needed |
| `runs` attribution, `approval_owners`, `internal_runs`, `delegations` | Run or call | Only open ones, plus any a settled record can still name |
| Taint | Damage found | Stays: rare |
| "Once" facts of section 3.1 | Delegation, skill | Keys only. Linear but tiny; accept, and revisit by measurement |
| Producer fold | Message | Unsettled bodies only (section 3.1), and settled ones as an id |
| `LineIndex`, raw lines | Record | Dropped for the windowed part: a sparse index of run boundaries, 1 entry per run, and `history()` finds the rest by reading |

**Each bound narrows what an append can be validated against.** A record naming something
older than the floor can no longer be checked. That is the real cost here, so each
needs an answer to "can a valid record still name this?" before it shrinks, from the
append rules, not from assumption. Phase 5 does these one at a time, by measured size.

## 7. Correctness

A fold that differs from the scan it replaced would quietly give the wrong options,
session, or prompt, and a snapshot that differs from a parse would do so on every later
turn, so:

- Today's scans stay as test-only references. Every fold is compared with its
  reference **at every event** of a log.
- **A snapshot and tail against a full parse, at every turn boundary** of the golden
  logs: projection (`assert_projection_equivalent` exists), `ValidationState` and its
  taint (`PartialEq`), index, line index, folds, harvest. In tests, every open through
  a snapshot also parses whole and asserts the two are equal, as the per-append
  reference fold does.
- A new golden ACP scenario, as none of the fixtures has one, with several agents,
  a detach, a fork, a revert, queued input, and producer messages.
- Run once, not committed: the same comparisons on copies of the store's 27 large
  ACP logs, before anything is trimmed.
- A crash at each step of writing and reading `state.json`, and one that is stale,
  truncated, of another `STATE_VERSION`, or from before a revert: each folds from the
  start.
- The digest test of section 4.4.
- For native sessions, section 5.3's check of the assembled request.
- A test that a normal ACP turn calls `history()` zero times.
- A session whose window moved gives the same answers as one reopened from disk.

## 8. Phases

Each ships on its own and leaves things working.

1. **The golden ACP scenario and the folds**, with their reference tests. `state()`
   is used where a fold exists, while the list stays whole. This alone stops
   refolding the producer projection per poll.
2. **The API split**: the rest of the uses move, and `event_snapshot()` goes. The
   list is still whole, so behaviour is unchanged.
3. **The window**, for ACP sessions. Measure live heap and RSS after `malloc_trim`.
4. **`state.json`** and the open from it, the tree load included. Measure the file's
   size, the open's peak and time, and RSS on opening.
5. **The bounds of section 6**, largest first, on a log grown synthetically to 100 times
   today's largest. Done when live heap, `state.json`, and open time stay flat as it
   grows.
6. **Native selection, not yet trimming**: refreshing at each checkpoint and not carrying skills
   (section 5.2), the pinned set, the folds of section 5.1, and the check of section 5.3 running in
   every test, on the new long scenario. The frozen context spec's sections 6.1 and 6.4, and the
   system prompt reference, change with it.
7. **The native window and its `state.json`.** Only 1 of this store's 82 native logs has a
   checkpoint, so its numbers come from the scenario, and from a real native forever agent
   once one runs.
8. **The reviewer's own log** (section 5.5), comparing its verdict turns with today's render on
   the golden logs.

## 9. Left out

- **The idle cap**: see section 1.
- **The log's format.** `state.json` is a new cache; the log and blobs do not change.
- **The log's size on disk.** It grows forever, and so does a pass over it for old
  pages. Sealed compressed chunks with their saved state would bound both, and fit this
  design, as a chunk boundary is a window start. A separate spec when a log's size or
  old pages' time calls for it.

## 10. Expected effect, and its limits

All estimates until phases 3 and 4 measure them. On the largest session:

| | Today | After |
|---|---|---|
| Live heap, idle | 38.5 MB | About 5 MB, plus folds not yet measured |
| Live heap, during a run | The same | The same: one turn's events, and what its open calls hold |
| Peak on opening | 55 MB | The `state.json` parse and the window: not measured, expected single-digit MB |
| Parsed on opening | 12.4 MB of log | The window |

Older pages, and a session's first open after `STATE_VERSION` rises, take time
proportional to the log, but no memory beyond a page.

A native session holds its context since the last checkpoint, and its pinned records,
whatever its log's length. That is not measured.

What a single result holds still counts while it is in the window. It is bounded by the
size of one result, and goes with the next turn.

## 11. Decisions

From the user, 2026-11-06:

1. Damage in old records is found when they are read.
2. `history()` streams from disk and keeps nothing.
3. `state.json` is kept across builds by a version (section 4.4), rather than tied to the
   exact build.
4. An append naming something older than the validation floor is refused (section 6).
5. An ACP session's window moves per committed turn, not per run.
6. Native sessions are in this spec.
7. A native session's context is read fresh at each checkpoint, with nothing pinned for it. An ACP
   session's stays frozen for the life of that ACP session.
8. A loaded skill is a regular turn: read when called, summarized like any other, and not injected
   again. What a load granted stays across refresh points.
9. The approval reviewer gets its own context and durable log, and the refresh flow.
10. A reviewer's refresh makes no model call to summarize: it renders from its own entries.
11. The reviewer's log is a hidden child session.
12. The order is ACP, then native, then the reviewer.

## 12. Implementation notes

**Phase 1** (2026-11-07), read against the code at `d7c8c15f2`:

- **`user_answer` reads a user's decision**, not a tool call, so it moved to the approval row of
  section 3.1. The decision can be older than one turn: an approval waits as long as the user does.
- **`context_composition` returns before reading anything for an ACP run**, so it left section
  3.2's table: phase 6 deals with it.
- **`acp/compaction.rs` reads two things, neither of them the current run's start.** Its commands
  are a section 3.1 fold. Its wait wants one run's outcome, which need not be the latest, as a
  queued input can start another: it now waits on the projection's run status and reads the log
  once that run has ended. Phase 2 makes that read `history()`.
- **Open calls answer for open calls.** `tool_call_owner` and `signal_approval_needs_you` can also
  ask about one that ended (a second termination, which then fails). The fold answers first and a
  pass over the log answers the rest, which phase 2 makes `history()`. Likewise `run_agent_prompt`
  for an older run, and an approval's record once it settled: only open ones are folded.
- **Input since a boundary lets go of what a committed turn covered**, so it assumes
  `input_through_seq` never falls. Nothing validates that. No real log breaks it (234 logs, with
  `input_boundary_fell`), and only a test's hand-written commit did.
- **Frozen context, confirmed:** a fork copies the `prompt_context` record, a revert that keeps the
  agent's reply keeps it too, as the record precedes the reply, and the session branched off either
  gets it unchanged, composing nothing
  (`a_branch_after_a_fork_or_a_revert_keeps_the_recorded_context`).
- **Not per agent today.** Section 3.4 has `state()` hold `prompt_context` per agent, and it does
  (`AcpAgentState::prompt_context`), but a resume or a branch sends the latest of *any* agent
  (`acp_prompt_context`, as `recorded_prompt_context` did): a session that goes from agent A to B
  and back resumes A with B's context. Phase 1 changes no behaviour, so this stays until decided.
- **More readers a per-turn window breaks**, all in phase 2's list: `last_assistant_message`,
  `fork_support`, a delegation's `visible` check and its parent's termination
  (`delegation/reports.rs`), a background job's start and acceptance (`background_tasks.rs`),
  `after_runs`, and the parent's creation record in `acp/subagents.rs`.
- **What the golden ACP scenario does not reach:** `delegated_context_seeded`, as `inherit_context`
  fails on an ACP parent, and no golden log has one. The fold's tests and the live delegation tests
  compare it.

# Sessions that run forever: recent events in memory, opening without a full parse

Status: **approved** by the user on 2026-11-06. Built on the `forever-sessions` branch, all eight
phases (section 12, with what remains open at its end); not on `main`. See [section 11](#11-decisions).

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

| Call | Gives | Audit's count | Where they landed (section 12) |
|---|---|---|---|
| `recent()` | The events since the log's window start | 36 | 1 |
| `state()` | A small fold kept up to date on append (section 3.1) | 51 | 76 |
| `history()` | An iterator over events read from disk, one at a time, unless the log holds them all | 30 | 43 |
| `model_context_events()` | Pinned records and the window, for a native model's context | 8 | 7 |

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

After each committed turn, a windowed log's window start moves to that turn's record. So a run
of any length holds about one turn's events, and a run that never ends is bounded too (the user's
choice, 2026-11-06). This first said "the earlier of that turn's record and its
`input_through_seq`", which section 12 explains is a whole run.

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

The loop guard reads a run's last calls, but only for native sessions (section 5.1). The
doom-loop check reads them for ACP sessions too, so it is a fold (section 12). Phase 1 confirms
each of these against the code,
and any other that a turn-long window breaks shows in section 7's tests as a fold that
differs.

A log is windowed while its session's agent is an ACP agent: from the first turn such an agent
commits, until a run starts with a native one. It also takes a file that holds what it lets go of, and since
phase 4 that goes for a read-only log too (section 12). A held snapshot keeps what it holds until it goes, as the list is
behind an `Arc`.

### 3.3 `history()`

It streams the file a record at a time and keeps nothing but up to 64 KiB of lines read ahead, so
its memory does not depend on the log's length. Its time does: a read of old events is a pass over the log. A
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

Wherever its cut falls, as a fold cannot be taken back, a revert reads what stays as a pass,
recomputes `state()` and validation over it, and holds the events since the new latest boundary.
It does this before it cuts the file, so a failed read changes nothing. It ends a pass still reading
what it cuts with `EventLogError::HistoryCut`.

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
| `LineIndex`, and the raw lines no record reproduces, which a line naming blobs no longer counts as (section 12) | `load_event_jsonl`, `RawLines` | Whole file |
| The sequences validation refused, which a pass leaves out too | `EventStorage::refused` | Whole list |
| Where a view cache starts, with the headers it can move to | `EventStorage::view_start` | Window |
| The projection: meta, usage, `runs`, fold state | `projection_fold_with` | Whole list |
| `state()`'s folds | Section 3.1 | |
| The tree load's harvest: grants, delegation records, producer state, a parent's facts | `state()`, as a fold of its own (section 12) | Whole list |

None of these types serialized, and some maps have tuple keys, so each has serde derives and
its maps written as sorted lists, in JSON, with no new dependency, which also makes two equal
states the same bytes. These are internal types, not history: this file is a cache, and it
stands outside the compatibility rules.

**Measured by phase 4:** 1.9 to 3.2 MB on the logs of section 12, about a quarter of the log.

### 4.2 Writing it

After a committed turn, when over 1 MiB of log came since the last write, so a long run
is covered and a turn does not pay for a write, and when a session leaves memory, which the
store's release counts as. Only the process that owns the log writes it, and only a log with a
window has one, a native log once a checkpoint moved its window (section 5.6). The session's slot and the log's locks are
held to copy what it holds; serializing and writing take no lock. A crash loses nothing: the log
stays the only source of truth. **A log shorter than 1 MiB has none** (`STATE_MIN_LOG_BYTES`, the user's choice of
2026-11-08): folding it from its start takes a few milliseconds, and on yunhao-labserver's store the
79 windowed logs under 1 MiB had 16.5 MB of files against 29.6 MB of log, the smallest larger than
their logs.

### 4.3 Opening

1. Read `state.json`, and check it as `view.json` is checked: its `history_format`, and
   that the line at its offset is the record it names, whole.
2. Read the log from the window's start. Records up to the snapshot's join the window
   as they are; each after it also goes through the same steps a live append takes:
   validation, index, projection, folds.

**Any mismatch means folding the whole log from its start.** That covers a missing or
unreadable file, a revert, a converted or rewritten log, and a line the loader would
repair. A revert also removes the file. A torn final record is handled by step 2. A file behind
its log that still matches is no mismatch: what came after it is the tail, whatever its length.

That fallback streams too, through the same steps, dropping each record once it has
been folded, so it costs time and not memory. Reading the whole file into a buffer, as
the loader does today, would not do at this scale. **Two kinds of log still read that way,**
at the peak of section 1 (section 12): one with damage the file does not already
answer for, and one of an older history format, which converts in memory.

Line by line, the tail is read as the loader reads it, including blobs. The log's first record
comes from its first line, not from the file.

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
- **A read-only open** uses the file too, and writes none. It holds only its window, through
  the file or through the fallback, so a pass of it checks at its end that the owner has not cut
  the file under it.

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
| Producer admissions not yet consumed | Of any age | Pinned records, chosen by the fold of section 3.1, with each one's acceptance, and the acceptance of a message admitted in the window (section 12) |
| What `close_event_dependencies` pulls in: an applied input's submission, a started call's turn, a terminated call's start | "Late tool completions can refer to events before the retained range" | At a checkpoint, pin the closure. Afterwards, open calls' records and inputs not yet applied are pinned, so any later reference resolves |
| Latest `NativeReplayCut`, `ToolCallLinked`, `ToolOutputElided`, whether any producer message exists | `assemble_history_with_replay`'s forward pass over everything | The cut that reaches furthest is pinned, and so are the latest `run_started` and its latest `system_prompt_composed` (section 12). Links and elisions matter only for calls in view, which the window and pins hold |
| The loop guard's last 64 calls, the doom-loop count | Read back to a run's start, which can precede the window | Folds, as a ring of digests |

So `model_context_events()` returns **pinned records and the window, in order**, and
assembly keeps working on a slice. `state()` chooses what is pinned: open calls, inputs
not yet applied, messages not yet consumed. Only producer admissions and back-references
are ever pinned, with the few records assembly reads beside them (section 12).

### 5.2 Fresh at each checkpoint, and skills when called

A native session's whole context refreshes at a checkpoint, and nothing is pinned for it (the
user, 2026-11-06; ACP sessions are the opposite, section 3.4).

| Part | When it is read | Today |
|---|---|---|
| System prompt, with its skill listing | At the checkpoint, inside a run; between runs, at the next run (section 12) | At the next run, so a run that never ends keeps the prompt it started with |
| Tool round | At the checkpoint, for the request right after it | At the request after that one |
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

**The system prompt** is composed again right after a checkpoint inside a run commits, as the run's
admission composes it: the agent's body as its document reads now, the runtimes, attachments, tool
instructions, and skills blocks, and the plugins' system-prompt hooks. When it differs from the
prompt in force it is recorded as `system_prompt_composed`, and the run's later requests carry it
(section 12). The run's agent otherwise stays as it was admitted: its permissions, models, and
limits come from the run's frozen policy, which only the next run freezes again.

**`AGENTS.md` and memory** are read as compaction starts, as their size counts in its budgets, and
held in memory. They are written just before the commit, so a compaction that fails leaves no
trace. If one cannot be read, the latest record is written again, from `state()`, so a machine
that is away does not lose its instructions, as a refresh already does for memory.

**Existing logs.** A commit gains an optional `context_fresh`, which is additive. Selection pins
older records only under a commit without it, so existing logs read as they did, and hold those
records until their next checkpoint. No format bump is needed.

**In-stream compaction** (ChatGPT sign-ins) commits no checkpoint, as built: the server's
compaction items stay in their turn's native replay. Its backstop is an ordinary checkpoint,
which reads these first like any other (section 12).

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
switch (as built, section 12).

### 5.4 Switching agents

- **ACP to native.** Its first `model_context_events()` after the native run's start reads the log
  whole, and the log then holds what the model's context needs: with no checkpoint, the whole log,
  until the next checkpoint or an ACP agent's next committed turn; after an earlier checkpoint, the
  context since that one at once (as built, section 12). Nothing is lost, and it is no worse than
  today. Asked of a session whose agent is still ACP, as `get_history` can, it reads whole and the log
  keeps its window.
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

As built (section 12, phase 8): the child is `reviewer/events.jsonl` in the reviewed session's own
folder, where nothing that lists or walks sessions looks, and its records are
`reviewer_turn_recorded`. A review reads the session's log after the previous turn's cut, the nearest
point a pass can start, not a run boundary. The epoch's first turn holds what the log kept and is
where its window starts, as a checkpoint is a native model's. A revert drops the turns whose verdicts
it cuts, and a fork copies those it keeps.

### 5.6 `state.json`

The same file, adding the pinned records, written after a checkpoint commits and per 1 MiB
of log, and as the session leaves memory. A session with no checkpoint yet has a whole window, so
opening reads the log, which its context limit bounds. As built (section 12): the file holds the
pinned records themselves and says the window is a model's context, and an open reads the window
from the file as an ACP agent's does and puts the pinned records before it.

## 6. What else grows with the log

Trimming events leaves these, each small today and each linear in the log. `state.json`
holds them, so they cost twice: in memory, and in writing the file.

| Term | Grows per | Bound (as built in phase 5, section 12) |
|---|---|---|
| `SessionProjection.runs`: agent copy, input, final text | Run | Unfinished runs, those a pending call or an open delegation holds, and the latest 16 that ended; older ones read from `history()` (`run_outcome`) |
| `ProjectionFoldState.tool_names` | Tool call | The last 16 committed turns' |
| `tool_starts`, `terminated_tools`, `elided_tools`, call owners | Tool call | Open calls, the latest 16 that ended, every call that ended since a native model's context started (phase 7), and the owners of the last 16 turns. A record naming an older one is refused: the "before the floor" rule |
| `turns`, `turn_models`, `usage_turns`, `attempts` | Turn | The last 16 turns; unfinished attempts and the latest 16 that ended, of the runs kept, under the same rule |
| `admissions` | Input | The latest 256 |
| `runs` attribution, `approval_owners`, `internal_runs`, `delegations` | Run or call | Open ones, whatever their age, and the latest 16 of each that settled |
| Taint | Damage found | Stays: rare. A damaged log's validation keeps every map, as its taint excuses records by what they hold |
| "Once" facts of section 3.1 | Delegation, skill | Keys only. Linear but tiny; accepted |
| Producer fold | Message | Unsettled messages, and the latest 16 that settled; of the rest, an 8-byte digest of each key with its message's ID, kept for as long as the log |
| `LineIndex`, raw lines | Record | From the window, or an earlier view cache start, on, and up to 512 marks before, 64 KiB apart or more; `history()` reads the file from the nearest. Raw lines: only those no record reproduces |
| The harvest's delegation records | Delegation | **Not bounded** (section 12): a tree load rebuilds the delegation registry from each one's records |

**Each bound narrows what an append can be validated against.** A record naming something
older than the floor can no longer be checked. That is the real cost here, so each
needs an answer to "can a valid record still name this?" before it shrinks, from the
append rules, not from assumption. Phase 5 did these one at a time, by measured size, and
section 12 gives each answer.

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
   the golden logs (as built, section 12).

## 9. Left out

- **The idle cap**: see section 1.
- **The log's format.** `state.json` is a new cache; the log and blobs do not change.
- **The log's size on disk.** It grows forever, and so does a pass over it for old
  pages. Sealed compressed chunks with their saved state would bound both, and fit this
  design, as a chunk boundary is a window start. A separate spec when a log's size or
  old pages' time calls for it.

## 10. Expected effect, and its limits

Estimates until phases 3 and 4 measure them; phase 3 measured the first row (section 12). On the
largest session:

| | Today | After |
|---|---|---|
| Live heap, idle | 39.5 MB | Measured by phase 3: 7.4 MB |
| Live heap, during a run | The same | The same: one turn's events, and what its open calls hold |
| Peak on opening | 55 MB | Measured by phase 4: 8.5 MB |
| Parsed on opening | 12.4 MB of log | The window, and 3.2 MB of `state.json` |

Older pages, and a session's first open after `STATE_VERSION` rises, take time
proportional to the log, but no memory beyond a page.

A native session holds its context since the last checkpoint, and its pinned records,
whatever its log's length. Measured by phase 7 on a synthetic native log (section 12): 1.0 to 2.7 MB
of live heap from 1 to 100 times this store's largest log, where one with compaction off holds 36 MB
at 1 time and 362 MB at 10.

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
- **Per agent, fixed in phase 2.** Section 3.4 has `state()` hold `prompt_context` per agent, but a
  resume or a branch sent the latest of *any* agent, as `recorded_prompt_context` did: a session
  that went from agent A to B and back resumed A with B's context. Every reader now takes the
  agent's own (`LogState::acp_prompt_context(agent)`), and the latest of any is gone:
  a resume or branch reads the agent it opens, `title_repeats_context` the agent that sent the title,
  and what a subagent inherits the parent's agent
  (`a_resume_after_another_agents_session_sends_its_own_context`). A session that starts afresh
  (`new`, `subagent`) clears its agent's entry, as it has been told nothing, so a resume of one
  that has not had its first prompt sends nothing rather than the context of the session it replaced.
- **More readers a per-turn window breaks**, all in phase 2's list: `last_assistant_message`,
  `fork_support`, a delegation's `visible` check and its parent's termination
  (`delegation/reports.rs`), a background job's start and acceptance (`background_tasks.rs`),
  `after_runs`, and the parent's creation record in `acp/subagents.rs`.
- **What the golden ACP scenario does not reach:** `delegated_context_seeded`, as `inherit_context`
  fails on an ACP parent, and no golden log has one. The fold's tests and the live delegation tests
  compare it.

**Phase 2** (2026-11-07), read against the code at `d7e313fb1`. `event_snapshot()`, `events()`, and
`snapshot_and_state()` exist only under `test-support`, so the compiler proves no production use.
The counts in section 2 are of the lines that call each, in the engine, the store, and permissions.

- **`recent()` has one caller, far from the 36 the audit expected.** Nearly every reader turned out
  to look further back than a turn, or to be answerable by a fold. The one is an ACP claim, which
  reads the log's tip. A claim serving a native model takes `model_context_events()`, and the claim
  says which by the run's policy, as the two must be taken at the claim's moment.
- **The doom-loop check and the cache-expiry rule are not native-only**, against section 3.2 and
  section 5.1's table. `await_user_approval` is the ACP permission path and the bridge's too, so the
  check runs on every approval of an ACP session, and `stamp_acp_cache_ttl` puts a cache TTL on ACP
  usage, which `cache_expiry` then plans from. Each read back further than a turn, and at a cost on
  an ordinary turn, so each is now a fold (`doom_loop_repetitions`, `latest_request`) with its scan as
  its reference. `latest_request`'s scan stopped looking for when a request went out 4,096 events back
  from the tip and fell back on the usage event's time; the fold has no such bound. That is the one
  difference in what a reader gets, and only for a session with that many events after its last
  request.
- **More folds than section 3.1 lists,** each with its scan as its reference, as a pass on an ordinary
  turn or on every open would defeat the window: the run streak (`after_runs`, on every wake), the open
  internal agents and the open approvals (`reconcile_session_with`, on every adoption), the approval
  that settled last (a response reads back what it just settled), a session's own ACP sessions
  (`own_records`, as a delete holds the tree's lock), the latest turn, and the latest fallback.
- **A pass is `history()` or `history_range()`**, which yield `Result<Arc<StoredEvent>, _>` and hold no
  lock. A scan from the tip becomes a pass keeping the latest answer (`last_map_ok`), as a file reads
  forward. **Phase 3 must read the file inside `History::next`, off the worker (`off_worker`), bounded
  at the tip it saw when asked, and cope with a revert cutting the file under a pass in flight.** Until
  then it hands out records of the list, so what it costs here says little of what it will cost.
- **`creation()`** answers for a log's first record. Phase 3 keeps that record whatever the window.
- **`model_context_events()` is fallible** because phase 3 must read a windowed log whole for it, for
  the ACP-to-native switch of section 5.4. `get_history`'s assembled view, compaction, the loop guard,
  `context_composition`, and the reviewer's own session take it; `get_history`'s full view takes
  `history()`, as its answer holds everything anyway.
- **The reviewer reads its own session as `model_context_events()`, which phase 7 narrows** before
  phase 8 gives it its own log. Its render reads the whole log today, so phase 7 changes what it sees
  unless phase 8 comes first. Its ancestors take `history()`, as a native child's root can be an ACP
  session (`RootOpening`, `delegation_prompt`).
- **A window holds the newest run while it is worked out.** `TailWindowReader` keeps the events from the
  last run boundary whose events on do not fit the budget, as `tail_window_after` needs them, and
  `read_view` holds the same today. So opening a session costs a run's events for a moment, and a run
  that never ends is the one case a window is not bounded by its budget. An older page keeps what fits
  its budget, in whole events: a cut result is held whole until the page is cut.
- **A window, a page, and the view cache read the list while the log holds it** (`History::tail_window`,
  `older_page`, `view_cache_start`), and in tests also as a pass, asserting the two match. A pass must
  measure every record, as what fits is known only at the end, where the list walks back and stops. On
  a 12 MB log of 9,072 events, in a release build: the newest window 12 ms against 233 ms, and the view
  cache 8 ms against 238 ms. The branch is one method, `History::held`, which phase 3 makes answer
  `None` for what comes from disk.
- **`write_view` then costs a pass at every run's end,** at the 238 ms above plus parsing, and phase 3's
  "no `history()` on a normal ACP turn" fails on it. **Phase 3 must make it incremental,** from what the
  last write held. An open of a windowed session costs the same pass, unless the view cache answers.
- **Other passes on the way to a normal ACP turn that phase 3 should look at:** the first title
  (`maybe_generate_session_title`, until a title commits), and `view_event_count` (two passes, for a
  reader's first forward page when more follow).
- **Passes under a lock.** A fork reads its source under the store's `mutation` lock
  (`fork_buffered`, `acp_detached`), and holds the copy in memory to write it whole. A revert reads under
  the session's actor. A final forward page that lost a race reads under the session's slot. A refold
  on `NeedsRefold` reads under the slot too. None holds the log's `events` mutex. Phase 3 should move the
  fork's reads before the lock. A delete takes no pass, as `acp_own_records` is a fold.
- **What else reads the whole list inside the log, for phase 3:** `visit_json`, `truncate`, and the
  validation and index rebuilds, by `storage.all` or `raw.lines`.

**Phase 3** (2026-11-07), read against the code at `109e91052`.

- **The window starts at the committed turn's record, against section 3.2's first text.** An ACP
  agent's `input_through_seq` is the tip when its prompt went out, and stays there for the prompt's
  whole turn. So "the earlier of the record and `input_through_seq`" is always the latter, and
  holds the whole run: on the store's 6 largest logs, 2.3 MB at the median turn on the worst log,
  and 7.5 MB at most, where the measurement in section 3.2 and decision 5 are per turn. Nothing needs
  the difference: `recent()`'s one caller reads the tip, and what an uncovered input needs is
  in `state()`. Section 3.2 is corrected. The user's decision 5 is what this follows, but it is
  a change of the approved text, so it is named here.
- **Whether a log is windowed is read from its records,** so a reopening agrees. A committed turn of
  the synthetic provider `acp` sets the start, a committed turn of any other model clears it, and a
  `run_started` of any other model clears it, as what comes next is a native model's. A log also has to be
  persisted, this process's, not converted, and not render-only; and a log whose last write failed,
  which may hold part of a record, lets nothing more go.
- **Read-only logs stay whole, in this phase and until phase 4 opens them.** Their owner can cut or
  rewrite the file with no word to this process, where a pass reading an owned file is told. Opening
  parses them whole anyway, and the cache of parsed logs lets them go when idle. A take-over
  (`owned_from_read_only`) narrows at its fold, as any owned open does. A session not resident whose
  view cache does not answer, or a page of one, therefore still holds its whole log for a while.
- **An owned open parses whole, folds, and only then narrows** (`refold` calls `narrow_window`), so its
  projection is not a second and third parse. Phase 4 replaces this.
- **`History`** gives the creation from memory, then what precedes the window at the moment it was
  asked from the file, then the window it saw, which is an `Arc`. So nothing appended later reaches
  it. It reads lines through the loader's own `LineReader`, so a repair, a blob, and a skip read the
  same, and leaves out what validation refused at load (`EventStorage::refused`). Off the worker, in
  visits of up to 64 KiB of lines, as a `block_in_place` per record would cost more than the read.
- **A revert under a pass in flight.** The log counts cuts, before it cuts the file and again as it
  installs what stays, and a pass checks the count after each read, so what it hands out was read
  before the cut. A pass with file left to read then ends in `EventLogError::HistoryCut`, whatever the
  distance from the cut, which errs towards refusing; what it already gave stays true of the log as
  it stood. **A reader sees that as an error, not a retry:** an older page or `event.read` racing a
  revert fails, and the client is about to be told `Rewound` anyway. A fork reads again under the lock
  instead. Nothing retries it otherwise.
- **`write_view` reads nothing of a windowed log, and the cache starts a little earlier than it did.**
  `CacheStartFold`, which an append feeds, notes the header at a run boundary every quarter of the
  budget, and those are the starts it moves to, once what follows one no longer fits. So a cache starts
  up to about a quarter of the budget (and a run) before the latest start it could have: a cold open
  reads that much more, and `view.json` is written that much less. The first version kept what
  `state_events` reads since the start and moved exactly; it held 5.6 MB of the largest log's 12 MB,
  159 `run_started` at 30 KB each. `view_start` comes from the list at the first narrowing, and from
  the pass at a revert. `display_view::cache_start` stays for a log that holds its events.
- **A resident windowed session's newest window** reads from the cache's start (`newest_pass`) and not
  over the log: 11 ms against 243 ms on the largest log, holding 1.9 MB for the moment against 5.8 MB.
  When no boundary is yet known whose events on do not fit, it takes the pass.
- **The first title** reads two folds, `opening_inputs` (the first 16 inputs) and `first_input_of_run`,
  with a pass only when the configuration asks for more than 16, or for a run that is not the latest.
- **`view_event_count`** answers the full view from the validation state's record count, and the
  display view in one pass: `DisplayCount` no longer needs a second one to apply what the first found.
  It stays a pass for a reader's first forward page when more follow: the reader is about to pass over
  the log itself. A fold of the display count needs per-call state, so it is left for when it shows.
- **Zero passes on a normal ACP turn**, enforced from the second turn on
  (`a_normal_acp_turn_asks_for_no_pass_over_its_log`, and with a log past the view cache's budget
  `a_windowed_session_places_its_view_cache_without_a_pass`, and with a title that will not commit
  `an_untitled_acp_session_asks_for_its_title_without_a_pass`). The first turn's passes all read what
  it holds. **What still reads the file on an ACP session**, by listing the calls of a scripted session and reading
  their callers: a background delegation's completion and the agent's `/compact`, now each only from its own
  run on (`a_delegation_and_a_compaction_read_only_their_own_run`), a tree load's harvest of a resident
  root, once, `reconcile_parent`, revert, fork, older pages, and `event.read`.
- **Fork.** `read_fork_prefix` and the `acp_detached` hook run before `mutation`, reading through a pass
  and validating too. Under the lock it compares the log and `cut_count`, and reads again if a revert
  won. `write_whole` still reads the source's lines whole with `RawLines::read`, under the lock, and the
  buffered fork holds the copy: +25 MB and 209 ms on the largest log, for a fork through half of it. Not this phase's to change; phase 5 should stream it.
- **Revert.** The pass of section 3.5 is one parse, and `refold`'s two passes are two more, so a revert
  of the largest log through half of it takes 300 ms and holds 2.1 MB. `reverted_children`, `history_cut`,
  and `AcpSessions::read` are passes of their own before it. Linear and rare, so left.
- **`visit_json`** of a windowed log streams the file's lines, as what it appended has no raw line. The
  rest (`truncate`, and the validation rebuilds of a refused append and a failed write) go through a pass
  (`fold_prefix`, `rebuilt_validation`), holding `append` and no other lock.
- **`NeedsRefold` did not come up** on the scripted turns, usage records included, and would cost two
  passes under the session's slot. A real ACP log's usage should be looked at when phase 5 measures.
- **A double revert with `ACP_FIXTURE_DELETE`** fails the next turn with `session/fork failed: no such
  session`, the agent's session having been deleted. It does so with the window off too, so it is not
  this phase's. Whether a real agent does the same was not looked at.
- **Tests.** Every store append of a committed turn, in every test, compares the log with a reopening of
  its file (`assert_window_reads_as_reopened`: records, the window, every fold against its scan,
  validation, line index, where the view cache starts and its header) and the projection with
  a reopening's. So the golden ACP scenario, when recorded, and all ACP tests are compared at each committed
  turn. `a_golden_log_appended_again_answers_as_reopened_at_each_committed_turn` does that for the frozen
  logs, with windows, pages, forward pages, lines, counts, metadata, and an owned reopening too;
  `a_long_scripted_session_holds_one_turn_and_answers_as_its_file` is the longer scripted run. Each narrowing
  in a test also reads what it let go of and compares it with what was held.
- **What phase 4 must know.** `state.json` needs two parts section 4.1 did not list, added there: `refused`
  and `view_start`. Its `window` is the `index.window_start` of the fold, so nothing more is needed to agree.
  The open's parse is now the whole of the peak, and the tree load's `harvest_records` over a resident
  windowed root is a pass. `opening_inputs` holds up to 16 inputs with their bodies. `History` assumes
  `lines` is whole or absent, and scans from the start without it, so a sparse index (section 6) has
  to give `file_range` its starts another way. Section 6's list gains `CacheStartFold`, bounded, except
  for the 256 records `StateEventsFold` holds before it settles.

Measured on 2026-11-07, on scratch copies of this machine's six largest ACP logs, converted to history
format 3 and opened by `begin_write` and `commit_adoption` in a release build. Live heap and peak by a
counting allocator, in MB, each log in a fresh process. "Before" is `109e91052`:

| Log | Events | Live, before | Live, after | Peak, before | Peak, after | Held, after | Open, before | Open, after |
|---|---|---|---|---|---|---|---|---|
| 12.5 MB | 7,033 | 39.47 | 7.37 | 55.91 | 55.97 | 5 records | 255 ms | 269 ms |
| 11.2 MB | 9,286 | 28.28 | 5.37 | 54.77 | 54.84 | 3 records | 190 ms | 223 ms |
| 10.2 MB | 6,105 | 24.91 | 5.73 | 41.91 | 41.95 | 4 records | 166 ms | 187 ms |
| 10.1 MB | 6,449 | 24.92 | 6.51 | 41.78 | 41.84 | 4 records | 156 ms | 176 ms |
| 9.7 MB | 9,813 | 26.98 | 5.25 | 52.08 | 52.16 | 3 records | 162 ms | 176 ms |
| 9.3 MB | 7,800 | 26.20 | 5.20 | 41.57 | 41.63 | 4 records | 388 ms | 447 ms |

A pass over the whole log of the same six, from the file: 104 to 195 ms, holding 0.7 to 2.0 MB beyond
what the log holds. `tail_window` as a whole pass: 151 to 243 ms, holding 4.5 to 17.4 MB, the newest run
while the window is worked out, as phase 2 said; a resident session takes `newest_pass` instead.

The daemon, a release `cookie daemon` on a scratch `HOME` holding the same copies, each in a fresh process:
RSS after `malloc_trim`, the rise on `session.resume`, and the rise left after a second `malloc_trim`, in MB:

| Log | Idle, before | Idle, after | Rise, before | Rise, after | Left after trim, before | Left after trim, after |
|---|---|---|---|---|---|---|
| 12.5 MB | 108.4 | 104.2 | 45.4 | 48.3 | 44.4 | 14.7 |
| 11.2 MB | 108.0 | 104.3 | 37.3 | 37.6 | 34.3 | 13.5 |
| 10.2 MB | 109.1 | 103.0 | 29.2 | 31.2 | 27.4 | 12.9 |
| 10.1 MB | 107.9 | 104.0 | 30.3 | 29.5 | 28.6 | 14.9 |
| 9.7 MB | 107.5 | 103.1 | 32.5 | 34.8 | 30.6 | 13.8 |
| 9.3 MB | 107.1 | 105.2 | 43.0 | 42.9 | 31.1 | 13.4 |

**So live heap fell by 74 to 81 percent and RSS left after a trim by 48 to 67 percent, and the peak did
not move, as opening still parses the whole log.** Phase 4 is what takes the peak away.

- **A trim was forced for this, and the daemon would not have run it.** `FreedMemory` needs 32 MiB free
  beyond what it settled on, and a single open frees less than that of what `mallinfo2` counts: over two
  janitor passes the real daemon, with no trim forced, stayed where the open left it (148.5 MB and 141.6 MB
  on the two largest, from about 103 MB idle). So until something else frees enough, opening one windowed session costs the daemon
  RSS what it did. What is free is reused by the next open, so it does not add up. Phase 4 should
  make this moot, and the threshold was left alone.
- **These are this machine's logs, not section 1's**, though the sizes match to a few percent. The
  39.47 against section 1's 38.5 is phases 1 and 2's folds, on another copy.
- One run each: treat a few percent, and the open times, as noise.

**Phase 4** (2026-11-07), read against the code at `2f7ccbe9d`.

- **What the file holds, as built:** `{version, history_format, session_id, at: {seq, timestamp,
  offset}, folds}`, with `folds` as section 4.1 lists. `STATE_VERSION` is 1, in
  `events/state_file.rs`. `at` is the log's tip when the file was written, which at a session's
  leaving memory need not be a committed turn: every fold holds at every record, and the per-append
  reference checks say so. The window's `seq` is in `folds`, and `LineIndex` gives its offset.
- **The harvest is a fold now** (`HarvestFold`, in `state()`, its scan the reference), or the file
  would have needed a pass per write. So `harvest_records` and `ParentRunFacts::from_projection`
  read no log, resident or not, which also answers phase 3's "a pass" for a resident windowed root.
  Whether a delegation record is tainted is still asked as it is handed over, of the validation
  state as it then stands.
- **A line naming blobs stays in the file.** It was kept in memory as read, and on the largest log
  that was half the file, 3.2 MB of 6.3 MB in base64, for 402 lines, and 2.4 MB of live heap on every
  open, a parse's too. No repair happened to those lines and their record reproduces them, so they
  now count as read as they are, and `ReadRawLines::bytes` reads the blobs back to compare, at a
  fork or a `visit_json`. What a newer build wrote on such a line survives as before
  (`a_line_naming_blobs_is_read_from_the_file_with_what_this_build_does_not_know`). The storage spec's
  sentence about it is corrected.
- **The fallback is one pass, with the projection folded on the way as a live append folds it.**
  Only an `Observed::NeedsRefold` falls back to `projection_fold_with`'s two passes, as it does live.
  It starts the same as an append: `EventStorage::push`, and `narrow` at each committed turn, so a
  native log holds what it always held, without the buffer. A log that goes from ACP to native
  keeps its window until its first `model_context_events()`, as a live one does.
- **Where the fallback does not stream, against section 4.3's first text.** Damage: a line whose
  read taints anything, a record that fails validation, or a broken sequence hands the log to the
  loader of old, whole file and all. `validate_loaded`'s quarantine rebuilds validation over what it
  accepted, and its reader's taint is the whole file's before the first record is checked, so
  streaming that exactly would take a pass per quarantined record, or holding the records anyway. A
  log with a state file that answers for its damage does not get here. **None of the 27 logs checked
  below has any.** An older history format still converts in memory on a read-only open. These are
  the only whole-file reads left; a lasting exception would be a damaged log no owner ever writes a
  file for.
- **"Stale" means not matching.** Section 7's "stale" file folds from the start when its record is no
  longer where it says, as `view.json`'s does. One merely behind opens with a longer tail
  (`a_state_file_behind_its_log_opens_with_a_longer_tail`), or per-MiB writes would be no use.
- **Damage before the offset** is `EventLogError::Corrupt` from the pass that reads it, when the load
  knew of none. A log whose load noted damage skips it in a pass, as it did. The open itself does not
  notice (`damage_before_the_offset_shows_when_a_pass_reads_it`).
- **A read-only log has a window now,** as phase 3 left for this phase. Its owner can still cut the
  file with no word, so a pass of it ends by checking that the window's first record is still at
  its offset, and ends in `HistoryCut` if not. A pass within a file an owner is appending to is
  unaffected. `cold_window` takes `newest_pass`, so a viewer reads from where the view cache starts.
- **What is not written.** A log with a sequence jump no taint explains gets no file: a parse would
  find the gap and this log did not. Nor does one whose tip is a line it holds no record of. A log
  that opened from the fallback writes at its next committed turn past 1 MiB, or as it leaves
  memory. **A child this process never owned has no file,** so an existing tree's children parse at
  each tree load until something writes to them. That is section 4.5's "each child that has one",
  and it is the difference between the two tree columns below.
- **Exact floats took a feature:** `float_roundtrip` on `serde_json` in the store, as a parse off by
  one unit in the last place would make a snapshot differ from a parse. It is no new dependency.
- **Tests.** Every open that does not read its log whole also parses it and compares validation,
  taint, diagnostics, the index and every fold, line index, raw lines, records, and projection, by
  `PartialEq` and by bytes (`state_file/reference.rs`), including every fallback. Every committed turn
  of a windowed log in every test writes a file of that moment and opens through it, so all ACP tests
  go through this. `a_state_file_and_the_tail_open_as_a_parse_at_every_turn_boundary_of_the_golden_logs`
  does it for all 12 golden logs, native ones too, with the tail and with none.
  `the_folds_of_the_golden_logs_match_their_recorded_digest` holds the digest of section 4.4; a change to
  `latest_turn`'s fold, tried once, failed it. Crashes and mismatches are in `state_file/tests.rs`.
- **Run once, not committed:** on scratch copies of the store's 27 largest ACP logs (3.5 to 12.5 MB, of
  77 over 1 MB), a file at every turn boundary, each with the tail to the log's end, against one parse.
  **17,438 boundaries, 122,480 records: every open went through the file, no fold differed, none
  needed a refold, and no log had damage.** So `NeedsRefold`, phase 3's open question, did not come up
  on real usage either.

Measured on 2026-11-07, on scratch copies of this machine's six largest ACP logs in history format 3,
opened by `begin_write` and `commit_adoption` in a release build. In MB by a counting allocator, the
median of 5, each in a fresh process. "Before" is `2f7ccbe9d`. The fallback is the same open with no
`state.json` anywhere:

| Log | Children | `state.json` | Peak, before | Peak, after | Peak, fallback | Live, before | Live, after | Live, fallback | Open, before | Open, after | Open, fallback |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 12.5 MB | 0 | 3.17 | 55.97 | 8.46 | 7.01 | 7.37 | 5.22 | 4.57 | 268 ms | 32 ms | 283 ms |
| 11.2 MB | 3 | 2.09 | 54.84 | 5.58 | 5.42 | 5.37 | 3.24 | 3.23 | 211 ms | 30 ms | 248 ms |
| 10.2 MB | 0 | 2.04 | 41.95 | 5.41 | 4.97 | 5.73 | 3.15 | 2.87 | 176 ms | 16 ms | 206 ms |
| 10.1 MB | 0 | 2.20 | 41.84 | 5.69 | 5.23 | 6.51 | 3.42 | 3.13 | 171 ms | 28 ms | 181 ms |
| 9.7 MB | 1 | 1.89 | 52.16 | 5.41 | 5.22 | 5.25 | 3.11 | 3.09 | 179 ms | 29 ms | 193 ms |
| 9.3 MB | 10 | 2.16 | 41.63 | 5.63 | 5.03 | 5.20 | 3.55 | 3.17 | 400 ms | 48 ms | 507 ms |

So a fallback costs about what an open cost, in time, and about what an open from the file does in
memory. Its peak follows the largest record and what the folds hold, not the log's length. The file's
own parse is what a peak over the fallback's is: its bytes held as they are read.

The tree load alone, with `load_tree`, in the same way. "Before" holds the root's parse in the cache
of read-only logs, which an adoption takes over:

| Log | Children | Peak, before | Peak, all with files | Peak, no files | Time, before | Time, all with files | Time, no files |
|---|---|---|---|---|---|---|---|
| 12.5 MB | 0 | 55.97 | 8.46 | 5.13 | 252 ms | 36 ms | 277 ms |
| 11.2 MB | 3 | 54.84 | 5.35 | 3.84 | 197 ms | 33 ms | 247 ms |
| 10.2 MB | 0 | 41.95 | 5.27 | 3.95 | 164 ms | 25 ms | 201 ms |
| 10.1 MB | 0 | 41.84 | 5.69 | 3.55 | 151 ms | 27 ms | 175 ms |
| 9.7 MB | 1 | 52.16 | 5.02 | 3.53 | 160 ms | 28 ms | 192 ms |
| 9.3 MB | 10 | 41.63 | 5.63 | 4.62 | 382 ms | 50 ms | 505 ms |

Live after a tree load falls from 24.5 to 38.0 MB, to 3.4 to 5.5 MB with files, for the same reason. One
root with its file and 10 children without took 360 ms, one run: the children's parses.

The daemon, a release `cookie daemon` on a scratch `HOME` holding the same copies, each in a fresh
process, with a thread preloaded to call `malloc_trim` on request, as nothing else here runs one. RSS
after a trim, the rise on `session.resume` with no trim, what a second trim leaves of it, in MB, and the
call's time, one run each:

| Log | Idle, before | Idle, after | Rise, before | Rise, after | Left after trim, before | Left after trim, after | Resume, before | Resume, after |
|---|---|---|---|---|---|---|---|---|
| 12.5 MB | 102.2 | 104.1 | 45.4 | 14.9 | 14.2 | 10.3 | 285 ms | 45 ms |
| 11.2 MB | 102.7 | 102.3 | 35.1 | 12.7 | 12.4 | 10.5 | 236 ms | 39 ms |
| 10.2 MB | 104.0 | 103.7 | 43.1 | 14.3 | 12.7 | 10.0 | 195 ms | 39 ms |
| 10.1 MB | 103.6 | 104.1 | 43.2 | 12.7 | 15.1 | 10.4 | 182 ms | 37 ms |
| 9.7 MB | 103.6 | 105.0 | 34.7 | 11.4 | 13.5 | 8.9 | 199 ms | 38 ms |
| 9.3 MB | 103.6 | 101.9 | 43.6 | 13.3 | 13.0 | 11.0 | 419 ms | 62 ms |

With the fallback, on the same logs: rises of 9.4 to 13.8 MB, 7.4 to 12.1 MB left after a trim, and 199 to
546 ms. The whole write as a session leaves memory, its view cache's included, took 9 to 14 ms and 4.3
to 6.7 MB for the moment, on the largest logs.

- **`FreedMemory` is moot for an open, and the threshold was left alone.** An open now frees 2 to 5 MB of
  what it allocated, where it freed 21 to 31 MB, so RSS stays near what a trim would leave without one:
  115 to 119 MB, where it stood at 138 to 148 MB. Nothing here calls for changing 32 MiB.
- **The peak fell by 85 to 90 percent and the open's time by 84 to 91 percent,** where phase 3 left the
  peak as it was. Live heap fell by a further 29 to 47 percent, as lines naming blobs left memory.
- **What these do not say.** They are this machine's logs, and a log at 100 times this length is phase
  5's. Open times are noise at a few ms, and one run of the daemon's rows is noise at a few MB.

**What phase 5 must know.** Bytes of JSON in the file on those six logs, in KB, smallest to largest. Each
is linear in the log unless it says otherwise, and each costs twice, in the heap and in the write:

| Part | KB | Grows per |
|---|---|---|
| Whole file | 1,892 to 3,173 | |
| `validation` | 1,100 to 1,383 | |
| `validation.attempts` | 353 to 447 | Attempt: a `ResolvedModelRef` each, nearly always the same |
| `validation.turn_models` | 203 to 258 | Turn: the same again |
| `validation.tool_starts` | 175 to 234 | Tool call |
| `validation.model_call_owners` | 170 to 227 | Tool call |
| `validation.turns` | 110 to 141 | Turn |
| `validation.runs` | 19 to 98 | Run |
| `projection.runs` | 163 to 913 | Run: an `AgentSnapshot` of 3.4 KB each, where 159 runs of the largest log hold 4 distinct ones |
| `projection.usage_rollup` | 7 to 36 | Turn: `observations`, which the wire leaves out and the file must not |
| `projection.fold.tool_names` | 21 to 28 | Tool call |
| `view_start` | 213 to 531 | Bounded: 1 to 3 noted starts and up to 256 unsettled records, each header written whole |
| `lines` | 90 to 145 | Record |
| `index`, of which `state` | 59 to 235 | |
| `state.producers` | 2 to 135 | Message: their bodies |
| `state.acp` | 24 to 44 | ACP session |
| `state.run` | 12 to 20 | Bounded: the current run's `run_started` |
| `state.opening_inputs` | 7 to 15 | Bounded: the first 16 inputs |
| `state.harvest` | 1 | Grant or delegation: none of these six has many |
| `raw` | 0 | Line no record reproduces, only with damage |

- **Sharing does not survive the file.** `Arc<StoredEvent>` and `Arc<AgentSnapshot>` are written once per
  holder and read as separate copies, so a log from the file holds about 0.65 MB more than one folded
  on the largest log (5.22 against 4.57). Bounding `runs` takes most of that. If not, the file should
  hold each once.
- **`LineIndex` is read whole for what a sparse index must still answer:** the prefix check of the
  open (a line's sequence at its offset), `window_start`, `StoredRaw::restore`, and `file_range`. `RawLines`
  is rebuilt from it with a line each, in memory, 32 bytes a line. The check for an unexplained
  sequence jump walks it at each write.
- **A write's cost follows the file's size**, though the 9 to 14 ms above was not split up. `validation`
  goes first if the file is to stay flat, and each of its maps is an append rule to answer for, as
  section 6 says.
- **A fork still reads the source's lines whole**, phase 3's note, and now reads the blobs of a line
  naming them to compare it. Worth streaming with the same pass.


**Phase 5** (2026-11-07), read against the code at `ba255d488`.

**The measurement.** `events::synthetic` (test-only) writes an ACP session's log of any length from
the golden logs' records with their identities changed, so each record has a real shape and whatever
takes it validates it: a real append (`AppendSink`) or a file a real open then folds (`FileSink`). Its
mix is the store's largest log's, by counts and sizes only, read from copies of this machine's 238
logs: 159 runs of 1 to 87 turns (7.3 on average), 0.87 calls a turn, a committed turn of 1.5 KB at
the median and 5.7 KB at the 99th percentile, a call's result of 4.4 KB and 18 KB, a `run_started` of
about 10 KB, half the runs started by a producer's message, an ACP session started in 38% of runs and
a new one every 30th. What the largest log does not do, it does at the other logs' rates: an approval
in 19% of runs, an internal agent after 19% of runs' end, and a delegation in 8% of runs, about one per
megabyte, which is the highest rate any log over 1 MB shows. And what makes a bound dangerous, at the
rates the store shows or above: a call open across up to 28 turns, a background job that ends up to 8
runs after its own, a delegation that finishes up to 7 runs later, an input admitted and recalled.
The store's logs agree on how far back a record reaches: of 33,526 call starts and 5,338 usage records,
every one names the latest committed turn; a call ends up to 29 turns after it starts; and after a run's
end only a delegation's finish (up to 6 runs later), a call's end, an internal agent's records, an
abandon, and a checkpoint name it, all but the first in the same moment.

`examples/forever_measure.rs` (`--features test-support`, release) generates a log `N` times the largest
into a scratch store, opens it in a fresh process by a fold, lets it leave memory, which writes
`state.json`, and opens it again from that, with a counting allocator; `check` converts a copy of a
store and opens each log both ways under the reference checks. `tests/forever_growth.rs`, a test
binary of its own for its allocator, holds the bound in CI time (below).

**Measured** on this machine in a release build, one process per open, live heap and peak in MB by the
counting allocator, a fresh generation at each length. "Before" is `ba255d488`:

| Length | Log | Live, before | Live, after | Peak, before | Peak, after | `state.json`, before | after | Open, before | after |
|---|---|---|---|---|---|---|---|---|---|
| 1x | 13.3 MB | | 0.79 | | 1.27 | 4.03 MB | 0.52 MB | | 11 ms |
| 4x | 55.5 MB | 22.36 | 1.51 | 38.40 | 1.77 | 16.02 MB | 0.66 MB | 90 ms | 14 ms |
| 10x | 139 MB | 52.44 | 2.69 | 93.18 | 2.84 | 40.05 MB | 1.03 MB | 239 ms | 20 ms |
| 25x | 348 MB | 141.24 | 7.52 | 240.87 | 8.23 | 100.27 MB | 2.01 MB | 600 ms | 37 ms |
| 50x | 686 MB | 281.17 | 14.11 | 479.08 | 15.53 | 198.93 MB | 3.40 MB | 1,207 ms | 52 ms |
| 100x | 1,352 MB | 561.20 | 27.99 | 955.00 | 30.55 | 395.77 MB | 6.54 MB | 2,464 ms | 87 ms |

These are the opens from `state.json`. What still grows is the delegations: about 1,270 at 100x, at about
21 KB of heap and 4.4 KB of the file each (below). Without them, the same mix:

| Length | Live | Peak | `state.json` | Open from it | Fold from the start |
|---|---|---|---|---|---|
| 1x | 0.73 | 1.28 | 0.54 MB | 10 ms | 156 ms |
| 4x | 0.70 | 1.22 | 0.50 MB | 10 ms | 602 ms |
| 10x | 0.76 | 1.31 | 0.55 MB | 4 ms | 1,445 ms |
| 25x | 0.99 | 1.67 | 0.67 MB | 12 ms | 3,652 ms |
| 50x | 1.26 | 2.06 | 0.80 MB | 5 ms | 7,150 ms |
| 100x | 1.78 | 2.89 | 1.10 MB | 16 ms | 14,373 ms |

**So, without delegations, live heap stays within 1.1 MB, `state.json` within 0.6 MB, and the open within a
few ms of what they are at today's length, across growth to 100 times it.** What grows is two terms kept
for as long as the log, on purpose: a producer message's key (0.50 MB of the file at 100x, about 7,900
messages at 64 bytes each) and each ACP session the log records (58 KB). A fold from the start, the
fallback, stays linear in time, 14.4 s at 100x against 15.0 s before, and flat in memory: 2.1 MB at its
peak, where the same open read whole would hold the log. Writing `state.json` at 100x takes 6 ms, against
2,676 ms and a 1,061 MB peak before. Open times under 20 ms are noise.

The growth test asserts this at 150 and 600 runs of the mix with texts a tenth of their size and no
delegations: what an open from `state.json` holds and the file each within a tenth and 64 KiB of the
shorter log's, its time within three times and 250 ms. In a debug build: 513 KB and 540 KB held, 300 KB and
318 KB of file, 18 ms each.

**The bounds, largest first**, each re-measured at 25x (`state.json`, then live heap of an open from it;
before: 100.3 MB and 141.2 MB):

1. `SessionProjection.runs`, 52 MB of the file: unfinished runs, those a pending call or an open
   delegation holds, and the latest 16 that ended. To 48.5 MB and 71.9 MB.
2. Validation, 37 MB (attempts 11.1, turn models 6.5, call owners 6.0, call starts 5.8, turns 3.8, runs
   2.5, ended calls 1.1, internal agents 0.4): to 49 KB. To 11.1 MB and 23.1 MB.
3. The producer fold, 4.0 MB: to 150 KB, the settled keys. To 7.3 MB and 18.7 MB.
4. `LineIndex`, 3.3 MB of the file and twice that of heap with the raw lines: to 21 KB. To 4.0 MB and 10.1
   MB.
5. The usage rollup's observations, 0.96 MB, and the projection's tool names, 0.63 MB: to a few KB. To 2.4
   MB and 7.9 MB.
6. The approval decisions, 0.25 MB, the attempt counts, 0.17 MB, and the cancelled runs: to 9 KB. To 2.0
   MB and 7.5 MB.

The harvest's delegation records, 1.3 MB of the file at 25x, are what is left (below).

**Can a valid record still name it?** Read from the append rules in `validate_record` and from what the
engine appends, and checked against the store's logs above. Validation lets go only while the log is
windowed (`windowed_after`, the window's own rule), and only of a log whose load found no damage.

- **Committed turns, their models, their usage, and call owners:** a call's start names its turn's owner,
  and a usage record its turn. Both name the latest: an ACP runner commits a turn and starts its calls at
  once (`runner.rs`), and reports usage with the round's last turn; a native loop does the same. The last
  16 turns stay. A start or usage naming an older turn is refused.
- **Attempts:** each lifecycle record names an unfinished attempt, an abandon follows its commit at once,
  and a replay's base names an earlier attempt of its own run, which only native runs record. Unfinished
  attempts of the runs kept stay, and the latest 16 that ended.
- **Calls:** an ACP call can stay open across many turns, and a background job's call ends after its run
  does, so a call stays until it ends, whatever its age. An ended call is named again only by a second end,
  which is refused either way, and by an elision, which a native model's compaction writes; a log a native
  model reads is validated whole again (below). The latest 16 ended stay.
- **Runs:** nearly every record names its run. After a run's end, a delegation's finish, a call's end, an
  internal agent, a producer's claim and release, and a question's answer can still name it, so a run stays
  while any of those is open, whatever its age. A title's internal agent starts after its run ends, a
  client may retry a run's `client_run_id`, and a checkpoint or an abandon follows by a moment: the latest
  16 ended runs stay for those. `DelegatedContextSeeded` needs no run to exist, which a kept run answers.
- **Approvals:** an approval waits as long as the user does, so it stays until it settles. A step or a grant
  can follow its settling at once; the latest 16 settled stay.
- **Delegations:** a delegation can finish long after its run, so it stays until it finishes; the latest 16
  finished stay. Its parent run stays with it.
- **Internal agents:** open ones stay; the latest 16 finished.
- **Admitted inputs:** a recall names an input while it waits: the latest 256.
- **Producer messages:** a message stays while anything can change it: until it is consumed and recorded, or
  discarded, with no claim on it, and its run ended. The latest 16 settled stay, as a cron job reads where
  its message landed once that run ended. After that, only a producer's retry of its key names it, as an
  exactly-once delivery, and that is kept for as long as the log, as an 8-byte digest of owner and key with
  the message's ID. A retry of a settled message is answered with its ID without its payload compared.
  Whether a goal's reminder was consumed is kept too. Ended runs go, but for the latest 16 and those a held
  message names: an admission names the run that starts.
- **Projection runs:** readers ask of the run they are in, a delegate's child run, or a client's retry.
  `SessionProjection::run_outcome` answers a status and final text for any run, reading the log for one it
  let go of; the engine's readers of an older run's outcome take it (delegation completion and reports,
  background jobs, admissions). Idempotency by `client_run_id` holds for the latest 16 ended runs and any
  running, and cancelling a run older than that is `MissingRun` rather than nothing to do.
- **Approval decisions, cancelled runs, attempt counts, tool names:** each is read of what just happened, an
  answer just given, the run just cancelled, the attempt's own run, the turn just committed. The latest 16
  of each stay, and the decisions of open approvals.
- **Line index and raw lines:** no record names them. A pass, a revert, a fork, and `visit_json` read the file
  from the nearest mark instead; a revert before the held lines reads the cut and its new index from the file.
- **Kept for as long as the log, and why:** a producer message's key, as above; each ACP session the log
  records, which a delete removes from its agent; tree grants, which are standing permissions; each "once"
  fact, a key per delegation or skill; and the delegation records, which follows.

**What cannot go flat here: the harvest's delegation records.** A tree load hands every delegation record
to the engine, which rebuilds its registry from them and verifies each invocation's records against its
fingerprint, so neither a record nor a part of one can go. Keeping only each child's latest invocation broke
the golden delegation scenario, whose resumed child lists only with its first. At the stress rate above,
about 1,270 delegations at 100x, they are 5.4 MB of `state.json` and about 26 MB of heap: the harvest, and
the tree load's products, which hold them again until the engine takes them. At the store's real rate,
about one per 7 MB of log, a log 100 times today's largest would hold about 190. Bounding them is the
registry's to change: it would keep a finished delegation's outcome and what a resume checks, not its
request. That is not this phase's.

**How a fold and a whole read are compared.** A whole read (`validate_loaded`, `rebuild_validation`) keeps
every map as it validates, since a record may name anything before it, and lets go of what settled at its
end (`finish_whole`). A record-at-a-time validation lets go after every record that could let something go,
not only at a committed turn: letting go at turns only kept, at a log's tip, what had settled since, and a
fork's prefix ending mid-run differed from its parse. What it lets go of depends only on what it holds, and
no later valid record brings it back, so the two keep the same. `assert_agrees_with_whole` checks that what
a fold holds, a whole read holds the same, and that once each lets go of all that settled they are equal,
which a log that went native at its end needs (a native log does not let go). A state file and its tail are
compared with a fold of the whole file exactly, as before. The producer fold is compared with its scan by
`agrees_with_whole`, and the decisions and cancelled runs by what the fold keeps of the scan's.

- **A record a fold refuses goes to the whole read.** A log written before this phase may hold a record naming
  what a fold now lets go of. Its open falls back to reading it whole, as for damage, which costs its peak
  once; its `state.json` then serves. A failed append's rebuild, and a revert's fold of what stays, do the same.
  None of this machine's logs needed it: **all 238, converted to history format 3, opened a record at a time
  with the bounds, and the 156 windowed ones opened again from the `state.json` they wrote, each under the
  reference checks.** That is at each log's tip, not at every turn boundary as phase 4 checked.
- **A duplicate older than the floor.** An append of a run, attempt, call, or model call ID that a settled
  record used is not found, where a whole read quarantines it. IDs are fresh UUIDs and model call IDs come
  per turn, so this would take damage; the whole read is what finds damage (decision 1).
- **A refusal says so.** Once anything was let go, a refused record's message adds that it may name what
  settled before the validation floor.
- **Native logs.** Validation, the projection's runs, and the line index let go only while the log is
  windowed. A log whose agent turns native reads itself whole at its first `model_context_events()`, and then
  validates whole again too, as a native compaction's elision may name an old call. The producer fold, the
  rollup, tool names, decisions, cancelled runs, and attempt counts are bounded for every log: a native
  model's history folds producer messages from its events (`GoalProducerProjection::from_events`), which keeps
  them all, and the rest is read of what just happened, which no reader of a native log asks otherwise.
- **The rollup's observations.** `ModelUsageRollup` listed every request, never on the wire, to work out its
  hit rate and cost and to take a provisional turn back out. It now holds `UsageObservations`: counts, sums,
  and the latest request. The client protocol's Rust type changes; its wire and schema do not.
- **The line index's sparse part is not "1 entry per run",** against section 6's first text, which grows
  with runs: up to 512 marks, 64 KiB apart, every other one going and the spacing doubling as they fill, so
  a read from one covers a bounded share of the file. Section 6 is corrected. Sequence gaps are noted as lines
  are read, so the check before a write no longer walks the index.
- `STATE_VERSION` rose from 1 to 6, once per commit that changed what the folds give, and the digest test
  insisted each time. A file of an earlier version, including version 1, folds from the start
  (`a_state_file_that_does_not_match_its_log_folds_the_log_from_its_start`).
- **Not done or not measured.** The daemon's RSS was not measured. The real logs were checked at their tips
  only. The mix has no skills, attachments, elicitations, checkpoints, or grants; a grant is kept for as long
  as the log and none of this machine's large logs has one. `view_start` (0.19 MB) and the projection's 16
  kept runs (0.22 MB, an agent snapshot each, written once per run where most are the same) are bounded and
  were left.

**What phases 6 to 8 must know.**

- A native window (phase 7) must decide what native validation lets go of: a replay's base names an earlier
  attempt of its run, an elision names an ended call of any age in the compaction's range, and `windowed`
  gates every bound. The line index already holds from wherever the window is, and `read_lines_again`
  rebuilds it after a cut.
- The reviewer's own log (phase 8) is a native child session: it gets what native logs get.
- `run_outcome` is how to ask of an older run; `ProjectionFolds.runs` no longer holds them all.
- Measure with `forever_measure generate <dir> <times> [delegations a run]`, `open <dir> [parts]`, and
  `check <dir>`, and keep `tests/forever_growth.rs` passing. `Mix` takes a rate of delegations.

**Phase 6** (2026-11-07), read against the code at `a9c2aeb16`.

- **What `model_context_events()` returns, as built** (`events/model_context.rs`): with no
  checkpoint, the whole log; otherwise the records from the window's start, the earlier of
  the latest checkpoint's kept range (`recent_from_seq`, or what follows its
  `source_through_seq`) and what follows its provider window, and under a provider window
  alone, which only builds before 1.29.0 wrote, the latest summary's range too, or the whole
  log when there is none. Before the start it keeps:
  - **back-references**, followed until nothing more is reached from the window and from what
    is pinned: an applied input's submission, a started call's turn and its link to a child
    session, an ended call's start;
  - **what is open now, from `state()`**: every open call's start (and so its turn and link)
    and every input submitted and not yet applied, so a record appended later names nothing
    a trimmed log let go of;
  - **producer messages not yet consumed or discarded**, per the store's own producer fold:
    each one's acceptance and its latest admission, and the acceptance of any message whose
    admission is in view, which holds the body assembly renders;
  - **what assembly reads beside what it renders**, against section 5.1's "the latest cut is
    a fold" and "its producer fold and replay cut come from `state()`": the latest
    `run_started` (its run decides which unconsumed admissions render, and whose `AGENTS.md`
    is current), its latest `system_prompt_composed` (the prompt its requests carry; see
    below), and the `native_replay_cut` that reaches furthest. Pinning these records
    lets assembly stay one function of a slice; the store's producer fold cannot serve it,
    as it lets go of settled messages whose bodies a window still renders;
  - **under a checkpoint without `context_fresh`**, every `skill_loaded` before the window,
    and the current `AGENTS.md` records and latest `memory_loaded`, as that build pinned them.
- **It is safe to apply at any moment, not only at a checkpoint**: the selection of what it
  returned, with what was appended since, is the selection of the whole log. In tests every
  call of `model_context_events()` checks that against a shadow that lets go each time
  (`assert_model_context_holds`), and so do the long scenario and the golden test below.
- **A kept tail starts in the window.** `compaction_tail_candidates` offered a group starting
  at a pinned record, an input submitted or a message admitted before the window, and a
  checkpoint keeping it would have reached back past what a trimmed log holds. Such a group
  is no longer a candidate. This changes only which tail a new compaction may keep; existing
  checkpoints read as before.
- **`context_fresh`** is a `bool` written only when set, client protocol 1.90.0 and plugin
  protocol 1.44.0 (plugins receive events); the frozen corpora and golden logs pass.
- **The refresh at a checkpoint, as built.** `maybe_compact_context` reads, as compaction
  starts, a root session's `AGENTS.md` (`checkpoint_agent_md`, the rule a run's admission
  uses: native, root, `agent_md` on) and the memory snapshot (`checkpoint_memory_snapshot`,
  a refresh's rule: taken while memory is available, an empty one when it went away from a
  turn that showed some, nothing when nothing shows), passes their rendered turns to every
  projection (`FreshTurns`), and records them, with any skip warnings, in the same actor call
  as the commit, after the commit is validated. A compaction that fails or is cancelled
  records nothing. An attachment whose runtime is away, or whose read fails, keeps its
  current record, and a snapshot that cannot be read writes the latest record again.
  "The current `AGENTS.md`" is the latest run's records, or, after a checkpoint with
  `context_fresh` in that run, those after its `input_through_seq`.
- **The system prompt at a checkpoint inside a run, as built** (after the user decided that
  everything refreshes at a checkpoint; client protocol 1.91.0, plugin protocol 1.45.0). The
  first cut of phase 6 left the prompt frozen in `run_started` until the next run, so a run
  that never ended kept the prompt it started with. Now:
  - **The record** is a new audit event, `system_prompt_composed { composed_prompt,
    prompt_fingerprint, blocks? }`, written right after the checkpoint commits, in the run,
    only when the composed prompt or its blocks differ from the ones in force. Neither
    `context_refreshed` nor `prompt_blocks_recorded` fits: the first is a client row with a
    closed reason a compaction never had (the compaction has its own row), and the second
    carries spans against the run's `run_started` prompt and is absent when a plugin replaced
    it, where this record must still carry the prompt. It is audit, as `prompt_blocks_recorded`
    is: only its own run reads it, a run does not outlive its engine, and the next run after a
    checkpoint composes its prompt afresh, so an engine that does not know it builds nothing
    differently. Writing it after the commit, not with the `AGENTS.md` and memory records
    before it, lets the prompt be composed as the run's admission composes it after a
    checkpoint: with the new round's deferral (the tool instructions block depends on which
    providers are deferred) and with the plugins' hooks, which are async. A compaction whose
    prompt cannot then be composed (`ToolPrompt`, too long or invalid, or a blocking hook that
    failed) keeps its checkpoint and leaves the prompt in force; the compaction's own budgets
    count the prompt it started with.
  - **What it composes** is what a run's admission composes live: the agent's body as the
    live registry holds it for the run's preset (the frozen one when the agent is gone or is
    an ACP agent now), the `runtimes`, `attachments`, `tool_instructions`, and `skills`
    blocks, then the `agent_before_start` hooks with `input` absent. A message a hook injects
    is not taken: injections join a run at its start. **What stays frozen** is everything else
    in the run's policy: permissions and the memory access they give, the model chain and
    limits, delegation targets (so the delegate section lists the targets the run started
    with), goal tools, the internal agents. The policy decides what the run may do, and
    changing it mid-run would let a document edit grant a running run new permissions; the
    prompt only says what the session looks like.
  - **Readers of the prompt in force:** each request takes it from the model context it is
    assembled from (`model_history::prompt_in_force`), as does the attempt's
    `prompt_fingerprint`; `run_agent_prompt` (compaction, `get_history`) reads
    `LogState::run_prompt`; `context_composition` takes the previous run's latest prompt and
    blocks; the prompt block fold treats the record's blocks as the run's and as what the
    model knows, so the next request announces nothing that the new prompt already says;
    validation lets the run's later attempts carry its fingerprint; the display view's header
    holds the latest run's latest one, and the clients show it in the system prompt row.
  - **`model_context_events()` pins** the latest run's latest record before the window, as it
    pins the run's `run_started`. A later checkpoint in the same run that composes the same
    prompt records none, so the one in force can be older than the window; the long scenario
    checks that case, and turning the pin off fails it.
  - **The tool round** now starts over for the request right after the checkpoint: the tools
    are published again when a checkpoint committed since they were (in both compaction
    paths, and after an overflow's), with every tool up front when the attempt was a retry
    after a refusal of deferred tools. The first cut published them again only at the next
    loop pass, so the request after a checkpoint declared the old round and the one after
    that the new, which moved the prefix twice.
  - **Reasoning and caching.** The request after a checkpoint already starts from the summary,
    so the prefix and its cached part change there anyway; a recomposed prompt moves the
    change up to the system prompt, once, and is recorded only when something changed. On a
    model that binds its reasoning, the stale-reasoning cut of the frozen context spec
    (section 4) cuts the kept turns' reasoning with cause `compaction`, as before. The approval
    reviewer does not read the session's system prompt (its own prompt, and the session's
    `AGENTS.md` records), so it is unaffected. ACP sessions are unaffected: their runs neither
    use `maybe_compact_context` nor record the event.
  - **Tests:** `a_checkpoint_inside_a_run_composes_its_system_prompt_afresh` (a tool
    provider's section changed between runs reaches the request after the run's checkpoint,
    with the record, the attempt fingerprints, the new round before that request, no
    announcement, and the same assembled history from a reopened engine);
    `auto_compaction_commits_checkpoint_before_the_attempt_that_uses_it` now also checks that
    an unchanged prompt records nothing and the round starts before the next attempt; the
    long scenario records prompts inside runs and checks the fold and the pin at every record;
    `attempts_after_a_composed_prompt_carry_its_fingerprint` and a case of the differential
    validation test; the protocol's and the display view's tests; and the client's
    `a_prompt_composed_inside_a_run_is_the_one_its_row_shows`. A plugin hook at a checkpoint
    is not tested on its own: it is the admission's code path, run with no input.
- **The next run takes `AGENTS.md` and memory once more,** as the first run after a checkpoint
  always did. So a compaction between runs reads them twice, once for the checkpoint and once
  for the run.
- **Skills, as built.** A skill renders before the conversation only if it was loaded before
  the latest checkpoint or refresh and after the latest checkpoint with `context_fresh`. So
  after a fresh checkpoint a skill in the kept range stays in place and an older one is gone;
  a refresh that is not a checkpoint still moves earlier skills after the memory turn, as
  nothing tells a new session's refresh from an old log's, and the conversation stays whole
  there. The duplicate check (`OnceFacts.skills`) starts over at a checkpoint with
  `context_fresh`, as the user put it: a skill in the kept range loaded again shows its body
  twice. Nothing cleared `skills_runtime` before, and nothing does now. The summarizer's rule
  reads "Omit pinned context such as AGENTS.md; it stays available separately.", and its input
  shows the skills it now covers. `checkpoint_retained_history` counts no skill.
- **In-stream compaction commits no checkpoint.** The ChatGPT sign-in spec's phase 6 kept the
  server's compaction items in their turn's native replay, with no window checkpoint and no
  background summary, so nothing commits inside a request. The engine's own checkpoint, the
  backstop, is `maybe_compact_context` like any other and reads the context afresh. Nothing
  was needed. Phase 7 should know that such a session's log is windowed only when the
  backstop fires.
- **The approval reviewer renders from `history()`**, the whole log as a pass, against phase
  2's choice of `model_context_events()`: its epochs and run numbers count from the log's
  start, which the model context no longer reaches once a checkpoint commits, so phase 6
  would have changed what it sees. This keeps it as it was until phase 8.
- **The loop guard reads a fold**, `LogState::loop_calls`, as section 5.1's table says: the
  latest run's calls since its latest input, with a digest of what each showed, a window of
  64 up to each open call and at most 256, its scan the reference at every event.
  Differences from the walk: a call checked while more than 256 others started after the
  oldest open one is compared with what is left, and a run other than the latest is not
  checked. `STATE_VERSION` rose from 6 to 7, with the digest. The skill fold's change needed
  none: no log an earlier build wrote holds a checkpoint with `context_fresh`.
- **Native compaction's durable elision now reads the model context**, so it no longer elides
  calls before the window, which no request sends.
- **Tests.** In every test, each request (`request_context`), each compaction, and each
  assembled `get_history` also assembles from the whole log through the same record and
  asserts that the request's context with and without provider windows, the summary view,
  the tail candidates, what a checkpoint keeps, a projected checkpoint for three tails with
  and without fresh turns, and the summarizer's input are the same
  (`model_history::assert_assembles_as_whole`).
  `a_long_native_session_assembles_from_its_model_context_as_from_the_whole_log` is the long
  scenario, a log written record by record and checked at every record on the model with the
  window and on another: an ACP agent's turns and then a native model's, an older checkpoint
  pinning a skill, a skill and a background call before a checkpoint with a window, a
  producer message accepted before it and admitted after, the call ending after it, the skill
  loaded again, a switch to a model with no window and a summary-only checkpoint, a message
  admitted to a cancelled run, a replay cut, a checkpoint with no kept tail in the middle of a
  run with a call open across it, and one at the earliest tail. Turning off the
  back-references, the open calls, the producer pins, the run's start, the replay cut, the
  older checkpoint's pins, or the window start of tails each fails it.
  `golden_logs_assemble_from_their_model_context_as_from_the_whole_log` does the same for every
  golden log, of every history format, at every committed turn, checkpoint, and run's end, on
  each model the log's runs bound and on another.
  `a_checkpoint_reads_agents_md_afresh_and_a_skill_loads_again_after_it` goes through the
  engine and the coding runtime.
- **Not done.** No golden directory was recorded: the long scenario is the synthetic log above,
  and through the engine only the smaller test. A real ACP agent switching to a native model
  is the existing `a_switch_to_a_native_agent_and_back_reads_the_log_whole_and_windows_it_again`,
  whose native request the check above now covers; the long scenario's ACP turns are records
  naming `acp/<agent>`. Nothing was measured.

**What phase 7 must know.**

- Trimming is `held = model_context(&held, &state)` at a checkpoint's commit, with `state` the
  log's fold at the tip; the shadow test shows any moment works. `model_context` passes over
  what it is given each time, `References::of` included, which is the context's size once
  trimmed.
- The window's start never moves back past a held record once the tail rule holds, except
  under a provider window alone (older logs), which reads the whole log on a model that cannot
  use it.
- `state.json` for a native log must carry the pinned records, as section 5.6 says; the folds
  that choose them (`calls`, `inputs.submitted`, the producer fold, and the run fold's latest
  `system_prompt_composed`) are already there.
- Native validation must keep what a pinned or windowed record can name: an elision now names
  only a call in view, a replay base an earlier attempt of its run, a late termination an open
  call.
- The reviewer then reads its session as a file pass per escalated approval, until phase 8.

**Phase 7** (2026-11-07), read against the code at `d5afe91f8`.

- **The window, as built.** `EventStorage` gains `context`: what it holds is a native model's
  context, the window from where the latest checkpoint's context starts and, before it, the records
  `model_context` pins, in order. `narrow_to_context` applies `model_context` to what the log holds,
  as phase 6 left it to: when a checkpoint commits (`narrow_after`, from an append, a fold of the
  file, an open from `state.json`, and a revert's fold of what stays), as an owned log opens
  (`narrow_window`, `mark_written`), and after a native model's first read of a log an ACP agent's
  window narrowed (`read_whole`). `storage.window` is the context's start itself, which need not be a
  record's sequence. A pass reads what is pinned from the file with the rest, and `recent()` leaves
  the pins out, so its one caller, an ACP claim, reads what it read. `model_context_events()` reads
  what the log holds when it holds the whole log or a context, and reads the log whole only when an
  ACP agent's window let go of what the model may need. An ACP agent's committed turn narrows a native
  window to its own and drops the pins.
- **What stays whole.** A native log with no checkpoint, so one with compaction off; a log whose
  context would start before what it holds, which only a provider window alone after a summary, as
  builds before 1.29.0 wrote, could make it do (it reads whole at its next model context instead); and,
  as for ACP logs, a buffered, converted, render-only, or read-only log that read itself whole.
- **A fold of a log that went from ACP to native cannot narrow at its checkpoints**, as what the ACP
  agent's window let go of is gone by then. It stays as it was, and reads the log whole at its first
  `model_context_events()`, as the log that wrote it did. So a live log and a fold of its file can
  hold differently for a while, with the same answers; the state file's exact comparison with a fold
  skips that case (`reads_whole_for_its_model`). When the context then starts before the lines the
  ACP agent's window kept, the line index is read again from the file (`lines_fall_short`), or
  `state.json` would open the window from the wrong line; a view cache's start that falls behind them
  does not count, as before.
- **`state.json` for native logs** (section 5.6): `LogFolds` holds `context` and `pinned`, the
  records themselves, and an open puts them before the window it reads from the file, checking they
  precede it in order. Written after each checkpoint commits, after a committed turn past 1 MiB, and
  as the session leaves memory, by the same code as an ACP log's. `STATE_VERSION` rose from 7 to 8 for
  the validation and projection change below, and to 9 for the window, the digest test insisting each
  time; the digest's boundaries now include checkpoints.
- **The bounds, per map: what a native append can name, read from the append rules in the code.**
  Native validation and the projection's runs now let go once a checkpoint has started the model's
  context (`LettingGo`: an ACP agent's window, or a native log with a checkpoint); a native log with
  none keeps every map, as its model reads all of it and an elision may name any call.
  - *Turns, their models, usage, call owners:* the native loop commits a turn, starts its calls, and
    records its usage at once (`model_loop.rs`), so each names the latest turn: the last 16 stay.
  - *Attempts:* lifecycle records name the attempt in flight; a replay's base is the attempt the run's
    loop evaluated last (`ReplayEvaluationBase` holds only that one), which each run keeps whatever
    its age (corrected after review, below: the latest 16 that ended did not always hold it); an
    abandon follows its commit at once.
  - *Calls:* a late completion, a background job's end, and a link name an open call, which stays. An
    ended call is named again only by an elision, which native compaction writes from the model context
    (`stage_tool_output_elision`), so of calls in view: those whose end is in the window, which is
    every call that ended since the context's start. **These stay whatever their age and whatever their
    run**: an elision names its call's run, and validation checks that against the call, not against
    the run's own record. The context's start is folded from the checkpoints (`context_from`) and never
    moves back, so a record-at-a-time validation and a whole one keep the same.
  - *Runs, approvals, delegations, internal agents:* as for an ACP agent's log. A native background job
    or delegation holds its run open; the compaction's summary agent, a title's, and the reviewer's are
    internal agents, open until they finish; a checkpoint, an abandon, or a composed prompt follows by a
    moment, inside a run that is still open.
  - *Admissions:* a recall names an input while it waits: the latest 256. An applied input's submission
    is pinned while it is unapplied and named from the window otherwise.
  - *What else a native record names before the window:* a replay cut names the tip, a checkpoint's
    boundaries and a composed prompt are not validated against records, a message's admission names its
    acceptance, which is pinned. None needs a map.
  - *Projection runs:* the native readers of a run ask of the run they are in, a child's, or a retry,
    and `run_outcome` reads the log for an older one, as phase 5 found; so they let go under the same rule.
  - *Line index:* from the window or a view cache's start, with marks before, as for ACP logs.
  - *From ACP to native:* the ACP agent's window let go of ended calls that the native context, starting
    at an earlier checkpoint or at the log's start, still holds, so the first `model_context_events()`
    validates whole again, as phase 5 did, and lets go by the native rule from there.
  - The reference comparison of a fold with a whole read lets go of what settled without the native
    rule (`let_go_settled_keeping(None)`), as a whole read keeps an ACP agent's calls that a fold let go
    of and the native context still holds.
- **Every reader of a native log, checked:** each reader of `model_context_events()` (compaction and its
  elision, `compaction_tail_candidates`, `get_history`'s assembled view, `context_composition`, a native
  claim, the tool round's refresh) gets what it got in phase 6, which every test now asserts against a
  whole read of the file at each call (`assert_context_reads_as_whole`) besides the engine's assembly
  check, whose whole log is now a pass over the file. The loop guard, the doom loop, `cache_expiry`,
  titles, `run_agent_prompt`, and the prompt in force are folds. `get_history`'s full view, revert, fork,
  older pages, `event.read`, the delegation prompt, and the approval reviewer are passes, which read the
  file. `cut_native_replay` reads the tip. In-stream compaction's backstop is an ordinary checkpoint, so
  such a session's window moves only when it fires.
- **Passes a native session still takes:** none on an ordinary turn, nor in a compaction, nor for the
  assembled history (`a_native_window_moves_at_its_checkpoint_and_an_ordinary_turn_reads_no_pass`). The
  full history, a revert, a fork, older pages, and `event.read` read the file, as for ACP logs, and so does
  **the approval reviewer, once per escalated approval**, until phase 8 gives it its own log.
- **Fixed on the way:** `visit_json` of a log holding its records serialized a record appended since the
  read without `write_stored_history`, so it differed from its own line (a display block's lines); its
  one reader collects artifact references. The extended golden test found it.
- **Tests.** Every committed turn and checkpoint of every test's store append compares the log with a
  reopening, the projection with a reopening's, and opens a `state.json` of that moment against a parse.
  `a_golden_*_log_appended_again_answers_as_reopened*` append every golden log of format 3 (ACP, coding,
  revert and fork, both native compactions, replay, delegation) and compare at every committed turn and
  checkpoint: windows, pages, lines, counts, metadata, the model's context, and an owned reopening's
  window and holdings. `a_native_log_holds_its_context_once_a_checkpoint_commits_and_its_file_the_pins`,
  `a_synthetic_native_log_holds_its_context_and_answers_as_its_file` (16 runs, 17 checkpoints, 53 turns,
  each checked, with skills, elisions, background jobs, delegations, and producer messages across
  checkpoints), `a_revert_of_a_native_window_folds_what_stays_as_its_file_does` (a fork through the
  window, and reverts into it, to before its latest checkpoint, and to before its first),
  `a_switch_between_an_acp_agent_and_a_native_one_with_a_checkpoint_windows_either_way` (ACP to native,
  its checkpoint, back to ACP, and native again, through the engine), and
  `a_native_log_that_compacts_costs_no_more_at_four_times_its_length`.
- **Not done as asked: the long native scenario of phase 6 does not go through a log.** Its records are
  not a valid log (turns without attempts, an ACP agent's turn inside a native run), so a real append
  refuses them; it stays the in-memory check of selection it was. Its through-disk counterpart is the
  synthetic native log above, checked at every committed turn and checkpoint, and the engine's tests.

Measured on 2026-11-07, release build, one process per open, by the counting allocator, in MB, on a
synthetic native log (`forever_measure generate <dir> <times> 0 native`): the mix of phase 5 on a native
model, each turn's usage recorded, a skill in a tenth of the runs, and a compaction inside the run once
the context since the last checkpoint holds about 400 KB, keeping the latest turn and eliding up to three
outputs in view. Without delegations. "Fold" is the first open, with no `state.json`; "file" the next:

| Length | Log | Records | Held after open | Fold: time | Fold: live | Fold: peak | `state.json` | File: time | File: live | File: peak |
|---|---|---|---|---|---|---|---|---|---|---|
| 1x | 15.1 MB | 7,903 | 219 + 3 pinned | 208 ms | 1.60 | 2.01 | 0.57 MB | 16 ms | 1.76 | 1.93 |
| 4x | 62.1 MB | 32,340 | 112 + 3 pinned | 863 ms | 1.44 | 2.14 | 0.70 MB | 17 ms | 1.58 | 1.91 |
| 10x | 155 MB | 80,731 | 24 + 1 | 2,127 ms | 1.01 | 2.14 | 0.62 MB | 5 ms | 1.13 | 1.68 |
| 25x | 380 MB | 198,314 | 241 + 1 | 5,246 ms | 1.96 | 2.27 | 0.72 MB | 19 ms | 2.08 | 2.26 |
| 50x | 761 MB | 397,947 | 109 + 1 | 10,511 ms | 1.62 | 2.47 | 0.79 MB | 16 ms | 1.75 | 2.17 |
| 100x | 1,513 MB | 791,101 | 238 + 1 | 20,898 ms | 2.58 | 2.87 | 1.10 MB | 23 ms | 2.71 | 3.00 |

**So a native log that compacts holds 1.0 to 2.7 MB, its file 0.6 to 1.1 MB, and opens from it in under 25
ms, from 1 to 100 times this store's largest log**: what moves is where the context since the last
checkpoint happens to end, and the producer keys kept for as long as the log, as in phase 5. The fold
from the start stays linear in time and flat in memory. For comparison, the same mix with compaction off
(`native-whole`), which holds every record as every native log did before this phase: live 35.8, 145.7,
and 362.5 MB at 1, 4, and 10 times, peaks 38.3, 155.7, and 385.3. With phase 5's rate of delegations, from
`state.json`: 1.39, 4.68, and 28.78 MB live and 0.66, 1.21, and 6.42 MB of file at 1, 10, and 100 times,
the delegation records of phase 5. The growth test, in a debug build at 150 and 600 runs of the small
native mix: 941 KB and 1,029 KB held, 376 KB and 400 KB of file, 23 ms and 25 ms.

**The one native log on this machine with a checkpoint**, a scratch copy converted to history format 3:
4.7 MB, 2,037 records, one checkpoint, 12 children. `forever_measure check` opened all 13 logs a record at
a time under the reference checks, 2 windowed, both reopened from the `state.json` they wrote. The root
alone: read whole, 64 to 81 ms, 12.36 live, 18.29 peak; by a fold, 68 ms, 7.26 live, 7.58 peak; from its
1.02 MB `state.json`, 28 ms, 7.91 live, 8.03 peak, holding 950 records and 3 pinned, as its one checkpoint
keeps half the log in its context. Through the store with its tree, 168 to 230 ms and a 21 MB peak: its
native children have no checkpoint, so no file, and the tree load parses them.

**What phase 8 must know.**

- The reviewer's own log is a native child session, and **a native log's window moves only at a
  checkpoint**: one that never records `context_checkpoint_committed` holds every record and has no
  `state.json`. Section 5.5's epoch makes no model call and so writes no checkpoint as built today;
  phase 8 must give its epoch a record that `context_window_start` reads (a checkpoint of its own, or
  teach `model_context` the epoch), and `LettingGo` the same, or the reviewer's log grows in memory.
- Until then the reviewer reads its session through `history()`, a pass over the file per escalated
  approval of a windowed native session.
- A native child without a checkpoint is parsed at each tree load (no file), as the real log above shows.
- `EventStorage::context`, `LettingGo`, `narrow_to_context`, and `reads_whole_for_its_model` are where a
  native window is decided; `model_context` is still the only selection, so a change to it changes what
  every native log holds, and the digest test and `assert_context_reads_as_whole` say so.

**Phase 8** (2026-11-08), read against the code at `bcfda059c`.

- **The hidden child session, as built** (`engine_store/src/session/reviewer.rs`). It is
  `reviewer/events.jsonl` in the reviewed session's own folder (a root's `<month>/<id>/`, a child's
  `subagents/<id>/`), with an id of its own (a new UUIDv7, read back from its first line), a
  `session_created` copied from the session's with origin `root` and no cron, and after it one
  `reviewer_turn_recorded { turn }` per review that gave a verdict: a runless state event (client
  protocol 1.92.0, plugin protocol 1.46.0). It is hidden by where it is, not by a flag: children are
  filed under `subagents/` and found through its index, and the store opens a reviewer log only when
  a review asks for it (`SessionStore::reviewer_log`), holding it beside its session. So, place by
  place:
  - *the tree load* reads `subagents/index.json` and the folders under `subagents/`, and *the index*
    is rebuilt from those folders: neither looks inside a member's folder;
  - *the root listing and the activity journal* list the roots of the month folders;
  - *`all`, `children`, `tree`, `tree_summaries`, `session_exists`, `resolve_dir`* (and its
    `locate_child`) answer from residency and `subagents/`, and the store test asserts each leaves it
    out (`a_reviewer_log_is_hidden_narrows_at_each_epoch_and_reopens_from_its_file`);
  - *unread and pinned marks, push signals* are of sessions the store lists; the log has no runs,
    approvals, producers, or subscribers;
  - *usage*: the reviewer's model calls are recorded on the session as `internal_agent_usage_recorded`,
    as before, and its log records no usage;
  - *delete*: a root's tree goes with its folder (`forget_tree` forgets the open log), and a child's
    folder with `delete_children`, which does too;
  - *export*: no session export exists in this workspace;
  - *the TUI and web trees* are drawn from the daemon's tree and listing, which never hold it, so no
    client changed;
  - *a history format conversion* (`convert_tree`) converts the root's and `subagents/` logs only. The
    one gap: at a future format bump, a reviewer log of the older format fails to open owned, is
    removed, and its next review starts one over with a pass over the session's log;
  - *a root publishing into a scaffold*: the folder is made only for a persisted session.
  The golden scenario's snapshot and `subagents/index.json` name none of its ids.
- **A turn** holds the review's `approval_id`, its cut (the session's `approval_requested`), the
  session's `evaluated` step of the verdict, the tool's name, the `user` turn and the verdict as sent,
  the entries it rendered since the previous turn, and a cursor of what the next delta needs of what
  came before: the run count and the latest 64 runs (numbered by first appearance, and whether an
  input came in each yet), the calls still open with the entry each renders as for each way it can
  end (the outcome is all a termination adds), producer messages accepted and not consumed with their
  rendered bodies (at most 256), the attachments, and the newest run's `AGENTS.md` (a delegated
  session: what it inherited). A turn that starts an epoch holds `epoch` instead of entries: the
  messages and trajectory a later fresh turn can still keep, what was left out before them, and the
  verdicts it carried.
- **Each review reads only its delta.** `prepare_review` (`approval/flow.rs`) takes the turns since
  the log's latest epoch from the reviewer log's window, finds its cut in the session's model context,
  the call's start in `state()` and its arguments in the committed turn the context pins, and reads
  `history_range(previous cut + 1 ..= cut)`: from the window when it holds that, else from the file's
  nearest line-index mark. Only the first turn of a log reads from the log's start, once. The turn is
  recorded after the session's log evaluated the verdict, with that step's sequence; one review runs at
  a time per session, from reading the reviewer's log to recording its turn, so the turns follow the
  evaluations' order. A revert since the review read the log leaves its turn out.
  `repeated_approvals_build_one_append_only_reviewer_conversation` asserts, through the engine and a
  restart, that the first review reads from sequence 1 and every later one from the previous cut plus
  one, exactly the records between the two cuts, and that the third request after the restart is the
  stored bytes of the first two turns plus its own.
- **A failed first review** (no verdict, so no turn) would have made the next review pass over the
  whole log again. Its turn is kept in memory with the session's cut count, and the next review renders
  a fresh turn from what that pass kept and the delta since its cut, which is what a pass over the
  whole log gives (`a_failed_first_review_is_read_on_from_rather_than_passed_over_again`). After a
  restart such a session's next review passes over its log once more. A failed review after the first
  needs nothing: its entries fall into the next turn's delta, from the last recorded cut.
- **The epoch is a checkpoint of the reviewer's log.** `context_window_start` takes the latest turn
  with `epoch` as where the window starts, `LettingGo` reads it as `context_from`, and `narrow_after`
  narrows at it: the log holds its creation and its turns since the epoch, with nothing pinned (it has
  no runs, calls, or inputs). The store writes its `state.json` at each epoch and as its session leaves
  memory (`evict`, `release_ownership`), and it opens from that file. `STATE_VERSION` stays 9: no log an
  earlier build wrote holds the record, and the folds of every earlier log are unchanged; the digest
  test's digest was recorded again for the new golden scenario's logs.
- **A fresh turn from kept entries gives the render's turn.** The render budgeted every entry of the
  log; the epoch keeps fewer, and it is exact because what it lets go of no later fresh turn keeps,
  under the same bounds. Messages: the tasks (the first three with the user's authority, the first
  instructions) are the same for every later epoch, and the newest kept start no earlier than the last
  epoch's did, as the walk from the newest breaks at the first that does not fit; so the epoch keeps
  what its fresh turn kept, with the count, tokens, and first position of what it left out for the
  marker and the total. Trajectory: halving keeps a suffix whose length depends on the total count, so
  the epoch keeps the newest entries up to the first that puts them over the budget and the count of
  the rest; any half that fits starts after that entry, and any half reaching it is over the budget.
  Carried verdicts are the last five of the epoch's and the turns since. A property test over six
  generated logs of 60 steps, with both budgets dropping entries and several epochs each
  (`the_log_sends_what_the_render_gives_across_epochs_and_budgets`), checks every review.
- **A delegated session's opening** (`root_messages_until`, passes over its root's and ancestors'
  logs) is read once, at its log's first turn: its messages are the epoch's first, at position 0, and
  its root's `AGENTS.md` is the cursor's `inherited`, so no later review reads another log.
- **Existing sessions** have no reviewer log: the next review passes over the session's log once,
  sends a fresh turn, which says the conversation started over and carries the last five verdicts the
  log records when it records any, and records it as the log's first epoch
  (`a_session_with_no_reviewer_log_starts_one_with_a_fresh_turn_carrying_its_verdicts`). The
  provider's cache misses once. Nothing is converted.
- **Revert and fork, decided here.** A revert of the session through a sequence keeps the turns whose
  `evaluated_seq` it keeps, cutting the reviewer log after the last of them (`truncate_after`, whose
  native fold moves the window back to the epoch they are in) and writing its `state.json` again; a
  revert before every verdict leaves the creation alone, and the next review starts over from a pass.
  The render did the same: a review whose evaluated step a revert cut has no verdict. A fork copies its
  source's turns with `evaluated_seq` at or before its point, from the latest epoch among them, into a
  reviewer log of its own (a new id), so the fork's conversation goes on as the source's did, and as
  the render of the fork's log would have. Either failing leaves no log, so the next review starts one
  over. `a_revert_cuts_the_reviewer_log_a_fork_copies_it_and_a_delete_takes_it` tests the store, and
  the golden scenario records both through the engine.
- **The comparison with today's render.** `render` and what it calls stay as a test-only reference
  (`approval/context/reference.rs`); the new code shares only the formatting of entries and the request
  block. Every reviewer test now also drives the reviewer's log over its log, as the flow does, turns
  round-tripped through JSON, and asserts it sends the render's conversation for that review
  (`context/tests/log_sends.rs`), epochs and budgets included, and so does every review of every golden
  log with roomy bounds and with bounds that start an epoch at nearly every review
  (`golden_logs_send_from_the_reviewer_log_what_the_render_gives`). The golden logs before this phase
  hold 13 reviews and 3 verdicts, all in format 1, as their scripted reviewer failed; so a new scenario,
  `history/3/reviewer`, records verdicts of each kind, a user's answer, enough reviews to start over, a
  fork after them, a revert before them, and a delegated child's review, and every turn its reviewer
  logs hold is what the render of the session's log gives with the same bounds
  (`the_golden_reviewer_logs_hold_what_the_render_gives`: 11 turns, 3 of them an epoch's first, one of
  them started over with five verdicts). **Where they differ, by design:**
  - a root session's fresh turn after its first reads the newest run's `AGENTS.md` before its cut,
    where the render kept the one before the first review for every epoch: the user's "read fresh"
    (`an_epoch_reads_agents_md_afresh_where_the_render_kept_the_first` shows the turns equal but for
    those lines). A delegated session keeps what it inherited when its log was created, where the
    render read the root's log again at each review (the same text, unless the root's log was reverted
    before the delegation);
  - a session with no reviewer log starts with a fresh turn, where the render would have sent every
    turn since its first review;
  - a turn stays as it was sent: when the reviewer model changes window, the render's epochs move
    retroactively and its fresh turns could reach entries the earlier bounds dropped, while the log's
    next epoch renders from what it kept;
  - the cursor keeps the latest 64 runs, 256 open calls, and 256 waiting messages: past those, a run
    would be numbered anew, a call's end or a message's consumption would have no entry;
  - a verdict the daemon stopped before recording is no turn, as a failed review's is;
  - concurrent reviews of one session wait for each other, where the render let a later review render
    while an earlier one was in flight.
  The system prompt is read at each review, as before: with the turns stored, a changed `approval.md`
  changes only the system turn, which the next review sends and whose cache then misses.
- **Measured** on 2026-11-08 in a release build, on the synthetic native log of phase 7
  (`forever_measure generate <dir> <times> 0 native`), each review given a verdict so the reviewer's log
  holds a turn per review, the last review timed both ways, the reviewer's window at 122,000 tokens
  (`reviewer_measure.rs`, three runs each, spread under 10%):

  | Length | Log | Reviews | Today: time | Today: records read | Log: time | Log: records read | Turns held |
  |---|---|---|---|---|---|---|---|
  | 1x | 15.1 MB, 7,903 records | 30 | 108 to 110 ms | 7,903 (15.7 MB as JSON) | 0.82 to 0.90 ms | 39 (56 KB) | 29, 0.66 MB |
  | 10x | 155 MB, 80,731 records | 306 | 1,216 to 1,260 ms | 80,731 (160 MB) | 1.52 to 1.55 ms | 45 (84 KB) | 34 since the 9th epoch, 1.33 MB |

  **So an escalated approval's review now costs about a millisecond and reads the records between two
  reviews, where it read the whole log, 0.1 s at today's largest and growing with it.** What the
  reviewer's log holds is the turns since its latest epoch, bounded by the reviewer model's window (the
  turns, the entries they repeat, and the epoch's kept entries): 0.66 MB and 1.33 MB here. The model
  call itself is not counted, and is the same either way; a log's first review costs one pass, as every
  review did. Daemon RSS was not measured.
- **Not done or not verified.** The engine's revert and fork of a session with a reviewer log are tested
  through the golden scenario and the store, not by a test of their own; a reviewer log of a session
  that is not persisted is never created (a review runs after the session's first input persisted it);
  the cursor's bounds were chosen, not measured on real logs, as no real reviewer log exists yet.

**Closing note: what the branch did, and what remains open.** Phases 1 to 8 made a session's memory
follow what it uses rather than its log: an ACP session holds about one turn and its `state()` folds, a
native one the model's context since its last checkpoint and what that pins, and the reviewer the turns
since its last epoch; each opens from `state.json` without a full parse, the bounds of section 6 keep
the folds and the file flat to 100 times today's largest log, and an ordinary turn, a compaction, and an
escalated approval read no pass over the whole log. Measured: an ACP session's live heap fell by 74 to 81
percent and its open's peak by 85 to 90; a synthetic log at 100 times holds under 2 MB without
delegations; a native log that compacts holds 1.0 to 2.7 MB from 1 to 100 times; a review reads the
delta between reviews. Open:

- **The harvest's delegation records still grow** with the log (phase 5): about 26 MB of heap at 100
  times the stress rate. Bounding them is the delegation registry's change.
- **A damaged log reads whole**, at the old peak (phase 4), and so does a log of an older history format
  on a read-only open; so does a native log that went from ACP to native at its first model context.
- **Native children with no checkpoint parse at each tree load**, as they have no `state.json`, and so
  does any child this process never owned (phases 4 and 7).
- **The long native scenario of phase 6 does not go through a real log** (phase 7): its through-disk
  counterpart is the synthetic native log.
- **Daemon RSS was not measured for native sessions**, nor for phase 5's bounds or the reviewer's log;
  the ACP numbers are phase 3's and 4's.
- **The log's size on disk grows forever** (section 9), and with it older pages and a fold from the
  start; only sealed chunks would bound it.
- **The reviewer's log**: a future history format bump starts reviewer logs over instead of converting
  them; a session whose reviewer keeps failing passes over its log once more after each restart; the
  cursor's bounds are chosen, not measured.
- **Real logs were checked at their tips**, not at every turn boundary, after phase 4; the store's
  native forever agents, once one runs, should be measured as section 8 asks for phase 7.

**Storage, measured after phase 8** (2026-11-08, a converted copy of yunhao-labserver's store, every
windowed log opened and sent out of memory): 161 `state.json` files took 49.1 MB, 13.4% of the 366.2
MB of logs (blobs 287.3 MB). By log size: 27 logs under 200 KB had 4.1 MB of files for 2.2 MB of log,
52 of 200 KB to 1 MB had 12.4 MB for 27.4 MB, 69 of 1 to 5 MB had 25.1 MB for 167.9 MB, and 13 over
5 MB had 7.5 MB for 106.8 MB. So logs under 1 MiB no longer get one, which leaves about 32.6 MB. What
grows with a log: a native checkpoint's records (memory 6.7 KB, `AGENTS.md` 0.7 KB a record at the
median, and a composed prompt of at most 7 KB when it changed), and the reviewer's log, at 2 to 6 KB
a review in the golden scenario.

**Fixes after review (2026-11-08).**

- **A pass that ends before what a native window holds.** A range ending before the window,
  its creation left out, started at the window's position after the pinned records and ended
  at 0, so `History::held` sliced backwards: an `events.page` with `before` of 1 or less on a
  native window with pins panicked, as did a range ending before it starts on any log.
  `History::new` now starts no later than it ends, for both of a pass's constructors
  (`a_pass_ending_before_what_a_native_window_holds_gives_only_what_precedes_it`).
- **A log gone native validates as a reopening's first model context does.** `read_whole`
  validates a log whole again when its agent turned native after an ACP agent's window let go
  of what settled, but a revert's fold (`fold_prefix`), which reads such a log whole for its
  model and drops the window, kept its record-at-a-time validation, and so did the rebuild after
  a refused append or a failed write (`rebuilt_validation`) of a log that had read itself whole.
  With the window gone nothing read it whole again, so a compaction's elision of a call the ACP
  agent ended was refused ("tool elision appeared before its start"). Both now validate whole
  when the log is native and the record-at-a-time pass let go of anything, as `read_whole` does
  (`a_log_gone_native_validates_as_its_file_after_a_revert_and_a_refused_append`).
  `assert_window_reads_as_reopened` no longer skips a log holding its records whose validation
  let go, and for a native model's log it also compares validation under the native rule, which
  keeps the calls in view; with the fixes turned off it fails right after the revert.
- **A run keeps its replay base.** Phase 7 held that the latest 16 attempts that ended hold a
  replay's base, but an attempt abandoned before its stream started ends without an evaluation and
  does not move the base, so with unlimited overload retries 17 such attempts let the base go and
  the next `model_replay_evaluated` was refused, failing the run. Each run's validation now notes
  the attempt it evaluated last (`RunAttribution::replay_base`), and that attempt stays while its
  run does, beside the latest 16 that ended. A record at a time and a whole read keep the same, as
  an attempt becomes a base only while it is in flight
  (`a_replay_base_stays_however_many_attempts_end_after_it_unevaluated`). `STATE_VERSION` rose from
  9 to 10, the digest test insisting.
- **A `state.json` whose line index misses its window folds from the start.** `window_start` took
  the first line the index holds at or after the window without checking the index reached it,
  so a file whose index starts later, or skips the window's line, opened a window missing its
  first records. An intermediate build wrote such files, and only scratch copies ran it. Now a
  sequence between the window and the first line the index holds must be one the file skips
  (`LineIndex::gaps`), or the file is a mismatch
  (`a_state_file_whose_line_index_misses_its_window_folds_the_log_from_its_start`). A stricter
  check of validity changes nothing a fold gives, so the digest held and `STATE_VERSION` did not
  rise for it.
- **A state write racing a revert is left out.** `refresh_state` took the state under the session's
  slot, let go of it, and wrote and noted it after; a revert in between removed the file and noted
  nothing written, then the rename in flight put back the file from before the cut, which no
  longer matches, and noted the log written as far as it then was, holding writes off until the
  log grew past that again. The cut count is now taken before the state, and the write is left
  out, or only its note when a revert comes between the check and the rename
  (`write_state_unless_cut`, which the reviewer's log uses too;
  `a_state_write_racing_a_revert_leaves_neither_its_file_nor_its_note`, through a test-only seam
  between taking the state and writing it).

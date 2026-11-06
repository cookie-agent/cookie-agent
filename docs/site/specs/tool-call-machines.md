# The machine on a tool call

Status: **implemented** (2026-11-05; client protocol 1.89.0), with the user's
decisions of 2026-11-05 in [section 8](#8-decisions), and a
[preview](tool-call-machines/preview.html). Finishes item 5 of
[machines on demand](machines-on-demand.md) section 5.4 ("each routed call's
machine as a badge in its tool header").

## 1. Goal and scope

**Today** nothing on a tool call's row says where it ran:

- The daemon takes `machine` out of a routed call's arguments before it
  prepares the call (`PublishedTool::route`, `crates/engine/src/runtime.rs`),
  so a tool's presentation never mentions it. An open row shows it only in
  the raw-argument fallback, for a call whose presentation has no argument
  blocks.
- A call that leaves `machine` to the default has none to show anywhere.
- An approval card names no machine: `tool_source_label` says `cookie` for a
  bridged call and nothing outside ACP. The approval reviewer, in contrast,
  is told `on <runtime>`.
- `reduced`, the default transcript mode, folds the rows away, so a run that
  worked on three machines reads `Worked for 2m`.

The information is already recorded. `ToolPresentation.tool.source` holds the
runtime: recorded sessions show `runtime` sources for routed calls
(`unicorn` / `unicorn_uniflow`, from an ACP agent hosted on `labpc`) and `acp`
sources for the agent's own tools (`labpc`).

**This spec** gives each call a **machine chip** on its header, and carries it
to the approval card and the reduced rows, once a session has worked with two or
more machines. A session on one machine looks as it does today.

Out of scope:

- what a command does on its own, such as `ssh unicorn …` inside a `bash` call
  or an ACP agent's own shell doing the same: the chip says where the tool ran,
  not what it reached;
- filtering the transcript by machine;
- changing how models or agents name machines.

## 2. The chip

```text
web, full mode (right-aligned column before the status)

 › $ Run cargo test -p cookie_agent_engine        ● yunhao-labserver   ✓
 › ▤ Read handoff/HANDOFF.md                      ● unicorn            ✓
 › ✎ Edit crates/web/src/render/transcript.rs     ● yunhao-labserver   ✓
 › ⌕ Search memory                                                     ✓
 › ⇄ Transferred labpc:~/a.bin → unicorn:~/a.bin                       ✓

TUI, 120 columns

 ▸ 💻 Run cargo test -p cookie_agent_engine              ● yunhao-labserver
 ▸ 📖 Read handoff/HANDOFF.md                            ● unicorn
 ▸ 💻 Run nvidia-smi                                     ● yunhao-labpc  [background]

narrow: a phone, or a TUI under 60 columns (shared prefix dropped)

 ▸ 💻 Run cargo test -p cookie_…     ● labserver
 ▸ 📖 Read handoff/HANDOFF.md        ● unicorn
```

- **Name and colour.** The machine's name in its existing colour
  (`badges::badge_hue`), the one its session badges, cards, and bottom-bar entry
  already use on the web. The dot carries the colour; the name stays readable
  text, because eight hues can collide and colour alone is not accessible. The
  TUI has no machine colours yet; it gets a table of the same eight slots from
  its palette, in the web's order, so a machine has one colour in both clients.
- **Place.** A column just before the status (and before `[background]` or
  `waiting for you`, which push it left until the call ends), on the first line
  of an open row whose header wraps. The column is as wide as the longest name
  among the session's machines, so dots and names line up and can be scanned
  down the transcript, which a chip after a variable-length title could not do.
- **Under pressure.** The chip outranks the argument. Today a TUI row gives up
  the status suffix, then the argument, then the label; the chip goes after the
  argument and before the label, shortened first.
- **Short name.** On a narrow row the chip drops the prefix the session's
  machines share, up to a `-`, `_`, or `.`: `yunhao-labpc` and
  `yunhao-labserver` become `labpc` and `labserver`, where cutting the end would
  make them look alike. With no shared prefix it stays whole, then cut with `…`.
  The full name is the chip's tooltip and accessible label on the web.

## 3. When it shows

Chips show while a session's machines come to **two or more**: those attached now,
leaving out `global`, and those of the calls in its transcript, counted by machine,
not runtime. The machine hosting the session's ACP agent counts as attached. Then
every call that has a machine shows its chip, even those on the default, since a row
with no chip would read as "the default" only to someone who knows which one that is.

Calls count so that detaching back down to one machine keeps the chips: an older row
that ran on the detached machine still says so, and new calls are labelled too. A
machine attached and detached without being used leaves nothing behind. A subagent's
session decides for itself; its parent's delegate row says where it works
([section 5](#5-other-places)).

## 4. Which calls have a machine

| Source | Chip |
| --- | --- |
| `runtime`, except `global` | that runtime's machine |
| `acp` | the machine hosting the agent |
| `mcp` with a `runtime_id`, except `global` | that runtime's machine |
| `runtime` `global`, `base`, `plugin`, `mcp` without a runtime | none: not tied to a workspace machine |
| no source (a call that named no tool, or a route that failed) | none: it ran nowhere, and its error names the machines |

`transfer` is a `global` tool and has no chip; its own title names both ends
(`Transferred {from} → {to}`).

### 4.1 Where the name comes from

Nothing new is recorded on a tool call. `ToolPresentation.tool.source` holds the
runtime, and clients look up its machine with `badges::runtime_machine`, from the
listing they already keep. A runtime the listing does not know shows its ID. While
no listing has answered, a client shows no chips, rather than `local` turning into
a machine name a moment later.

The owner's listing is `runtimes.list`. A guest of a conversation share is refused
that, so **`session.runtimes.list`** gives a guest the same shape for one session:

```text
session.runtimes.list { session_id } -> RuntimesListResult
```

- **Who:** any client. Guests may call it for a session of their tree
  (`guest.rs` `access`, reading in the tree), and the web client calls it in place
  of `runtimes.list` when it is a guest's.
- **Covers:** the runtimes the session and its subagents attach (`attachments` and
  `delegated_attachments`), without `global`. A runtime since detached, or an ACP
  agent's host that is not attached, is not covered, so a guest sees its ID on a row
  that shows a chip for it.
- **Each entry:** `runtime_id`, `lifetime`, `connected`, `machine`, `platform`,
  `home`. `capabilities` is empty and `message`, `active_runs`, and
  `background_calls` are left out: they describe the daemon beyond this session,
  and a connection error can hold hosts and usernames. `instances` is empty.
- **Side effect:** since it fills the listing the guest's page already reads, guest
  pages show paths as `~/…` and session badges by machine, and the Runtimes tab
  gains the status it now leaves out. That amends
  [conversation shares](conversation-shares.md).

A listing gives a runtime's machine now, so a runtime later moved to another
machine relabels its old calls. The user accepted this rather than record a name on
each call.

## 5. Other places

- **Approval cards** (web `approval_source`, TUI `approval_body`). They show the
  machine under the same rule as the row. This is the place it matters most: the
  user is deciding whether a command may run, and the same command means something
  else on another machine. `tool_source_label` no longer says `cookie` in any
  session; an ACP agent's own MCP server keeps its name, beside the chip, since the
  title drops that prefix.
- **Reduced mode, live row.** The tool line of `WorkLines` leads with the chip:
  `● unicorn  Run nvidia-smi`, and `Waiting for approval` keeps it too.
- **Reduced mode, folded row.** `WorkSummary` gains the machines of its calls,
  in first-use order: `Worked for 2m 14s · 14 tools · ● labpc ● unicorn`. Past
  three machines it counts the rest (`+2`).
- **Delegate rows** on the web name the machines of the subagent's session
  (`badges::session_machines` on the child's meta), beside its agent and handle.
  They appear even when the root itself is on one machine, because the row is
  the only sign in the root's transcript that the work went elsewhere, and stay
  quiet when the subagent is on the only machine its parent is on. The TUI has no
  delegate row to carry them.

## 6. Implementation

- **Protocol:** `session.runtimes.list`, client protocol 1.89.0.
- **Server:** the method (`runtimes::session_runtimes_list`), and its entry in the
  guest `access` table. Tool calls do not change.
- **`client_state`:** `call_machines` is the one answer both clients share:
  `call_runtime` for section 4, `MachineLabels::of_session` for section 3 and the
  short names of section 2, and `work_row` for section 5. `SessionState::call_runtimes`
  folds the runtimes a session's calls ran on, and `WorkSummary` and `WorkLines` carry
  theirs.
- **Web:** a guest's connection reads `session.runtimes.list` where an owner's reads
  `runtimes.list`, and again when a runtime attaches. `tool_html` draws the chip in
  both of its header forms, and every cached row's key includes the labels'
  fingerprint.
- **TUI:** `tool_child_layout` takes the chip's columns from the title's budget, and
  the layout caches key on the same fingerprint. A title that wraps on an open row
  has the chip after it, not in the column.
- **Tests:** `call_machines`'s rule, table, and prefix cases; the TUI's
  `tests/machines.rs` and a web row test for both header forms; what
  `session.runtimes.list` covers and leaves out, and that a guest reads its own
  session's alone.

## 7. Not chosen

- **A coloured edge or band per machine.** Quick to scan, but carries no name,
  collides at eight hues, and competes with the status tints on TUI rows.
- **A separator row when the machine changes** (`── unicorn ──`). No repetition,
  but a row scrolled to mid-run doesn't say where it is, and work that alternates
  between machines turns into a stack of separators.
- **The chip leading the row.** Pushes every title to a different column.
- **`machine` kept in the arguments.** Says nothing for default calls, and is
  buried in an open row.
- **The machine recorded on each call** (`ToolCallStart.machine`). Keeps history
  true and needs no listing, but records what the listing already says. Not inside
  `ToolSource` in any case: `engine_permissions::identity` hashes the whole source
  into a tool's approval key.

## 8. Decisions

The user's, 2026-11-05:

1. **The name's source:** nothing recorded on tool calls; guests get a session-level
   runtime list (section 4.1).
2. **When chips show:** only with two or more machines (section 3). Detaching back
   down to one keeps the chips on older rows.
3. **Reduced mode:** machines on the live and folded rows (section 5).
4. **The `cookie` chip on an approval card:** removed in every session (section 5).

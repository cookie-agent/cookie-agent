# Reduced transcript display

Status: **approved draft** (2026-10-29), with the user's decisions of
2026-10-29 in [section 10](#10-decisions). Client-only: no client, plugin,
or runtime protocol change, no session history change, no `config.toml`
change.

## 1. Goal and scope

**Today** both clients show every part of an agent's work in the
transcript: each thinking block (collapsed to a `💭 ▸ thought for 12s`
row), each tool call with its header and status, the text the model wrote
between tool calls, and then its reply. A run with forty tool calls is forty
rows the user scrolls past to find the answer.

**This spec** adds a second **transcript display mode**, `reduced`, shown
the way Codex and Claude Code show agent work:

- while the agent works, its work so far is **one row**, `Working… 42s`,
  with a muted second line naming what it is doing now: the newest line of
  its thinking or the title of its newest tool call (section 4);
- once that work ends it becomes **`Worked for 1m 23s`**, folded, with the
  reply below it (section 5); opening the row shows the work as the `full`
  mode does today (section 6);
- what needs the user (approvals, an agent's questions) and everything
  that is not the agent's own work stays shown (section 3).

Each client keeps the mode as its own setting: the TUI in `tui.toml`,
which `/transcript` and Ctrl+O write (section 8), the web per browser in
Settings › Display (section 9). `reduced` is the default in both clients;
today's behaviour is mode `full`.

Out of scope:

- a per-session or per-agent mode: the mode is the client's;
- sharing the mode between clients through the daemon: like the
  diagnostics filter, it is a view choice of each client;
- changing what `full` shows;
- the CLI (`cookie run` and friends), which prints no transcript rows.

## 2. What folds

The unit is the **assistant block**, `TranscriptItem::Assistant`: the
stretch of one run's output between two rows that are not the agent's
(a run start, a user message or steer, a producer message). Blocks already
split at those rows, so a steer mid-run gives two blocks, each folded on
its own.

A block's children divide into its **work** and its **reply**:

- **reply**: the block's trailing `Text` and `MediaFile` children, after
  its last child of any other kind; with no other kind, the whole block is
  reply;
- **work**: every child before the reply: `Thinking`, `Tool`,
  `CommittedTool`, `Attribution`, `Notice`, and any `Text` written between
  tool calls ("I'll check the reducer next").

A block with no work renders exactly as in `full` mode: a plain reply
gets no `Worked for` row. A block with work but no reply (the run ended on
a tool call, failed, or was cancelled) is the work row alone.

While a block streams, text after its newest tool call shows as reply as
it arrives; if another thinking part or tool call follows, that text was
not the reply after all and moves into the work. The reply that remains
when the block seals is the one that stays below the row.

`Notice` children (`model interrupted`) stay in the work and also mark the
work row (section 5).

## 3. What never folds

Reduced mode only folds the work children of assistant blocks. These keep
rendering as in `full` mode:

- user messages, producer messages, goals, compaction rows, AGENTS.md,
  skill, plugin-message, and memory-change rows, the system prompt and
  tools rows, the ACP agent's plans, notices, and session messages;
- diagnostic `Event` rows, filtered by the diagnostics level as today
  (the two settings are independent);
- the block's footer (`⚡ 42.1 tps · 12.5K ctx · $0.0040`);
- **a tool call waiting on the user**: an approval, or an ACP agent's
  question tied to a call, renders below the work row, outside the fold,
  with its inline approval preview, until it is answered or expires. Once
  answered it folds into the work again. Approvals and questions shown in
  their own panels and bars are unchanged.

## 4. The working row

A block that is still open (its run is going and nothing has split it)
shows its work as one row in place of the work children:

```
▸ Working… 42s · 3 tool calls
  Reading crates/client_state/src/reduce.rs
```

- **No spinner of its own.** The row has the folded chevron, like a
  worked row, and the label `Working…`, then the elapsed time (section 7),
  ticking each second. The spinner stays where each client already shows
  it (below).
- **Subtext**, one muted line, truncated with `…` to the width, from the
  block's newest work child:
  - a **thinking** part: its newest line that has text. When the thinking
    has Markdown headings or lines that are bold alone (the reasoning
    summaries of OpenAI models start each step with `**Inspecting the
    reducer**`), the newest of those instead, without its markers, so the
    line changes per step rather than per word;
  - a **tool call**: its header as the tool row shows it, its title and
    primary argument (`client_state::tool_header`), such as `bash · cargo
    test -p cookie_agent_client_state`; a call waiting on approval reads
    `Waiting for approval: <title>`;
  - **text** between tool calls: its newest line, like thinking;
  - nothing yet (the run just started): the line reads `Starting…`, so
    the row keeps its height when the first part arrives.
- **Counts.** After the time, muted: the tool calls so far, `· 7 tool
  calls`, left out while there are none.
- **Click** opens the work as a sealed row does (section 6); open, the
  live work streams in place as in `full` mode.

The spinners stay as they are in both modes (user decision): the web's
trailing `working…` row with its spinning circle after the transcript's
last row while a run goes (and `waiting for your answer` while an agent
question waits), and the TUI's `◐ working` in the bottom bar. A working
row never hides or replaces them, so nothing appears or vanishes as
blocks open and seal.

## 5. The worked row

When the block seals (its run ends, or a user or producer row splits it),
the working row becomes:

```
▸ Worked for 1m 23s · 7 tool calls
```

- the elapsed time is the block's (section 7), fixed from then on;
- `· 7 tool calls` counts the work's `Tool` and `CommittedTool` children,
  left out at none;
- a failed call among them adds `· 1 failed`, in the warning colour;
- an interrupted or cancelled block reads `Stopped after 1m 23s`, with the
  same counts;
- the chevron is `▸` folded and `▾` open, like a thinking row's.

## 6. Opening a row

Clicking the row (Enter on it in the TUI, as for a thinking or tool row)
opens it: the work children render in place, exactly as `full` mode
renders them, with their own thinking and tool rows folded or open as the
user leaves them. Clicking again folds it.

- Open rows are view state, kept per block for the life of the view like
  a tool row's, not saved anywhere.
- Anything that scrolls the transcript to a row inside a folded block
  opens the block first.
- Switching modes keeps the transcript's anchor row on screen, as a
  diagnostics change does; a block folds again on a switch back to
  `reduced`.
- Copying a selection copies what is shown: a folded row copies as its
  label line.

## 7. Elapsed time

The time a block worked is measured on durable event timestamps only, so
a replayed session reads the same `Worked for` as the live one, and two
clients agree:

- **start**: the timestamp of the event that opened the block: the
  `RunStarted`, or the row that split the run's previous block;
- **end**: while open, now (ticking); sealed, the timestamp of the event
  that sealed it (`RunFinished`, or the splitting row).

Time spent waiting on an approval or a question counts: it is the wall
time of the work, as in Codex. Format: `42s`, `1m 23s`, `1h 4m`, as the
thinking rows and the background sheet format durations
(`client_state::reply` gains the shared formatter).

The reducer records `opened_at` and `sealed_at` per assistant item
(`SessionState`), beside `child_times`.

## 8. TUI

- **`tui.toml`**: a new key, strict like the rest of the file:

  ```toml
  # How the conversation shows agent work. "reduced" folds each block's
  # thinking and tool calls into one "Worked for" row; "full" shows them
  # all. Default: "reduced". /transcript and Ctrl+O write this key.
  # transcript = "reduced"
  ```

  Values `"full"` and `"reduced"`; any other value is an error naming
  the key. A missing key is `reduced`.
- **Palette commands**, like `/agent-panel`:
  - `/transcript full`: show every thinking block and tool call;
  - `/transcript reduced`: fold agent work into Working / Worked rows.

  Each applies at once to every transcript the TUI shows (a root's and
  a subagent's) and saves the mode to `tui.toml`.
- **Key**: Ctrl+O toggles between the two modes and saves the same way;
  the bottom bar briefly says which mode is on.
- **Saving `tui.toml`** (the first time the TUI writes the file):
  - the TUI writes the file it loaded: `--config <path>` when given, else
    `~/.cookie-agent/tui.toml`;
  - it edits only the `transcript` key with `toml_edit` (already a
    workspace dependency), so the user's comments, other keys, and their
    order stay as written; with no file it creates one holding only that
    key;
  - it rereads the file just before writing and writes atomically (a
    temporary file beside it, then a rename). If the file no longer
    parses, or is gone from an unreadable directory, it is left untouched;
  - a failed write keeps the new mode for this process, and the bottom
    bar says the setting was not saved and why (the path and the error
    kind, never the file's text);
  - other running TUIs keep their mode until they start again: the TUI
    does not watch the file.

  `/events` and `theme` are unchanged: they still never write the file.
- **Rendering**: the transcript layout keys each assistant block's layout
  on the mode and the row's open state, so a switch relayouts only
  assistant blocks. The work row is a new `BlockId` with a
  `PointerTarget` variant resolved in `App::pointer_target_at`, and it
  paints a hover in `apply_hover`; the pointer sweep test reaches it.
- **Docs**: `docs/site/tui/configuration.md` and `docs/tui.toml.example`
  (both now say the file is never rewritten, which becomes: except the
  `transcript` key), and the commands and keys tables in
  `docs/site/clients/tui.md`.

## 9. Web client

- **Setting**: `localStorage` key `cookie.transcript`, values `full` and
  `reduced`, read at start; a missing or unknown value is `reduced`. One
  setting for every tab of the browser, applied live to open tabs on the
  `storage` event.
- **Settings › Display**, a new section (`SettingsSection::Display`,
  labelled "Display", listed after Notifications and before This
  browser), holds the page's view settings, each kept in this browser:
  - **Transcript**: **Reduced** ("agent work folded into one row per
    reply") and **Full** ("every thinking block and tool call");
  - **Diagnostics in the transcript**, moved here from This browser
    unchanged: the same choice as Diagnostics in the session's ⋮ menu,
    still stored under `cookie.event_level`.

  It reads nothing from the daemon, so opening it polls nothing, as This
  browser does.
- **Settings › This browser** keeps installing the page as an app and the
  daemon's token; its diagnostics heading, note, and choice go, and its
  doc comment (`SettingsSection::Browser`) stops naming the diagnostics
  filter.
- **Rendering**: `render/transcript.rs` renders an assistant block's
  folded form as a button row (`data-work`, `aria-expanded`) followed by
  its reply; a mode change re-renders assistant rows only. The working row
  has `role="status"`, as the trailing `working…` row has.
- **Docs**: `docs/site/clients/web.md`: the transcript modes, and its
  Settings list gains **Display** (transcript and diagnostics) while
  **This browser** no longer picks the diagnostics.

## 10. Decisions

The user decided on 2026-10-29:

1. **`reduced` is the default** in both clients.
2. **Text between tool calls folds into the work** (section 2), so a
   reduced block reads as one row and the reply.
3. **The TUI saves the mode to `tui.toml`**: `/transcript` and Ctrl+O
   write the `transcript` key (section 8).
4. **Ctrl+O toggles the mode** in the TUI (Claude Code's key for its
   detailed transcript; it looks unbound in cookie's TUI, to confirm at
   implementation). The web gets no key.
5. **The row counts tool calls and failures** (section 5), not the time
   alone.
6. **The web's setting lives in Settings › Display**, a new section.
7. **Diagnostics in the transcript moves to Settings › Display** too, out
   of This browser.
8. **The spinners stay** (section 4): the web's trailing `working…` row
   and the TUI's bottom-bar `◐ working` show in both modes, and the
   working row has no spinner of its own.

## 11. Tests and phases

Tests (no tests pin the default mode):

- client state: the work and reply split for blocks with no work, no
  reply, text between calls, trailing media, and notices; trailing text
  moving into the work when a tool call follows; the working subtext for
  thinking with and without bold step lines, for a tool header, and for a
  call waiting on approval; `opened_at` and `sealed_at` from durable
  timestamps, equal live and on replay; the row labels for worked,
  stopped, and failed calls;
- TUI: `tui.toml` accepts both values and rejects others; `/transcript`
  and Ctrl+O write only the `transcript` key and keep comments and other
  keys byte for byte, create the file when it is missing, leave a file
  that no longer parses untouched, and keep the mode with a bottom-bar
  notice when the write fails; snapshots of a
  live and a sealed reduced block, an opened one, and an approval shown
  outside the fold; `/transcript` and Ctrl+O switch every view; the
  pointer sweep covers the work row's hover;
- web e2e: switching the mode from Settings › Display, in two tabs;
  the diagnostics choice in Display and no longer in This browser; the
  working row's subtext while a scripted run streams, the trailing
  spinner shown under it, and opening a worked row.

Phases:

1. Client state: the split, labels, subtext, and block times, with tests.
2. TUI: the setting and its saving, commands, key, rendering, and docs.
3. Web: the setting, Settings › Display with the diagnostics moved into
   it, rendering, and docs.

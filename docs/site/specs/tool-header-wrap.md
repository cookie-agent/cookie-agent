# Full tool headers on open rows

Status: **approved draft** (2026-10-29), with the user's decisions of
2026-10-29 in [section 6](#6-decisions). Client-only: no client, plugin, or
runtime protocol change.

## 1. Goal and scope

**Today** a tool call's header is one line in both clients, open or
folded: its icon, its title, and its primary argument
(`ToolPresentation::title` and `primary_argument`), cut to the width with
`…`. The TUI drops the argument altogether when the title alone fills the
row; the web cuts the header with CSS `truncate`. Opening the row does not
help: its body leaves out every argument block that repeats what the
header shows (`client_state::tool_header::shown_arguments`), so a long
command, path, or URL that the header cut is shown nowhere in full.

**This spec:** an **open** tool row's header wraps over as many lines as
its title and primary argument need, so the user sees all of both. A
folded row stays one line, cut as today.

Out of scope:

- text the primary argument itself does not hold. Producers bound it: the
  wire caps it at 1024 bytes, and an ACP agent's is the first line of its
  input, at most 200 characters. When the full value differs, as a
  multi-line command does, the body already keeps its argument block, since
  `shown_arguments` drops only exact repeats;
- headers that are not tool rows: thinking rows, the system prompt and
  tools rows, producer and plan rows;
- the approvals panel and the TUI's approval card, which show their own
  call text.

## 2. What an open header shows

- The **title**, whole.
- The **primary argument** after it, whole, flattened to one logical line
  as today (`flatten_header_argument`: control characters to spaces,
  whitespace runs to one space), when the title does not already name it
  (`ToolCallState::header_argument`). The TUI no longer drops it when the
  title fills the first line: it starts on the next line.
- It **wraps at word boundaries**, and inside a word longer than the line
  (a path or URL with no spaces) at any character. No line cap: the
  wire's 1024-byte bound keeps the header short enough.
- The **status** (the running pulse, `✓`, the failure markers, the
  duration) and the TUI's `[background]` button stay at the right end of
  the **first** line; continuation lines use the whole width under the
  title.
- A **memory URL** argument stays a link across its wrapped lines.
- **Progress** of a running call (the web's `· <progress>`) stays one line,
  cut with `…`, after the header's last line.

Folding the row returns it to one line. A row the client opens on its
own wraps the same way as one the user opened.

## 3. TUI

- `tool_header_title` gains an open variant that returns the header's
  wrapped lines for the width instead of one cut line. Continuation lines
  start at the title's column (after the chevron and icon) and sit on the
  title band like the first line.
- The block region's `header_lines` counts every header line, so a click
  on any of them folds the row, and the hover paints all of them
  (`apply_hover`), as for a one-line header today. The memory link's
  `PointerTarget` covers each of its line fragments, and each hovers.
- A tool row's cached layout depends on its open state and the width
  (adding either to the cache key if it is not there yet), so a resize
  rewraps the header.
- The pointer sweep (`crates/tui/src/ui/transcript/tests/pointer.rs`)
  reaches a wrapped header and its link on a continuation line.

## 4. Web client

- An open row's header `<span data-header>` drops `truncate` for
  `whitespace-normal [overflow-wrap:anywhere]`, and the header button
  aligns its items to the top (`items-start`), so the chevron, icon, and
  status stay on the first line.
- The memory-link variant's three-button header wraps the same way: the
  link button may wrap, and the status button stays at the top right.
- A folded row keeps `truncate` and gets no `title` tooltip (section 6,
  decision 1): opening the row is how to read the whole header.
- `render/transcript.rs` already re-renders a row when it opens or folds.

## 5. Tests

- client state: the open header's text is the title and the whole
  flattened argument, and leaves the argument out when the title names it;
- TUI: snapshots of an open row whose argument wraps over three lines, of
  one whose title fills the first line (the argument starts on the
  second), and of a path with no spaces broken mid-word; a click on a
  continuation line folds it; a folded row is unchanged; the pointer sweep
  covers the wrapped header and link;
- web e2e: an open row's header is taller than one line and shows the
  whole argument, its status stays on the first line, folding it returns
  one line.

One phase: shared helper, then both clients.

## 6. Decisions

The user decided on 2026-10-29:

1. **No tooltip on folded web headers**: a very long title would make one
   too big; opening the row shows the whole header.
2. **The ACP argument stays cut at its first line and 200 characters**; a
   longer value is already in the body's argument block.

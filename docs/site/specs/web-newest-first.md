# The web page opens a session newest first

Status: **implemented** (2026-11-06). Client-only: no client, plugin, or
runtime protocol change, no session history change, no `config.toml` change.

## 1. Problem

The web page reads a session's whole display view before it shows it
(`HistoryReading::Whole`, since `05fc8d474`). A long session waits behind a
skeleton for rows the user may never scroll to. The TUI already opens at the
newest window and reads older pages with `events.page`, and the daemon and
`client_state` need nothing new.

The page did the same from `32ec38005` to `05fc8d474`, and that was dropped
because of how it scrolled: a page arriving while the view rested at its top
left the view at the top, showing the page's oldest rows, which put the row
of history not loaded back in view, which read the next page, and so on to
the session's start.

## 2. Cause

Browsers do not anchor a scroller resting at scroll position 0, by the scroll
anchoring spec's own rule. Measured on 2026-11-06 in headless Chrome 156,
adding 20 rows of 300 px above the view, with rows carrying
`content-visibility: auto` as the transcript's do, marked once drawn:

| Layout | Row in view moved, at the top | 1 px off the top | 300 px off the top | Row added at the end, scrolled up |
|---|---|---|---|---|
| Normal, anchoring on | **6,020 px** | 0 | 0 | 0 |
| `column-reverse`, anchoring on | 0 | 0 | 0 | 0 |
| `column-reverse`, anchoring off | 0 | 0 | 0 | 301 px |

A `column-reverse` scroller's position counts from its end, so rows added
above the view change nothing: the position stays, and no scroll event fires.
These were scripted positions on a test page. **The real page behaved
differently** (section 8): there, Chrome's anchoring moved a reversed scroller
along with the rows too, which section 3.1's last paragraph answers.

## 3. Design

### 3.1 The scroller

`#transcript` becomes a `flex-direction: column-reverse` scroller with **one
child**, which holds everything that is now a direct child, in today's order.
DOM order, text selection, tab order, and find in page are unchanged. The
child takes `margin-bottom: auto`, so that a short transcript and the new
session's cards sit at the top as they do now.

`scrollTop` is then zero at the end and negative above it. `Follow`
(`crates/web/src/follow.rs`) and `Metrics` measure the distance from the end
instead of the distance from the top: a top position jumps when rows arrive
above, with no scroll event to say so. Every other reader or writer of the
transcript's `scrollTop` moves to the same coordinates.

No code writes the scroll position when history arrives, so nothing can stop
a fling. `keep_off_top` and `restore_after_history` do not come back.

The transcript's scroll anchoring is off for the frames a page takes to draw
(`data-page-arriving`, `browser::page_arriving`), and on otherwise, for a reply
growing below a scrolled-up view.

### 3.2 Reading history

The page asks for `HistoryReading::Newest`. `Workspace::load_history` and the
row of history not loaded return from `05fc8d474^`. A page is read when:

- a scroll toward older rows, or a wheel, key, or touch toward them at the
  very top where no scroll event follows, leaves the row of history within
  1.5 screens above the view, and no page is being read;
- the user clicks the row; or
- the content does not fill the box, or the page just read drew no rows (a
  run's end, a setting), up to 5 pages in a row with no new user scroll.

Nothing else reads a page, a page arriving included: it fires no scroll event.
A failed read waits for a click, as it does in the TUI.

While a page reads, the row shows a loading block (asked for by the user,
2026-11-06). The row is as tall with the block as without: growth above a
reversed scroller goes out of a view resting at the top, so a block that grew
would not show to whoever waits there.

### 3.3 Find in page

Find in page only reaches loaded rows now, so searching older messages means
scrolling up to them first. A **Load all** button was built, beside **Latest**,
and the user removed it after trying the page (2026-11-06).

### 3.4 What goes

The loader keeps its skeleton for the first answer. `HistoryReading::Whole`,
`ClientDelivery::ReplayProgress`, `replay_counts`, and the loader's count go,
as nothing else reads them. `view_events` stays on the wire: client protocol
fields are not removed.

## 4. Out of scope

- **Browsers without scroll anchoring.** Safari is believed not to have
  `overflow-anchor` (not tested). Rows read above the view should hold there,
  as they need no anchoring, but a reply streaming below a scrolled-up view
  shifts it, the last column of the table in section 2. Deferred by the user,
  2026-11-06.
- **Taking rows out of the page while scrolling.** `content-visibility`
  already keeps far rows cheap.
- **The TUI**, which already opens newest first.

## 5. Tests

`newest_first` replaces the `whole_session` browser scenario, using real wheel,
key, and touch input: a session opens with history not loaded; one scroll draws
one page; the row in view moves by under a pixel, resting at the top included;
nothing is read while idle; a slow page shows the loading block in view;
scrolling on reaches the first row, and find in page then does too.
`history_gap` fills a gap inside a run. Unit tests cover `Follow` in its new
coordinates and the rule of section 3.2 (`history_scroll`).

A browser scenario's WebDriver click scrolls its element into view with
ChromeDriver's own code, which assumes positive scroll positions, so
`Browser::click` brings a transcript element into view first.

## 7. Decisions

From the user, 2026-11-06:

- `column-reverse`, with position correction only as a fallback.
- Find in page covers loaded rows. **Load all**, approved at first, was removed
  once the user had tried the page.
- Browsers without scroll anchoring are left for a later phase.

## 8. Implementation notes (2026-11-06)

On the real page with the long session's rows, measured by what a row did on
screen in the frame the content grew, a page arriving with the view resting at
the very top:

| Page's setup | Row in view moved | View afterwards |
|---|---|---|
| Anchoring on | 59,526 px | at the very top, which read the next page |
| Anchoring on, rows' `content-visibility` off | 59,526 px | at the very top |
| Anchoring on, the pinned rows and history row anchorable | 59,526 px | at the very top |
| Anchoring off | 0 | as it was, with `scrollTop` unchanged |
| Anchoring off, rows' `content-visibility` off | 0 | as it was |

So anchoring is the cause, and reversing alone did not stop it. Why the small
test page of section 2 did not show this is not known. With anchoring off while
a page draws, a scripted scroll, a wheel, the Home key, and a phone's flicks each
drew one page, moved the rows in view 0 px, and wrote no scroll position.

In reduced mode a long run's pages fold under its one row and draw nothing, so
reading on (section 3.2) can stop at its limit with the view at the very top and
a row of history in view. A wheel, a key, or a finger there asks again, as does a
click on the row; a script's scroll to where the view already is does not.

Headless Chrome took no input on the scrollbar, with CDP or WebDriver mouse
events, and the scenarios hide scrollbars. The user dragged it in desktop Chrome
on copies of their long sessions and reported that it works (2026-11-06).

Not measured: the cost of folding every held event again per page, over all of
a long session's pages.

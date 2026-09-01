# Terminal UI

## Themes

With no theme setting, the TUI automatically chooses the light or Dark Roast
bakery palette. It first queries the terminal background with OSC 11, then falls
back to the last `COLORFGBG` field, and finally uses the light palette when
neither signal is available. Set `theme = "auto"` or `COOKIE_THEME=auto` to
request the same detection explicitly.

Set `theme = "default"` for the light palette or `theme = "dark"` for the bakery
palette's dark roast. Dark is a curated palette with its own surfaces and
accents; HighContrast uses terminal-driven bright ANSI colors on the terminal's
own background. `NO_COLOR` and `TERM=dumb` force monochrome after selection for
every configured theme.

## Composer

Enter submits the composer. Use Ctrl-J or modified Enter to insert a newline.
Arrow keys move by character or visual line; Ctrl-Left and Ctrl-Right move by
word. Ctrl-Backspace and Ctrl-Delete remove a word. Home and End move within a
line, while Ctrl-Home and Ctrl-End move to the start or end of the whole draft.

Ctrl-P opens the command palette. The available commands are:

| Command | Action |
|---|---|
| `/new` | Choose the next root-run agent |
| `/preset` | Select the preset for the next root run and future new sessions; see [Agent presets](agents.md#agent-presets) |
| `/connect` | Connect or update a managed provider |
| `/mcp` | Manage MCP servers; see [MCP servers](mcp.md) |
| `/permissions` | Edit session permission overrides; see [Permissions](permissions.md) |
| `/sessions` | Choose a session |
| `/skills` | List discovered skills, sources, precedence, and permission effects |
| `/<skill-name> [args]` | Invoke a user-invocable skill |
| `/usage` | Show selected-session and session-tree usage |
| `/cancel` | Cancel the active run |
| `/compact [focus]` | Compact the selected idle session |
| `/approve once\|all\|reject\|cancel` | Answer the current approval |
| `/events debug\|info\|warning\|error` | Change the event filter |
| `/help` | Show command help |
| `/quit` or `/q` | Exit the TUI |

A multiline paste beginning with `/` is sent as a normal prompt. Prefix a
single-line prompt with `//` to send one leading `/` literally.

## Steering and the pending strip

Submitting while a run is active calls `run.steer`. The input enters a durable
pending lane and is not model-visible yet. At tool or completion boundaries,
pending inputs are promoted in admission order as separate user messages.

The `Pending` strip means exactly that the model has not seen those messages.
It shows up to three rows and folds additional entries into `+N more`. To recall
the newest pending input into the composer, press Up while the composer is empty
or click any row in the strip. Recall is LIFO even when another row is clicked.
If a run terminates with pending text, the TUI restores that text to the
composer when the session is viewed.

## Selection and clipboard

Drag in the conversation or composer to select text. Ctrl-C copies the selected
text and clears the selection; Ctrl-X cuts only a composer selection. With no
selection, Ctrl-C cancels the active run. Esc clears a selection before it can
count toward the double-Esc run-cancel gesture.

Conversation copy removes role gutters, borders, and code-fence chrome, so code
is copied as raw source. Clipboard writes use OSC 52 and work over SSH when the
terminal emulator supports OSC 52.

## User-message menu

Click a past `USER` message to open its action menu. Use Up/Down and Enter, Esc
to close, or the `c`, `r`, and `f` accelerators.

- **Copy** writes the original message text to the clipboard.
- **Revert** asks for confirmation, rolls the visible branch back to just
  before that message, and restores the message text to the composer.
- **Fork** creates and selects an independent session whose copied prefix
  includes that message.

Assistant and tool rows keep their normal expand/collapse behavior and do not
open the message menu.

## Transcript details

System prompts, context compaction checkpoints, plugin-injected messages, and
committed media parts appear collapsed by default. Click one of these rows to
show or hide its text or metadata. Expansion state is kept separately for
each session. Expanded bodies are display-bounded; oversized content ends with
a truncated-lines indicator while the complete data remains in session state.

## Live tool output

Expanded bash rows show sanitized stdout and stderr while the command runs.
The live preview is capped at 1 MiB and reports when that limit is reached. On
completion, failure, cancellation, or interruption, the same row swaps to the
committed terminal result. Reopening a session renders only that committed
content. Assistant text follows the same rule: streamed partials are replaced
by the committed turn.

## Approvals

An approval modal presents the prepared operation and the decisions allowed by
its constraints: allow once, allow for the session tree, reject, or cancel.
Use the on-screen controls or the `/approve` command. Esc cancels only when the
request is cancellable. Long approval details scroll with arrows, Page Up/Page
Down, Home, and End.

The permission mode appears in the bottom bar. Click it to cycle
`auto-approve -> auto-n -> auto-y -> ask -> yolo`; the mode applies only to
subsequent approvals in the selected session. Hard policy denies and doom-loop
rejection still win in every mode. See [Permissions](permissions.md).

When pricing is available, the bottom bar also shows the selected session's
estimated cost between the permission mode and context usage. Click the cost to
open the `/usage` dashboard. Unpriced sessions omit the segment.

## Usage dashboard

`/usage` opens a read-only view with the selected session first and its complete
delegation tree second. The tree count includes the selected session. Each
section shows request count, input, output, and reasoning tokens, cache reads
and writes, cache hit percentage, estimated cost when configured, and a
per-model breakdown. Models are sorted by descending estimated cost, with
unpriced models last; ties use input tokens and then model name. Token counts
use thousands separators, hit rates use one decimal place, and costs use the
same formatting as the bottom bar. `unpriced` means the active configuration
does not provide all rates needed for the observed token categories, while
`n/a` means a hit rate cannot be computed.

Use Up/Down, Page Up/Page Down, or the mouse wheel to scroll when needed, and
Esc to close. Clicking the selected session cost in the bottom bar still opens
this panel.

See [Usage and cost](usage.md) for recording and pricing semantics.

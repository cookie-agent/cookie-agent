# Terminal UI

## Composer

Enter submits the composer. Use Ctrl-J or modified Enter to insert a newline.
Arrow keys move by character or visual line; Ctrl-Left and Ctrl-Right move by
word. Ctrl-Backspace and Ctrl-Delete remove a word. Home and End move within a
line, while Ctrl-Home and Ctrl-End move to the start or end of the whole draft.

Ctrl-P opens the command palette. The available commands are:

| Command | Action |
|---|---|
| `/new` | Choose the next root-run agent |
| `/connect` | Connect or update a managed provider |
| `/sessions` | Choose a session |
| `/skills` | List discovered skills, sources, precedence, and permission effects |
| `/<skill-name> [args]` | Invoke a user-invocable skill |
| `/usage` | Show selected-session and global usage |
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

## Approvals

An approval modal presents the prepared operation and the decisions allowed by
its constraints: allow once, allow for the session tree, reject, or cancel.
Use the on-screen controls or the `/approve` command. Esc cancels only when the
request is cancellable. Long approval details scroll with arrows, Page Up/Page
Down, Home, and End.

The permission mode appears in the bottom bar. Click it to cycle
`auto-approve -> ask -> yolo`; the mode applies only to subsequent approvals in
the selected session. Hard policy denies and doom-loop rejection still win in
every mode. See [Permissions](permissions.md).

## Usage dashboard

`/usage` opens a read-only view with the selected session and project-wide
rollups. Each section shows request count, input and output tokens, cache reads
and writes, cache hit percentage, estimated cost when configured, and a
per-model breakdown. `unpriced` means the active configuration does not provide
all rates needed for the observed token categories. Press Esc to close.

See [Usage and cost](usage.md) for recording and pricing semantics.

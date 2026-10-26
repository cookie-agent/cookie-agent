# Terminal UI

This page covers the terminal UI: starting it, the composer and its
attachments, the command palette, steering and queueing, the transcript, error
details, background calls, approvals, the runtimes panel, and the usage
dashboard. The [web client](web.md) works with the same daemon from a browser,
the [command line](cli.md) runs a prompt without a UI, and
[Persistent daemon](../daemon/persistent.md) covers running the daemon apart
from the TUI and how plain `cookie` finds one already running.

## Starting the TUI

Start in the workspace where the agent should operate:

```sh
cookie
```

When a daemon already runs on this machine, `cookie` opens the terminal
client on it (see [A daemon already running](../daemon/persistent.md#a-daemon-already-running));
otherwise it starts the engine and the terminal client together in one
process. The bottom bar starts with where the engine runs: the daemon's
address, or `standalone` (after the session's [project](#projects), while
no run is active). Configure the
[theme](../tui/theme.md) and [diagnostic filter](../tui/minimum_event_level.md)
in the independent client configuration.
When at least one model is available, select an agent and model if needed, type
a request in the composer, and press Enter. If no authored root agent is
runnable, the engine supplies the built-in `default` coding agent.
Every clickable element highlights under the mouse pointer; text fields, such
as the composer and picker searches, show their caret instead.

Useful first commands, from the [command palette](#command-palette), are
`/sessions`, `/new`, `/model`, `/compact`, and `/cancel`. [Agent](../extend/agents.md) covers authored prompts and permissions.

Model fallback progress is local to the session. For a chain `[A, B, C]`, once B
commits a successful model turn after fallback, later turns and runs continue
from `[B, C]`. A later successful fallback to C leaves `[C]`. This survives
replay and restart, and the TUI updates its next-run model accordingly. A failed
attempt alone does not advance the durable selection; if every candidate fails,
the last accepted/successful starting point remains available.

Choose an earlier model in the picker, or use `cookie run --model ...` when
resuming, to explicitly restore its suffix. Explicit agent/preset choices take
precedence. New sessions use the configured starting selection; configuration
files and previous runs' frozen chains are never rewritten.


## Composer

Enter submits the composer, and Alt-Enter queues it (see
[Steering, queueing, and the pending strip](#steering-queueing-and-the-pending-strip)).
Use Shift-Enter, Ctrl-Enter, or Ctrl-J to insert a newline.
Arrow keys move by character or visual line; Ctrl-Left and Ctrl-Right move by
word. Ctrl-Backspace and Ctrl-Delete remove a word. Home and End move within a
line, while Ctrl-Home and Ctrl-End move to the start or end of the whole draft.
Tab and Shift-Tab cycle the draft agent; Ctrl-T cycles the draft model's
variants, like clicking the bracketed variant in the composer title. In an
existing session the agent, model, and variant are the session's: once a pick
settles it is shared with every client attached to the daemon, whose drafts
follow it and say another client picked it. The latest pick wins. Ctrl-A
selects the whole draft; typing or pasting over a selection replaces it. The
composer keeps keyboard focus while you click around the conversation; only a
panel, the command palette, or an approval on top of it takes focus away.

The agent and model panels include search fields. Agent search matches agent IDs
and descriptions; model search matches display names and `provider/model_id`.
Both are case-insensitive. Use Down, Tab, or Enter to move from the search field
into the matching rows.

On Unix, Ctrl+Z suspends the TUI to the shell as it would any terminal
program, and `fg` brings it back with its screen redrawn. A TUI stopped some
other way, such as `kill -STOP` or an editor opened from `/config` or `/memory` being
suspended along with it, takes its terminal back and redraws when it is
continued.

### Attachments

When the selected model accepts files (images, PDFs, audio, or video; see
[Attaching media](../sessions/media.md)), `/attach` asks for the path of a file and
attaches it to the next message. Pasting text that is exactly the path of one
existing file, as a terminal pastes a file dropped on it, asks whether to
attach that file; Cancel pastes the path as text instead. The TUI uploads the
file to the daemon, which may run on another machine, so the path only needs
to exist where the TUI runs. Before uploading, it refuses a kind of file the
model does not accept (and says which kinds it does), a file larger than the
model's limit, and an image with more pixels than the model takes, naming the
limit; it does not shrink images. Each attachment shows on the composer's
bottom border as `📎 name · type · size`; Backspace at the start of the draft
removes the newest one. A message may be attachments alone. A message taken
back from the pending strip, or handed back after it failed to send, returns
its attachments with its text.

## Command palette

Press `/` in an empty composer, or Ctrl-P at any time, to open the command
palette. The palette has its own search field: what you type there filters the
commands and never touches the composer, so a half-written message survives
opening, searching, and closing the palette. Up and Down move, Enter chooses,
and Esc closes. Matches rank exact names first, then prefixes, then substrings.
Some commands have aliases, listed below, that match the search the same way
but are never shown in the list.

Commands that need input ask for it in a follow-up step instead of taking
typed arguments; `/rename` alone takes its title after it in the search.
Esc in a step returns to the command list with the search intact.

| Command | Action |
|---|---|
| `/new` | Choose the next root-run agent |
| `/preset` | Select the preset for the next root run and future new sessions; see [Agent presets](../extend/agents.md#agent-presets) |
| `/agent` | Choose the agent for the next run |
| `/model` | Choose the model for the next run, then its variant; nothing applies until the variant is chosen, and Esc in the variant list returns to the models; each row shows, dimmed after its name, the ID an agent document's `models` writes for it (the model's `provider/model` key, the variant's ID, an ACP agent's option value), once when the name is the ID; for an [ACP agent](../extend/acp-agents.md), which chooses its own model, it lists the values of the agent's model option, and the title's bracketed variant (or ctrl+t) cycles its effort option; a new session's draft offers what the agent last started a session with |
| `/options` | Change the options of the session's ACP agent, such as its model, mode, and effort: pick an option, then a value; listed while the agent offers options |
| `/connect` | Connect or update a managed provider |
| `/mcp` | Manage MCP servers; see [MCP servers](../extend/mcp.md) |
| `/runtime` (also `runtimes`) | Attach and detach the selected session's runtimes, or choose a new session's before its first prompt; see [Runtimes panel](#runtimes-panel) |
| `/background` (also `bg`) | List and stop the selected session's background calls; see [Background calls](#background-calls) |
| `/config` | Edit the daemon's `config.toml` and agent documents in your editor, create and delete agents, reload hand edits (`r` or *Reload from disk*), and restart the daemon (`R` or *Restart daemon*, highlighted while a part waits for a restart); see [config.toml](../engine/configuration.md#changes-while-the-daemon-runs) |
| `/restart` | Restart the daemon, when idle or now; see [Restarting the daemon](#restarting-the-daemon) |
| `/onboard` | Set up Cookie: connect a provider, choose the default and small models, and install the starter kit; see [Setting up](#setting-up) |
| `/memory` (also `memories`) | Browse the daemon's memories by layer, edit one in your editor, create and delete memories, and see invalid files with the reason; see [Memory](../sessions/memory.md#managing-memory) |
| `/cron` (also `crons`) | List, create, edit, enable and disable, run now, and delete the daemon's cron jobs; see [Scheduled prompts](#scheduled-prompts) |
| `/cron new` | Schedule a prompt in the session on screen: opens a new job's form for it; listed only while a session is open; see [Scheduled prompts](#scheduled-prompts) |
| `/plugins` (also `plugin`) | Show each plugin's status, tools, hooks, and last failure, and restart one with `r`; see [Plugins](../extend/plugins.md#status-and-restarts) |
| `/permissions` (also `perms`) | Edit session permission overrides; see [Permissions](../extend/agents.md#permissions) |
| `/sessions` (also `resume`, `load`, `continue`) | Choose a session, rename or delete one, or list every directory's; see [Session picker](#session-picker) |
| `/project` (also `projects`) | Choose the project of the selected session, or of a new session before its first prompt; see [Projects](#projects) |
| `/rename` (also `title`) | Rename the selected session: `/rename <title>` sets the title, `/rename --auto` goes back to the generated title, and `/rename` alone asks for the title in a field holding the current one; see [Renaming](#renaming) |
| `/skills` | Search user-invocable skills, pick one, then enter optional arguments; see [Skills](../extend/skills.md) |
| `/attach` (also `file`, `image`) | Ask for a file's path and attach it to the next message; listed only when the selected model accepts files; see [Attachments](#attachments) |
| `/usage` | Show selected-session and session-tree usage |
| `/cancel` | Cancel the active run |
| `/goal` | Prompt for an objective and set the root session's goal; see [Goal mode](../sessions/goals.md) |
| `/compact` | Prompt for an optional focus, then compact the selected idle session |
| `/events` | Pick the diagnostic event filter from a list |
| `/agent-panel show`, `/agent-panel hide`, `/agent-panel auto` | Pin the agent panel shown or hidden for the current root session, or return it to following delegated agents (the default) |
| `/quit` (also `q`, `exit`) | Exit the TUI |

In a read-only session owned by another cookie process, commands that write to
the session (`/permissions`, `/runtime`, `/skills`, `/attach`, `/cancel`,
`/goal`, `/compact`, `/rename`, `/cron new`, `/options`) are hidden.

In a session of an [ACP agent](../extend/acp-agents.md), the palette also
lists the slash commands the agent offers, after cookie's own and marked
`(agent)`. Choosing one that takes input asks for it; choosing one sends
`/<name> <input>` to the agent. The agent picker lists the agents runtimes
offer with where the session would run them (`claude [on local]`); one that
cannot run there is dimmed with why (`[needs lab attached]`,
`[offered by lab, local: attach one]`, `[on lab · unavailable]`) and cannot
be picked. The
agent's plan shows in the transcript as a checklist (`[x]` done, `[~]` in
progress, `[ ]` pending), its own tool calls name it after their title and
fill in their arguments as the agent sends them, and its notices show as
`Notice` blocks. A message steered into the agent's running turn stays in
the pending strip until the agent takes it, then shows as any user message;
for an agent that cannot be steered, the pending strip says `queued: this agent can't be steered`, and
the message waits for the agent's next turn.

The composer only ever sends messages: text beginning with `/` is sent as a
prompt like any other. To start a message with `/`, open the palette and type
`/` in its empty search; the palette closes and a literal `/` lands in the
composer. A `/` typed after other text, as in a path, is always literal.

## Restarting the daemon

While the daemon's configuration lists parts that apply only after a
restart, a banner above the conversation says so, such as
`⟳ Restart needed to apply: plugins, runtimes`, with a *Restart daemon*
button. The button, `/restart`, and `R` or *Restart daemon* in `/config`
open the same confirmation:

- **Restart when idle** (Enter or `i`, the default) waits until no turn is
  running in any session and every background call has reported; new turns
  are refused meanwhile.
- **Restart now** (`n`) interrupts the running turns and ends the background
  calls; the confirmation says how many turns there are.
- **Cancel** (Esc) keeps the daemon running and returns to the panel it
  opened over.

Once the daemon announces the restart, the banner shows
*Restarting when idle… (2 running turns, 1 background call)* until the
connection closes, then
the TUI reconnects as after any dropped connection. A daemon not started by
`cookie daemon`, including the engine of `cookie --standalone`, cannot
restart itself and says so. See
[Restarting the daemon](../daemon/persistent.md#restarting-the-daemon).

## Setting up

When the TUI connects to a daemon that is not set up (no provider connected,
or `default_model` or `small_model` not naming an available model), a notice
takes the bar above the composer: what is left, then **Set up** and **Not
now**. Set up opens the onboarding panel; Not now hides the notice on this
machine (in `~/.cookie-agent/tui-state.json`) until what the daemon misses
changes. `/onboard` opens the panel any time, also on a daemon that is set
up. Every client sees the same state: finishing in the web client or with
[`cookie onboard`](cli.md#cookie-onboard) hides the notice here too.

The panel shows the four steps as tabs, each done one with a check; ← and →
(or Tab) move between them, and a tab can be clicked:

1. **Provider**: the connected providers with their count of available
   models. Enter opens the [connect panel](#command-palette) (`/connect`);
   it returns to the panel when it closes.
2. **Default model**: Enter opens the model list, searchable and grouped by
   provider; choosing a model with variants opens its variants, the model's
   default variant highlighted.
3. **Small model**: the same list with nothing suggested; the variant list
   highlights the model's first listed variant, its cheapest. It may be the
   same model as the default.
4. **Starter kit**: what installing the kit does to each file (new, same as
   yours and skipped, or differs and written beside yours as `.new`); Up and
   Down select a file and `d` shows its diff. `i` turns installing on or off,
   and `o` takes the path of a downloaded `cookie-agent-defaults.tar.gz`
   instead of the release's kit.

`f` (or the **finish** button) sets everything up with `onboarding.run`; while
something is missing the panel says what instead. The summary lists the
settings and files written and offers a first session on `primary`. `n` is
Not now, and Esc closes the panel.

## Steering, queueing, and the pending strip

While a run is active, a message can reach it two ways:

- **Enter steers.** The message enters a durable pending lane the model has not
  seen yet. At the next tool or completion boundary, pending messages join the
  running conversation in submission order as separate user messages.
- **Alt-Enter queues.** The message waits for the run to finish, then starts a
  new run of its own with the selection current when it was queued. Queued
  messages run one at a time, oldest first.

On an idle session both keys simply start a run. The daemon decides between
starting, steering, and queueing atomically, so a run that another client starts
or that ends while you type only changes which of those happens.

The `Pending` strip shows both lanes: `⏳` rows are steered messages and `⏭`
rows are queued ones. It shows up to three rows and folds additional entries
into `+N more`. Several clients can share one daemon (see
[Architecture](../architecture.md)); a message another client sent is labelled
with that client, such as `· tui-0192f3`. Click a row to take that message
back into your composer for editing, another client's too (a daemon before
client protocol 1.31.0 lets only the client that sent it), or press Up in an
empty composer to take back your newest one.

Queued messages run only after the current run completes normally. If the run
fails or is cancelled, or the daemon restarts mid-run, the queue is cleared, and
each client gets its own unsent messages, steered or queued, back in its
composer, as it does with steered messages a run never reached. This happens
only for a run that ends while the TUI is connected; replayed history never
restores text, so opening or reattaching to an old session leaves the composer
alone.

## Selection and clipboard

Drag in the conversation or composer to select text. Ctrl-C copies the selected
text and clears the selection; Ctrl-X cuts only a composer selection. With no
selection, Ctrl-C cancels the active run, stopping the tool calls it is running
at once (see [Cancelling a run](../reference/tools.md#cancelling-a-run)); when
there is nothing to interrupt, pressing it twice within two seconds quits. Esc clears a selection before it can
count toward the double-Esc run-cancel gesture.

Conversation copy removes role gutters, borders, and code-fence chrome, so code
is copied as raw source. Clipboard writes use OSC 52 and work over SSH when the
terminal emulator supports OSC 52.

## Session picker

`/sessions` lists the root sessions of the directory the TUI started in,
newest first under day headings; type to filter them by title, and press
Enter or click a row to open one.

- **Ctrl+A**, or the **All directories** button, lists every directory's
  sessions instead, each row naming its directory; press it again (the
  button now reads **This directory**) to go back. In the search field,
  Home still moves to the start of the text.
- **Ctrl+P**, or the project button beside it, filters by project: all
  projects, then each project in name order, then the sessions in no
  project, then all again. The picker's title names the project shown.
  Each row names its project, with its emoji, before its agent.
- A session a [cron job](#scheduled-prompts) started leads with a clock,
  🕒, before its title.
- **Ctrl+R**, or the **Rename** button, renames the highlighted session:
  a field holding its title opens over the search. See
  [Renaming](#renaming).
- **Ctrl+D**, **Delete** while the list has focus, or the **Delete**
  button asks to delete the highlighted session. The confirmation names
  the session and says how many subagent sessions go with it; **y** or
  Enter (or **Confirm**) deletes it, and **n** or Esc (or **Cancel**)
  keeps it. While it is open, the rows behind it do not respond to clicks.

Deleting removes the whole tree from disk: the session, its subagents, and
their messages, tool output, and attachments. It cannot be undone. The
daemon refuses while the session or one of its subagents is running, while
another cookie process owns it, and when it changed since the picker showed
it; the status line then says why. A daemon older than client protocol 1.5
cannot delete sessions, and the TUI says so instead of asking. When the
session you are viewing is deleted, from this TUI or another client, the
TUI leaves it for a new session, as it does when it starts without one. See
[Delete](../sessions/index.md#delete).

## Renaming

A session's title is generated from its first messages (see
[Session Title](../engine/session_title.md)); a title you set replaces it.

- `/rename Fix the parser` sets the selected session's title.
- `/rename --auto` drops your title for the generated one again.
- `/rename` alone, and **Ctrl+R** in the session picker, open a field
  holding the current title. Enter saves it; Enter on an emptied field
  goes back to the generated title, and Esc keeps the title as it was.

The title can be up to 512 bytes, without line breaks; a longer one stays
in the field with the reason in the status line. Every client sees the new
title at once. A session you have not sent a first message to yet has
nothing to rename.

## Projects

While no run is active, the bottom bar starts with the session's
[project](../concepts/projects.md): its emoji and name, or `no project`.
That covers a session you have open and a new session before its first
prompt. An emoji counts as two columns. Click the name, or choose
`/project`, for the project picker: the projects, **No project**, and
**New project…**, which asks for a name and creates the project.

- **An open session** moves to the project you pick with
  `session.project.set`, even while it runs; its runtimes and agent stay.
  A subagent belongs to its root session's project, so select the root to
  move it.
- **A new session** is created in the project you pick. A project whose
  defaults name runtimes gives them to the draft, and the bottom bar shows
  their workspaces before you send anything; otherwise the draft keeps the
  usual runtimes. Its default agent and preset become the draft's, with
  the agent's own model; an agent that is gone, or cannot start a session,
  leaves the usual one, and the status line says so.

A new session belongs to no project unless you pick one; `cookie --project
<name>` starts in one, and nothing is ever picked from the directory the TUI
started in. A daemon older than client protocol 1.8 keeps no projects, so
the bar names none and `/project` is not offered.

## User-message menu

Click a past `You` message to open its action menu. Use Up/Down and Enter, Esc
to close, or the `c`, `r`, and `f` accelerators.

- **Copy** writes the original message text to the clipboard.
- **Revert** asks for confirmation, removes that message and everything after
  it from the session (including subagents it started), and restores the
  message text to the composer. If the session gained events since this view
  last showed it, for example a turn another client started, the revert is
  refused so nothing unseen is deleted; review the new turns and revert again.
- **Fork** creates and selects an independent session whose copied prefix
  includes that message.

Assistant and tool rows keep their normal expand/collapse behavior and do not
open the message menu.

## Transcript details

The conversation follows new output while it rests at the bottom. Scrolling up
(the wheel, Page Up, the scrollbar) stops following at once, and the title says
`↑ scrolled · PgDn: bottom` until you scroll back to the bottom or send a
message (steered or queued too): output that streams in, or a block that folds
below, never moves a scrolled-up view.

System prompts, AGENTS.md context, loaded skills, context compaction
checkpoints, plugin-injected messages, and committed media parts appear
collapsed by default. An AGENTS.md row appears at a run whose loaded context
differs from the previous run's: the first load, a change, or loading again
after a run without AGENTS.md. A run that reloads identical files adds no row.
Expanded,
it shows each file's source and content. A skill row shows the skill's source
path, arguments, and rendered body. An AGENTS.md file skipped for size appears
as a warning row. A session's system-prompt
row remains hidden until its first run starts, then identifies the agent from
the latest run snapshot. If the next-run draft uses another agent, the row also
shows `next: <agent>`. Click one of these rows to show or hide its text or
metadata. Expansion state is kept separately for each session. Expanded system
prompts, skill bodies, and each AGENTS.md file are display-bounded to 256 lines
or 32 KiB of sanitized text;
compaction, plugin-message, and media bodies are bounded to 64 lines or 8 KiB.
Oversized content ends with a truncated-lines indicator while the complete data
remains in session state.

Expanding a tool row opens its title block: the header, followed by the rest of
the call's title and primary argument when the collapsed header had to shorten
them. The whole title block is one click target. A row's icon comes from the
tool's hint (shell, file read, file edit, fetch, search, delegation, and so on;
any other tool gets the hammer), never from its name, so a plugin or MCP tool
shows the same way a built-in one does when it describes itself the same way.
The header shows the call's title, and its primary argument after it unless
the title already names it. While a call runs, its latest progress message
follows the title.

Expanded tools use ordinary text colours and end with a blank row: a grey
title block over a lighter grey panel. The panel shows the call's argument
blocks (a command, a URL) under a muted `arguments` label, leaving out what
the header already says: a block whose text is the header's primary argument
or the text its title quotes, and a key/value row that only repeats the
primary argument. A block that says more, such as a long command the header
shortened, stays. Then comes what the call shows, drawn by the kind of each
block rather than by which tool sent it:

- **Diffs** name how the file changed (`created`, `modified`, `deleted`, or
  `renamed`), its path, and the lines added and removed, then each hunk with
  real file line numbers and `+`/`-` markers in a gutter, added and removed
  rows on the theme's diff tints, highlighted by language. A diff too large to
  carry its lines shows the counts alone.
- **Terminal output** draws the program's SGR colours (fitted to the
  terminal's colour depth, and none under `NO_COLOR`), stderr in the warning
  tone unless the program coloured it, and the exit code below, red when it
  is not zero. It grows while the command runs.
- **File excerpts** and **code** show file line numbers and are highlighted
  by language; **JSON** is pretty-printed; **markdown** renders like the
  assistant's text; **tables** and **key/value** lists align their columns.
- **Media** shows a chip, `📎 name · type · size`; a **link** its label and
  address; a **session reference** a row you can click to open that session.

A call without argument blocks shows its raw arguments instead, and only when
the diagnostic filter is `info` or `debug` (see `/events`). Output beyond the
row budget ends with `… N more lines (click to expand)`. A block the tool had
to cut says how much is missing, and when it names its complete content (a
long command's stdout, a large diff), `click to load the full output` reads up
to 1 MiB of it from the daemon and shows it in place of the cut rows; click the
row under it to put it away again.

OpenAI Responses turns can retain opaque continuation witnesses for provider
metadata such as message phase, annotations, and logprobs. Keep these parts
alongside native replay data when preserving or restoring history. Internal
agents validate the transport parts while using the text for summaries,
approval decisions, and titles. Older saved turns may lack witnesses that were
not captured at the time; historical events are not rewritten to invent them.

## Error details

When the daemon refuses a request, the TUI's status line and the CLI show its
message, a sentence a person can act on, and then its hint, what to do next.
Every refusal also has a stable code (see [Errors](../reference/errors.md)).
Technical detail, such as the upstream cause or a path, is shown on request:
the TUI adds an error row to the session you are viewing whose first line is
the error and whose collapsed rest, behind `▸`, is the detail; the CLI prints
it with `--error-details`, an option of every command.

Run failures show a concise headline followed by available model/provider identity,
HTTP status, provider code, request ID, and response body. The same diagnostics
are retained in session events and shown on replay. Model fallback and internal
agent failures also include the available response diagnostics. Warning and
error rows lead with 🚨 or ❌ instead of a badge row. A multiline diagnostic row
shows only its first line, behind a `▸`; click it to show the rest, such as a
response body. A warning or error identical to the row before it counts on that
row (`repeated 6×`) instead of adding a copy, so a retry loop hitting the
same provider error stays one row; this includes subagent rows shown in an
ancestor's transcript. For a failed tool, expand the tool row for its display
output.
Failed tools show their failure reason before their bounded inline output, even
when they supplied separate display text. Tools with usable output do not add a
separate TUI error row; output-less failures still do. A bash command's non-zero
exit code is normal result data, not a tool failure. Genuine execution failures
such as timeouts remain failures. Bounded diagnostics prioritize exit status, source context, and
diagnostic streams such as stderr. Each stream gets space for its beginning and
end so loud stdout cannot hide a later failure cause. MCP connection failures
appear in `/mcp`; plugin initialization and RPC
failures preserve operation context and selected cause fields. Configuration
errors identify invalid settings and, for file/parse failures, available paths
and locations without printing configuration source lines.
Transport failures retain send/receive context for pending calls and appear as
connection diagnostics in the TUI. Rejected WebSocket handshakes show a bounded,
response body with its content preserved when supplied, without printing response headers.

Response bodies are capped at 4,096 UTF-8 bytes with a truncation marker. JSON is
formatted for reading; plaintext and HTML gateway errors remain plain text.
Terminal controls and Unicode format controls are stripped or replaced with spaces;
newlines and tabs are preserved. The application does not redact credential-like
text, sensitive field names, header records within a response body, or echoed
secret values. It does not add request headers or credential-store dumps to
diagnostics. MCP OAuth failures likewise preserve bounded response bodies and
authorization error descriptions. Retained diagnostics have the same access and
persistence rules as other session events.

The pinned model SDK preserves sanitized HTTP error-body text for OpenAI,
OpenAI-compatible, Anthropic, Google, Google Vertex, Bedrock, Azure, and Cohere.
SDK body retention is bounded to 64 KiB; the application further sanitizes and
bounds displayed and persisted body diagnostics to 4 KiB. The pinned SDK still
redacts some provider response fields before the application receives them; the
application preserves the text it receives and cannot recover replaced values.
A body is available only when the provider
returned one: validation, connection, or cancellation errors may have no HTTP
response body. Missing detail is not reconstructed or fetched with another API
request.

`cookie run` reports failures on its own streams; see
[Failure diagnostics](cli.md#failure-diagnostics).

Internal tracing logs are off by default. Setting `RUST_LOG` (for example
`RUST_LOG=debug`, using `tracing-subscriber` filter syntax) writes them to
stderr, never stdout, so they cannot mix into `cookie run` output or the daemon
ready line. The TUI draws on the same terminal, so redirect stderr to a file
when logging a TUI session: `RUST_LOG=debug cookie 2>cookie.log`.

## Live tool output

Expanded command rows show sanitized stdout and stderr while the command runs,
with the call's latest progress message above them. The live display is capped
at 64 KiB. On completion, failure, cancellation, or interruption, the same row
swaps to the call's result display; a call that ended without one keeps what it
showed while running. Reopening a session renders only that committed
content. Assistant text follows the same rule: streamed partials are replaced
by the committed turn.

## Scheduled prompts

`/cron` opens the daemon's [cron jobs](../sessions/scheduling.md): prompts it
fires on a schedule, into a session or into a new session per fire. Each row
shows the job's name (or the start of its prompt), its schedule read plainly
(`every day at 03:00`, `every Monday to Friday at 09:30, once`, `once at
2026-10-23 03:00`), how long until its next fire, its target (`this
session`, `session <title>`, `subagent <title>`, or `new session · <agent>`),
whether it is enabled or why not (`finished`, `failed 3 times in a row`,
`disabled by you`), and what its last fire did. Under the list, **Details**
gives the selected job's schedule, run count, delivery, template, and
prompt, and **History** its fires, newest first, each with its outcome and
how its run ended. The title says when the daemon's firing is off (`[cron]
enabled = false`).

Up and Down select a job, or click its row. Each action is a key and a
button below the panel:

| Key | Action |
|---|---|
| `e` or Enter | Edit the selected job |
| `n` | Create a job: into the session on screen, or, with none open, into a new session per fire |
| `t` | Enable or disable the selected job |
| `r` | Run it now; the status line says what the fire did |
| `d` | Delete it, after `y` confirms; its run sessions stay |
| Esc | Cancel a confirmation or a refused form, else close the panel |

`/cron new` in the command palette opens a new job's form for the session on
screen directly: a root session is targeted as a session, a subagent's as a
subagent.

A job is created and edited as a short form in `$VISUAL` or `$EDITOR`, as
`/memory` and `/config` edit theirs: `key: value` lines, then the prompt
after a `---` line. Lines starting with `#` explain the fields and are
ignored.

```text
name: Nightly dependency check
schedule: 0 3 * * *
max_runs: unlimited
target: new
delivery: queue
agent: builder
model: deepseek/deepseek-flash[high]
permission_mode: ask
---
Check for outdated dependencies and open a summary.
```

- `schedule` is five cron fields in the daemon host's local time, a delay
  (`in 20m`), or a time (`at 2026-10-23T03:00:00Z`).
- `max_runs` applies to cron schedules: a number from 1, or `unlimited`;
  left empty, the job fires once.
- `target: session` fires into the session the form was opened for (or the
  job's own session when editing); `target: new` starts a session per fire.
- `delivery` (`queue` or `steer`) applies to a session target; `agent`,
  `model`, and `permission_mode` to a new-session target. `model` pins
  `provider/model` or `provider/model[variant]`; empty, the agent's own
  chain runs. `permission_mode` empty means `auto_approve_n`.

A new-session job's template starts from the session on screen: its agent
and preset (the agent you picked for its next run), its project, and the
directory the TUI started in; with no session open, from the agent a new
session would get. Saving unchanged text creates or changes nothing, and an
edit sends only the fields it changed. A form the TUI cannot read, or that
the daemon refuses, stays open with the reason: `e` edits it again as you
left it, Esc drops it. The panel refreshes when any client or agent changes a
job. A daemon older than client protocol 1.15 keeps no jobs: `/cron` says so,
and `/cron new` is not offered. See
[Scheduling prompts](../sessions/scheduling.md) for how jobs fire.

## Background calls

A running call of a tool that supports the background (such as `bash` or
`delegate_subagent`) shows
a `[background]` button at the right end of its title row. Clicking it moves
the call to the background once the tool can be detached: the call's row ends
with the result the model got (`User manually moved this call to the
background …`) and `· background`, and the run goes on while the call keeps
running. The model can also start a call in the background itself.

While the selected session has background calls, a bar above the composer
lists them; clicking it, or `/background`, opens a panel with one row per call
(job, tool, its argument, how long it has run, and who moved it). Up/Down
select a row, `s` or a row's `[stop]` stops that call, and Esc closes the
panel. A stopped call still reports to the model, as stopped by you. A
subagent working on its own, outside a call that waits for it, is listed under
its handle; stopping it cancels the turn it is running, and the subagent
reports that turn as cancelled. Cancelling the session's run (Esc) never stops
its subagents. See
[Background calls](../reference/tools.md#background-calls) for what the model
sees.

## Approvals

An approval docks above the status bar in place of the composer, leaving the
conversation visible above it. It shows the call it gates the way you need to
judge it: its icon and title, its argument blocks (such as a command), and its
preview (such as an edit's or write's diff as of preparing the call), drawn as
the transcript draws them; a call that describes neither shows the request's
resources. Below that, `why` says what asked (your permission
rule and its source, or the model's own reason), `reviewer` gives the approval
reviewer's reason for passing the call to you ("The reviewer asks: …", or that
its review failed), and a last row says what the focused decision does. A
call the reviewer allowed or denied without you names its reason in its info
row. A countdown appears in the top border when the request
expires (the window opens when the call reaches you), and `d` (or clicking `details`) expands the full prepared-operation
identity: ids, fingerprints, and the policy trace.

The decisions follow the request's constraints: allow once, allow for the
session tree, reject, or cancel. Left/Right or Tab move focus between them and
Enter answers with the focused one; each button also names its own key: `y`
allows once, `a` allows for the session tree, `n` rejects, and Esc cancels when
the request is cancellable. Letter keys are case-insensitive and do nothing for
a decision the request does not offer. Clicking a button answers with it.

`e` opens a note for a rejection: type why, or what the agent should do
instead, and press Enter to reject with it; the agent receives the note with
the refusal. Esc closes the note without answering, and an empty note sends a
plain rejection. Long panels scroll with arrows, Page Up/Page Down, Home, and
End, or the mouse wheel.

While an approval is on top it owns the keyboard: other typing and pastes are
dropped rather than landing in the hidden composer (a paste goes into an open
note), and Ctrl-C still cancels the run. Enter, the letter keys, and `e` ignore
presses for 400 ms after a request appears, so keystrokes already on their way
to the composer cannot answer it; the panel says so beside its buttons, and a
second press once it has settled goes through.

The permission mode appears in the bottom bar. Click it to cycle
`auto-approve -> auto-n -> auto-y -> ask -> yolo`; the mode applies to
subsequent approvals throughout the selected session tree, including delegated
descendants. While a new-session draft is open, the mode shown and cycled is the
draft's own, applied when its first prompt creates the session. Hard policy denies and doom-loop rejection still win in every
mode. See [Permissions](../extend/agents.md#permissions).

When pricing is available, the bottom bar also shows the selected session's
estimated cost between the permission mode and context usage. Click the cost to
open the `/usage` dashboard. Unpriced sessions omit the segment.

The left of the bottom bar shows the session's project while no run is
active (see [Projects](#projects)), then the runtimes the selected session,
or a new session before its first prompt, attaches besides `global`. With one, it
shows the runtime and its workspace, for example `labpc · ~/api`; with
several, their attachment names, for example `local_web, labpc_api`. It
shows `global` when that is all the session attaches, and `no runtimes` for
a session with none.

## Questions

An [ACP agent's question](../extend/acp-agents.md#questions-from-the-agent)
docks in the same place, titled `<agent> asks · waiting on you`, while no
approval is on top. A form lists its fields: radios for a choice, boxes for
a multiple choice and for yes or no, and a line for text and numbers, each
with its title, what it takes (`required`, bounds, a format), and its
description. Tab and Shift-Tab move between the fields and the buttons, the
arrows between a field's choices (past the last, on to the next field),
Space picks a choice or flips a box, and Enter picks and moves on, or
presses the focused button. **Submit** checks the form and, when a field
does not fit, names it under the field and moves there; **Decline** and
**Cancel** (Esc) answer at once. A page to open shows its host, the whole
URL, and any warning; focus starts on **Copy link** (`c`), and **Done**
answers once you have finished there. The panel owns the keyboard and
pastes (a paste goes into the focused text field), the wheel scrolls it
when it is long, Ctrl-C cancels the run, and the answers wait out the same
400 ms as an approval's.

## Notifications on other devices

The daemon can push a notification to a phone or a browser when a session
finishes its turn or an approval waits (turn them on in the web client; see
[Notifications](web.md#notifications)). It pushes nothing for the session
the TUI shows while its terminal has focus and you used it in the last two
minutes. The TUI asks the terminal to report focus; in a terminal that does
not (or inside a multiplexer that does not pass it on), the TUI never says
you are there, so the push still comes.

## Runtimes panel

`/runtime` (titled *Attached runtimes*) lists the selected session's
[runtime attachments](../specs/runtimes.md#5-session-attachments): each
attachment's name, runtime and capability (`labpc/coding`), workspace, and
instance status (`starting`, `ready`, `reconnecting`, `failed`, `stopped`, or
`not running`), and the MCP servers its instance hosts counted by state
(`MCP 1 connected, 1 failed`). The details pane shows the selected
attachment's kind, what its capability offers (the runtime's description of
it), platform, for a failed instance the reason, and each MCP server the
instance hosts with its state, tool count, and why it failed or what it
waits for (from `mcp.server.list`), its runtime's runtime-scoped servers
among them marked `runtime-wide`. The machines view counts each runtime's
MCP servers the same way and lists them in the selected runtime's details,
including, before any workspace of it is open, its runtime-scoped servers
and its workspace-scoped ones as `starts with a workspace`.
The panel refreshes on every runtime change and while it is open.

Press `a` to attach: the panel lists every capability of every runtime
(`labpc/coding`, `devbox/computer_use`), with its description and whether
the runtime is connected; the details below the list repeat the selected
capability's description. Type to filter by runtime, capability, kind, or
description, choose with the arrow keys, and press Enter. A capability that
takes no workspace attaches at once; one that takes a workspace asks for its
path next (Enter attaches, Esc returns to the list). A runtime that has not
connected yet is one entry: attaching it connects it and takes its only
capability, its workspace is optional, and the details show why its last
connection failed. The path is passed to the runtime as typed and resolved on its
machine (see [Runtimes](../runtimes/index.md)). Press `d` to detach the selected
attachment, and `r` to retry its instance now, for example after fixing why it
failed (`runtimes.reconnect`). The daemon refuses both
while the session has an active run, and refuses attaching an instance the
session already has; the refusal appears in the status line. Attaching starts
the instance first and records nothing when it fails to start.

Before a new session's first prompt (at startup, or after `/new`), the panel
edits the runtimes that session will attach, starting from the defaults: `global`, and the `coding` capability at
the TUI's directory (`local`'s, or this machine's when the daemon is remote).
Each instance opens as soon as it joins the list, so its status and any
failure show before the session exists, and an instance the list already
holds is refused. `a`, `d`, and `r` edit the list and contact no session;
the first prompt creates the session with exactly the listed runtimes, and
fails without a session when one of them cannot start.

When an MCP server or runtime instance a session uses fails to connect, or
drops and does not reconnect, the TUI adds an error row to each affected
session naming the server or runtime and the reason (see
[MCP connection lifecycle](../extend/mcp.md#connection-lifecycle)).

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

See [Usage and cost](../sessions/usage.md) for recording and pricing semantics.

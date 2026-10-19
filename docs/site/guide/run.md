# Run

## Terminal UI

Start in the workspace where the agent should operate:

```sh
cookie
```

This starts the local engine and terminal client together. Configure the
[theme](../tui/theme.md) and [diagnostic filter](../tui/minimum_event_level.md)
in the independent client configuration.
When at least one model is available, select an agent and model if needed, type
a request in the composer, and press Enter. If no authored root agent is
runnable, the engine supplies the built-in `default` coding agent.

Useful first commands, from the [command palette](#command-palette), are
`/sessions`, `/new`, `/model`, `/compact`, and `/cancel`. [Agent](agents.md) covers authored prompts and permissions.

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


## Separate server and client


The daemon binds to `127.0.0.1:7419` by default:

```sh
cookie daemon
```

Its first stdout line is the machine-readable ready frame; everything the
daemon prints afterward is human logging. A supervisor writes the daemon's
stdout to a pipe, reads exactly that one line, and ignores the rest:

```text
daemon-ready {"url":"ws://127.0.0.1:7419/ws","token":"<43-char base64url>"}
```

The token is `[server] token` in `config.toml`; the daemon writes a fresh one
there at its first start when there is none, and keeps it across restarts.
Treat it, and the ready line, as a secret: anyone who reads either can
connect to the daemon.
Pass `--port 0` to let the OS pick an ephemeral port; the ready line then
carries the real port, and clients must use that URL rather than guessing 7419.

Attach from another terminal on the same machine:

```sh
cookie attach
```

`attach`, `connect`, `disconnect`, and `mcp` find the daemon in this
machine's `config.toml`: `[server] url`, else the local daemon the same
table describes (`ws://127.0.0.1:<port>/ws`), and `[server] token`. Flags
override either: `--url` (any host, `ws://` or `wss://` with the exact `/ws`
path) and `--token` (or `COOKIE_DAEMON_TOKEN`), so a local `cookie attach`
can still reach a daemon elsewhere:

```sh
cookie attach --url wss://laptop.example:7419/ws --token '<43-char base64url>'
```

A daemon on another machine listens with `--listen`; see
[Server](../engine/server.md).

The daemon, `cookie runtime serve`, and the TUI stop cleanly on a stop
signal: `SIGINT`, `SIGTERM` (`kill`, systemd), or `SIGHUP` (a closed
terminal); on Windows Ctrl-C, Ctrl-Break, a closed console, logoff, or
shutdown. The daemon stops listening and shuts down its engine and the
runtimes it started; a runtime cancels its calls and stops its MCP servers; the
TUI restores the terminal. A second signal during that shutdown exits the
daemon or runtime at once.

### Running as a service

Both the daemon and a runtime that dials in run until stopped, so each can
be a systemd user service. On the daemon's machine,
`~/.config/systemd/user/cookie-daemon.service`:

```ini
[Unit]
Description=cookie agent daemon

[Service]
ExecStart=%h/.cargo/bin/cookie daemon --listen 0.0.0.0:7419
Restart=on-failure

[Install]
WantedBy=default.target
```

On a machine whose `config.toml` names the daemon (`[server] url` and
`token`; see [Runtimes that dial in](runtimes.md#runtimes-that-dial-in)),
`~/.config/systemd/user/cookie-runtime.service`:

```ini
[Unit]
Description=cookie agent runtime

[Service]
ExecStart=%h/.cargo/bin/cookie runtime serve --daemon
Restart=on-failure

[Install]
WantedBy=default.target
```

Enable each with `systemctl --user enable --now <name>`, and run
`loginctl enable-linger` so they run without a login session. Either may
start first: the runtime retries until the daemon answers. The journal keeps
the daemon's ready line, token included, readable by whoever can read your
user journal. A daemon running as a service has
no `SSH_AUTH_SOCK` from your shell, so an ssh `[runtimes.<id>]` template
needs a key file in its arguments (`-i`) or an agent socket set with the
unit's `Environment=`.

One daemon serves sessions of any directory. The TUI, whether attached or
running the daemon itself, creates its sessions for the directory it was
started in: they work there with `local`'s `coding` capability, and its
session list shows that directory's sessions. Sessions stay in the store of
the directory the daemon started in.

When the daemon runs on another machine (its handshake reports another
machine ID), `cookie attach` also offers this machine to it: it dials in a
runtime named `client_<host name>` that serves the TUI's directory only, and
the TUI's new sessions use its `coding` capability there instead of the
daemon's `local`. The runtime lives as long as the TUI; sessions keep its
attachment and find it again when the TUI attaches again from this machine.
Configured plugins and eager MCP servers, which come only from the user
`config.toml`, start during engine initialization, before a model tool call is
approved. Repository `AGENTS.md` files and project skills still enter model
context, so open unfamiliar workspaces with care.
Allowed shell commands run without a filesystem sandbox; see the
[security contract](security.md#process-boundary).

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
other way, such as `kill -STOP` or an editor opened from `/config` being
suspended along with it, takes its terminal back and redraws when it is
continued.

### Attachments

When the selected model accepts files (images, PDFs, audio, or video; see
[Attaching media](media.md)), `/attach` asks for the path of a file and
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
typed arguments. Esc in a step returns to the command list with the search
intact.

| Command | Action |
|---|---|
| `/new` | Choose the next root-run agent |
| `/preset` | Select the preset for the next root run and future new sessions; see [Agent presets](agents.md#agent-presets) |
| `/agent` | Choose the agent for the next run |
| `/model` | Choose the model for the next run, then its variant; nothing applies until the variant is chosen, and Esc in the variant list returns to the models |
| `/connect` | Connect or update a managed provider |
| `/mcp` | Manage MCP servers; see [MCP servers](mcp.md) |
| `/runtime` (also `runtimes`) | Attach and detach the selected session's runtimes, or choose a new session's before its first prompt; see [Runtimes panel](#runtimes-panel) |
| `/background` (also `bg`) | List and stop the selected session's background calls; see [Background calls](#background-calls) |
| `/config` | Edit the daemon's `config.toml` and agent documents in your editor, create and delete agents, and reload hand edits; see [config.toml](configuration.md#changes-while-the-daemon-runs) |
| `/plugins` (also `plugin`) | Show each plugin's status, tools, hooks, and last failure, and restart one with `r`; see [Plugins](plugins.md#status-and-restarts) |
| `/permissions` (also `perms`) | Edit session permission overrides; see [Permissions](agents.md#permissions) |
| `/sessions` (also `resume`, `load`, `continue`) | Choose a session |
| `/skills` | Search user-invocable skills, pick one, then enter optional arguments; see [Skills](skills.md) |
| `/attach` (also `file`, `image`) | Ask for a file's path and attach it to the next message; listed only when the selected model accepts files; see [Attachments](#attachments) |
| `/usage` | Show selected-session and session-tree usage |
| `/cancel` | Cancel the active run |
| `/goal` | Prompt for an objective and set the root session's goal; see [Goal mode](goals.md) |
| `/compact` | Prompt for an optional focus, then compact the selected idle session |
| `/events` | Pick the diagnostic event filter from a list |
| `/agent-panel show`, `/agent-panel hide`, `/agent-panel auto` | Pin the agent panel shown or hidden for the current root session, or return it to following delegated agents (the default) |
| `/quit` (also `q`, `exit`) | Exit the TUI |

In a read-only session owned by another cookie process, commands that write to
the session (`/permissions`, `/runtime`, `/skills`, `/attach`, `/cancel`,
`/goal`, `/compact`) are hidden.

The composer only ever sends messages: text beginning with `/` is sent as a
prompt like any other. To start a message with `/`, open the palette and type
`/` in its empty search; the palette closes and a literal `/` lands in the
composer. A `/` typed after other text, as in a path, is always literal.

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
with that client, such as `· tui-0192f3`. Click one of your own rows to take
that message back into the composer for editing, or press Up in an empty
composer to take back your newest one. Only the client that sent a message can
take it back.

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
selection, Ctrl-C cancels the active run; when there is nothing to interrupt,
pressing it twice within two seconds quits. Esc clears a selection before it can
count toward the double-Esc run-cancel gesture.

Conversation copy removes role gutters, borders, and code-fence chrome, so code
is copied as raw source. Clipboard writes use OSC 52 and work over SSH when the
terminal emulator supports OSC 52.

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
blocks (a command, a path) under a muted `arguments` label, then what the
call shows, drawn by the kind of each block rather than by which tool sent it:

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

`cookie run` writes failure diagnostics to stderr even with `--output none` and
without `--verbose`. JSON output retains diagnostic event fields and adds an
`error` field to the final summary (null for success). A failed tool or fallback
can produce stderr diagnostics even if the overall run subsequently succeeds.
This includes internal-agent fallback failures and session-scoped plugin notices
for the selected session. Events from unrelated sessions/runs remain excluded.
Exit codes retain their existing meanings.

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
rule and its source, or the model's own reason), and a second row says what the
focused decision does. A countdown appears in the top border when the request
expires, and `d` (or clicking `details`) expands the full prepared-operation
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
mode. See [Permissions](agents.md#permissions).

When pricing is available, the bottom bar also shows the selected session's
estimated cost between the permission mode and context usage. Click the cost to
open the `/usage` dashboard. Unpriced sessions omit the segment.

The left of the bottom bar shows the runtimes the selected session, or a new
session before its first prompt, attaches besides `global`. With one, it
shows the runtime and its workspace, for example `labpc · ~/api`; with
several, their attachment names, for example `local_web, labpc_api`. It
shows `global` when that is all the session attaches, and `no runtimes` for
a session with none.

## Runtimes panel

`/runtime` (titled *Attached runtimes*) lists the selected session's
[runtime attachments](../specs/runtimes.md#5-session-attachments): each
attachment's name, runtime and capability (`labpc/coding`), workspace, and
instance status (`starting`, `ready`, `reconnecting`, `failed`, `stopped`, or
`not running`). The details pane shows the selected attachment's kind,
what its capability offers (the runtime's description of it), platform, and,
for a failed instance, the reason. The panel refreshes on every runtime change and while it
is open.

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
machine (see [Runtimes](runtimes.md)). Press `d` to detach the selected
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
[MCP connection lifecycle](mcp.md#connection-lifecycle)).

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

## Headless runs

`cookie run` executes one prompt through the local engine without starting the
TUI, daemon, or an in-process protocol server. It is intended for CI and scripts.

### Prompt input

Provide exactly one prompt source:

```console
cookie run "Review this workspace"
cookie run -p "Review this workspace"
cookie run -f request.txt
printf '%s\n' 'Review this workspace' | cookie run -
printf '%s\n' 'Review this workspace' | cookie run -p -
```

The positional prompt, `-p/--prompt`, and `-f/--prompt-file` conflict with each
other. `-` reads standard input for any of them.

### Selection and limits

The default agent is the root-runnable `primary` agent, or the first
root-runnable agent. The default model is its first live fallback, including a
valid variant; if none is live, the first available model and its default
variant are used. Select an agent preset with `--preset`; override the effective
selection with `-a/--agent`, `-m/--model`, and `--variant`. Use `--variant base`
to select no named variant. Every override is validated against the current
coherent runtime before a session starts.

`--resume-session <id>` continues an existing session and defaults to its
creation preset. Supplying `--preset` selects a different preset for that run
without rewriting the session's creation selection. See
[Agent presets](agents.md#agent-presets) for resolution and persistence details.
`--data-dir <path>` selects the session and artifact store. `--max-turns` and
`--timeout` are positive guards and default to 100 root model turns and 600
seconds. Reaching either guard cancels the run and waits for its terminal event.
A stop signal (`SIGINT`, `SIGTERM`, or `SIGHUP`; on Windows Ctrl-C,
Ctrl-Break, a closed console, logoff, or shutdown) follows the same
cancellation path.

### Permissions

`--permission-mode` accepts `auto-approve` (`auto_approve`), `auto-approve-n`
(`auto_approve_n`), `auto-approve-y` (`auto_approve_y`), `ask`, or `yolo`.
The selected mode applies to the whole runtime session tree, including delegated
descendants.
Headless runs never wait for approval input. In `auto-approve-n`, a classifier
escalation is rejected and cancels the root run with exit code `3`. In
`auto-approve-y`, an escalation is approved once and the run continues
automatically. Other escalations anywhere in the session tree are rejected;
the runner cancels the root run and waits for the matching terminal event.

`--allowed-tools` may be repeated or comma-delimited and accepts `read`,
`write`, `bash`, `delegate`, `mcp`, `plugin`, `plugin:<name>`, and
`skill:<name>` — each entry adds an `allow` overlay rule (resource `*`, or the
given name). It does not deny omitted actions or replace existing agent policy.
`webfetch` is not accepted; grant web access in the agent document.

### Skills

`--skill <name>` loads a user-invocable skill before the prompt run.
`--skill-args <text>` supplies its raw arguments and requires `--skill`. Skill
permission is evaluated before injection; use `--allowed-tools skill:<name>` to
grant it explicitly in unattended runs. The load appends the same durable event
used by interactive and model invocation.

### Output

Select `text`, `json`, or `none` with `-o/--output`. `--json` is an alias for
`--output json` and conflicts with an explicit `--output`. `--output-file`
redirects text or JSON output to a file and cannot be combined with
`--output none`.

Text mode writes only the terminal `final_text` to standard output. It does not
stream model deltas. With `--verbose`, ANSI-free progress lines are written to
standard error, including one for each finished tool call: `cookie run: tool`
and the call's title (with `(failed)`, `(cancelled)`, or `(interrupted)` when
it did not complete), then the first eight lines of its display as plain text,
indented, and a count of the rest. `--output none` prints the same lines. Failures and model fallback diagnostics are written to standard
error regardless of verbosity, including recovered failures in a successful run.

JSON mode writes JSON Lines. Records use these stable `type` tags:

- `event`: one accepted, ordered event for the active run.
- `tool_output`: a retained or live tool-output delta, emitted with `--verbose`.
- `tool_output_gap`: a tool-output retention or delivery gap, emitted with
  `--verbose`.
- `summary`: the final record, containing terminal status and exit code, IDs,
   turn/rejection/recovery counts, cancellation cause, final text, error detail, and the
  session usage and estimated-cost rollup.

`--output none` suppresses command output. Diagnostics and verbose progress
still use standard error.

### Exit codes

The active run's terminal event determines the runtime exit code. Command-line
syntax errors use Clap's exit code `2` before the runtime starts.

| Exit code | Meaning |
|---|---|
| `0` | `RunCompleted`, or `user_before_input` intentionally handled the input without starting a run |
| `1` | `RunFailed`, blocked model selection, or an unrecoverable active-driver failure |
| `3` | `RunCancelled` after the engine accepted a permission-triggered cancellation |
| `4` | Other `RunCancelled` outcomes and every `RunInterrupted`, including a stop signal, timeout, and turn-limit cancellation |
| `5` | Environment or setup failure before the run becomes active |

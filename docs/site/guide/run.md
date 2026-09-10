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

Useful first commands are `/help`, `/sessions`, `/new`, `/compact`, and
`/cancel`. [Agent](agents.md) covers authored prompts and permissions.

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

Attach from another terminal:

```sh
cookie attach
```

The attach URL defaults to `ws://127.0.0.1:7419/ws` and may be changed with
`--url`. Only loopback WebSocket URLs with the exact `/ws` path are accepted.
The client uses the local daemon token; see [Server](../engine/server.md).
Only open trusted workspaces: configured plugins and eager MCP servers can
start during engine initialization, before a model tool call is approved.
Allowed shell commands run without a filesystem sandbox; see the
[security contract](security.md#process-boundary).

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
| `/permissions` | Edit session permission overrides; see [Permissions](agents.md#permissions) |
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

The agent and model panels include search fields. Agent search matches agent IDs
and descriptions; model search matches display names and `provider/model_id`.
Both are case-insensitive. Use Down, Tab, or Enter to move from the search field
into the matching rows.

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
committed media parts appear collapsed by default. A session's system-prompt
row remains hidden until its first run starts, then identifies the agent from
the latest run snapshot. If the next-run draft uses another agent, the row also
shows `next: <agent>`. Click one of these rows to show or hide its text or
metadata. Expansion state is kept separately for each session. Expanded system
prompts are display-bounded to 256 lines or 32 KiB of sanitized text;
compaction, plugin-message, and media bodies are bounded to 64 lines or 8 KiB.
Oversized content ends with a truncated-lines indicator while the complete data
remains in session state.

OpenAI Responses turns can retain opaque continuation witnesses for provider
metadata such as message phase, annotations, and logprobs. Keep these parts
alongside native replay data when preserving or restoring history. Internal
agents validate the transport parts while using the text for summaries,
approval decisions, and titles. Older saved turns may lack witnesses that were
not captured at the time; historical events are not rewritten to invent them.

## Error details

Run failures show a concise headline followed by available model/provider identity,
HTTP status, provider code, request ID, and response body. The same diagnostics
are retained in session events and shown on replay. Model fallback and internal
agent failures also include the available response diagnostics. Error rows show
multiline text by default; expand the tool row for its display output.
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
`auto-approve -> auto-n -> auto-y -> ask -> yolo`; the mode applies to
subsequent approvals throughout the selected session tree, including delegated
descendants. Hard policy denies and doom-loop rejection still win in every
mode. See [Permissions](agents.md#permissions).

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
`SIGINT` follows the same cancellation path.

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
standard error. Failures and model fallback diagnostics are written to standard
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
| `4` | Other `RunCancelled` outcomes and every `RunInterrupted`, including `SIGINT`, timeout, and turn-limit cancellation |
| `5` | Environment or setup failure before the run becomes active |

# Headless runs

`cookie run` executes one prompt through the local engine without starting the
TUI, daemon, or an in-process protocol server. It is intended for CI and scripts.

## Prompt input

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

## Selection and limits

The default agent is the root-runnable `primary` agent, or the first
root-runnable agent. The default model is its first live fallback, including a
valid variant; if none is live, the first available model and its default
variant are used. Override these with `-a/--agent`, `-m/--model`, and
`--variant`; use `--variant base` to select no named variant. Every override is
validated against the current coherent runtime before a session starts.

`--resume-session <id>` continues an existing session. `--data-dir <path>`
selects the session and artifact store. `--max-turns` and `--timeout` are
positive guards and default to 100 root model turns and 600 seconds. Reaching
either guard cancels the run and waits for its terminal event. `SIGINT` follows
the same cancellation path.

## Permissions

`--permission-mode` accepts `auto-approve`, `ask`, or `yolo`. Headless runs
never wait for approval input. When an approval escalates anywhere in the
session tree, the runner rejects it, cancels the root run, and waits for the
matching terminal event.

`--allowed-tools` may be repeated or comma-delimited and accepts `read`,
`write`, `bash`, `delegate`, `mcp`, and `skill:<name>`. It adds an `allow` overlay for each
listed permission action. It does not deny omitted actions or replace existing
agent policy.

## Skills

`--skill <name>` loads a user-invocable skill before the prompt run.
`--skill-args <text>` supplies its raw arguments and requires `--skill`. Skill
permission is evaluated before injection; use `--allowed-tools skill:<name>` to
grant it explicitly in unattended runs. The load appends the same durable event
used by interactive and model invocation.

## Output

Select `text`, `json`, or `none` with `-o/--output`. `--json` is an alias for
`--output json` and conflicts with an explicit `--output`. `--output-file`
redirects text or JSON output to a file and cannot be combined with
`--output none`.

Text mode writes only the terminal `final_text` to standard output. It does not
stream model deltas. With `--verbose`, ANSI-free progress lines are written to
standard error; standard error stays empty for a successful non-verbose run.

JSON mode writes JSON Lines. Records use these stable `type` tags:

- `event`: one accepted, ordered event for the active run.
- `tool_output`: a retained or live tool-output delta, emitted with `--verbose`.
- `tool_output_gap`: a tool-output retention or delivery gap, emitted with
  `--verbose`.
- `summary`: the final record, containing terminal status and exit code, IDs,
  turn/rejection/recovery counts, cancellation cause, final text, and the
  session usage and estimated-cost rollup.

`--output none` suppresses command output. Diagnostics and verbose progress
still use standard error.

## Exit codes

The active run's terminal event determines the runtime exit code. Command-line
syntax errors use Clap's exit code `2` before the runtime starts.

| Exit code | Meaning |
|---|---|
| `0` | `RunCompleted`, or `user_before_input` intentionally handled the input without starting a run |
| `1` | `RunFailed`, blocked model selection, or an unrecoverable active-driver failure |
| `3` | `RunCancelled` after the engine accepted a permission-triggered cancellation |
| `4` | Other `RunCancelled` outcomes and every `RunInterrupted`, including `SIGINT`, timeout, and turn-limit cancellation |
| `5` | Environment or setup failure before the run becomes active |

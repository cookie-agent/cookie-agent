# Scheduling Prompts

A cron job is a prompt the daemon sends on a schedule: once after a delay,
or on a cron schedule. Each fire goes into a session that already exists or
starts a new session. You create jobs from the TUI, the web client, or
`cookie cron`; agents create them with the cron tools. The daemon keeps the
jobs in `crons.json` in its data directory, fires them while it runs, and
records what each fire did. The [cron spec](../specs/cron.md) has the full
design.

Jobs need a daemon of client protocol 1.15 or later. The clients hide their
cron views from an older one, and `cookie cron` refuses it with a message.

## A job

A job has an ID (eight lowercase letters and digits, which the daemon
assigns), an optional name of at most 80 characters, a prompt of up to 32 KiB,
a schedule, and a target. Lists show the name, or the start of the prompt
when the job has none.

### Schedules

A schedule is one of:

- **A cron expression:** five fields, minute, hour, day of month, month, and
  day of week, with `*`, lists (`1,15`), ranges (`1-5`), and steps (`*/15`);
  months and days may be named (`jan`, `mon`), and both 0 and 7 mean Sunday.
  When both day of month and day of week are restricted, either one matching
  fires, as in Vixie cron (`0 8 13 * 5` fires on the 13th and on every
  Friday). An expression that can never match, such as `0 0 30 2 *`, is
  refused.
- **A delay:** `in` and a whole number with `s`, `m`, `h`, or `d`, from `1s`
  to `365d`, such as `in 20m`. The daemon turns it into the time it fires
  at once, and the job fires once.

Cron times are the **daemon host's local time**. Daylight saving time is
handled as it comes: a time skipped by the spring change fires at the moment
the clock jumps to, and a time that occurs twice in autumn fires the first
time only.

The clients read a schedule plainly beside the expression: `every day at
03:00`, `every Monday to Friday at 09:00`, `every 15 minutes`, `on the 1st of
every month at 08:00`, `once at 2026-10-23 03:00`.

### How many times it runs

`max_runs` limits a cron schedule's fires: a whole number from 1, or
`unlimited`. **It defaults to 1**, so a job fires once unless it says
otherwise. A delay always fires once.

A job that has delivered `max_runs` fires, or a one-shot that has fired, is
finished:

- a job **you** created stays, disabled with the reason `finished`, until you
  delete it or enable it again (which starts its count over; a one-shot whose
  time has passed needs a new schedule first);
- a job **an agent** created is deleted.

A job with `max_runs: "unlimited"` runs until it is disabled or deleted.
There is no other expiry.

### Targets

A job's target is chosen when it is created:

| Target | Each fire |
|---|---|
| A session | Sends the prompt into that root session |
| A subagent's session | Sends the prompt into that delegated session, which reports its turn to its parent as every subagent turn does |
| A new session | Creates a new root session from the job's template and sends the prompt as its first input |

You can target any session. An agent can target only its own session or a
session below it (one of its subagents'); anything else is refused whatever
its rules say.

### Steer or queue

A job that fires into a session has a **delivery**:

- `queue` (the default): the prompt starts the session's next run when it is
  idle, or waits for the running turn to end;
- `steer`: the prompt joins the running turn; on an idle session it starts a
  run.

Either way the prompt runs with the session's current agent, model, and
permission mode. A session that has paged out is loaded again by the fire.
While an enabled job has a next fire, the session it targets counts as
waiting (`waiting on: scheduled job` in the session list), and a subagent
with such a job keeps its parent waiting. A disabled or finished job counts
for nothing, and a job never keeps a session in memory.

The model sees a fire marked as a scheduled job, never as you:

```text
<scheduled_job id="k3v9q2xa" name="Nightly dependency check" fired_at="2026-10-23T03:00:00+02:00">
Check for outdated dependencies and open a summary.
</scheduled_job>
```

A fire is not user input: it never answers an approval and never counts as
consent.

### New sessions

A new-session job carries a **template**: the agent each run session starts
with (and its preset), the permission rules added to it as a
[session overlay](../extend/agents.md#session-permission-overlays), its
runtime attachments and working directory, and its project. When you create
the job from the TUI or the web, the agent, its preset, and the project start
as the current session's, and the attachments are what a new session gets
(`global` and the daemon's default runtimes); a model pin and a permission
mode are yours to add. An agent's job takes the creating run's agent and the
creating session's attachments and project. The
template is checked when the job is saved, as `session.create` would check
it, and again at each fire.

Each run session:

- runs an agent that can run as a root (`primary` or `all` mode);
- starts in the permission mode **`auto-approve-n`** unless you chose
  another: the approval classifier decides each `ask`, and what it would pass
  to you is rejected, so an unattended run never waits on you. Only you can
  set a job's mode, from a client; agents cannot, and the tools have no mode
  parameter;
- runs the agent's own models unless you pinned a model; only you can pin
  one;
- is titled `<name or the prompt's start> · <local time of the fire>`, which
  you can rename as usual;
- appears in the session list with a clock mark, and records the job and the
  fire that started it (`SessionMeta::cron`). It is an ordinary session
  otherwise, and stays when the job is deleted.

Because the creator adds the template's rules, a job can do more than the
session that created it: an agent allowed to create new-session jobs can
start sessions of any agent with any rules. Grant `cron` on `new` to a
subagent with that in mind ([Permissions](#permissions)).

## Firing

### Skips and no catch-up

A fire is **skipped**, and recorded with the reason, when:

- the job's previous prompt into its session has not been delivered yet
  (`previous fire not delivered`);
- a new-session job's previous run session is still running (`previous run
  still running`);
- `[cron] enabled = false` holds (`firing turned off`);
- the daemon stopped between recording a fire and delivering it
  (`interrupted`).

There is **no catch-up**. A fire missed while the daemon was down is not
run: when the daemon starts, each job whose time passed gets one history
entry, `skipped: daemon was down`, with the number of fires missed, and
moves to its next future time. Each fire happens exactly once, even across
a restart or two daemons sharing a data directory.

### Failures

A fire **fails** when it cannot start (the agent is missing or disabled, no
model is available, a runtime is offline, or the session cannot be created)
or when the run it led to ends `failed`. A fire's outcome is the run its
prompt lands in: a steered prompt succeeds or fails with the turn it joined,
a queued one with the run that takes it.

After `[cron] max_consecutive_failures` failures in a row (see
[Cron jobs](../engine/cron.md)), which a job can override, the job is
disabled with the reason `failed <n> times in a row`. A completed run resets
the count; a cancelled run and a skip change nothing. Enabling the job again
resets the count. A failed fire does not count toward `max_runs`.

### History

Each job keeps its last `[cron] history` fires. An entry has the scheduled
time and the time it fired, the session it went to and the run it started or
joined, its outcome (`delivered`, `skipped` with the reason, or `failed` with
the error), and how that run ended. The clients show it newest first.

### Running a job now

You can fire a job at once from any client (`crons.run`): it is delivered as
its schedule would deliver it, a new root session for a new-session job, and
recorded as `run now`. A run now counts toward neither `max_runs` nor the
failure count, moves nothing else, and fires even with `[cron] enabled =
false`. It still obeys the skips: a run now of a job whose previous prompt is
still waiting is recorded as skipped.

An agent runs a job now with `cron_run`, the way it delegates: a new-session
job runs as a **subagent of the caller** with the job's agent, rules, and
attachments, in the foreground (waiting for the result) or in the background
(reporting when done), and counts against the caller's delegation depth. The
job's agent need not be one the caller may delegate to. An in-session job
fires into its session as scheduled.

### Deleting

Deleting a session deletes the jobs that target it or any session in its
tree. Deleting a project deletes its sessions with their jobs, and the
new-session jobs whose template names the project. Deleting a job keeps its
run sessions.

## Managing jobs

### Terminal UI

`/cron` opens the cron panel: each job with its name, schedule and its plain
reading, next fire, target, state, and the outcome of its last fire; under
it, the selected job's details and history. Its actions, create, edit,
enable or disable, run now, and delete, are keys and buttons. `/cron new`
from the command panel creates a job that fires into the session on screen.
A job is written as a short form in your editor. Run sessions have a 🕒
mark in the session picker.
See [Terminal UI](../clients/tui.md#scheduled-prompts).

### Web

The clock icon in the left rail opens **Scheduled prompts**, with the same list,
details, history, and actions. In a session, **Schedule a prompt** in the ⋮
menu creates a job that fires into it. A run session's clock mark in the
session list opens its job. See [Web](../clients/web.md#scheduled-prompts).

### Command line

```sh
cookie cron create --name "Nightly dependency check" \
  --prompt "Check for outdated dependencies and open a summary." \
  --schedule "0 3 * * *" --max-runs unlimited --agent builder
cookie cron list
cookie cron run k3v9q2xa
```

`cookie cron` talks to the daemon like `cookie memory`; for a remote one
pass `--url wss://host/ws --token <token>`. See
[`cookie cron`](../clients/cli.md#cookie-cron).

## Agents and the cron tools

The `global` runtime offers five tools: `cron_create`, `cron_list`,
`cron_update`, `cron_delete`, and `cron_run`, and `list_agents`, which lists
the agents a new-session job may run with their models and starting rules.
Their arguments and descriptions are in the
[tools reference](../reference/tools.md#cron-tools). An agent's `target` is
`"self"`, a subagent's handle, or `{"new": {…}}`.

### Permissions

The tools are checked under the `cron` action, whose resource is the target
kind of the job a call touches: `session`, `subagent`, or `new`. One action
covers listing, creating, editing, enabling and disabling, deleting, and
running:

| Run | Default without a `cron` key |
|---|---|
| Root, any agent but an internal one | `cron: allow` |
| Delegated | none: the tools are hidden |
| Internal agent | none |

An agent manages every job its rules allow, its own or not; `cron_list`
shows only the jobs whose kind its rules allow or ask for. To let a subagent
schedule prompts into its own session and the sessions below it, but not
start new sessions:

```yaml
permissions:
  cron:
    subagent: allow
```

See [Cron permissions](../extend/agents.md#cron-permissions).

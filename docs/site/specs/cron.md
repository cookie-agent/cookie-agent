# Cron jobs

Status: **draft** (2026-10-22), settled with the user, awaiting the go-ahead
to implement. Builds on [background tool calls](background-tools.md)
(sections 12 and 13: what keeps a parent waiting, and producer waits that
let a session page out), [projects](projects.md), and the
[memory spec](memory.md) (its permission defaults).

## 1. Goal and scope

You and agents can schedule prompts: once after a delay, or on a cron
schedule. Each fire either goes into an existing session or starts a new
session. The daemon keeps the jobs, fires them while it runs, and records
what each fire did.

The design draws on a survey of Claude Code (in-session `CronCreate` and
`/loop`, Desktop scheduled tasks, cloud routines), OpenClaw (automations)
and Hermes Agent (`cronjob`). From Claude Code it takes "each run is a
session" for new-session jobs and "a fire waits its turn" for in-session
ones.

**Decided 2026-10-22:**

- You and agents can create, list, edit, and delete jobs. A job is an id,
  a prompt, a schedule, and a target.
- **Schedule:** a 5-field cron expression, or a one-shot delay such as
  `in 200s`. The only other schedule option is `max_runs`, default 1 (added
  2026-10-22). Cron times are
  the daemon host's local time. There are no self-paced loops (an agent can
  already wait on a background command).
- **Payload:** a prompt, nothing else (no shell commands, no scripts).
- **Target**, chosen when the job is created:
  - **a session:** you can name any session, and `/cron` in the TUI or web
    command panel targets the current one. An agent can target its own
    session or one of its subagents' sessions, nothing else;
  - **a new session per run**, from a template: the creator picks the
    agent and may add permission rules, so a job can do more than its
    creator. Only you can set a job's permission mode; every job, an
    agent's or yours, starts in `auto_approve_n`.
- **Firing into a session:** the creator picks `steer` or `queue`. The
  fire runs with the session's current agent.
- **Subagents** may use the cron tools when their rules allow it; `cron`
  has no default for delegated runs, so it is hidden unless granted. Root
  runs default to `cron: allow`.
- **Agents can manage every job.** The `cron` permission's resource is the
  target kind (`session`, `subagent`, or `new`), and one action covers
  reading and changing jobs.
- **No expiry:** a job with `max_runs: "unlimited"` runs until it is
  deleted or disabled. A job that reaches `max_runs` is deleted if an agent
  created it and kept, disabled as finished, if you did.
- **No catch-up:** a fire missed while the daemon was down is recorded as
  skipped and not run.
- **Failures:** a job is disabled after `[cron] max_consecutive_failures`
  failures in a row (default 3), which a job can override. Skips do not
  count and do not reset the count.
- **Overlap:** a fire is skipped while the job's previous fire is still
  undelivered, or, for a new-session job, while its previous run is still
  running.
- **Results:** run sessions appear in the session list, marked with the
  job that started them. Notifications are designed later.
- A subagent with an enabled job that has a next fire time keeps its
  parent waiting. A session whose only pending thing is a job can page
  out; the fire reloads it.
- Deleting a session deletes the jobs that target it.
- The `global` runtime gains a `list_agents` tool, offered whenever
  `delegate_subagent` or `cron` is, so a model can choose an agent, and
  the rules to add, from what each agent is (section 8.1).

**Out of scope:** notifications, catch-up runs, self-paced loops, shell or
script payloads, time zones other than the host's, webhooks and event
triggers.

## 2. Vocabulary

| Term | Meaning |
|---|---|
| **Job** | A scheduled prompt with an id, a schedule, and a target. |
| **Fire** | One moment a job's schedule comes due. |
| **Run** | What a delivered fire leads to: a run in the target session, or a new session's first run. |
| **In-session job** | A job whose target is an existing session (`session` or `subagent`). |
| **New-session job** | A job whose target is `new`: each fire creates a root session from the job's template. |

## 3. The job

```json
{
  "id": "k3v9q2xa",
  "name": "Nightly dependency check",
  "prompt": "Check for outdated dependencies and open a summary.",
  "schedule": { "cron": "0 3 * * *" },
  "max_runs": "unlimited",
  "target": { "new": { "agent": "builder", "project": "cookie-agent", "...": "..." } },
  "delivery": null,
  "permission_mode": "auto_approve_n",
  "max_consecutive_failures": null,
  "enabled": true,
  "created_by": { "agent": { "agent": "default", "session": "…" } },
  "created_at": "2026-10-22T18:00:00Z"
}
```

- **`id`:** eight lowercase letters and digits, unique among the daemon's
  jobs, assigned by the daemon.
- **`name`:** optional, at most 80 characters, shown in lists and run
  titles. Without one, lists show the start of the prompt.
- **`prompt`:** 1 to 32 KiB of text.
- **`schedule`:** one of:
  - `{"cron": "<expr>"}`: five fields (minute, hour, day of month, month,
    day of week), with `*`, lists, ranges, and steps; day of month and day
    of week combine with OR, as in Vixie cron. Evaluated in the host's
    local time zone. An expression with no next match (`0 0 30 2 *`) is
    refused.
  - `{"in": "<duration>"}` on create and update: a whole number with `s`,
    `m`, `h`, or `d`, from `1s` to `365d`. The daemon turns it into
    `{"at": "<RFC 3339 time>"}` at once and stores that; it fires once.
- **`max_runs`:** cron schedules only: a whole number from 1, or
  `"unlimited"`. **Default 1**, so a job fires once unless told otherwise.
  A job that has run `max_runs` times:
  - created by an agent: is deleted;
  - created by you: stays, disabled with the reason "finished", until you
    delete or re-arm it.
- **`target`:** one of:
  - `{"session": "<session id>"}`: a root session;
  - `{"subagent": "<session id>"}`: a delegated session;
  - `{"new": <template>}`, section 3.1.
- **`delivery`:** `steer` or `queue` for in-session jobs, default `queue`;
  absent for new-session jobs.
- **`permission_mode`:** new-session jobs only. Default `auto_approve_n`.
  Only you can set it (section 7); an agent's create or update cannot name
  it.
- **`max_consecutive_failures`:** optional override of the config value.
- **`enabled`**, and when disabled a **`disabled_reason`**: `finished`,
  `failed <n> times in a row`, or `disabled by <user or agent>`.
- **`created_by`:** `user`, or `{"agent": {agent, session}}`.

### 3.1 The new-session template

| Field | Meaning | When an agent creates the job | When you create it |
|---|---|---|---|
| `agent` | The agent, and optionally its preset | Any agent; default the creating run's agent | Any; default the current session's |
| `model` | A model selection | Not settable: the agent's own model chain | Optional pin |
| `permission_rules` | Rules added to the run, as a session overlay | Any | Any |
| `runtimes`, `global`, `cwd` | Attachments, as `session.create` takes them | Default the creating session's | Default the current session's, or the project's |
| `project` | The project the run sessions belong to | Default the creating session's | Default the current session's |

A template is checked when it is saved, as `session.create` would check it
(the agent exists and is enabled, the runtimes are known), and again at
each fire.

### 3.2 Job state

The daemon keeps, per job:

- **`next_fire`:** the next due time, or none for a disabled or finished
  job;
- **`runs`:** fires that were delivered or started a session, counted
  against `max_runs` (skips do not count);
- **`consecutive_failures`**;
- **`last_fire`**;
- **the run history:** the last `[cron] history` fires (default 20), oldest
  dropped first. Each entry has:
  - the scheduled time and the actual fire time;
  - the session it went to (the new session for a new-session job) and
    the run it started or steered into;
  - the outcome: `delivered`, `skipped` with a reason (`previous fire not
    delivered`, `previous run still running`, `daemon was down`,
    `interrupted`), or `failed` with the error;
  - for a run the fire started or that it steered into, how that run ended
    (`completed`, `failed`, `cancelled`) and when.

## 4. Firing

### 4.1 The scheduler

- One scheduler task in the daemon, started and stopped like the residency
  janitor. It sleeps until the earliest `next_fire` or until jobs change,
  and fires every due job.
- **Exactly once.** A fire is keyed by job id and scheduled time. Before it
  delivers, the daemon writes the fire to the job's history; a restart
  that finds a fire written but neither delivered nor started records it as
  `skipped (interrupted)`.
- **Missed fires.** At start, a job whose `next_fire` has passed gets one
  history entry, `skipped (daemon was down)`, naming the first missed time
  and how many fires were missed, and `next_fire` moves to the next future
  match. A one-shot that passed is finished the same way.
- **The kill switch:** `[cron] enabled = false` stops firing; jobs, RPCs,
  and tools keep working.

### 4.2 In-session fires

- The prompt arrives through the same producer path subagent reports use,
  so it is persisted, survives restarts, and is delivered exactly once:
  - `queue`: the session's next run starts with it when the session is
    idle, or after the current turn when it is busy;
  - `steer`: into the current turn when one is running; on an idle session
    it starts a run.
- It runs with the session's current agent, model, and permission mode.
- The model sees it marked as a scheduled job, never as the user:

  ```text
  <scheduled_job id="k3v9q2xa" name="Nightly dependency check" fired_at="2026-10-23T03:00:00+02:00">
  Check for outdated dependencies and open a summary.
  </scheduled_job>
  ```

  A fire is not user input: it never answers an approval and never counts
  as consent.
- **Skipped** while the job's previous fire is still undelivered.
- A fire into a **subagent's session** wakes it like any producer message;
  its turn reports to its parent as every subagent turn does.
- A fire into a session that has paged out reloads it.

### 4.3 New-session fires

- Each fire creates a root session from the template, in the template's
  project, with the job's permission mode and the template's rules as its
  overlay, and submits the prompt (marked as in section 4.2) as its first
  input.
- **Title:** `<name, or the prompt's start> · <local time of the fire>`.
- **The session records its job:** `SessionMeta::cron = {job_id,
  fired_at}`, so clients can mark and filter run sessions. The session is
  otherwise ordinary; it stays when the job is deleted.
- A new-session job's agent must be one that can run as a root
  (`primary` or `all` mode). A `subagent`-mode agent reaches a job only
  through an in-session job targeting a subagent's session.
- **Skipped** while the job's previous run session is still running.

### 4.4 Failures and finishing

- **A failure** is a run the fire started that ends `failed`, or a fire
  that cannot start: the agent is missing or disabled, no model is
  available, a runtime is offline, or session creation fails.
- **A fire's outcome is the run its prompt lands in.** Steering into a busy
  turn is not a failure: the fire succeeds or fails with the turn it
  joined. A queued fire succeeds or fails with the run that takes it. While
  that prompt is still waiting, a new fire is skipped (section 4.2), so its
  outcome is the earlier prompt's.
- A run that completes resets `consecutive_failures` to 0. A cancelled run
  and a skip change nothing.
- At `max_consecutive_failures` the job is disabled with the reason. Enabling
  it again resets the count.

## 5. Waiting and residency

- **Waiting.** A session with an enabled in-session job that has a next
  fire time lists a new `WaitingOn::ScheduledJob`. It plugs into
  `live_waiting_kinds` (background tools section 12), so a subagent with
  such a job keeps its parent waiting, up the tree. Disabled and finished
  jobs count for nothing.
- **Residency.** A job never keeps a session in memory (background tools
  section 13); the fire reloads it. The jobs live in the cron store, not
  in the session.

## 6. Deleting

- Deleting a session deletes the jobs that target it or any session in
  its tree.
- Deleting a project deletes its sessions, so their jobs go too.
  New-session jobs whose template names the project are deleted as well.
- A job's run sessions are ordinary sessions: deleting the job keeps them.

## 7. Permissions

- **The action:** `cron`. Its resource is the target kind of the job a call
  touches: `session`, `subagent`, or `new`. One action covers listing,
  creating, editing, enabling and disabling, and deleting.
- **Defaults** (each applies only when the agent document has no `cron`
  key):

  | Run | Default |
  |---|---|
  | Root, any non-internal agent | `cron: allow` |
  | Delegated | none: the tools are hidden |
  | Internal | none |

- **What a call checks:**
  - create: the new job's target kind;
  - update: the job's current and, when it changes, new target kind;
  - delete, enable, disable: the job's target kind;
  - list: shows only jobs whose target kind the rules allow or ask for.
- **Which sessions an agent may target:** its own session, or one of its
  subagents' sessions (any session below it in its tree). Anything else is
  refused whatever the rules say. You can target any session.
- **Permission mode:** the tools have no permission-mode parameter. Only a
  client (`crons.create`, `crons.update`) can set it.

## 8. Model tools

The cron provider, `builtin.cron`, offers five tools, each gated by
`cron`:

| Tool | Arguments | Result |
|---|---|---|
| `cron_create` | `prompt`, `schedule` (`{cron}` or `{in}`), `max_runs?`, `name?`, `target` (`"self"`, `{subagent: <handle>}`, or `{new: {agent?, permission_rules?, runtimes?, project?}}`), `delivery?` | The job: id, next fire time in local time and UTC |
| `cron_list` | `target?` (a kind, to filter) | Each job the rules let it see: id, name, schedule, next fire, target, enabled, the last fire's outcome |
| `cron_update` | `id`, any field of `cron_create`, `enabled?` | The updated job |
| `cron_delete` | `id` | Confirmation |
| `cron_run` | `id`, `background?` | Runs the job now as a delegated subagent of the caller (section 8.2) |

- `"self"` is the calling session. A subagent is named by its handle, as
  `delegate_subagent` names it.
- Descriptions say: schedules are in the host's local time; a fire is
  marked as a scheduled job; prefer updating a job to creating a near
  duplicate; `max_runs` defaults to 1, and `"unlimited"` runs until
  deleted.

### 8.1 `list_agents`

A tool of the `global` runtime (provider `builtin.agents`), so a model
picking an agent for a delegation or a new-session job sees what each agent
is and which permissions it starts with.

- **Visibility:** offered exactly when the run is offered
  `delegate_subagent` or any cron tool. It has no permission action of its
  own, as it reveals nothing the caller could not use.
- **Arguments:** `for?`: `delegate` or `cron`, to list only the agents that
  use allows. Without it, both.
- **Which agents:**
  - for `delegate`: the run's delegation targets (enabled `subagent` and
    `all` agents its `delegate` rules allow), at its remaining depth;
  - for `cron`: every enabled non-internal agent, since a new-session job
    may run any of them.
- **Each agent:** its id, description, mode, presets, the models of its
  default chain (provider/model and variant), and its permission rules as
  a run of it would have them: its own document's rules plus the defaults
  for an action it names no key for, with each rule marked `document` or
  `default`. Memory and cron defaults are shown for a root run, the
  delegated defaults for a delegation.
- **Example result:**

  ```text
  builder (primary) — Primary agent for building features and implementing code changes directly.
    models: deepseek/deepseek-flash (high)
    usable for: cron
    permissions:
      read: allow
      write: ${workspace_dir}/* allow; * ask
      bash: allow
      webfetch: allow
      mcp: my_chrome_* allow
      list_runtimes: allow (default)
      memory: * allow (default) ...
  ```

### 8.2 `cron_run`

Added 2026-10-22. An agent runs a job now the way it delegates:

- **A delegated subagent of the caller.** For a new-session job, the child
  session uses the job's template (agent, permission rules, attachments)
  and the prompt as its first input, under the caller's session, not as a
  new root.
- **Foreground or background,** like `delegate_subagent`: foreground waits
  and returns the result; background returns the child's handle at once,
  and the caller reads the result with `get_subagent_result` or receives
  the usual background report.
- The child counts against the caller's delegation depth and its resident
  subagents, like any delegated child.
- See section 15 for what `cron_run` does with an in-session job and how it
  counts in the job's history.

## 9. Client protocol

- **RPCs:**
  - `crons.list` → every job with its state and history;
  - `crons.create`, `crons.update` (any field, `permission_mode`
    included), `crons.delete`;
  - `crons.run`: fire a job now, as a scheduled fire would (a new root
    session for a new-session job). It is recorded with the actual time as
    its scheduled time and moves nothing else;
  - `crons.changed`: notification, empty, coalesced like
    `projects.changed`.
- **Additions:** `SessionMeta::cron`, `WaitingOn::ScheduledJob` (an open
  enum), the RPCs above.
- **Delivery's owner.** `ProducerOwner` is a closed enum, so a new variant
  would be a major bump. The implementation must find an additive
  encoding, for example an existing owner shape or a new optional field,
  and record its choice.
- Client protocol minor, plugin protocol minor (plugins see events and
  `SessionMeta`). Runtime protocol and history format unchanged.

## 10. Configuration

```toml
[cron]
enabled = true                 # false stops firing; jobs stay
max_consecutive_failures = 3   # a job can override it
history = 20                   # fires kept per job
```

## 11. Storage

- `<data_root>/crons.json`, one versioned file (`version: 1`), written
  atomically like `machines.json` (temporary file, sync, rename) under the
  daemon's state lock, with a watch channel for `crons.changed`.
- Unknown fields are refused, as in `machines.json`.

## 12. Clients

- **TUI:**
  - `/cron` opens a panel: jobs with name, schedule (and its plain
    reading, such as "every day at 03:00"), next fire, target, and last
    outcome; the selected job's details and history; actions to create,
    edit, enable or disable, run now, and delete. Every action is also a
    button with its own hover.
  - In the command panel, `/cron` creates a job targeting the current
    session.
  - Run sessions show a clock mark in the session list.
- **Web:**
  - a Cron view behind a new icon in the left rail, with the same list,
    details, history, and actions;
  - the ⋮ menu's "Schedule a prompt" creates a job for the current
    session;
  - run sessions show a clock mark in the session list, linking to their
    job.
- **CLI:** `cookie cron list | show <id> | create | edit <id> | rm <id> |
  enable <id> | disable <id> | run <id>`, through the daemon like `cookie
  memory`, with the token inline (`--token <token>`) for a remote daemon.

## 13. Tests

- **Schedules:** cron parsing and next-match boundaries with explicit times
  (month ends, Vixie OR, DST changes in an explicit zone), `in` units and
  limits, refused expressions.
- **Firing:** queue and steer into idle and busy sessions; the
  `<scheduled_job>` marking; exactly-once across a restart between the
  history write and delivery; a missed fire recorded as `skipped (daemon
  was down)` with no catch-up; skips on an undelivered fire and on a still
  running new-session run; a paged-out target reloaded by its fire.
- **New sessions:** template defaults from the creating session; the
  permission mode `auto_approve_n` unless a client set another; an agent's
  call cannot set the mode; `SessionMeta::cron`; a `subagent`-mode agent
  as a job's agent.
- **Failures:** the count across failures with skips between them, reset
  on completion, unchanged on cancel, disable at the threshold with a
  per-job override.
- **`list_agents`:** offered with `delegate_subagent` or a cron tool and
  never otherwise; the agents per use; rules marked `document` or
  `default`, with root and delegated defaults where they differ.
- **Permissions:** root default, hidden for delegated runs without a key,
  target kinds checked on create, update and delete, list filtered,
  targets outside the caller's subtree refused.
- **Waiting:** a subagent's job keeps the parent waiting; disabling it
  releases the parent.
- **Deleting:** session and project deletion take their jobs.
- **Clients:** TUI and web panels, the clock mark, `cookie cron`.

## 14. Phases

1. **Engine and protocol:** the store, scheduler, in-session and
   new-session firing, failures, waiting, permissions and the tools,
   `list_agents`, the RPCs and notification, config.
2. **Clients:** TUI, web, CLI.
3. **Docs:** a scheduling guide, the permissions and configuration pages,
   the protocol changelog.

## 15. Open questions

- **`cron_run` on an in-session job.** Does it fire into the job's target
  session as a scheduled fire would, or also start a delegated subagent of
  the caller with the prompt?
- **`cron_run` in the job's state.** Is a run-now recorded in the job's
  history, and does it count toward `max_runs` and the failure count?
- **`cron_run`'s agent mode.** A new-session job's agent is `primary` or
  `all`; a `primary`-only agent cannot normally be delegated to. Does
  `cron_run` make an exception, or refuse a job whose agent cannot run as
  a subagent?

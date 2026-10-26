# Held sessions: stopped and failing automatic runs

Status: **approved** (2026-10-26).

## 1. Problem

A session starts runs of its own for producer messages: subagent reports,
agent mail, plugin and runtime producers, cron fires into the session, and
goal reminders. On 2026-10-25 a session looped: its run failed at once with
a non-retryable 400, the failure released the subagent report it carried,
and the next reconcile started a new run with it, about twice a second,
16,000 events in fifteen minutes. The user's Stop cancelled one run and the
next started at once.

`[model_retry]` bounds the retries of one model attempt inside a run; nothing
bounded the runs a producer message starts. A bound alone is not enough
either: an engine-made goal reminder is a new producer message, so counting
failures "since the last message" lets goal mode retry forever.

## 2. The rule

A session is **held** when its automatic runs have stopped:

- **Stopped:** the user cancelled its run (Stop in a client, `run.cancel`;
  user decision of 2026-10-25: Stop also stops what would start next).
- **Failing:** its runs failed more than `[model_retry] standard_retries`
  times in a row (negative: never held for failures). The count is of
  consecutive `failed` runs since the last run that completed or the
  user's last input or resume; producer messages, goal reminders included,
  never reset it. Each retry before the hold waits the model retry backoff
  (1 s, 2 s, 4 s, ... up to `backoff_ceiling_ms`).

The bound and the refusals follow the user's proposal of 2026-10-26:
producers are refused while a session holds failed messages, as cron
skips, and the user clears the state with a Resume in place of Send.

A run interrupted by a daemon restart is neither: it resumes once, as now,
and a failure of that run counts.

While a session is held, nothing starts a run on its own, and **nothing new
is accepted for it**, as a cron fire is skipped while the job's previous
fire is still undelivered:

| Producer | While held |
|---|---|
| Plugin and runtime producers (`producer.send`) | Refused with the error `session_held` (its reason in `data`); the producer may send again after a resume. |
| Agent mail to the session | From the session's parent: a resume (section 4). From any other agent: refused to the sender's tool call, `the session is held (<reason>); it takes mail once the user or its parent resumes it`. |
| A cron fire into the session | Skipped, recorded as `skipped (session held)`; a skip is not a failure (cron spec section 4.4). |
| A subagent's completion report | Deferred, not refused: it stays with the delegation and is delivered on resume, as the engine already re-sends a report its parent has not accepted. |
| Goal reminders | Not made; the goal stays active and its reminders continue after a resume. |

Messages accepted before the hold stay pending in the log and go with the
resume. Nothing is lost and nothing is sent until the user acts.

## 3. Recording it

Two additive events, so the hold survives restarts and clients show it:

- `automatic_runs_held { reason }`: appended when the session becomes held.
  `reason` is open: `stopped`, or `failing` with `failures` (the count) and
  `error` (the last run's model error summary).
- `automatic_runs_resumed {}`: appended by a resume (section 4), with the
  user's origin.

The session is held while its latest of the two is `automatic_runs_held`.
`SessionMeta` carries `held: { reason } | null`, so session lists badge it.

## 4. Resuming

`session.resume { session_id, content? }`: clears the hold
(`automatic_runs_resumed`) and starts one run with every pending producer
message and, when given, `content` as the user's input. The failure count
starts over. Refused when the session is not held (`session_not_held`) or
is running.

- **A plain message while held** (`run.start`, `session.input`, an older
  client) resumes the same way: it is the user acting.
- **No discard** (user decision of 2026-10-26): Resume always sends the
  pending messages.
- **The parent resumes a held subagent** (user decision of 2026-10-26):
  its `delegate_subagent` with the child's `resume_session_id`, or its
  `send_message` to the child, clears the child's hold as the user's
  message does, with the parent's prompt or mail as the input. Only the
  session's own parent can; other agents in the tree are refused. A held
  subagent's turn reports to the parent as failed, with the hold's reason
  in the report (`status="held"`), so the parent can decide.

## 5. Clients

- **Web:** while the viewed session is held, the composer's Send and Queue
  buttons are replaced by **Resume**, which sends the composer's text, if
  any, with the held messages. A line in the transcript says why: *Stopped
  by you* or *Failed 4 times: Anthropic request failed (400) ...*, with
  the pending messages counted (*2 messages waiting*).
- **TUI:** the same line; Enter in the composer resumes (the hint row says
  *Enter resume*), and a Resume button in the line, with its hover, does
  the same.
- **Session lists** badge a held session (`held`).
- **Push** (user decision of 2026-10-26): a notification when a root
  session becomes held for failures (*<title>: stopped after 4 failed
  runs*), none for a stop, which the user did.

## 6. Protocols and history

- Client protocol minor bump: the two events, `SessionMeta.held`,
  `session.resume`, and the errors `session_held` and `session_not_held`.
  An older client sees the events as unknown records; its plain message
  still resumes.
- Plugin and runtime protocols minor bump: `producer.send` may answer
  `session_held`.
- History format: additive events, no conversion.

## 7. Tests

- A run failing with a non-retryable error: `standard_retries` retries
  with their backoff, then `automatic_runs_held { failing }`, and no run
  after it; a pending goal does not keep the session going.
- A goal whose runs fail after consuming their reminder: held at the
  budget, no new reminders.
- Stop: held at once; the pending report goes with the resume.
- While held: `producer.send` refused, agent mail refused, a cron fire
  skipped, a subagent report deferred and delivered on resume.
- A restart: an interrupted run resumes; a held session stays held.
- `session.resume` with and without content, and refused when not held;
  a plain message resumes.
- A held subagent: its report to the parent says `held`; the parent's
  `delegate_subagent` resume and its `send_message` clear the hold; a
  sibling's mail is refused.
- A push for a hold for failures, none for a stop.
- Clients: the Resume button in place of Send and Queue, the transcript
  line, the TUI's Enter and button hover.

## 8. Decisions

All of 2026-10-26, by the user:

1. No Discard: Resume always sends the pending messages.
2. A push when a session becomes held for failures, none for a stop.
3. The parent resumes a held subagent with `delegate_subagent`
   (`resume_session_id`) or `send_message`; other agents cannot.

# Held sessions

A session can start runs on its own: a subagent reports, another agent sends
mail, a plugin or a cron job delivers a prompt, or a goal reminds the agent
to go on. When that goes wrong, the session is **held** instead of trying
again and again.

## When a session is held

- **You stop its run while something could start the next one**: a
  message waiting for it, an active goal, or a background call, subagent,
  or plugin that will report. Stop (Esc in the TUI, the Stop button on the
  web) ends the run, and none of these starts the next one. With nothing
  such, a stop holds nothing.
- **You revert it** to a point where messages were still waiting: they
  wait for you.
- **Its runs keep failing.** A run that fails is tried again after a short
  backoff (1 s, 2 s, 4 s, ... up to `[model_retry] backoff_ceiling_ms`),
  as long as the failures in a row stay within `[model_retry]
  standard_retries` (3 by default; negative never holds). Past that, the
  session is held, and a push notification says so.

A run that a daemon restart interrupted is neither: it resumes as before.

## While it is held

- Nothing starts a run of the session on its own, and goal reminders stop.
- What was waiting stays waiting: nothing is lost.
- Reports from its own subagents, delegations, and background calls are
  taken and wait too.
- Plugins and runtimes are refused with `producer_rejected`, a cron job's
  fire into it is skipped (`session held`), and mail from any agent other
  than its parent is refused.

The transcript records why (*Automatic runs held: stopped by you* or
*failed 4 times in a row: ...*), and the session's line says what waits.

## Resuming

- **Resume** clears the hold and sends what waits in one run. On the web it
  takes the place of Send and Queue, and sends the composer's text too, if
  any. In the TUI, click *resume* on the banner above the composer, or press
  Enter in an empty composer.
- **Any message you send** resumes the session as well, and takes what waits
  with it.
- **A subagent's parent** resumes it by continuing it
  (`delegate_subagent` with its `resume_session_id`) or by sending it a
  message.

Clients use `session.held.resume` (client protocol 1.34.0); see
[Held sessions](../reference/protocol.md#held-sessions) in the protocol
reference.

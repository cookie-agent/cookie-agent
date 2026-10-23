# Cron Jobs [cron]

Control how the daemon fires cron jobs, the prompts you and agents schedule
into sessions (see [Scheduling prompts](../sessions/scheduling.md) and the
[cron spec](../specs/cron.md)). Complete
`config.toml`:

```toml
[cron]
enabled = true
max_consecutive_failures = 3
history = 20
```

The jobs themselves live in `crons.json` in the data directory, not in the
configuration. An omitted table inherits; an authored table replaces the
lower table and uses defaults for omitted fields. Unknown fields fail; see
[config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Whether the daemon fires jobs. With `false`, each due fire is recorded as skipped (`firing_disabled`) and the job moves to its next fire; jobs, the `crons.*` requests, and the cron tools keep working. |
| `max_consecutive_failures` | integer | `3` | Failures in a row that disable a job, unless the job sets its own `max_consecutive_failures`. A failure is a fire that cannot start or a run it started that ends `failed`; skips and cancelled runs do not count, and a completed run resets the count. Must be at least 1. |
| `history` | integer | `20` | Fires each job's history keeps, oldest dropped first. Must be 1..=1000. |

The table applies at once when the configuration changes: the next fire reads
it.

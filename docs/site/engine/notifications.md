# Push Notifications [notifications]

Control the Web Push notifications the daemon sends to the phones and
browsers you turned notifications on in (see the
[push notifications spec](../specs/push-notifications.md)). Complete
`config.toml`:

```toml
[notifications]
enabled = true
preview = "reply"
approvals = true
cron_sessions = true
contact = "mailto:you@example.com"
```

The daemon pushes when a root session settles (its run ended `completed` or
`failed`, and nothing but a recurring cron job can start another run), and,
with `approvals`, when an approval or an ACP agent's question waits for you
anywhere in a session's tree. The persistent daemon also pushes when it goes
down for a restart a client asked for (not to the device that asked) and
when it is up again. Nothing is pushed for a session a client shows
you at that moment. The subscriptions themselves live in `push/` in the data
directory, not in the configuration. An omitted table inherits; an authored
table replaces the lower table and uses defaults for omitted fields. Unknown
fields fail; see [config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Whether the daemon sends pushes. With `false`, nothing is pushed; turning notifications on, the device list, and test pushes keep working. |
| `preview` | string | `"reply"` | What a notification shows on the lock screen: `"reply"`, the start of the final reply, the error, the tool call, or the question; or `"status"`, only `Finished.`, `Failed.`, `Approval waiting`, or `Question waiting`. |
| `approvals` | boolean | `true` | Push approvals and ACP agents' questions that wait for you. |
| `cron_sessions` | boolean | `true` | Push when a root session a new-session cron job created settles. Its approvals and questions are pushed either way. |
| `contact` | string | the repository URL the daemon was built from | The contact push services see for this daemon (the VAPID `sub` claim): an `https:` URL or a `mailto:` address. Nothing checks it but its form; set it when you want to be the one a push service writes to. |

The table applies at once when the configuration changes: the next push reads
it.

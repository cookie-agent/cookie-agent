# External Producers

Status: **implemented** (decisions of 2026-10-16; both code phases landed 2026-10-16). This document is the governing
specification for letting plugins and runtimes register any number of named
producers and send messages into sessions. Section
[Decisions](#10-decisions) records what review settled.

## 1. Goal and scope

A *producer* puts messages into a session's model input from outside a user
turn: a background call's result, a subagent's report, agent mail, a goal
reminder, or a plugin's message. The durable model (accepted, admitted,
claimed, consumed, and discarded messages, deduplicated on
`(session, owner, idempotency key)`), the `steer` and `queue` delivery modes,
and waking an idle session are already generic over the owner.

What is not generic is who may produce from outside the daemon, and as whom:

- **One identity per plugin.** A plugin produces as `plugin { plugin }`. It
  cannot run several distinct producers, such as a CI watcher and a file
  watcher, each with its own label and its own idempotency namespace.
- **Runtimes cannot produce.** The runtime protocol has no producer methods
  ([runtimes spec](runtimes.md) section 9 deferred them). A remote runtime's
  tool therefore cannot use `reports_itself`: once detached, it has no channel
  to report on.
- **No body bound.** A plugin's body is limited only by the 4 MiB frame.
- **Plugin-only readiness.** Recovery state and the goal-readiness gate know
  only plugins.

After this work, any plugin or runtime can register any number of named
producers, for the sessions it may reach, under one owner kind the daemon
derives from the connection.

**Out of scope:** clients as producers (a later `client:<id>` source fits the
same owner), a model-facing envelope around external bodies, and changes to
the engine's own producers (`delegation`, `goal`, `goal_control`, `agent`,
`tool`, `subagent`).

## 2. The external owner

`ProducerOwner::Plugin { plugin }` is replaced by:

```json
{ "type": "external", "source": "runtime:local", "name": "ci-watcher" }
```

- **`source`** says who is producing. The daemon derives it from the
  authenticated connection, never from request parameters:
  - `plugin:<configured plugin name>` for a plugin;
  - `runtime:<attachment name>` for a runtime, the name the instance reports
    in `runtime/initialize` and sessions attach it under.
- **`name`** is a free-form label the sender chooses when it registers, one
  of `[a-z0-9][a-z0-9_.-]{0,63}`. One source may register under any number of
  names, and several registrations may share a name.

The owner is part of the deduplication key, so two names of one source never
collide on an idempotency key, and a sender that restarts and registers the
same name again replays its earlier sends instead of duplicating them.

The engine's own owners stay as they are. Nothing in the engine, the
projection, or a client branches on the `source` prefix: every external owner
gets the same treatment, and a client shows it as `<source> · <name>`.

**Stored logs.** A stored `plugin { plugin }` owner decodes as
`external { source: "plugin:<plugin>", name: "default" }`, and the plugin
SDK registers under `default` when the plugin names nothing, so a plugin
upgraded across this change keeps deduplicating against its earlier sends.

## 3. Registration and authority

The four producer operations are the same for every source:

| Operation | Params | Result |
|---|---|---|
| register | `session_id`, `name` | `producer_id` |
| send | `session_id`, `producer_id`, `mode`, `idempotency_key`, `description`, `body` | `message_id` |
| unregister | `session_id`, `producer_id` | `{}` |
| discard | `session_id`, `message_id` | `{}` |

- A registration belongs to one connection. Its authority is the connection's
  source and its connection epoch; it ends when the connection does (a plugin
  exits, a runtime instance stops or reconnects), and the sender registers
  again on its next connection.
- Send needs a live registration of the calling connection, as today.
  Everything else about send is unchanged: the receipt follows durable
  acceptance, and an identical retry returns the original `message_id`.
- Discard addresses a message, not a registration: the caller's `source` must
  match the message owner's `source`, whatever the name. The other discard
  rules are unchanged.
- **Body.** The body is sent to the model as is, as today. It must be
  non-empty and at most `[producers] max_body_bytes` (section 6).

## 4. Plugins

Extension protocol 0.0.8:

- `plugin/producer/register` gains `name` (required on the wire; the SDK's
  `register_producer` defaults it to `default`).
- Everything else is unchanged: the plugin declares `producer_messaging`, the
  operator enables `plugins.<name>.producer_messaging`, and a plugin may
  target any session the daemon can load.

## 5. Runtimes

Runtime protocol 4 adds producer messaging to runtimes.

| Change | Message |
|---|---|
| Added | `producer_messaging` (boolean, default `false`) in the `runtime/initialize` result. |
| Added | `producer/register`, `producer/send`, `producer/unregister`, `producer/discard` (runtime → daemon, requests), with the params and results of section 3. |
| Added | `producer/recovery/start` (daemon → runtime, notification) and `producer/recovery/complete` (runtime → daemon, request, `outcome` `ready` or `failed { message }`), as for plugins. |

- **Scope.** A runtime may register for, send to, and discard in only the
  sessions attached to its instance. A session that detaches it, or is
  deleted, drops the runtime's registrations there; a send to a session it is
  not attached to is rejected.
- **Grant.** No configuration switch: attaching a runtime to a session is the
  grant, and the runtime opts in by declaring `producer_messaging`.
- **Recovery.** When a runtime declares `producer_messaging`, the daemon sends
  `producer/recovery/start` after `runtime/initialize`; the runtime restores
  whatever external work it tracks and answers `producer/recovery/complete`.
  Until then its source is `starting` (section 7).
- **Self-reporting tools.** A runtime tool may now detach with
  `reports_itself` and report its own results later as producer messages,
  as `delegate_subagent` does in the daemon.
- On the runtime side, `runtime_api` gains `ProducerChannel` (register,
  send, unregister, discard), which a runtime's tools receive as
  `ToolExecutionContext::producers`; the coding runtime's protocol server
  (`coding_runtime::server`) implements it with these requests when started
  with `ServeOptions::producer_messaging`, and reports its recovery ready at
  once since it tracks no work of its own.
- A runtime may send producer requests only after `producer/recovery/start`;
  earlier ones are rejected.
- A runtime's call that detaches with `reports_itself` is not cancelled when
  the daemon stops waiting for it; it keeps running and reports through its
  producers.

## 6. Configuration

```toml
[producers]
max_body_bytes = 65536
```

| Key | Type | Default | Description |
|---|---|---|---|
| `max_body_bytes` | integer | `65536` | Largest body an external producer may send, in UTF-8 bytes. A larger or empty body fails the send. Must be 1..=4194304. |

The limit applies to external producers only; the engine's own producers and
agent mail (`[messaging] max_body_bytes`) keep their own bounds.

## 7. Readiness

`PluginRecoveryState { plugin, status }` becomes
`ProducerSourceState { source, status }`, with the same statuses (`starting`,
`ready`, `failed`, `disabled`).

- `session.producers` returns `sources`: the state of every producer-capable
  plugin, and of every producer-capable runtime attached to the session.
- A goal's reminders wait until every source in that list is `ready`, where
  today they wait for every producer-capable plugin. A source that fails or
  is disabled records a runless `producer_diagnostic { source, kind, message }`
  (`recovery_failed` or `recovery_disabled`), once per session. The
  `plugin_diagnostic` event stays for the plugin host's other diagnostics; its
  `recovery_failed` and `recovery_disabled` kinds are no longer written.
- A live external registration in a session still holds back goal reminders,
  as a plugin registration does today.
- External messages stay outside the agent-mail caps (`max_pending_per_session`,
  `max_inflight_per_pair`, `max_hops`), as plugin messages are today.

## 8. Client protocol

| Change | Where |
|---|---|
| Changed | `ProducerOwner`: `plugin { plugin }` replaced by `external { source, name }`. |
| Changed | `session.producers` result: `plugin_recovery` replaced by `sources` (`ProducerSourceState`). |
| Added | `producer_diagnostic { source, kind, message }` event for a producer source's failed or disabled recovery. |

Client protocol 43. The TUI labels an external producer `<source> · <name>`
and its notices name the source.

## 9. Phases

Each phase lands as its own commits with every gate green, and is pushed.

1. **External owner and plugins.** The `external` owner and legacy decoding;
   named registrations; source-matched discard; `[producers]
   max_body_bytes`; `ProducerSourceState` and the generalized readiness gate;
   extension protocol 0.0.8 and the SDK; client protocol 43 and the TUI.
2. **Runtimes.** Runtime protocol 4: the capability, the four producer
   requests scoped to attached sessions, recovery, dropping registrations on
   detach and reconnect; the `runtime_api` producer handle; a remote
   self-reporting tool test.
3. **Documentation.** Plugins guide, goal-mode producer sections, the
   protocol, runtime protocol, and events references, the runtimes spec's
   deferral note; this spec marked implemented.

## 10. Decisions

Decided 2026-10-16:

- One generic `external { source, name }` owner for producers from outside
  the daemon, with a daemon-derived source and a sender-chosen name.
- A runtime produces only for the sessions attached to it.
- External bodies reach the model raw, as today; only a size bound is added.
- Clients do not register producers in this work.

Defaults confirmed in review:

- The `name` charset `[a-z0-9][a-z0-9_.-]{0,63}`.
- Stored `plugin { plugin }` owners decode as name `default`, and the SDK
  registers under `default` by default.
- Discard matches the caller's source, not its name.
- Runtimes need no configuration switch; attaching is the grant.
- `[producers] max_body_bytes` defaults to 64 KiB.

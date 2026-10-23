# Client Protocol Changelog

The client protocol's version is `MAJOR.MINOR.PATCH`
(`cookie_agent_protocol::CLIENT_PROTOCOL_VERSION`). From 1.0.0 a minor only
adds and a major breaks; before it, every minor could break the wire (see the
[compatibility spec](../specs/compatibility-v1.md) section 3).

## 1.20.0

- ACP agents ([ACP agents spec](../specs/acp-agents.md) section 15,
  2026-10-23 addendum), all optional members an older client ignores:
  - `acp_session_started` has `steering`: the agent takes messages into its
    running turn (its steering extension). Clients label the Steer button
    as queueing for a session whose agent does not.
  - `acp_session_updated` has `notice` (`severity` `info`, `warning`, or
    `error`, `title`, optional `description`): a notice of the agent's, or
    a compaction it reported; `steered` (`user_inputs`, the
    `user_input_submitted` sequence numbers, and `producer_messages`): input
    delivered into the running turn; and `tool_presentation`
    (`tool_call_id`, `presentation`): a new title, hint, primary argument,
    arguments, and preview for one of the agent's tool calls whose raw
    input arrived after it started.
- The plugin protocol is 1.9.0 for the same event members.

## 1.19.0

- Clients restart the daemon ([engine configuration spec](../specs/engine-configuration.md)
  section 11):
  - New method `daemon.restart` (`when`: `idle`, the default, or `now`),
    answering `scheduled` and `active_runs`, and the notification
    `daemon.restarting` (`when`, `active_runs`, optional `reason`).
  - New errors `restart_unsupported`, for a daemon that cannot relaunch
    itself, and `daemon_restarting`, for a run asked for while an `idle`
    restart waits.
  - The cron skip reason `daemon_restarting`; an older client shows it as
    an unknown skip.
  - A restarting daemon closes client connections with WebSocket close code
    1012 and the reason `restarting`. Behaviour, with no message change: a
    daemon that shuts down now sends the replies and notifications it has
    queued and a close frame before it closes a connection.

## 1.18.0

- `ConfigStatus` (in the results of `config.get`, `config.apply`, and
  `config.reload`) has the optional `diagnostics`: what the applied
  configuration names that does not exist yet, such as a delegation target
  no agent has (an ACP agent whose runtime has not reported it, or a typo).
  The configuration applies anyway; a diagnostic clears once the agent
  appears. An older client does not show them.

## 1.17.0

- ACP agents ([ACP agents spec](../specs/acp-agents.md), phases 1 to 3):
  external coding agents a runtime hosts take a session's or a subagent's
  turns over the Agent Client Protocol.
  - `AgentDescriptor` has the optional `runtime` (`runtime_id`, `agent`,
    `available`) for an agent a runtime hosts, listed as
    `<runtime>-<agent>` with the synthetic model `acp/<agent ID>`. An older
    client lists it as any agent.
  - New events `acp_session_started` (the agent's ACP session: agent,
    runtime, ACP session ID, cwd, protocol version, agent info, and whether
    it was new, resumed, or loaded) and `acp_session_updated` (the agent's
    `commands`, `options`, and `plan`). The plugin protocol, 1.8.0, carries
    them too; an older reader skips them.
  - The display block `plan` (`entries` of `content`, `status`, and an
    optional `priority`), which an older client receives as text; it is also
    part of runtime protocol 1.7.0 and plugin protocol 1.8.0.
  - The tool source `acp` (`agent`, `runtime_id`) of the calls an ACP agent
    reports. An older reader shows it as an unknown source.
  - New method `session.agent_option.set` (`session_id`, `option_id`,
    `value`), answering the session's agent `options`.
- Runtime protocol 1.7.0 adds the agents' declarations, relay, and MCP
  bridge (see the [runtime protocol](runtime-protocol.md#acp-agents)).

## 1.16.0

- MCP tool filters ([MCP tool filters spec](../specs/mcp-tool-filters.md)):
  - `McpServerDefinition` (in `mcp.server.list`, `mcp.server.add`,
    `mcp.server.edit`, and their results) has the optional `include_tools`
    and `exclude_tools`, pattern lists over the server's own tool names. An
    older client that edits a server sends a definition without them, which
    clears them, as it would any member it does not know of a command.
  - `McpServerInfo` has the optional `filtered_tools`: how many of the tools
    the server lists its filters left out, absent when none were. Runtime
    protocol 1.6.0 adds the same member to an MCP server report, so a
    runtime's servers say it too. The plugin protocol and history format 1
    are unchanged.
- Behaviour, with no message or field change: an MCP tool is offered only
  when the run's merged rules do not deny `mcp` for its published name on its
  runtime, where any `mcp` `allow` or `ask` rule used to offer every MCP
  tool. Agents with narrow `mcp` rules see fewer tools, and a session
  override changes the list at the next model request. Other actions stay
  offered by action.

## 1.15.0

- Cron jobs ([cron spec](../specs/cron.md), phase 1): the daemon keeps
  scheduled prompts in `crons.json` and fires them into a session or into a
  new session per fire.
  - New methods `crons.list`, `crons.create`, `crons.update`,
    `crons.delete`, and `crons.run`, and the notification `crons.changed`;
    new errors `cron_not_found`, `cron_invalid`, and `cron_store_failed`.
  - `SessionMeta.cron` (`{ job_id, fired_at }`), also the optional `cron`
    member of `session_created`, names the job whose fire created a run
    session. A reader that does not know it shows an ordinary session.
  - `WaitingOn::ScheduledJob` (`scheduled_job`): an enabled job with a next
    fire targets the session. A 1.14 client reads it as an unknown producer
    (`Unknown`), still something the session waits on.
  - A fire's prompt is a producer message whose `producer_owner` is
    `external` with `source` `cron:<job id>` and `name` `scheduled`: the
    closed `ProducerOwner` keeps its shape, and the source kind `cron` is
    the daemon's own, which no plugin or runtime connection has.
  - The built-in permission action `cron` joins `PermissionAction::BUILT_IN`,
    so the effective permissions list it.
  - Plugin protocol 1.7.0 too, since plugins receive events and
    `SessionMeta`. History format 1 and the runtime protocol are unchanged.
- Behaviour, with no message or field change and no version bump
  (2026-10-23, [permission consolidation](../specs/permission-consolidation.md)):
  a session's permission overlay merges with the agent's frozen rules into one
  list. An overlay rule replaces the agent rule with the same action, runtime
  selector, and pattern, and the most specific matching rule decides a call,
  so a broad overlay rule no longer beats a narrower agent rule. An overlay
  `deny` that replaces every allowing rule now hides the action's tools.
  `session.permission.get` (and the results of `session.permission.set`,
  `session.permission.clear`, and `session.permission.preview`) keeps listing the agent's
  patterns under an overlay `*` rule, each with its source, where it used to
  drop them. Recorded approvals and decisions replay as they were written.

## 1.14.0

- Memory that stays worth keeping ([memory spec section 16.9](../specs/memory.md#169-addendum-2026-10-22-memory-that-stays-worth-keeping)):
  - A new audit event, `memory_changed { change, url }`, records each
    write, edit, or delete a model's memory tool makes; `change` is the open
    enum `MemoryChangeKind` (`saved`, `edited`, `deleted`; an unknown value
    reads as a change). A transcript shows it as one line, such as `Saved
    project://cookie-agent/release`. Plugin protocol 1.6.0 too, since
    plugins receive events.
  - `memory_loaded` records when the snapshot was taken (`taken_at`), and
    each memory it shows, whole or as an index line, carries the optional
    `modified`, `size`, and `max_size` (`MemoryDocumentStatus`, flattened
    into `MemorySnapshotCategory`, `MemorySnapshotMemory`, and
    `MemorySnapshotIndexEntry`); a `size` above `max_size` marks a memory
    over its limit. A snapshot without `taken_at` was taken by an earlier
    daemon, and a run takes a new one.
  - `memory.list` entries (`MemoryInfo`) carry the optional `created`,
    `modified`, `last_read`, and `max_size`; `memory.read` (`MemoryReadResult`)
    carries `created`, `modified`, `size`, and `max_size`. `memory.write`
    and `memory.delete` keep their revisions. A `memory.write` that would
    leave a memory over its limit is `memory_refused`.
- A 1.13 client ignores the new members and skips `memory_changed`, which
  is marked skippable; a 1.14 client reading a 1.13 daemon sees no stamps
  or limits. History format 1 is unchanged.

## 1.13.0

- Runtime-scoped MCP servers (`scope = "runtime"` on a runtime template's
  server; runtime protocol 1.5.0): `McpServerInfo` has the optional `scope`,
  `runtime` or `workspace` (an open enum; an unknown value reads as
  `workspace`), for a server a runtime other than `global` hosts.
  `mcp.server.list` lists a runtime-scoped server once, without a
  `workspace`, with its live state, and, while no instance of a connected
  runtime hosts it, a workspace-scoped server without a `workspace`,
  `disconnected` with the message "starts when a workspace opens", so a
  client shows a runtime's servers before any session opens a workspace of
  it. `mcp.server.reconnect` without a `workspace` reconnects a
  runtime-scoped server; for a waiting workspace-scoped one it fails. A
  runtime-scoped server's failure names no `workspace` and reaches the
  sessions attached to any instance of its runtime.
- A 1.12 client ignores `scope` and shows a runtime-scoped server as one its
  runtime hosts without a workspace, and a waiting one as `disconnected`
  with its message; a 1.13 client reading a 1.12 daemon sees no scope.

## 1.12.0

- `mcp.server.list` lists the MCP servers a dial-in runtime hosts from
  `[runtimes.local.mcp]` on its own machine, which earlier daemons left
  out because they do not see those definitions. Such a server has the new
  `McpConfigSource` value `machine` (an open enum value) and an empty
  `definition` (no `command` or `url`); its `state`, `tool_count`, and
  `message` are live as for any server. A failure of one is now also among
  the failures a client hears when it subscribes to a session.
- A `memory_loaded` snapshot holds only what the run's `memory` rules let
  it read ([memory spec section 7.5](../specs/memory.md#75-addendum-2026-10-22-the-snapshot-follows-the-read-rules)):
  a memory they allow is shown as before, one they ask for is listed by URL
  and description only, in the new `MemorySnapshot.user_listed` (user
  categories) and `MemorySnapshotAgent.listed` (the agent's file; a project
  overview they ask for joins the project's `memories`), and one they deny
  or do not match is left out and not counted. `MemorySnapshot.rules`, a
  fingerprint of the rules it was taken under, makes a run whose rules
  changed take a new snapshot. All three are optional; plugin protocol
  1.5.0 too, since plugins receive events. History format 1 is unchanged;
  older logs read as having none.
- A 1.11 client reads `machine` as `runtime` (a server `mcp.server.*`
  cannot edit); a 1.12 client reading a 1.11 daemon sees no dial-in
  servers, as before; a 1.11 client ignores the snapshot's new members.

## 1.11.0

- MCP OAuth from the web client ([spec](../specs/web-mcp-oauth.md)):
  `mcp.auth.begin` accepts an `https` `redirect_uri` (a host, no user info,
  no fragment, any path and query) and plain `http` to `localhost` as well as
  to a loopback IP literal, with or without a port, path, or query. The web
  client uses its own page with `?oauth=callback`. A dynamic registration
  is made for each flow's redirect (`application_type` `web` for an `https`
  one, `native` for loopback); a configured `client_id` is used as is.
- `mcp.auth.complete` also requires the redirect's own query parameters
  (such as `oauth=callback`) in the `callback_url`.
- No wire type changes. A 1.10 daemon refuses the new redirects, so a client
  offers them only to a 1.11 daemon; a 1.10 client sees nothing new.

## 1.10.0

- The machine registry remembers where runtimes ran
  ([memory spec section 16.8](../specs/memory.md#168-addendum-2026-10-21-the-registry-remembers-where-runtimes-ran)):
  a runtime that is not connected keeps the machine it was last seen on, by a
  new `assigned_by` reason, `last_seen` (an open enum value). `MachineInfo` in
  `machines.list` gains `remembered`, the offline runtimes it remembers
  (`RememberedRuntime`: `runtime_id`, `assigned_by`, `platform`, `home`,
  `last_seen`). `runtimes.list` gives such a runtime its remembered `machine`,
  `platform`, and `home`.
- `machines.forget_runtime { runtime }` drops an offline runtime's record and
  answers with the machines afterwards, with the error codes
  `runtime_not_remembered` and `runtime_still_connected`. `machines.forget`
  no longer counts remembered offline runtimes, and drops their records.
- Delegating to runtimes ([spec](../specs/runtime-delegation.md)):
  `SessionMeta` gains `delegated_attachments`, a root session's record of the
  attachments its subagents brought, folded from the new runless state event
  `delegated_attachments_added { child_session_id, attachments }` (plugin
  protocol 1.4.0 too). `sessions.changed` `upserted` announces it. History
  format 1 is unchanged; older logs read as having none.
- `list_runtimes` joins the built-in permission actions that
  `session.permission.get` lists. The `delegate_subagent` and `list_runtimes`
  tool changes are model-facing and need no wire change.
- A 1.9 client ignores the new members and event, and sees offline runtimes
  as before; a 1.10 client reading a 1.9 daemon sees no remembered runtimes
  and no delegated attachments, and calling `machines.forget_runtime` gets
  `method_not_found`.

## 1.9.0

- `SessionMeta` gains `waiting_on`: what will start the session's next run
  without the user, each kind once, omitted when nothing will. Kinds
  (an open enum `WaitingOn`): `goal` (the root's goal is active; folded from
  the goal events, so the metadata cache keeps it), `background_call` (a
  background call is running), `subagent` (a direct child runs a turn on its
  own, or a background delegation is queued or running), and
  `external_producer` (a plugin or runtime holds a live producer
  registration for the session). The last three are the daemon's live state,
  never cached, and end with it. It is reported whatever `status` says;
  clients show a running session as running first (see
  [What a session waits on](protocol.md#what-a-session-waits-on)).
- `sessions.changed` `upserted` announces every change of `waiting_on`: a
  goal activated, paused, resumed, completed, or cancelled, a background
  call detached or reported, a child's turn started or ended, a registration
  made or dropped.
- A 1.8 client ignores the member; a 1.9 client reading a 1.8 daemon sees no
  session waiting on anything.

## 1.8.0

- Projects: `projects.list`, `project.get`, `project.create`,
  `project.update`, and `project.delete` manage named bodies of work, each
  with an ID, a name, an optional emoji, default attachments and agent, an
  `AGENTS.md`, and a memory folder; `projects.changed` announces every
  change (see [Projects](protocol.md#projects)). They come with the error
  codes `project_not_found`, `project_name_taken`, `project_changed`,
  `project_invalid`, `project_sessions_running`,
  `project_root_session_only`, and `project_store_failed`.
- Sessions in projects: `session.create` takes an optional `project`, whose
  default runtimes the session attaches when the creation omits `runtimes`;
  `session.project.set` moves a root session into a project or out of one;
  `session.list` takes an optional `project` filter (an ID, or `"none"`);
  `SessionMeta` gains `project`.
- Events (plugin protocol 1.3.0 too): the state event `session_project_set`,
  `memory_loaded`'s optional `project` (the overview and an index of the
  project's memories), and `agent_md_loaded`'s optional `project`, which
  names a project's `AGENTS.md`. History format 1 is unchanged; older logs
  read as having no project.
- A `project://` memory write, from `memory.write` as from a model, is
  `memory_refused` unless the project exists; the first write no longer
  creates the project. A project's `overview` memory keeps the daemon's
  description, reported in `memory.write`'s `notice`.
- A 1.7 client ignores the new members and notification, and sees sessions
  without projects; a project's `AGENTS.md` and memory still reach its
  sessions' prompts, which the daemon renders. A 1.8 client calling a project
  method on a 1.7 daemon gets `method_not_found`.

## 1.7.0

- Paths and runtimes: `SessionMeta` gains `attachments`, the session's
  runtime attachments now (`RuntimeAttachment`: `name`, `runtime_id`,
  `capability`, `kind`, `workspace`), kept current on attach and detach,
  which also announce the session with `sessions.changed` `upserted`; session
  lists can show and filter sessions by runtime and workspace without a
  `session.get` each. `RuntimeListing` in `runtimes.list` gains `home`, the
  home directory the runtime reported (runtime protocol 1.3.0), which clients
  show the runtime's paths under as `~/…` (see [Runtimes](protocol.md#runtimes)).
  A 1.6 client ignores both; a 1.7 client reading a 1.6 daemon sees no
  attachments and no homes, and shows paths absolute.
- A new root session's `cwd_identity` is decided at creation, once its
  attachments have opened: the canonical workspace its first attachment with
  a workspace reported, else `cwd` canonicalized on the daemon's machine (as
  given when that fails). A client's `~/x` and another's `/home/ada/x`
  attaching one workspace now give one identity. Sessions created before keep
  the identity they stored.

## 1.6.0

- `memory.write` results carry an optional `notice`: what the daemon kept in
  place of the text it was given. The daemon now keeps a memory file for
  every agent and owns its description and two keywords, so a write of an
  `agent://` file that changes them keeps the daemon's and says so (see
  [Memory](protocol.md#memory)). A 1.5 client ignores the member.

## 1.5.0

- Session deletion: `session.delete { session_id, expected_last_seq }`
  deletes a root session's whole tree from disk and returns the `deleted`
  session IDs, the root first; every connection gets `sessions.changed`
  `removed` for each (see [Delete](protocol.md#delete)). It comes with the
  error codes `not_a_root_session`, `session_tree_running`, and
  `session_changed`, besides the existing `session_not_found` and
  `session_owned_elsewhere`. A 1.4 client sees only the `removed` changes; a
  1.5 client calling `session.delete` on a 1.4 daemon gets
  `method_not_found`.

## 1.4.0

- Memory for clients: `memory.list`, `memory.read`, `memory.write`, and
  `memory.delete` list, read, and change the daemon's memory files for the
  user, with no session (see [Memory](protocol.md#memory)). They come with
  the error codes `memory_url_invalid`, `memory_not_found`, `memory_exists`,
  `memory_revision_mismatch`, `memory_invalid`, `memory_refused`,
  `memory_busy`, and `memory_store_failed`. The `memory.changed`
  notification reaches every connection when a memory file changes through
  the daemon. A 1.3 client sees none of it; a 1.4 client calling a
  `memory.*` method on a 1.3 daemon gets `method_not_found`.

## 1.3.0

- A new state event, `memory_loaded`, records the memory snapshot a
  session's model sees from a run on: user categories, the run's agent's
  memory, and an index of the session's machines' memories (see
  [Events](events.md#memory-snapshots)). An event without `agent` records
  that memory is no longer available. A 1.2 client reads it as an unknown
  state event. The plugin protocol is `1.2.0` for the same event.
- `memory` and `memory_write` join the built-in permission actions that
  `session.permissions.get` lists.

## 1.2.0

- Machines: the computers runtimes run on. `machines.list`,
  `machines.rename`, `machines.assign`, `machines.unassign`,
  `machines.merge`, and `machines.forget` read and change the daemon's
  machine registry (see [Machines](protocol.md#machines)), with the error
  codes `machine_not_found`, `machine_name_taken`, `machine_in_use`,
  `machine_memory_conflict`, `machine_merge_self`, `runtime_not_pinned`, and
  `machine_store_failed`. The `machines.changed` notification reaches every
  connection when the registry changes, and each `runtimes.list` entry
  gains `machine`. A 1.1 client sees none of it; a 1.2 client calling a
  `machines.*` method on a 1.1 daemon gets `method_not_found`.

## 1.1.0

- A new audit event, `record_omitted { omitted_type }`, stands in for a record
  its writer deliberately did not carry over, at that record's sequence, run,
  and time. A fork writes one for each of its source's `delegation_reserved`,
  `delegation_progressed`, and `delegation_finished` records: the fork keeps
  the conversation but not the subagents, which stay the source's (see
  [Events](events.md#delegation)). It is skippable and changes nothing a
  reader projects; a 1.0 client reads it as an unknown event. The plugin
  protocol is `1.1.0` for the same event.

## 1.0.0

The client protocol is frozen: every later 1.x version reads and is read by
this one. A `0.x` build never interoperates with a 1.x one: the handshake
refuses another major with `-32002` `unsupported_protocol`, so a client and a
daemon from before the freeze must be upgraded together. The plugin and
runtime protocols froze at `1.0.0` too, and session history is history
format 1.

What 1.x keeps:

- **Versions.** The handshake carries `"MAJOR.MINOR.PATCH"` strings and
  connects equal majors; each side sends a method, a member, or a display
  block kind only when the other's minor includes it, and the patch never
  matters.
- **Information is tolerant, commands are strict.** Results, notifications,
  events, and the JSON-RPC envelope ignore members a reader does not know,
  and their enums are open, read as a documented fallback or as `unknown`;
  an event a client cannot read arrives as an unknown event, and a
  notification that does not decode fails no call. Params refuse an unknown
  member (`unknown_field`) or value (`unknown_variant`) with its JSON path.
- **Errors.** Every application error is `-32000` with a registry `data.code`,
  a readable `message`, an optional `hint`, and `detail` on request (see
  [Errors](errors.md)); clients branch only on `data.code`.
- **User content and media.** Inputs are `content`: text and file parts
  naming artifacts uploaded with `artifact.upload.*`, admitted against the
  `input_media` each model of the catalog view advertises, and read back with
  `artifact.read`.
- **Tool display.** Tool calls carry presentation v2 (title, hint, tool
  identity, argument and preview blocks), progress display deltas, and result
  display blocks of twelve kinds with a canonical text rendering; a connection
  receives only the kinds its version includes. A result always carries its
  whole `display`.
- **The catalog view.** `runtime.snapshot.get` and `runtime.changed` carry a
  `ModelCatalogView` with one opaque revision.

Changed since 0.52.0: a tool result's `display` is always whole and
required; `display_span` belonged to the session log's storage encoding and
no longer reaches clients.

## 0.52.0

- Every model of the catalog view states `input_media`: the kinds it accepts
  in user input, with their media types and effective byte, dimension, pixel,
  and count limits.
- `run.start`, `run.steer`, and `session.submit_input` take `content` (text
  and file parts) in place of `input`, and a `recalled` outcome of
  `session.recall_input` returns `content`.
- `artifact.upload.begin`, `artifact.upload.chunk`, `artifact.upload.finish`,
  `artifact.upload.cancel`, and `artifact.read` upload media into a session
  tree's store and read any of its artifacts back; new error codes
  `upload_offset_mismatch`, `upload_size_mismatch`, and `too_many_uploads`.
- Input file parts are admitted against the model the input reaches:
  `media_unsupported`, `media_type_unsupported`, `media_too_large`,
  `media_limit_exceeded`, and `artifact_not_found`.

## 0.51.0

- The handshake's `protocol_version` is a semver string. A client of another
  major or, before 1.0, another minor is refused with `-32002`
  `unsupported_protocol`, as is a build that sends the old integer.
- The handshake takes an optional `token` and `features`, and its reply
  carries `engine_version` and `features`. The WebSocket upgrade no longer
  requires `Authorization`: a connection without it authenticates with the
  handshake's `token`, and gets `-32003` for anything else until it does.
- `[server] allowed_origins` lets web pages of listed origins connect.
- Results, notifications, and the JSON-RPC envelope are information: a
  reader ignores members it does not know, and every enum in them is open,
  read as a documented fallback or as `unknown` (see
  [Decoding](protocol.md#decoding)). Params stay strict.
- Every application error is `-32000` with a registry `data.code`, a
  `message` and `hint` from the registry, and the cause as `data.detail` (see
  [Errors](errors.md)). The old codes `-32011`, `-32012`, `-32020` to
  `-32022`, `-32030`, `-32031`, and `-32040`, the several meanings of
  `-32602` for refusals, the typed error data (`RunStartConflict`,
  `SessionRenameError`, `SessionRevertConflict`, `ApprovalRespondError`,
  `ProviderConnectError`, `ProviderDisconnectError`, `ConfigApplyError`), and
  `debug_code` are gone.

- `runtime.snapshot.get` and `runtime.changed` carry a `ModelCatalogView`
  (`revision`, `catalog`, `providers`, `models`, `agents`) in place of the
  runtime snapshot: one opaque revision replaces the snapshot schema version
  and the eleven revisions and counters, and descriptors lose their
  fingerprints. `provider.connect` and `provider.disconnect` take that
  revision as `expected_revision`; the disconnect receipt keeps its ID and
  time only. `runtime.changed` takes any number of reasons.
- `SessionMeta.short_id` is required (nullable).
- The daemon sends a connection only the display-block kinds its version
  includes, downgrading newer kinds to `text` and leaving out kinds it does not
  know.

## Before semver versions

Builds before 0.51.0 sent the protocol version as an integer, from 34 to 50;
the last of them was 50. They cannot connect to this build.

# Client Protocol Changelog

The client protocol's version is `MAJOR.MINOR.PATCH`
(`cookie_agent_protocol::CLIENT_PROTOCOL_VERSION`). From 1.0.0 a minor only
adds and a major breaks; before it, every minor could break the wire (see the
[compatibility spec](../specs/compatibility-v1.md) section 3).

## 1.95.0

- Threads for new-session cron jobs ([webhooks spec](../specs/webhooks.md)
  section 8): `CronTemplate` gains `thread_key`, a time format whose key
  routes each fire into one session per key, and `thread_prompt`, sent to
  each session the job creates as a plain user message, again after each
  compaction; `CronJob` gains `threads` (`CronThread { key, session_id,
  last_used }`), `CronFire` and `SessionCron` gain `thread_key`, and
  `delivery` is taken by a new-session job with a thread key. A thread
  prompt's `user_input_submitted` has the origin `engine:cron-<job id>`
  (`thread_prompt_origin`, `thread_prompt_owner`). Plugin protocol 1.47.0
  carries the new members.

## 1.94.0

- `AssetThumbnailResult` gains `retry_later` (false when absent): without a
  `thumbnail`, asking again later may give one, because the tool that draws
  it was missing or too slow, or the content went while it was read. When
  false, the content never has one, and a client may remember that by its
  `sha256` ([Asset thumbnails](../specs/asset-thumbnails.md) section 5).

## 1.93.0

- `asset.thumbnail { store, asset }` returns `sha256`, the asset's current
  content, and `thumbnail?`, an `AssetThumbnail` (`media_type`, base64
  `data`, `width`, `height`): a JPEG, or a PNG for a picture with
  transparency, whose longer side is at most 768 pixels
  (`ASSET_THUMBNAIL_MAX_EDGE`). The daemon makes it of PNG, JPEG, GIF, and
  WebP pictures itself, of a video's first frame with `ffmpeg`, and of a
  PDF's first page with `pdftoppm`, each when it is on the daemon's `PATH`,
  and keeps it with the content. Other kinds have none
  ([Asset thumbnails](../specs/asset-thumbnails.md)). A guest of a
  conversation share may ask for the assets it may read.

## 1.92.0

- A new state event, `reviewer_turn_recorded { turn }`: a turn of the approval
  reviewer's conversation, with the `user` turn and the verdict as they were
  sent, in the reviewer's own log, a hidden child session filed in the reviewed
  session's folder ([Sessions that run forever](../specs/forever-sessions.md)
  section 5.5). No session a client lists or reads holds it, so clients never
  receive it. Plugin protocol 1.46.0 carries it in the event schema.

## 1.91.0

- A new audit event, `system_prompt_composed { composed_prompt,
  prompt_fingerprint, blocks? }`: a checkpoint inside a run composed the run's
  system prompt afresh, and the run's requests carry it from there instead of
  its `run_started` agent's `composed_prompt`
  ([Sessions that run forever](../specs/forever-sessions.md) section 5.2). The
  display view's header holds the latest run's latest one. Plugin protocol
  1.45.0 carries it in event notifications.

## 1.90.0

- `ContextCheckpointCommit` gains an optional `context_fresh`: the checkpoint
  recorded `AGENTS.md` and the memory snapshot again just before itself, and
  a native model's context pins nothing from before its kept range, loaded
  skills included ([Sessions that run forever](../specs/forever-sessions.md)
  section 5.2). Plugin protocol 1.44.0 carries it in event notifications.

## 1.89.0

- `session.runtimes.list` (`SessionRuntimesListParams`, answered with a
  `RuntimesListResult`): the runtimes one session works on, without what describes
  the daemon beyond it, which a guest may read in place of `runtimes.list`
  ([The machine on a tool call](../specs/tool-call-machines.md)).

## 1.88.0

- Conversation shares ([spec](../specs/conversation-shares.md)):
  `session_share.create` and `session_share.update`
  (`SessionShareCreateParams`, `SessionShareUpdateParams`,
  `SessionShareResult`, `SessionShare`); `sessions` on `SharesListResult` and
  `session` on `SharesListParams`; `ServerHello.guest` (`GuestHello`); and the
  `not_allowed_for_guest`, `shared_session_not_root`, and
  `invalid_share_label` errors. See [Conversation shares](protocol.md#conversation-shares).

## 1.87.0

- `machines.runtime_token` (`MachinesRuntimeTokenParams`,
  `MachinesRuntimeTokenResult`): the token runtimes that dial in present on
  `/runtime`, which `/ws` refuses
  ([The runtime token](../specs/runtime-token.md)).
- `mcp.server.reconnect` of a top-level server that wants a sign-in answers
  with the server, `needs_auth`, rather than an error.

## 1.86.0

- Share links ([spec](../specs/share-links.md)): `shares.list`,
  `share.create`, `share.update`, `share.revoke`, `shares.revoke_all`, and
  the `shares.changed` notification; `Share`, `ShareCreated`, and
  `ShareExpiry`; `ServerHello.share_url`; `deleted_shares` on
  `project.delete`; the `share_not_found` and `shares_disabled` errors; and
  the built-in `share` permission action. See
  [Share links](protocol.md#share-links).

## 1.85.0

- MCP servers connect on demand
  ([Session tool groups](../specs/session-tool-groups.md) section 10):
  `McpServerState` gains `stopped`, a server no session wants, listed with
  the tools of its last connection.

## 1.84.0

- `runtimes.groups` (`RuntimeGroupsParams`, `RuntimeGroupsResult`,
  `RuntimeToolGroup`, `RuntimeSkill`): what a runtime capability brings at a
  workspace, live or as of the last time it ran
  ([Session tool groups](../specs/session-tool-groups.md) section 4.4).

## 1.83.0

- Tools sent up front that leave mid-round are announced
  ([Session tool groups](../specs/session-tool-groups.md) section 8.2): the
  `tools_availability_announced` event (`unavailable` and `available_again`,
  lists of `UnavailableToolGroup { name, source, machine?, available_on,
  tools }`), and `RoundToolDefinition` gains `group` (`RoundToolGroup`) and
  `machines`.

## 1.82.0

- Session tool groups and skills
  ([spec](../specs/session-tool-groups.md)): `session.tools.get` and
  `session.tools.set`; `ToolExposure` (`tool_groups` of
  `ToolGroupRule { selector, state, from_project? }`, `skills` of
  `SkillRule { skill, state, from_project? }`), `ToolGroupSelector`,
  `ToolGroupState` (`on`, `deferred`, `off`), `SkillState`, and
  `ExposureLayer`; `session.create`'s optional `exposure`; a project's
  `defaults.exposure`, copied into its new sessions; the
  `session_tool_exposure_set` event; and `AgentSnapshot.exposure`, the
  agent document's `tool_groups` and `skills`. See
  [Tool groups and skills](protocol.md#tool-groups-and-skills).
- Skills take no permission: the built-in `skill` action is gone, with its
  default (`skill: allow`), and the `skill` tool needs no rule.
  `SkillDescriptor.permission_effect` is always `allow`; `visible` follows
  the session's skill choices. `session.permission.set` still accepts a
  rule for an action named `skill`, which governs nothing.
- Built-in tool groups: `webfetch` moved from `coding` to a group of its
  own, `web`.
## 1.81.0

- Plugin settings ([Plugin web pages and settings](../specs/plugin-web-ui.md)
  section 5): `plugins.settings.get` and `plugins.settings.set`; `PluginInfo`
  gains `has_settings` and `settings_missing`; the elicitation text input
  gains `secret`.

## 1.80.0

- Plugin pages ([Plugin web pages](../specs/plugin-web-ui.md)):
  `ServerHello` gains an optional `page_ticket`, which the web page keeps in
  its `cookie_page_ticket` cookie to open the plugin pages the daemon serves
  at `/plugins/<name>/`; `PluginInfo` gains an optional `ui_path`, where a
  connected plugin's pages are (`plugins/<name>/`, relative to the page).

## 1.79.0

- `session.fork` cuts as `session.revert` does: a run the prefix would
  leave with nothing but its `run_started` is left out, so a fork to just
  before a message leaves out the run it starts instead of copying it as
  an interrupted run. The prefix no longer needs a submitted user message,
  so a session forks before its first message too. The TUI and the web
  client fork at a user message this way, putting its text in the fork's
  composer, and revert and fork at the end of a reply and before a
  compaction ([Sessions](../sessions/index.md#revert)).

## 1.78.0

- Compaction before the prompt cache expires
  ([Cache-aware idle sessions](../specs/cache-aware-idle.md) section 4):
  `model_usage_recorded` gains an optional `cache_ttl` (`CacheTtlStamp`:
  `ttl_secs`, `0` never expiring, and an open `CacheTtlSource`, `set`,
  `declared`, or `assumed`); `ContextCheckpointCommit` gains an optional
  `reason` (an open `CheckpointReason`, `cache_expiry` with `expires_at`,
  `next_fire`, and `job_id`); `SessionMeta` gains the live
  `cache_compaction` (`PlannedCacheCompaction`: `at`, `expires_at`,
  `next_fire`, `job_id`, `running`); and the engine diagnostic kind
  `cache_expiry_compaction`.
- `AcpAgentDefinition`, `AcpPreset`, and `OnboardingAcpAgentInput` gain an
  optional `cache_ttl`, the declaration's prompt cache lifetime.

## 1.77.0

- `AgentsUpdateResult` gains `templates` (`OfficialTemplate`: `name`,
  `version`), `updated_templates`, and `removed_templates`: the official
  templates the refresh kept and changed, named without their `_` (the
  `_acp` template is `acp`). A template's `_<name>` is no agent ID, so
  before this the answer left templates out.

## 1.76.0

- `EventsSubscribeResult` gains `view_events` on the first page (no
  `cursor`) of a `limit`ed request that has more: how many events the asked
  `view` holds in all, for a client that reads a session's whole history
  page by page to count its progress against. The web client now opens a
  session whole, in the display view, and shows how many of its events
  have loaded.

## 1.75.0

- The agents panel
  ([Onboarding with ACP agents](../specs/acp-onboarding.md) section 7):
  `agents.documents` lists every agent document as an
  `AgentDocumentInfo` (open `AgentDocumentKind` and
  `AgentDocumentOrigin`); `agents.customize`, `agents.set_enabled`,
  `agents.reset`, and `agents.replace` change the user's document of an
  agent (`AgentDocumentParams`, `AgentsSetEnabledParams`), each answering
  its `path` (`AgentDocumentResult`).

## 1.74.0

- Onboarding with ACP agents
  ([Onboarding with ACP agents](../specs/acp-onboarding.md) sections 2, 5,
  and 9): `OnboardingStatus` gains `paths` (`OnboardingPaths`: the models
  path's `complete` and `missing`, the ACP path's `complete` and every
  declaration as an `OnboardingAcpAgent`), `acp_presets` (`AcpPreset`),
  `mcp_presets` (`McpPreset`, its place an open `McpPresetPlace`),
  `mcp_servers` (`OnboardingMcpServer`), and `default_agent`. While the ACP
  path is complete, `missing` no longer names `provider` or
  `default_model`.
- `OnboardingRunParams` gains `acp_agents` (`OnboardingAcpAgentInput`),
  `mcp_servers` (`OnboardingMcpServerInput`), and `default_agent`.
- `acp.check { runtime_id, agent }` starts a declared ACP agent and opens a
  session to see it works: an `AcpCheckResult` with an open
  `AcpCheckStage`.

## 1.73.0

- Attachments in assistant messages
  ([spec](../specs/message-attachments.md)): the event
  `assistant_attachment` (`model_turn_seq`, `content_index`, `ordinal`,
  `tag` an `AttachTag`, `outcome` an `AttachmentOutcome`), and the methods
  `attachment.decide` and `attachment.save_asset`. Plugin protocol 1.38.0
  carries the event.
- The `show_asset` tool and `write_asset`'s `show` and `caption` are gone:
  agents show assets with `<attach asset="…"/>`. The `asset` display block
  and the `figure` hint stay, for calls older builds recorded.

## 1.72.0

- Why agents cannot run
  ([Onboarding with ACP agents](../specs/acp-onboarding.md) section 8):
  `AgentDescriptor` gains `unavailable`, an `AgentUnavailable` (`reason`, an
  open `AgentUnavailableReason`; `target`; `entries`, each an
  `AgentModelEntryStatus` with an open `AgentModelProblem`). The agent
  listing also holds documents that are no agent now: an ACP document whose
  declaration no runtime has, and one whose `inherit` target does not exist
  yet, never runnable.
- `agent_unavailable` gains the optional field `reason`.

## 1.71.0

- Assets across stores ([Assets](../specs/assets.md) section 8.3):
  `assets.search { project?, scope? }` returns the assets of many stores,
  newest first, with `sessions`, an `AssetSession` (`session`, `title?`,
  `project?`) for each session tree whose own assets it lists. `project` is
  an `AssetProjectFilter` (`project` or `no_project`), every one when
  absent; `scope` an `AssetScope`, both when absent.

## 1.70.0

- Where an ACP agent runs
  ([ACP agents across several machines](../specs/acp-several-runtimes.md)
  section 3): `RunSelection` gains `acp_runtime`, the runtime the user
  chose to run the selected ACP agent on, and `CronTemplate` the same for a
  job's runs. A session or a selection naming one that it does not attach
  attaches it at the runtime's home before the agent starts.
- Several attached runtimes may offer one ACP agent: it runs on the runtime
  its ACP session started on, else the chosen one, else the first attached
  that offers it and can run it. A session no longer refuses a second
  runtime that offers it; detaching the runtime the agent started on is
  refused, and so is a `session.select_model` naming another runtime for
  it.
- Plugin protocol 1.37.0 carries `acp_runtime` the same way.

## 1.69.0

- Fallback lists for the model fields
  ([fallback lists](../specs/model-fallback-lists.md)): `OnboardingStatus`
  gains `default_model_list`, `small_model_list`, and `utility_model_list`,
  each field as configured, entries `OnboardingModelEntry`
  (`{ model?, field?, variant? }`, exactly one of `model` and `field`,
  `field` an open `OnboardingModelField`); `default_model`, `small_model`,
  and `utility_model` there are the first model each field reaches.
  `onboarding.run` takes the same three lists, refused with
  `invalid_params` beside the same field's single value.
## 1.68.0

- ACP agents' own permission options on approvals
  ([ACP agent permissions](../specs/acp-permissions.md) section 4):
  `ApprovalRequest` gains `agent_options`, the options an ACP agent's
  harness offered with its permission request (`option_id`, `name`, and
  `kind`: `allow_once`, `allow_always`, `reject_once`, or `reject_always`,
  an open set read as `reject_once`), in its order. `approval.respond`
  takes `agent_option`, one of their IDs, whose kind must agree with the
  decision (`approve_once` for an allowing one, `reject` for a rejecting
  one), and the `user_decision` approval step records it. An answer
  without one gets the agent its allow-once or reject-once option.
- An ACP session's approvals no longer depend on the tree's permission
  mode: what cookie's rules leave open always waits for the user, and
  carries no tree grant. A rejection's `feedback` is steered into the
  agent's turn as the user's input.
- Plugin protocol 1.36.0 carries the members the same way.

## 1.67.0

- Assets ([spec](../specs/assets.md); see [Assets](protocol.md#assets)):
  `assets.list`, `asset.get`, `asset.read`, `asset.rename`, and
  `asset.delete` over a `store` (`session`, `project`, or `shared`), and the
  `assets.changed` notification.
- The `asset` display block, a figure of an asset, and the `figure`
  presentation hint. Older clients receive the block as text.
- `ServerHello` gains `page_url`, the daemon's `[server] page_url`.
- `project.delete` returns `deleted_assets`.
- The plugin protocol (1.35.0) and the runtime protocol (1.15.0) share the
  `asset` display block.

## 1.66.0

- Pinned sessions ([session pins and filters](../specs/session-pins-and-filters.md);
  see [Pinned sessions](protocol.md#pinned-sessions)): `SessionMeta` gains
  `pinned`, true while the user has a root session pinned, and
  `session.pin.set` (`session_ids`, `pinned`) pins or unpins roots,
  answering like `session.unread.set`.
- `session.list` with `active_since` also returns every pinned root, and
  every root that an enabled cron job with a next fire time targets,
  whatever its age.

## 1.64.0

- Less repeated history on the wire
  ([session storage v3](../specs/session-storage-v3.md) section 3.4; see
  [Newest first](events.md#newest-first)):
  - `events.page` takes an optional `header`: `full` (the default), as
    before; `delta`, for a client that holds the header of what starts at
    `before`, which then gets only the header events it lacks or holds
    differently; or `none`, for a page read into a gap, whose header the
    client does not use.
  - To a client of 1.64.0 or newer, a display-view answer
    (`events.subscribe` with `view: "display"`, `events.page`) sends the
    display text of a tool result that repeats its output as a range of
    it: the text in the event is empty, and the answer's `spans` lists each
    (`seq`, `block`, `segment` of a `terminal` block, `start`, `end`). A
    `file_excerpt`'s range holds its numbered lines. Live events and
    `event.read` stay whole.
- `events.subscribe`'s answer has `live: true` when it registered the
  live tail, the daemon owning the session; a client then needs no second
  subscribe once `session.resume` says it owns the session.

## 1.63.0

- History format 2 ([session storage v3](../specs/session-storage-v3.md)
  section 4.2; see [History formats](events.md#history-formats)): the
  daemon keeps large tool output, ACP option and command lists, and a
  run's model settings chains in the tree's `blobs/` folder, and reads them
  back, so events on the wire stay whole. A converted older log holds a
  `record_omitted` for each ACP live-state record it no longer stores;
  clients of this version show no row for it, and the display view leaves
  it out. `session_created`'s
  `history_format` names the log's format, 2 for a log this build wrote or
  converted, and a client reads a `session_created` of any format. A client
  before 1.63.0 refuses any format but 1, so it receives
  `history_format: 1`, which is what its whole values are. Plugins, whose
  events carried no such check, receive the log's format.

## 1.62.0

- The frozen context ([frozen context](../specs/frozen-context.md) section
  6; see [Prompt blocks](events.md#prompt-blocks)):
  - The audit event `context_refreshed { reason, changed }`: a run composed
    the system prompt, the `AGENTS.md` turn, and the memory turn again
    between compactions, and started a new tool round. `reason` is an open
    enum (`engine_updated`, `tools_changed`, `agent_changed`).
  - `agent_md_loaded` and `memory_loaded` have an optional `in_place`: the
    context of a runtime attached between runs, rendered where it was
    recorded.
  - `tool_round_started` has an optional `engine_build`.
  - A run copies the previous run's system prompt and `AGENTS.md` turn
    until a compaction or refresh, on every model, so
    `prompt_blocks_recorded.composed_live` is false on most runs and
    `prompt_blocks_announced` appears on models that do not defer tools too.
  - Plugin protocol 1.34.0 carries the event and the members the same way.

## 1.61.0

- Stale reasoning ([frozen context](../specs/frozen-context.md) sections
  4, 5, and 7.1; see [Stale reasoning](events.md#stale-reasoning)):
  - `model_turn_committed` has an optional `prefix_fingerprint`: the
    SHA-256 of what the request sent before the turn, on a model that binds
    its reasoning to its prefix.
  - `native_replay_cut` has `cause`, an open enum: `refused` (absent means
    this), `prefix_changed`, `provider_dropped`, `compaction`, or
    `refreshed`.
  - A refusal's `native_replay_cut` now comes before the
    `attempt_abandoned` it recovers, and no `native_replay_cut` diagnostic
    follows it.
  - Plugin protocol 1.33.0 carries both members in events the same way.

## 1.60.0

- An ACP agent's live state is no longer stored
  ([store locks and durability](../specs/store-locks-and-durability.md)
  section 4.3; see [Live-only stream
  output](events.md#live-only-stream-output)):
  - The live-only event `acp_live_update` carries a running call's newest
    `tool_presentation`, the agent's `context` use, or both, each the
    whole new value, as `acp_session_updated` carried them. A subscription
    gets the newest of each when it starts, from a cursor too.
  - `tool_call_terminated` has an optional `presentation`: the call's
    final title, hint, primary argument, arguments, and preview, when they
    differ from those `tool_call_started` stored. It replaces them; the
    tool identity stays.
  - The daemon stores no `acp_session_updated` with `tool_presentation`,
    and one with `context` only before a run's `run_finished`, when it
    changed since the last stored. A client that does not know
    `acp_live_update` drops it, and sees a call's refined presentation when
    the call ends and the context use once per run.
  - Plugin protocol 1.32.0 carries both in events the same way.

## 1.59.0

- Newest-first session history
  ([session storage v3](../specs/session-storage-v3.md) section 3; see
  [Newest first](events.md#newest-first)):
  - `events.subscribe` takes an optional `view`, `full` (the default) or
    `display`, which leaves out bookkeeping, superseded settings, and tool
    results' metadata, and cuts tool results over 4 KiB; the result lists
    them in `cut` (`seq`, `bytes`).
  - `events.subscribe` takes `from: "tail"` and an optional `max_bytes`:
    the newest events within the budget, starting at a run, and a `window`
    with the `header` events the state depends on, `older_cursor`, and a
    `gap` inside a run larger than the budget. It goes live as a final page
    does.
  - `events.page` (`session_id`, `before`, optional `view` and
    `max_bytes`) reads older history without a tail.
  - `event.read` (`session_id`, `seq`) returns one stored event whole, or
    `null`.

## 1.58.0

- `acp_session_updated` has an optional `prompt_context_system_prompt`
  beside `prompt_context`: whether cookie gave the agent its context as
  its system prompt (`true`) or ahead of its first prompt (`false`).
  `prompt_context` now holds the parts in a model's system prompt order:
  the agent document's body, the attachments, the tool instructions, the
  skill listing, then the memory snapshot turn. Clients show it as the
  system prompt row, no longer as a row of its own. Plugin protocol 1.31.0
  carries it in events the same way.

## 1.57.0

- `session.list` takes `active_since` and `before` and answers `older`
  ([session storage v3](../specs/session-storage-v3.md) section 4.3): with
  `active_since`, the root sessions last active since then and every unread
  or busy one whatever its age; with `before`, the cursor a previous answer
  returned as `older`, the sessions last active before it, back to the start
  of the newest month that holds any. `older` is absent once nothing is
  older. Without either, every root session, as before.

## 1.56.0

- Official agents and model tiers
  ([machines on demand](../specs/machines-on-demand.md) section 2):
  - `agents.update` refreshes the official agents now, as `skills.update`
    does the skills: `enabled`, `agents` (each `id` and `version`),
    `updated`, `removed`, and an optional `error`.
  - An `AgentDescriptor` has `official` (default `false`) and an optional
    `official_version`; `unset_models` may name `utility_model`.
  - `ModelCatalogView` has an optional `default_agent`, `[agents] default`:
    the agent a client preselects for a new session (an older daemon sends
    none, and its default is `primary`).
  - Onboarding: `OnboardingStatus` and `OnboardingRunParams` have an
    optional `utility_model`; `missing` may hold `default_agent`, and no
    longer holds `small_model`, which is optional. There is no starter kit:
    `onboarding.preview` refuses with `onboarding_kit_unavailable`, and
    `onboarding.run` ignores `install_kit` and `source` and writes no files,
    fetching the official agents first when the default agent is missing.
- Files the model only names
  ([machines on demand](../specs/machines-on-demand.md) section 6.4):
  `artifact.upload.begin` takes an optional `kind`, `media` (the default)
  or `file`. A `file` upload is any file, up to the same 25 MiB, stored as
  `application/octet-stream`; it reaches the model as its label, `[file
  report.zip, 3.2 MB, artifact 5f2c81d0]`, which names it for `transfer`.
  Admission no longer refuses a file part the run's model does not take
  inline: the model sees its label, with the reason it is not shown.

## 1.55.0

- `runtimes.connect` and `runtimes.disconnect` (`runtime_id`, answering the
  `runtime` as `runtimes.list` reports it) connect a runtime the daemon
  starts without opening an instance, to try that it starts, and disconnect
  it; disconnecting is refused while a run or background call uses it (see
  [Connecting by hand](protocol.md#connecting-by-hand)).
- `RuntimeListing` has `active_runs` and `background_calls`, omitted when
  zero: the active runs of sessions that attach the runtime and the
  background calls executing on it. `runtimes.changed` announces a change
  to either.

## 1.54.0

- Two audit events ([stable prompt prefix](../specs/stable-prefix.md)
  section 6): `prompt_blocks_recorded` (`blocks`, each a `key`, `start`, and
  `len` in bytes of the run's `composed_prompt`, and `composed_live`) before
  a run's first request, and `prompt_blocks_announced` (`blocks`, each a `key`
  and an optional `text`), the instructions that changed since the system
  prompt carried them, rendered in place as a user turn
  (`model_text::prompt_blocks_announcement`). Plugin protocol 1.30.0
  carries them in events the same way.

## 1.53.0

- A `tool_round_started` tool has an optional `source` (`label`, `tool`): the
  attachment, or the label its provider composes under, and the tool's own
  name, for a runtime's tool. A round's tools keep their published names
  until it ends ([stable prompt prefix](../specs/stable-prefix.md) section
  5). Plugin protocol 1.29.0 carries it in events the same way.

## 1.52.0

- `acp_session_updated` has an optional `prompt_context`: the text blocks
  an ACP session's first prompt carried ahead of the input, as sent, so
  clients show what the agent reads beyond the conversation. Plugin
  protocol 1.28.0 carries it in events the same way.

## 1.51.0

- `SessionMeta` has an optional `unread` (left out when false): a root
  session with a finished turn, an approval, or a question the user has not
  seen ([unread sessions](../specs/unread-sessions.md)). The daemon marks it
  when a settle or a needs-you push signal comes while nobody is present on
  the tree, and `client.presence` on the tree marks it read;
  `sessions.changed` `upserted` announces each change.
- `session.unread.set` (`session_ids`, `unread`) marks root sessions unread
  or read, answering the `changed` ones and the `skipped` ones with a
  `reason` (`session_not_found`, `not_a_root_session` with its
  `root_session_id`).

## 1.50.0

- `acp_session_updated` has an optional `background_ended`
  (`AcpBackgroundEnded`: `tool_call_id`, `tool`, `display`, `started_at`,
  `exit_code`, `signal`): a command of an ACP agent's that ran in the
  background ended. Clients show it as a background call's report, so a
  wake the end causes no longer arrives unexplained
  ([ACP v2](../specs/acp-v2.md) section 9).

## 1.49.0

- Providers can be connected by a browser sign-in instead of typed
  credentials ([Sign in with ChatGPT](../specs/chatgpt-sign-in.md)).
  `ProviderDescriptor` has an optional `sign_in_methods`
  (`SignInMethodDescriptor`: `id`, `display_name`, `redirect_path`) and
  `sign_in_account` (`SignInAccountDescriptor`: `email`,
  `sign_in_required`). A sign-in method is never listed in `auth_methods`.
- `provider.auth.begin` (`provider_id`, `method`, `redirect_uri`, null for
  the daemon to catch the redirect itself), `provider.auth.complete`
  (`provider_id`, `callback_url`), and `provider.auth.cancel` run it; the
  daemon holds the PKCE verifier and the tokens. `provider.auth.completed`
  announces a sign-in whose redirect the daemon caught.
- `provider.connect` refuses a sign-in method with
  `provider_sign_in_rejected`, the error of every failed `provider.auth.*`
  request.

## 1.48.0

- `model_tools_published` has an optional `deferred_tool_names`: the tools
  of `tool_names` the request sent deferred and not yet loaded, which
  clients show as searchable apart from the provided ones
  ([deferred tools in cookie](../specs/deferred-tools-cookie.md) section 12).
  Each must also be in `tool_names`. Plugin protocol 1.26.0 carries it in
  events the same way.

## 1.47.0

- MCP server definitions (`mcp.server.*`) take `always_load`,
  `always_load_tools` (patterns over the server's own tool names, none
  empty), and `instructions` (absent reads as `true`), for
  [deferred tools](../specs/deferred-tools-cookie.md) section 10: tools sent
  up front, and whether the server's `initialize` instructions are its
  summary. A `server_tools` entry has `always_load: true` for a tool sent up
  front. Runtime protocol 1.11.0 carries the instructions and the flag in
  MCP reports.
- Plugin protocol 1.25.0: the initialize result's optional `summary`, what
  the plugin's tools are for, shown with them in deferred tool
  announcements.

## 1.46.0

- `ConfigStatus` has an optional `deferred_tools`, a list of
  `DeferredToolsMark` (`provider_id`, `model_id`, `reason`, `age_seconds`):
  the models the daemon sends every tool up front after they refused
  deferred tools ([deferred tools in cookie](../specs/deferred-tools-cookie.md)
  section 9).

## 1.45.0

- A summary checkpoint's `folded_tools` (`context_checkpoint_committed`,
  `internal_summary` only, left out when empty): the deferred tools loaded
  before it, and the previous checkpoint's. The next tool round declares
  them up front ([deferred tools in cookie](../specs/deferred-tools-cookie.md)
  section 8.3). Plugin protocol 1.24.0 carries it in events the same way.

## 1.44.0

- Deferred tools ([deferred tools in cookie](../specs/deferred-tools-cookie.md)):
  a model that loads tools natively is sent MCP and plugin tools deferred,
  with `tool_search` and `tool_load` to find and load them.
  - `tool_round_started` (`tools`, `prompt_groups`): the eager tools a
    session declares until its next compaction.
  - `deferred_tools_announced` (`added`, `removed`, each a list of groups
    with `name`, `source` (`mcp`, `plugin`, `builtin`, open), an optional
    `summary` and `untrusted`, and `tools`): what became loadable, and what
    left, as the model was told.
  - A tool result's `loaded_tools`: the deferred tools it loaded.

  Both events are skippable audit events. Plugin protocol 1.23.0 carries
  them, and the result field, in events the same way.

## 1.43.0

- `runtimes.agents.*` take `runtime_id` `global` for the top-level
  `[acp.agents]` table: ACP agents every runtime offers, which a runtime's
  own declaration of the same key replaces there. A `global` declaration is
  `available` while some runtime runs it. Older daemons refuse `global`.

## 1.42.0

- `crons.create` and `crons.update` take `max_runs` as a whole number of 0
  or less for no limit, besides `"unlimited"`. Jobs still report no limit
  as `"unlimited"`.

## 1.41.0

- `acp_session_updated` has `compaction` (`compaction_id`, `state`,
  `summary`), a compaction of the ACP agent's own, recorded as it starts
  and as it ends, and `session_message` (`direction`, `peer`, `text`), a
  message between the agent's sessions. Both were `notice`s before; clients
  show them as the session's own compaction and as producer messages. A
  compaction's end the agent repeats is recorded once. Plugin protocol
  1.22.0 carries them in events the same way.

## 1.40.0

- A background call may be an ACP agent's own command still running after
  its call ended ([ACP v2 spec](../specs/acp-v2.md) section 9), such as
  Claude Code's backgrounded commands: `agent_task: true`, its terminal as
  `job`. `session.background.stop` refuses it; it leaves the list when the
  command's terminal exits. An idle `daemon.restart` waits for it, and it
  counts in `background_calls`.

## 1.39.0

- The ACP agent's own subagents ([ACP v2 spec](../specs/acp-v2.md) section
  8) are child sessions of its session: `delegated` in origin, listed by
  `session.children` and `session.tree`, whose log starts with
  `acp_session_started` `how: subagent`. Their runs are the subagent's own
  work, started by an `acp_agent` message ("Started with a task", the task
  as its body); they report nothing to the parent and take no prompts. An
  older client reads `how: subagent` as `new`. Plugin protocol 1.21.0
  carries it in events the same way.

## 1.38.0

- Work an ACP agent starts on its own ([ACP v2 spec](../specs/acp-v2.md)
  sections 6 and 7), such as Claude Code's follow-up after a background
  command, is a run started by a producer message of the new owner
  `acp_agent { agent }` (description "Woke on its own"). An older client
  reads the `producer_message_accepted` event as an unknown payload. Plugin
  protocol 1.20.0 carries the owner in events the same way.

## 1.37.0

- Clients change a runtime's `[runtimes.<id>]` table ([runtime management
  spec](../specs/runtime-management.md) phase 2), all additions an older
  peer ignores (see [Runtime tables](protocol.md#runtime-tables)):
  - `mcp.server.add`, `edit`, `remove`, and `set_enabled` take an optional
    `runtime_id`; without it, or with `global`, they act on the top-level
    `[mcp]` servers as before.
  - `mcp.server.rename { runtime_id?, name, new_name }`.
  - `McpServerDefinition.scope`: a runtime server's `runtime` or
    `workspace`.
  - `runtimes.agents.list`, `set`, `remove`, and `rename`, and their types
    `RuntimeAgentDeclaration`, `AcpAgentDefinition`, `RuntimesAgentsResult`,
    and `RuntimesAgentsRenameResult`.

## 1.36.0

- `McpServerInfo.server_tools` ([runtime management
  spec](../specs/runtime-management.md) phase 1): every tool the server
  lists, by its own name and in its order, as `McpServerTool { name,
  filtered }`; absent while it has listed nothing. An older peer ignores it.
  Runtime protocol 1.9.0 adds the same list to `McpServerReport`.

## 1.35.0

- An `idle` `daemon.restart` waits for background calls too: a call still
  running in the background, or one whose result is not yet committed,
  holds the restart, since the shutdown ends it without a report. Both
  additions an older peer ignores:
  - `DaemonRestartResult.background_calls` and
    `DaemonRestartingNotification.background_calls`: the pending background
    calls, omitted when zero.
  - `daemon.restarting` is sent again whenever the count changes.

## 1.34.0

- Held sessions ([held sessions spec](../specs/held-sessions.md)): a session
  whose run the user stopped, or whose runs failed past `[model_retry]
  standard_retries`, is held until it is resumed. All additions an older
  peer ignores:
  - `automatic_runs_held { reason }` and `automatic_runs_resumed {}`, new
    runless state events. `reason` (`HeldReason`) is open: `stopped`, or
    `failing` with `failures` and an optional `error`.
  - `SessionMeta.held`: the reason while the session is held.
  - `session.held.resume { session_id }` → `{ resumed }`: clears the hold
    and lets what waits run; `resumed: false` for a session that is not
    held. A message the user sends clears it too.
  - The cron skip reason `session_held`.
  - While held, `producer.send` from a plugin or runtime answers
    `producer_rejected` with the hold in its detail, and `send_message` from
    an agent other than the session's parent fails with
    `send_message:recipient_held`. Plugin protocol 1.19.0: plugins receive
    the new events.

## 1.33.0

- `native_replay_cut { through_seq }`, a new state event
  ([thinking-replay recovery spec](../specs/thinking-replay-recovery.md)):
  the provider refused a replayed reasoning block bound to a conversation
  prefix that changed since, and the reasoning of every turn committed at or
  before `through_seq` is no longer replayed. A `diagnostic` of kind
  `native_replay_cut` beside it says so in the transcript. A reader that does
  not know the event opens the log render-only, as for any unknown state
  event. Plugin protocol 1.18.0: plugins receive the event.

## 1.32.0

- An ACP agent's document may run another declaration (`acp.agent`), so
  several agents can run one declaration, each with its own permissions,
  mode, and body. `AgentRuntime.agent` then differs from the agent's ID:
  it names the declaration, and clients show it beside the runtime
  (`claude on local`). No wire change.

## 1.31.0

- `session.recall_input` takes back any client's pending or queued input, not
  only the caller's: the recall is recorded under the caller's origin and the
  content goes back to the caller. `not_owner` is no longer answered; clients
  offer recalling another client's input only to a daemon of 1.31.0 or later.
  No wire change.

## 1.30.0

- Push notifications ([push notifications spec](../specs/push-notifications.md)),
  all additions an older peer ignores:
  - `notifications.get`, `notifications.subscribe`,
    `notifications.unsubscribe`, and `notifications.test`: the daemon's
    VAPID public key and the browsers it pushes to (`PushSubscription`:
    `id`, `label`, `push_service`, `created_at`, `last_sent_at`,
    `last_error`). `NotificationsTestStatus` is open; an unknown value reads
    as `unknown`.
  - `notifications.changed`, sent to every connection when a subscription
    is added, changed, or removed.
  - `client.presence`: the session a connection shows the user, or `null`,
    so the daemon does not push what is on screen.
  - The errors `push_subscription_not_found`, `push_subscription_invalid`,
    and `push_unavailable`.

## 1.29.0

- Native compaction beside the summary ([native compaction spec](../specs/native-compaction.md)),
  all additions an older peer ignores:
  - The `internal_summary` checkpoint of `context_checkpoint_committed` has
    an optional `native` (`CheckpointNativeWindow`: `window`,
    `source_through_seq`), the provider window compacted from the same
    events. An older reader uses the summary. Plugin protocol 1.17.0
    carries it too.
  - The `native_window` checkpoint is no longer written; earlier logs keep
    reading.
  - `ConfigStatus` has an optional `native_compaction`, a list of
    `NativeCompactionMark` (`provider_id`, `state`, `reason`,
    `age_seconds`): the providers the daemon stopped attempting native
    compaction for (`unsupported`) or whose windows it ignores
    (`rejects_windows`). `NativeCompactionState` is open; an unknown value
    reads as `unknown`.

## 1.28.0

- The approval reviewer ([approval reviewer spec](../specs/approval-reviewer.md)
  sections 7, 10, and 11), all additions an older peer ignores:
  - `ApprovalInternalDecision` has an optional `rationale`, the reviewer's
    one-sentence reason for its verdict, at most 300 characters.
  - `ApprovalReasonCode` has the value `internal_agent_failed`: the reviewer
    failed (an error, a timeout, an unparseable reply, or no model), so the
    evaluated step escalates and no permission mode approves it for the
    user. An older reader reads it as `system_error`; a reader before
    1.28.0 also checked an evaluated step's decision with that fallback and
    refuses the step as contradictory, which 1.28.0 readers no longer do.
    The `escalated` step that follows keeps the reason `escalated`.
  - The `escalated` approval step has an optional `expires_at`, when the
    user's window to answer ends: it now starts at the escalation, so the
    request a 1.28.0 daemon writes carries no `expires_at` of its own, and
    the approval's record (`approval.list`) carries the step's.
  - New event `permission_mode_set` (`mode`), which a root session records
    outside its runs when its tree's permission mode changes. Plugin
    protocol 1.16.0 carries these too; an older reader skips the event.

## 1.27.0

- `FrozenOpenAiPromptCacheTtl` (the `ttl` of an `openai` frozen cache
  strategy, in `run_started` and delegation events) gains `12h`, OpenAI's
  12-hour lifetime for the Responses API, and is now an open enum: a value
  a reader does not know reads as `30m`. Plugin protocol 1.15.0 carries it
  too.

## 1.26.0

- ACP agents ([ACP agents spec](../specs/acp-agents.md) section 15, input
  and pickers), all optional members an older peer ignores:
  - `AgentRuntime` (an entry of `AgentDescriptor.runtimes`, and `runtime`)
    has `options`, the config options a new ACP session of the agent starts
    with on that runtime as the daemon last saw them, with the agent
    document's `acp.mode` and `acp.config` applied, and `input_media`, the
    files the agent takes in a prompt (`image`, `audio`, `text`).
  - `session.create` takes `agent_options` (`[{ option_id, value }]`): the
    values the session's new ACP sessions start with, on top of the
    agent document's `acp.config`. Send it only to a daemon of 1.26.0 or
    later.
  - `acp_session_updated` has `starting_options`, those values, recorded as
    the session is created. Plugin protocol 1.14.0 carries it too.

## 1.25.0

- Questions an agent asks the user ([ACP agents spec](../specs/acp-agents.md)
  section 15, elicitation; [Questions](protocol.md#questions)):
  - New events `elicitation_requested` (`elicitation_id`, `agent`, optional
    `tool_call_id`, `message`, and a `form` of typed `fields` or a `url`)
    and `elicitation_resolved` (`action` `accept`/`decline`/`cancel`, an
    accepted form's `content`, `answered_by`, and the daemon's `reason`).
    Plugin protocol 1.13.0 carries them too; an older reader skips them.
  - New method `elicitation.respond` (`session_id`, `elicitation_id`,
    `action`, and `content` for an accepted form), with the new errors
    `elicitation_not_pending` and `elicitation_answer_invalid`.

## 1.24.0

- ACP agents ([ACP agents spec](../specs/acp-agents.md) section 15,
  session lifecycle): `acp_session_started` has the `how` value `forked`
  (the agent's `session/fork` of a session a fork or a revert left behind;
  an older client reads it as `new`) and the optional `forked_from`, the
  agent's session it was forked from. `acp_session_updated` has the
  optional `detached` (`acp_session_id`, `reason` `forked` or `reverted`,
  and the optional `message_id` of the agent's last message the session
  keeps): a fork or a revert left that ACP session behind, so the agent's
  next turn branches off it instead of resuming it. `acp_session_started`
  also has the optional `fork` (the agent offers `session/fork`) and
  `fork_point` (its fork takes a fork point), and `acp_session_updated` the
  optional `fork_point`, found once the daemon has asked the agent. Plugin
  protocol 1.12.0 carries them; an older reader ignores them.
- New error `acp_agent_unsupported` (`reason`): `session.revert` or
  `session.fork` would cut into the conversation of an ACP agent that
  cannot fork it there.

## 1.23.0

- ACP agents ([ACP agents spec](../specs/acp-agents.md) section 15,
  display round): `acp_session_updated` has the optional `context`
  (`used`, optional `size`), the agent's context use from its latest
  `usage_update`, which clients show as the session's context; an older
  client shows no context for an ACP session, as before.
- `AcpConfigOption` has the optional `kind` (`select`, left out, or
  `boolean`). A boolean option's `current_value` is `true` or `false`, and
  its `values` are `true` (On) and `false` (Off), so an older client shows
  it as a two-value select that still sets it; `session.agent_option.set`
  takes `"true"` or `"false"` for it.
- Plugin protocol 1.11.0 carries both in the events plugins receive; an
  older plugin ignores them.

## 1.22.0

- Onboarding ([default models and onboarding spec](../specs/default-models-and-onboarding.md)):
  - New methods `onboarding.get` (whether the daemon is set up: `ready`,
    `missing`, `providers_connected`, `default_model`, `small_model`),
    `onboarding.preview` (what installing the starter kit from a release or
    a sent archive would do, per file), and `onboarding.run` (sets the
    models and installs the kit as one change, or refuses with the new error
    `onboarding_incomplete` and writes nothing). A kit that cannot be
    fetched or used is the new error `onboarding_kit_unavailable`.
  - New notification `onboarding.changed`, the new `onboarding.get` answer,
    to every connection. An older client ignores it.
- `AgentDescriptor` has the optional `unset_models`: the configuration
  fields (`default_model`, `small_model`) its `models` entries name that are
  not set. An older client does not show them.
- The frozen `${parent_model}` fallback of an internal agent definition in
  `run_started` has the optional `variant` that replaces the parent model's
  variant. Plugin protocol 1.10.0 carries it too; an older reader runs the
  parent's own variant.
- Official skills: the skill source `official` (an older client reads it as
  `user`; runtime protocol 1.8.0 carries it too), the optional `version` of
  an official skill's `SkillDescriptor`, `skills.list` without a
  `session_id` for the daemon's own skills, and the new method
  `skills.update`, which refreshes the official skills now.

## 1.21.0

- ACP agents ([ACP agents spec](../specs/acp-agents.md) section 15,
  naming) are named by the key of their runtimes' declarations (`claude`), not `<runtime>-<agent>`, and every runtime that declares an
  ID offers the one agent: `AgentDescriptor` has `runtimes`, every runtime
  that offers it (`runtime_id`, `agent`, `available`); `runtime` stays for
  an agent exactly one runtime offers, so a client older than 1.21.0 lists
  one several runtimes offer as a daemon agent. A session runs the agent on
  the one runtime it attaches that offers it; with none or two, creating the
  session, starting a run, or delegating is refused, and so is attaching a
  second runtime that offers a session's ACP agent.

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

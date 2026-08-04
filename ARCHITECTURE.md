# cookie_code Architecture

**Status:** accepted future implementation architecture
**Required versions:** configuration schema 6; agent document schema 1;
protocol 7; event schema 7; session JSONL 7; session metadata 7;
delegation-journal schema 7
**Tagline:** subagent-first coding harness

This file is the authoritative architecture record. The strict implementation
contract for provider configuration, Markdown agents, variants, fallback
resolution, permissions, delegation, protocol fields, persistence, security,
and TUI behavior is
[docs/agent-model-variant-redesign.md](docs/agent-model-variant-redesign.md).
That contract is part of this architecture and must be implemented as written.

Cookie Agent is a Rust coding-agent harness whose local daemon owns agent
behavior. Thin clients consume one versioned protocol. Sessions form durable
delegation trees, while each session remains an isolated conversation with its
own frozen run policy and event log.

## 1. Architectural invariants

1. **Current-only formats.** Config is exactly schema 6, Markdown agent
   documents exactly schema 1, and protocol/event/persistence exactly 7. Older
   project formats are rejected; there are no aliases, compatibility readers,
   migrations, dual paths, or deprecated fields.
2. **Provider-centric configuration.** Runtime TOML defines providers and
   included models directly. A runnable model key is `provider/model-id`; model
   aliases do not exist.
3. **Markdown agents.** Agents are strict `.md` documents whose required body
   is the complete system prompt. Agent policy is not stored in runtime TOML.
4. **Immutable runs.** Before a run begins, the engine freezes its complete
   agent prompt, tools, permissions, delegation policy, exact model/variant
   suffix, descriptors, defaults, options, and fingerprints. Live file or
   credential changes do not reinterpret that run.
5. **Subagent-first, delegate-only.** Only the `delegate` tool can create a
   child. Children are ordinary sessions with durable provenance and use their
   own agent permissions.
6. **Event-sourced.** Strict per-session JSONL events and the delegation journal
   are durable truth. Metadata and client state are rebuildable projections.
7. **One engine, many frontends.** The daemon owns model loops, tools,
   permissions, approvals, persistence, titles, and delegation. TUI, web, and
   editor clients are protocol consumers.
8. **Explicit capability honesty.** Provider/model/adaptor behavior is explicit
   and validated. Model names never infer adaptors, capabilities, defaults, or
   variants.
9. **Prepared authority.** Filesystem and process operations are prepared once,
   approved against immutable resources, then executed through the held
   capability. Permissions are consent control, not an OS sandbox.
10. **Stable UI attribution.** Draft selection and frozen producer attribution
    are separate. Exact selections use canonical
    `provider/model[variant]` text, including `[base]`, and visible assistant
    headers come from persisted attempt data rather than current picker state.
11. **Locked internal distribution.** Workspace crates are nonpublishable. The
    root `Cargo.lock` is the sole dependency graph and releases are locked
    workspace builds of the `cookie` binary.

Explicit non-goals for the initial implementation remain OS sandboxing, MCP,
plugins, remote deployment, budget accounting, and user-visible parallelism
caps.

## 2. Components and dependency direction

```text
TUI / CLI / future web / VS Code
                 │
                 ▼
             protocol v7
                 │
              server
                 │
        ┌────────▼────────┐
        │     engine      │  sessions, runs, prompts, events,
        │                 │  permissions, approvals, provenance
        ├─────────────────┤
        │ tools/delegate  │  built-ins and only child-creation path
        ├─────────────────┤
        │ models/config   │  immutable provider/model snapshots
        └─────────────────┘
```

Dependency direction has no cycles:

```text
tui ──────► protocol ◄────── server
               ▲               │
               │               ▼
models ◄──── engine ◄──────── tools
  ▲            │
identity ◄── config
```

- `identity` owns `AgentId`, `ProviderId`, `ProviderModelId`, `ModelKey`,
  `VariantId`, and `ModelSelection`.
- `config` loads schema-6 runtime TOML plus schema-1 agent documents.
- `models` constructs reviewed Oven adapters and immutable variant-aware model
  snapshots.
- `engine` owns sessions, runs, frozen `AgentSnapshot`s, event persistence,
  replay, permissions, approvals, and privileged child admission.
- `tools` implements built-ins and the `delegate` provider through engine APIs;
  the engine does not import concrete tool implementations.
- `protocol` owns exact v7 requests, responses, events, generated schemas, and
  TypeScript bindings.
- `server` routes JSON-RPC over transport-agnostic streams.
- `tui` is a pure protocol client.
- `cookie_agent` is the sole composition root and publishes complete snapshots
  atomically.

The engine actor for one session serializes history mutations but never awaits
tool futures. Tool calls execute outside the mailbox and report back through
messages, preserving cancellation and deadlock-free delegation.

## 3. Current workspace configuration

The only workspace path is:

```text
<cwd>/.cookie-agent/
  config.toml
  agents/
    <agent-id>.md
```

Optional user configuration is below `~/.config/cookie_agent/`. Layering is
built-in runtime defaults, user TOML, then workspace TOML; user agents are
replaced by same-ID workspace agents. Provider definitions and agent documents
are atomic replacements, not field merges. Arrays replace.

There is no upward search and no environment configuration layer. Interpolation
is single-pass and restricted to provider endpoints, approved auth secret
fields, and provider header values. Agent documents and model behavior fields
do not interpolate.

Loading is descriptor-relative and no-follow from the exact user config root or
cwd. User files require current-user ownership and private modes. Both user and
workspace loaders reject links, wrong types, multiply linked files, malformed
or oversized TOML/YAML, duplicate keys/IDs, and unknown fields. Secret values
are redacted and excluded from events, errors, debug output, fingerprints,
session persistence, and generated artifacts.

`attach` and `connect` are workspace-independent. `.cookie_agent` and old
workspace-acceptance/trust artifacts are not inspected.

## 4. Provider and model architecture

`RuntimeConfig.providers` is a strict map of tagged `ProviderDefinition`:

- `ModelsDevProvider` binds an exact vendored models.dev catalog revision,
  reviewed recipe, optional permitted endpoint/adaptor override, auth, headers,
  and an explicit map of included model IDs.
- `ExplicitProvider` requires endpoint, supported adaptor, auth, headers, and a
  nonempty explicit model map.

Only listed, enabled models are runnable. Every model has honest capabilities,
normalized frozen request defaults, strict adaptor-specific provider options, base
behavior, a variant map, an optional default variant, and behavior/configuration
fingerprints. Models.dev model entries derive complete capabilities from the
pinned source and reviewed recipe and cannot author a capabilities table;
explicit declarations must state every capability field, including false
booleans and an empty media map. `parallel_tool_calls = true` requires tool
calling, seed support is an explicit capability, and each non-text input
modality requires its matching bounded media entry. Unknown or
unsupported fields and auth/adaptor/capability/default combinations fail the
whole provider candidate.

For both provider forms, `source`, `auth`, and a nonempty `models` map are
required; models.dev additionally requires catalog revision, while explicit
requires endpoint and adaptor. Headers default empty. Model `enabled` defaults
true; defaults/options/variants default empty; models.dev display name defaults
to its source value; explicit display name and complete capabilities are
required. Every other provider/model field is either explicitly required or
has the omission behavior fixed in the redesign contract.

Provider publication is atomic: construct every enabled model and variant,
validate all defaults and fingerprints, then replace the immutable provider
snapshot in one swap. A failed refresh leaves the preceding snapshot intact.
Credential values are never part of a snapshot or fingerprint; safe auth shape
and header names are.

The engine retains immutable authored agent documents, not one startup-resolved
registry. `agent.list` and `session.create` each load one current model-manager
snapshot and materialize the complete registry against that same snapshot, so a
provider connection cannot advertise an agent that session creation still sees
as unavailable. Published model snapshots referenced by existing sessions and
runs remain retained for the daemon lifetime.

The public root model catalog is the complete set of configured, enabled, and
currently executable models in strict `ModelKey` order. Credential-blocked
entries remain in the immutable internal snapshot for agent resolution but are
not emitted as public `AvailableModelDescriptor` values until a coherent
credential refresh makes them executable.

### 4.1 Variants

A `ModelSelection` contains a direct `ModelKey` plus `Option<VariantId>`; `None`
means exact base behavior. Variants are named behavior presets under one model,
not separate model IDs. Its canonical display is
`provider/model[variant]`: base is always explicit as `[base]`, and named IDs
are preserved exactly, including a literal `[default]`. This display rule does
not change the structured serialized shape.

Agent fallback authoring stores `Option<ConfiguredVariantRef>` with three
distinct states:

- omitted/`None` → the provider model's already-resolved default selection;
- explicit `base`/`Some(Base)` → exact base even when a named default exists;
- any other valid string/`Some(Named)` → named variant, including an ID literally named
  `default`.

Provider model default authoring is separate:
`default_variant: Option<ConfiguredModelDefault>` uses `None` for omitted and
retaining the provider source default, `Some(Base)` for explicit exact base, and
`Some(Named)` for a named variant. Both provider defaults and agent fallback
entries resolve to exact `ModelSelection` before freezing.

Every authored entry resolves to exact `ModelSelection` before freezing. A
fallback chain may not repeat a `ModelKey`, so delegated chain selection remains
unambiguous and does not wrap.

The only models.dev reasoning option forms are `effort`, `toggle`, and
`budget_tokens`. Effort supports `none`, `minimal`, `low`, `medium`, `high`,
`xhigh`, `max`, `default`, and `null`; null produces `off`. Toggle produces
`off`/`on`. Budget metadata produces only the deterministic IDs justified by
the pinned `min`/`max` fields: `min = -1` produces `budget-auto`, finite `min`
produces `budget-min`, and present `max` produces `budget-max`. There is no
additional budget field or generated ID; separate reviewed recipe
metadata affects base/source behavior only. Absent bounds produce no invented
variants. Multiple options form a deterministic union, not a Cartesian
product. Explicit add/replace/disable directives are applied after generation;
remaining
collisions follow the strict precedence and failure rules in the redesign
contract.

Reasoning is authorable only as `VariantDirective.reasoning`; ordinary
`RequestDefaults` and provider options reject reasoning aliases. Every variant
passes an adaptor-specific compiler into internal resolved defaults and
typed provider options. If behavior cannot be encoded exactly, provider
construction fails. Silent approximation or dropped variants are forbidden.

## 5. Agents, prompts, and run selection

An agent document basename is its `AgentId`. Strict schema-1 frontmatter defines
description, mode (`primary`, `subagent`, or `all`), enabled state, fallback,
tool allowlist, ordered permissions, and optional delegation. Its normalized,
required, nonempty Markdown body replaces the generic system prompt.

The engine appends no environment, cwd, date, repository, tool, or project
metadata to this system prompt. The complete composed prompt is therefore the
normalized body, and both text and fingerprint are frozen in `AgentSnapshot`.
Tool schemas are request tools; a delegated task is an initial user message.

Every `primary` agent must configure a nonempty fallback chain. `subagent` and
`all` may configure an empty chain and inherit only when delegated. Every
empty-chain agent has `runnable_as_root = false`; a subagent is never root
runnable, and an `all` agent is root-selectable only when enabled with its own
nonempty chain and at least one available selection. Root creation and the root
selector use only `runnable_as_root = true` descriptors.

`SessionCreateParams` and `RunStartParams` use exact `RunSelection { agent,
model }`. Root model/variant validation and plan construction use one current
model snapshot. Before `RunStarted`, the engine freezes:

- complete prompt text and fingerprint;
- identity, mode, description, and document fingerprint;
- tools and ordered permissions;
- delegation targets and effective depth ceiling;
- exact resolved model/variant bindings, descriptors, defaults, options, and
  fingerprints;
- the selected fallback suffix.

For a root session, each public `run.start` may select any agent that is
currently `runnable_as_root` after the authored documents are materialized
against one current model snapshot. Its exact model and requested base or named
variant may be any entry in the public root model catalog. If the model occurs
in the authored chain, the root plan is that exact selected head plus the
available authored tail after it. If it is outside the chain, the root plan is
a synthetic exact head plus every available authored fallback entry.
Unavailable authored entries are skipped in either plan. Root eligibility still
requires the agent's own authored chain to be nonempty and to contain at least
one available entry; an arbitrary catalog selection does not make an otherwise
ineligible agent runnable. The accepted run freezes the resulting exact plan,
so later selections or provider refreshes do not alter it.

Delegated planning is unchanged. A delegated agent with an authored chain uses
the selected exact authored suffix, preserving its later authored entries; an
empty-chain child inherits the invoking parent's active frozen suffix.

Retries, fallback attempts, tool-loop passes, compaction, title generation,
approval work, and replay use that frozen run policy rather than live config.

## 6. Permissions and approvals

Permission actions are exactly `read`, `write`, `bash`, `grep`, `glob`,
`delegate`, and `external_directory`; effects are exactly `allow`, `ask`, and
`deny`. Permission rule IDs use the same 1..=128-byte lowercase `SafeCode`
grammar in authored configuration and frozen protocol values. Rules are ordered
and last match wins; no match is Ask. The sole wildcard grammar is nonempty and
at most 4096 UTF-8 bytes, with no controls, globstar (`**`), escaping, classes,
or braces; `*` matches any characters including `/` and `?` matches exactly one
character. A terminal ` *` also matches the same resource without that suffix.
Checked agent fixtures spell both root and nested secret labels explicitly,
including `.env`, `.env.*`, credential/token files, `id_*`, `.netrc`, and cloud
credential files; `.env.example` is the explicit root/nested exception.

Tool mapping is exact: read→read, write/edit→write, bash→bash, grep→grep,
glob→glob, and delegate→delegate. `external_directory` is a prior guard, not a
tool. Bash permissions evaluate parsed subcommands; file actions evaluate held
canonical prepared resources; delegate evaluates the target `AgentId`.

Every operation is prepared once into immutable resources and an operation
fingerprint. Permission and approval evaluate those resources, and execution
uses only the held capability. External-directory checks occur before the
underlying file action. `.env` reads ask by default, prepared-resource changes
fail closed, and the fourth repeated identical operation in one run without
intervening user input or a successful different operation is denied as a doom
loop.

Ask creates durable internal evaluation. Only an escalated request is exposed
to users. Responses are exact, revision/fingerprint bound, idempotent, and
limited to approve once/tree, reject, or cancel. Unattended Ask denies.
Prepared process-local filesystem authority is never reconstructed after
restart.

The frontmatter tool allowlist contains only read/write/edit/bash/grep/glob;
delegate is engine-owned and cannot be listed. Tool exposure starts from that
frozen allowlist and registration. A final
unconditional deny can make a tool unavailable. Ask-by-default tools remain
exposed and request approval at invocation. A child uses only its own frozen
permissions; configured parent rules never flow down. Exact tree-scoped runtime
grants are consent records, not permission inheritance.

## 7. Delegation and stable trees

Absence of an agent's delegation block disables delegation. The `delegate` tool
is the only child-creation path. Targets must be listed, enabled agents of mode
`subagent` or `all`. Tool schema/list exposure filters to eligible targets, and
the engine authoritatively revalidates target, permission, frozen registry,
depth, ceiling, and idempotency immediately before durable reservation.

Root depth is 0 and child depth is parent + 1. `max_depth` is the inclusive
maximum depth beneath that root. A root ceiling remains authoritative for the
tree; a child's effective onward ceiling is the minimum of the inherited
ceiling and its own configured `max_depth`. A child without delegation cannot
delegate.

A child with a configured nonempty chain uses its own chain. A child with an
empty chain inherits the invoking parent run's currently active frozen fallback
suffix at admission, including the exact selected variant. It never inherits
the parent's original full chain or exhausted entries.

Delegation uses durable reservation/idempotency, a parent `ToolCallLinked`, a
child `SessionCreated` with engine-derived provenance, exactly-once run start,
cancellation propagation, bounded retained result, and restart reconciliation.
The engine derives root and depth; no tool/client value is trusted.

Client tree projections retain one stable root while descendants are watched.
Only the Sessions picker reroots. Rows are `agent-id:session-title`, title
updates are sequence-monotonic, and stale tree responses cannot overwrite a
newer title.

## 8. Protocol, events, and persistence

JSON-RPC 2.0 message semantics are transport independent. In-process and
WebSocket are initial transports; stdio and Unix sockets remain deferred.
Handshake `ProtocolVersion` is exactly 7 and durable `EventSchemaVersion` is
exactly 7.

`StoredEvent` contains version, session ID, optional run ID, per-session
monotonic sequence, timestamp, and one strict payload. The envelope does not
repeat policy. `SessionCreated` stores creation selection and creation
`AgentSnapshot`; every `RunStarted` stores the complete authoritative run
snapshot and selected suffix.

Required attribution/ownership events include:

- `ModelAttemptStarted` before any delta, with exact resolved model/variant and
  prompt fingerprint;
- `ModelReplayEvaluated` with ordered decisions for that attempt;
- `ModelTurnCommitted` with exact resolved model, stable model-turn sequence,
  complete persisted turn, warnings, and input boundary;
- `ModelFallback` with exact from/to selections, indices, attempt count, and
  safe typed error;
- `ToolCallStarted` with `AssistantToolCallRef` ownership and safe compact
  presentation;
- `ToolCallTerminated` as the sole completed/failed/cancelled/interrupted
  terminal tool event, repeating exact ownership;
- `SessionTitleCommitted`, whose event sequence is the authoritative
  `title_updated_seq`.

Every stream delta carries `attempt_id`; replay attribution derives from
`RunStarted` and attempt/turn events. It never uses current picker state or live
configuration.

`events.jsonl`, `meta.json`, and the delegation journal are strict version 7.
`SessionMeta` is the separate `meta.json` schema and is not an event payload.
The first session event is `SessionCreated`; sequence/ownership references are
validated. A partial final JSONL line may be truncated, but non-tail corruption
fails closed. Restart rebuilds metadata, titles, attempts, turns, tool ownership,
approvals, and trees from events/journal, marks nonterminal runs interrupted,
and never recreates prepared OS capabilities.

Frozen model bindings require an exact retained selection/behavior fingerprint.
An obsolete fingerprint leaves history readable but fails execution with a
typed error; there is no fallback by key/name.

Secrets and provider-native private payloads never enter safe events. Live raw
tool-output deltas remain ephemeral; the model and event log receive one bounded
final result plus safe artifact metadata.

## 9. TUI transcript and selectors

The Message title represents client-local draft selection as
`Agent • provider/model[variant]`, with one ASCII space on each side of the
decorative bullet. Agent and canonical model-selection text are the selection
regions; the bullet is not clickable, and there is no separate variant picker.
Base is rendered explicitly as `[base]`. The global model picker has one row per
available model, showing its canonical resolved default selection and display
name; it never expands variants into rows. Changing models selects that default,
while reselecting the current model retains its exact variant. Clicking the
Message title's `[variant]` region cycles exact variants directly. Draft changes
do not alter an active run.

The visible header of every assistant item has the exact form
`<agent-id> • <provider>/<model-id>[<variant>]` from frozen attempt attribution,
with `[base]` for base behavior. It has no textual `ASSISTANT` prefix and never
hides or infers the exact variant.

One assistant item owns ordered child segments:

```text
Text | Thinking | Tool
```

Thinking and tools are independently collapsible and cached. Each toggle row
has exactly one chevron. Thinking is plain wrapped text with no standalone
`REASONING` block. Tools remain in owning model-content order with no standalone
`TOOL` block. Running compact rows add `…`; successful rows add nothing;
failures/cancellation/interruption use concise text; `COMPLETED` is forbidden.
Expanded details retain arguments, output, truncation, artifacts, attachments,
and read syntax highlighting.

The Agents panel text rows are exactly
`clamp(visible_tree_row_count, 1, 3)`, with borders outside the count. The TUI
keeps the full-width vertical stack, stable scroll geometry, grapheme-safe
multiline input, Markdown/table/code rendering, semantic no-color themes,
approval UI, diagnostics filtering, and immediate monotonic title patching.
Conversation and Message titles contain no instructional drag/hotkey prose.

## 10. Tools and runtime behavior retained

The built-ins remain read, write, edit, bash, grep, and glob, plus conditional
delegate. Write is atomic; edit is optimistic exact-match with a pre-rename
conflict check; bash uses process groups, cancellation, streamed stdout/stderr,
and optional stdin. Read supports bounded text/directory output and validated
durable image/PDF attachments. Grep/glob honor repository ignore behavior.

Every tool returns a rich final result with safe title, model-visible bounded
text, structured metadata, truncation/retention details, and attachment
descriptors. Complete oversized output is retained atomically in private
content-addressed artifacts before a bounded preview is exposed. Retention
failure fails closed.

Tool output streaming uses ephemeral per-call offsets and gap markers; it is
not session-event replay. `ToolCallProgress` may be durable. Tool stdin persists
only call ID and byte count, never submitted bytes. Cancellation stops model
streaming and in-flight tools.

Model fallback remains error-classified and sticky within one run. Same-entry
retry is permitted only before meaningful text, reasoning, or tool-call output;
retryable failure after meaningful output advances. Abandoned partial output is
not committed into later prompt history. Earlier committed turns and tool
results remain.

Native replay/context artifacts are selection-fingerprint scoped. A foreign
variant is discarded with a typed disposition rather than guessed compatible.
Context compaction, approval, and title work inherit the owning run's active
frozen suffix and exact prompt policy.

## 11. Source ownership and generated artifacts

Implementation ownership is:

| Owner | Surface |
|---|---|
| Config-model | `crates/identity/**`, `crates/config/**`, `crates/models/**`, pinned models.dev updater/compiler |
| Protocol | `crates/protocol/**`, JSON Schemas, TypeScript bindings, protocol/event snapshots |
| Runtime | `crates/engine/**`, `crates/tools/**` |
| Service | `crates/server/**`, `crates/cookie_agent/**` |
| TUI | `crates/tui/**` |
| Integration | root manifests and `Cargo.lock`, architecture/docs, checked `.cookie-agent/**`, integration fixtures, generated artifact aggregation, stale-claim review, final locked-workspace validation |

The integration owner is accountable for cross-phase coherence and assigns any
generated schema/binding/snapshot change to one owner. No phase silently edits
another phase's generated output.

## 12. Validation and deferred work

Required validation is warning-free formatting, locked workspace
build/check/clippy/test/rustdoc, Rust 1.88 equivalents, dependency audit/deny,
schema/binding/snapshot regeneration checks, provider/adaptor conformance,
filesystem/security adversarial tests, restart/replay/delegation E2E, and TUI
render/hit-region tests.

Final stale-claim checks must reject old workspace paths, old agent/profile
configuration, model aliases, protocol/event/storage v6, hidden assistant
variants, separate variant pickers, standalone reasoning/tool blocks, inherited
parent permissions,
original-full-chain child inheritance, stale title replacement, and
compatibility decoders.

Deferred features remain session forks, stdio/Unix-socket transports, MCP, web,
VS Code, SQLite projections, and plugins. They must consume the same strict
identity, frozen-policy, event, and security boundaries when introduced.

# cookie_code Architecture

**Status:** frozen current implementation contract

**Required versions:** configuration schema 10; agent document schema 4;
protocol 9; event schema 14; session JSONL 14; session metadata 9;
delegation-journal schema 10; runtime snapshot schema 3; catalog cache schema 2;
provider store schema 3; family recipe registry schema 1; project model-snapshot
manifest schema 1.

Only those versions are accepted. Configuration schema 9, agent schema 3,
event/session schema 13, delegation-journal schema 9, protocol/persistence 8, catalog cache
1, provider stores 1/2, and every unversioned or earlier replacement are
rejected. There are no migrations, compatibility readers, aliases, or dual paths.

Session metadata schema 9 adds required `last_activity: Timestamp`. Its value is
the timestamp of the latest event in the session JSONL log; a session containing
only `SessionCreated` therefore reports its creation-event timestamp. The event
log is the single source of truth: list/get/tree responses derive the value from
the in-memory log tail, so live appends are reflected immediately and restart
replay reconstructs the same value. The rebuildable `meta.json` cache remains
versioned by `SessionMetaSchemaVersion` but deliberately omits this derived
field; metadata cache version 8 and every other non-9 version are rejected.

New sessions begin as in-memory projections with a buffered event log. Creating
a session, changing its live permission mode, renaming it, or appending other
pre-message events creates no per-session directory, `meta.json`, or
`events.jsonl`. The first `UserInputSubmitted` append is the persistence gate:
under the session-store mutation lock, the engine appends that event to the
buffer, rebuilds metadata, writes the complete contiguous log and cache into a
temporary session directory, fsyncs it, and atomically renames it into place.
All later events use normal append-per-event durability. Consequently live
session list/get/tree calls include empty sessions, while restart forgets every
session that never received a user message. Root empty-session creation does not
write delegation or artifact entries. Predictive compaction is disabled for an
unpersisted session, so the first run buffers `RunStarted`, skips compaction,
then flushes `SessionCreated`, `RunStarted`, and `UserInputSubmitted` in sequence.

Steering uses a durable pending-input lane. `run.steer` requires the target run
to be active and immediately appends run-scoped `UserInputAdmitted`; admission
does not add model history. Pending membership is replayed in event order as
admissions minus LIFO `UserInputRecalled` events and FIFO promoted
`UserInputSubmitted` events after each run's first submission. The first
submission following `RunStarted` is derivably the run's initial input and never
consumes an admission, even if a steer is admitted during start-time predictive
compaction before that initial submission lands. At every completed tool batch and at the no-tool
completion boundary, the engine promotes all currently pending inputs in
admission order as separate `UserInputSubmitted` events before assembling the
next provider request. A no-tool boundary with no pending input appends
`RunCompleted`; a tool boundary with no pending input simply continues.
`run.recall_steer { run_id }` removes the newest pending input, appends
`UserInputRecalled`, and returns its text, or returns null without an event when
the lane is empty. Terminal run events implicitly void any remaining lane; the
TUI is responsible for retaining or restoring its local composer text. Initial
and delegated run-start input remains an immediate submission because it is
delivered in that run's first provider request. Delegate code that steers an
already active run uses the same admission lane.

Session history has an append-only physical stream and a derived visible branch.
`session.revert { session_id, through_seq }` is idle-only and appends runless
`SessionReverted { through_seq }`; no record is truncated and the next physical
sequence continues from the current tip. The target must be an existing positive
physical sequence. `last_event_seq` and `last_activity` continue to report that
physical tail, including the revert marker, because revert is durable user
activity; branch-derived title, status, usage, approvals, transcript, and model
context use the visible stream. Replay maintains a monotonic historical ceiling equal to the
minimum target of all revert markers seen so far: each marker removes previously
visible records above that ceiling, remains as a physical control record, and
later records form the new branch. Model context, transcript, metadata status,
title and usage, approvals, pending input, and compaction select only that
visible branch. A revert marker leaves the session idle even when its historical
prefix ends inside a run, so the next user input starts a fresh run. Event
subscriptions still carry every physical record, allowing clients to rebuild
their complete disposable projection when a revert arrives.

`session.fork { session_id, through_seq }` may read an active source but requires
a persisted prefix containing a submitted user message. It atomically creates a
new session directory whose prefix preserves source schema versions,
sequences, timestamps, run IDs, and payloads exactly while rebinding envelope
`session_id` to the new directory identity. A fork-local revert marker closes
any historical in-flight prefix, then a title commit appends ` (fork)` to the
visible title (or uses `Untitled (fork)`). The fork is independent and receives
new physical sequences after its copied prefix. Artifacts are content-addressed
in the project-global artifact store, so checkpoint, tool-output, and attachment
references in a copied prefix remain resolvable without copying artifact bytes.

This file is authoritative. Exact types, ordering, failure behavior, startup,
RPC, persistence, and TUI semantics are in
[docs/agent-model-variant-redesign.md](docs/agent-model-variant-redesign.md).
Family registry 1 is described in
[docs/provider-conformance.md](docs/provider-conformance.md).

## 1. Core invariants

1. The daemon refreshes only `https://models.dev/catalog.json`. The URL is
   code-owned, receives no provider credentials, and never redirects.
2. Catalog startup selection is exactly network, validated cache schema 2, then
   bundled bootstrap. A selected body's revision is
   `sha256:<lowercase SHA-256 digest of the exact selected body bytes>`.
3. Structural candidate failure rejects one network/cache/bootstrap candidate.
   A bounded, structurally valid candidate quarantines malformed or ambiguous
   provider/model records independently so valid siblings survive.
4. `/connect` lists every valid current catalog provider and every configured
   managed provider removed from the current catalog. “Configured managed” is
   the union of authored `source = "models_dev"` definitions and provider-store
   records. Custom providers never appear in `/connect` and never use store 3.
5. Runtime TOML has exactly one `[providers.<id>]` namespace. Definitions are
   tagged `source = "models_dev" | "custom"`. Custom IDs begin with `custom.`.
6. A same-ID user/workspace provider definition is an atomic replacement. No
   provider, model, map, array, auth, or override field merges across layers.
7. A managed endpoint is an authored `base_url`, otherwise catalog `api`, otherwise
   the npm-family default. Catalog endpoint metadata is authoritative. An authored
   base URL requires authored auth in that same provider definition.
8. Credentials are semantic values. `api_key` is permitted only for an
   unambiguous one-secret default API-key method. `auth_override` is exactly
   `{ method, values }`. A recipe owns the reviewed wire header/signing form;
   registry 1 emits no credential query.
9. Every authored/catalog endpoint is absolute, bounded, query-free,
   userinfo-free, fragment-free, and HTTPS, except an exact code-reviewed
   loopback HTTP policy. Query strings are rejected, including empty `?`.
10. All managed supported, non-deprecated text-output models are automatic.
    Managed TOML has sparse `model_overrides`; it has no inclusion map.
11. Provider protocol/auth behavior comes from family registry schema 1, keyed by
    catalog `npm`. Catalog endpoint, shape, capability, modality, limit, and nested
    provider override values are compiled directly. Unknown npm families are unsupported.
12. `provider.connect` and `provider.disconnect` mutate only managed provider
    store state. Connect compiles before a single durable transaction; after a
    successful transaction publication is an infallible in-memory swap.
13. Runtime snapshot schema 3 is the sole coherent discovery surface. Legacy
    independently refreshed provider/model/agent list flows do not exist in
    protocol 9.
14. Runs persist exact safe model bindings, source kind, credential source, and
    config-override fingerprint. Rehydration never changes credential source.
15. Model keys split at the first `/`; a model ID may contain `/`.
16. Secrets never enter caches, revisions, snapshots, events, errors, logs,
    generated artifacts, session files, or TUI projections.
17. **Empty setup is valid.** Config schema 10 permits `providers` to be omitted
    or empty. When provider store 3 is also empty and no effective authored
    custom provider exists, startup publishes zero models/root-runnable agents
    and opens the TUI so `/connect` can bootstrap setup. Empty TOML does not hide
    existing per-user stored managed connections.
18. **A model-bearing runtime is immediately runnable.** After authored-agent
    projection, if no authored agent is root-runnable and at least one model is
    available, the engine adds the reserved built-in primary agent `default`.
    Authored agents never use that ID and are never rewritten to use another
    model.

## 2. Component boundary

```text
TUI / CLI
    │ protocol 9
    ▼
server ─────────────── runtime.changed notifications
    │
    ▼
engine ── version-10 events/sessions and frozen run policy
    │
    ▼
model manager ── atomic RuntimeSnapshot schema 1
    │
    ├── catalog manager ── fixed HTTPS / cache 2 / bootstrap
    ├── family registry 1 ── npm-family/protocol/auth compilers
    ├── config loader ── schema 10 and agent schema 4
    └── provider store 3 ── managed durable connections only
```

The catalog manager validates catalog structure. The family registry classifies npm
packages and decides protocol/auth semantics; catalog records decide endpoints and capabilities.
and Oven construction. The model manager compiles a complete candidate runtime
before one atomic publication. The engine freezes one published snapshot at run
admission. Clients consume one coherent snapshot and notifications.

## 3. Filesystem layout and protection

Workspace configuration is only:

```text
<exact-cwd>/.cookie-agent/
  config.toml
  agents/
    <agent-id>.md
```

User configuration is only:

```text
~/.config/cookie_agent/
  config.toml
  agents/
    <agent-id>.md
```

There is no upward search. User and workspace configuration uses ordinary
filesystem reads. Config and agent files may be reached through symlinks and do
not have owner, mode, or hard-link restrictions. Wrong object types, oversize
content, duplicate keys, and unknown fields still fail closed.

On Unix, runtime user data is fixed below:

```text
~/.local/share/cookie_agent/
  catalog/
    models-dev-v2.json
    models-dev-v2.meta.json
    models-dev-v2.lock
  providers/
    store-v3.json
    store-v3.lock
  sessions/
```

Each project may additionally contain the private runtime subtree:

```text
<exact-cwd>/.cookie-agent/model-snapshots/
  <64-lowercase-hex>.json
  model-snapshots-v1.lock
```

The project cwd anchor may be an ordinary shared or worktree directory with any
owner-write/group-write mode. Its `.cookie-agent/model-snapshots` storage subtree,
like the global `cookie_agent`, `catalog`, `providers`, and session directories,
is current-user-owned mode `0700`. Cache/store/body/meta/manifest/lock/temp files
are current-user-owned mode `0600`, regular, and single-link. Traversal is
descriptor-relative and no-follow. A writable project anchor permits
collaborators to remove the private subtree and deny service, but not to inject
accepted storage objects. Every mutation locks and rereads, writes an exclusive
same-directory temporary file, fsyncs it, atomically renames it, and fsyncs the
parent. Unsafe private state fails closed; no permissive fallback path exists.

## 4. Configuration layering

Runtime defaults and catalog/recipes are not TOML layers. The only authored
layer precedence is:

```text
user config schema 10 < exact-cwd workspace config schema 10
user agent document < same-ID workspace agent document
```

A provider definition is atomic by `ProviderId`: if workspace TOML defines an
ID, the entire user definition with that ID is discarded before parsing and
semantic validation. Provider fields, model maps, overrides, variants, headers,
auth values, and arrays never merge. Agent documents are likewise atomic.

Agent schema 4 uses one ordered `permissions` map entry per
`PermissionAction`. Each action value is either a bare `allow`, `ask`, or `deny`
effect, or an ordered resource-pattern-to-effect map; the two forms cannot be
 mixed for one action. Bare effects compile to resource `"*"`. Within an action,
matching precedence is deterministic: more literal characters wins, then fewer
`*`/`?` wildcards wins, and an exact tie is won by the later declaration. The
universal catch-all `*` therefore carries the lowest specificity.
`${workspace_dir}` is the only permission-resource expression. It is accepted
only for `read` and `write`; bash
commands, delegation targets, unknown `${...}` expressions, and every other
brace form reject. The token remains literal in configuration, frozen policy,
serialization, and fingerprints. Its literal token characters count toward the
specificity metric, so ordering is stable across machines and checkout paths.

At permission evaluation, a resource pattern containing `${workspace_dir}` is
expanded against the engine workspace root and matched against the resource's
absolute path. A pattern without the token continues to match the existing
workspace-relative label. The workspace root uses the same normalization choice
as filesystem capability preparation: canonicalize the workspace directory
when possible, otherwise retain it, then render path separators as `/`.
Prepared read/write labels derive from canonical capability paths;
joining a relative label does not require the target to exist, which preserves
absent-write behavior. An expanded workspace pattern therefore cannot match an outside path; such rules
are accepted because config loading is checkout-independent, and simply do not
match outside resources. Ordinary absolute read/write patterns such as `/etc/*`
or `*/.ssh/*` govern outside paths. There is no environment-variable expansion
in permission patterns.

Agent schema 4 extends `mode` with `internal`. Internal agents are never
root-runnable, never valid `delegate` targets, and are filtered from every TUI
agent picker even though runtime snapshot descriptors retain them for coherent
discovery. They otherwise use the same document body, ordered model fallback,
enabled flag, and bounded `limits` object (`timeout_ms`, `max_input_tokens`, and
`max_output_tokens`). The reserved built-in internal documents are `approval`,
`compaction`, and `title`; same-ID user/workspace documents replace them through
the ordinary atomic layer precedence, with workspace winning over user and
built-in.

Only `mode: internal` fallback chains may contain the literal
`${parent_model}`. It carries no authored variant because it resolves at internal
policy freeze time to the parent run's exact active frozen binding, including
variant and manifest identity. Parentless resolution skips that entry; an empty
resolved chain uses the existing `unavailable` builtin lifecycle. Historical
title regeneration reconstructs the parent policy from `RunStarted` and its
persisted selected suffix, so `${parent_model}` never consults current model
selection. Approval and title internal model requests use the shared tool-less
request builder and always emit an empty tool list. Compaction is the deliberate
exception described in section 12: it preserves the parent request's tool
definitions for cache-prefix identity but rejects every non-text response.
Internal input limits cover the complete assembled history and tool definitions.
Approval enforces its authored limit against its single stateless request;
compaction raises its effective input ceiling to at least the largest
frozen parent-model context window in its resolved chain so a built-in 16,384
default cannot reject the context it was invoked to compact.
The built-in compaction chain begins with `${parent_model}`; the built-in
approval and title chains preserve the same parent-model behavior used before
schema 4. Title generation receives only the first user message, never assistant
answer text.

The approval internal agent is stateless and reasoning-blind. Every evaluation
uses exactly two history turns: the frozen composed approval prompt as the
byte-stable system turn, then one user turn containing a frozen framing wrapper,
the latest exact persisted `UserInputSubmitted` bytes, and the current tool name
plus normalized parameters last. The engine searches the current run newest-first
and then the full session newest-first; if no user input exists it inserts the
fixed `[no user message]` variant. It never supplies older messages, assistant
prose, tool results, or prior approval decisions. Keeping tool parameters at the
append-only tail preserves a cacheable `[system][latest user request]` prefix
across consecutive approvals. Approval decisions are accepted only from
pure-text model output; any tool call or other non-text part invalidates the
response and fails safe to escalation. Event schema 13 removes the obsolete
approval-conversation increment counter from `ApprovalEvaluated`.
The parameter object comes from tool preparation, not the model-authored JSON:
filesystem paths are resolved to prepared display paths, defaulted read bounds
are explicit, and bash is the whole command string. Thus the classifier and
permission pipeline inspect the same prepared operation semantics and raw
traversal spellings never reach the classifier.
`PreparedTool::new` requires this normalized-argument object at construction;
parameterless tools pass `{}` explicitly, so external providers cannot silently
omit classifier parameters. Construction is fallible and rejects JSON `null`;
non-null scalar forms remain valid for tools whose normalized schema requires
them.

Unmatched resources ask. Generic read allows do not override the built-in
default ask for `.env`/`.env.*`; exact or more-specific authored rules decide
naturally. A bare deny hides the corresponding tool. Map form hides it only
when `"*": deny` exists and there are no non-deny exceptions. `tools` remains a
separate allowlist and both gates apply.

Every tool implements two mandatory argument extractors. `get_primary_argument`
is the exact permission resource label: `read`/`write`/`edit` use the file
path, `bash` uses the full command, and
`delegate` uses the target agent id. After preparation the engine calls
`get_primary_argument` on the prepared `normalized_arguments` and uses that
string as the primary policy label; providers cannot keep a different prepared
label. `get_display_argument` is TUI-only compact-title display and must not
feed permission matching. The display forms are: `write`/`edit` use a
workspace-relative or home-abbreviated path; `read` uses that path plus any
explicit zero-based offset and/or limit window; `bash` uses a compact
one-line command that never elides `&&` segments, and `delegate` uses the
same agent id both ways. Malformed arguments fail closed. The approval
classifier still receives the full prepared `normalized_arguments` object.

`PreparedOperationIdentity` stores a non-empty resource vector. Its constructor,
deserializer, and `PreparedTool::new` reject an empty vector, so no tool can reach
permission evaluation without a resource; multiple resources remain available
for future operations. Permission evaluation combines them deterministically:
any deny wins, otherwise any ask wins, otherwise the operation is allowed. Each
resource label is the prepared `get_primary_argument` result. Filesystem labels
are workspace-relative for inside paths and absolute for outside paths; both use
the same read/write permission action and matching pipeline. Current built-ins
prepare exactly one resource per call.

The `bash` tool no longer reroutes simple commands onto `read`/`write`. The
permission engine only pattern-matches: each bash call is one resource whose
label is the whole command string from `get_primary_argument`, matched only
against `bash:` patterns. There is no AST split, no simple/complex
classification, and no parse-fallback resource. A narrow allow such as
`bash: "git *"` therefore also matches `git status && rm -rf x`; users who
want to block that must write containment patterns such as `*rm*`. That
smuggle is the designed semantic, not a bug. Shell file access such as
`cat .env` is governed by bash rules, not read/write rules.

The removed agent `delegation` field has no decoder. Delegation targets are the
keys of the `delegate` permission resource map, whose target effects are
`allow` or `ask`; targets must resolve to enabled `subagent`/`all` agents.
Runtime schema 10 owns `[delegation]`: `max_depth` defaults to 3 and
`max_concurrency` defaults to unlimited. A frozen child ceiling is the parent
ceiling bounded by runtime `max_depth`. Admission serializes the count of
delegated sessions in the root tree that currently have an active run; reaching
`max_concurrency` denies delegation and names the configured limit.

Managed `source = "models_dev"` definitions may directly author credentials in
either user or workspace TOML using `api_key` or typed `auth_override`. The
effective atomic definition's authored credential always takes precedence over
provider-store credentials. A workspace replacement never inherits a user-layer
credential. On recomposition, removing authored auth from a definition that has
no authored `base_url` makes an exact eligible provider-store credential
effective. If authored `base_url` remains, removing same-definition authored
auth is invalid rather than falling through to the store.

`${env:NAME}` interpolation is single-pass and allowed only in schema-approved
secret values and authored endpoint strings. Environment variables do not form
a config layer. Documentation recommends interpolation or `/connect` provider
store input rather than plaintext. Parsed and interpolated secrets live in
`SecretString`; owned buffers are best-effort zeroized on drop and after
dispatch. Transport/kernel copies cannot be promised forensic erasure.

## 5. Catalog architecture

The fixed request rejects redirects, cookies, configurable headers, auth,
userinfo, fragments, and every query. If cache schema 2 has a validated ETag,
only `If-None-Match` is added.

The request sends `Accept-Encoding: identity`. Any non-identity
`Content-Encoding` is rejected. A `Content-Length` greater than 16 MiB is
rejected before body read; absent or acceptable length is still streamed through
a hard 16 MiB counter before buffering, UTF-8 decoding, or JSON parsing. JSON is
bounded to depth 32, 4096 providers, 65,536 provider models per provider, 65,536
root canonical models, 1,000,000 total object/array entries, and 256 KiB per
string before narrower field limits.

Candidate boundaries are exact:

- the strict root is exactly `{ providers, models }`; both maps are required,
  bounded, and nonempty, and every other root field is unknown;
- invalid UTF-8, invalid JSON, non-object root, missing/non-object/empty
  `providers` or `models`, unknown top-level fields, root duplicate keys, or exceeded
  candidate/depth/container/string bounds is a **candidate failure**;
- once the bounded root provider map is structurally recoverable, each provider
  value is parsed independently;
- malformed provider-local shape, duplicate provider-local keys, invalid unique
  provider identity, or ambiguous normalized provider identity quarantines that
  provider and all of its models;
- within a valid provider, each model value is parsed independently; malformed
  model-local shape, duplicate model-local keys, invalid identity, or ambiguous
  normalized model identity quarantines only that model and all colliding peers;
- executable metadata is classified directly. Provider and nested
  model `npm` values classify protocol families; `api` and `shape` are authority.
  Unknown nested npm or shape values make only that model unsupported.

`providers` contains provider-scoped executable metadata: provider identity,
npm/API/env values, provider model records, and optional model provider
overrides. `models` contains canonical metadata/provenance keyed by canonical
model ID. It never supplies endpoint, protocol, auth, setup, package selection,
or executable inclusion. Provider model keys/embedded IDs and canonical model
keys/embedded IDs must agree. An exact provider-model key match to `models` is an
optional provenance cross-reference only; provider-scoped executable metadata wins
on disagreement, absence of a canonical record is allowed, and a malformed
canonical record quarantines only that canonical record/cross-reference.

Quarantine entries are safe diagnostics and never executable. Valid siblings in
the same candidate remain eligible. The candidate may publish when at least one
provider record is valid or safely quarantined with a recoverable unique ID.

Cache metadata schema 2 records the catalog revision exactly as
`sha256:<lowercase SHA-256 digest of the exact selected body bytes>`, ETag, byte length,
validation time, last-check time, source, stale flag, structural diagnostics,
and `last_error { code, safe_message, occurred_at } | null`. Network/cache/
bootstrap fallback updates stale/error metadata atomically even when body bytes
remain unchanged. Errors never contain bodies, URLs with secrets, or credentials.

## 6. Provider and endpoint architecture

All provider definitions live in `[providers.<id>]`:

- `source = "models_dev"`: optional `base_url`, `shape = "chat" | "responses"`,
  typed `setup`, `api_key`, `auth_override`, and sparse model overrides whose
  per-model fields may also include `shape`;
- `source = "custom"`: required `endpoint`, `adaptor`, `auth`, and nonempty
  explicit `models`; optional typed adapter `setup` and strict `headers`.

The top-level `providers` map defaults to `{}` and may be empty. It produces the
zero-model setup state only when provider store 3 is empty and no effective
authored custom provider exists; otherwise stored managed connections or
authored custom providers still materialize. Empty setup is not a configuration
error and must never produce `runtime providers must be nonempty`. Agent
fallback references with no current model are retained as unavailable
descriptors; their authored agents are not root-runnable. If no authored agent
is root-runnable but at least one model is available, the engine separately
materializes built-in agent `default` with the lexicographically first available
model and that model's default variant (base when there is no named default).
The synthetic agent disappears whenever an authored agent is root-runnable.

Custom IDs must begin `custom.`; managed IDs must not. Custom providers are
config-only, are not `/connect` rows, and never read or write provider store 3.

Custom static headers are non-secret behavior metadata with bounded RFC-token
names and control/CRLF/NUL-free safe values. They never interpolate and are
included exactly, sorted by canonical lowercase name plus value, in custom
behavior/config fingerprints and model-snapshot manifests. They reject
case-insensitive duplicates and transport/protocol/auth-owned names, including at
least authorization, host, content-length, transfer-encoding, connection,
proxy-authorization, cookie/set-cookie, accept, content-type, and user-agent.
Managed and custom model requests use opencode 1.18.2's provider compatibility
identity, exactly `opencode/1.18.2 ai-sdk/provider-utils/4.0.27
runtime/bun/1.3.14`, through the transport-owned user-agent header.
Adaptor recipes extend the owned set. Secret header auth must use a typed auth
method; static header values are never secret/redacted.

For managed providers the endpoint precedence is exactly:

```text
same-definition authored base_url > catalog api > npm-family default
```

Neither provider store, another config layer's replaced definition, nor ambient
environment values supply an endpoint. Catalog API metadata is authoritative.
`${VAR}` placeholders derive required setup fields and are substituted before
endpoint validation.

An authored `base_url` requires `api_key` or `auth_override` in that same atomic
definition unless the recipe's selected method is reviewed no-auth. It also
requires every non-defaulted setup field in that same definition; only setup
fields explicitly declared defaultable by the recipe may use recipe defaults.
It cannot inherit store setup or credentials. This blocks store or replaced-user
state from attaching to a new endpoint.

## 7. Authentication architecture

Managed auth precedence is exact:

```text
same-definition api_key mapped by the recipe default method
> same-definition auth_override
> exact active provider-store-3 connection only without authored base_url
> reviewed no-auth
> unavailable
```

`auth_override` and `api_key` are mutually exclusive. `auth_override` is exactly
`{ method = <AuthMethodId>, values = { ... } }`; keys must exactly equal that
auth method's credential schema. Provider `setup` is separate and owns only
typed endpoint/resource fields. `api_key` is legal only when the recipe has one
default auth method with exactly one required credential field classified
`api_key`. Multiple auth methods, any additional required credential, or a
differently classified credential is ambiguous and requires `auth_override`.

Provider-store auth is considered only when there is no authored auth and no
authored `base_url`. Its compatibility scope is provider ID, code-owned
`recipe_fingerprint`, recipe default normalized base URL/endpoint identity,
canonical safe setup fingerprint, and auth method/credential compatibility. For
nested npm families, compatible methods and semantic fields are mapped explicitly
(for example API-key to bearer API-key, or OAuth access token to bearer API-key).
Azure Foundry models classified as `@ai-sdk/anthropic` use the package's
`x-api-key` API-key behavior. Bedrock Converse uses SigV4 credentials, while
nested Bedrock Mantle Responses requires a Bedrock API key as bearer material;
models incompatible with the selected connection auth remain unavailable.
No endpoint,
host-family, method, provider-name, or identity fallback exists.

`source_record_digest` and `recipe_fingerprint` are independent secret-free
identities. `source_record_digest` is immutable provenance: SHA-256 over the
canonical exact validated catalog provider record selected to compile that
runtime or manifest. `recipe_fingerprint` is compatibility: a canonical hash of
Family registry 1 revision/schema, compiler version, selected family/adapter/
setup/auth/endpoint identity, and catalog-derived model semantics.
Provider store 3 persists both. A harmless catalog metadata refresh produces a
new source-record digest but the same recipe fingerprint, so the unchanged
stored setup and credentials remain eligible. Family, adapter, protocol, setup,
auth, endpoint-policy, compiler, registry, or model-exception drift changes or
quarantines the recipe identity and blocks reuse.

Managed setup precedence is complete same-definition authored `setup`, then
provider-store setup, then explicitly declared recipe defaults, then
unavailable. An authored setup map never field-merges with store or the replaced
user definition; it must supply every non-defaulted field, while recipe defaults
may fill only fields declared defaultable. Provider store 3 stores normalized
non-secret setup values and secret auth credentials plus policy/scope, exact
source-record provenance, recipe compatibility fingerprint, generation, and
receipt metadata. Store state revisions and connect receipt digests cover both
fingerprints. Any non-schema-3 store, including `store-v2.json`, is rejected.
Custom setup is authored-only and custom compilation/
fingerprints/rehydration never depend on provider store. Every family-registry setup
field is non-secret behavioral metadata and its normalized value participates
directly in safe behavior/config fingerprints where relevant. Every secret or
sensitive value belongs to auth, is excluded from fingerprints, and may rotate
without behavior-fingerprint change. A future non-auth sensitive setup need
requires a future schema/recipe version and independent mechanism.

If a managed definition has no authored `setup` and no authored `base_url`, its
exact stored setup may become effective. Removing authored setup, auth, and
`base_url` on recomposition permits exact stored setup and stored auth to become
effective. An authored `base_url` disables all stored setup and stored auth
inheritance.

## 8. Providers and models

Managed provider support is `npm`-family based. The static registry is keyed by
the package values documented in `docs/provider-conformance.md`; unknown package
families use `no_known_protocol_family`. Each model first applies its nested
`provider { npm, api, shape }` override. Unknown nested families affect only that
model. OpenAI defaults to Responses; authored provider/model shape may select
Chat or Responses, and catalog `responses`/`completions` shapes route likewise.

Capabilities are derived from catalog `tool_call`, `structured_output`,
`temperature`, `reasoning`, `reasoning_options`, input modalities, and context/
output limits, plus protocol-required native replay. Anthropic-compatible models
with reasoning require native replay so signed thinking blocks survive tool-use
continuations. Interleaved reasoning metadata selects the compatible reasoning
field. Deprecated and non-text-output models are omitted.

`/connect` contains every current structurally valid/quarantined catalog provider
with recoverable ID plus authored or store-backed managed providers absent from
the current catalog. Removed configured providers use state `removed`; they are
connectable only when family registry 1 still has an exact family/package/
source recipe match retained in a prior durable connection or other validated
safe source projection. Otherwise they remain visible with
`removed_without_retained_recipe_match`. Custom providers never appear.

For each supported managed provider, the runtime automatically includes every
non-deprecated, text-output model that compiles honestly. Sparse model
overrides may disable a model or alter only recipe-approved defaults, variants,
and display text. They cannot invent an absent model, capabilities, protocol,
auth, package, or endpoint.

Custom providers manually define every model and complete honest capabilities.
A custom model ID may contain `/`. A model key splits once at its first slash.

## 9. Connect, disconnect, and publication

`provider.connect` requires an expected catalog revision exactly equal to
`sha256:<lowercase SHA-256 digest of the exact selected body bytes>`. It accepts current managed providers and
configured managed providers absent from the catalog when recipe/source matching
succeeds. Absence of a prior store record is a normal upsert.

The service holds the serialized runtime-mutation lock, then the provider-store
lock; rereads the store; assigns the final store revision, connection generation,
and receipt into a proposed state; validates exact recipe-typed setup and auth
fields; and compiles the complete provider/model/agent/runtime candidate against
exactly that proposed state. Only then does one provider-store transaction write
normalized setup, credentials, connection policy metadata, and idempotency
receipt. After durable commit, publishing the already compiled
`Arc<RuntimeSnapshot>` is an infallible atomic swap. No fallible refresh or
revision allocation occurs between durable write and publication.

The response contains `durable_connection`, `effective_auth_source`, the full
coherent runtime snapshot, and `replayed`. A stored update never outranks
same-definition authored auth; therefore a successful durable update may report
effective source `authored_api_key` or `authored_override`.

`provider.disconnect` is managed/store-only. It removes the exact stored active
setup and credentials with the connection and commits its receipt atomically,
compiles before commit, then
infallibly publishes. It never edits TOML, removes authored auth, touches custom
providers, or deletes a different scope. Custom disconnect returns typed
`custom_provider_not_store_backed`.

Disconnect params contain provider ID, client idempotency ID, expected runtime
and provider-state revisions, and optional expected connection generation. Same
ID/same canonical payload replays the durable result; same ID/different payload
is `idempotency_conflict`. An absent managed provider is a successful idempotent
disconnect with a durable receipt. The result includes receipt, provider ID,
`disconnected = true`, post-removal effective auth state, coherent
`RuntimeSnapshotResult`, and `replayed`. Present removal or absent no-op plus
receipt is one atomic store transaction after candidate compilation, followed by
infallible publication with `ProviderDisconnected`; replay publishes nothing.

## 10. Runtime snapshot and notifications

Runtime snapshot schema 3 contains the snapshot schema version, recipe registry
revision, catalog revision/source/cache state, provider-state revision, model
revision, provider-store generation, agent revision, aggregate runtime revision,
provider descriptors, model descriptors, and materialized agent descriptors.
Each model descriptor carries ID-sorted variant metadata plus a `variant_order`
permutation containing every named variant exactly once. Base is implicit and
precedes that list when cycling. Managed order follows catalog option traversal:
effort values retain list order, toggles emit `off` then `on`, and token budgets
emit minimum/automatic before maximum; duplicate generated IDs retain their
first position. When generated toggle `on` coexists with any generated explicit
effort or token-budget level, compilation removes `on` from the generated map
and order as redundant. This suppression precedes managed override directives,
so an authored addition can intentionally restore `on`. Managed authored
additions append in directive key order, replacements retain position, and
disables remove the position. Custom variant definitions are TOML table maps
decoded into `BTreeMap`, so their declared semantic order is variant-ID key
order.
Materialized agents include authored descriptors unchanged plus the conditional
built-in `default` descriptor when the runtime has available models but no
root-runnable authored agent.

`runtime.snapshot.get` is mandatory and atomically returns the entire object.
Protocol 9 removes legacy independently refreshed catalog/provider/model/agent
list RPCs and list-refresh choreography. Every mutation returns the published
snapshot. The server emits `runtime.changed { previous_revision, snapshot,
reasons }`; Rust reason variants and wire values include
`ProviderConnected`/`provider_connected`,
`ProviderDisconnected`/`provider_disconnected`,
`ProviderStoreChanged`/`provider_store_changed`, and
`ProviderStoreReloaded`/`provider_store_reloaded`, in addition to startup,
catalog, config, and agent reasons.

The local durable connect transaction publishes `ProviderConnected`; local
disconnect publishes `ProviderDisconnected`. Detecting another process's newer
store generation publishes the pair `ProviderStoreChanged` and
`ProviderStoreReloaded` after successful coherent recompilation.

Provider store 3 is per-user global across workspaces. Before discovery RPCs,
session admission, or root-run admission, the process locks and rereads the
store generation. A changed generation blocks admission, recompiles and
publishes a coherent runtime with both `ProviderStoreChanged` and
`ProviderStoreReloaded`, then retries against that snapshot. Reload failure
returns `provider_store_reload_failed`; accepted runs remain unchanged.

## 11. Project model-snapshot manifests and exact rehydration

Project model-snapshot manifest schema 1 is stored only at
`<exact-cwd>/.cookie-agent/model-snapshots/<64-lowercase-hex>.json`, with exact
lock `model-snapshots-v1.lock`. The filename digest and manifest
`revision = "sha256:<digest>"` are SHA-256 over RFC 8785 JCS bytes of the
self-contained secret-free `payload` only; envelope schema/revision are excluded
to avoid self-reference. Each manifest contains catalog, recipe
registry, provider-state, and model revisions plus compiled safe blueprints.

Each blueprint records exact model/variant selection, descriptor/capabilities,
safe endpoint identity, provider/protocol/auth recipe IDs and compiler versions,
source kind, exact source-record digest, independent recipe fingerprint,
config-override fingerprint, credential
binding source/method/semantic field names, normalized defaults/options/variants,
and behavior fingerprints. Custom static header names/values are included as safe
behavior metadata. Auth credential values, generated auth-owned header values,
environment values, live handles, raw catalog records, and provider-native
private payloads are forbidden.

The cwd anchor need only be an actual directory and may be shared or writable;
the `.cookie-agent/model-snapshots` subtree is current-user-owned `0700` and
manifests/lock/temp files are current-user-owned `0600`, regular, single-link,
descriptor-relative/no-follow, bounded, and atomically written by lock/reread,
exclusive sibling temp, fsync, rename, and parent fsync. A manifest is durable
before any event/session JSONL 14 record may reference its revision. Referenced manifests are
retained for the lifetime of their sessions and delegation journals and are
never garbage-collected; family registry 1 performs no automatic manifest GC.

Startup scans direct matching filenames in sorted byte order, validates filename
digest, strict schema, RFC-8785 reserialization/payload digest, unique blueprint
fingerprints, and bounds, then indexes them by revision. Maps use JCS ordering;
models/variants/setup/binding-name arrays are ID-sorted, semantically ordered
arrays are preserved, JSON floats are forbidden, decimal domain values are
normalized strings, integers are I-JSON-safe, and Unicode code points are
preserved without normalization.
Unsafe objects or malformed matching
files fail project open. Session/journal references to absent manifests fail
reconciliation without deleting history.

Protocol-9 frozen bindings reference one manifest revision/blueprint and record
the accepted exact selection. Their managed source carries both the immutable
source-record provenance and recipe compatibility fingerprint. Runtime provider
descriptors expose the safe recipe fingerprint, and persisted events/delegation
journals retain it transitively through their exact frozen bindings. No secret
value is stored.

Credential source is frozen as `authored_api_key`, `authored_override`,
`provider_store`, or `no_auth`. Rehydration never changes it:

- authored config sources require the current same-ID atomic provider definition
  to have the same source kind and safe config-override fingerprint; missing or
  changed config fails and never falls to provider store;
- provider-store sources require the exact managed provider, current
  `recipe_fingerprint`, recipe endpoint policy, normalized setup values/
  fingerprint, and auth method/shape; config is not substituted. They do not
  require the store connection's historical `source_record_digest` to equal a
  newly refreshed manifest's source digest;
- managed bindings reconstruct from persisted safe source projection only when
  family registry 1 still has the exact family fingerprint/package/protocol/
  compiler match. The binding and referenced blueprint must retain their own
  exact source-record digest, but source provenance does not scope credential
  compatibility; current catalog presence is unnecessary;
- custom bindings require a current `source = "custom"` definition whose safe
  definition fingerprint exactly matches the frozen fingerprint; custom has no
  store fallback;
- every reconstruction must reproduce the frozen behavior fingerprint.

New root runs always resolve against the current coherent runtime and durably
write/reference its manifest. A harmless catalog refresh therefore freezes the
new source-record digest while retaining the same compatible store credentials.
Previously accepted runs retain their older manifest/source provenance and may
rehydrate the same store secret through the unchanged recipe/auth/setup identity.
Delegated sessions remain pinned to the invoking parent's accepted manifest and
frozen suffix. Once a run is accepted, catalog, config, provider-store,
manifest-directory, or runtime changes do not reinterpret it.

Typed failures leave history readable and never substitute another model:
`snapshot_config_mismatch`, `snapshot_credentials_unavailable`,
`unsupported_snapshot_recipe`, and `snapshot_rehydration_mismatch`.

## 12. Context compaction

Runtime schema 10 exposes `context_compaction.auto` (default `true`),
`buffer_tokens` (default 33,000), and `max_summary_bytes` (default 262,144).
The effective trigger is `context_limit - buffer_tokens`, saturating at zero.
Provider-native compaction and its authored capability are not part of the
runtime; the Oven descriptor boundary is always frozen as unsupported.

Two automatic signals feed one compaction path. The real-usage signal compares
the latest committed turn's `input_tokens + output_tokens` with the effective
trigger. The pre-send predictor retains the learned per-session tokens-per-byte
ratio: after a committed turn, nonzero reported input tokens divided by the exact
serialized history byte length replaces the ratio. At pending-input promotion,
each input is projected in admission order against the same effective trigger;
on crossing, compaction commits before any input in that promotion batch is
submitted. A committed checkpoint resets the estimator baseline to its estimated
post-compaction input size while preserving the learned ratio. A zero ratio does
not predict. `auto = false` disables both automatic
signals, while forced compaction and context-overflow recovery remain available.

Compaction first stages old bulky completed tool outputs. Results attached to
the newest two model turns are protected. Older outputs of at least 8 KiB are
retained in the artifact store and represented in model history as
`[tool output elided; retained at <artifact-uri>; <original-bytes> bytes]` via a
durable `ToolOutputElided` event. Elision occurs only after a complete tool
call/result pair and checkpoint boundaries never split the pair. If the reduced
request estimate is below the trigger, automatic compaction stops without a
paid summarizer call.

The summarizer request is a fork of the exact next normal request: the assembled
system prompt and history are unchanged, the same tool definitions remain, and
one fixed detailed-summary user instruction is appended last. This compaction
fork is the sole exception to the structural no-tools rule so its request prefix
is byte-identical and cacheable. Any returned tool call or other non-text part is
invalid internal-agent output and is never dispatched. Optional forced-focus
text is appended only after the fixed instruction.
The TUI `/compact [focus]` command calls strict `session.compact` for the
selected idle session; its RPC focus field is required, nullable, and bounded.
Manual, start-time predictive, and promotion-time predictive compaction reserve
the session inside its actor, then run provider and tool futures outside the
actor. While reserved, concurrent run starts and explicit compaction are
rejected, but steering and recall remain cheap serialized actor appends. A steer
is therefore admitted during compaction and is included before reservation
release; promotion commits only after the checkpoint and replays the lane again
so recalls during compaction are honored. This preserves pre-send/checkpoint
ordering without blocking model stream appends, input admission, recall, or
cancellation behind a slow summarizer. Manual compaction uses
the run's persisted active fallback index when resolving `${parent_model}`.
Barrier-sensitive `PromptSnapshot`, `PromotePendingOrComplete`, and `Resume` commands
wait behind that completion barrier. The deferred set is bounded to one command
of each kind and retains FIFO order among those first surviving commands. A
duplicate snapshot or resume receives the same session-running retry signal used
by competing start/compact requests, while a duplicate completion reports that it
did not complete the run. Keeping the first command prevents a later observer
from displacing the model loop's already-queued context barrier.
For start-time prediction, the durable `RunStarted` event and the real active-run
cancellation token are installed before the summarizer starts. Cancellation is
therefore accepted during prediction, aborts the internal agent, records
`RunCancelled`, and prevents the pending initial user input from being appended.

The validated summary replaces the covered range as one user turn. The system
turn remains index zero. The framing is frozen as
`This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n<summary>\n{summary}\n</summary>\n\nPlease continue the conversation from where we left off without asking the user any further questions.`
Pending input remains after the checkpoint. Commit validation keeps monotonic
boundaries and requires strict estimated shrinkage.

After commit, the engine scans completed tool lifecycles newest-first and trusts
only paths from persisted assistant tool-call parts whose exact originating tool
identity is `read`. Up to five distinct candidates are re-prepared through the
normal read provider, evaluated by the frozen permission pipeline, and executed
through the prepared capability so path or symlink changes fail closed. Each
readable UTF-8 file is capped at 32 KiB and the aggregate is capped at 128 KiB.
Denied, changed, missing, unreadable, and non-UTF-8 files are skipped.
`ContextRehydrated` durably records the bounded content; history projects it as
ordinary synthetic `read` tool calls and tool results after the summary turn.

A provider `ContextLength` failure with no meaningful output abandons the
attempt, forces this same compaction path, and retries the turn exactly once; a
second overflow is surfaced. After a checkpoint, the first later real-usage
sample is the anti-thrash check. If it remains at or above the trigger,
automatic compaction is latched off in memory for that session and a durable,
user-visible `ContextCompactionAutoDisabled` notice is emitted. Forced/manual
compaction remains available.

## 13. Normative startup order

Startup order is frozen and must not be reordered:

1. **Schema 10 and agents:** securely open roots, load family registry 1, then
   strictly load atomic config schema 10 and agent schema 4 documents.
2. **Catalog:** securely open cache schema 2, perform the bounded identity-only
   network request, then select network, validated cache, or bundled bootstrap
   and apply record quarantine.
3. **Provider store:** lock and load provider store 3 and its generation.
4. **Effective providers:** combine authored definitions, global stored managed
   connections, catalog metadata, and code-owned recipes.
5. **Coherent runtime:** compile all effective providers/models/authored agents;
   if models are available but no authored agent is root-runnable, synthesize
   built-in `default`; then build one runtime snapshot 1. An empty effective
   model set is valid.
6. **Project manifests:** scan/validate model-snapshot manifests 1 and rehydrate
   every referenced safe blueprint needed by project sessions.
7. **Engine:** open version-11 session/event state and version-9 delegation state and reconcile it
   against the manifest index; accepted/delegated bindings stay pinned.
8. **Service:** atomically publish runtime, open server/TUI, then emit startup
   `runtime.changed`.

Any failure before step 8 opens no server/TUI except the documented usable
catalog fallback paths. Cross-process provider-store generation reconciliation
is mandatory again before discovery, session admission, and root-run admission.

## 14. TUI contract

The TUI consumes only runtime snapshot schema 3 and `runtime.changed`. Required
global/row states are:

Within the transcript, one assistant block spans all model attempts in one run;
its header is frozen from the first attempt, and a mid-run resolved-model change
adds a subtle inline `now using …` row. Committed turn content supersedes its
streamed deltas, while abandoned attempts contribute no durable text or
thinking content. The TUI state model represents these boundaries with
`AssistantChild::Attribution`, and committed tool placeholders carry
`CommittedTool.turn_seq` alongside their content index so tools from different
turns cannot alias. A completed block closes with one muted, passive footer row
(`╰─ ⚡ 42.0 tps · 12.5K ctx`): committed output tokens over generation wall
time measured between durable event timestamps (the input-closing event at
`input_through_seq` and the commit itself, so replays render identically), and
the last committed turn's end-of-turn context (`input_tokens + output_tokens`)
in the bottom bar's K convention.
Blocks without usage data or a positive generation span render no footer.

- `loading`: no snapshot yet; controls disabled;
- `empty`: after authored providers and global store records are applied, a valid
  snapshot has zero available models. The
  Message model/draft display is exactly `type /connect to continue`; it has no
  Model or Variant hit region, ordinary text/run submission is blocked with the
  same guidance, and `/connect` remains accepted;
- `ready`: live or not-modified catalog selected with no unresolved global error;
- `stale`: validated cache used after refresh failure; existing rows remain
  usable and a durable global explanation names the safe error/time;
- `bootstrap`: bundled catalog used; durable global explanation remains visible;
- `unsupported`: row has typed unsupported reason; Enter opens details only and
  never starts connect;
- `disconnected`: supported managed row lacks complete effective setup and/or
  auth; Enter opens separate public setup and secret credential controls from
  recipe descriptors;
- `connected-reconnect`: effective stored state exists; Enter opens reconnect/
  update with public setup prefilled and secret credentials blank; disconnect
  removes both stored setup and credentials;
- `removed`: configured managed provider or retained session model is absent
  from current catalog; details explain whether recipe-matched connect or session
  rehydration is available;
- `error-retry`: no runtime snapshot or an operation failed without a usable
  fallback; Enter/retry performs the explicit operation.

Global stale/bootstrap/error explanations are durable application state, not
toasts, and remain visible across picker navigation until a newer snapshot
clears or replaces them. Active frozen runs never mutate on refresh/connect/
disconnect. Unsupported Enter is details-only.

`loading`, `empty`, and `error-retry` are distinct: loading has no snapshot;
empty has a valid coherent zero-model snapshot and no runnable draft; error-retry has no
usable snapshot or a failed explicit operation. After connect publishes a
coherent snapshot with any available model, an authored root-runnable agent or
the synthetic built-in `default` supplies normal structured draft attribution
and activates Model/Variant hit regions. No model is fabricated; authored
fallbacks remain unresolved and unchanged.

The built-in `default` agent has source `built_in`, mode `primary`, the standard
coding tools (`read`, `write`, `edit`, `bash`), no delegation
targets, and the same action-keyed ordered permission map as authored agents.
Workspace reads are allowed; outside reads, write/edit, bash, and delegate ask;
reads of `.env` variants, `store-v3.json`, `token-v1`, `id_*`, `.netrc`, and
`application_default_credentials.json` are denied, with the existing exact
`.env.example` read exceptions. `default` is a reserved authored-agent ID.

`/connect` always displays the exact durable copy
`Stored setup, connections, and credentials are per-user and shared across workspaces.`
Stored setup is non-secret and projected only where recipe policy permits;
credential values remain secret/redacted.
The provider picker renders a reusable, focused single-line `Search` input
above the provider list and filters every visible provider state by
case-insensitive substring over display name and provider ID. The input accepts
Unicode key and paste events; Left/Right and Home/End move its grapheme-safe
cursor, Backspace/Delete edit at the cursor, and Ctrl-U clears it. Down, Tab,
or Enter transfers focus from input to results; Up from the first result,
BackTab, or Esc transfers focus back to input. Enter activates only a focused
matching result, and Esc from the input closes the picker and clears the query.
The list title shows filtered/total count, a new `/connect` always starts with
the full unfiltered list and input focus, and an empty result is rendered
explicitly with no selected row.
The provider connect form has one focus order: a selectable auth-method row
when multiple descriptor methods exist, the selected method's credential input
boxes, every projected setup input box, and Submit. A sole auth method is
displayed read-only. Left/Right or Space cycles a focused method and
best-effort wipes/rebuilds its credential buffers so values never cross method
boundaries. Tab/Down and Shift-Tab/Up traverse the form; Enter dispatches the
form from every focus position, matching Submit; Esc cancels and wipes
secret-bearing buffers. Pointer interaction mirrors the same contract: clicking a credential
or setup box focuses it and places its cursor at the clicked cell, clicking the
auth-method row cycles the method with the same wipe/rebuild, clicking Submit
dispatches, and hovered controls highlight without moving focus. All boxes use
the shared grapheme-safe input state and accept Unicode
typing and paste, cursor movement, Backspace/Delete, and Ctrl-U. Credential
values and setup descriptors with `safe_to_project = false` are bullet-masked;
the latter includes recipe-projected KEY/TOKEN/SECRET placeholder fields.
Connect is store-and-go and performs no TUI-side provider request or credential
test. The form states `Credentials are verified on first use.` because validity
is exercised only when a conversation invokes the provider. Client-side
validation failures keep the form open with the modal and focus unchanged and
surface the error inline until the next edit, auth-method change, or submit
attempt. A failed connect
RPC opens a persistent full-message error state rather than a transient notice;
Esc returns to the retained form for correction and retry. Public setup inputs
remain populated, while credential and secret-classified setup inputs are
best-effort wiped when the request is dispatched. User-facing JSON-RPC error
formatting includes the RPC code/message and only scalar `data.code` and
`data.message` fields; arbitrary error data remains redacted.
Connect/disconnect changes become visible to other workspace daemons through
provider-store generation reconciliation.

### 14.1 Live per-session permission mode

Every session has a live permission mode, defaulting to `auto_approve` when no
explicit value has been set. `auto_approve` invokes the stateless approval
classifier and escalates when that agent asks, emits malformed output, times out,
or fails. `ask` skips the internal
approval agent and routes every policy-ask or model-requested approval through
the durable escalation transaction and user modal. `yolo` skips both the
internal agent and escalation, durably appends `ApprovalEvaluated { allow,
source: policy, reason_code: yolo_approved }` followed by
`ApprovalFinalized { approved, source: policy, reason_code: yolo_approved }`,
and resolves the operation immediately. `policy` is the decision source because
the mode is a live policy override rather than an internal-agent or user
decision; protocol coherence permits `yolo_approved` only for policy-sourced
Allow/Approved decisions.

The mode short-circuit occurs inside `Engine::await_user_approval` only after
the doom-loop guard, prior tree-grant lookup, and hard policy-deny evaluation.
Consequently deny rules and doom-loop rejection take precedence under every
mode. Existing pending user escalations are unaffected by a later mode change;
the new value applies to subsequent approval evaluations immediately. Modes are
keyed by the approval's own session ID. A delegated session therefore uses its
own default or explicitly set mode, and a root session's mode never cascades.

The JSON-RPC method `session.set_permission_mode` accepts
`{ session_id, mode }`, where `mode` is `auto_approve`, `ask`, or `yolo`, and
returns an empty success object. The engine validates that the session exists
before updating its live in-memory mode. The TUI mirrors values per session for
display while the engine remains authoritative. Its bottom bar renders the
clickable mode immediately left of context usage, for example
"auto-approve    ctx 48.2K (24%)    `ctrl+p` commands", and clicking cycles
`auto-approve → ask → yolo → auto-approve`. The context value is the
end-of-turn total (`input_tokens + output_tokens`) of the latest committed
turn, hidden when the turn reported no usage, with the percentage taken
against the model context limit. Narrow layouts remove the command
hint, percentage, and context value in that order while retaining the mode
control; the working-directory field truncates into the remaining left space.

### 14.2 Pending-input lane strip

A prompt submitted while a run is active takes the `run.steer` path. The engine
admits steered inputs into a per-session pending-input lane (admission succeeds
even while a compaction reservation is held) and reports the lane through
events: `UserInputAdmitted { input }` adds a pending entry,
`UserInputSubmitted { input }` promotes one to the model-facing log (the
transcript user row renders here, as always), and `UserInputRecalled { input }`
withdraws one. The TUI reduces these into `SessionState.pending_inputs`
(text + durable admission timestamp) as a pure event projection — no
client-side FIFO text-matching and no send-retry logic — so live streams and
replays build the lane identically. The reduction is strictly positional,
mirroring the engine's own replay: promotion pops the oldest entry,
recall pops the newest, and payload text is never consulted. A steer that
fails at the transport level restores its text into the composer (parked
per session when another session is being viewed).

While a session's lane is non-empty, a strip renders between the conversation
pane and the status line; its rows are reclaimed from the conversation like
composer growth, the status line and composer stay pinned, and the
conversation keeps at least one row. The strip is a bordered block in the
standard panel chrome (crust border, muted text) titled
`Pending · oldest <age>` with a coarse age label (`<1m`, `Nm`, `Nh`). Each
entry is one ellipsized, newline-flattened line with a 1-based index and a
muted `⏳`; at most three text rows show, with overflow folded into a `+N more`
row. The strip's meaning is exactly "the model has not seen this yet".

Entry rows are hoverable and clickable because recall is a real action:
clicking any row, or pressing Up in an empty composer with a non-empty lane,
calls `run.recall_steer { run_id }`, which withdraws the engine's newest
pending input and returns its text. The returned text restores into the
composer for editing (parked per session if another session is being viewed,
restored on selection); the `UserInputRecalled` event removes the entry from
the strip itself. A `recalled: null` result means the lane raced ahead (a
promotion landed first) and only updates the status.

Run end (completed, failed, cancelled, interrupted) voids any still-pending
inputs without per-entry events. The reducer moves their text aside into
`SessionState.voided_inputs`, the strip clears, and the UI drains the voided
text into the composer the moment its session is viewed — user text is never
silently lost.

### 14.3 User-message action menu

Clicking a past `USER` row opens the message action menu; assistant and tool
rows never open it and keep their expand/collapse toggle. The menu is a
small picker-style modal with three rows, driven by keyboard (↑↓/enter/esc
plus the `c`/`r`/`f` accelerators) and clickable, hoverable rows:

- **copy** sends the raw message text to the clipboard (§14.4).
- **revert** is confirm-guarded. Confirming calls
  `session.revert { session_id, through_seq = seq - 1 }`, rolling the
  visible branch back to just before the message — the message and every
  later turn leave the visible branch while the append-only log is kept.
  On success the message text restores into the composer for editing and
  resending (parked per session when another session is being viewed).
  The transcript rebuild rides the `SessionReverted` event; the tree
  refreshes because title, status, and usage are branch-derived.
- **fork** calls `session.fork { session_id, through_seq = seq }`, keeping
  the message inside the copied prefix, then selects the new session
  (rerooting when it lies outside the current tree).

The menu targets the physical sequence stored on the transcript item
(`TranscriptItem::User.seq`, the `UserInputSubmitted` sequence) and captures
the message text when the menu opens, so a concurrent rebuild can neither
retarget the action nor change what copy/revert operate on.

### 14.4 Mouse text selection and clipboard

A left-button press inside the conversation viewport or the composer text
rect becomes a pending press; motion beyond one cell promotes it to a text
selection, and a release without promotion dispatches the plain click
(block toggles, the message menu, cursor placement) exactly as before.
Scrollbar presses and overlays never start a selection; overlay ownership
is read from state (modal, palette, approval), not from stale hit
geometry, so a panel that opened since the last frame still owns its
presses. Selections are stored in content coordinates — conversation
`(logical line, display column)` into the rendered lines, composer buffer
bytes — so they survive scrolling while held. The highlight is a pure
cell-style patch over the rendered cells in the themed `text_selection`
wash: background-only where subtle color exists (ANSI-256, true color), so
foregrounds and code highlighting are preserved, and visually distinct
from the keyboard `selected` row. ANSI-16 and high-contrast targets are
the one exception: their text is always a bright color that a light wash
would swallow, so the wash is pinned to a fixed black-on-light-cyan pair
there. No-color targets use bold reverse video.

`ctrl+c` copies the selection and clears it; with no selection it keeps
its run-cancel meaning. In the composer, `ctrl+x` additionally cuts the
selected bytes from the draft. Esc or any plain press clears the
selection without side effects; that Esc does not count toward the
double-Esc run cancel.

Extraction maps the coordinates back to real text, one copied line per
rendered row. Conversation rows strip exactly their gutter spans (role
gutters, quote bars, wrap continuations, narrow-mode tags). A row vanishes
when it is gutterless header/border chrome (role headers, attribution,
footers) or when every remaining span carries the code/table border
signature (fence headers, table grids). The signature is the border's
foreground *and* full added-modifier set, compared exactly: the parchment
band only patches backgrounds, and in high contrast syntect's quantized
plain-code foreground equals the border's white, so only the border's
DIM|BOLD set — which syntect never emits — keeps single-color code rows
from vanishing as chrome. Copied code is therefore the raw source with no
band or glyph chrome, blank rows inside the range stay as paragraph
breaks, and leading/trailing blanks drop. The composer leg slices the
draft buffer between the mapped byte offsets.

The clipboard write is an OSC 52 escape (`ESC ] 52 ; c ; <base64> BEL`)
emitted to the terminal: no platform dependency, and it works over SSH
because the terminal emulator — not the remote host — owns the clipboard.
Terminals without OSC 52 support ignore the sequence; neither arboard nor
copypasta is carried. Copy feedback is a status-line note.

## 15. Validation ownership

Required validation covers every version rejection, secure file mode/ownership/
link attack, exact startup order, identity-only streamed 16 MiB acquisition,
ETag and stale/error metadata, candidate versus exact-record quarantine,
strict root `providers`/`models` roles and cross-references, bundled/live npm/API/
env/model-override metadata, Vertex family rejection, atomic provider replacement,
separate provider setup schemas and auth methods, complete custom-model validation, endpoint/auth
precedence, all-input-query rejection, connect/disconnect absent/replay/conflict
transactions, cross-process store-generation reload, infallible publication,
coherent notifications, RFC-8785 manifest digest/ordering/mismatch/retention/
rehydration, slash-containing IDs, frozen credential-source rehydration, and
every TUI state.

TUI validation must exercise the actual rendered/input buffer and hit map, not
only helper strings: absent/empty provider TOML plus empty provider store and no
effective custom provider; nonempty store with empty TOML; zero-model snapshot;
byte-exact guidance; no Model/Variant target; blocked ordinary submission with
no run RPC; accepted `/connect`; all-provider discovery RPC; and guidance removal
after a coherent model-bearing refresh, including selection of synthetic
`default` when authored agents are absent or unrunnable. Engine tests must prove
the synthetic agent's iff trigger, reserved ID, first-available/default-variant
fallback, disappearance when an authored agent is runnable, permissions/source,
and successful session admission. Connect tests must cover recipe-derived setup
and auth descriptors, multi-method selection and per-method credential resets,
boxed public/secret controls, masking, Unicode input, focus traversal, submit and
cancel behavior, persistent full connect errors with public-value retry state,
first-use verification copy, defaults, missing/extra rejection,
absent-provider upsert, reconnect replacement, cross-workspace setup persistence,
and disconnect removal of setup plus credentials. Server tests must prove empty
zero-model startup and typed run rejection without `runtime providers must be
nonempty`.

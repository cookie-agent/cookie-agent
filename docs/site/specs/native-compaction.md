# Native compaction: parallel summaries and default on

Status: **implemented** (2026-10-24); approved 2026-10-24, with the
deviations in section 10. Revises [Compaction](../sessions/compaction.md)
(native provider compaction).

## 1. Goal and scope

**Native compaction is opt-in today.** A model override sets
`compaction = "openai-responses-compact"` or `"azure-responses-compact"`, and
the setting works only on the managed `openai` and `azure.openai` providers.
A successful native compaction stores an opaque provider window as the
session's checkpoint.

**That window locks the session to one exact model.** Every later request
checks it against its own model binding (`ContextCheckpointCommit::validate_for_binding`)
and fails the run with a corrupt-history error (`NativeSelectionMismatch`) on
any mismatch. So switching models in the picker, or the fallback chain moving
to its next model, fails every request from then on. The full event log
behind the checkpoint is kept, but nothing falls back to it.

**This spec:**
- runs native compaction alongside the internal summary;
- uses the window only for the same provider and model;
- turns native compaction on by default for every Responses-API provider.

## 2. Decisions (2026-10-24)

1. **Parallel compaction.** Native compaction and the internal summary agent
   run at the same time. The checkpoint holds the summary, plus the provider
   window when native succeeded.
2. **The window only for the same provider and model.** A request whose
   model has the same provider ID and model ID as the window uses the window.
   Every other request uses the summary. A variant change within the same
   model keeps the window.
3. **A window rejected at request time fails that request.** The provider is
   then marked as rejecting windows (section 5), so later requests use the
   summary instead.
4. **On by default for every Responses-API provider.** This covers managed
   OpenAI, Azure OpenAI with deployment metadata, and OpenAI-compatible
   gateways. `compaction = "unsupported"` turns it off for a model.
5. **Remembered per provider ID** in `<data_root>/native-compaction.json`
   (section 5).

## 3. Compaction

**When native compaction is attempted.** It is attempted when the compacting
model:
- uses a Responses adapter (OpenAI Responses, Azure OpenAI Responses, or an
  OpenAI-compatible Responses endpoint);
- does not set `compaction = "unsupported"`;
- has a provider that section 5 does not mark unsupported;
- on Azure, has `model`, `version` and `deployment_type` in the provider's
  `setup` table, as today. Azure without them is not attempted.

The internal summary always runs.

**Both start together.**
- **Native succeeds:** the checkpoint commits the summary and the window.
- **Native fails:** the checkpoint commits the summary alone, and the failure
  is classified for section 5. A failed native attempt delays the checkpoint
  only by its own duration, which overlaps the summary call.
- **The summary fails:** the checkpoint fails as it does today, even if
  native succeeded. Every checkpoint carries a summary.

**Recent messages.** Native and summary checkpoints keep different recent
history today: a native window keeps no independent recent tail. The
committed checkpoint records the summary's boundaries (`source_through_seq`,
`recent_from_seq`) and the window's own (`source_through_seq`). Each request
path selects events by the boundaries of what it uses.

**Cancellation and hooks.** Cancelling compaction cancels both calls.
Compaction hooks see one compaction, as today.

## 4. Requests

**Choosing the window or the summary.** When a request is built from a
checkpoint holding a window, it uses the window only if the request's binding
has the same provider ID and model ID as the window's scope. It uses the
summary when any of these hold:
- the binding names another provider or model;
- the window's provider is marked as rejecting windows (section 5);
- the window fails its integrity checks.

This replaces the current `NativeSelectionMismatch` failure. A
summary-only checkpoint behaves as today.

**Scope checks.** The window's integrity checks (adapter, payload) stay. The
check that compares the selection fingerprint becomes "same provider and same
model", so a variant change keeps the window.

**A window rejected at request time.** The provider answers a request that
carries a window with an error identifying the window as unusable. That
request fails with the provider's error, as today, and the provider is then
recorded as rejecting windows (section 5). The next request in that session,
and in any other, uses the summary instead.
- **Classifying the error:** the implementation classifies OpenAI's error for
  an invalid or expired compaction item from OpenAI's official documentation.
- **Gateways:** for OpenAI-compatible gateways, a 400 that names the
  compaction or encrypted content field counts.
- **Unclear errors:** an unclassified error does not mark the provider.
- **The window's wire names (added 2026-10-24):** besides OpenAI's code, a
  400 or 422 whose code, message, or body names one of the names the window
  travels under on the wire counts: the compaction item's `type`,
  `compaction`, and its `encrypted_content` field. A gateway without
  compaction support typically answers "unknown item type `compaction`" or
  "content keyed `encrypted_content` isn't supported". The same match on the
  compact call's own answer marks the provider `unsupported`. The names are
  defined once in oven-sdk (`oven_sdk::provider_support`), where the OpenAI
  and Azure window validators read them too, so the list cannot drift from
  what is sent.

## 5. The provider record

`<data_root>/native-compaction.json` holds one entry per provider ID:

```json
{
  "providers": {
    "openrouter": {
      "state": "unsupported",
      "since": "2026-10-24T12:00:00Z",
      "reason": "compact endpoint answered 404",
      "config_digest": "…"
    },
    "my-gateway": {
      "state": "rejects_windows",
      "since": "…",
      "reason": "…",
      "config_digest": "…"
    }
  }
}
```

**States:**
- **`unsupported`:** native compaction is not attempted, and only the summary
  runs.
- **`rejects_windows`:** native compaction is not attempted, and stored
  windows from that provider are ignored in favour of the summary.

**What marks a provider:**
- **`unsupported`** comes only from a definitive answer to the compact call:
  - HTTP 404, 405 or 501;
  - an error explicitly naming compaction as unsupported;
  - an adapter that cannot build the compact request.
- **`rejects_windows`** comes from section 4.
- **Never:** transient errors (timeouts, 429, 5xx other than 501, network
  errors) and authentication errors mark nothing.

**Expiry:**
- An entry expires after 7 days.
- An entry is dropped when that provider's configuration changes: the entry
  stores a digest of the provider's compiled definition, and a different
  digest drops it.

**Writes and reads:**
- Writes are atomic: a temporary file, then a rename.
- A missing or unreadable file counts as empty and is rewritten on the next
  change.
- The daemon holds the record in memory and writes it on change.

**Status:** config status lists marked providers, with their state, reason
and age.

## 6. Configuration

- **`compaction`** on a model override takes:
  - `"unsupported"`: never attempt native compaction;
  - `"openai-responses-compact"` and `"azure-responses-compact"`: still
    accepted, now meaning the default, so existing configurations keep
    loading. The docs mark them redundant.
- **The limit to managed providers is lifted.** Any provider on a Responses
  adapter qualifies, compatible gateways and credential-less endpoints
  included.
- **No option to force native compaction** past the record. Deleting the
  file, or changing the provider's configuration, clears it.

## 7. History format

All changes are additive under the 1.x freeze:
- the summary-and-window checkpoint stores the window beside the summary in
  the existing summary checkpoint, as a new optional field, not as a new
  checkpoint variant;
- older readers see a summary checkpoint;
- existing native-only checkpoints, written before this change, still read.

**Old native-only checkpoints have no summary.** A request on another
provider or model then has nothing to fall back to.
- When that happens, the engine runs an internal compaction of the covered
  range from the preserved log before the request, and commits it as a new
  summary checkpoint. This is a one-time cost per old session.
- The request proceeds on that new checkpoint.

## 8. Testing

- **Parallel compaction:** native and summary both succeed (the checkpoint
  holds both); native fails (summary only, failure classified); summary
  fails (the checkpoint fails).
- **Request selection:**
  - same provider and model uses the window;
  - another model on the same provider uses the summary;
  - another provider uses the summary;
  - a variant change keeps the window.
- **The labpc-style regression:** compact natively, switch models, and the
  run continues on the summary. This fails today.
- **Rejection at request time:** the request fails, the provider is marked
  `rejects_windows`, and the next request uses the summary.
- **The record:**
  - `unsupported` marked only for definitive answers, not for 429, 5xx or
    timeouts;
  - 7-day expiry;
  - a configuration digest change drops the entry;
  - atomic writes;
  - an unreadable file counts as empty.
- **Defaults:** a compatible Responses gateway attempts native compaction
  with no override, and `compaction = "unsupported"` skips it.
- **Old checkpoints:** a native-only checkpoint plus a model switch triggers
  a one-time internal compaction.

## 9. Docs

`sessions/compaction.md`, `extend/providers.md` (the `compaction` setting),
the configuration reference for the record file, and a troubleshooting
entry: "native compaction stopped being used for a provider".

## 10. Implementation notes

Implemented 2026-10-24 in client protocol 1.29.0 and plugin protocol 1.17.0,
with oven-sdk 342a029 (native compaction on compatible Responses endpoints,
and the window's wire names with `error_names_responses_native_context`).
Where it differs from, or makes concrete, the sections above:

**Classifying OpenAI's error (section 4).** OpenAI's documentation does not
list an error for an invalid or expired compaction item: the compaction
guide (developers.openai.com/api/docs/guides/compaction), the conversation
state guide, the `/responses/compact` reference, the reasoning guide, and
the error codes guide (developers.openai.com/api/docs/guides/error-codes)
describe the item as opaque and name no failure. The API has been observed
to answer an unusable compaction item with HTTP 400, type
`invalid_request_error`, and code `invalid_encrypted_content` ("The
encrypted content for item cmp_... could not be verified"), reported in
OpenAI's own Codex tracker (github.com/openai/codex/issues/36704). The
implementation does not rely on that code alone: it matches the window's
wire names, and `invalid_encrypted_content` contains `encrypted_content`.
A reasoning item's encrypted content refused on a request that also carried
a window marks the provider too; the cost is the summary until the mark
lifts.

**Compaction (section 3).**
- **Durable elision stays native's.** When the native call's input would
  exceed the model's limit less the compaction output allowance, bulky tool
  outputs are still elided in the saved log before both calls, as native
  compaction did before; the summary otherwise keeps its private pruning
  retry.
- **The summary builds on the latest summary.** A window alone (section 7)
  is passed over: the summary path summarizes from the latest checkpoint
  that has a summary, or the whole log, so the one-time summary of an old
  native-only checkpoint is an ordinary compaction. Its `source_from_seq`
  follows the summary it replaces; `prior_checkpoint_seq` still names the
  latest checkpoint of any kind.
- **The window's boundaries.** `native.source_through_seq` is the
  checkpoint's `input_through_seq`; a window the boundaries cannot hold is
  dropped and the summary commits alone.
- **"The adapter cannot build the compact request"** is the adapter's
  `supports_compaction` refusing the prepared request; it marks
  `unsupported`. A compact call that carried the previous window and is
  refused naming the window marks `rejects_windows` instead.

**Requests (section 4).**
- **Integrity checks.** Besides the adapter, provider, and model, the
  request asks the adapter to validate the window before any I/O; a window
  whose scope (endpoint, account, compaction setting) it refuses serves no
  request, and the summary does.
- **A window alone** that a request cannot use makes the request compact
  first (forced, summary beside native), then rebuild; if that fails, the
  run fails with "the session's latest checkpoint is a provider window this
  model cannot use, and it has no summary".
- **`unsupported` keeps windows.** Only `rejects_windows` stops stored
  windows from serving; `unsupported` stops new native calls.

**The record (section 5).** The configuration digest covers what defines
the provider rather than one model: its source (the managed recipe and its
fingerprint, or the custom definition's fingerprint), endpoint, provider
recipe, setup, and authentication method and parameters, not credentials.
The configuration status carries each entry as `NativeCompactionMark`
(`provider_id`, `state`, `reason`, `age_seconds`) in a new optional
`ConfigStatus.native_compaction`; clients show them as notes.

**Configuration (section 6).**
- **Custom models.** A custom model's `capabilities.compaction` became
  optional: omitted, a Responses adaptor compacts natively; `"unsupported"`
  opts out and stays the only value accepted.
- **Variants.** A variant that switches `request_endpoint` compacts
  natively when its own adaptor does and the base model's setting allows
  it; the frozen binding's native check reads the selected adaptor, not the
  base model's protocol recipe.
- **Azure.** Native compaction needs `model`, `version`, and
  `deployment_type` in the provider's setup; the `azure.openai` identity is
  no longer checked separately, since Oven requires it of every Azure
  model.
- **Replay scopes change once.** The official OpenAI adapter's replay scope
  includes its compaction setting, so turning native compaction on by
  default gives every OpenAI Responses model a new scope: reasoning replayed
  from turns before the upgrade is omitted once. Compatible endpoints keep
  their scope.

**History (section 7).** The window is `native` (`CheckpointNativeWindow`:
`window`, `source_through_seq`) on the `internal_summary` checkpoint. The
`native_window` checkpoint is read, never written.


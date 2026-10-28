# Sign in with ChatGPT as a provider

Status: **draft** (2026-10-27); every decision made by the user on
2026-10-27 ([section 12](#12-decisions)); phase 0 run on 2026-10-28 ([section 13](#13-phase-0-results)). Builds on
[engine configuration](engine-configuration.md) (provider connect),
[default models and onboarding](default-models-and-onboarding.md),
[web MCP OAuth](web-mcp-oauth.md) (whose section 1 left "model-provider
sign-ins" open), [store lock retention](store-lock-retention.md) (D6), and
[native compaction](native-compaction.md).

## 1. Goal and scope

A user with a ChatGPT plan (Go, Plus, Pro, Business, ...) signs in from
`/connect`, onboarding, `cookie connect`, or the web client, and cookie's
own engine runs on that plan: cookie's tools, subagents, compaction, and
permissions, with no API key and no per-token billing.

This is different from the [Codex ACP agent](../extend/acp-agents.md), which
already runs Codex (and its own login) as an agent inside cookie. That
offers the plan's models only through Codex's harness; this spec makes them
an ordinary cookie provider any agent can select.

In scope: the sign-in flow in every client, including a daemon on another
machine and a headless one; credential storage and refresh; the model list;
the wire profile; usage-limit errors.

Out of scope: several ChatGPT accounts at once (one `chatgpt` provider,
one account; signing in again switches it), and reading or importing
`~/.codex/auth.json` (section 2).

## 2. Two ways in

Research on 2026-10-27, from sources: the openai/codex source at `7ac6081`,
OpenAI's SIWC developer docs, and the OpenCode `86ce19d` and pi-mono `a922830`
implementations. Two different paths exist:

| | **A. Sign in with ChatGPT token sharing (SIWC)** | **B. Codex's own login** |
|---|---|---|
| Status | Official program, launched 2026-09-29; OpenCode and Pi are launch partners | Codex CLI's private client; third-party tools reuse it |
| Client ID | Registered per user: first sign-in uses `dynamic_agent_client`, the callback issues `oaiapp_...` to save and reuse | Codex's fixed `app_EMoamEEZ73f0CkXaXp7hrann` |
| Authorize / token | `auth.openai.com/api/accounts/authorize`, `/api/accounts/oauth/token` | `auth.openai.com/oauth/authorize`, `/oauth/token` |
| Scopes | `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`, `resource=https://api.openai.com/v1` | `openid profile email offline_access` (plus Codex's connector scopes) |
| Redirect | `http://127.0.0.1:<any port>/auth/callback`, never `localhost` | Allow-listed `localhost`/`127.0.0.1` ports 1455 and 1457 only |
| Device code | None; docs prescribe signing in elsewhere and copying the record | Yes (beta; the user must enable it in ChatGPT security settings) |
| Inference | Public `https://api.openai.com/v1/responses` and `/models` | Private `https://chatgpt.com/backend-api/codex/responses` |
| Wire limits | Documented: `store:false`, `stream:true`, no system-role items, no `previous_response_id`, no `max_output_tokens`/`temperature`/`top_p`/`truncation`/`user`/`metadata`/`prompt_cache_retention`, no hosted `tool_search`, no `/responses/compact` | Undocumented; Codex impersonation headers (`originator`, `ChatGPT-Account-ID`, `session-id`, ...) |
| Errors | Documented vocabulary (`subscription_sharing_*`) | `usage_limit_reached` with `resets_at`, `x-codex-*` usage headers |
| Policy | OpenAI's docs: "Do not point the flow at ChatGPT `backend-api` endpoints." Plan usage is "available to all open-source partners and selected private clients" | Codex's lead (2026-09-08, via a secondary source) called third-party use "not an approved use" outside open-source clients and partners |

**Recommendation: A only.** It is the sanctioned path, its contract is
written down, and it runs on the public Responses API that oven-sdk already
speaks. B means impersonating Codex's client against a private endpoint
that can change without notice. B's one real advantage, a device-code flow
for headless machines, is covered by section 5.3. Cookie is Apache-2.0 open
source, so it fits A's open-source track.

**Path A has no client ID to obtain or borrow.** Checked 2026-10-27 in
OpenCode (`packages/core/src/plugin/provider/chatgpt.ts` at `86ce19d`) and
Pi (`packages/ai/src/auth/oauth/openai-chatgpt.ts` at `a922830`):

- Neither ships a client ID, secret, partner key, or attestation of its own.
- Both send the shared public `client_id=dynamic_agent_client`, their own
  name as `agent_name_hint` ("OpenCode", "Pi"), and a random `urn:uuid`
  host ID.
- OpenAI issues a fresh `oaiapp_...` client ID per user on the callback.
- OpenCode saves the issued ID and reuses it; Pi registers again on every
  sign-in.
- Pi makes the name a library option (`options.agentName`), so apps built
  on pi-ai already register under their own names.

Cookie does the same with `agent_name_hint="Cookie Agent"` (user, 2026-10-28). It must never send
another tool's name: the hint is what the user sees on the consent screen
and in ChatGPT's connected-apps list.

Phase 0 confirmed on 2026-10-28 that OpenAI accepts "Cookie Agent":
registration and inference both worked (section 13).

The rest of this spec describes A.

## 3. What the user sees

- `/connect`, `cookie connect`, the onboarding panels, and the web
  client's provider list show a built-in provider, ID `chatgpt`, shown as
  **Codex / ChatGPT** (D2). It appears
  without config, like a catalog provider, and is not in models.dev.
- Its only auth method is a sign-in, labelled **Continue with ChatGPT**,
  following OpenAI's branding rules. It has no credential fields.
- The client opens the browser. The user signs in, picks a workspace on
  the first sign-in, and consents. The client reports "Signed in as
  `<email>`", and the plan's models appear in the model pickers.
- Status lines and `/connect` show the account email next to the provider.
  When the sign-in can no longer refresh, they show **Sign in again**.
- Disconnect revokes the refresh token at OpenAI and forgets the record.

## 4. Sign-in flow (daemon)

The daemon owns the whole OAuth exchange, as with MCP OAuth. Only a
single-use authorization code crosses the client connection; tokens never
do.

1. **Begin.** The daemon:
   1. Makes the PKCE verifier (S256), `state`, and `nonce`.
   2. Uses the saved issued `client_id` if it has one, else
      `dynamic_agent_client` with `agent_name_hint="Cookie Agent"` (user, 2026-10-28).
   3. Builds the authorize URL with `response_type=code`, `redirect_uri`,
      the scopes and `resource` from section 2, `ext_agent_host_id`, and
      `login_hint`/`id_token_hint` on a re-sign-in.
2. **`ext_agent_host_id`.** It is `urn:uuid:<v4>`, made once per data root
   and stored in the sign-in record (section 6). It is stable for the
   daemon's machine. Cookie has no stable daemon UUID today; the machine
   registry keys machines by name.
3. **Complete.** The daemon:
   1. Checks `state`.
   2. Saves the issued `client_id` from the callback of a first sign-in.
   3. Exchanges the code (form-encoded: `client_id`, `code`,
      `code_verifier`, `redirect_uri`, `resource`; no secret).
   4. Requires `chatgpt.tokens.use.direct` among the granted scopes.
   5. Validates the ID token: its signature against
      `https://auth.openai.com/.well-known/jwks.json`, `iss`,
      `aud == client_id`, `exp`, and `nonce`.
   6. Writes the sign-in record, connects the provider (section 6.3), and
      fetches the model list (section 8).
4. **Errors** reach the client by name:
   - `sign_in_denied` for a consent the user refused;
   - `sign_in_not_eligible` for `subscription_sharing_user_not_eligible`;
   - `sign_in_scope_missing`;
   - `sign_in_state_mismatch`;
   - `sign_in_expired`, when the flow is older than the 10-minute timeout;
   - `sign_in_client_rejected`, for `invalid_client` on the saved client
     ID. The daemon then clears that ID, and the next sign-in registers
     again.

## 5. Client protocol and clients

### 5.1 RPCs

New methods, which are a client protocol minor bump. They mirror
`mcp.auth.*`, keyed by provider:

```
provider.auth.begin    { provider_id, method, redirect_uri }  -> { provider_id, authorization_url }
provider.auth.complete { provider_id, callback_url }          -> ProviderConnectResult
provider.auth.cancel   { provider_id }                        -> { provider_id }
```

- `redirect_uri` must be exactly `http://127.0.0.1:<port>/auth/callback`.
  The daemon refuses anything else, with a message naming that form.
- A second `begin` with the same `redirect_uri` returns the same URL; a
  different `redirect_uri` replaces the flow.
- `callback_url` is redacted in `Debug`.

`ProviderDescriptor` gains `sign_in_methods: [SignInMethodDescriptor { id,
display_name, redirect }]` next to `auth_methods`.

- **Why a separate list:** `AuthMethodDescriptor` requires 1 to 32
  credential fields, so an older client cannot parse a method that has
  none. It ignores a new field, so it simply never offers ChatGPT.
- **Account display:** `DurableConnectionDescriptor` gains the signed-in
  account, `{ email }`.
- **Auth state:** `EffectiveAuthState` gains `sign_in_required`.
- `check-compat.py` decides whether the plugin protocol needs a minor bump
  too, if these types are in its schema.

### 5.2 Where the redirect lands

1. **TUI and CLI.** The client binds `127.0.0.1:0` and passes
   `http://127.0.0.1:<port>/auth/callback`.
   - `OAuthCallbackListener` gains a path parameter; it serves only
     `/callback` today.
   - The browser runs where the client runs, so a daemon on another
     machine works unchanged: the laptop catches the redirect and forwards
     the URL.
2. **Web client.** A page cannot listen on loopback, and SIWC refuses page
   redirects, so the web client uses paste-back:
   - It passes `http://127.0.0.1:1455/auth/callback` and opens the URL.
   - After consent, the browser fails to load that address.
   - The panel says "Copy the address from the tab that failed to load
     and paste it here", and hands the pasted URL to `complete`.
   - With D3, a page served from loopback by the daemon skips this
     (section 5.3).
3. **Daemon listener (D3).** When the page's origin is loopback, the
   browser and the daemon share a machine. `begin` then takes
   `redirect: "daemon"` in place of `redirect_uri`:
   - The daemon binds the listener itself and completes the flow on its
     own.
   - A `provider.auth.completed` notification tells the page.
   - This also serves the common case of plain `cookie` with the web
     client from the local daemon.

### 5.3 Headless and remote daemons

SIWC has no device-code flow. Cookie covers its use cases:

- **A TUI or web client on the user's machine, daemon anywhere.** This is
  5.2 unchanged.
- **Only an SSH shell on the daemon's machine.** Run `cookie connect
  chatgpt` there:
  - The CLI prints the URL, also binds its loopback listener, and waits
    for the redirect or a pasted URL, whichever comes first.
  - The user opens the URL on any machine and pastes back the address the
    browser failed to load.
  - Forwarding the port over SSH (`-L <port>:127.0.0.1:<port>`) also
    works, and the CLI prints that hint.
- **Never copy the record between daemons.** Each daemon signs in with its
  own `ext_agent_host_id`. OpenAI's self-hosted-VM page supports copying,
  but says per-host attribution and revocation do not work for
  transferred sessions.

## 6. Storage and refresh

### 6.1 The sign-in record

Tokens rotate hourly. Keeping them in the provider store (`store-v3.json`)
would recompile the model runtime on every refresh: `reconcile_provider_store`
publishes a new `CompiledModelRuntime` for any store change. So the tokens
live in their own file, `<data_root>/provider-oauth.json` with
`provider-oauth.lock`, under `SecureDirectory` (0600, atomic replace).

The record holds:

| Field | Notes |
|---|---|
| `provider_id`, `issuer`, `subject`, `email` | |
| `client_id` | Issued at the first sign-in; kept across sign-outs |
| `ext_agent_host_id` | Kept across sign-outs |
| `access_token`, `expires_at` | |
| `refresh_token`, `scopes`, `id_token`, `saved_at` | |
| `generation` | For compare-and-swap writes |

It follows D6 of store lock retention:

- Reads take no lock.
- Writes take a bounded lock and check the generation before replacing.
- A refresh claims the lock for its whole round trip. Refresh tokens
  rotate, so two refreshes racing would sign the user out
  (`refresh_token_reused`).

`crates/mcp/src/oauth.rs`'s `OAuthCredentialFile` has this shape but is
bound to MCP server keys. Implementation lifts a small generic version into
`crates/models/src/secure_store/` and uses it here. The MCP file stays as
it is.

### 6.2 Refresh

A `ChatGptCredentials` service in the daemon holds the live access token in
memory and refreshes it:

- **Before expiry.** It refreshes 5 minutes before `expires_at`, and never
  before `earliest_refresh_at`. In phase 0, access tokens lasted 3600 s,
  and `earliest_refresh_at` came 54 minutes after issue, so the refresh
  window is the last 6 minutes of a token. The request is form-encoded to the token
  endpoint: `grant_type=refresh_token`, `client_id`, `refresh_token`, and
  `resource`, with no `scope`.
- **On demand.** A request that finds the token expired waits for the
  refresh in flight or starts one (section 7.2).
- **On 401.** The daemon refreshes once and retries the request once. That
  retry does not count against `[model_retry] standard_retries`. A second
  401 is an auth failure.
  - A 401 whose `error.type` is `rejected_by_hardened_oauth_boundary` is
    not about the token: it means the credential may not use that
    operation. It never triggers a refresh (section 9).
- **Permanent failure.** These responses mean the user must sign in again:
  - `invalid_grant`, `invalid_refresh_token`, `token_expired`;
  - `refresh_token_expired`, `refresh_token_invalidated`,
    `refresh_token_reused`;
  - `invalid_client`, which also clears the saved `client_id`.

  The provider then turns `sign_in_required`, its models become
  unavailable with that reason, and runs on them fail with
  `provider_sign_in_required`. That error does not hold the session the
  way repeated failures do; resending after a new sign-in works.
- **Transient failure.** Network errors and 5xx are retried with backoff
  while the access token is still valid.
- **Refresh token lifetime.** It lasts 30 days and is renewed on each
  refresh, so an idle daemon still refreshes once a day to keep it alive.

### 6.3 The provider-store connection

`provider.auth.complete` writes an ordinary connection, so onboarding's
`providers_connected`, `durable_connection`, `connection_generation`, and
disconnect all work unchanged:

- The connection uses the new auth method `chatgpt-sign-in-v1`.
- Its values are non-secret references only: `subject` and `email`.
- The connection changes only on sign-in, a switch of account, and
  sign-out. Token refreshes never touch it.

`provider.disconnect` also revokes the refresh token at the
`revocation_endpoint` from OpenID discovery. It sends `token`,
`token_type_hint=refresh_token`, and `client_id`. The daemon deletes the
tokens, keeps `client_id` and `ext_agent_host_id`, and removes the
connection.

## 7. Models on the wire

### 7.1 Adapter

The provider compiles to oven-sdk's compatible Responses model
(`OpenAiResponsesModel::new_compatible`), with:

- **Endpoint:** `https://api.openai.com/v1`.
- **Adapter ID:** `oven.openai-compatible.chatgpt`.
- **Auth:** the async token source of section 11.1, which sends
  `Authorization: Bearer <access_token>`.
- **`routing_discriminator`:** the account's `subject`. Switching
  accounts changes the replay scope, so encrypted reasoning from the old
  account is never replayed to the new one.
- **Compaction:** the standalone endpoint is refused, but in-stream
  compaction works (section 7.3).

### 7.2 Wire profile

The documented SIWC limits become a named oven-sdk profile, so the encoder
never sends what the route rejects (section 11.2). Phase 0 (section 13)
confirmed each item below against the live route.

- **Always sent:** `store: false` and `stream: true`, as oven sends
  already. System turns go out as `developer` messages, as today. SIWC
  rejects `system`-role items with `400 {"detail": "System messages are
  not allowed"}`.
- **Never sent:** `temperature`, `top_p`, `truncation`, `user`, `metadata`,
  `prompt_cache_retention`, and `previous_response_id`. Each one is refused
  live with `400 {"detail": "Unsupported parameter: <name>"}`.
  - An agent or variant that sets one of these on a ChatGPT model gets a
    configuration diagnostic, not a silent drop.
- **`max_output_tokens`.** The docs' omit list names it, although the live
  route accepted it (phase 0). The docs are the source of truth, so the
  profile strips it:
  - `effective_max_output_tokens` (`crates/engine/src/policy.rs`) sends the
    model's output limit by default, so that is dropped silently.
  - An agent's own `limits.max_output_tokens` cap gets the diagnostic,
    because it cannot be honored.
- **`service_tier` is stripped in v1** (user, 2026-10-28). The route
  supports it, but variants carry reasoning effort, and a Fast tier waits
  for a later version (section 15). For that version:
  - The preview-limitations page's omit list does not name it, and no SIWC
    page forbids it.
  - The errors page only says that an `unsupported_capability` refusal may
    name "a service-tier override" in `error.param`.
  - The live route accepted `"priority"`.
  - Pi's Responses client sends `service_tier` for ChatGPT sign-ins while
    stripping `max_output_tokens`, `temperature`, and the prompt-cache
    retention fields (pi-mono `b2fcfee`,
    `packages/ai/src/api/openai-responses.ts`).
  - OpenCode lists a `-fast` variant of every model on its SIWC route.
- **Sent as today, all accepted live:**
  - `include: ["reasoning.encrypted_content"]`, `reasoning.effort` and
    `reasoning.summary`, `parallel_tool_calls`, `text.verbosity`;
  - image input, function tools, and `prompt_cache_key`.
  - Replaying encrypted reasoning works. A corrupted one is refused with
    the standard `400 invalid_encrypted_content`.
- **Deferred tools.** They keep oven's native `additional_tools` input
  items, which work live. A call to a tool loaded this way comes back with
  no `namespace`; oven already falls back to the tool's own name.
- **Prompt-cache affinity needs the `session-id` header.** This is measured
  in phase 0, on the same 7.2k-token prompt sent twice:
  - with `session-id`, the second request read 7,040 tokens from the cache;
  - without the header, it read 0;
  - with only `prompt_cache_key`, it read 0;
  - with cookie's shipped session headers only (`x-session-id`,
    `x-session-affinity`, `x-opencode-session`), it read 0.

  So cookie's shipped `[headers]` (`crates/config/src/defaults.toml`) gain
  `session-id = "${session_id}"` and `thread-id = "${session_id}"` for
  every provider (user, 2026-10-28). Before that change they had no
  `session-id`.
- **Large bodies need no compression.** A 1.1 MiB uncompressed body was
  accepted. A zstd body with `Content-Encoding: zstd` was also accepted.
  oven gains no compression option.
- **`x-codex-turn-state`.** Every response carries this opaque response
  header. Codex echoes it back within a turn for sticky routing. Cookie
  ignores it in v1, because caching worked without it.
- **Usage.** It reports `input_tokens_details.cache_write_tokens` and a
  per-item `attribution` map. Cookie reads `cached_tokens` as today and
  ignores the rest.

### 7.3 Native compaction

SIWC refuses the standalone `/responses/compact` with `401
rejected_by_hardened_oauth_boundary` (`hardened_oauth_rule_missing`).
OpenAI's other native compaction, server-side compaction in the stream,
does work there (phase 0, 2026-10-28).

- **How it works.** A normal `/responses` request carries
  `context_management: [{"type": "compaction", "compact_threshold": N}]`.
  When the rendered input crosses `N`, the stream emits `compaction` output
  items (`{id, type, encrypted_content}`) beside the answer.
  - The terminal `response.completed` then lists an empty `output`, so the
    output must be assembled from the `response.output_item.done` events.
- **Replaying it.** The last compaction item, sent back in place of
  everything before it, carried the context into the next turn.
  - That worked on the same model and on another model of the account.
  - A corrupted item is refused with `400 invalid_encrypted_content`.
- **Not measured.** Whether it shrinks a real session. The probe's
  repetitive 7.2k-token prompt still rendered about 7.3k tokens after
  compaction.

Cookie's [native compaction](native-compaction.md) only knows the
standalone endpoint (`OpenAiResponsesCompaction::V1`). D8 (user,
2026-10-28) adds the in-stream form; section 7.4 designs it. Until it
lands, the provider uses `OpenAiResponsesCompaction::Unsupported`, and the
internal summary does the work.

Pi has no native compaction in its core, which only summarizes. Community
Pi extensions add it:
- `pi-better-compaction` calls the standalone endpoint;
- `pi-codex-compact` and `pi-codex-conversion` use Codex's private
  compaction on the `openai-codex` route.

[`pi-codex-compaction`](https://pi.dev/packages/pi-codex-compaction)
(0.2.4, 2026-10-25) is the in-stream form on SIWC:
- It adds `context_management` with a `compact_threshold` to ordinary
  `/responses` requests on Pi's `openai` provider. It works with
  "Sign in with ChatGPT", where its author checked Astra and Sol live, and
  with API keys.
- It has dropped Pi's legacy `openai-codex` route.
- Its default threshold is 60% of Pi's own compaction trigger.
- At the end of each turn, it stores the latest encrypted checkpoint and
  the provider output that follows it. Later requests send these in place
  of the earlier history.
- It replays only for the same model, endpoint, auth mode, credential, and
  relevant headers.

Phase 0 found that a checkpoint also carried over to another model. Cookie
still keeps its rule that a native window serves only its own provider and
model, as Pi's package does.

### 7.4 In-stream compaction in cookie (D8)

**The threshold.** The server compacts only when a request's rendered input
crosses `compact_threshold`. Cookie's own checkpoint fires when the latest
turn's reported usage reaches `trigger_tokens`. If cookie fired first, the
server would never compact, so the server must be the first to act.

There are two ways to arrange that:
- **Lower the server's threshold below cookie's trigger.** This is what
  `pi-codex-compaction` does, at 60%. It compacts earlier than the user
  configured, so compactions are more frequent and fewer recent messages
  stay verbatim.
- **Keep the server at the configured point and raise cookie's own
  trigger to a backstop.** This is the design.

The rules for a model in in-stream mode:

```text
compact_threshold = trigger_tokens                       (unchanged formula: percent or buffer_tokens of the input budget)
backstop_tokens   = trigger_tokens + (input_budget - trigger_tokens) / 2
```

- **`[context_compaction] trigger` keeps its meaning,** "compact when the
  context reaches this". The server does the compacting at exactly that
  point.
  - With the default `percent = 70`, the server compacts at 70% of the
    input budget and the backstop sits at 85%.
  - With `buffer_tokens = B`, the server compacts at `budget - B` and the
    backstop sits at `budget - B/2`.
- **The backstop is cookie's ordinary checkpoint**, a summary with no
  window. It fires only when the server did not compact: the
  `context_management` field was refused, or one turn jumped past both
  levels.
  - The context-length retry is unchanged: a request that overflows still
    compacts once and retries.
- **A turn that streamed a compaction item is a checkpoint boundary for
  the trigger.** Its reported usage is ignored, as usage before a
  checkpoint is today. The response that compacted reports the size before
  compaction (phase 0), so counting it would fire the backstop on the next
  turn for nothing.
- **No new configuration.** The threshold derives from the existing
  `[context_compaction]` table, and each request carries the threshold of
  its own model.

**Recording the window.** When a response's output carries compaction
items, the engine keeps the last one as the session's native window:

1. **The window commits at once.** It is a checkpoint with the window and
   no summary yet.
   - The window-only checkpoints of older sessions already work this way:
     they serve their own model, and another model first summarizes the
     events they cover ([native compaction](native-compaction.md), "Older
     sessions").
   - Requests on the same provider and model send the window and the
     items after it, and drop everything before it.
2. **The summary follows in the background.** The internal summary of the
   span the window covers starts right away, beside the continuing
   session. This matches today's "native beside the summary".
   - When it lands, the checkpoint gains its summary, so a later model
     switch needs no wait.
   - A failed summary leaves the window-only checkpoint. The next switch
     summarizes on demand, as for older sessions.
3. **A refused window marks the provider.** A window refused with
   `invalid_encrypted_content` marks the provider `rejects_windows`, as
   today, and requests fall back to the summary or the full history.

**oven-sdk.** `OpenAiResponsesCompaction` gains a third mode, `InStream`:
- The request carries `context_management: [{type: "compaction",
  compact_threshold}]`, with the threshold taken from a per-request
  option.
- `compaction` output items decode as native replay artifacts.
- Output is assembled from `response.output_item.done` events, because
  `response.completed` arrives with an empty `output` once the server has
  compacted.
- Replay sends the latest compaction item and what follows it.

**Where it is used.**
- The `chatgpt` provider uses `InStream`, since SIWC refuses the
  standalone endpoint.
- Other Responses models keep the standalone `V1` by default.
  `compaction = "in_stream"` on a model override opts one in. It is a new
  value of an existing key, so it needs a configuration reference update
  and tests.
- OpenAI API keys keep `V1` by default (D10).

**Protocol and history.** Adding a summary to a committed checkpoint is a
new event, for example `context_checkpoint_summarized`. That means a
client protocol minor bump, and a history format bump with its converter,
if `check-compat.py` classes it so.

## 8. Model list

models.dev has no ChatGPT provider, and the plan's lineup differs by plan
and changes often. In 2026 it went from `gpt-5.3-codex` to `gpt-6-*` and
`gpt-6.1-*`. So the list comes from the account:

- `GET https://api.openai.com/v1/models` with the access token returns
  `{models: [{slug, display_name, visibility, supported_in_api,
  context_window, input_modalities, supported_reasoning_levels, ...}]}`.
  - Each entry carries about 50 Codex fields, including `priority`,
    `default_reasoning_level`, `supports_parallel_tool_calls`,
    `supports_reasoning_summaries`, `available_in_plans`, and
    `service_tiers`. Cookie reads only the fields in the table below.
  - Phase 0 listed 10 entries, 8 of them `list`. The `gpt-6.1-*` models
    have a 500k context window; the rest have 272k.
- Cookie keeps entries with `visibility == "list"` and
  `supported_in_api == true`.
- **When it is fetched:**
  - at sign-in;
  - at the catalog's hourly refresh;
  - on a switch of account.
- **Cache:** the list is cached in `<data_root>/chatgpt-models.json`. A
  failed fetch keeps the cache.
- **Not an entitlement check.** The SIWC docs say the listing does not
  prove access; a completed request does. A model the route refuses with
  `subscription_sharing_unsupported_capability` or "model not found" is
  marked unavailable until the next fetch.

Each entry maps to a managed model:

| Cookie | Source |
|---|---|
| id, name | `slug`, `display_name` |
| context tokens | `context_window` |
| input media | `input_modalities` (`image` → images) |
| reasoning variants | `supported_reasoning_levels`, as catalog reasoning variants |
| output limit, knowledge date | The models.dev `openai` record with the same ID, when there is one; otherwise output is unbounded, since `max_output_tokens` is never sent |
| tool calling | Assumed: the list carries no tool flag, and every model listed is a Codex model |
| pricing | None; usage is shown in tokens, and the cost column reads "plan" |

**`ultra` reasoning is left out.** Codex describes it as "automatic task
delegation", which is its multi-agent mode, and SIWC refuses `multi_agent`.

**No Fast tier in v1** (user, 2026-10-28). Variants stay reasoning
efforts. Cookie ignores `service_tiers` (every model in phase 0 listed
`priority`, which Codex labels "Fast: 2x speed, increased usage"), and
the wire profile strips `service_tier`.

`default_model`/`small_model` can name `chatgpt/<slug>` like any model.
Onboarding suggests one when ChatGPT is the only provider connected
(decision D5).

## 9. Usage limits and errors

| Response | Cookie error, classification | Retried |
|---|---|---|
| 429 `subscription_sharing_usage_limit_exceeded` | `provider_usage_limit`, Quota | No. The message says the plan's limit was reached, with no reset time: OpenAI documents none, and says not to infer one |
| 503 `subscription_sharing_usage_unavailable` / `subscription_sharing_user_unavailable` | Overload | Yes, standard retries |
| 403 `subscription_sharing_user_not_eligible` | `provider_not_eligible`, PermissionDenied | No |
| 401 `subscription_sharing_invalid_user`, 403 `chatpass_v2_*` | Auth; triggers the refresh-and-retry of 6.2 | Once |
| 400 `subscription_sharing_unsupported_capability` | InvalidRequest, naming `error.param` | No |
| 403 `subscription_sharing_route_not_supported` | Unsupported | No |
| 401 `rejected_by_hardened_oauth_boundary` (seen live on `/responses/compact`) | Unsupported; never triggers a refresh | No |
| 400 `{"detail": "Unsupported parameter: <name>"}` or `"System messages are not allowed"` (seen live) | InvalidRequest, naming the parameter. It means the wire profile has a gap | No |
| 401 `invalid_api_key` (seen live for a bad bearer) | Auth; triggers the refresh-and-retry of 6.2 | Once |
| `response.failed` / `response.incomplete` mid-stream | As today for Responses | As today |

Pre-stream bodies may be `{"detail": "..."}` instead of `{"error": ...}`,
and the error decoder accepts both. A Quota error counts toward
[held sessions](held-sessions.md) like any failed run, so a cron job does
not hammer an exhausted plan.

## 10. Configuration

No configuration is needed. `[providers.chatgpt]` is optional and strict.
It has `source = "chatgpt"` and allows only the managed-provider fields that
make sense here:

```toml
[providers.chatgpt]
source = "chatgpt"
headers = { "x-example" = "value" }   # merged as for any provider
models = { "gpt-6-astra" = { ... } }  # managed model overrides, as for catalog providers
```

- `api_key`, `auth_override`, `base_url`, and `setup` are rejected for this
  source, with a message saying ChatGPT is signed in, not configured.
- The configuration reference documents the source, and tests cover it.
- Disabling the provider follows whatever mechanism catalog providers use.
  It is not a new key.

## 11. oven-sdk changes

oven-sdk is checked against OpenAI's SIWC docs, not against other
clients.

1. **Async bearer for compatible Responses.** `OpenAiCompatibleAuth` gains
   `token_source: Arc<dyn TokenSource>`:
   - `async fn token(&self, ctx) -> Result<SecretString, ModelError>`;
   - `fn rejected(&self, ctx)`, called on a 401 before the one retry.

   The sync `HeaderProvider` cannot wait for a refresh, and Azure's
   `AzureOpenAiAuth::Entra` closure already sets the precedent for an async
   token. This keeps refresh logic in cookie and the retry in oven.
2. **A SIWC wire profile on `OpenAiResponsesSettings`** (`wire_profile:
   Standard | ChatGptTokenSharing`). It strips the fields of 7.2 at
   encoding and reports each as a request warning. It also decodes the
   `subscription_sharing_*` codes and `{"detail"}` bodies into the kinds of
   section 9.
3. **A model-list helper** for `GET /v1/models` in this shape. It could
   also live in cookie; the decision is not important.

Each change is a separate oven-sdk commit and version bump. Cookie pins
the new rev, and the crates.io publish needed before a release covers it.

## 12. Decisions

The user decided D1 to D7 on 2026-10-27, and D8 to D10 on 2026-10-28.

| # | Question | Decision |
|---|---|---|
| D1 | Path A only, or also path B (Codex's client and backend) as an opt-in method? | **A only** (user, 2026-10-27). If phase 0 shows cookie cannot self-register, file OpenAI's interest form and stop; never ship B |
| D2 | Provider ID and label | **Shown as "Codex / ChatGPT"** (user, 2026-10-27), so it is found under either name; the ID stays `chatgpt` (models are `chatgpt/<slug>`), and the sign-in button keeps OpenAI's required "Continue with ChatGPT" |
| D3 | Should the daemon bind the loopback listener itself for web pages served from loopback (5.2 item 3)? | **Yes** (user, 2026-10-27) |
| D4 | Model list from the live `/v1/models`, cached, with no bundled list | **Yes** (user, 2026-10-27) |
| D5 | Should onboarding suggest a default and small model from the ChatGPT list when it is the only provider? | **Yes** (user, 2026-10-27): the top `priority` listed model as default, and the cheapest listed `*-luna` (or the lowest priority) as small |
| D6 | Several ChatGPT accounts (personal and work) | **Not in v1** (user, 2026-10-27) |
| D7 | Plan usage limits in the status line | **Not in v1** (user, 2026-10-27); SIWC exposes no usage headers, only the 429 |
| D8 | Use in-stream server-side compaction (`context_management`) as native compaction (7.3)? | **Yes** (user, 2026-10-28), designed in 7.4: the server compacts at cookie's configured trigger, and cookie's own checkpoint moves to a backstop halfway to the input budget |
| D9 | Fast service tier | **Not in v1** (user, 2026-10-28); variants stay reasoning efforts, and `service_tier` is stripped |
| D10 | Should OpenAI API-key models default to in-stream compaction instead of the standalone endpoint? | **No for now** (user, 2026-10-28): the standalone `V1` works there, and in-stream proves itself on `chatgpt` first. `compaction = "in_stream"` opts a model in |

## 13. Phase 0 results

Run on 2026-10-28 with one real sign-in by the user. A scratch script
outside the repo made the requests from yunhao-labserver, through the
official endpoints only (not the user's gateway). Requests used
`gpt-6-luna`.

1. **Registration works.** `dynamic_agent_client` with
   `agent_name_hint="Cookie Agent"` reached the consent screen. The
   callback carried `client_id` (an issued `oaiapp_...`), `code`, `scope`,
   and `state`.
2. **Tokens.**
   - The exchange granted every requested scope, including
     `chatgpt.tokens.use.direct`.
   - `expires_in` is 3600. `earliest_refresh_at` came 54 minutes after
     issue.
   - The access token is a JWT, but cookie treats it as opaque.
   - The ID token's `aud` equals the issued client ID, its `nonce`
     matched, its `iss` is `https://auth.openai.com`, and it carries
     `email`.
3. **Refresh, rotation, and revocation.**
   - **Early refresh.** A refresh 26 minutes after sign-in, well before
     `earliest_refresh_at` (54 minutes), succeeded. So `earliest_refresh_at`
     is advice, not a hard floor. The new token's `earliest_refresh_at` was
     again 54 minutes after issue.
   - **Rotation.** Every refresh returns a new refresh token.
   - **Reuse.** Sending the old refresh token again right after rotation
     was accepted and issued new tokens. There is a grace period, so a lost
     race does not sign the user out at once. Cookie still serializes
     refreshes (6.1).
   - **Revocation.** The `revocation_endpoint` from discovery is
     `https://auth.openai.com/api/accounts/oauth/revoke`. Revoking the
     refresh token answered 200 with an empty body, and the current access
     token was refused (401) immediately afterwards.
4. **`GET /v1/models`** returned 10 Codex-shaped entries, 8 of them listed
   (section 8).
5. **`POST /v1/responses`** results are folded into 7.1, 7.2, and 9. In
   short:
   - Everything cookie sends works, apart from the parameters the docs
     already exclude.
   - The `session-id` header is what enables prompt caching.
   - No compression is needed.
   - Native compaction is refused.

The probe cost plan usage: about 470k input tokens, mostly the two
1.1 MiB bodies.

## 14. Phases

Each phase lands as its own commits with its gates and is pushed when
done.

1. **oven-sdk** (section 11): the token source, the wire profile, and
   error decoding. Conformance tests use recorded SIWC responses.
2. **Storage and refresh** (section 6):
   - the generic secure OAuth file;
   - `ChatGptCredentials`;
   - the `chatgpt-sign-in-v1` method;
   - the `chatgpt` source in `crates/models` (recipes, authoring, the
     compiler, and the manager's effective-auth precedence).
3. **Daemon sign-in and the model list** (sections 4 and 8):
   - the `provider.auth.*` routes;
   - descriptor fields;
   - the protocol bump with its baselines and changelog;
   - `chatgpt-models.json`.
4. **Clients** (section 5):
   - the TUI's `/connect` and onboarding;
   - `cookie connect chatgpt` with paste-back;
   - the web provider panel with paste-back, and D3's daemon listener;
   - "Sign in again" states.
5. **Docs:** a section in `extend/providers.md`, the configuration
   reference for `source = "chatgpt"`, the protocol reference, and the
   errors reference.
6. **In-stream compaction** (section 7.4, after v1 ships):
   - oven's `InStream` mode;
   - the threshold and backstop;
   - the window-first checkpoint with its background summary;
   - `compaction = "in_stream"`;
   - the new checkpoint event, with its protocol bump.

## 15. Non-goals

- Path B, Codex's backend, unless D1 says otherwise.
- Importing `~/.codex/auth.json`. It is Codex's client and Codex's tokens;
  using them is path B.
- Device-code sign-in, which SIWC does not offer.
- WebSocket transport, which belongs to path B. Codex's in-stream
  compaction is the documented `context_management` of section 7.3, which
  SIWC serves (D8).
- The Fast service tier in v1 (D9). The route accepts `service_tier:
  "priority"`, and every model lists it (7.2), so a later version can add
  it without new research.

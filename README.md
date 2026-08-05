# cookie agent

<p align="center"><img src="assets/logo.png" alt="cookie agent logo" width="256"></p>

Subagent-first coding harness.

The authoritative contract is [ARCHITECTURE.md](ARCHITECTURE.md). Exact schema
and runtime behavior are in
[docs/agent-model-variant-redesign.md](docs/agent-model-variant-redesign.md), and
the implemented provider/protocol/auth baseline is frozen in
[docs/provider-conformance.md](docs/provider-conformance.md).

## Current-only versions

| Surface | Version |
|---|---:|
| Runtime configuration | 7 |
| Agent document | 1 |
| Protocol, events, session JSONL/metadata, delegation journal | 8 |
| Runtime snapshot | 1 |
| Catalog cache | 1 |
| Provider store | 2 |
| Recipe registry | 1 |
| Project model-snapshot manifest | 1 |

Earlier project formats are rejected. There is no schema-6 or protocol-7
migration.

## Dynamic catalog

Every startup requests exactly `https://models.dev/catalog.json`. Resolution is
network, validated secure ETag cache, then bundled bootstrap. The selected
revision is `sha256:<lowercase SHA-256 digest of the exact selected body bytes>`.
Structurally invalid candidates
fall through; malformed provider/model records in a bounded structurally valid
candidate are quarantined independently so valid siblings remain usable.

On Unix, catalog cache schema 1 is fixed below
`~/.local/share/cookie_agent/catalog/`. Directories are private `0700`; body,
metadata, lock, and temporary files are current-user-owned `0600` regular
single-link files handled no-follow and atomically. The metadata records stale/
fallback state, safe last error, ETag, timestamps, revision, and quarantine
counts.

## Configuration schema 7

There is one top-level `providers` map; nonempty entries use
`[providers.<id>]`:

- `source = "models_dev"` uses optional `base_url`, separate typed `setup`,
  `api_key`, credential-only `auth_override`, and sparse `model_overrides`;
- `source = "custom"` uses required `endpoint`, `adaptor`, `auth`, and explicit
  complete `models`, plus separate optional adapter `setup` and `headers`.
  Custom IDs start with `custom.`.

Same-ID workspace definitions atomically replace user definitions; fields do
not merge. Managed models are automatic: every reviewed supported,
non-deprecated text-output model is included unless disabled by a sparse
override.

Managed providers may author credentials directly in user or workspace config
with `api_key` or typed `auth_override`. The effective authored credential
outranks provider-store credentials. Because replacement is atomic, a workspace
provider does not inherit a same-ID user credential. If authored auth is later
removed and no authored `base_url` exists, recomposition may use the exact
eligible provider-store credential. Removing authored setup, auth, and base URL
allows exact stored setup and auth to become effective. An authored `base_url` without
same-definition authored auth remains invalid and cannot fall through to store.

`providers` may be omitted or explicitly empty. This is the recommended
bootstrap configuration:

```toml
schema_version = 7

[server]
host = "127.0.0.1"
port = 17419

[providers]
```

When provider store 2 is also empty and there is no effective authored custom
provider, it opens the TUI with no models/root-runnable agents and keeps
`/connect` available. It must not fail with
`runtime providers must be nonempty`. Because provider store 2 is per-user
global, empty TOML may still materialize stored managed connections.

The following is a separate nonempty example:

```toml
schema_version = 7

[providers.openai]
source = "models_dev"
api_key = "${env:COOKIE_AGENT_EXAMPLE_OPENAI_API_KEY}"

[providers.google-vertex]
source = "models_dev"
setup = { project = "example-project", location = "us-central1", resource = "publishers/google" }
auth_override = { method = "oauth-access-token-v1", values = { access_token = "${env:COOKIE_AGENT_EXAMPLE_VERTEX_ACCESS_TOKEN}" } }

[providers."custom.example"]
source = "custom"
endpoint = "https://api.example.invalid/v1"
adaptor = "openai-compatible"
setup = {}
auth = { method = "bearer-api-key-v1", values = { api_key = "${env:COOKIE_AGENT_EXAMPLE_CUSTOM_API_KEY}" } }
headers = { "x-example-feature" = "enabled" }

[providers."custom.example".models."example-org/example-text-model"]
display_name = "Example Custom Text Model"
defaults = { max_output_tokens = 4096 }

[providers."custom.example".models."example-org/example-text-model".capabilities]
input = ["text"]
output = ["text"]
context_tokens = 32768
output_tokens = 4096
tool_calling = true
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = false
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {}
```

The custom model ID contains `/`; its model key splits at the first slash.
Custom `display_name` and complete `capabilities` are required. `enabled`
defaults true; `defaults`, adapter `options`, and `variants` default empty; an
omitted `default_variant` selects exact base. Catalog data is never consulted for
a custom model.

Custom static headers are public behavior metadata, never credentials. They do
not support `${env:...}`, are fingerprinted and persisted exactly, and may appear
in safe diagnostics. Secret-header authentication must use a typed `auth` method
such as bearer, fixed/provider API-key, `api-key-header-v1`, access token, or AWS
SigV4 where the selected adaptor allows it. Static headers cannot collide with
transport-, protocol-, or auth-owned headers.

For example, reviewed compatible header auth uses
`auth = { method = "api-key-header-v1", parameters = { header_name = "x-api-key" }, values = { api_key = "${env:COOKIE_AGENT_EXAMPLE_CUSTOM_API_KEY}" } }`;
the secret never belongs in `headers`.

The Vertex example keeps project/location/resource in `setup` and the credential
in `auth_override`; neither side may duplicate the other's fields.
All Registry-1 setup values are non-secret behavioral metadata included directly
in safe fingerprints. Every secret belongs to auth, is excluded from
fingerprints, and may rotate without changing model behavior identity.

Use `/connect` provider store 2 or `${env:NAME}` interpolation instead of
plaintext credentials. Interpolation is not available in custom static headers.
`api_key` is semantic input only; recipe registry 1 owns
its wire encoding. `auth_override` is exactly
`{ method = "...", values = { ... } }` and is required when an API-key auth
method is ambiguous. `api_key` and `auth_override` are mutually exclusive.

Managed auth precedence is exactly same-definition `api_key`, then
same-definition `auth_override`, then provider store only when no authored
`base_url` exists, then reviewed no-auth, then unavailable.

Managed setup precedence is complete same-definition `setup`, then stored setup,
then explicitly defaultable recipe fields, then unavailable. Provider store 2
stores normalized non-secret setup and secret auth credentials plus policy/
scope/receipt metadata.
Setup maps never merge with auth or across layers; custom providers remain fully
config-only and store-independent.

Managed endpoint precedence is authored `base_url`, then code-owned recipe
default. Catalog API metadata is only checked. An authored base URL requires
same-definition authored auth and every required non-defaulted setup field unless
the recipe is reviewed no-auth. Only explicitly defaultable setup fields may use
recipe defaults. Stored setup and stored auth never flow to an authored endpoint. All
endpoint queries, userinfo, and fragments are rejected.

User/workspace config and agent directories must be current-user-owned `0700`;
their TOML/Markdown files must be `0600` regular single-link files. Secret
buffers use best-effort zeroization and are redacted from safe state.

## Connect and disconnect

`/connect` lists every current catalog provider plus authored or store-backed
managed providers removed from the catalog. Unsupported rows remain visible
with typed reasons. Custom providers never appear and are never
provider-store-backed.

Managed connect requires catalog revision
`sha256:<lowercase SHA-256 digest of the exact selected body bytes>`, validates and
compiles the full candidate before a single durable provider/receipt write, then
publishes by infallible atomic swap. The result includes durable connection,
effective auth source, coherent runtime snapshot, and replay status. Authored
auth remains effective after a stored update.

`/connect` obtains non-secret setup descriptors and secret credential
descriptors from the code-owned recipe. It renders setup and auth separately,
validates exact missing/extra fields, and atomically stores normalized setup plus
credentials. This supports Vertex, Bedrock, Azure, and empty/default setup
API-key providers without authored provider TOML.

Provider store schema 2 is fixed at
`~/.local/share/cookie_agent/providers/store-v2.json` with the same private
ownership/no-follow/atomic guarantees. Disconnect removes managed stored setup
and credentials; it never edits config or touches custom providers.

Disconnect is revision-bound and idempotent. Same client request ID and payload
replays its durable receipt/result; conflicting reuse fails. Disconnecting an
already absent managed provider succeeds as disconnected. The result reports
post-removal effective auth and one coherent runtime snapshot; authored auth may
therefore remain effective.

The `/connect` UI always states:
`Stored setup, connections, and credentials are per-user and shared across workspaces.`
Stored setup is non-secret and may be projected where the recipe marks it safe;
stored credential values remain secret and redacted.
Other daemon processes reconcile the global store generation before discovery,
session admission, or root-run admission.

## Runtime and sessions

`runtime.snapshot.get` returns runtime snapshot schema 1 containing recipe,
catalog, provider-state, model, agent, and aggregate runtime revisions plus all
descriptors. Protocol 8 has no independently racing list-refresh flow. Every
publication emits `runtime.changed` with a complete snapshot and typed reasons.

Version-8 sessions freeze source kind, config-override fingerprint, exact recipe
and compiler IDs, endpoint identity, credential source, model behavior, and
fingerprints. Rehydration never changes credential source: authored config never
falls to store, managed snapshots require exact recipe/source matching, and
custom snapshots require the current safe custom-definition fingerprint.

Secret-free compiled model blueprints are retained in project manifest schema 1
at `.cookie-agent/model-snapshots/<64-lowercase-hex>.json`, where the filename is
SHA-256 of RFC 8785 JCS payload bytes, with lock
`model-snapshots-v1.lock`. New root runs use the
current coherent runtime; delegated sessions remain pinned to their parent's
accepted manifest; accepted runs never change. Referenced manifests are not
garbage-collected.

## TUI states

The TUI implements `loading`, `empty`, `ready`, `stale`, `bootstrap`, `unsupported`,
`disconnected`, `connected-reconnect`, `removed`, and `error-retry`.
Unsupported Enter opens details only. Stale/bootstrap/error explanations are
durable global state across navigation, not transient notifications.

In valid empty state, the Message model/draft display is exactly
`type /connect to continue`. It is not clickable as Model or Variant. Ordinary
text/run submission is blocked with the same guidance, while `/connect` opens
all-provider discovery. A coherent refresh that yields a root-runnable
agent/model replaces the guidance with normal draft selection; otherwise empty
state remains without fabricating a model.

## Running

Copy `.env.example` to a gitignored `.env`, fill only invented-placeholder
variables with your own credentials, export them, and run:

```sh
set -a; source .env; set +a
cargo run --locked -p cookie_agent -- daemon
```

Do not commit `.env` or any credential.

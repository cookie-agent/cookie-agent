# Default models, the starter kit, and onboarding

Status: **implemented** (2026-10-23); see [implementation notes](#14-implementation-notes).

## 1. Goal and scope

- **Default models.** Two configuration fields, `default_model` and
  `small_model`, that agent documents reach as `${default_model}` and
  `${small_model}`, so one edit retargets every agent that uses them.
- **The starter kit.** A default `config.toml` and default agent documents,
  shipped with each release as a downloadable asset, not bundled in the
  binary.
- **Onboarding.** `cookie onboard` (or the TUI or web client) connects a
  provider, sets the models, and can install the kit; clients offer it while
  the daemon's configuration is not set up.

**Decided 2026-10-23:**

- `${default_model}` and `${small_model}` carry a model and its variant
  together, as `${parent_model}` carries the parent's; any agent may use
  them.
- A `variant` written beside `${default_model}`, `${small_model}`, or
  `${parent_model}` overrides the variant it carries (new for
  `${parent_model}`, which refused one).
- A `models` entry may be a plain string: `- ${default_model}`.
- The approval and title agents use `${small_model}`, then `${parent_model}`.
  Compaction stays on `${parent_model}`, which keeps the session's prompt
  cache warm.
- The built-in `default` agent uses `${default_model}`.
- The starter kit is a release asset; no GitHub Pages copy.
- **Whether the daemon is set up comes from its configuration, not a marker
  file** (decided 2026-10-23): it is set up when the requirements below hold,
  and clients offer onboarding only while they do not.
- **Onboarding requires a connected provider, `default_model`, and
  `small_model`** (decided 2026-10-23): it completes only once at least one
  model provider is connected and both fields name available models of the
  connected providers (they may name the same model). The kit stays
  optional.
- **Any client can onboard** (decided 2026-10-23): everything onboarding does
  is a daemon RPC (section 8), so the TUI, the web client, `cookie onboard`,
  and any third-party client do it the same way, with no client-side file
  access and nothing that depends on the kind of client.
- The kit's agents: `primary`, and the subagents `explore` and `worker`.

**Out of scope:** more model slots than these two, per-project default
models, fetching the kit without being asked.

## 2. The two fields

```toml
# config.toml
default_model = { model = "anthropic/claude-opus-5-5", variant = "high" }
small_model   = "deepseek/deepseek-flash"      # a bare key: its default variant
```

- **Shape:** a model key string, or a table `{ model, variant }`. Both
  optional. Unknown keys in the table are errors, as everywhere in the
  configuration.
- **Validation:** the model key must parse; whether the model is available
  is a runtime matter (section 3), so a configured model whose provider is
  disconnected does not fail the load.
- **Clients:** `/config`, the web configuration view, and `cookie config`
  show and set both, with model pickers.

## 3. The variables in agent documents

```yaml
models:
  - ${default_model}                               # model and variant from config
  - { model: "${small_model}", variant: "max" }    # the variant overridden
  - deepseek/deepseek-flash                        # a plain model key
```

- **Where:** any `models` entry of any agent document, built-in or authored.
  `${parent_model}` stays limited to internal agents, as today.
- **Resolution:** when the agent registry compiles (at start and on every
  configuration reload), so changing a field retargets every agent that uses
  it without touching documents.
- **Variants:** an entry's `variant` overrides the variable's; without one the
  variable's variant applies, and a variable without a variant means the
  model's default variant. The same now holds for `${parent_model}`.
- **Unset:** an entry naming an unset variable is skipped, with a
  configuration diagnostic naming the field ("`small_model` is not set").
  Later entries still apply. An agent with no entry left is unavailable, and
  its reason names the field to set.
- **Duplicates:** the existing rule against duplicate entries compares the
  entries as written, so `${default_model}` and the key it currently names may
  both appear.

## 4. Plain-string entries

- A `models` entry may be a string instead of a mapping; the string is the
  entry's `model`, with no variant override. Strings and mappings mix freely.
- YAML reads `- ${default_model}` as a string without quotes; only an entry
  starting with `{` or `[` needs them.
- Any other entry shape (a number, a mapping with unknown keys) stays an
  error.

## 5. Built-in agents

| Agent | `models` |
|---|---|
| `default` (built in, when no document replaces it) | `${default_model}`, then the first available model as today |
| `approval` (internal) | `${small_model}`, then `${parent_model}` |
| `title` (internal) | `${small_model}`, then `${parent_model}` |
| `compaction` (internal) | `${parent_model}` (unchanged) |

- For an ACP session, which has no model of its own, the approval reviewer
  runs on `${small_model}`; without it, `ask` requests go to the user as now.

## 6. The starter kit

- **Source:** `defaults/` in the repository:

  ```text
  defaults/
    config.toml        # commented: default_model, small_model, [server], [memory], [cron], an MCP example
    agents/primary.md  # mode primary, models: [${default_model}]
    agents/explore.md  # mode subagent, read-only exploring, models: [${small_model}, ${default_model}]
    agents/worker.md   # mode subagent, does a delegated task, models: [${default_model}]
    manifest.json      # kit version, the cookie version it was built for, each file's sha256
                       # skills are not in the kit (section 13)
  ```

- **Agents:** `primary` is the root agent (the daemon already falls back to an
  agent named `primary`); it may delegate to `explore` and `worker`.
  `explore` reads and searches and never writes; `worker` carries out a task
  with the same tools as `primary`. **Permissions (decided 2026-10-23):**
  `primary` allows `read`, `write`, `webfetch`, `skill`, `memory` with the
  related `memory_write` rules (`user://*`, `agent://${agent_id}`,
  `machine://${session_machine}/*`, `project://${session_project}/*`), and
  `delegate` to `explore` and `worker`; `bash` asks except read-only
  commands (`ls *`, `cat *`, `rg *`, `git status*`, `git diff*`, `git log*`,
  …); `mcp` asks, `cron` and `list_runtimes` as their defaults. `worker`
  has `primary`'s `read`, `write`, and `bash` rules; `explore` stays
  read-only. Every permission is written explicitly, the defaults included,
  each line with a YAML comment saying whether it equals the daemon's
  default and what the default would be without the key, and why a
  non-default line is there.
- **Skills (decided 2026-10-23, then moved by section 13):** the kit first
  carried two skills, `customize-cookie-agent` (pointers for an agent
  asked to change cookie itself: where things live, the `cookie` commands
  to prefer over editing files, and the docs of each topic) and
  `add-cookie-runtime` (adding a machine as a runtime over ssh or by
  dialing in, short and pointing at the docs, with what to ask the user
  first). Both are now official skills (section 13), kept honest by
  `cookie skills check`.
- **Release asset:** the release workflow packs `defaults/` as
  `cookie-agent-defaults.tar.gz` and uploads it with the binaries, as it does
  `cookie-agent-web.tar.gz`. Prereleases carry it too.
- **Kept valid:** a test loads every kit file with the build's own
  configuration and agent parsers (strict), so a release cannot ship defaults
  its binary rejects.

## 7. `cookie onboard` (one client of the onboarding API)

```text
cookie onboard [--from <file>] [--diff] [--yes]
               [--default-model <key[:variant]>] [--small-model <key[:variant]>]
               [--url <url>] [--token <token>]
```

- **Where the kit comes from:** the release asset of the binary's own
  version; when the binary is not a release build, or its release has no
  kit, the latest release's. `--from` takes a local archive instead.
  The manifest's hashes are checked before anything is written.
- **Installing:** `cookie onboard` itself writes nothing: it calls the
  daemon's onboarding API (section 8), which writes under the daemon's
  `~/.cookie-agent`, local or remote. A file that does not
  exist is written; one that exists and differs is left alone and the kit's
  version is written beside it as `<name>.new`; an identical one is skipped.
  `--diff` prints what would change and writes nothing.
- **Steps:** connect a provider when none is connected (the existing
  `provider.connect`, with its auth methods), choose `default_model` from the
  connected providers' available models, then `small_model` the same way
  (both required), then optionally install the kit. The flags answer the
  questions; with `--yes` and no `--default-model` or `--small-model` it
  refuses rather than guess.
- **Running it again** on a set-up daemon works: it shows the current
  provider and models, and offers the kit again.

## 8. The daemon's onboarding API

Client protocol minor, additive. These calls are the only way onboarding
happens: every client uses them, the daemon does the work (downloading,
checking hashes, writing files and configuration), and it never
branches on the kind of client asking.

- `onboarding.get` → `{ ready, missing, providers_connected, default_model?,
  small_model? }`, computed from the live configuration and
  runtime: `ready` is `missing` being empty, and `missing` lists `provider`
  (no provider connected), `default_model`, and `small_model` (unset, or
  naming a model no connected provider offers). Nothing is stored for it, so
  setting things up by hand (`config.apply`, `provider.connect`, editing
  `config.toml`) counts just the same, and disconnecting the last provider
  makes the daemon not ready again. The kit is not part of readiness.
- `onboarding.preview { source? }` → the kit's version and, per file, what
  installing would do (`write`, `skip` as identical, `write_new` beside a
  changed file) with a diff for changed files. `source` is `release`
  (default: the binary's version, else the latest release) or an uploaded
  archive (an upload lease, as attachments use). It downloads and checks the
  kit but writes nothing; the daemon keeps it briefly for the run that
  follows.
- `onboarding.run { install_kit, source?, default_model?, small_model? }` →
  the files written, skipped, and written as `.new`, and the configuration
  fields set. Model values use the configuration's shapes and are
  checked like `config.apply`. **It refuses** (error `onboarding_incomplete`,
  naming what is `missing`) when, after applying its own fields, no provider
  is connected or `default_model` or `small_model` does not name an available
  model; nothing is written then, kit included.
- Providers are connected with the existing `provider.connect` and
  `provider.disconnect`; onboarding adds no provider call of its own.
- No dismiss call: there is nothing on the daemon to dismiss. A client may
  remember "not now" locally (the browser's storage, the TUI's own state)
  and ask again later; the daemon's answer stays what its configuration says.
- `onboarding.changed` notification whenever `onboarding.get`'s answer would
  change: a configuration reload or apply, a provider connected or
  disconnected, or a model becoming available or not.
- The models to choose from come from the existing model listing, so a
  client needs nothing onboarding-specific for the pickers.

## 9. Clients

Every client walks the same four steps, using only the RPCs of section 8 and
`provider.connect`; each step starts from what is already configured.

1. **Provider** (required while none is connected): the known providers, the
   connected ones marked with their count of available models; choosing one
   offers its auth methods (API key, browser login) and connects it, errors
   shown in place with a retry. Several may be connected.
2. **Default model** (required): a searchable list of the connected
   providers' available models, grouped by provider, with context size and
   price where the catalog has them; choosing one offers its variants,
   preselecting the model's own default variant.
3. **Small model** (required): the same list, with **no suggestion** (decided
   2026-10-23). Its variant picker preselects the model's first listed
   variant: models list their variants in increasing effort, so the first is
   the cheapest, and no variant names are hard-coded. The same model as the
   default is allowed.
4. **Starter kit** (optional): `onboarding.preview` per file, **new**,
   **same as yours (skipped)**, or **differs → written as `.new`** with a diff
   on request; install, skip, or use a downloaded archive.

**Finish** calls `onboarding.run`; it is disabled, with the reason, while
`missing` is not empty. The summary lists the files and settings written and
offers a first session on `primary`.

- **`cookie onboard`:** the steps as numbered questions in the terminal
  (section 7), with `/` to search lists and hidden key entry.
- **TUI:** on connecting to a daemon that is not ready, a notice line offers
  "Set up" or "Not now" (remembered for the machine); `/onboard` opens it any
  time. The panel shows the four steps as tabs with a check on each done
  step, reusing the provider connect panel and the model picker; every
  clickable has its hover (AGENTS.md).
- **Web:** a first-run sheet with a four-step stepper, Back and Next, and
  "Not now" (remembered in the browser); Settings has "Set up Cookie" to
  reopen it. Provider cards reuse the Settings connect forms (browser logins
  use the page as the redirect), model steps a searchable list with variant
  chips, the kit step status badges with expandable diffs and "Use a
  downloaded kit…". Full screen with touch-sized controls on a phone. Finish
  ends on "Start your first session", a new-session draft.
- Every connected client follows `onboarding.changed`: finishing in one tab
  closes the sheet in the others.

## 10. Tests

- Configuration: both field shapes, unknown keys, defaults absent.
- Variables: resolution, variant overrides for all three variables, unset
  skipping with diagnostics, an agent left with no entry, string entries,
  reload retargeting.
- Built-ins: the approval and title chains, compaction unchanged, the default
  agent.
- Kit: every file loads with the build's parsers; the manifest's hashes.
- Onboarding: install into an empty root, `.new` beside a changed file,
  identical files skipped, `--diff`, hash mismatch refused,
  `ready` following the configuration (set up by hand, a provider
  disconnected, a model gone), `onboarding.changed`, `onboarding.run` refused without a provider or an available
  `default_model` or `small_model` (and writing nothing), `missing` in `onboarding.get`,
  `onboarding.get/preview/run` from a plain protocol client (no TUI
  or web code), an uploaded archive as the source, the TUI notice, the web
  sheet.

## 11. Phases

1. Engine and configuration: the fields, variables, string entries, built-ins.
2. The kit and the release asset.
3. Onboarding: the RPCs, `cookie onboard`, the TUI and web offers.
4. Docs: the configuration reference, the agents page, a getting-started page
   built around `cookie onboard`.

## 12. Open questions

None.

## 13. Addendum 2026-10-23: official skills kept up to date

Status: **implemented** (2026-10-23); see the implementation notes.

The starter kit is installed once, so skills added in later releases would
never reach a daemon that already onboarded. Skills are cookie's own
instructions about cookie; agent documents and `config.toml` are the user's.
So skills leave the kit and are fetched from a versionless official source
that every daemon keeps current.

**Decided 2026-10-23:**

- A repository, `cookie-agent/skills`, served by GitHub Pages at
  `https://cookie-agent.github.io/skills/`, holds the official skills; its
  URL carries no version.
- Daemons use them by default (`[skills] official = true`) and refresh them
  at start and then daily.
- The kit keeps `config.toml` and the agent documents; its skills
  (`customize-cookie-agent` and the runtime setup skill) move to the
  repository.

### 13.1 The source

- `index.json`: `{ "version": 1, "skills": [ { "name", "version",
  "requires_cookie", "files": [ { "path", "sha256" } ] } ] }`.
  `requires_cookie` is a semver range (`">=0.4.0"`) matched against the
  daemon's own version.
- `<name>/SKILL.md` and any other files a skill lists.
- **CI in the repository** runs the latest release's `cookie skills check`
  on every push and nightly: each skill parses, names only `cookie`
  subcommands that exist, and links only to documentation pages that exist
  (`/doc/dev/` paths until a stable release carries the current docs, then `/doc/latest/`). A failing skill does not publish.

### 13.2 The daemon

- **Managed skills** live in `<data_root>/official_skills/<name>/` (`~/.cookie-agent/official_skills/` by default),
  written only by the daemon.
- **Refresh:** at start and then every 24 hours; the index first, then the
  files whose hashes changed, each checked against the index before it
  replaces the cached copy. Skills the index no longer lists, or whose
  `requires_cookie` this version does not satisfy, are removed from the
  official skills folder. Network failures keep the cached copies and are reported
  as a configuration diagnostic; offline daemons keep working.
- **Discovery:** the official skills folder is one more skill root with the lowest
  precedence: a user or project skill of the same name wins.
- **Configuration:** `[skills] official = true` (default) or `false`, which
  stops refreshing and leaves the official skills folder unused.
- **Trust:** the source URL is fixed to the official one, over HTTPS, with
  every file hash-checked. Skills grant no tools; permission rules still
  decide every call.

### 13.3 Commands and clients

- `cookie skills update` refreshes now (through the daemon); `cookie skills
  list` shows each skill with its origin (user, project, official) and, for
  official ones, its version.
- `cookie skills check <dir>` validates skills: parsing, `cookie`
  subcommands, documentation links (used by the repository's CI and by
  anyone writing skills).
- Clients' skill lists mark official skills.
- Onboarding needs no step for them: they are on by default.

### 13.4 Tests

The refresh against a local fixture server (new, changed, removed, a hash
mismatch refused, a `requires_cookie` range not met, offline with a cache),
precedence against a user skill of the same name, `official = false`,
`cookie skills check` passing and failing cases.

## 14. Implementation notes

Implemented 2026-10-23 in four phases on one branch.

**Versions.** Client protocol 1.22.0, plugin protocol 1.10.0 (the events
carry the `${parent_model}` variant), runtime protocol 1.8.0 (the skill
source `official`), history
format unchanged: the frozen `ParentModel` fallback gained an optional
`variant` that is omitted when absent, so earlier logs read as before.

**Engine and configuration.**

- `default_model` and `small_model` are `ConfiguredModel { model, variant }`
  on `EngineConfig`, decoded from a key string or a strict table; the
  top-level key allowlist names both.
- `AgentModelRef` has `DefaultModel` and `SmallModel`; a `models` entry
  deserializes from a string or a strict mapping. `${parent_model}` takes a
  variant override (still internal-only); the override is frozen into
  `run_started` and applied by binding the parent's model in that variant,
  skipping the entry when the variant is not available.
- An entry naming an unset field is skipped; the agent lists the field in
  `AgentDescriptor.unset_models`, and an authored agent reports it in
  `ConfigStatus.diagnostics` (which landed meanwhile for ACP agents):
  "agent `x` skips a `models` entry: `small_model` is not set", or "has no
  model" when nothing is left. Built-in agents report no diagnostic, since
  their fallback to `${parent_model}` is by design.
- **Deviation:** a resolved chain keeps the first entry per model. The
  protocol refuses a chain naming one model twice, so `${default_model}`
  followed by the key it names resolves to one entry rather than two.
- The built-in `default` agent resolves `${default_model}` (when set and
  available), then the first available model when that is another model.
  Its permissions are unchanged and differ from the kit's `primary`: it
  asks before writes, commands, and delegation and has no `webfetch`.

**The kit and the release.**

- `defaults/` holds `config.toml`, the three agents, and `manifest.json`
  (`kit_version` and
  each file's SHA-256; `scripts/pack-defaults.sh --update-manifest`
  refreshes the hashes). The release workflow's `build-web` job packs it
  with `scripts/pack-defaults.sh <tag>`, which checks the hashes and writes
  `cookie_version` into the archive's manifest, and uploads
  `cookie-agent-defaults.tar.gz` with the other artifacts, prereleases
  included.
- Kit files are configuration documents only (`config.toml`,
  `agents/…`); a skill path in a kit is refused. The engine reads the
  archive in memory (gzip plus a small ustar/pax reader, no new
  dependency), refuses unlisted, missing, unsafe, or mismatched files, and
  loads the kit alone with this build's configuration and agent parsers
  before comparing it with the user root.
- Tests load `defaults/` with those parsers, check its manifest, load
  `config.toml` with every commented example uncommented, read a GNU tar
  archive packed by the script.

**Onboarding.**

- The release source tries, in order: `<releases>/download/v<version>/…`
  (this build's release); the newest release in one unauthenticated page
  of the GitHub releases API (prereleases included, drafts skipped) whose
  tag's major.minor.patch is this build's version and which carries the
  kit, since nightlies are tagged `v<version>-alpha.<n>` while the crate
  version is a plain `<version>`; then `<releases>/latest/download/…`, the
  latest stable release. A rate limit or any other failure of the list
  moves on to the next step; when every step fails the kit is unavailable,
  with each step's reason. The base URL is injectable
  (`Engine::set_releases_url`; a non-GitHub base serves its list at
  `<base>/api`) and tests serve kits and lists from a local HTTP server. A downloaded kit is kept ten minutes for the run after a preview.
- **Deviation:** an uploaded archive travels inline, base64 in the
  `source` (`{"type": "archive", "data": …}`, at most 1 MiB), not through
  an upload lease: upload leases belong to a session's tree and accept only
  attachment media, while onboarding runs before any session exists.
- `onboarding.run` edits the configuration under the configuration lock:
  new kit documents and the model fields go into one candidate tree, which
  is validated, checked for readiness against the current models, and only
  then written; `.new` files and skills follow. A refusal is
  `onboarding_incomplete` with `missing`; a kit that cannot be used is
  `onboarding_kit_unavailable`; a model value the configuration refuses is
  `config_apply_refused`, as for `config.apply`.
- `onboarding.changed` is sent per connection after any runtime
  publication that changes the answer (configuration applies and reloads,
  provider changes, and catalog refreshes all publish one).
- "Connected providers" are those offering at least one available model.
- Readiness before a run is checked against the models compiled now; a run
  whose candidate configuration changes providers is not recompiled first,
  and its result reports the state after it applied.
- `cookie onboard` reuses `cookie connect`'s provider flow and the shared
  rules in `client_state::onboarding`; without `--yes` or `--diff` it needs
  a terminal.
- **TUI:** while the daemon is not ready, a notice in the background bar's
  row (background calls take precedence) says what is missing, with Set up
  and Not now. Not now is kept in `~/.cookie-agent/tui-state.json` until
  the missing set changes. `/onboard` opens a panel of four clickable step
  tabs with checks; step 1 opens the connect panel, steps 2 and 3 the model
  picker in an onboarding mode, step 4 the kit preview (`d` diff, `i`
  install, `o` a local archive path); `f` finishes. Four new pointer
  targets hover visibly. The kit preview is fetched on reaching its step.
- **Web:** a first-run sheet (full screen with 44px targets on a phone, a
  dialog on wider windows) opens on connecting to a daemon that is not
  ready, unless Not now is stored (`cookie.onboarding_not_now` in
  localStorage; the page's origin is the daemon's, so it is per daemon). It
  reuses Settings' connect forms, closes in every tab when the daemon
  becomes ready, and Settings has "Set up Cookie". The browser suite's main
  daemon now sets both model fields so the sheet stays out of the other
  scenarios; a new scenario covers the sheet on an empty daemon. The kit
  step is not browser-tested, since previewing the release kit would reach
  GitHub. **Deviation:** the model lists show context size but no price:
  the catalog view's model descriptors carry none.
- **Deviation:** `/config`, the web configuration view, and
  `cookie config` edit `default_model` and `small_model` as `config.toml`
  text (`cookie config set default_model …`); the model pickers are the
  onboarding panels', which run again on a set-up daemon.

**Official skills (section 13).**

- The skills live in `cookie-agent/skills` (`index.json`, one
  `<name>/SKILL.md` each, `scripts/update-index.py` for the hashes, and a
  CI workflow that installs the latest release and runs
  `cookie skills check .` on every push and nightly, with the newest
  release, prereleases included). Both start at version 1.0.0 with
  `requires_cookie = ">=0.4.0-alpha.0"`: the 0.3.0 nightlies have no
  fetcher.
- The engine keeps them in `<data_root>/official_skills/` with its record
  of what it keeps in `.index.json` there; entries starting with `.` are
  skipped by discovery. A refresh downloads a skill's changed files, checks
  every hash and that `SKILL.md` loads, and only then writes them (each
  through a temporary file and a rename), so a refused skill keeps its
  cached copy whole. `requires_cookie` takes comparators (`>=`, `>`, `<=`,
  `<`, `=`, a bare version, `*`) separated by commas or spaces, compared by
  plain semver precedence, pre-releases included (not Cargo's rule that a
  pre-release only matches a comparator with a pre-release on the same
  version): nightlies are tagged `0.4.0-alpha.<n>`, below `0.4.0`, so the
  skills ask for `>=0.4.0-alpha.0`. The daemon compares its own crate
  version, which is a plain `0.4.0` for nightlies and stable builds alike
  (the tag's pre-release is not in it). A range that does not read refuses
  the skill.
- The refresh loop runs in the daemon binary beside the catalog's (at start,
  then every 24 hours, stopped with it). The source URL is fixed in release
  builds; tests call `Engine::set_official_skills_url`, and debug builds
  under the binary's test switch (`COOKIE_AGENT_TEST_BUNDLED_CATALOG`) stay
  off the network unless `COOKIE_AGENT_TEST_OFFICIAL_SKILLS_URL` names a
  source.
- **Beyond the spec:** a refresh that changes the official skills reloads
  the global runtime's skills (user skills included) for new sessions, so
  a freshly started daemon offers them without a restart. `official =
  false` stops refreshing at once; whether the folder is offered follows at
  the next start, when skills load.
- **Protocol:** the skill source `official` (client 1.22.0, runtime 1.8.0),
  an official skill's `version` on `SkillDescriptor`, `skills.update`, and
  `skills.list` without a session for the daemon's own skills (which
  `cookie skills list` uses; `permission_effect` is then `ask` and
  `visible` says whether the skill wins its name).
- **`cookie skills check` (decision):** links are fetched from the published
  site by default, which the skills repository's CI uses; `--docs <dir>`
  checks them against a `docs/site` checkout instead, as the tests do. It
  also checks an `index.json` against the files beside it.
- The TUI's skill picker labels official skills and its detail line shows
  `official <version>`; the web page's skill list shows the same badge.


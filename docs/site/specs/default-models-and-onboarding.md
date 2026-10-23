# Default models, the starter kit, and onboarding

Status: **draft** (2026-10-23), settled with the user, awaiting the go-ahead
to implement.

## 1. Goal and scope

- **Default models.** Two configuration fields, `default_model` and
  `small_model`, that agent documents reach as `${default_model}` and
  `${small_model}`, so one edit retargets every agent that uses them.
- **The starter kit.** A default `config.toml` and default agent documents,
  shipped with each release as a downloadable asset, not bundled in the
  binary.
- **Onboarding.** `cookie onboard` installs the kit and sets the models, and
  records that the daemon is onboarded so clients stop offering it.

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
- `cookie onboard` installs it and writes a persistent `onboarded` marker;
  the daemon answers whether it is onboarded, so the TUI and the web client
  offer onboarding only until then.
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
  ```

- **Agents:** `primary` is the root agent (the daemon already falls back to an
  agent named `primary`); it may delegate to `explore` and `worker`.
  `explore` reads and searches and never writes; `worker` carries out a task
  with the same tools as `primary`. Their permission rules are conservative:
  writes inside the workspace allowed, elsewhere asked; `bash` asked except
  read-only commands.
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
- **The marker:** on success writes `~/.cookie-agent/onboarded`, a small JSON
  file (`{"version": 1, "at": "…", "cookie": "0.3.0", "kit": "…"}`). Running
  `cookie onboard` again works and updates the marker.

## 8. The daemon's onboarding API

Client protocol minor, additive. These calls are the only way onboarding
happens: every client uses them, the daemon does the work (downloading,
checking hashes, writing files and configuration, the marker), and it never
branches on the kind of client asking.

- `onboarding.get` → `{ onboarded, at?, cookie_version?, kit_version?,
  providers_connected, default_model?, missing }`, where `missing` lists what
  completing still needs: `provider` (no provider connected),
  `default_model`, and `small_model` (unset, or naming a model no connected
  provider offers).
- `onboarding.preview { source? }` → the kit's version and, per file, what
  installing would do (`write`, `skip` as identical, `write_new` beside a
  changed file) with a diff for changed files. `source` is `release`
  (default: the binary's version, else the latest release) or an uploaded
  archive (an upload lease, as attachments use). It downloads and checks the
  kit but writes nothing; the daemon keeps it briefly for the run that
  follows.
- `onboarding.run { install_kit, source?, default_model?, small_model? }` →
  the files written, skipped, and written as `.new`, the configuration fields
  set, then the marker. Model values use the configuration's shapes and are
  checked like `config.apply`. **It refuses** (error `onboarding_incomplete`,
  naming what is `missing`) when, after applying its own fields, no provider
  is connected or `default_model` or `small_model` does not name an available
  model; nothing is written then, kit included.
- Providers are connected with the existing `provider.connect` and
  `provider.disconnect`; onboarding adds no provider call of its own.
- `onboarding.dismiss` writes the marker without the requirements, for a
  user who configures everything by hand; clients offer it as a secondary
  "skip" action.
- `onboarding.changed` notification, so every connected client updates.
- The models to choose from come from the existing model listing, so a
  client needs nothing onboarding-specific for the pickers.

## 9. Clients

- **TUI:** on connecting to a daemon that is not onboarded, a notice offers
  onboarding (connect a provider, pick `default_model` and `small_model`,
  preview and install the kit) or skipping it; `/onboard`
  opens it later. Finishing is disabled until `missing` is empty.
- **Web:** the same as a first-run sheet, and an entry in Settings.
- **`cookie onboard`:** the same steps in a terminal (section 7).
- All three use only the RPCs of section 8, and none asks again once
  `onboarding.get` says onboarded.

## 10. Tests

- Configuration: both field shapes, unknown keys, defaults absent.
- Variables: resolution, variant overrides for all three variables, unset
  skipping with diagnostics, an agent left with no entry, string entries,
  reload retargeting.
- Built-ins: the approval and title chains, compaction unchanged, the default
  agent.
- Kit: every file loads with the build's parsers; the manifest's hashes.
- Onboarding: install into an empty root, `.new` beside a changed file,
  identical files skipped, `--diff`, hash mismatch refused, the marker,
  `onboarding.run` refused without a provider or an available
  `default_model` or `small_model` (and writing nothing), `missing` in `onboarding.get`,
  `onboarding.get/preview/run/dismiss` from a plain protocol client (no TUI
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

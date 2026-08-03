# cookie agent

<p align="center"><img src="assets/logo.png" alt="cookie agent logo" width="256"></p>

Subagent-first coding harness

## TUI tree-widget spike

`tui-tree-widget` was evaluated for the TUI tree but not adopted. Its widget
owns selection/expand interaction as a single row action, while this TUI needs
separate hit targets: expand-marker clicks only collapse/expand and row clicks
watch the selected session. The existing custom tree therefore remains the
source of hit-map rectangles and per-session collapsed state.

## Distribution

Cookie Agent is an internal application workspace, not a set of crates.io
packages. Every workspace crate is marked `publish = false`. Supported release
artifacts are workspace-built binaries produced from the locked dependency
graph:

```sh
cargo build --release --locked --workspace --all-targets
```

The executable is `target/release/cookie`. `cargo package`, `cargo publish`,
and files under `target/package` are not supported distribution paths or
release artifacts.

The root `Cargo.lock` is the only authoritative dependency graph for builds,
tests, audits, and releases. Vendored library directories intentionally carry
no standalone lockfiles and are never tested as independent default-feature
universes.

## Workspace configuration

The checked-in `.cookie_agent/config.toml` is the schema-v5 workspace fixture
loaded by both `cookie` and `cookie daemon`. Copy `.env.example` to the
gitignored `.env`, set `COOKIE_TEST_API_KEY`, and export it before startup:

```sh
set -a; source .env; set +a
cargo run --locked -p cookie_agent -- daemon
```

Configuration layering is built-in defaults, then user TOML, then workspace
TOML. There is no environment configuration layer: arbitrary environment
variables never become config keys or alter the config tree. Environment values
are read only where an approved model endpoint, authentication field, or static
header explicitly uses `${env:NAME}` interpolation.

The TUI has its own independent config file — it is not part of the layered
engine/workspace config above and has no environment-variable override:

- Path: `$XDG_CONFIG_HOME/cookie_agent/tui.toml`, falling back to
  `~/.config/cookie_agent/tui.toml`. There is no workspace layer.
- Schema version 1 keys (see `docs/tui.toml.example`):
  - `minimum_event_level = "debug" | "info" | "warning" | "error"` — default
    `"warning"`, so debug/info diagnostics are hidden by default. Hidden rows
    stay in the session projection; `/events debug|info|warning|error`
    changes the threshold for the current view only and never rewrites the
    file. The conversation title shows the active filter (`events ≥ warning`).
  - `theme = "default" | "mono" | "high-contrast"` — precedence over
    `COOKIE_THEME`; `NO_COLOR` and `TERM=dumb` always force monochrome.
- A missing file means defaults. A malformed file or unknown key fails with
  an actionable path/key error and never echoes file contents.

Diagnostic rows carry textual badges `[D] [I] [W] [E]` (readable without
color) with theme styling on top. Classification is fixed: DEBUG covers
replay/subscription/cache internals, INFO routine lifecycle and successful
checkpoints/titles, WARNING model warnings/discarded-or-reconstructed replay
state/fallbacks/abandoned attempts (including attributed child warnings),
ERROR run/tool/internal-agent/approval/storage failures.

Within `[models.<alias>]`, `provider_id` is the caller-defined stable identity
of the serving provider. It enters model descriptors, native replay/context
scopes, and configuration fingerprints, so it must remain stable across
configuration edits. `adaptor` instead selects the concrete Oven adapter and
wire protocol. They may match (`anthropic` / `anthropic`) or differ
(`openai` / `openai-responses`, `quantumcookie.gateway` /
`openai-compatible`); `provider_id` need not match the `adaptor` value.

Workspace permission rules load after user rules and use last-match-wins
ordering. The checked-in policy keeps ordinary source reads available, denies
root/nested `.env`, `credentials.{json,toml,yaml,yml}`, `store-v1.json`,
`secrets.{json,toml,yaml,yml}`, `token`, `token.{json,txt}`, `token-v1`,
`.netrc`, `.npmrc`, `.pypirc`, `.aws/credentials`,
`application_default_credentials.json`, and `id_rsa`/`id_ed25519` (while
allowing `.env.example`). It disables grep/glob enumeration because those tool
manifests do not provide a complete per-file permission surface.

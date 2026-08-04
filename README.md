# cookie agent

<p align="center"><img src="assets/logo.png" alt="cookie agent logo" width="256"></p>

Subagent-first coding harness

The accepted future architecture is documented in [ARCHITECTURE.md](ARCHITECTURE.md).
The strict redesign implementation contract is
[docs/agent-model-variant-redesign.md](docs/agent-model-variant-redesign.md).

## Workspace configuration

Cookie Agent uses provider-centric configuration schema 6 and Markdown agent
document schema 1:

```text
.cookie-agent/
  config.toml
  agents/
    primary.md
    worker.md
```

Runtime TOML defines providers and included `provider/model-id` models,
capabilities, defaults, options, and variants. It does not define agents.
Agents use strict YAML frontmatter plus a required Markdown body that becomes
the complete system prompt.

Models.dev model capabilities are derived from the pinned catalog and reviewed
recipe. Explicit models must provide every capability field. Provider headers
and model defaults/options/variants default empty; model `enabled` defaults
true. Reasoning is authored only through a variant's `reasoning` field, never
through ordinary request defaults or provider options.

Configuration precedence is built-in runtime defaults, user TOML, then the
exact cwd's workspace TOML. Same-ID workspace providers and agents replace the
complete user definition; they are not field-merged. There is no upward search
and no environment configuration layer. `${env:NAME}` interpolation is allowed
only in approved provider endpoints, auth secret fields, and header values.

The workspace path `.cookie_agent` and old TOML agent/profile/model-alias
configuration are unsupported and are not inspected or migrated. Protocol,
event, session JSONL, session metadata, and delegation-journal version 6 are
also unsupported; the accepted protocol/persistence version is 7.

Before startup, copy `.env.example` to the gitignored `.env`, set the required
provider credential variables, export them, and run:

```sh
set -a; source .env; set +a
cargo run --locked -p cookie_agent -- daemon
```

The checked fixture declares the direct model keys
`anthropic/kimi-for-coding`, `openai/gpt-5.6-luna`, and
`quantumcookie.gateway/deepseek-v4-flash`. The first two expose a named `high`
variant; the compatible-chat fixture uses exact base behavior. Agent documents
are `primary`, `worker`, `anthropic`, `responses`, and `chat` under
`.cookie-agent/agents/`.

The completed TUI is included by default. Running `cookie` starts it against an
in-process server for the current workspace, while `cookie attach` connects it
to an existing local daemon. The `daemon` and `connect` commands remain
available in the same binary.

User/workspace configuration and agent documents are loaded descriptor-relative
with no-follow rules and strict bounds. Provider secrets are redacted and are
excluded from events, errors, fingerprints, persistence, and generated output.

## Agents, variants, and delegation

Agent fallback chains use direct model keys and optional variants. An omitted
fallback `variant` uses the provider model's resolved default selection;
explicit `base` selects exact base; any other value selects that named variant.
Separately, provider model `default_variant` omission retains its source
default, explicit `base` selects base, and any other value selects a named
variant. Both resolve to exact model selections before freezing. Duplicate
model keys within one chain are invalid.

Every `primary` agent requires a nonempty chain. `subagent` and `all` agents may
have an empty chain for delegated inheritance, but every empty-chain agent has
`runnable_as_root = false`. A subagent is never root-selectable; an `all` agent
is root-selectable only when enabled with its own nonempty chain and at least
one available selection.

Delegation is disabled when an agent has no delegation block. Only the
`delegate` tool creates children, and targets must be listed enabled
`subagent`/`all` agents. A child with an empty chain inherits the invoking
parent run's currently active frozen suffix, including its selected variant;
a child with a configured chain uses its own.

Permissions are ordered, last-match-wins, and Ask by default. Each child uses
only its own agent permissions. Filesystem tools use prepared descriptor-bound
resources, and `write` permission governs both write and edit.

## TUI behavior

The Message panel title is `Agent(Model-Variant)` with separate Agent, Model,
and Variant selectors. A visible assistant header is `Agent(Model)`—that is,
`<agent-id>(<provider>/<model-id>)`, with no `ASSISTANT` prefix; its
variant is retained in structured attribution, replay/persistence, diagnostics,
and optional expanded metadata rather than the visible label.

Thinking and tool calls render as collapsible children of their owning
assistant turn, never as standalone `REASONING` or `TOOL` blocks. Compact tool
rows use safe primary arguments, `…` while running, no success suffix, and a
concise textual failure/cancellation/interruption marker.

Agents-panel rows are `agent-id:session-title`; child watching preserves the
tree root, titles patch immediately by monotonic sequence, and panel text height
is exactly `clamp(visible_tree_row_count, 1, 3)` with borders outside that
count.

The TUI has an independent strict file at
`$XDG_CONFIG_HOME/cookie_agent/tui.toml`, falling back to
`~/.config/cookie_agent/tui.toml`. It has no workspace layer. See
`docs/tui.toml.example`.

## TUI tree-widget spike

`tui-tree-widget` was evaluated for the TUI tree but not adopted. Its widget
owns selection/expand interaction as a single row action, while this TUI needs
separate hit targets: expand-marker clicks only collapse/expand and row clicks
watch the selected session. The custom tree remains the source of hit-map
rectangles and per-session collapsed state.

## Distribution

Cookie Agent is an internal application workspace, not a set of crates.io
packages. Every workspace crate is nonpublishable. Supported release artifacts
are workspace-built binaries produced from the locked dependency graph:

```sh
cargo build --release --locked --workspace --all-targets
```

The executable is `target/release/cookie`. `cargo package`, `cargo publish`, and
files under `target/package` are not supported distribution paths. The root
`Cargo.lock` is the only authoritative dependency graph for builds, tests,
audits, and releases.

# config.toml

The engine reads `~/.cookie-agent/config.toml` over built-in defaults. The file
is not required; an empty file is valid. This complete example changes one
setting:

```toml
[model_retry]
standard_retries = 1
```

## Locations and inheritance

Configuration is the built-in defaults plus one optional authored layer, the
user layer in `~/.cookie-agent/`. Within it, `config.toml` and the `agents/`
directory are optional. There is no workspace layer: a
`<cwd>/.cookie-agent/config.toml` or `<cwd>/.cookie-agent/agents/` directory is
never opened, so a leftover file there is ignored without a warning. The
working directory still supplies [AGENTS.md context](agents.md#agentsmd-context)
and project [skills](skills.md), which are context, not configuration.

An authored settings table replaces the built-in table as a whole. Omitted
fields in it use defaults. For example, the snippet above leaves
`overload_retries` and `backoff_ceiling_ms` at their defaults. An omitted table
keeps the defaults unchanged. Global [headers](../engine/headers.md) are the
exception: names merge with the built-in headers and empty values delete them.
Model and variant sparse inheritance happens within one provider definition.

Agents are separate [Markdown documents](agents.md); [skills](skills.md) have
their own discovery rules. The client loads [tui.toml](../tui/configuration.md)
independently.

## Accepted top-level keys

Every key is optional. Unknown keys, including removed top-level `pricing`, fail.

| Key | Canonical settings page |
|---|---|
| `server` | [Server](../engine/server.md): where the daemon listens, its token, and the web origins (`allowed_origins`) whose pages may connect |
| `tool_output` | [Tool Output](../engine/tool_output.md) |
| `subagent_output` | [Subagent Output](../engine/subagent_output.md) |
| `agent_md` | [AGENTS.md Context](../engine/agent_md.md) |
| `loop_warning` | [Loop Warning](../engine/loop_warning.md) |
| `approval` | [Approval](../engine/approval.md) |
| `model_retry` | [Model Retry](../engine/model_retry.md) |
| `context_compaction` | [Context Compaction](../engine/context_compaction.md) |
| `session_title` | [Session Title](../engine/session_title.md) |
| `delegation` | [Delegation](../engine/delegation.md) |
| `messaging` | [Agent Messaging](../engine/messaging.md) |
| `producers` | [External Producers](../engine/producers.md) |
| `memory` | [Memory](../engine/memory.md) |
| `headers` | [Request Header](../engine/headers.md) |
| `providers` | [Providers](providers.md) |
| `mcp` | [MCP](mcp.md) |
| `plugins` | [Plugins](plugins.md) |
| `runtimes` | [Runtimes](runtimes.md) |

Every key above is a **top-level** table: write `[messaging]`, `[delegation]`,
and so on, never `[runtime.messaging]` or another nested spelling. Older docs
used the `[runtime.messaging]` form; a nested table such as that is an unknown
top-level field and fails strict validation.

## Changes while the daemon runs

The daemon reads `config.toml` and the agent documents when it starts. Clients
read them with `config.get` and change them with `config.apply`: a set of
whole-document puts and deletes and single-key `set` edits of `config.toml`,
checked against the revision and fingerprints the client read, validated
whole, written, and applied together (see the
[protocol reference](../reference/protocol.md)). After editing the files by
hand, reload them with `config.reload`. A configuration that fails validation
is not applied: the daemon keeps running on the one it has and reports the
error. Applying a configuration does this:

| Part | When it takes effect |
|---|---|
| `tool_output`, `subagent_output`, `agent_md`, `loop_warning`, `approval`, `model_retry`, `context_compaction`, `session_title`, `delegation`, `messaging`, `producers`, `memory` | At once, from each setting's next use; a run keeps the result limits, retry policy, and delegation depth it started with |
| Agent documents and presets | At once for new runs; a run in progress keeps its agent, and its delegated children resolve against the agents it started with |
| `providers`, `headers` | At once: models are recompiled |
| `mcp` | At once: changed servers restart, removed ones stop; servers added or changed at runtime (`/mcp`) stay on top |
| `tool_output` for runtime instances | When an instance next starts or reconnects |
| `server`, `plugins`, `runtimes` | Only after the daemon restarts; the reload reports them as needing a restart |

Skills are not reloaded.

### Editing from a client

In the TUI, `/config` lists `config.toml`, the shared agents, and each preset
with its agents. Enter opens the selected document in `$VISUAL` (else
`$EDITOR`, else `vi`); saving and quitting applies it. A document the daemon
refuses, because the result does not validate, is not applied: the panel shows
why and marks the document *not applied*, and Enter reopens your text to fix
it. `n` creates an agent document (`agents/<id>.md`, or
`agents/<preset>/<id>.md` with a preset selected) from a template, `d` deletes
the selected agent after a `y`, and `r` reloads hand edits. The status pane
shows when the files differ from the applied configuration, why the last
reload failed, and what needs a restart. The text travels over the protocol,
so this works against a daemon on another machine.

From a terminal, `cookie config` does the same against the daemon that
`--url` and `--token` (or `[server]` in this machine's `config.toml`) name:

```sh
cookie config edit                      # config.toml in $VISUAL or $EDITOR
cookie config edit agents/reviewer.md   # a new document starts from a template
cookie config set loop_warning.enabled false
cookie config set 'providers."custom.x".base_url' https://llm.example/v1
cookie config unset model_retry
cookie config remove agents/reviewer.md
cookie config reload                    # after editing the files by hand
cookie config status
```

`set` takes a dotted key with TOML quoting and a TOML value; a value that is
not TOML, such as a bare URL, is a string. It edits only that key, keeping the
rest of the file and its comments. `edit` asks to edit again when the daemon
refuses the result.

## Strictness and limits

Every authored file is parsed strictly. Unknown keys, leftover `schema` or
`schema_version` keys, wrong types, and malformed content are hard errors with an
actionable path, key, and line where available. No authored-file migrations or
unknown-field ignores exist. Decoded values that hold secrets are zeroized when
the load completes.

TOML-level limits (enforced before deserialization):

- Configuration file at most 1 MiB; agent document at most 256 KiB.
- Maximum TOML nesting depth 32; at most 4096 entries per table or array.
- String values at most 256 KiB; TOML datetimes rejected; floats must be finite.

## Environment interpolation

`${env:NAME}` is single-pass interpolation. `${env:NAME:-fallback}` uses the
fallback when the variable is unset; the fallback may be empty and is split on
the first `:-`. `$$` emits a literal `$`. Interpolation is allowed only in these
paths:

- `providers.<id>.endpoint`
- `providers.<id>.base_url`
- `providers.<id>.setup.<field>`
- `providers.<id>.api_key`
- `providers.<id>.auth_override.values.<field>`
- `providers.<id>.auth.values.<field>`
- `headers.<name>`
- `providers.<id>.headers.<name>`
- `providers.<id>.models.<model>.headers.<name>`
- model and model-override `variants.<variant>.headers.<name>`

`NAME` must be `[A-Z_][A-Z0-9_]*` (uppercase letters, digits, underscore, starting
with a letter or underscore). A missing variable, a non-UTF-8 value, or an
interpolation used anywhere else is a load error. A missing variable without a
default remains an error. Header templates are validated at load time but kept
unresolved for request-time expansion and stable manifests. Interpolation is not
available in permission patterns or agent documents.

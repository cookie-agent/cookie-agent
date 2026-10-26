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
working directory still supplies [AGENTS.md context](../extend/agents.md#agentsmd-context)
and project [skills](../extend/skills.md), which are context, not configuration.

An authored settings table replaces the built-in table as a whole. Omitted
fields in it use defaults. For example, the snippet above leaves
`overload_retries` and `backoff_ceiling_ms` at their defaults. An omitted table
keeps the defaults unchanged. Global [headers](headers.md) are the
exception: names merge with the built-in headers and empty values delete them.
Model and variant sparse inheritance happens within one provider definition.

Agents are separate [Markdown documents](../extend/agents.md); [skills](../extend/skills.md) have
their own discovery rules. The client loads [tui.toml](../tui/configuration.md)
independently.

## Accepted top-level keys

Every key is optional. Unknown keys, including removed top-level `pricing`, fail.

| Key | Canonical settings page |
|---|---|
| `default_model` | [Default models](#default-models): the model agents reach as `${default_model}` |
| `small_model` | [Default models](#default-models): the model agents reach as `${small_model}` |
| `server` | [Server](server.md): where the daemon listens, its token, and the daemon clients on this machine use |
| `tool_output` | [Tool Output](tool_output.md) |
| `subagent_output` | [Subagent Output](subagent_output.md) |
| `agent_md` | [AGENTS.md Context](agent_md.md) |
| `loop_warning` | [Loop Warning](loop_warning.md) |
| `approval` | [Approval](approval.md) |
| `model_retry` | [Model Retry](model_retry.md) |
| `context_compaction` | [Context Compaction](context_compaction.md) |
| `session_title` | [Session Title](session_title.md) |
| `delegation` | [Delegation](delegation.md) |
| `messaging` | [Agent Messaging](messaging.md) |
| `producers` | [External Producers](producers.md) |
| `memory` | [Memory](memory.md) |
| `cron` | [Cron Jobs](cron.md) |
| `notifications` | [Push Notifications](notifications.md) |
| `acp` | [ACP Agents](acp.md) |
| `skills` | [Official skills](../extend/skills.md#official-skills): `official`, whether the daemon keeps the official skills up to date and offers them |
| `headers` | [Request Header](headers.md) |
| `providers` | [Providers](../extend/providers.md) |
| `mcp` | [MCP](../extend/mcp.md) |
| `plugins` | [Plugins](../extend/plugins.md) |
| `runtimes` | [Runtimes](../runtimes/templates.md) |

Every key above except `default_model` and `small_model`, which are values,
is a **top-level** table: write `[messaging]`, `[delegation]`,
and so on, never `[runtime.messaging]` or another nested spelling. Older docs
used the `[runtime.messaging]` form; a nested table such as that is an unknown
top-level field and fails strict validation.

## Default models

`default_model` and `small_model` name the models agent documents reach as
`${default_model}` and `${small_model}`, so one edit retargets every agent
that uses them. Each is a model key, which means the model's default
variant, or a table `{ model, variant }`; both are optional, and unknown keys
in the table fail. Being top-level values, they go before the first table:

```toml
default_model = { model = "anthropic/claude-opus-5-5", variant = "high" }
small_model   = "deepseek/deepseek-flash"

[model_retry]
standard_retries = 1
```

In the starter kit `small_model` serves the internal `approval` and `title`
agents, and the variant chosen for it, during onboarding or here, is the
approval reviewer's effort. Reasoning output dominates what each review
costs, and the reviewer's prompt cache reduces only the input side, so a low
variant keeps reviews cheap ([the reviewer's cache](approval.md#the-reviewers-cache)).

Only the key's grammar is checked when the file loads. Whether the model is
available is a runtime matter: a model whose provider is disconnected loads
fine, and the agent entries naming it are skipped until it is available
again. See [agents](../extend/agents.md#models) for how documents use them
and [onboarding](../quickstart.md) for setting both from a client.

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
| `default_model`, `small_model` | At once for new runs: the agents are recompiled, and every entry naming them follows |
| `skills` | `official = false` stops refreshing at once; whether the official skills are offered follows at the next start |
| `tool_output`, `subagent_output`, `agent_md`, `loop_warning`, `approval`, `model_retry`, `context_compaction`, `session_title`, `delegation`, `messaging`, `producers`, `memory`, `cron`, `notifications` | At once, from each setting's next use; a run keeps the result limits, retry policy, and delegation depth it started with |
| Agent documents and presets | At once for new runs; a run in progress keeps its agent, and its delegated children resolve against the agents it started with |
| `providers`, `headers` | At once: models are recompiled |
| `mcp` | At once: changed servers restart, removed ones stop; servers added or changed at runtime (`/mcp`) stay on top |
| `tool_output` for runtime instances | When an instance next starts or reconnects |
| `server.session_idle_eviction_after` | At once, from the next check of idle sessions |
| `runtimes.<id>.mcp`, `runtimes.<id>.acp` | At once, for a connected runtime: added servers and agents start, removed ones stop, changed ones restart (see [Runtime templates](../runtimes/templates.md)) |
| `server` (its other keys), `plugins`, a template's `command`, `env`, and timeouts | Only after the daemon restarts; the reload reports them as needing a restart, and a client can restart the daemon (see [Restarting the daemon](../daemon/persistent.md#restarting-the-daemon)) |

Skills are not reloaded.

### Editing from a client

In the TUI, `/config` lists `config.toml`, the shared agents, and each preset
with its agents. Enter opens the selected document in `$VISUAL` (else
`$EDITOR`, else `vi`, or on Windows `code --wait` when VS Code is on `PATH`,
else Notepad); saving and quitting applies it. A document the daemon
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

## State kept beside the configuration

Some files in the data folder are what the daemon learned, not configuration:
they are not validated as authored files, and an unreadable one counts as empty
and is written again. `native-compaction.json` is the record of providers the
daemon stopped attempting [native compaction](../sessions/compaction.md#the-provider-record)
for, one entry per provider ID:

```json
{
  "providers": {
    "my-gateway": {
      "state": "unsupported",
      "since": "2026-10-24T12:00:00Z",
      "reason": "the compact endpoint answered HTTP 404",
      "config_digest": "sha256:…"
    }
  }
}
```

`state` is `unsupported` or `rejects_windows`. An entry expires 7 days after
`since`, and is dropped when the provider's configuration no longer matches
`config_digest`. Delete the file, or an entry, to try native compaction again.
The daemon writes the file through a temporary file and a rename.

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

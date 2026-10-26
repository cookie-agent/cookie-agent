# Memory [memory]

Size limits of memory documents and of the memory snapshot a session's model
sees. Complete `config.toml`:

```toml
[memory]
user_max_bytes = 16384
agent_max_bytes = 16384
machine_max_bytes = 16384
project_max_bytes = 16384
overview_max_bytes = 16384
snapshot_user_bytes = 12288
snapshot_index_bytes = 4096
```

Memory is a folder of Markdown notes, `~/.cookie-agent/memory`; see the
[memory spec](../specs/memory.md). An omitted table inherits; an authored
table replaces the lower table and uses defaults for omitted fields. Unknown
fields fail, `max_file_bytes` included, which the five document limits
replaced; see [config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `user_max_bytes` | integer | `16384` | Largest `user://` category, frontmatter included, in bytes. Must be positive and at most 57344. |
| `agent_max_bytes` | integer | `16384` | Largest `agent://` file, frontmatter included, in bytes. Must be positive and at most 57344. |
| `machine_max_bytes` | integer | `16384` | Largest `machine://` memory, frontmatter included, in bytes. Must be positive and at most 57344. |
| `project_max_bytes` | integer | `16384` | Largest `project://` memory other than an overview, frontmatter included, in bytes. Must be positive and at most 57344. |
| `overview_max_bytes` | integer | `16384` | Largest project overview, `project://<project>/overview`, frontmatter included, in bytes. Must be positive and at most 57344. |
| `snapshot_user_bytes` | integer | `12288` | Room for the user categories in a snapshot, in bytes of rendered text. Categories that do not fit are listed by name only. Must be positive. |
| `snapshot_index_bytes` | integer | `4096` | Room for the index of the session's machine memories in a snapshot, in bytes of rendered text, summed over all machines. Memories that do not fit are only counted. Must be positive. |

A write or edit that would leave a memory over its kind's limit is refused,
and the model is shown the memory's current content to condense. A memory
already over its limit, as after the limit is lowered, stays valid and is
still shown, marked over its limit; only changes that make it smaller are
accepted until it fits. A file larger than 57344 bytes is invalid: search
and the snapshot skip it.

The snapshot limits are measured on the snapshot's rendered text (see
[System Prompt Composition](../reference/system-prompt.md#memory-turn)): a
category or machine memory is shown only while its whole section still fits.
The run's agent's own file is measured on its escaped description and body
against the room the two snapshot limits leave in the event; a file that
does not fit is named instead of shown. A snapshot is recorded as one
session event, bounded at 64 KiB. `agent_max_bytes`, `snapshot_user_bytes`,
and `snapshot_index_bytes` together must not exceed 57344 bytes, which
leaves 8 KiB for the snapshot's fixed text, headings, and escaping; a
configuration whose limits sum to more fails to load.

The limits apply at once when the configuration is applied or reloaded: a
lowered document limit applies from the next write, and the next snapshot
marks the memories over it.

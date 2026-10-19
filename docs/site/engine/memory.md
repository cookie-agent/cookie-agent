# Memory [memory]

Size limits of memory files and of the memory snapshot a session's model
sees. Complete `config.toml`:

```toml
[memory]
max_file_bytes = 4096
snapshot_user_bytes = 12288
snapshot_index_bytes = 4096
```

Memory is a folder of Markdown notes, `~/.cookie-agent/memory`; see the
[memory spec](../specs/memory.md). An omitted table inherits; an authored
table replaces the lower table and uses defaults for omitted fields. Unknown
fields fail; see [config.toml](../guide/configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `max_file_bytes` | integer | `4096` | Largest memory file, frontmatter included, in bytes. A larger file is invalid: search and the snapshot skip it, and a write that would produce one fails. Must be positive. |
| `snapshot_user_bytes` | integer | `12288` | Room for the user categories in a snapshot, in bytes of rendered text. Categories that do not fit are listed by name only. Must be positive. |
| `snapshot_index_bytes` | integer | `4096` | Room for the index of the session's machine memories in a snapshot, in bytes of rendered text, summed over all machines. Memories that do not fit are only counted. Must be positive. |

A snapshot is recorded as one session event, bounded at 64 KiB. The three
limits together must not exceed 57344 bytes, which leaves 8 KiB for the
snapshot's fixed text, headings, and escaping; a configuration whose limits
sum to more fails to load.

The limits apply at once when the configuration is applied or reloaded: a
lowered `max_file_bytes` makes larger files invalid from the next read.

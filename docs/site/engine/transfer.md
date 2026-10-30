# Transfers [transfer]

Complete `config.toml` limiting what one `transfer` call moves to 1 GiB:

```toml
[transfer]
max_bytes = 1073741824
```

An omitted table inherits; an authored table replaces the lower table and uses
defaults for omitted fields. Unknown fields fail. See
[config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `max_bytes` | integer | `0` | The most bytes one transfer moves: a file's size, or a folder's files together. `0` or less means no limit. |

[`transfer`](../reference/tools.md#transfer) copies a file or a folder to
one of a session's machines, from another of its machines or from an artifact
of its tree. The daemon relays the bytes between the two runtimes a chunk at a
time and keeps none of them, so the limit bounds the time a call takes and
what reaches the destination's disk, not the daemon's storage. A file over the
limit is refused before anyone is asked to approve the call; a folder, whose
size is known only as it is read, fails once it passes the limit and leaves
nothing at the destination. A folder copied within one runtime under a limit
is relayed through the daemon, which counts it, rather than copied by the
runtime on its own.

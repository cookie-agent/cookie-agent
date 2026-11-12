# Share links [share]

Complete `config.toml`:

```toml
[share]
enabled = true
```

An omitted table inherits; an authored table replaces the lower table and uses
defaults for omitted fields. Unknown fields fail. See
[config.toml](configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Whether the daemon serves share links and makes new ones. Off, every link answers as gone and no link is made; links are kept, and work again once it is on. |

The address links are built on and the port that serves them alone are
[`[public]`](public.md) settings, shared with webhooks; they were
`[share] url` and `[share] listen` before, which now fail as unknown keys.
A share link is a public URL for one [asset](../sessions/assets.md) or
conversation: see [Share links](../sessions/sharing.md) for making and
revoking them.

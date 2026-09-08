# tui.toml

The terminal client reads `~/.cookie-agent/tui.toml` independently of the engine.
Both settings are optional. This is a complete file:

```toml
minimum_event_level = "warning"
theme = "auto"
```

There is no workspace layer, upward search, environment-selected config path, or
environment interpolation. A missing file uses defaults. Unknown keys, wrong
types, and malformed values fail with the path and offending key. Engine
`config.toml` settings do not inherit into this file.

| Top-level key | Purpose |
|---|---|
| [Minimum Event Level](minimum_event_level.md) | Filter diagnostic rows |
| [Theme](theme.md) | Select the terminal palette |

These are scalar keys, not TOML tables. Omitted fields use their individual
defaults; the theme's environment and terminal fallbacks are described on its
page. Runtime `/events` changes are view-only and never rewrite this file.
See [Run](../guide/run.md) for terminal operation.

# tui.toml

The terminal client reads `~/.cookie-agent/tui.toml` independently of the engine.
Every setting is optional. This is a complete file:

```toml
minimum_event_level = "warning"
theme = "auto"
transcript = "reduced"
```

There is no workspace layer, upward search, environment-selected config path, or
environment interpolation. A missing file uses defaults. Unknown keys, wrong
types, and malformed values fail with the path and offending key. Engine
`config.toml` settings do not inherit into this file.

| Top-level key | Purpose |
|---|---|
| [Minimum Event Level](minimum_event_level.md) | Filter diagnostic rows |
| [Theme](theme.md) | Select the terminal palette |
| [Transcript](transcript.md) | Fold agent work into one row per reply, or show all of it |

These are scalar keys, not TOML tables. Omitted fields use their individual
defaults; the theme's environment and terminal fallbacks are described on its
page. Runtime `/events` palette changes are view-only and never rewrite this file.
The TUI writes one key itself: `transcript`, when `/transcript` or Ctrl-O
changes the mode, leaving the rest of the file as written.
See [Terminal UI](../clients/tui.md) for terminal operation.

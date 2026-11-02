# Theme [theme]

Select a terminal palette. Complete `tui.toml`:

```toml
theme = "dark"
```

| Value | Selection |
|---|---|
| `"auto"` | Detect the terminal background, ignoring `COOKIE_THEME` |
| `"default"` | Light palette |
| `"dark"` | Dark palette |

Any file preference takes precedence over `COOKIE_THEME`, including `"auto"`.
When the key is omitted, `COOKIE_THEME` can select a palette; if that variable
is also unset or `auto`, selection uses automatic detection: OSC 11,
the last `COLORFGBG` field, and finally the light default. `NO_COLOR` and
`TERM=dumb` turn colour off after selection: the chosen palette then shows
state by bold, italic, underline, and reverse video only.

Colour depth is detected separately. `COLORTERM=truecolor` (or `24bit`) selects
24-bit colour; when `COLORTERM` is unset, Windows Terminal (`WT_SESSION`, also
visible inside WSL) and `TERM_PROGRAM` values `vscode`, `iTerm.app`, `WezTerm`
and `ghostty` count as 24-bit too. Otherwise a `TERM` containing `256color`
selects 256 colours, and anything else 16. Set `COLORTERM=truecolor` to opt a
capable terminal in, for example over SSH, where these variables are not
forwarded.

Unknown strings and non-string values fail, including `"mono"` and
`"high-contrast"`, which were removed with the Quiet Ink palette. There are no nested fields or
workspace overrides. Use the scalar `theme`, not a `[theme]` table; see
[tui.toml](configuration.md) for file loading.

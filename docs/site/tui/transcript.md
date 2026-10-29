# Transcript [transcript]

Choose how the conversation shows an agent's work. Complete `tui.toml`:

```toml
transcript = "reduced"
```

Accepted strings are `"reduced"` and `"full"`. The default is `"reduced"`.
Other strings and non-string values are errors. This is a top-level scalar,
not a `[transcript]` table.

- **`reduced`** folds each reply's thinking, tool calls, and the text written
  between calls into one row above the reply. While the agent works, the row
  reads `▸ Working… 42s · 3 tool calls` and the line under it names what it
  is doing now: the newest step or line of its thinking, or the title of its
  newest tool call. Once that work ends, it reads `▸ Worked for 1m 23s · 7 tool
  calls`, with `· 1 failed` when calls failed, or `Stopped after …` when the
  run was cancelled or interrupted. Click the row to
  open the work as the full mode shows it. A call waiting for your approval
  shows outside the fold until you answer it. A reply with no thinking or tool
  calls has no row.
- **`full`** shows every thinking block and tool call.

`/transcript reduced`, `/transcript full`, and Ctrl-O change the mode in the
running TUI and save it here: the TUI rewrites only this key, keeping the
file's comments and other keys as written, and creates the file when it is
missing. A file that no longer parses is left as it is, and the bottom bar
says the mode was not saved; the new mode still applies until the TUI exits.
Other running TUIs keep their mode until they start again. See
[tui.toml](configuration.md) for loading behavior.

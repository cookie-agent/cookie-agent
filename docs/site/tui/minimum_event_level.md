# Minimum Event Level [minimum_event_level]

Choose which diagnostic rows appear in the conversation. Complete `tui.toml`:

```toml
minimum_event_level = "warning"
```

Accepted strings, from least to most severe, are `"debug"`, `"info"`, `"warning"`,
and `"error"`. The default is `"warning"`. Other strings and non-string values
are errors. This is a top-level scalar, not a `[minimum_event_level]` table.

Rows below the threshold remain in the session projection. Lowering the filter
with `/events debug` reveals them again. Runtime changes do not edit the file or
remove events from history. There are no nested fields or workspace overrides;
see [tui.toml](configuration.md) for loading behavior.

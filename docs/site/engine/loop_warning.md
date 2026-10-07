# Loop Warning [loop_warning]

Complete `config.toml` to turn off repeated-tool-call warnings:

```toml
[loop_warning]
enabled = false
```

An omitted table inherits; an authored table replaces the lower table and uses
defaults for omitted fields. Unknown fields fail. See
[config.toml](../guide/configuration.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Warn the model when it keeps repeating the same tool calls with identical results. Applies to root and delegated agent runs. |

A run counts as looping when its latest tool calls are at least three
consecutive copies of the same block of one to eight calls: same operations,
same outputs or errors. Changing output counts as progress, and user input
starts the count over. The engine then appends a `<system-reminder>` to the
latest successful result, telling the model how many times it has repeated
itself and to work from the results it already has. The warning repeats, with
a rising count, for as long as the loop continues. It never blocks a call or
stops the run.

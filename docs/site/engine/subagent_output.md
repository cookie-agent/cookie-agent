# Subagent Output [subagent_output]

Complete `config.toml` using the defaults:

```toml
[subagent_output]
max_lines = 100
max_bytes = 10240
head_lines = 20
tail_lines = 20
```

An omitted table inherits. An authored table replaces the lower table, with
defaults for omitted fields; unknown fields fail. See
[config.toml](../guide/configuration.md).

Controls the preview of a subagent's final report in the `delegate_subagent`
result and the `<subagent_notification>` a background delegation sends. It
truncates the way [`[tool_output]`](tool_output.md) does: a report within both
`max_lines` and `max_bytes` is shown whole and ends in `full output shown`; a
longer one shows its first `head_lines` and last `tail_lines` lines, splitting
`max_bytes` between them in the same ratio, around a marker such as
`[… 200 lines omitted. Read more: get_subagent_result(session_id="explore_1a2b3c4d", offset=20)]`.
`get_subagent_result` pages the full report and is never truncated.

The preview is stored on the parent's `delegate_finished` event, so changing
these limits affects later completions only.

| Key | Type | Default | Description |
|---|---|---|---|
| `max_lines` | integer | `100` | Longest report, in lines, shown whole. Must be greater than zero. |
| `max_bytes` | integer | `10240` (`10 * 1024`) | Longest report, in bytes, shown whole. Must be 1..=61440 so the stored preview fits its 64 KiB event bound. |
| `head_lines` | integer | `20` | Lines kept from the start of a truncated report. When omitted it is 20, capped at the rest of `max_lines` after `tail_lines`, or at half of `max_lines` (rounded up) when both are omitted. |
| `tail_lines` | integer | `20` | Lines kept from the end of a truncated report. When omitted it is 20, capped at the rest of `max_lines` after `head_lines`, or at half of `max_lines` (rounded down) when both are omitted. `head_lines + tail_lines` must be 1..=`max_lines`. |

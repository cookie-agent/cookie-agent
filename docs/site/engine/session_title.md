# Session Title [session_title]

Complete `config.toml` to keep titles manual:

```toml
[session_title]
generate_on_first_turn = false
```

An omitted table inherits. An authored table replaces the lower table, with
defaults for omitted fields. Unknown fields fail; see
[config.toml](../guide/configuration.md). Title workflows belong to
[Sessions](../guide/sessions.md#titles), and the model prompt belongs to the
[internal title agent](../guide/agents.md#internal-agents).

Controls automatic session titles generated from the first user message.

| Key | Type | Default | Description |
|---|---|---|---|
| `max_chars` | integer | `80` | Maximum title length in characters. Must be greater than zero. |
| `max_input_messages` | integer | `4` | Maximum number of opening user messages included in the title-agent prompt. The engine uses the first N messages so the title remains anchored to the session's original topic. Must be greater than zero. |
| `generate_on_first_turn` | boolean | `true` | Generate a title automatically after the first user message. When `false`, no automatic title is produced (user-set titles still work). |
| `fallback_to_input_excerpt` | boolean | `true` | When the internal title agent fails or returns an unusable title, fall back to an excerpt of the first user message instead of leaving the session untitled. |

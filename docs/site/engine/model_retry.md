# Model Retry [model_retry]

Complete `config.toml` using the defaults:

```toml
[model_retry]
standard_retries = 3
overload_retries = 5
backoff_ceiling_ms = 60000
```

An omitted table inherits; an authored table replaces it completely, with
defaults for omitted fields. Unknown fields fail. See
[config.toml](../guide/configuration.md) for layer precedence.

Controls retries on the current model before the engine advances through its
fallback chain. Values count retries after the initial attempt. Zero disables
retries for that class, while any negative value retries indefinitely until the
request succeeds or the run is cancelled.

| Key | Type | Default | Description |
|---|---|---|---|
| `standard_retries` | signed 64-bit integer | `3` | Retry budget for ordinary retryable model errors: one initial attempt plus three retries gives up to four total attempts by default. |
| `overload_retries` | signed 64-bit integer | `5` | Separate retry budget for overload errors and retryable HTTP 429/503 responses. |
| `backoff_ceiling_ms` | integer | `60000` | Positive local ceiling, in milliseconds, for exponential backoff after jitter. |

Retries use exponential backoff from one second with 25% jitter, clamped to
`backoff_ceiling_ms` and a positive minimum of one millisecond. A provider
`Retry-After` is authoritative when longer and may exceed that local ceiling
without a cap. Cancellation still interrupts the wait.

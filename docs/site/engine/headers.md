# Request Header [headers]

Complete `config.toml` to add a public header and delete a shipped one:

```toml
[headers]
x-client-name = "workspace-agent"
x-session-affinity = ""
```

This key accepts a map of header names to string values, with no fixed nested
field names. Invalid names, values, forbidden ownership, or limits fail loading.
Unlike settings tables, user and workspace header maps merge by normalized name;
an omitted name inherits and an empty value deletes it. See
[config.toml](../guide/configuration.md#environment-interpolation) for templates.

Global request headers are merged with provider, model, and variant `headers`
tables. Names are case-insensitive and later values win:

`shipped defaults -> global -> provider -> model or variant`

An empty value deletes an inherited header. Each authored table and each merged
result is limited to 64 entries, 128 bytes per name, 8192 bytes per value, and
64 KiB in aggregate. The shipped lowest-priority layer is:

| Header | Value |
|---|---|
| `user-agent` | `cookie-agent/<build version>` |
| `x-session-id` | `${session_id}` |
| `x-session-affinity` | `${session_id}` |
| `x-session-parent-id` | `${parent_session_id}`; omitted for root sessions |

`${session_id}` and `${parent_session_id}` are expanded for each request.
Templates, rather than resolved session IDs or environment values, are stored in
model manifests and fingerprints. Header values are public behavior metadata:
environment interpolation is supported, but the configured template and its
resolved plaintext value may be exposed in process memory and on the wire.

Transport-owned headers (`host`, `content-length`, `transfer-encoding`,
`connection`, `proxy-authorization`) and protocol-owned headers (`content-type`,
`accept`, `anthropic-version`, `anthropic-beta`, and every `x-amz-*` name) are
forbidden at every level. Auth-owned headers (`authorization`, `x-api-key`,
`api-key`, `cookie`, `set-cookie`, `x-goog-api-key`) and `user-agent` are allowed.
If any auth-owned header is configured, caller headers take precedence and the
adapter does not inject its typed authentication header.

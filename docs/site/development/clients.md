# Write a client

A client is any program that speaks the [client protocol](../reference/protocol.md)
to a daemon: the TUI, the web page, and the `cookie` subcommands are three. The
daemon treats every client alike, so a client of your own, an editor
integration, a dashboard, a bot, sees and does what the shipped ones do. This
page is the path through the protocol; the
[protocol reference](../reference/protocol.md) and the
[event reference](../reference/events.md) hold the details it links to.

## Connect

Open a WebSocket to the daemon's client URL, by default
`ws://127.0.0.1:7419/ws` (`wss://` when it serves TLS). The URL and the token
come from the same places the CLI takes them; see
[Reaching a daemon](../clients/cli.md#reaching-a-daemon). Each WebSocket text
frame carries one JSON-RPC 2.0 message, up to 128 MiB.

Present the token one of two ways:

- **A header**, `Authorization: Bearer <token>`, on the upgrade request. A
  wrong token is refused with HTTP 401 before the WebSocket opens. Native
  clients should use this.
- **The handshake**, for clients that cannot set headers, such as a browser
  page. The daemon ignores the `Origin` header a browser sends; see
  [Web clients](../daemon/remote-access.md#web-clients).

## Handshake

The first request must be `handshake`:

```json
{"jsonrpc":"2.0","id":1,"method":"handshake","params":{
  "protocol_version":"1.16.0",
  "client":{"kind":"mytool","instance_id":"0192f3c4-8d1e-7a2b-9c3d-4e5f60718293"},
  "features":[]}}
```

- `protocol_version` is the version your client was written against. The
  daemon accepts any version of its major and both sides use the lower minor;
  another major is refused with `-32002 unsupported_protocol`. Send only the
  methods and members of the version you name.
- `client.kind` is a label of your choosing, a lowercase letter and up to 15
  lowercase letters, digits, or hyphens. `client.instance_id` is a UUID you
  keep across reconnects, so your client recognizes what it sent.
- `token` goes here when you did not send the header.

The reply names the daemon's protocol and engine versions, your `origin`, the
label your submissions carry, and the daemon's `machine_id`. Until the
handshake succeeds the daemon answers no other request, and it closes a
connection that has not authenticated within 10 seconds. See
[Handshake and authentication](../reference/protocol.md#handshake-and-authentication)
and [Compatibility](../reference/compatibility.md#handshakes).

## Call methods

After the handshake, send requests with any `id`; replies can arrive out of
order, and the daemon runs up to 64 of one connection's requests at once.
[`methods.json`](../schemas/client/methods.json) lists every method with its
params and result schema, and [Methods](../reference/protocol.md#methods)
describes them by area: sessions, runs, approvals, runtimes, machines, memory,
configuration, providers, MCP servers, and plugins.

A typical flow: `session.list` to show sessions, `session.create` for a new
one, `session.submit_input` to send a message (it starts, steers, or queues a
run; see [Submitting from several clients](../reference/protocol.md#submitting-from-several-clients)),
`approval.list` and `approval.respond` when a tool call waits for an answer,
and `run.cancel` to stop.

## Follow a session

`events.subscribe` with a `session_id` and a `cursor`, the last `seq` you
hold, returns the events after it and starts `events.subscription`
notifications for that session. Each notification is one of:

- `event`: a stored event, with its `seq`; advance your cursor.
- `transient`: live-only output, such as streamed text or tool output, placed
  after the stored event `after_seq`; it never advances the cursor, and the
  committed event that follows replaces it.
- `gap`: you fell behind and the daemon dropped your tail; subscribe again
  from your cursor.
- `rewound`: a revert cut the log; drop what you hold past `through_seq`.

Large histories page with `limit` and `has_more`, and `cursor_timestamp` lets
you detect that a revert reused your cursor's sequence number. Read events
openly: an event type or a member you do not know is skipped, not an error.
See [Subscriptions](../reference/events.md#subscriptions),
[Records](../reference/events.md#records), and
[Decoding](../reference/protocol.md#decoding).

Changes outside one session arrive as their own notifications, such as
`sessions.changed`, `runtimes.changed`, or `memory.changed`, after which you
refetch the matching list; see
[Daemon-wide changes](../reference/protocol.md#daemon-wide-changes).

## Show tool calls

Tool calls describe themselves to clients as typed display blocks: `text`,
`markdown`, `code`, `file_excerpt`, `diff`, `terminal`, `key_value`, `table`,
`json`, `media`, `link`, and `session_ref`. Draw each kind you support. The
daemon fits blocks to your protocol version: a kind newer than your minor
reaches you as `text`, and every block has a text rendering to fall back on.
See [Tool display](../reference/tools.md#tool-display).

## Handle errors

An error reply carries a JSON-RPC `code` and, for the daemon's own errors,
`data.code`, a stable snake_case name, with a `message` for people, an
optional `hint` saying what to do, and an optional `detail`. Branch on
`data.code`, show `message` and `hint`, and show `detail` on request. The
[error reference](../reference/errors.md) lists every code and its fields.

## Send files

Attachments upload in chunks before the message that uses them:
`artifact.upload.begin` with the session and size, `artifact.upload.chunk`
with base64 data of at most 1 MiB, each after the previous one is answered,
and `artifact.upload.finish` with the SHA-256, which returns the artifact to
put in the message content. `artifact.read` reads media back. See
[Media](../reference/protocol.md#media).

## Any language

The protocol's schemas are published with these pages:
[`methods.json`](../schemas/client/methods.json), one JSON Schema per params
and result root under `schemas/client/schemas/`, and the
[`version`](../schemas/client/version) they describe. They are committed in
`crates/protocol/compat/client/`, and a checker keeps every change additive
within a major, so code generated from them keeps working with later
daemons. See [Published schemas](../reference/schemas.md#published-schemas).

## Rust

The `cookie_agent_protocol` crate has the client the TUI and the web page
use. With its `websocket` feature, `Client::connect_websocket_with_token`
connects natively, and on `wasm32` `Client::connect_browser_websocket` uses the
browser's WebSocket (send the token with `handshake_with_token`). The client
pairs replies with requests, reads events openly, and delivers events,
replays, and notifications in order on one stream. A sketch:

```rust
use cookie_agent_protocol::{Client, ClientIdentity, ClientKind, SessionListParams};

let client = Client::connect_websocket_with_token(url, &token).await?;
client
    .handshake_as(ClientIdentity::generate(ClientKind::new("mytool")?))
    .await?;
let mut deliveries = client.subscribe_deliveries().expect("taken once");
let sessions = client.list_sessions(SessionListParams::default()).await?;
client.subscribe_events(session_id, None).await?;
while let Some(delivery) = deliveries.recv().await {
    // ClientDelivery::Live, ReplayEvent, SessionsChanged, Disconnected, …
}
```

`cookie_agent_client_state` folds those deliveries into per-session state,
rows ready to draw, pending approvals, runtimes, and the rest, with the panel
rules the TUI and the web page share; it builds for `wasm32` too. See the
[Rust API](../reference/api.md) and
[Session mechanics](../reference/protocol.md#session-mechanics).

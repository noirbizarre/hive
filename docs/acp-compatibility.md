# ACP compatibility and draft maturity

Hive clients always initialize with ACP protocol version 2. The current SDK
`agent-client-protocol =3.2.0` needs its `unstable_protocol_v2` feature;
`agent-client-protocol-http =3.2.0` provides both server transport shapes.
Their crate versions do not denote the ACP wire version. The SDK pins schema
`=1.10.2`; the Cargo lockfile records the tested dependency set.

Implemented v2 operations: initialize, session/new, list, resume, close,
prompt, cancel and session/update. A prompt response acknowledges acceptance;
observe `running` then `idle` for work completion. Only text prompts are
adapted to v1. V1 message chunks are forwarded with a synthetic v2 message
ID; its completed prompt response generates a v2 idle update. V2-only tool,
permission, MCP bridge, forks, config options, compaction and other optional
capabilities are **not** advertised. Non-text v1 prompts return an error.

ACPv2 `session/new` has no standard agent selector. Hive reserves
`_meta.hive.agent` for a profile ID in **client-to-Hive** requests; Hive removes
the selector before forwarding to downstream agents. Omit it to use
`default_agent`; if no default exists, creation returns an error. Example:

```json
{"cwd":"/workspace","mcpServers":[],"_meta":{"hive":{"agent":"pi"}}}
```

The same request works over WebSocket and HTTP/SSE. The SDK supports `_meta`
on `v2::NewSessionRequest`; this selector is a Hive extension, not an ACP
standard field. Profile-declared versions and transports select one downstream
connection without speculative retry: v2 first where declared, otherwise the
explicit v1 compatibility adapter. SDK client transports support stdio,
WebSocket and HTTP/SSE. No automatic fallback on failed initialization is
claimed.

The draft [v2 specification](https://github.com/agentclientprotocol/agent-client-protocol/tree/main/docs/protocol/v2)
and [draft announcement](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/announcements/acp-v2-draft.mdx)
may change. See the [SDK v2 guide](https://github.com/agentclientprotocol/rust-sdk/blob/main/md/protocol-v2.md)
and [HTTP guide](https://github.com/agentclientprotocol/rust-sdk/blob/main/md/http-transport.md).
Replay is an in-memory message-snapshot subset, not complete history or
transport-event recovery; a lost prompt acknowledgement must not be blindly
retried. Updates may arrive before responses and idle is not a turn-ID boundary.

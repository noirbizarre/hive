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

The v2 downstream provider is stdio; the v1 provider is a separate stdio
adapter. Selection is explicit via provider configuration, not speculative
transport probing. Initialization failure is actionable and does not retry
automatically. Downstream WS and HTTP/SSE agent connections have not been
implemented even though `HttpClient` supports them as a client-facing transport.

The draft [v2 specification](https://github.com/agentclientprotocol/agent-client-protocol/tree/main/docs/protocol/v2)
and [draft announcement](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/announcements/acp-v2-draft.mdx)
may change. See the [SDK v2 guide](https://github.com/agentclientprotocol/rust-sdk/blob/main/md/protocol-v2.md)
and [HTTP guide](https://github.com/agentclientprotocol/rust-sdk/blob/main/md/http-transport.md).
Replay is an in-memory message-snapshot subset, not complete history or
transport-event recovery; a lost prompt acknowledgement must not be blindly
retried. Updates may arrive before responses and idle is not a turn-ID boundary.

# Architecture

The daemon (`crates/hive-daemon`) owns a shared in-memory session broker. The
official `AcpHttpServer` mounts ACP at `/acp`: GET with WebSocket upgrade is a
persistent bidirectional JSON-RPC channel; POST plus per-connection and
per-session GET SSE streams form the HTTP alternative. Both create the same
v2 agent handlers against the same broker. `/hive/v1/*` contains operational
HTTP endpoints and a same-origin browser asset route, not session RPC methods.

`hive-core` defines transport-free Hive session IDs and lifecycle vocabulary.
`hive-protocol` pins SDK integration, provider declarations and shared native
status access. The daemon broker maps each Hive session ID to one agent process
and its downstream session ID. It retains user/agent message snapshots for
in-process replay; on resume it attaches a different client connection and
sends these snapshots before returning. Replay is not durable or complete.

The downstream version is selected **per declared provider configuration**:
`--agent-version v2` (default) or `v1`. One process is launched for each session;
there is no speculative probing or fallback after initialization errors. v1
text chunks are adapted to v2 agent chunks and the v1 prompt response produces
a v2 idle state. The transport exposed to clients remains v2 regardless.

The TUI uses SDK `Client.v2()` / `HttpClient`; the browser implements the ACPv2
WebSocket JSON-RPC boundary and queries the operational status endpoint.
ZJHive uses Zellij's host WebAccess permission for one-shot daemon status;
Zellij's `web_request` is not a persistent WebSocket or SSE transport.

The current ownership model is one trusted local OS user. Session routing
checks the exact Hive session ID and downstream ID. There is no persistent
storage, per-client authorization, multi-observer event broadcast, reconnect
cursor, guaranteed stream replay, or remote security policy yet.

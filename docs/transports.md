# Implemented transport matrix

| Direction | Protocol | Transport | Status |
| --- | --- | --- | --- |
| Client to Hive | v2 | WebSocket `/acp` | Integration-tested SDK client through mock agent |
| Client to Hive | v2 | HTTP POST + SSE GET `/acp` | Integration-tested SDK client through same broker |
| Hive-side peer client to owner Hive | v2 | Streamable HTTP `/acp` | Standard requests, configuration, streamed updates, disconnect and explicit live reattachment tested against a real owner broker and stdio agent |
| Hive to agent | v2 | stdio | Integration-tested with deterministic mock |
| Hive to agent | v1 | stdio | Integration-tested text prompt compatibility adapter |
| Hive to agent | v2/v1 | WebSocket, HTTP/SSE | SDK client transport; v2 HTTP/SSE and WS integration-tested with mock |
| Hive to OpenCode server | REST + SSE (not ACP) | HTTP `/session`, `/event` or the `/api/*` preview | `kind = "opencode-server"`; integration-tested with a scripted fake and probed against OpenCode 1.19.3 (v1 routes). See [the adapter](compatibility.md#opencode-server-adapter) |
| Client to Hive | v1 | Any | Rejected by v2 gateway |

HTTP/SSE uses connection- and session-scoped GET streams for server events;
requests and cancellation travel by POST. The SDK handles its connection ID,
session ID and routing. No transport `Last-Event-ID` resumption is claimed.

## Remote Hive specification boundary

The [Hive ACP extension specification](acp-extensions.md) defines versioned agent
discovery, selection and qualified session location through standard ACP transport.
Persistent owners implement negotiated discovery and selected startup; qualified
session location/routing remains #80. See the [current extension implementation](acp-extensions.md#current-implementation).

Production startup defaults to loopback. Explicit `--allow-remote` permits the
shared listener on a private interface, with optional bearer authentication.
See [deployment examples](operations.md#private-network-access). Controller tokens
are attachment fencing, separate from transport credentials. Remote session
ownership and discovery remain the owner's responsibility.

## SDK audit and limits

Audited 2026-11-05: `agent-client-protocol` and `agent-client-protocol-http`
**3.5.0**, with resolved schema **1.12.0** and unstable ACPv2 enabled. Published
HTTP crate source records commit `305aefda6fbe88be6ae8b9088003e383f909b135`.
See the pinned [HTTP transport documentation][sdk-http] and
[implementation][sdk-source]. This validates the tested ACP SDK contract, not
interchangeability with MCP Streamable HTTP or full draft-protocol conformance.

- Initialize POST returns `Acp-Connection-Id`. Subsequent POST, SSE GET and
  connection DELETE use it. Session streams additionally use `Acp-Session-Id`.
  Open the session stream before expecting session-scoped replies or updates.
- Top-level `params.sessionId` determines transport session scope. The specified
  resolver's nested `session: {owner, id}` remains connection-scoped; a test
  receives its current method-not-found response without any session attachment.
- POST bodies have a **16 MiB** SDK limit. Invalid content type is rejected;
  malformed frames without a connection ID fail at the HTTP boundary. With an
  established connection, JSON-RPC errors may instead arrive on SSE.
- Connection DELETE is **not** ACP `session/close`. SSE loss or a dropped client
  ends its attachment without killing the owner session. Cleanup DELETE is
  best-effort and is not guaranteed during a network partition.
- The SDK does not automatically reconnect SSE or replay `Last-Event-ID`.
  Reconnect initializes a new connection and explicitly resumes a known owner
  session. Hive's transport helper never recreates sessions or retries prompts.
- SDK queues are unbounded; there is no general memory/backpressure guarantee.
  Long-lived SSE must not use finite whole-request HTTP timeouts.
- Hive peer endpoints are exact HTTP(S) URLs without userinfo or fragments.
  The helper disables redirects, supports optional sensitive bearer headers,
  and bounds connection establishment. Generic agent base URLs retain the SDK's
  `/acp` expansion and WebSocket support. Stdio is unchanged.
- Authentication and server-side browser Origin enforcement belong to Hive's
  router, not SDK CORS. Authenticated native requests need no Origin header.
  The Hive peer driver replaces SDK failures with the controlled
  `hive::transport::connection` diagnostic, without remote bodies/URLs. Construction
  errors also omit credentials. Direct users of the raw SDK remain responsible
  for sanitizing its errors; standard per-request ACP errors are separate.

Evidence: `http::tests` in `crates/hive-daemon/src/http.rs`,
`remote_http` in `crates/hive-daemon/tests/support/remote_http.rs`, and the CLI
shared-listener test in `crates/hive-cli/tests/daemon.rs`. Coverage includes
malformed traffic, server failures, refused redirects, SSE loss, reconnecting
to the same session, bearer/cookie access and optional-auth deployment.

Hive-side consumers construct the reusable `hive_protocol::transport::Credentials`
from an explicitly named token variable (or use its unauthenticated default),
then call `peer("http://100.64.0.10:8765/acp")`. The returned `PeerTransport`
implements the SDK's `ConnectTo<Client>` and can be passed to `Client.v2().connect_with`.
It creates no catalog record or session. The reusable `hive_protocol::remote::Peer`
negotiates extensions, lists safe profiles, and explicitly starts a selected agent
through standard `session/new`. Neither helper retries startup after a lost response.
There is no `[peers]` TOML routing registry; cross-Hive session routing remains #80.

[sdk-http]: https://github.com/agentclientprotocol/rust-sdk/blob/305aefda6fbe88be6ae8b9088003e383f909b135/md/http-transport.md
[sdk-source]: https://github.com/agentclientprotocol/rust-sdk/tree/305aefda6fbe88be6ae8b9088003e383f909b135/src/agent-client-protocol-http/src

## Reconnect, recovery, and replay

These are separate guarantees:

- **Live reconnect:** session resume attaches to an existing broker session,
  with bounded incremental replay or a full in-memory transcript snapshot. The worker keeps running.
- **Daemon restart:** Hive attempts downstream reattachment using saved IDs,
  where the agent supports it. Failure remains a visible offline record.
- **Incremental replay:** the latest 256 updates are in memory only. Restart loses
  them, and even a live daemon can expire an old cursor. Fresh `start` attachments
  use a separate transcript snapshot, rebuilt from agent-retained history after
  restart. Hive does not durably archive conversation payloads.

Controlled native fatal-error entries participate in both incremental replay
and full transcript snapshots. They remain visible after ring eviction while
the daemon stays alive. Historical failures do not replay state transitions or
override current foreground activity. Repeated idle/error snapshots add no
duplicate failures. After restart, failure history depends on what the agent
replays; a last-reported error snapshot alone does not establish complete history.

The separate `/v1/sessions/events` SSE feed announces catalog revision
changes only. Clients re-list sessions rather than replaying this feed.

See [session workflows](guides/sessions.md) and [recovery operations](operations.md#restart-and-recovery).

# Implemented transport matrix

| Direction | Protocol | Transport | Status |
| --- | --- | --- | --- |
| Client to Hive | v2 | WebSocket `/acp` | Integration-tested SDK client through mock agent |
| Client to Hive | v2 | HTTP POST + SSE GET `/acp` | Integration-tested SDK client through same broker |
| Hive to agent | v2 | stdio | Integration-tested with deterministic mock |
| Hive to agent | v1 | stdio | Integration-tested text prompt compatibility adapter |
| Hive to agent | v2/v1 | WebSocket, HTTP/SSE | SDK client transport; v2 HTTP/SSE and WS integration-tested with mock |
| Hive to OpenCode server | REST + SSE (not ACP) | HTTP `/session`, `/event` or the `/api/*` preview | `kind = "opencode-server"`; integration-tested with a scripted fake and probed against OpenCode 1.19.3 (v1 routes). See [the adapter](acp-compatibility.md#opencode-server-adapter) |
| Client to Hive | v1 | Any | Rejected by v2 gateway |
| ZJHive to Hive | v2 | Native bridge client to WebSocket `/acp`; plugin polls HTTP catalog and local bridge | Integration-tested against mock agent |

HTTP/SSE uses connection- and session-scoped GET streams for server events;
requests and cancellation travel by POST. The SDK handles its connection ID,
session ID and routing. No transport `Last-Event-ID` resumption is claimed.

## Reconnect, recovery, and replay

These are separate guarantees:

- **Live reconnect:** session resume attaches to an existing broker session,
  with bounded retained replay and current snapshots. The worker keeps running.
- **Daemon restart:** Hive attempts downstream reattachment using saved IDs,
  where the agent supports it. Failure remains a visible offline record.
- **Replay:** the latest 256 updates are in memory only. Restart loses them,
  and even a live daemon can expire an old cursor. Replay is not a full transcript.

The separate `/v1/sessions/events` SSE feed announces catalog revision
changes only. Clients re-list sessions rather than replaying this feed.

See [session workflows](guides/sessions.md) and [recovery operations](operations.md#restart-and-recovery).

# Hive operational HTTP API

Loopback only. The current single-user policy has no separate login;
administration and session access MUST NOT be exposed on an external listener.

| Route | Result |
| --- | --- |
| `GET /hive/v1/health` | `{ "status": "ok" }` when HTTP responds |
| `GET /hive/v1/ready` | `{ "ready": true }` when listener is active (does not probe the provider) |
| `GET /hive/v1/status` | name, build version, in-memory session count, default agent ID and active sessions by agent |
| `GET /hive/v1/providers` | configured agent IDs, display names, declared transports and versions, default indication |
| `GET /hive/v1/web` | same-origin browser client |

ACP session methods live exclusively on `/acp`, not under `/hive/v1`.
Discovery never returns commands, endpoints, arguments, environment or headers;
it reports configuration, not live availability. Session creation uses
`session/new` and the optional Hive-specific `_meta.hive.agent` selector.
Transfer of control uses ACPv2 `session/resume` over `/acp` on either WebSocket
or HTTP/SSE, not a separate operational endpoint. A response `_meta.hive`
contains the current generation, token, next event sequence and running state;
all mutating ACP calls must include that generation/token. Prompts additionally
require a stable idempotency key. See [ACP compatibility](acp-compatibility.md)
for the exact wire contract, replay cursor and retention guarantees. The TUI
and browser are both clients of this daemon contract. ZJHive's current Zellij
WebAccess integration reports one-shot status only; its host API does not
provide the persistent WebSocket/SSE connection needed for an ACP controller.

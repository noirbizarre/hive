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

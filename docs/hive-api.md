# Hive operational HTTP API

Loopback only. The current single-user policy has no separate login;
administration and session access MUST NOT be exposed on an external listener.

| Route | Result |
| --- | --- |
| `GET /hive/v1/health` | `{ "status": "ok" }` when HTTP responds |
| `GET /hive/v1/ready` | `{ "ready": true }` when listener is active (does not probe the provider) |
| `GET /hive/v1/status` | name, build version, in-memory session count, provider transport |
| `GET /hive/v1/providers` | configured stdio protocol version and text prompt availability |
| `GET /hive/v1/web` | same-origin browser client |

ACP session methods live exclusively on `/acp`, not under `/hive/v1`.

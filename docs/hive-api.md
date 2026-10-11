# Hive operational HTTP API

Loopback only. The current single-user policy has no separate login;
administration and session access MUST NOT be exposed on an external listener.

| Route | Result |
| --- | --- |
| `GET /hive/v1/health` | `{ "status": "ok" }` when HTTP responds |
| `GET /hive/v1/ready` | `{ "ready": true }` when listener is active (does not probe the provider) |
| `GET /hive/v1/status` | name, build version, in-memory session count, default agent ID and active sessions by agent |
| `GET /hive/v1/providers` | configured agent IDs, display names, declared transports and versions, default indication |
| `GET /hive/v1/sessions` | current broker-owned session summaries and catalog revision |
| `GET /hive/v1/sessions/events` | SSE catalog revision notifications; re-list on change |
| `GET /hive/v1/web` | same-origin browser client |

ACP session methods live exclusively on `/acp`, not under `/hive/v1`.
Discovery never returns commands, endpoints, arguments, environment or headers;
it reports configuration, not live availability. Session creation uses
`session/new` and the optional Hive-specific `_meta.hive.agent` selector.
Transfer of control and observe/relinquish attachment use ACPv2 `session/resume`
over `/acp` on either WebSocket or HTTP/SSE, not a separate operational endpoint.
A response `_meta.hive` contains the current generation, next event sequence,
running state and role; only pilot responses include an opaque token;
all mutating ACP calls must include that generation/token. Prompts additionally
require a stable idempotency key. See [ACP compatibility](acp-compatibility.md)
for the exact wire contract, replay cursor and retention guarantees. The TUI
and browser are both clients of this daemon contract. ZJHive's current Zellij
WebAccess integration reports one-shot status only; its host API does not
provide the persistent WebSocket/SSE connection needed for an ACP controller.

## Session catalog

`GET /hive/v1/sessions` returns a sorted snapshot of the daemon's in-memory
sessions, independent of the connection that created them:

```json
{
  "revision": 7,
  "sessions": [
    {
      "sessionId": "hive-…",
      "cwd": "/workspace",
      "agent": "my-profile",
      "title": "Review the parser",
      "lifecycle": "active",
      "activity": "idle",
      "lastActivityAt": 1791754566280,
      "controlOwner": "opaque-connection-id",
      "generation": 2,
      "observers": 1
    }
  ]
}
```

`sessionId` and `agent` are Hive's session and configured profile IDs, not
downstream agent identities. `title` is the first non-empty text prompt,
whitespace-normalized and limited to 120 characters; it is `null` until one is
accepted. `lastActivityAt` is Unix milliseconds of the latest daemon-observed
accepted prompt or agent update, or `null` before either. It is not proof of
ongoing agent work. `activity` is `unknown` until Hive receives an agent
running/idle update or admits a prompt, then `running` or `idle` according to
the last known foreground state. Agents may perform other work without a
corresponding status update; Hive does not guess its state. `lifecycle` is
`active` or temporarily `closing`; closed and failed sessions are removed.
These sessions do not persist across a daemon restart.

`controlOwner` is the current pilot's opaque **connection-scoped** ID, or
`null` when control is unclaimed. It can change across reconnects and is not a
person/user identity or a credential. `generation` follows the same fencing
epoch as ACP `session/resume`. No control token is ever present in a summary.
`observers` counts attached non-pilot connections.

The WebUI lists these summaries and opens a selected session, including one
created in the TUI, read-only using its `sessionId` and `cwd`. Typing in its
prompt explicitly requests control through ACP `session/resume`; Send remains
unavailable until Hive confirms pilot control and the session is idle. Cancel
and permission/elicitation responses are available only to the pilot. A
transferred pilot immediately becomes read-only. Browser reload reconstructs
history from replay rather than trusting a cursor without rendered messages;
WebSocket reconnect resumes observing from the last applied event and never
resends a prompt. If creation itself lost its acknowledgement, the browser
requires selection of the original session from the catalog before prompting
again, rather than silently creating a second agent session. An uncertain
prompt acknowledgement can be retried manually with its original idempotency
key. If the 256-update in-memory replay window has expired, the WebUI reports
missing history and offers an explicit
attach-without-history action. Daemon restart loses both the sessions and
their replay/command outcomes; the WebUI does not restore them from storage.

To observe the catalog, open `GET /hive/v1/sessions/events` as an EventSource
(or another SSE client), **then** fetch `/hive/v1/sessions`. Each event is named
`revision`, with its decimal revision in `data` and `id`; the first event on
each connection reports the current revision. If an event has a revision
greater than the snapshot's, fetch a fresh snapshot; on reconnection always
fetch a fresh snapshot. Revisions are monotonic only for this daemon process.
The stream may coalesce changes and has no replay guarantee: events invalidate
the snapshot, rather than carrying session deltas. An SSE connection is only
for catalog discovery; ACP `/acp` remains the source of session conversations
and control operations. Both endpoints use the same loopback-only policy as
the rest of this API.

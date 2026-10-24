# Hive operational HTTP API

This surface is loopback-only under production daemon startup and has no
separate login. It serves discovery and administration for one trusted local
user. Session conversation/control methods live exclusively on `/acp`.

## Routes

| Route | Result |
| --- | --- |
| `GET /v1/health` | `{ "status": "ok" }` when HTTP responds |
| `GET /v1/ready` | `{ "ready": true }` when listener is active, without probing agents |
| `GET /v1/status` | Name, build version, live/persisted session counts, default agent, active sessions by agent |
| `GET /v1/providers` | Profile IDs, names, declared transports/versions, default indication |
| `GET /v1/providers/{agent}/native-sessions` | Native sessions an OpenCode server profile can adopt |
| `GET /v1/sessions` | Live/persisted session summaries and catalog revision |
| `GET /v1/sessions/events` | SSE catalog revision invalidations |
| `GET /ui` | Embedded browser HTML |
| `GET /ui/app.js` | Embedded browser JavaScript |

Provider discovery omits commands, endpoints, arguments, environment, and headers.
It describes configuration rather than successful availability probes.

## ACP boundary

Creation, control transfer, observation, relinquishment, prompting, cancellation,
close, and agent interactions use ACPv2 on `/acp` over WebSocket or HTTP/SSE.
There is no parallel REST session-mutation or question protocol.

New/resume responses expose role, current snapshots, and sequence metadata.
Only pilots receive control tokens. Mutations require current generation/token,
and prompts additionally need a stable key. See the
[ACP wire contract](acp-compatibility.md#control-and-replay).

## Session catalog

`GET /v1/sessions` returns a sorted snapshot independent of the client
connection that created each session:

```json
{
  "revision": 7,
  "usageByAgent": {
    "my-profile": {
      "turns": 0,
      "reportedTurns": 0,
      "tokens": {
        "totalTokens": null,
        "inputTokens": null,
        "outputTokens": null,
        "thoughtTokens": null,
        "cachedReadTokens": null,
        "cachedWriteTokens": null
      },
      "estimatedCost": null,
      "actualSessionCost": null,
      "contextUsed": null,
      "contextSize": null
    }
  },
  "sessions": [
    {
      "sessionId": "hive-…",
      "cwd": "/workspace",
      "agent": "my-profile",
      "title": "Review the parser",
      "lifecycle": "active",
      "recovery": "active",
      "recoveryReason": null,
      "activity": "idle",
      "lastActivityAt": 1791754566280,
      "controlOwner": "opaque-connection-id",
      "generation": 2,
      "observers": 1,
      "usage": {
        "turns": 0,
        "reportedTurns": 0,
        "tokens": {
          "totalTokens": null,
          "inputTokens": null,
          "outputTokens": null,
          "thoughtTokens": null,
          "cachedReadTokens": null,
          "cachedWriteTokens": null
        },
        "estimatedCost": null,
        "actualSessionCost": null,
        "contextUsed": null,
        "contextSize": null
      },
      "origin": "hive",
      "nativeId": null,
      "externalActivity": false
    }
  ]
}
```

### Identity and activity fields

| Field | Meaning |
| --- | --- |
| `sessionId` | Stable Hive ID, not the downstream ID |
| `agent` | Configured profile ID |
| `cwd` | Original session working directory |
| `title` | First accepted nonempty text prompt, normalized and limited to 120 characters; initially null |
| `lastActivityAt` | Unix milliseconds of last observed accepted prompt/update; initially null |
| `activity` | `unknown`, `running`, or `idle` foreground state |
| `origin` | `hive` or `adopted` |
| `nativeId` | OpenCode native ID for server profiles, otherwise null |
| `externalActivity` | Native OpenCode client is driving foreground work |

Activity timestamps are not proof of ongoing work. Unknown is not idle.
Foreground state does not describe every tool, plan, or late background task.
For those, use `_hive_activity` and the atomic `activitySnapshot` on `/acp`.
See [live execution activity](acp-compatibility.md#live-execution-activity).

### Lifecycle and recovery fields

`lifecycle` is `active` or temporarily `closing` for live workers. Offline
records are `recovering`, `unavailable`, `orphaned`, or `lost` and retain their
original Hive identity.

`recovery` is `active` for new workers, `reattached` after recovery,
`recovery_required` for uncertain work needing acknowledgement, or the offline
lifecycle. `recoveryReason` explains the required action.

Offline records cannot accept ACP commands. Reattached uncertain sessions can
be observed but not controlled until acknowledged. See
[recovery operations](operations.md#restart-and-recovery).

### Ownership and usage fields

`controlOwner` is an opaque connection-scoped ID, not a person or credential.
It is null when unclaimed. `generation` is the fencing epoch. `observers` counts
non-pilot attachments. Summaries never expose a control token.

`usage` is the current session aggregate. `usageByAgent` groups by configured
profile across sessions. Null values mean unknown, not zero. See
[token usage and costs](acp-compatibility.md#token-usage-and-costs).

## Native session discovery

`GET /v1/providers/{agent}/native-sessions?directory=<cwd>&limit=<n>`
lists candidates from an `opencode-server` profile without attaching them.
`directory` selects the project lookup; when omitted the server's default project
is used. `limit` defaults to 50 and is capped at 200.

```json
{
  "agent": "opencode-server",
  "server": { "version": "1.19.3", "api": "v1" },
  "sessions": [
    {
      "nativeId": "ses_…",
      "title": "Fix the bug",
      "directory": "/work/app",
      "updatedAt": 1791754566280,
      "busy": false,
      "adoptedAs": null
    }
  ]
}
```

`adoptedAs` identifies the existing Hive record for that profile/native ID, or
null. Duplicate protection is profile-scoped, not global across equivalent profiles.
Adopt through ACP `session/new` with both `_meta.hive.agent` and
`_meta.hive.adopt`. See the [complete example](agents/opencode.md#discover-and-adopt-a-native-session).

Errors contain `code` and `message`. Some also include `help`.

| HTTP status | Meaning |
| --- | --- |
| `404` | Unknown profile |
| `400` | Profile does not support native discovery |
| `502` | Unreachable server, rejected credentials, or unsupported API/version (`hive::opencode::*`) |

Responses omit endpoints and credentials but can reveal native titles and
working directories. The local trust boundary applies.

## Catalog revision stream

Open `GET /v1/sessions/events` as an EventSource or SSE client **before**
fetching `/v1/sessions`. Each event is named `revision`, with its decimal
revision in `data` and `id`. The first event reports the current revision.

If an event is newer than the snapshot, fetch another snapshot. Always fetch a
fresh snapshot on reconnection. Revisions are monotonic only within this daemon
process and changes may coalesce.

There is no replay guarantee or session delta stream. These events invalidate
discovery snapshots; ACP remains the source of conversation/control events.

## Client navigation

The [TUI](guides/tui.md), [browser](guides/web.md), and external
[ZJHive](https://github.com/noirbizarre/zjhive) use the same catalog and ACP session
identities. ZJHive owns its complete Zellij integration and transport companion;
Hive exposes the same ACP interface to it as to other clients.

Client guides describe controls. [Sessions and handoff](guides/sessions.md)
explains roles, retry, and history semantics. A client-local attention marker
reflects visible questions, not a catalog-wide inventory of pending interactions.

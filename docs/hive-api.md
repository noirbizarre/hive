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
| `POST /v1/config/reload` | Validate and publish the original configuration for new sessions |
| `GET /v1/projects` | Sorted effective project definitions with ownership origin |
| `POST /v1/projects` | Register durable metadata without cloning |
| `GET /v1/projects/{id}` | Inspect one definition |
| `PUT /v1/projects/{id}` | Replace a managed definition, preserving its ID |
| `DELETE /v1/projects/{id}` | Remove a managed definition only |
| `GET/PUT/DELETE /v1/projects/{id}/nodes/{node}/override` | Inspect, set, or remove a node-local directory preference |
| `GET /v1/providers/{agent}/native-sessions` | Native sessions an OpenCode server profile can adopt |
| `GET /v1/sessions` | Live/persisted session summaries and catalog revision |
| `GET /v1/sessions/events` | SSE catalog revision invalidations |
| `GET /ui` | Embedded browser HTML |
| `GET /ui/app.js` | Embedded browser JavaScript |
| `GET /ui/style.css` | Embedded browser stylesheet |

Provider discovery omits commands, endpoints, arguments, environment, and headers.
It describes configuration rather than successful availability probes.

## Configuration reload

`POST /v1/config/reload` takes no body or source-path parameter. It reloads the
daemon's original file and returns `{ "reloaded": true, "revision": 8 }` on
success. Existing workers, pricing, ownership, and replay remain unchanged.
An accepted reload invalidates the catalog even if no session changed or the
file was unchanged; clients should refresh both sessions and provider inventory.

Failure bodies carry `code`, `message`, and optional `help`, without connection
secrets. Failed reloads preserve configuration and do not emit an invalidation:

| Status | Meaning |
| --- | --- |
| `422` | Read, TOML, or validation failure (`hive::config::*`) |
| `409` | Existing session identity conflict (`hive::reload::profile_identity`) or an embedded broker without a file (`hive::reload::no_source`) |
| `403` | Browser origin rejected (`hive::reload::origin`) |
| `503` | Daemon shutting down (`hive::reload::stopping`) |
| `500` | Internal reload failure (`hive::reload::internal`) |

No-Origin local CLI requests are accepted. Browser mutations must originate from
the exact listener origin or its `http://localhost:<same-port>` alias; foreign,
null, duplicate origins and cross-site fetches are rejected. This preserves the
trusted-local-user boundary, without introducing a separate login.

Unix SIGHUP and `hive daemon reload` call the same reload implementation.
See [configuration](configuration.md#reloading-changes) for identity and environment
rules and [operations](operations.md#reloading-configuration) for examples.

## Project registry

`POST /v1/projects` accepts an optional `id` and project metadata:

```json
{
  "name": "Hive",
  "repository_source": "https://github.com/noirbizarre/hive.git",
  "default_ref": "main",
  "preparation": [["mise", "install"]]
}
```

Only `name` is required. Without an explicit `id`, registration generates a name
slug (`Hive` becomes `hive`), trying `hive-2`, `hive-3`, and so on for collisions
with either configured or managed projects. Allocation is serialized, so concurrent
registrations cannot choose the same ID. A successful registration returns the allocated `id`,
`origin: "managed"`, and the metadata (absent optional source/ref fields serialize
as null). `GET /v1/projects` returns `{ "projects": [...] }` sorted by ID.
Configured entries have `origin: "config"`. `PUT` accepts the same metadata
without `id` and replaces all mutable fields. Unknown fields are rejected.

Overrides never appear in portable project responses. A `PUT` to the node override
route accepts an explicit owning node and absolute path syntax:

```json
{
  "directory": {
    "node": "node-22222222-2222-4222-8222-222222222222",
    "syntax": "posix",
    "path": "/home/user/work/hive"
  }
}
```

The body node must match the route node. `windows` syntax also works. `GET`
returns `{ "project": "…", "node": "…", "directory": { ... } }`. Override writes
return `{ "updated": true }`; definition/override deletion returns
`{ "removed": true }`. Orphaned overrides remain inspectable and explicitly
removable after a definition is removed.

All project mutations use the same origin policy as reload and invalidate the
catalog after successful durable publication. Errors carry `code`, `message`,
and `help`, with `hive::project::*` codes: 422 for invalid input, 404 for unknown
resources, 409 for duplicate IDs or configured ownership, 403 for rejected origins,
503 for shutdown or unavailable persistence, and 500 for storage failures.
Configured/managed collisions also reject configuration reload with 409.

Registration stores intent without Git or filesystem operations. Definitions
contain no local paths or credential fields. Deletion never deletes resources or
overrides. See [configuration](configuration.md#logical-projects) for validation
and authority rules.

## ACP boundary

Creation, control transfer, observation, relinquishment, prompting, cancellation,
close, and agent interactions use ACPv2 on `/acp` over WebSocket or HTTP/SSE.
There is no parallel REST session-mutation or question protocol.

New/resume responses expose role, current snapshots, and sequence metadata.
Only pilots receive control tokens. Mutations require current generation/token,
and prompts additionally need a stable key. See the
[ACP wire contract](compatibility.md#control-and-replay).

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
      "externalActivity": false,
      "transcript": {
        "owner": "agent",
        "source": "live",
        "restoration": "supported",
        "availability": "available",
        "completeness": "unknown",
        "warnings": []
      }
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
| `activity` | `unknown`, `running`, `requires_action`, or `idle` foreground state |
| `origin` | `hive` or `adopted` |
| `nativeId` | OpenCode native ID for server profiles, otherwise null |
| `externalActivity` | Native OpenCode client is driving foreground work |

Activity timestamps are not proof of ongoing work. Unknown is not idle.
`requires_action` is blocked active foreground work. ACP attachment/list
`running` booleans are true for both `running` and `requires_action`.
Foreground state does not describe every tool, plan, or late background task.
For those, use `_hive_activity` and the atomic `activitySnapshot` on `/acp`.
See [live execution activity](compatibility.md#live-execution-activity).

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

### Transcript ownership and status

Every session summary includes `transcript`, also exposed through ACP list
metadata, new/resume `_meta.hive`, and sequenced `_hive_control` updates:

| Field | Values and meaning |
| --- | --- |
| `owner` | `agent`: retained conversation authority |
| `source` | `unknown`, `live`, `agent_history`: how the current projection was seeded |
| `restoration` | `unknown`, `supported`, `unsupported`: negotiated reconstruction support |
| `availability` | `loading`, `available`, `unavailable`: current display projection, not execution readiness |
| `completeness` | `unknown` or `partial`: original retention cannot be certified by successful replay |
| `warnings` | Controlled `{code, message}` diagnostics; repeated reasons are deduplicated |

An available projection can be empty. Offline records remain listable without
transcript payloads, with unknown support and unavailable projection. Legacy
clients may ignore this additive object; new clients treat absent fields as
unknown rather than unsupported. Warnings do not grant control or acknowledge
uncertain work. Genuine restoration failures remain failures.

Attachment metadata includes `transcriptSeq`, an atomic status watermark. Ignore
older replayed control status; apply successful response status before queued
newer updates. Keep existing transcript snapshot boundaries unchanged. For
ownership, retention and migration policy, see
[ADR-009](adr/009-agent-owned-history-and-session-registry.md).

### Ownership and usage fields

`controlOwner` is an opaque connection-scoped ID, not a person or credential.
It is null when unclaimed. `generation` is the fencing epoch. `observers` counts
non-pilot attachments. Summaries never expose a control token.

`usage` is the current session aggregate. `usageByAgent` groups by configured
profile across sessions. Null values mean unknown, not zero. See
[token usage and costs](compatibility.md#token-usage-and-costs).

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
fetching `/v1/sessions` and `/v1/providers`. Each event is named `revision`, with its decimal
revision in `data` and `id`. The first event reports the current revision.

If an event is newer than the snapshot, refresh both inventories. Always fetch a
fresh snapshot on reconnection. Revisions are monotonic only within this daemon
process and changes may coalesce.

There is no replay guarantee or session delta stream. These events invalidate
discovery snapshots; ACP remains the source of conversation/control events.

## Client navigation

The [TUI](guides/tui.md), [Web UI](guides/web.md), and other ACP clients use the
same catalog and ACP session identities.

Client guides describe controls. [Sessions and handoff](guides/sessions.md)
explains roles, retry, and history semantics. A client-local attention marker
reflects visible questions, not a catalog-wide inventory of pending interactions.

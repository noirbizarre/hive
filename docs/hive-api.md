# Hive operational HTTP API

Loopback only. The current single-user policy has no separate login;
administration and session access MUST NOT be exposed on an external listener.

| Route | Result |
| --- | --- |
| `GET /hive/v1/health` | `{ "status": "ok" }` when HTTP responds |
| `GET /hive/v1/ready` | `{ "ready": true }` when listener is active (does not probe the provider) |
| `GET /hive/v1/status` | name, build version, live and persisted session count, default agent ID and active sessions by agent |
| `GET /hive/v1/providers` | configured agent IDs, display names, declared transports and versions, default indication |
| `GET /hive/v1/providers/{agent}/native-sessions` | native sessions an `opencode-server` profile can adopt (see below) |
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
running state, current `activitySnapshot` and role; only pilot responses include
an opaque token;
all mutating ACP calls must include that generation/token. Prompts additionally
require a stable idempotency key. See [ACP compatibility](acp-compatibility.md)
for the exact wire contract, replay cursor and retention guarantees. The TUI,
browser and ZJHive are clients of this daemon contract. ZJHive uses the existing
catalog for discovery and a native `hive zellij-bridge` ACP client for
attachment; the bridge supplies short loopback HTTP replies to Zellij's
one-shot WebAccess API. The daemon exposes no Zellij-specific session routes.

## Session catalog

`GET /hive/v1/sessions` returns a sorted snapshot of live and persisted
sessions, independent of the connection that created them:

```json
{
  "revision": 7,
  "usageByAgent": {
    "my-profile": {
      "turns": 0, "reportedTurns": 0,
      "tokens": {"totalTokens": null, "inputTokens": null,
        "outputTokens": null, "thoughtTokens": null,
        "cachedReadTokens": null, "cachedWriteTokens": null},
      "estimatedCost": null, "actualSessionCost": null,
      "contextUsed": null, "contextSize": null
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
      "usage": {"turns": 0, "reportedTurns": 0, "tokens": {"totalTokens": null,
        "inputTokens": null, "outputTokens": null, "thoughtTokens": null,
        "cachedReadTokens": null, "cachedWriteTokens": null},
        "estimatedCost": null, "actualSessionCost": null,
        "contextUsed": null, "contextSize": null},
      "origin": "hive",
      "nativeId": null,
      "externalActivity": false
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
corresponding status update; Hive does not guess its state. This foreground
summary does not represent tool, plan, or late background task state: use
`_hive_activity` updates on `/acp` and `activitySnapshot` from new/resume for
those; see [the activity contract](acp-compatibility.md#live-execution-activity).
`lifecycle` is
`active` or temporarily `closing` for a live worker. Offline records are
`recovering`, `unavailable`, `orphaned` (agent cannot attach), or `lost`
(agent rejects the saved ID) and retain their original Hive ID.
`recovery` reports `active` for a newly created live worker, `reattached`
after successful downstream recovery, `recovery_required` when
interrupted work needs an explicit acknowledgement, or the offline lifecycle.
`recoveryReason` explains why action is needed. Offline records cannot accept
ACP commands; an uncertain reattached session can be observed but not
controlled until acknowledged.

`controlOwner` is the current pilot's opaque **connection-scoped** ID, or
`null` when control is unclaimed. It can change across reconnects and is not a
person/user identity or a credential. `generation` follows the same fencing
epoch as ACP `session/resume`. No control token is ever present in a summary.
`observers` counts attached non-pilot connections.

`origin` is `hive` when Hive created the downstream session and `adopted` when
it attached to one found natively. `nativeId` is the OpenCode session ID for
`opencode-server` profiles (`null` for ACP profiles), so a session can be
resumed in OpenCode itself. `externalActivity` is `true` while a native
OpenCode client, not Hive, is driving the session; Hive prompts are refused
until it clears. See [the OpenCode server adapter](acp-compatibility.md#opencode-server-adapter).

## Native session discovery

`GET /hive/v1/providers/{agent}/native-sessions?directory=<cwd>&limit=<n>`
lists the sessions held by the OpenCode server behind an
`kind = "opencode-server"` profile, without attaching to any. `directory`
scopes OpenCode's project lookup (omitted, the server's own default project is
used); `limit` defaults to 50 and is capped at 200.

```json
{
  "agent": "opencode-server",
  "server": { "version": "1.19.3", "api": "v1" },
  "sessions": [
    { "nativeId": "ses_…", "title": "Fix the bug", "directory": "/work/app",
      "updatedAt": 1791754566280, "busy": false, "adoptedAs": null }
  ]
}
```

`adoptedAs` is the Hive session ID already bound to that native session, or
`null`. Adopt one with `session/new` and `_meta.hive.adopt = {"nativeId": …}`.
Errors are `{ "code", "message", "help" }`: `404` for an unknown agent, `400`
for a profile that has no native sessions, and `502` when the OpenCode server
itself is unreachable, rejects the credentials or is unsupported (the
`hive::opencode::*` codes). Responses never contain the endpoint or credentials,
but they do contain session titles and working directories.

The WebUI lists these summaries and opens a selected session, including one
created in the TUI, read-only using its `sessionId` and `cwd`. Typing in its
prompt explicitly requests control through ACP `session/resume`; Send remains
unavailable until Hive confirms pilot control and the session is idle. Cancel
and session-scoped permission/elicitation responses are available only to the pilot. A
transferred pilot immediately becomes read-only. Browser reload reconstructs
history from replay rather than trusting a cursor without rendered messages;
WebSocket reconnect resumes observing from the last applied event and never
resends a prompt. If creation itself lost its acknowledgement, the browser
requires selection of the original session from the catalog before prompting
again, rather than silently creating a second agent session. An uncertain
prompt acknowledgement can be retried manually with its original idempotency
key. If the 256-update in-memory replay window has expired, the WebUI reports
missing history and offers an explicit
attach-without-history action. That action restores current activity from the
atomic resume snapshot but cannot restore missed conversation messages.
Daemon restart retains session records and prompt keys, but loses replay
events and activity snapshots. The browser reports the history gap and offers
live-only attachment; interrupted work requires a separate explicit
acknowledgement before new prompts.

ZJHive's picker uses this catalog and displays the same Hive session IDs,
activity and owner information. Selecting a session observes it first; an
explicit takeover through `/acp` is required before prompts, cancellation or
interaction replies. The local bridge keeps a separate ACP connection per
plugin instance. Losing or reloading the plugin detaches that connection; the
daemon-owned agent session continues running. The bridge does not persist
session history or create sessions on behalf of the plugin.

Interactions themselves use ACP `session/request_permission`, `elicitation/create`,
`elicitation/complete`, and `$/cancel_request` on `/acp`. There is no operational
HTTP question/permission protocol. Request-scoped elicitation is answered by
the connection holding its live parent request. All clients queue concurrent
questions, withdraw cancelled requests, validate form values, and distinguish
URL consent from external completion. Optional-mode fallback and client
requirements are specified in
[ACP compatibility](acp-compatibility.md#interaction-capabilities-and-fallback).

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

## Client navigation

The TUI starts with the overview visible and hides it after opening a session;
`Tab` toggles the overview from chat, `Ctrl-B` switches back to the previous
session visited in this TUI run, and `a` toggles all directories/current
directory in the overview. Enter attaches as pilot, `o` as observer. TUI
summaries refresh while connected and on selection and reconnect. The browser
shows a collapsible session rail by default; "Hide sessions" and "Show
sessions" control its visibility, and selecting a row observes the existing
conversation before prompting requests control. Recently visited sessions
appear first during the current client run; other sessions are ordered by
reported last activity. The rail preserves keyboard focus, selection and
conversation state across catalog refreshes and visibility changes.

Clients display `unknown` activity as unknown, rather than treating it as
idle. A currently visible permission/input request may be flagged as needing
attention; the catalog cannot identify such requests for unrelated sessions.
Client recents do not survive restart; the daemon's session catalog does.
In the TUI, inspect a recovered session as an observer, then press `Ctrl-Y`
only after verifying agent state to acknowledge interrupted work and take
control. In the browser, use the explicit recovery acknowledgement button.

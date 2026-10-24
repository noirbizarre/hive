# ADR-006: Local operational HTTP and ACP boundary

## Status

Accepted. Retrospective record written on 2026-10-24 for implemented behavior.

## Context

Clients need health, configured providers, session discovery, and browser assets
without implementing conversation control twice. The current product serves a
single trusted local user, not an authenticated remote tenant system.

Putting all discovery into ACP would complicate simple local integration.
Parallel REST mutation APIs would duplicate control semantics. Remote multi-user
deployment would require authentication and authorization beyond pilot fencing.

## Decision

Use `/acp` for conversation and session control. Use `/v1/*` for operational
status, discovery, catalog snapshots, and revision invalidation. Serve the browser
UI at `/ui` and its assets under `/ui/`.
Both surfaces derive session summaries from the same broker.

Catalog SSE means “fetch a new snapshot,” not replay of conversation events.
Public provider discovery omits private commands, endpoints, headers, and environment.

Production startup restricts listeners to loopback and holds a per-user lock.
The trust boundary remains the local OS user. Pilot tokens reject stale
controllers, not hostile local processes. Browser-origin policy is not login.

## Consequences

- Local clients can discover sessions without a second authoritative catalog.
- Two surfaces must describe the same broker state consistently.
- Readiness is not agent availability.
- Local processes can access sessions and administration without authentication.
- Remote multi-user exposure is outside the current supported boundary.

Implementation: [router and startup](https://github.com/noirbizarre/hive/blob/main/crates/hive-daemon/src/lib.rs),
[public profiles](https://github.com/noirbizarre/hive/blob/main/crates/hive-daemon/src/config.rs), and
[browser client](https://github.com/noirbizarre/hive/blob/main/clients/hive-web/app.js).
See the [API reference](../hive-api.md).

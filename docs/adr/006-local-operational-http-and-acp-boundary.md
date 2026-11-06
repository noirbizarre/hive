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

## Federation clarification — 2026-11-05

The [Hive ACP extension specification](../acp-extensions.md), defined for
[#77](https://github.com/noirbizarre/hive/issues/77), adds a specified remote-peer
discovery surface on ACP. A remote Hive should discover profiles and locate
sessions through its ACP connection, using standard session methods for control.
The local operational API continues to serve simple local integrations; both
discovery views derive from the same broker rather than separate authorities.

The new negotiation, discovery and session-location extensions are not implemented
by this decision record. Remote transport validation, authenticated/authorized
deployment and safe non-loopback binding remain follow-up implementation work.
The retrospective local trust boundary above described production at that time.
Subsequent implementation in #79 supplies negotiated profile discovery/selection
for persistent owners; #80 adds qualified session location and routing through
statically registered, identity-verified peers over the same ACP boundary.

## Shared private-network listener — 2026-11-05

[#78](https://github.com/noirbizarre/hive/issues/78) extends the deployment boundary:
loopback remains the default, while explicit remote opt-in exposes the same ACP
and operational listener. Authentication is optional according to deployment
topology. When enabled, shared bearer credentials or ephemeral browser-login
cookies protect both surfaces. There are no per-user roles. The ACP/operational
protocol separation and owner-side session supervision remain unchanged.
See [current deployment guidance](../operations.md#private-network-access) and
[SDK validation/limitations](../transports.md#sdk-audit-and-limits).

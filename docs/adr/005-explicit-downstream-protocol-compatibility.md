# ADR-005: Explicit downstream protocol compatibility

## Status

Accepted. Retrospective record written on 2026-10-24 for implemented behavior.

## Context

Agents expose different draft ACP versions and transports. Clients need one
session/control contract rather than a separate integration per agent.
Initialization failure can mean a bad command, missing credentials, a crash,
or the wrong wire version. Retrying another version hides these distinctions.

Exposing each downstream version directly would multiply client logic. Inferring
adapter type from a URL would confuse ACP HTTP with native REST servers.
Speculative fallback could start unnecessary processes and mask failures.

## Decision

Keep upstream clients on ACPv2. Require explicit profile adapter, transport, and
ACP version declarations. Prefer declared v2, otherwise select the explicit v1
worker. Do not fall back after initialization failure.

Give each session one downstream connection and, for stdio, its own process.
Treat v1 as a bounded compatibility adapter, not universal v2 equivalence.
Advertise optional features only when the broker actually implements them.
Select OpenCode REST/SSE explicitly through `kind = "opencode-server"`.

## Consequences

- Configuration must match the installed agent and can fail loudly after upgrades.
- Client session semantics remain independent of downstream transport/version.
- Some v2 features cannot be adapted to v1.
- Configuration declarations are not proof of successful real-agent operation.
- Native OpenCode identity and API flavors remain governed by
  [ADR-002](002-opencode-server-adoption.md).

Implementation: [configuration](https://github.com/noirbizarre/hive/blob/main/crates/hive-daemon/src/config.rs),
[compatibility probes](https://github.com/noirbizarre/hive/blob/main/crates/hive-daemon/src/compat.rs), and
[workers](https://github.com/noirbizarre/hive/blob/main/crates/hive-daemon/src/lib.rs).
See the [transport matrix](../transports.md).
